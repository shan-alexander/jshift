//! Field projection: keep-list / JMESPath subset → smaller JSON.
//!
//! # Architecture
//!
//! | Layer | Module | Role |
//! | --- | --- | --- |
//! | AST | [`select`] | [`SelectExpr`] (identity, object, array, multi-select, pipe, flatten, …) |
//! | Paths | [`jmespath`] | keep-list paths + JMESPath **subset** parser (byte-oriented, not a DOM port) |
//! | Plan | [`plan`] | [`ProjectPlan`], styles, missing policy |
//! | Focus | [`focus`] | span/arena model: pure projections never copy trees |
//! | Emit | [`emit`] | one-pass writer; index-accelerated hops; PreserveSource tree replay |
//!
//! ## Byte-oriented JMESPath (not jmespath.rs)
//!
//! Evaluation walks **raw JSON bytes** (`&[u8]` spans). Pure field/index chains and
//! pipes of pure paths resolve to document spans with **zero intermediate tree
//! copies**. Only synthesized values (multi-select, many functions, flatten of
//! reconstructed arrays) materialize into a reclaimable buffer. This is the same
//! design as jshift’s path engine — not a port of
//! [jmespath.rs](https://github.com/jmespath/jmespath.rs) (which builds a
//! `Variable` DOM).
//!
//! ## Index-wired project
//!
//! [`project_indexed`] passes [`crate::IndexedDocument`] side-tables into emit:
//! array element hops and object key lookups use O(1) tables when the open `[`/`{`
//! offset matches an indexed container.
//!
//! ## Streaming cards / JSONL (no giant output array)
//!
//! [`project_each`] / [`project_object_fields_each`] project **one element at a time**
//! into a reusable card buffer and invoke a callback — peak RAM tracks one card, not
//! the full `[card,card,…]` result. [`project_jsonl_write`] /
//! [`project_object_fields_jsonl_write`] write NDJSON (one JSON object per line) to any
//! [`std::io::Write`].
//!
//! ## Parallel auto-pick
//!
//! [`project_indexed_auto`] (and [`project_parallel_auto`]) enable Rayon list emit only
//! when [`plan_prefers_parallel`] says the per-element work is likely CPU-bound
//! (filters, functions, nested list work). Thin pure-field multi-select cards stay
//! sequential — they are usually memory-bound and parallel can lose.
//!
//! JMESPath compliance: see `tests/jmespath_compliance.rs` (tier A + full suite
//! with **zero residuals** on vendored official fixtures, excluding benchmarks).
//! Null semantics: missing paths / failed projections emit JSON `null` under
//! default [`MissingPolicy::Skip`]; hard failures use [`crate::Error::Jmespath`].

mod emit;
mod focus;
mod jmespath;
mod plan;
mod select;
mod sink;
mod transform;

pub use jmespath::{parse_jmespath_expr, parse_project_path, select_from_project_path};
pub use plan::{MissingPolicy, ProjectPlan, ProjectStyle};
pub use select::{
    ArraySelect, CmpOp, HashField, ObjectSelect, ProjectPathSegment, SelectExpr,
};
pub use sink::{CountingSink, WriteSink};
pub use transform::{Transform, TransformPipeline};

use std::io::Write;

use crate::error::Error;
use crate::index::IndexedDocument;
use crate::path::{parse_path, PathSegment};
use crate::scan::{find_value, find_value_offsets, skip_value, skip_whitespace};

use sink::{CountingSink as CountSink, EmitOut, WriteSink as WSink};

/// Project `json` according to `plan`, returning a new buffer.
///
/// ```
/// use jshift::{project, ProjectPlan};
///
/// let json = br#"{"id":7,"title":"Hat","images":[1,2,3]}"#;
/// let plan = ProjectPlan::from_paths(&["id", "title"]).unwrap();
/// assert_eq!(project(json, &plan).unwrap(), br#"{"id":7,"title":"Hat"}"#);
/// ```
pub fn project(json: &[u8], plan: &ProjectPlan) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    project_into(json, plan, &mut out)?;
    Ok(out)
}

/// Project into an existing buffer (appends).
pub fn project_into(json: &[u8], plan: &ProjectPlan, out: &mut Vec<u8>) -> Result<(), Error> {
    project_to_sink(json, plan, out)
}

/// Project using a pre-built [`IndexedDocument`].
///
/// Array and object side-tables (and Stage-1 structurals when present) accelerate
/// hops during emit: element access is O(1) for indexed arrays, key lookup is O(1)
/// for indexed objects. Share one index build across many `project_indexed` calls
/// on the same snapshot.
///
/// Prefer [`IndexedDocument::index_for_plan`] (or [`project_auto_indexed`]) so array
/// paths from the plan are side-tabled before projecting mid/last indices.
///
/// Evaluation still uses the span/arena model: pure projections never copy trees;
/// only synthesized values (multi-select, function results) touch the arena.
pub fn project_indexed(doc: &IndexedDocument<'_>, plan: &ProjectPlan) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    project_to_sink_indexed(doc, plan, &mut out)?;
    Ok(out)
}

/// Ensure `doc` has side-tables for the plan's array paths, then project.
///
/// Mutates `doc` (idempotent indexing). Ideal when reusing one snapshot for many plans
/// or many `project_indexed` calls after a single prepare.
pub fn project_indexed_prepare(
    doc: &mut IndexedDocument<'_>,
    plan: &ProjectPlan,
) -> Result<Vec<u8>, Error> {
    doc.index_for_plan(plan)?;
    project_indexed(doc, plan)
}

/// Build structural + plan array indexes, then project (one-shot convenience).
///
/// Equivalent to:
/// ```ignore
/// let mut doc = IndexedDocument::empty(json);
/// doc.index_for_plan(plan)?;
/// project_indexed(&doc, plan)
/// ```
///
/// For many projections on the same buffer, build once with
/// [`IndexedDocument::index_for_plan`] and call [`project_indexed`] repeatedly.
pub fn project_auto_indexed(json: &[u8], plan: &ProjectPlan) -> Result<Vec<u8>, Error> {
    let mut doc = IndexedDocument::empty(json);
    doc.index_for_plan(plan)?;
    project_indexed(&doc, plan)
}

/// Generic “thin cards” helper: project each object in an array down to named fields.
///
/// Works for **any** document shape — only the path to the array and field names matter
/// (e.g. `"products"`, `"data.rows"`, `"items"`, or `""` for a root array). Not tied to
/// any particular catalog or domain.
///
/// ```
/// use jshift::project_object_fields;
///
/// let json = br#"{"items":[{"id":1,"t":"a","x":9},{"id":2,"t":"b","x":8}]}"#;
/// let out = project_object_fields(json, "items", &["id", "t"]).unwrap();
/// assert_eq!(out, br#"[{"id":1,"t":"a"},{"id":2,"t":"b"}]"#);
/// ```
pub fn project_object_fields(
    json: &[u8],
    array_path: &str,
    fields: &[&str],
) -> Result<Vec<u8>, Error> {
    let plan = plan_object_fields(array_path, fields)?;
    project(json, &plan)
}

/// Build a [`ProjectPlan`] for [`project_object_fields`] (array of objects → multi-select hash).
pub fn plan_object_fields(array_path: &str, fields: &[&str]) -> Result<ProjectPlan, Error> {
    if fields.is_empty() {
        return Err(Error::InvalidPath {
            msg: "project_object_fields requires at least one field name",
        });
    }
    let multi = fields
        .iter()
        .map(|f| format!("{f}: {f}"))
        .collect::<Vec<_>>()
        .join(", ");
    let expr = if array_path.is_empty() || array_path == "@" {
        format!("[*].{{{multi}}}")
    } else {
        format!("{array_path}[*].{{{multi}}}")
    };
    ProjectPlan::from_jmespath(&expr)
}

/// Parallel list projection using Rayon (requires feature `parallel`).
///
/// Builds plan indexes, then projects. Large `[*]` / keep-list array walks use
/// side-table chunking across threads when beneficial. Still `#![forbid(unsafe_code)]`.
#[cfg(feature = "parallel")]
pub fn project_parallel(json: &[u8], plan: &ProjectPlan) -> Result<Vec<u8>, Error> {
    let mut doc = IndexedDocument::empty(json);
    doc.index_for_plan(plan)?;
    project_indexed_parallel(&doc, plan)
}

/// Like [`project_indexed`], but enables parallel array `[*]` emission when a side-table
/// is present and the array is large enough (feature `parallel`).
#[cfg(feature = "parallel")]
pub fn project_indexed_parallel(
    doc: &IndexedDocument<'_>,
    plan: &ProjectPlan,
) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    let start = skip_whitespace(doc.as_bytes(), 0);
    if start >= doc.as_bytes().len() {
        return Err(Error::InvalidJsonSyntax {
            pos: start,
            msg: "Unexpected EOF",
        });
    }
    let end = match plan.select() {
        SelectExpr::Identity | SelectExpr::Current => skip_value(doc.as_bytes(), start)?,
        _ => {
            let mut end = doc.as_bytes().len();
            while end > start && matches!(doc.as_bytes()[end - 1], b' ' | b'\t' | b'\n' | b'\r') {
                end -= 1;
            }
            end
        }
    };
    let mut ctx = emit::EmitCtx::new(plan, Some(doc));
    #[cfg(feature = "parallel")]
    {
        ctx.allow_parallel = true;
    }
    emit::emit_value(doc.as_bytes(), start, end, plan.select(), &mut ctx, &mut out)?;
    Ok(out)
}

/// Parallel variant of [`project_object_fields`] (feature `parallel`).
#[cfg(feature = "parallel")]
pub fn project_object_fields_parallel(
    json: &[u8],
    array_path: &str,
    fields: &[&str],
) -> Result<Vec<u8>, Error> {
    let plan = plan_object_fields(array_path, fields)?;
    project_parallel(json, &plan)
}

// ── Streaming cards / JSONL (P3) ─────────────────────────────────────────────

/// Project each element of a **list projection** plan as a separate card.
///
/// `plan` must be a `[*]` list projection (optionally under a field path), e.g.
/// `items[*].{id: id, title: title}` or root `[*].{a: a}`. Each card is evaluated
/// into a reusable buffer; **`f(index, card_bytes)`** is called once per kept
/// element. Null results are omitted (JMESPath list-projection semantics).
///
/// Peak memory is **one card** plus the input document — not a giant
/// `[card,card,…]` output array. For NDJSON files see [`project_jsonl_write`].
///
/// ```
/// use jshift::{project_each, ProjectPlan};
///
/// let json = br#"{"items":[{"id":1,"t":"a","x":9},{"id":2,"t":"b"}]}"#;
/// let plan = ProjectPlan::from_jmespath("items[*].{id: id, t: t}").unwrap();
/// let mut cards = Vec::new();
/// project_each(json, &plan, |_, card| {
///     cards.push(card.to_vec());
///     Ok(())
/// }).unwrap();
/// assert_eq!(cards[0], br#"{"id":1,"t":"a"}"#);
/// assert_eq!(cards[1], br#"{"id":2,"t":"b"}"#);
/// ```
pub fn project_each<F>(json: &[u8], plan: &ProjectPlan, f: F) -> Result<(), Error>
where
    F: FnMut(usize, &[u8]) -> Result<(), Error>,
{
    project_each_inner(json, plan, None, f)
}

/// Like [`project_each`], using side-tables from `doc` when the array path is indexed.
pub fn project_each_indexed<F>(
    doc: &IndexedDocument<'_>,
    plan: &ProjectPlan,
    f: F,
) -> Result<(), Error>
where
    F: FnMut(usize, &[u8]) -> Result<(), Error>,
{
    project_each_inner(doc.as_bytes(), plan, Some(doc), f)
}

/// Thin-card variant: project named fields on each object in `array_path`, one callback
/// per element (no giant result array).
///
/// Domain-agnostic — any array path (`""` for root, `"items"`, `"data.rows"`, …).
///
/// ```
/// use jshift::project_object_fields_each;
///
/// let json = br#"[{"id":1,"t":"a","noise":true},{"id":2,"t":"b"}]"#;
/// let mut n = 0usize;
/// project_object_fields_each(json, "", &["id", "t"], |_, card| {
///     n += 1;
///     assert!(card.starts_with(b"{"));
///     Ok(())
/// }).unwrap();
/// assert_eq!(n, 2);
/// ```
pub fn project_object_fields_each<F>(
    json: &[u8],
    array_path: &str,
    fields: &[&str],
    f: F,
) -> Result<(), Error>
where
    F: FnMut(usize, &[u8]) -> Result<(), Error>,
{
    let plan = plan_object_fields(array_path, fields)?;
    project_each(json, &plan, f)
}

/// Indexed thin-card each-callback (prefer when reusing an [`IndexedDocument`]).
pub fn project_object_fields_each_indexed<F>(
    doc: &IndexedDocument<'_>,
    array_path: &str,
    fields: &[&str],
    f: F,
) -> Result<(), Error>
where
    F: FnMut(usize, &[u8]) -> Result<(), Error>,
{
    let plan = plan_object_fields(array_path, fields)?;
    project_each_indexed(doc, &plan, f)
}

/// Write each list-projection card as one NDJSON line (JSON object + newline) to `w`.
///
/// Never materializes the full projected array — only one card buffer is held.
/// Returns total bytes written (including newlines).
///
/// ```
/// use jshift::{project_jsonl_write, ProjectPlan};
///
/// let json = br#"{"rows":[{"a":1,"b":2},{"a":3,"b":4}]}"#;
/// let plan = ProjectPlan::from_jmespath("rows[*].{a: a}").unwrap();
/// let mut out = Vec::new();
/// let n = project_jsonl_write(json, &plan, &mut out).unwrap();
/// assert_eq!(&out[..], b"{\"a\":1}\n{\"a\":3}\n");
/// assert_eq!(n, out.len());
/// ```
pub fn project_jsonl_write<W: Write>(
    json: &[u8],
    plan: &ProjectPlan,
    mut w: W,
) -> Result<usize, Error> {
    let mut written = 0usize;
    project_each(json, plan, |_, card| {
        write_card_line(&mut w, card, &mut written)
    })?;
    Ok(written)
}

/// NDJSON thin cards: each object in `array_path` → one line with selected fields.
pub fn project_object_fields_jsonl_write<W: Write>(
    json: &[u8],
    array_path: &str,
    fields: &[&str],
    w: W,
) -> Result<usize, Error> {
    let plan = plan_object_fields(array_path, fields)?;
    project_jsonl_write(json, &plan, w)
}

/// Indexed NDJSON list projection (reuse side-tables).
pub fn project_jsonl_write_indexed<W: Write>(
    doc: &IndexedDocument<'_>,
    plan: &ProjectPlan,
    mut w: W,
) -> Result<usize, Error> {
    let mut written = 0usize;
    project_each_indexed(doc, plan, |_, card| {
        write_card_line(&mut w, card, &mut written)
    })?;
    Ok(written)
}

fn write_card_line<W: Write>(w: &mut W, card: &[u8], written: &mut usize) -> Result<(), Error> {
    w.write_all(card).map_err(|_| Error::InvalidJsonSyntax {
        pos: 0,
        msg: "Write error while emitting JSONL card",
    })?;
    w.write_all(b"\n").map_err(|_| Error::InvalidJsonSyntax {
        pos: 0,
        msg: "Write error while emitting JSONL newline",
    })?;
    *written = written.saturating_add(card.len()).saturating_add(1);
    Ok(())
}

fn project_each_inner<F>(
    json: &[u8],
    plan: &ProjectPlan,
    index: Option<&IndexedDocument<'_>>,
    mut f: F,
) -> Result<(), Error>
where
    F: FnMut(usize, &[u8]) -> Result<(), Error>,
{
    let (array_path, each) = peel_list_each(plan.select()).ok_or(Error::Jmespath {
        msg: "project_each requires a list projection plan (e.g. items[*].{…} or [*].…)",
    })?;

    let (open, end) = resolve_array_open_end(json, &array_path, index)?;
    let omit_nulls = true;
    let mut ctx = emit::EmitCtx::new(plan, index);
    let mut card = Vec::new();
    let mut out_index = 0usize;

    emit::for_each_array_element(json, open, end, index, |_, s, e| {
        card.clear();
        emit::emit_value(json, s, e, each, &mut ctx, &mut card)?;
        if omit_nulls && is_json_null_value(&card) {
            return Ok(());
        }
        f(out_index, &card)?;
        out_index += 1;
        Ok(())
    })
}

fn resolve_array_open_end(
    json: &[u8],
    array_path: &str,
    index: Option<&IndexedDocument<'_>>,
) -> Result<(usize, usize), Error> {
    let (start, end) = if array_path.is_empty() {
        let start = skip_whitespace(json, 0);
        let end = skip_value(json, start)?;
        (start, end)
    } else {
        let segs = parse_path(array_path);
        if let Some(doc) = index {
            match doc.find_offsets(&segs) {
                Ok(span) => span,
                Err(_) => find_value_offsets(json, &segs)?,
            }
        } else {
            find_value_offsets(json, &segs)?
        }
    };
    let open = skip_whitespace(json, start);
    if open >= json.len() || json[open] != b'[' {
        return Err(Error::TypeMismatch {
            expected: "array",
            found: match json.get(open).copied() {
                Some(b'{') => "object",
                Some(b'"') => "string",
                Some(b't') | Some(b'f') => "bool",
                Some(b'n') => "null",
                Some(b'-') | Some(b'0'..=b'9') => "number",
                _ => "unknown",
            },
        });
    }
    Ok((open, end))
}

/// Peel `path[*].card` / `[*].card` into `(array_path, each_expr)`.
///
/// Supports nested field chains (`meta.rows[*].…`). Returns `None` for filters,
/// slices, flatten `[]`, multi-select lists, or non-list roots.
fn peel_list_each(expr: &SelectExpr) -> Option<(String, &SelectExpr)> {
    match expr {
        SelectExpr::Paren(inner) => peel_list_each(inner),
        SelectExpr::Array(ArraySelect::Each(child)) => Some((String::new(), child.as_ref())),
        SelectExpr::Sub(left, right) => match right.as_ref() {
            SelectExpr::Array(ArraySelect::Each(child)) => {
                let path = field_chain_path(left)?;
                Some((path, child.as_ref()))
            }
            SelectExpr::Sub(_, _) | SelectExpr::Paren(_) | SelectExpr::Array(_) => {
                let (suffix, child) = peel_list_each(right)?;
                let prefix = field_chain_path(left)?;
                let full = join_dot_path(&prefix, &suffix);
                Some((full, child))
            }
            _ => None,
        },
        _ => None,
    }
}

fn field_chain_path(expr: &SelectExpr) -> Option<String> {
    match expr {
        SelectExpr::Identity | SelectExpr::Current => Some(String::new()),
        SelectExpr::Field(k) | SelectExpr::FieldQuoted(k) => Some(k.clone()),
        SelectExpr::Paren(inner) => field_chain_path(inner),
        SelectExpr::Sub(left, right) => {
            let a = field_chain_path(left)?;
            let b = field_chain_path(right)?;
            Some(join_dot_path(&a, &b))
        }
        _ => None,
    }
}

fn join_dot_path(prefix: &str, suffix: &str) -> String {
    if prefix.is_empty() {
        suffix.to_string()
    } else if suffix.is_empty() {
        prefix.to_string()
    } else {
        format!("{prefix}.{suffix}")
    }
}

fn is_json_null_value(v: &[u8]) -> bool {
    let s = skip_whitespace(v, 0);
    v.get(s..s + 4) == Some(b"null")
        && (s + 4 >= v.len()
            || matches!(
                v[s + 4],
                b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}'
            ))
}

// ── Parallel auto-pick ───────────────────────────────────────────────────────

/// Heuristic: prefer Rayon list emit when per-element work is likely **CPU-bound**.
///
/// Returns **true** when the plan walks filters, function calls, comparisons that
/// feed list work, flatten, or multi-select fields that are not pure key lookups.
/// Returns **false** for thin pure-field multi-select cards (`items[*].{id:id,t:t}`)
/// and simple keep-lists — those are usually **memory-bound** and sequential
/// indexed emit often wins (see README heavy-vs-thin parallel tables).
///
/// Used by [`project_indexed_auto`]. Force parallel with `project_indexed_parallel`
/// (Cargo feature `parallel`) or sequential with [`project_indexed`].
pub fn plan_prefers_parallel(plan: &ProjectPlan) -> bool {
    expr_prefers_parallel(plan.select())
}

fn expr_prefers_parallel(expr: &SelectExpr) -> bool {
    match expr {
        SelectExpr::Call { .. } => true,
        SelectExpr::Array(ArraySelect::Filter { .. }) => true,
        SelectExpr::Array(ArraySelect::Each(c)) => expr_prefers_parallel(c),
        SelectExpr::Array(ArraySelect::Slice { each, .. }) => expr_prefers_parallel(each),
        SelectExpr::Array(ArraySelect::Indices(map)) => map.values().any(expr_prefers_parallel),
        SelectExpr::MultiSelectHash(fields) => fields.iter().any(|f| match &f.expr {
            SelectExpr::Field(_) | SelectExpr::FieldQuoted(_) | SelectExpr::Identity
            | SelectExpr::Current | SelectExpr::Literal(_) => false,
            other => expr_prefers_parallel(other),
        }),
        SelectExpr::MultiSelectList(items) => items.iter().any(expr_prefers_parallel),
        SelectExpr::Sub(l, r) | SelectExpr::Pipe(l, r) | SelectExpr::And(l, r)
        | SelectExpr::Or(l, r) => expr_prefers_parallel(l) || expr_prefers_parallel(r),
        SelectExpr::Cmp { left, right, .. } => {
            expr_prefers_parallel(left) || expr_prefers_parallel(right)
        }
        SelectExpr::Flatten(i)
        | SelectExpr::Not(i)
        | SelectExpr::Paren(i)
        | SelectExpr::Expref(i)
        | SelectExpr::ObjectProjection(i) => expr_prefers_parallel(i),
        SelectExpr::Object(obj) => obj.fields.values().any(expr_prefers_parallel),
        SelectExpr::Identity
        | SelectExpr::Current
        | SelectExpr::Field(_)
        | SelectExpr::FieldQuoted(_)
        | SelectExpr::Literal(_) => false,
    }
}

/// Project with side-tables, enabling parallel `[*]` only when [`plan_prefers_parallel`].
///
/// Without feature `parallel`, always sequential (same as [`project_indexed`]).
/// Thin pure-field cards stay sequential even with `parallel` enabled.
pub fn project_indexed_auto(
    doc: &IndexedDocument<'_>,
    plan: &ProjectPlan,
) -> Result<Vec<u8>, Error> {
    #[cfg(feature = "parallel")]
    if plan_prefers_parallel(plan) {
        return project_indexed_parallel(doc, plan);
    }
    project_indexed(doc, plan)
}

/// Build plan indexes, then [`project_indexed_auto`] (seq vs parallel by heuristic).
///
/// Contrast [`project_auto_indexed`], which always projects **sequentially** after
/// indexing. This entry point is the “just do the right thing for bulk list project”
/// convenience when feature `parallel` may or may not be enabled.
pub fn project_parallel_auto(json: &[u8], plan: &ProjectPlan) -> Result<Vec<u8>, Error> {
    let mut doc = IndexedDocument::empty(json);
    doc.index_for_plan(plan)?;
    project_indexed_auto(&doc, plan)
}

/// Stream projection into any [`Write`] without buffering the full output.
///
/// Pipe/function intermediates are **spans** into the document or a reclaimable
/// arena — not full intermediate `Vec` trees. The final document is written
/// incrementally to `w`.
///
/// Returns the number of bytes written.
pub fn project_write<W: Write>(json: &[u8], plan: &ProjectPlan, w: W) -> Result<usize, Error> {
    let mut sink = WSink::new(w);
    project_to_sink(json, plan, &mut sink)?;
    Ok(sink.written)
}

/// Exact projected byte length without retaining the output (counting sink).
///
/// Prefer this over allocating a full `Vec` when only capacity / size ratio matters.
/// Uses the same span/arena evaluator as [`project`] (no full intermediate trees).
pub fn projected_len(json: &[u8], plan: &ProjectPlan) -> Result<usize, Error> {
    let mut c = CountSink::default();
    project_to_sink(json, plan, &mut c)?;
    Ok(c.len)
}

fn project_to_sink(json: &[u8], plan: &ProjectPlan, out: &mut impl EmitOut) -> Result<(), Error> {
    project_to_sink_inner(json, plan, None, out)
}

fn project_to_sink_indexed(
    doc: &IndexedDocument<'_>,
    plan: &ProjectPlan,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    project_to_sink_inner(doc.as_bytes(), plan, Some(doc), out)
}

fn project_to_sink_inner(
    json: &[u8],
    plan: &ProjectPlan,
    index: Option<&IndexedDocument<'_>>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    let start = skip_whitespace(json, 0);
    if start >= json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos: start,
            msg: "Unexpected EOF",
        });
    }
    // Sparse-project critical path: do **not** `skip_value` the entire root document
    // just to compute `end`. On a 300 MiB `{"products":[...]}` catalog that alone
    // was ~0.5–0.8s before any real work. Scanners stop at matching `}`/`]`, so an
    // upper bound of (trimmed) `json.len()` is safe for descent. Only Identity /
    // Current root plans need an exact value end (to avoid trailing junk).
    let end = match plan.select() {
        crate::project::select::SelectExpr::Identity
        | crate::project::select::SelectExpr::Current => skip_value(json, start)?,
        _ => {
            let mut end = json.len();
            while end > start && matches!(json[end - 1], b' ' | b'\t' | b'\n' | b'\r') {
                end -= 1;
            }
            end
        }
    };
    let mut ctx = emit::EmitCtx::new(plan, index);
    emit::emit_value(json, start, end, plan.select(), &mut ctx, out)
}

/// Convenience: [`ProjectPlan::from_paths`] then [`project`].
pub fn project_paths(json: &[u8], paths: &[&str]) -> Result<Vec<u8>, Error> {
    project(json, &ProjectPlan::from_paths(paths)?)
}

/// Convenience: [`ProjectPlan::from_jmespath`] then [`project`].
pub fn project_jmespath(json: &[u8], expr: &str) -> Result<Vec<u8>, Error> {
    project(json, &ProjectPlan::from_jmespath(expr)?)
}

/// Ballpark capacity hint for flat keep-lists (not exact projector output).
pub fn estimate_projected_len(json: &[u8], paths: &[&str]) -> Result<usize, Error> {
    if paths.is_empty() {
        return Ok(2);
    }
    let mut total = 1usize;
    for (i, p) in paths.iter().enumerate() {
        if i > 0 {
            total += 1;
        }
        let segs = parse_path(p);
        let val = find_value(json, &segs)?;
        total += key_wire_len(&segs);
        total += 1;
        total += val.len();
    }
    total += 1;
    Ok(total)
}

/// Sum of on-wire value lengths for `paths`.
pub fn estimate_values_len(json: &[u8], paths: &[&str]) -> Result<usize, Error> {
    let mut total = 0usize;
    for p in paths {
        total += find_value(json, &parse_path(p))?.len();
    }
    Ok(total)
}

fn key_wire_len(segs: &[PathSegment<'_>]) -> usize {
    for seg in segs.iter().rev() {
        if let PathSegment::Key(k) = seg {
            return 2 + k.len();
        }
    }
    2
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_top_level_keys() {
        let json = br#"{"id":1,"title":"x","blob":true}"#;
        assert_eq!(
            project_paths(json, &["id", "title"]).unwrap(),
            br#"{"id":1,"title":"x"}"#
        );
    }

    #[test]
    fn project_jmespath_multi_hash_rename() {
        // JMESPath returns the projected array (not re-wrapped under "products").
        let json = br#"{"products":[{"id":1,"t":"a"},{"id":2,"t":"b"}]}"#;
        let out = project_jmespath(json, "products[*].{id: id, title: t}").unwrap();
        assert_eq!(
            out,
            br#"[{"id":1,"title":"a"},{"id":2,"title":"b"}]"#
        );
    }

    #[test]
    fn project_jmespath_slice() {
        // Slice projection yields the array value itself.
        let json = br#"{"a":[10,20,30,40]}"#;
        let out = project_jmespath(json, "a[1:3]").unwrap();
        assert_eq!(out, br#"[20,30]"#);
    }

    #[test]
    fn project_jmespath_negative_index() {
        let json = br#"{"a":[10,20,30]}"#;
        assert_eq!(project_jmespath(json, "a[-1]").unwrap(), b"30");
        assert_eq!(project_jmespath(json, "a[-2]").unwrap(), b"20");
    }

    #[test]
    fn project_jmespath_filter() {
        let json = br#"{"items":[{"n":1,"ok":true},{"n":2,"ok":false},{"n":3,"ok":true}]}"#;
        let out = project_jmespath(json, "items[?ok == `true`].n").unwrap();
        assert_eq!(out, br#"[1,3]"#);
    }

    #[test]
    fn project_jmespath_functions() {
        let json = br#"{"a":[3,1,2],"t":"Hello"}"#;
        assert_eq!(project_jmespath(json, "length(a)").unwrap(), b"3");
        assert_eq!(project_jmespath(json, "length(t)").unwrap(), b"5");
        assert_eq!(project_jmespath(json, "sort(a)").unwrap(), br#"[1,2,3]"#);
        assert_eq!(project_jmespath(json, "max(a)").unwrap(), b"3");
        assert_eq!(
            project_jmespath(json, "starts_with(t, 'He')").unwrap(),
            b"true"
        );
        assert_eq!(project_jmespath(json, "type(a)").unwrap(), br#""array""#);
    }

    #[test]
    fn project_jmespath_paren_and_or() {
        let json = br#"{"x":1,"y":0}"#;
        assert_eq!(
            project_jmespath(json, "(x == `1`) && (y == `0`)").unwrap(),
            b"true"
        );
        assert_eq!(project_jmespath(json, "x == `2` || y == `0`").unwrap(), b"true");
        assert_eq!(project_jmespath(json, "!(x == `2`)").unwrap(), b"true");
    }

    #[test]
    fn project_paths_keeps_nesting_wrapper() {
        // Keep-list paths preserve ancestor keys (unlike pure JMESPath result).
        let json = br#"{"products":[{"id":1,"t":"a"},{"id":2,"t":"b"}]}"#;
        let out = project_paths(json, &["products[].id", "products[].t"]).unwrap();
        assert_eq!(
            out,
            br#"{"products":[{"id":1,"t":"a"},{"id":2,"t":"b"}]}"#
        );
    }

    #[test]
    fn project_jmespath_pipe_flatten() {
        let json = br#"{"x":[[1,2],[3],[4,5]]}"#;
        let out = project_jmespath(json, "x | []").unwrap();
        assert_eq!(out, br#"[1,2,3,4,5]"#);
    }

    #[test]
    fn project_jmespath_multi_list() {
        let json = br#"{"id":1,"title":"h"}"#;
        let out = project_jmespath(json, "[id, title]").unwrap();
        assert_eq!(out, br#"[1,"h"]"#);
    }

    #[test]
    fn project_jmespath_literal_field() {
        let json = br#"{"id":1}"#;
        let out = project_jmespath(json, "{id: id, source: 'teefury'}").unwrap();
        assert_eq!(out, br#"{"id":1,"source":"teefury"}"#);
    }

    #[test]
    fn project_array_wildcard_paths() {
        let json = br#"{"products":[{"id":1,"t":"a"},{"id":2,"t":"b"}]}"#;
        let out = project_paths(json, &["products[].id"]).unwrap();
        assert_eq!(out, br#"{"products":[{"id":1},{"id":2}]}"#);
    }

    #[test]
    fn project_pretty() {
        let plan = ProjectPlan::from_paths(&["a", "b"])
            .unwrap()
            .style(ProjectStyle::Pretty { indent: 2 });
        let out = project(br#"{"a":1,"b":2}"#, &plan).unwrap();
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            "{\n  \"a\": 1,\n  \"b\": 2\n}"
        );
    }

    #[test]
    fn project_preserve_source() {
        let json = br#"{ "id" : 1 , "drop": 2 }"#;
        let plan = ProjectPlan::from_paths(&["id"])
            .unwrap()
            .style(ProjectStyle::PreserveSource);
        assert_eq!(project(json, &plan).unwrap(), br#"{ "id" : 1 }"#);
    }

    #[test]
    fn project_nested_merge() {
        let json = br#"{"user":{"name":"a","age":9,"city":"z"},"x":0}"#;
        assert_eq!(
            project_paths(json, &["user.name", "user.age"]).unwrap(),
            br#"{"user":{"name":"a","age":9}}"#
        );
    }

    #[test]
    fn project_write_streams_and_counts() {
        let json = br#"{"id":1,"title":"x","blob":[1,2,3]}"#;
        let plan = ProjectPlan::from_paths(&["id", "title"]).unwrap();
        let mut buf = Vec::new();
        let n = project_write(json, &plan, &mut buf).unwrap();
        assert_eq!(n, buf.len());
        assert_eq!(buf, br#"{"id":1,"title":"x"}"#);
        assert_eq!(projected_len(json, &plan).unwrap(), n);
    }

    #[test]
    fn project_indexed_matches_project() {
        let json = br#"{"products":[{"id":1},{"id":2}]}"#;
        let plan = ProjectPlan::from_paths(&["products[].id"]).unwrap();
        let doc = crate::IndexedDocument::build(json, &["products"]).unwrap();
        assert_eq!(
            project_indexed(&doc, &plan).unwrap(),
            project(json, &plan).unwrap()
        );
    }

    #[test]
    fn flatten_projection_vs_list_projection() {
        let json = br#"{"reservations":[{"instances":[{"foo":1},{"foo":2}]}]}"#;
        assert_eq!(
            project_jmespath(json, "reservations[].instances[].foo").unwrap(),
            br#"[1,2]"#
        );
        assert_eq!(
            project_jmespath(json, "reservations[*].instances[*].foo").unwrap(),
            br#"[[1,2]]"#
        );
    }

    #[test]
    fn pure_pipe_matches_direct_path() {
        let json = br#"{"a":{"b":{"c":42}}}"#;
        assert_eq!(
            project_jmespath(json, "a.b | c").unwrap(),
            project_jmespath(json, "a.b.c").unwrap()
        );
    }

    #[test]
    fn plan_array_paths_for_index() {
        let plan = ProjectPlan::from_paths(&["products[0].id", "products[2].title"]).unwrap();
        assert_eq!(plan.array_paths_for_index(), vec!["products".to_string()]);

        let plan = ProjectPlan::from_jmespath("products[*].{id: id, title: title}").unwrap();
        assert_eq!(plan.array_paths_for_index(), vec!["products".to_string()]);
    }

    /// Pure-field multi-select must be a single object walk: correctness for renames,
    /// missing→null, field order, and any key names (not catalog-specific).
    #[test]
    fn multi_select_pure_fields_one_pass_semantics() {
        let json = br#"{"z":9,"a":1,"noise":true,"b":"two","c":{"nested":3}}"#;

        // Declared output order, not document order; rename b→alias.
        assert_eq!(
            project_jmespath(json, "{first: a, alias: b, last: z}").unwrap(),
            br#"{"first":1,"alias":"two","last":9}"#
        );

        // Missing keys → null (JMESPath multi-select).
        assert_eq!(
            project_jmespath(json, "{a: a, missing: nope, b: b}").unwrap(),
            br#"{"a":1,"missing":null,"b":"two"}"#
        );

        // Same source field twice.
        assert_eq!(
            project_jmespath(json, "{x: a, y: a}").unwrap(),
            br#"{"x":1,"y":1}"#
        );

        // Nested expr still works (mixed path; not pure-field one-pass).
        assert_eq!(
            project_jmespath(json, "{a: a, n: c.nested}").unwrap(),
            br#"{"a":1,"n":3}"#
        );

        // Array of heterogeneous objects — one-pass per element.
        let arr = br#"[{"id":1,"t":"a","x":9},{"id":2,"t":"b"},{"id":3,"t":"c","x":1}]"#;
        assert_eq!(
            project_jmespath(arr, "[*].{id: id, t: t, x: x}").unwrap(),
            br#"[{"id":1,"t":"a","x":9},{"id":2,"t":"b","x":null},{"id":3,"t":"c","x":1}]"#
        );
    }

    #[test]
    fn project_object_fields_generic_shapes() {
        // Root array of objects
        let root = br#"[{"a":1,"b":2,"c":3},{"a":4,"b":5,"c":6}]"#;
        assert_eq!(
            project_object_fields(root, "", &["a", "c"]).unwrap(),
            br#"[{"a":1,"c":3},{"a":4,"c":6}]"#
        );
        // Nested array under arbitrary keys
        let nested = br#"{"meta":{"rows":[{"x":9,"y":8,"z":7}]}}"#;
        assert_eq!(
            project_object_fields(nested, "meta.rows", &["z", "x"]).unwrap(),
            br#"[{"z":7,"x":9}]"#
        );
    }

    #[test]
    fn project_each_and_jsonl_match_array_project() {
        let json = br#"{"items":[{"id":1,"t":"a","x":9},{"id":2,"t":"b"},{"id":3,"t":"c","x":1}]}"#;
        let plan = ProjectPlan::from_jmespath("items[*].{id: id, t: t}").unwrap();
        let as_array = project(json, &plan).unwrap();

        let mut cards = Vec::new();
        project_each(json, &plan, |i, card| {
            cards.push((i, card.to_vec()));
            Ok(())
        })
        .unwrap();
        assert_eq!(cards.len(), 3);
        assert_eq!(cards[0].1, br#"{"id":1,"t":"a"}"#);
        assert_eq!(cards[2].1, br#"{"id":3,"t":"c"}"#);

        // Reconstruct array from cards and compare to project().
        let mut rebuilt = Vec::from(b"[".as_slice());
        for (i, (_, c)) in cards.iter().enumerate() {
            if i > 0 {
                rebuilt.push(b',');
            }
            rebuilt.extend_from_slice(c);
        }
        rebuilt.push(b']');
        assert_eq!(rebuilt, as_array);

        let mut jsonl = Vec::new();
        let n = project_jsonl_write(json, &plan, &mut jsonl).unwrap();
        assert_eq!(n, jsonl.len());
        assert_eq!(
            &jsonl[..],
            b"{\"id\":1,\"t\":\"a\"}\n{\"id\":2,\"t\":\"b\"}\n{\"id\":3,\"t\":\"c\"}\n"
        );

        // Nested path + object_fields_each
        let nested = br#"{"meta":{"rows":[{"z":7,"x":9,"y":1}]}}"#;
        let mut got = None;
        project_object_fields_each(nested, "meta.rows", &["z", "x"], |_, card| {
            got = Some(card.to_vec());
            Ok(())
        })
        .unwrap();
        assert_eq!(got.unwrap(), br#"{"z":7,"x":9}"#);

        // Root array
        let root = br#"[{"a":1,"b":2},{"a":3,"b":4}]"#;
        let mut jsonl = Vec::new();
        project_object_fields_jsonl_write(root, "", &["a"], &mut jsonl).unwrap();
        assert_eq!(&jsonl[..], b"{\"a\":1}\n{\"a\":3}\n");

        // Indexed path
        let mut doc = crate::IndexedDocument::empty(json);
        doc.index_for_plan(&plan).unwrap();
        let mut n = 0usize;
        project_each_indexed(&doc, &plan, |_, _| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn plan_prefers_parallel_heuristic() {
        let thin = ProjectPlan::from_jmespath("items[*].{id: id, title: title}").unwrap();
        assert!(
            !plan_prefers_parallel(&thin),
            "pure-field multi-select should stay sequential"
        );
        let heavy = ProjectPlan::from_jmespath(
            "records[*].{id: id, n: length(scores[?@ > `0.5`])}",
        )
        .unwrap();
        assert!(
            plan_prefers_parallel(&heavy),
            "filter + length should prefer parallel"
        );
        let filtered = ProjectPlan::from_jmespath("items[?id > `0`].id").unwrap();
        assert!(plan_prefers_parallel(&filtered));
    }

    #[test]
    fn project_indexed_auto_thin_matches_sequential() {
        let mut json = String::from(r#"{"rows":["#);
        for i in 0..80 {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!(r#"{{"id":{i},"name":"n{i}"}}"#));
        }
        json.push_str("]}");
        let bytes = json.as_bytes();
        let plan = ProjectPlan::from_jmespath("rows[*].{id: id, name: name}").unwrap();
        let mut doc = crate::IndexedDocument::empty(bytes);
        doc.index_for_plan(&plan).unwrap();
        let seq = project_indexed(&doc, &plan).unwrap();
        let auto = project_indexed_auto(&doc, &plan).unwrap();
        assert_eq!(seq, auto);
        let prep = project_parallel_auto(bytes, &plan).unwrap();
        assert_eq!(seq, prep);
    }

    #[cfg(feature = "parallel")]
    #[test]
    fn project_parallel_matches_sequential_on_array() {
        let mut json = String::from(r#"{"rows":["#);
        for i in 0..200 {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!(r#"{{"id":{i},"name":"n{i}","noise":true}}"#));
        }
        json.push_str("]}");
        let bytes = json.as_bytes();
        let seq = project_object_fields(bytes, "rows", &["id", "name"]).unwrap();
        let par = project_object_fields_parallel(bytes, "rows", &["id", "name"]).unwrap();
        assert_eq!(seq, par);
        let plan = ProjectPlan::from_jmespath("rows[*].{id: id, name: name}").unwrap();
        let mut doc = crate::IndexedDocument::empty(bytes);
        doc.index_for_plan(&plan).unwrap();
        assert_eq!(project_indexed_parallel(&doc, &plan).unwrap(), seq);
    }

    #[test]
    fn project_auto_indexed_mid_element() {
        let mut json = String::from(r#"{"products":["#);
        for i in 0..100 {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!(r#"{{"id":{i},"title":"T{i}"}}"#));
        }
        json.push_str("]}");
        let bytes = json.as_bytes();
        let plan = ProjectPlan::from_paths(&["products[50].id"]).unwrap();
        let out = project_auto_indexed(bytes, &plan).unwrap();
        assert_eq!(out, br#"{"products":{"id":50}}"#);

        let mut doc = crate::IndexedDocument::empty(bytes);
        let out2 = project_indexed_prepare(&mut doc, &plan).unwrap();
        assert_eq!(out2, out);
        assert!(doc.array_len(&crate::parse_path("products")).unwrap() >= 100);
    }

    /// Sparse single-index correctness across first / mid / last / negative.
    #[test]
    fn single_index_short_circuit_correctness() {
        let mut json = String::from(r#"{"products":["#);
        for i in 0..200 {
            if i > 0 {
                json.push(',');
            }
            json.push_str(&format!(
                r#"{{"id":{i},"title":"P{i}","pad":"{pad}"}}"#,
                pad = "x".repeat(64)
            ));
        }
        json.push_str("]}");
        let bytes = json.as_bytes();

        assert_eq!(project_jmespath(bytes, "products[0].id").unwrap(), b"0");
        assert_eq!(project_jmespath(bytes, "products[99].id").unwrap(), b"99");
        assert_eq!(project_jmespath(bytes, "products[199].id").unwrap(), b"199");
        assert_eq!(project_jmespath(bytes, "products[-1].id").unwrap(), b"199");
        assert_eq!(
            project_jmespath(bytes, "products[-1].title").unwrap(),
            br#""P199""#
        );

        // Multi-select list of sparse index projects.
        let multi =
            project_jmespath(bytes, "[products[0].id, products[2].id, products[-1].id]").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&multi).unwrap();
        assert_eq!(v, serde_json::json!([0, 2, 199]));

        // Keep-list + index: first product only, structure preserved.
        let mut doc = crate::IndexedDocument::empty(bytes);
        doc.index_array_str("products").unwrap();
        let plan = ProjectPlan::from_paths(&["products[0].id", "products[0].title"]).unwrap();
        let out = project_indexed(&doc, &plan).unwrap();
        assert_eq!(out, project(bytes, &plan).unwrap());
        assert_eq!(out, br#"{"products":{"id":0,"title":"P0"}}"#);
    }


    #[test]
    fn p0_synthetic_sparse_index_is_fast() {
        // ~80 MiB catalog: if we accidentally full-scan, this exceeds 50ms easily.
        let mut json = String::from(r#"{"products":["#);
        let pad = "x".repeat(256);
        for i in 0..50_000 {
            if i > 0 { json.push(','); }
            json.push_str(&format!(r#"{{"id":{i},"title":"T{i}","pad":"{pad}"}}"#));
        }
        json.push_str("]}");
        let bytes = json.as_bytes();
        let t0 = std::time::Instant::now();
        let out = project_jmespath(bytes, "products[0].id").unwrap();
        let dt = t0.elapsed();
        assert_eq!(out, b"0");
        // Full scan of 50k * ~300B ≈ 15MB+ of skip_value work would be tens of ms;
        // true sparse should be well under 5ms on a quiet machine.
        assert!(
            dt.as_millis() < 25,
            "products[0].id took {dt:?} — sparse short-circuit not active?"
        );

        let t1 = std::time::Instant::now();
        let out = project_paths(bytes, &["products[0].id", "products[0].title"]).unwrap();
        let dt1 = t1.elapsed();
        assert_eq!(out, br#"{"products":{"id":0,"title":"T0"}}"#);
        assert!(
            dt1.as_millis() < 25,
            "keep-list products[0] took {dt1:?}"
        );

        let mut doc = crate::IndexedDocument::empty(bytes);
        doc.index_array_str("products").unwrap();
        let plan = ProjectPlan::from_paths(&["products[24999].id"]).unwrap();
        let t2 = std::time::Instant::now();
        let out = project_indexed(&doc, &plan).unwrap();
        let dt2 = t2.elapsed();
        assert_eq!(out, br#"{"products":{"id":24999}}"#);
        assert!(
            dt2.as_millis() < 15,
            "indexed products[24999] took {dt2:?}"
        );
    }

    #[test]
    fn transform_pipeline_rename_drop_inject() {
        use crate::project::transform::{Transform, TransformPipeline};
        let json = br#"{"id":1,"title":"Hat","noise":true}"#;
        let out = TransformPipeline::new()
            .then(Transform::KeepPaths(&["id", "title"]))
            .then(Transform::Rename {
                from: "title",
                to: "name",
            })
            .then(Transform::Inject {
                key: "source",
                value: br#""api""#,
            })
            .then(Transform::Drop("id"))
            .apply(json)
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["name"], "Hat");
        assert_eq!(v["source"], "api");
        assert!(v.get("id").is_none());
        assert!(v.get("noise").is_none());
    }

    #[test]
    fn preserve_source_array_spacing() {
        let json = br#"[ 1 , 2 , 3 ]"#;
        // Identity array projection via jmes
        let plan = ProjectPlan::from_jmespath("@")
            .unwrap()
            .style(ProjectStyle::PreserveSource);
        // Full identity copies raw span including spaces
        let out = project(json, &plan).unwrap();
        assert_eq!(out, json);
    }

    #[test]
    fn serde_accepts_projection() {
        let json = br#"{"id":7,"title":"Hat","images":[1,2,3],"meta":{"x":1}}"#;
        let out = project_paths(json, &["id", "title", "meta.x"]).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["id"], 7);
        assert!(v.get("images").is_none());
    }
}
