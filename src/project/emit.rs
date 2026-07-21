//! Projection emitter: walk input spans, write output per [`SelectExpr`].
//!
//! # Span / arena evaluation
//!
//! Pure projections (`field`, `index`, identity chains) resolve to document spans
//! with **no intermediate tree copies**. Pipes and `Sub` evaluate the left side to a
//! span (document or reclaimable arena), then apply the right side. Only synthesized
//! JSON (multi-select, many functions) grows the arena. Final output is streamed via
//! [`EmitOut`].
//!
//! # Compact bulk path
//!
//! Default [`ProjectStyle::Compact`] is the production bulk style (thin cards, list
//! projections, one-pass multi-select). On that style the emitter **does not** maintain
//! pretty indent depth, does not call newline/indent helpers, and closes containers with
//! a single `]` / `}` byte. Pretty and PreserveSource keep full bookkeeping; Compact
//! streaming Each and pure-field multi-select write `{k:v,...}` / `[...]` with only
//! commas and value spans — the path that matters for multi-megabyte catalog rewrites.

use crate::convert::escape_json_key;
use crate::error::Error;
use crate::index::IndexedDocument;
use crate::project::plan::{MissingPolicy, ProjectPlan, ProjectStyle};
use crate::project::select::{
    resolve_index, resolve_slice, ArraySelect, CmpOp, HashField, ObjectSelect, SelectExpr,
};
use crate::project::sink::EmitOut;
use crate::scan::{find_string_end, skip_value, skip_whitespace};

pub(crate) struct EmitCtx<'a> {
    pub plan: &'a ProjectPlan,
    pub depth: usize,
    /// Optional structural indexes for O(1) array/object hops.
    pub index: Option<&'a IndexedDocument<'a>>,
    /// When true (and `parallel` feature + Compact + large side-table), `[*]` may use Rayon.
    #[cfg(feature = "parallel")]
    pub allow_parallel: bool,
}

impl<'a> EmitCtx<'a> {
    pub fn new(plan: &'a ProjectPlan, index: Option<&'a IndexedDocument<'a>>) -> Self {
        Self {
            plan,
            depth: 0,
            index,
            #[cfg(feature = "parallel")]
            allow_parallel: false,
        }
    }
}

/// Minimum array length before parallel `[*]` emission is considered (avoids pool overhead).
#[cfg(feature = "parallel")]
const PARALLEL_EACH_MIN_ELEMS: usize = 64;

pub(crate) fn emit_value(
    json: &[u8],
    start: usize,
    end: usize,
    expr: &SelectExpr,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    match expr {
        SelectExpr::Identity | SelectExpr::Current => {
            out.emit_bytes(&json[start..end])?;
            Ok(())
        }
        SelectExpr::Field(key) | SelectExpr::FieldQuoted(key) => {
            let wire = escape_json_key(key);
            match find_object_value_span_idx(json, start, end, wire.as_bytes(), ctx.index) {
                Ok((s, e)) => {
                    out.emit_bytes(&json[s..e])?;
                    Ok(())
                }
                Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. }) if soft_null(ctx) => {
                    out.emit_bytes(b"null")?;
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        SelectExpr::Literal(bytes) => {
            out.emit_bytes(bytes)?;
            Ok(())
        }
        SelectExpr::Object(sel) => emit_object(json, start, end, sel, ctx, out),
        SelectExpr::Array(sel) => emit_array(json, start, end, sel, ctx, out),
        SelectExpr::MultiSelectHash(fields) => emit_multi_hash(json, start, end, fields, ctx, out),
        SelectExpr::MultiSelectList(items) => emit_multi_list(json, start, end, items, ctx, out),
        SelectExpr::Pipe(left, right) => {
            // Pure left: open-ended descent when right continues (same as Sub).
            if is_pure_focus_expr(left)
                && !matches!(right.as_ref(), SelectExpr::Identity | SelectExpr::Current)
            {
                if let Ok(Some(s)) = resolve_focus_start_idx(json, start, end, left, ctx.index) {
                    return emit_value(json, s, end, right, ctx, out);
                }
            }
            if let Ok(Some((s, e))) = resolve_focus_idx(json, start, end, left, ctx.index) {
                return emit_value(json, s, e, right, ctx, out);
            }
            // Synthesized left: one intermediate buffer (not a full DOM), then right.
            let mut mid = Vec::new();
            emit_value(json, start, end, left, ctx, &mut mid)?;
            let m0 = skip_whitespace(&mid, 0);
            if m0 >= mid.len() {
                return Err(Error::InvalidJsonSyntax {
                    pos: 0,
                    msg: "Empty pipe intermediate",
                });
            }
            let m1 = skip_value(&mid, m0)?;
            emit_value(&mid, m0, m1, right, ctx, out)
        }
        SelectExpr::Flatten(inner) => {
            if let Ok(Some((s, e))) = resolve_focus_idx(json, start, end, inner, ctx.index) {
                return flatten_emit_on(json, s, e, ctx, out);
            }
            let mut mid = Vec::new();
            emit_value(json, start, end, inner, ctx, &mut mid)?;
            flatten_emit(&mid, ctx, out)
        }
        SelectExpr::Sub(left, right) => {
            // Open-ended pure descent: only need the *start* of intermediate containers
            // (e.g. `products` array open) so we never `skip_value` a multi-hundred-MB array
            // just to discover its end before `products[0]`.
            //
            // Note: `right` is `&Box<SelectExpr>`; match on `**right` / `right.as_ref()`.
            let right_is_identity =
                matches!(right.as_ref(), SelectExpr::Identity | SelectExpr::Current);
            if is_pure_focus_expr(left) && !right_is_identity {
                match resolve_focus_start_idx(json, start, end, left, ctx.index) {
                    Ok(Some(s)) => {
                        return emit_value(json, s, end, right, ctx, out);
                    }
                    Ok(None) | Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. })
                        if soft_null(ctx) =>
                    {
                        out.emit_bytes(b"null")?;
                        return Ok(());
                    }
                    Ok(None) => return Err(Error::PathNotFound),
                    Err(e) => return Err(e),
                }
            }
            // Pure focus with exact span (Identity right, or leaf field copy).
            if is_pure_focus_expr(left) {
                match resolve_focus_idx(json, start, end, left, ctx.index) {
                    Ok(Some((s, e))) => return emit_value(json, s, e, right, ctx, out),
                    Ok(None) | Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. })
                        if soft_null(ctx) =>
                    {
                        out.emit_bytes(b"null")?;
                        return Ok(());
                    }
                    Ok(None) => return Err(Error::PathNotFound),
                    Err(e) => return Err(e),
                }
            }
            // Full left evaluation (sort_by(...), flatten, multi-select, …) into one mid buffer.
            let mut mid = Vec::new();
            match emit_value(json, start, end, left, ctx, &mut mid) {
                Ok(()) => {}
                Err(Error::PathNotFound)
                | Err(Error::TypeMismatch { .. })
                | Err(Error::IndexOutOfBounds { .. })
                    if soft_null(ctx) =>
                {
                    out.emit_bytes(b"null")?;
                    return Ok(());
                }
                Err(e) => return Err(e),
            }
            let m0 = skip_whitespace(&mid, 0);
            if m0 >= mid.len() {
                if soft_null(ctx) {
                    out.emit_bytes(b"null")?;
                    return Ok(());
                }
                return Err(Error::PathNotFound);
            }
            let m1 = skip_value(&mid, m0)?;
            if soft_null(ctx) && trim_json(&mid[m0..m1]) == b"null" {
                out.emit_bytes(b"null")?;
                return Ok(());
            }
            emit_value(&mid, m0, m1, right, ctx, out)
        }
        SelectExpr::Cmp { op, left, right } => {
            let lv = eval_buf(json, start, end, left, ctx)?;
            let rv = eval_buf(json, start, end, right, ctx)?;
            match cmp_values(&lv, &rv, *op) {
                Some(t) => {
                    out.emit_bytes(if t { b"true" } else { b"false" })?;
                    Ok(())
                }
                None => {
                    out.emit_bytes(b"null")?;
                    Ok(())
                }
            }
        }
        SelectExpr::And(a, b) => {
            let av = eval_buf(json, start, end, a, ctx)?;
            if !is_truthy(&av) {
                out.emit_bytes(trim_json(&av))?;
                return Ok(());
            }
            emit_value(json, start, end, b, ctx, out)
        }
        SelectExpr::Or(a, b) => {
            let av = eval_buf(json, start, end, a, ctx)?;
            if is_truthy(&av) {
                out.emit_bytes(trim_json(&av))?;
                return Ok(());
            }
            emit_value(json, start, end, b, ctx, out)
        }
        SelectExpr::Not(inner) => {
            let v = eval_buf(json, start, end, inner, ctx)?;
            out.emit_bytes(if is_truthy(&v) { b"false" } else { b"true" })?;
            Ok(())
        }
        SelectExpr::Call { name, args } => emit_call(json, start, end, name, args, ctx, out),
        SelectExpr::Expref(inner) => emit_value(json, start, end, inner, ctx, out),
        SelectExpr::ObjectProjection(each) => emit_object_projection(json, start, end, each, ctx, out),
        SelectExpr::Paren(inner) => emit_value(json, start, end, inner, ctx, out),
    }
}

fn unwrap_expref(expr: &SelectExpr) -> &SelectExpr {
    match expr {
        SelectExpr::Expref(inner) | SelectExpr::Paren(inner) => unwrap_expref(inner),
        other => other,
    }
}

fn emit_object_projection(
    json: &[u8],
    start: usize,
    end: usize,
    each: &SelectExpr,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    let (_, members, _) = match collect_object_members(json, start, end) {
        Ok(x) => x,
        Err(Error::TypeMismatch { .. }) if soft_null(ctx) => {
            out.emit_bytes(b"null")?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    // JMESPath projections omit null results.
    emit_projection_list(
        members
            .iter()
            .map(|m| (json, m.val_start, m.val_end))
            .collect(),
        each,
        ctx,
        out,
    )
}

/// Emit `[...]` from evaluating `each` on each span; skip JSON null (projection rule).
fn emit_projection_list(
    spans: Vec<(&[u8], usize, usize)>,
    each: &SelectExpr,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    out.emit_byte(b'[')?;
    let mut first = true;
    for (buf, s, e) in spans {
        let mut piece = Vec::new();
        emit_value(buf, s, e, each, ctx, &mut piece)?;
        if trim_json(&piece) == b"null" {
            continue;
        }
        if !first {
            out.emit_byte(b',')?;
        }
        maybe_pretty_newline_indent(ctx, out, true)?;
        ctx.depth += 1;
        out.emit_bytes(&piece)?;
        ctx.depth -= 1;
        first = false;
    }
    emit_close_array(ctx, out)?;
    Ok(())
}

fn soft_null(ctx: &EmitCtx<'_>) -> bool {
    ctx.plan.missing == MissingPolicy::Skip
}

/// Expressions that resolve to a document/arena span without synthesizing JSON.
fn is_pure_focus_expr(expr: &SelectExpr) -> bool {
    match expr {
        SelectExpr::Identity | SelectExpr::Current => true,
        SelectExpr::Field(_) | SelectExpr::FieldQuoted(_) => true,
        SelectExpr::Paren(inner) => is_pure_focus_expr(inner),
        SelectExpr::Sub(l, r) => is_pure_focus_expr(l) && is_pure_focus_expr(r),
        SelectExpr::Object(obj) if obj.len() == 1 => {
            let child = obj.get(obj.keys().next().unwrap()).unwrap();
            is_pure_focus_expr(child)
        }
        SelectExpr::Array(ArraySelect::Indices(map)) if map.len() == 1 => {
            is_pure_focus_expr(map.values().next().unwrap())
        }
        _ => false,
    }
}

fn eval_buf(
    json: &[u8],
    start: usize,
    end: usize,
    expr: &SelectExpr,
    ctx: &mut EmitCtx<'_>,
) -> Result<Vec<u8>, Error> {
    // Pure focus: return a copy of the span only (no full-tree re-encode path).
    if is_pure_focus_expr(expr) {
        if let Ok(Some((s, e))) = resolve_focus_idx(json, start, end, expr, ctx.index) {
            return Ok(json[s..e].to_vec());
        }
    }
    let mut v = Vec::new();
    match emit_value(json, start, end, expr, ctx, &mut v) {
        Ok(()) => Ok(v),
        // Soft failures → JSON null (JMESPath “no value”).
        Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. }) | Err(Error::IndexOutOfBounds { .. })
            if soft_null(ctx) =>
        {
            Ok(b"null".to_vec())
        }
        Err(e) => Err(e),
    }
}

fn resolve_focus_idx(
    json: &[u8],
    start: usize,
    end: usize,
    expr: &SelectExpr,
    index: Option<&IndexedDocument<'_>>,
) -> Result<Option<(usize, usize)>, Error> {
    match expr {
        SelectExpr::Identity | SelectExpr::Current => Ok(Some((start, end))),
        SelectExpr::Paren(inner) => resolve_focus_idx(json, start, end, inner, index),
        SelectExpr::Field(key) | SelectExpr::FieldQuoted(key) => {
            let wire = escape_json_key(key);
            match find_object_value_span_idx(json, start, end, wire.as_bytes(), index) {
                Ok((s, e)) => Ok(Some((s, e))),
                Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. }) => Ok(None),
                Err(e) => Err(e),
            }
        }
        SelectExpr::Object(obj) if obj.len() == 1 => {
            let key = obj.keys().next().unwrap();
            let child = obj.get(key).unwrap();
            let (s, e) = match find_object_value_span_idx(json, start, end, key.as_bytes(), index) {
                Ok(x) => x,
                Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. }) => return Ok(None),
                Err(e) => return Err(e),
            };
            if matches!(child, SelectExpr::Identity | SelectExpr::Current) {
                Ok(Some((s, e)))
            } else {
                resolve_focus_idx(json, s, e, child, index)
            }
        }
        SelectExpr::Array(ArraySelect::Indices(map)) if map.len() == 1 => {
            let (idx, child) = map.iter().next().unwrap();
            let Some((s, e)) = nth_array_element(json, start, end, *idx, index)? else {
                return Ok(None);
            };
            if matches!(child, SelectExpr::Identity | SelectExpr::Current) {
                Ok(Some((s, e)))
            } else {
                resolve_focus_idx(json, s, e, child, index)
            }
        }
        SelectExpr::Sub(left, right) => {
            if let Some((s, e)) = resolve_focus_idx(json, start, end, left, index)? {
                resolve_focus_idx(json, s, e, right, index)
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

fn find_object_value_span_idx(
    json: &[u8],
    start: usize,
    end: usize,
    key: &[u8],
    index: Option<&IndexedDocument<'_>>,
) -> Result<(usize, usize), Error> {
    let start = skip_whitespace(json, start);
    if start < json.len() && json[start] == b'{' {
        if let Some(doc) = index {
            if let Some(oi) = doc.object_index_at_open(start) {
                return oi.get(key).ok_or(Error::PathNotFound);
            }
        }
    }
    find_object_value_span(json, start, end, key)
}

/// Byte offset of a field's value start (after `:`) without scanning the value body.
/// Prior siblings are still skipped (required to reach the key).
fn find_object_value_start(
    json: &[u8],
    start: usize,
    end: usize,
    key: &[u8],
    index: Option<&IndexedDocument<'_>>,
) -> Result<usize, Error> {
    let start = skip_whitespace(json, start);
    if start < json.len() && json[start] == b'{' {
        if let Some(doc) = index {
            if let Some(oi) = doc.object_index_at_open(start) {
                return oi.get(key).map(|(s, _)| s).ok_or(Error::PathNotFound);
            }
        }
    }
    if start >= json.len() || json[start] != b'{' {
        return Err(Error::TypeMismatch {
            expected: "object",
            found: type_name_at(json, start),
        });
    }
    let mut pos = start + 1;
    loop {
        pos = skip_whitespace(json, pos);
        if pos >= end || json[pos] == b'}' {
            return Err(Error::PathNotFound);
        }
        if json[pos] != b'"' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected object key",
            });
        }
        let key_inner_end = find_string_end(json, pos + 1)?;
        let key_on_wire = &json[pos + 1..key_inner_end];
        pos = key_inner_end + 1;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] != b':' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ':' after object key",
            });
        }
        pos += 1;
        pos = skip_whitespace(json, pos);
        let val_start = pos;
        if key_on_wire == key {
            return Ok(val_start);
        }
        // Skip non-matching values to continue the scan.
        let val_end = skip_value_smart(json, val_start, index)?;
        pos = skip_whitespace(json, val_end);
        if pos < json.len() && json[pos] == b',' {
            pos += 1;
        } else if pos < json.len() && json[pos] == b'}' {
            return Err(Error::PathNotFound);
        } else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ',' or '}' in object",
            });
        }
    }
}

/// `skip_value` accelerated by Stage-1 structural list when present on the document index.
fn skip_value_smart(
    json: &[u8],
    start: usize,
    index: Option<&IndexedDocument<'_>>,
) -> Result<usize, Error> {
    let start = skip_whitespace(json, start);
    if start < json.len()
        && matches!(json[start], b'{' | b'[')
        && let Some(doc) = index
        && let Some(st) = doc.structural()
    {
        return st.skip_container(json, start);
    }
    skip_value(json, start)
}

/// Resolve pure-focus expression to a **start offset only** (no full value close).
/// Used for intermediate descent into huge arrays/objects.
fn resolve_focus_start_idx(
    json: &[u8],
    start: usize,
    end: usize,
    expr: &SelectExpr,
    index: Option<&IndexedDocument<'_>>,
) -> Result<Option<usize>, Error> {
    match expr {
        SelectExpr::Identity | SelectExpr::Current => Ok(Some(start)),
        SelectExpr::Paren(inner) => resolve_focus_start_idx(json, start, end, inner, index),
        SelectExpr::Field(key) | SelectExpr::FieldQuoted(key) => {
            let wire = escape_json_key(key);
            match find_object_value_start(json, start, end, wire.as_bytes(), index) {
                Ok(s) => Ok(Some(s)),
                Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. }) => Ok(None),
                Err(e) => Err(e),
            }
        }
        SelectExpr::Object(obj) if obj.len() == 1 => {
            let key = obj.keys().next().unwrap();
            let child = obj.get(key).unwrap();
            let s = match find_object_value_start(json, start, end, key.as_bytes(), index) {
                Ok(s) => s,
                Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. }) => return Ok(None),
                Err(e) => return Err(e),
            };
            if matches!(child, SelectExpr::Identity | SelectExpr::Current) {
                Ok(Some(s))
            } else {
                resolve_focus_start_idx(json, s, end, child, index)
            }
        }
        SelectExpr::Array(ArraySelect::Indices(map)) if map.len() == 1 => {
            let (idx, child) = map.iter().next().unwrap();
            let Some((s, _e)) = nth_array_element(json, start, end, *idx, index)? else {
                return Ok(None);
            };
            if matches!(child, SelectExpr::Identity | SelectExpr::Current) {
                Ok(Some(s))
            } else {
                resolve_focus_start_idx(json, s, end, child, index)
            }
        }
        SelectExpr::Sub(left, right) => {
            if let Some(s) = resolve_focus_start_idx(json, start, end, left, index)? {
                resolve_focus_start_idx(json, s, end, right, index)
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

/// Side-table for the array value starting at `start` (after optional leading ws), if indexed.
#[inline]
fn array_side_table<'a>(
    json: &[u8],
    start: usize,
    index: Option<&'a IndexedDocument<'a>>,
) -> Option<&'a crate::index::ArrayIndex> {
    let open = skip_whitespace(json, start);
    if open >= json.len() || json[open] != b'[' {
        return None;
    }
    index.and_then(|doc| doc.array_index_at_open(open))
}

/// Length of the array at `start` without building full element spans when indexed.
#[allow(dead_code)] // available for slice/filter planning helpers
fn array_len_fast(
    json: &[u8],
    start: usize,
    end: usize,
    index: Option<&IndexedDocument<'_>>,
) -> Result<usize, Error> {
    if let Some(ai) = array_side_table(json, start, index) {
        return Ok(ai.len());
    }
    Ok(array_element_starts_scan(json, skip_whitespace(json, start), end)?.len())
}

/// Locate a single array element by JMESPath signed index **without** materializing
/// sibling spans.
///
/// | Index table | Positive `i` | Negative `i` |
/// | --- | --- | --- |
/// | present | O(1) jump + `skip_value` | O(1) |
/// | absent | O(`i`) scan, stop early | O(n) one pass (`-1` tracks last only) |
///
/// Returns `Ok(None)` when the index is out of range (JMESPath soft-null path).
fn nth_array_element(
    json: &[u8],
    start: usize,
    end: usize,
    index: i64,
    doc_index: Option<&IndexedDocument<'_>>,
) -> Result<Option<(usize, usize)>, Error> {
    let open = skip_whitespace(json, start);
    if open >= json.len() || json[open] != b'[' {
        return Err(Error::TypeMismatch {
            expected: "array",
            found: type_name_at(json, open),
        });
    }

    if let Some(ai) = array_side_table(json, open, doc_index) {
        let Some(i) = resolve_index(index, ai.len()) else {
            return Ok(None);
        };
        return ai.element_value_span(json, i).map(Some);
    }

    // No side-table: sparse scan.
    if index >= 0 {
        return nth_array_element_scan_positive(json, open, end, index as usize);
    }
    nth_array_element_scan_negative(json, open, end, index)
}

/// Walk only as far as element `target` (0-based). Never visits later siblings.
fn nth_array_element_scan_positive(
    json: &[u8],
    open: usize,
    end: usize,
    target: usize,
) -> Result<Option<(usize, usize)>, Error> {
    let mut pos = skip_whitespace(json, open + 1);
    if pos < end && json[pos] == b']' {
        return Ok(None);
    }
    let mut i = 0usize;
    loop {
        pos = skip_whitespace(json, pos);
        if pos >= end || json[pos] == b']' {
            return Ok(None);
        }
        let e_start = pos;
        let e_end = skip_value(json, e_start)?;
        if i == target {
            return Ok(Some((e_start, e_end)));
        }
        i += 1;
        pos = skip_whitespace(json, e_end);
        if pos < json.len() && json[pos] == b',' {
            pos += 1;
        } else if pos < json.len() && json[pos] == b']' {
            return Ok(None);
        } else if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF in array index scan",
            });
        } else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ',' or ']' in array index scan",
            });
        }
    }
}

/// Negative index without a side-table. `-1` keeps only the last span (O(n) time, O(1) mem).
/// Other negatives collect start offsets only once, then resolve.
fn nth_array_element_scan_negative(
    json: &[u8],
    open: usize,
    end: usize,
    index: i64,
) -> Result<Option<(usize, usize)>, Error> {
    debug_assert!(index < 0);
    if index == -1 {
        let mut pos = skip_whitespace(json, open + 1);
        if pos < end && json[pos] == b']' {
            return Ok(None);
        }
        let mut last: Option<(usize, usize)> = None;
        loop {
            pos = skip_whitespace(json, pos);
            if pos >= end || json[pos] == b']' {
                return Ok(last);
            }
            let e_start = pos;
            let e_end = skip_value(json, e_start)?;
            last = Some((e_start, e_end));
            pos = skip_whitespace(json, e_end);
            if pos < json.len() && json[pos] == b',' {
                pos += 1;
            } else if pos < json.len() && json[pos] == b']' {
                return Ok(last);
            } else {
                return Err(Error::InvalidJsonSyntax {
                    pos,
                    msg: "Expected ',' or ']' in array index scan",
                });
            }
        }
    }

    // General negative: one pass of start offsets (u32), then skip_value for the winner.
    let starts = array_element_starts_scan(json, open, end)?;
    let Some(i) = resolve_index(index, starts.len()) else {
        return Ok(None);
    };
    let s = starts[i] as usize;
    let e = skip_value(json, s)?;
    Ok(Some((s, e)))
}

/// Collect only element start offsets (u32) — cheaper than full `(start,end)` pairs when
/// ends are computed lazily.
fn array_element_starts_scan(json: &[u8], open: usize, end: usize) -> Result<Vec<u32>, Error> {
    let mut starts = Vec::new();
    let mut pos = skip_whitespace(json, open + 1);
    if pos < end && json[pos] == b']' {
        return Ok(starts);
    }
    loop {
        pos = skip_whitespace(json, pos);
        if pos >= end || json[pos] == b']' {
            break;
        }
        if pos > u32::MAX as usize {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Document too large for u32 array starts",
            });
        }
        starts.push(pos as u32);
        let e_end = skip_value(json, pos)?;
        pos = skip_whitespace(json, e_end);
        if pos < json.len() && json[pos] == b',' {
            pos += 1;
        } else if pos < json.len() && json[pos] == b']' {
            break;
        } else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ',' or ']' in array",
            });
        }
    }
    Ok(starts)
}

/// Resolve several signed indices into spans without building unused sibling ends when
/// a side-table exists. Multi-index result order follows sorted signed keys (stable plan).
fn resolve_indices_sparse(
    json: &[u8],
    start: usize,
    end: usize,
    keys: &[i64],
    doc_index: Option<&IndexedDocument<'_>>,
) -> Result<Vec<(usize, usize, i64)>, Error> {
    if keys.is_empty() {
        return Ok(Vec::new());
    }
    if keys.len() == 1 {
        return match nth_array_element(json, start, end, keys[0], doc_index)? {
            Some((s, e)) => Ok(vec![(s, e, keys[0])]),
            None => Ok(Vec::new()),
        };
    }

    if let Some(ai) = array_side_table(json, start, doc_index) {
        let mut out = Vec::with_capacity(keys.len());
        for &k in keys {
            if let Some(i) = resolve_index(k, ai.len()) {
                let (s, e) = ai.element_value_span(json, i)?;
                out.push((s, e, k));
            }
        }
        return Ok(out);
    }

    // No side-table: if any key is negative, need length → start table once.
    let needs_len = keys.iter().any(|&k| k < 0);
    if needs_len {
        let starts = {
            let open = skip_whitespace(json, start);
            array_element_starts_scan(json, open, end)?
        };
        let mut out = Vec::with_capacity(keys.len());
        for &k in keys {
            if let Some(i) = resolve_index(k, starts.len()) {
                let s = starts[i] as usize;
                let e = skip_value(json, s)?;
                out.push((s, e, k));
            }
        }
        return Ok(out);
    }

    // All non-negative: single forward scan, stop after max needed index.
    let mut needed: Vec<usize> = keys.iter().map(|&k| k as usize).collect();
    needed.sort_unstable();
    needed.dedup();
    let max_need = *needed.last().unwrap();
    let mut want = needed.into_iter().peekable();
    let mut found: std::collections::HashMap<usize, (usize, usize)> =
        std::collections::HashMap::new();

    let open = skip_whitespace(json, start);
    let mut pos = skip_whitespace(json, open + 1);
    let mut i = 0usize;
    if pos < end && json[pos] == b']' {
        return Ok(Vec::new());
    }
    while want.peek().is_some() {
        pos = skip_whitespace(json, pos);
        if pos >= end || json[pos] == b']' {
            break;
        }
        let e_start = pos;
        let e_end = skip_value(json, e_start)?;
        if want.peek() == Some(&i) {
            found.insert(i, (e_start, e_end));
            want.next();
        }
        if i >= max_need {
            break;
        }
        i += 1;
        pos = skip_whitespace(json, e_end);
        if pos < json.len() && json[pos] == b',' {
            pos += 1;
        } else {
            break;
        }
    }

    let mut out = Vec::with_capacity(keys.len());
    for &k in keys {
        let ui = k as usize;
        if let Some(&(s, e)) = found.get(&ui) {
            out.push((s, e, k));
        }
    }
    Ok(out)
}

fn collect_array_elems_idx(
    json: &[u8],
    start: usize,
    end: usize,
    index: Option<&IndexedDocument<'_>>,
) -> Result<Vec<(usize, usize)>, Error> {
    // Prefer side-table starts + lazy skip_value (still O(n) if caller needs all elems,
    // but avoids a second structural walk to discover starts).
    if let Some(ai) = array_side_table(json, start, index) {
        let mut elems = Vec::with_capacity(ai.len());
        for i in 0..ai.len() {
            elems.push(ai.element_value_span(json, i)?);
        }
        return Ok(elems);
    }
    collect_array_elems_scan(json, start, end)
}

/// Stream each kept list-projection card (`[*]`, `[?pred]`, slice) into `f`.
///
/// Applies `each` per source element, omits JSON `null` cards (list-projection
/// semantics). Peak RAM is one card buffer — same policy as [`emit_array_stream`].
pub(crate) fn for_each_projected_card<F>(
    json: &[u8],
    open: usize,
    end: usize,
    sel: &ArraySelect,
    each: &SelectExpr,
    ctx: &mut EmitCtx<'_>,
    mut f: F,
) -> Result<(), Error>
where
    F: FnMut(usize, &[u8]) -> Result<(), Error>,
{
    let mut card = Vec::new();
    let mut out_index = 0usize;
    let side = ctx.index;

    let mut emit_card = |json: &[u8],
                         s: usize,
                         e: usize,
                         ctx: &mut EmitCtx<'_>|
     -> Result<(), Error> {
        card.clear();
        emit_value(json, s, e, each, ctx, &mut card)?;
        if is_emitted_null(&card) {
            return Ok(());
        }
        f(out_index, &card)?;
        out_index += 1;
        Ok(())
    };

    match sel {
        ArraySelect::Each(_) => {
            for_each_array_element(json, open, end, side, |_, s, e| {
                emit_card(json, s, e, ctx)
            })
        }
        ArraySelect::Filter { pred, each: _ } => {
            for_each_array_element(json, open, end, side, |_, s, e| {
                let pv = eval_buf(json, s, e, pred, ctx)?;
                if is_truthy(&pv) {
                    emit_card(json, s, e, ctx)?;
                }
                Ok(())
            })
        }
        ArraySelect::Slice {
            start: st,
            end: en,
            step,
            each: _,
        } => {
            if step == &Some(0) {
                return Err(Error::Jmespath {
                    msg: "Invalid slice: step cannot be 0",
                });
            }
            if let Some(ai) = array_side_table(json, open, side) {
                let idxs = resolve_slice(ai.len(), *st, *en, *step);
                for i in idxs {
                    let s = ai.element_start(i).unwrap();
                    let e = skip_value_smart(json, s, side)?;
                    emit_card(json, s, e, ctx)?;
                }
            } else {
                let starts = array_element_starts_scan(json, open, end)?;
                let idxs = resolve_slice(starts.len(), *st, *en, *step);
                for i in idxs {
                    let s = starts[i] as usize;
                    let e = skip_value(json, s)?;
                    emit_card(json, s, e, ctx)?;
                }
            }
            Ok(())
        }
        ArraySelect::Indices(_) => Err(Error::Jmespath {
            msg: "project_each does not support multi-index list projections",
        }),
    }
}

fn is_emitted_null(v: &[u8]) -> bool {
    let s = skip_whitespace(v, 0);
    v.get(s..s + 4) == Some(b"null")
        && (s + 4 >= v.len()
            || matches!(
                v[s + 4],
                b' ' | b'\t' | b'\n' | b'\r' | b',' | b']' | b'}'
            ))
}

/// Iterate every array element without materializing a full `Vec` of spans first.
///
/// With a side-table: O(1) start per element via [`ArrayIndex::element_start`], then
/// a single `skip_value` for that element's end (siblings are never scanned to *find*
/// the start). Without a table: sequential scan.
///
/// Used by streaming list emit and by [`crate::project::project_each`] (per-card /
/// JSONL paths that must not build one giant output array).
pub(crate) fn for_each_array_element<F>(
    json: &[u8],
    start: usize,
    end: usize,
    doc_index: Option<&IndexedDocument<'_>>,
    mut f: F,
) -> Result<(), Error>
where
    F: FnMut(usize, usize, usize) -> Result<(), Error>, // (elem_index, start, end)
{
    if let Some(ai) = array_side_table(json, start, doc_index) {
        // Prefer start table + local skip_value (not a full pre-collect of ends).
        let n = ai.len();
        for i in 0..n {
            let s = ai.element_start(i).expect("i < len");
            let e = skip_value_smart(json, s, doc_index)?;
            f(i, s, e)?;
        }
        return Ok(());
    }
    let open = skip_whitespace(json, start);
    if open >= json.len() || json[open] != b'[' {
        return Err(Error::TypeMismatch {
            expected: "array",
            found: type_name_at(json, open),
        });
    }
    let mut pos = skip_whitespace(json, open + 1);
    if pos < end && json[pos] == b']' {
        return Ok(());
    }
    let mut i = 0usize;
    loop {
        pos = skip_whitespace(json, pos);
        if pos >= end || json[pos] == b']' {
            break;
        }
        let e_start = pos;
        let e_end = skip_value(json, e_start)?;
        f(i, e_start, e_end)?;
        i += 1;
        pos = skip_whitespace(json, e_end);
        if pos < json.len() && json[pos] == b',' {
            pos += 1;
        } else if pos < json.len() && json[pos] == b']' {
            break;
        } else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ',' or ']' in array",
            });
        }
    }
    Ok(())
}

fn flatten_emit_on(
    json: &[u8],
    start: usize,
    end: usize,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    let start = skip_whitespace(json, start);
    if start >= end || json.get(start) != Some(&b'[') {
        // non-array: identity (JMESPath flatten of non-array is the value itself? Spec: null)
        // jmespath.rs: Flatten of non-array → null
        if soft_null(ctx) {
            out.emit_bytes(b"null")?;
            return Ok(());
        }
        out.emit_bytes(&json[start..end])?;
        return Ok(());
    }
    let outer = collect_array_elems_idx(json, start, end, ctx.index)?;
    out.emit_byte(b'[')?;
    let mut first = true;
    for (s, e) in outer {
        let s = skip_whitespace(json, s);
        if s < json.len() && json[s] == b'[' {
            let inner = collect_array_elems_idx(json, s, e, ctx.index)?;
            for (is, ie) in inner {
                if !first {
                    out.emit_byte(b',')?;
                }
                maybe_pretty_newline_indent(ctx, out, true)?;
                out.emit_bytes(&json[is..ie])?;
                first = false;
            }
        } else {
            if !first {
                out.emit_byte(b',')?;
            }
            maybe_pretty_newline_indent(ctx, out, true)?;
            out.emit_bytes(&json[s..e])?;
            first = false;
        }
    }
    emit_close_array(ctx, out)?;
    Ok(())
}

/// JMESPath truthiness: false, null, empty string/array/object are falsey.
pub(crate) fn is_truthy(v: &[u8]) -> bool {
    let s = skip_whitespace(v, 0);
    let e = match skip_value(v, s) {
        Ok(x) => x,
        Err(_) => return false,
    };
    let slice = &v[s..e];
    match slice {
        b"null" | b"false" => false,
        b"true" => true,
        b"\"\"" => false,
        b"[]" | b"{}" => false,
        x if x.starts_with(b"[") || x.starts_with(b"{") || x.starts_with(b"\"") => true,
        x if x.first().is_some_and(|c| c.is_ascii_digit() || *c == b'-') => {
            // numbers: 0 is truthy in JMESPath
            true
        }
        _ => !slice.is_empty(),
    }
}

/// Compare two JSON values. `None` means incomparable → JMESPath `null`.
fn cmp_values(left: &[u8], right: &[u8], op: CmpOp) -> Option<bool> {
    let l = trim_json(left);
    let r = trim_json(right);

    // Structural equality for objects/arrays (filters like `key == \`{"a":1}\``).
    if matches!(op, CmpOp::Eq | CmpOp::Ne)
        && (l.starts_with(b"{") || l.starts_with(b"[") || r.starts_with(b"{") || r.starts_with(b"["))
    {
        let eq = json_struct_equal(l, r);
        return Some(match op {
            CmpOp::Eq => eq,
            CmpOp::Ne => !eq,
            _ => unreachable!(),
        });
    }

    // null equality / inequality only; order with null → null
    if l == b"null" || r == b"null" {
        return match op {
            CmpOp::Eq => Some(l == r),
            CmpOp::Ne => Some(l != r),
            _ => None,
        };
    }
    // boolean — only eq/ne with booleans
    if (l == b"true" || l == b"false") && (r == b"true" || r == b"false") {
        let lb = l == b"true";
        let rb = r == b"true";
        return match op {
            CmpOp::Eq => Some(lb == rb),
            CmpOp::Ne => Some(lb != rb),
            _ => None,
        };
    }
    // numbers
    if let (Ok(ln), Ok(rn)) = (parse_f64(l), parse_f64(r)) {
        return Some(match op {
            CmpOp::Eq => ln == rn,
            CmpOp::Ne => ln != rn,
            CmpOp::Lt => ln < rn,
            CmpOp::Le => ln <= rn,
            CmpOp::Gt => ln > rn,
            CmpOp::Ge => ln >= rn,
        });
    }
    // strings (JSON quoted)
    if let (Some(ls), Some(rs)) = (json_string_content(l), json_string_content(r)) {
        return Some(match op {
            CmpOp::Eq => ls == rs,
            CmpOp::Ne => ls != rs,
            CmpOp::Lt => ls < rs,
            CmpOp::Le => ls <= rs,
            CmpOp::Gt => ls > rs,
            CmpOp::Ge => ls >= rs,
        });
    }
    // JMESPath: equality/inequality across different types is false/true (not null).
    // Order comparisons across types → null.
    match op {
        CmpOp::Eq => Some(false),
        CmpOp::Ne => Some(true),
        _ => None,
    }
}

/// Deep structural equality for JSON objects/arrays (order-sensitive for arrays;
/// objects compared by key multiset of wire form).
fn json_struct_equal(a: &[u8], b: &[u8]) -> bool {
    let a = trim_json(a);
    let b = trim_json(b);
    if a == b {
        return true;
    }
    if a.first() != b.first() {
        return false;
    }
    match a.first() {
        Some(b'[') => {
            let ea = match collect_array_elems(a, 0, a.len()) {
                Ok(v) => v,
                Err(_) => return false,
            };
            let eb = match collect_array_elems(b, 0, b.len()) {
                Ok(v) => v,
                Err(_) => return false,
            };
            if ea.len() != eb.len() {
                return false;
            }
            ea.iter().zip(eb.iter()).all(|(&(sa, ea_), &(sb, eb_))| {
                json_struct_equal(&a[sa..ea_], &b[sb..eb_])
            })
        }
        Some(b'{') => {
            let (_, ma, _) = match collect_object_members(a, 0, a.len()) {
                Ok(v) => v,
                Err(_) => return false,
            };
            let (_, mb, _) = match collect_object_members(b, 0, b.len()) {
                Ok(v) => v,
                Err(_) => return false,
            };
            if ma.len() != mb.len() {
                return false;
            }
            // Compare as sets of (key, value)
            for ma_ in &ma {
                let found = mb.iter().any(|mb_| {
                    ma_.key_on_wire == mb_.key_on_wire
                        && json_struct_equal(
                            &a[ma_.val_start..ma_.val_end],
                            &b[mb_.val_start..mb_.val_end],
                        )
                });
                if !found {
                    return false;
                }
            }
            true
        }
        _ => a == b,
    }
}

fn trim_json(v: &[u8]) -> &[u8] {
    let s = skip_whitespace(v, 0);
    let e = skip_value(v, s).unwrap_or(v.len());
    let mut end = e;
    while end > s && matches!(v[end - 1], b' ' | b'\t' | b'\n' | b'\r') {
        end -= 1;
    }
    &v[s..end]
}

fn parse_f64(v: &[u8]) -> Result<f64, ()> {
    let s = std::str::from_utf8(v).map_err(|_| ())?;
    s.parse().map_err(|_| ())
}

fn json_string_content(v: &[u8]) -> Option<&[u8]> {
    if v.len() >= 2 && v[0] == b'"' && v[v.len() - 1] == b'"' {
        Some(&v[1..v.len() - 1])
    } else {
        None
    }
}

struct Member<'a> {
    key_on_wire: &'a [u8],
    key_span: (usize, usize),
    colon: usize,
    val_start: usize,
    val_end: usize,
    member_start: usize,
    /// End of member value; next is comma or `}`.
    after_value: usize,
    /// Inclusive end of trailing comma if present (for preserve).
    comma: Option<usize>,
}

fn collect_object_members<'a>(
    json: &'a [u8],
    start: usize,
    end: usize,
) -> Result<(usize, Vec<Member<'a>>, usize), Error> {
    let start = skip_whitespace(json, start);
    if start >= json.len() || json[start] != b'{' {
        return Err(Error::TypeMismatch {
            expected: "object",
            found: type_name_at(json, start),
        });
    }
    let mut members = Vec::new();
    let mut pos = start + 1;
    loop {
        let ws_before = pos;
        pos = skip_whitespace(json, pos);
        if pos >= end {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unclosed object in project",
            });
        }
        if json[pos] == b'}' {
            return Ok((start, members, pos));
        }
        if json[pos] != b'"' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected object key in project",
            });
        }
        let key_open = pos;
        let key_inner_end = find_string_end(json, pos + 1)?;
        let key_on_wire = &json[pos + 1..key_inner_end];
        pos = key_inner_end + 1;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] != b':' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ':' in project object",
            });
        }
        let colon = pos;
        pos += 1;
        pos = skip_whitespace(json, pos);
        let val_start = pos;
        let val_end = skip_value(json, val_start)?;
        let after_value = val_end;
        pos = skip_whitespace(json, val_end);
        let comma = if pos < json.len() && json[pos] == b',' {
            let c = pos;
            pos += 1;
            Some(c)
        } else {
            None
        };
        members.push(Member {
            key_on_wire,
            key_span: (key_open, key_inner_end + 1),
            colon,
            val_start,
            val_end,
            member_start: key_open,
            after_value,
            comma,
        });
        let _ = ws_before;
        if comma.is_none() && pos < json.len() && json[pos] == b'}' {
            // continue to see }
        } else if comma.is_none() && (pos >= json.len() || json[pos] != b'}') {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ',' or '}' in project object",
            });
        }
    }
}

fn find_object_value_span(
    json: &[u8],
    start: usize,
    end: usize,
    key: &[u8],
) -> Result<(usize, usize), Error> {
    let (_, members, _) = collect_object_members(json, start, end)?;
    for m in members {
        if m.key_on_wire == key {
            return Ok((m.val_start, m.val_end));
        }
    }
    Err(Error::PathNotFound)
}

fn collect_array_elems(json: &[u8], start: usize, end: usize) -> Result<Vec<(usize, usize)>, Error> {
    collect_array_elems_idx(json, start, end, None)
}

fn collect_array_elems_scan(
    json: &[u8],
    start: usize,
    end: usize,
) -> Result<Vec<(usize, usize)>, Error> {
    let start = skip_whitespace(json, start);
    if start >= json.len() || json[start] != b'[' {
        return Err(Error::TypeMismatch {
            expected: "array",
            found: type_name_at(json, start),
        });
    }
    let mut elems = Vec::new();
    let mut pos = start + 1;
    pos = skip_whitespace(json, pos);
    if pos < end && json[pos] == b']' {
        return Ok(elems);
    }
    loop {
        pos = skip_whitespace(json, pos);
        if pos >= end {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unclosed array in project",
            });
        }
        if json[pos] == b']' {
            break;
        }
        let e_start = pos;
        let e_end = skip_value(json, e_start)?;
        elems.push((e_start, e_end));
        pos = skip_whitespace(json, e_end);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF in project array",
            });
        }
        if json[pos] == b',' {
            pos += 1;
        } else if json[pos] == b']' {
            // next
        } else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ',' or ']' in project array",
            });
        }
    }
    Ok(elems)
}

fn emit_object(
    json: &[u8],
    start: usize,
    end: usize,
    sel: &ObjectSelect,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    // Compact/Pretty: selective scan — do not `skip_value` a kept field's body when it is
    // the last kept key (e.g. root `{"products":[ 25k… ]}` keep-list only needs value start).
    // PreserveSource still collects members for whitespace fidelity.
    if !matches!(ctx.plan.style, ProjectStyle::PreserveSource) {
        return emit_object_selective(json, start, end, sel, ctx, out);
    }
    emit_object_preserve(json, start, end, sel, ctx, out)
}

/// Keep-list / multi-field object emit without pre-scanning huge kept values.
fn emit_object_selective(
    json: &[u8],
    start: usize,
    end: usize,
    sel: &ObjectSelect,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    let obj_start = skip_whitespace(json, start);
    if obj_start >= json.len() || json[obj_start] != b'{' {
        if soft_null(ctx) {
            out.emit_bytes(b"null")?;
            return Ok(());
        }
        return Err(Error::TypeMismatch {
            expected: "object",
            found: type_name_at(json, obj_start),
        });
    }

    let mut remaining: std::collections::HashSet<&str> = sel.fields.keys().map(|s| s.as_str()).collect();
    let missing_err = ctx.plan.missing == MissingPolicy::Error;
    let total_keep = remaining.len();

    out.emit_byte(b'{')?;
    let mut wrote = 0usize;
    let mut pos = obj_start + 1;

    while !remaining.is_empty() {
        pos = skip_whitespace(json, pos);
        if pos >= end || json[pos] == b'}' {
            break;
        }
        if json[pos] != b'"' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected object key in project",
            });
        }
        let key_open = pos;
        let key_inner_end = find_string_end(json, pos + 1)?;
        let key_on_wire = &json[pos + 1..key_inner_end];
        let key_str = std::str::from_utf8(key_on_wire).map_err(|_| Error::InvalidJsonSyntax {
            pos: key_open,
            msg: "Object key is not UTF-8",
        })?;
        pos = key_inner_end + 1;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] != b':' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ':' in project object",
            });
        }
        pos += 1;
        pos = skip_whitespace(json, pos);
        let val_start = pos;

        if let Some(child) = sel.fields.get(key_str) {
            if wrote > 0 {
                out.emit_byte(b',')?;
            }
            // Compact: no indent depth / pretty newline; Pretty: full bookkeeping.
            let compact = matches!(ctx.plan.style, ProjectStyle::Compact);
            if !compact {
                maybe_pretty_newline_indent(ctx, out, true)?;
            }
            // key + colon (compact / pretty)
            out.emit_bytes(&json[key_open..key_inner_end + 1])?;
            match ctx.plan.style {
                ProjectStyle::Pretty { .. } => out.emit_bytes(b": ")?,
                _ => out.emit_byte(b':')?,
            }
            // Open-ended child bound: descent uses value start; Identity still needs a real end.
            let child_end = if matches!(child, SelectExpr::Identity | SelectExpr::Current) {
                skip_value_smart(json, val_start, ctx.index)?
            } else {
                end
            };
            if compact {
                emit_value(json, val_start, child_end, child, ctx, out)?;
            } else {
                ctx.depth += 1;
                emit_value(json, val_start, child_end, child, ctx, out)?;
                ctx.depth -= 1;
            }
            wrote += 1;
            remaining.remove(key_str);

            if remaining.is_empty() {
                // Last kept field: do **not** skip its (possibly huge) original value.
                break;
            }
            // More kept keys later: advance past this value.
            let val_end = skip_value_smart(json, val_start, ctx.index)?;
            pos = skip_whitespace(json, val_end);
            if pos < json.len() && json[pos] == b',' {
                pos += 1;
            }
        } else {
            let val_end = skip_value_smart(json, val_start, ctx.index)?;
            pos = skip_whitespace(json, val_end);
            if pos < json.len() && json[pos] == b',' {
                pos += 1;
            } else if pos < json.len() && json[pos] == b'}' {
                break;
            } else if pos >= json.len() {
                return Err(Error::InvalidJsonSyntax {
                    pos,
                    msg: "Unclosed object in project",
                });
            }
        }
    }

    if missing_err && wrote != total_keep {
        return Err(Error::PathNotFound);
    }
    emit_close_object(ctx, out)?;
    Ok(())
}

fn emit_object_preserve(
    json: &[u8],
    start: usize,
    end: usize,
    sel: &ObjectSelect,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    let (obj_start, members, close_pos) = match collect_object_members(json, start, end) {
        Ok(x) => x,
        Err(Error::TypeMismatch { .. }) if soft_null(ctx) => {
            out.emit_bytes(b"null")?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    let mut kept: Vec<(&Member<'_>, &SelectExpr)> = Vec::new();
    for m in &members {
        let key_str = std::str::from_utf8(m.key_on_wire).map_err(|_| Error::InvalidJsonSyntax {
            pos: m.key_span.0,
            msg: "Object key is not UTF-8",
        })?;
        if let Some(child) = sel.fields.get(key_str) {
            kept.push((m, child));
        }
    }

    if ctx.plan.missing == MissingPolicy::Error {
        for key in sel.fields.keys() {
            if !members.iter().any(|m| m.key_on_wire == key.as_bytes()) {
                return Err(Error::PathNotFound);
            }
        }
    }

    out.emit_byte(b'{')?;
    if let Some((first_m, _)) = kept.first() {
        let between = &json[obj_start + 1..first_m.member_start];
        if is_ws_only(between) {
            out.emit_bytes(between)?;
        }
    }

    for (i, (m, child)) in kept.iter().enumerate() {
        if i > 0 {
            emit_member_sep(json, &kept, i, ctx, out)?;
        }
        emit_object_key(json, m, ctx, out)?;
        emit_colon(json, m, ctx, out)?;
        ctx.depth += 1;
        emit_value(json, m.val_start, m.val_end, child, ctx, out)?;
        ctx.depth -= 1;
        if i + 1 < kept.len() {
            if let Some(c) = m.comma {
                let mid = &json[m.after_value..c];
                if is_ws_only(mid) {
                    out.emit_bytes(mid)?;
                }
            }
        }
    }

    if let Some((last_m, _)) = kept.last() {
        if let Some(c) = last_m.comma {
            let between = &json[last_m.after_value..c];
            if is_ws_only(between) {
                out.emit_bytes(between)?;
            }
        } else {
            let between = &json[last_m.after_value..close_pos];
            if is_ws_only(between) {
                out.emit_bytes(between)?;
            }
        }
    }

    emit_close_object(ctx, out)?;
    let _ = end;
    Ok(())
}

fn emit_member_sep(
    json: &[u8],
    kept: &[(&Member<'_>, &SelectExpr)],
    i: usize,
    ctx: &EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    match ctx.plan.style {
        ProjectStyle::PreserveSource => {
            // Prefer original comma between adjacent original neighbors.
            let prev = kept[i - 1].0;
            let curr = kept[i].0;
            if let Some(c) = prev.comma {
                // from comma through whitespace before curr key
                out.emit_byte(b',')?;
                let between = &json[c + 1..curr.member_start];
                if is_ws_only(between) {
                    out.emit_bytes(between)?;
                }
            } else {
                out.emit_byte(b',')?;
            }
        }
        ProjectStyle::Compact | ProjectStyle::Pretty { .. } => out.emit_byte(b',')?,
    }
    Ok(())
}

/// Stream `[*]` / filter / slice projections without cloning the child AST per element.
///
/// [`ProjectStyle::Compact`] uses a dedicated streaming path that never touches indent
/// depth or pretty helpers (see module docs). Pretty / PreserveSource keep full style
/// bookkeeping via [`stream_projection_elem`].
fn emit_array_stream(
    json: &[u8],
    open: usize,
    end: usize,
    sel: &ArraySelect,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    let omit_nulls = matches!(
        sel,
        ArraySelect::Each(_) | ArraySelect::Slice { .. } | ArraySelect::Filter { .. }
    );
    let compact = matches!(ctx.plan.style, ProjectStyle::Compact);

    let side = ctx.index;
    match sel {
        ArraySelect::Each(child) => {
            #[cfg(feature = "parallel")]
            if ctx.allow_parallel
                && compact
                && let Some(ai) = array_side_table(json, open, side)
                && ai.len() >= PARALLEL_EACH_MIN_ELEMS
            {
                // Owns brackets end-to-end (do not emit `[` before this return).
                return emit_array_each_parallel(json, open, ai, child.as_ref(), omit_nulls, ctx, out);
            }
            out.emit_byte(b'[')?;
            let mut first = true;
            if compact {
                for_each_array_element(json, open, end, side, |_, s, e| {
                    stream_projection_elem_compact(
                        json,
                        s,
                        e,
                        child.as_ref(),
                        omit_nulls,
                        ctx,
                        out,
                        &mut first,
                    )
                })?;
                out.emit_byte(b']')?;
            } else {
                for_each_array_element(json, open, end, side, |_, s, e| {
                    stream_projection_elem(
                        json,
                        s,
                        e,
                        child.as_ref(),
                        omit_nulls,
                        ctx,
                        out,
                        &mut first,
                    )
                })?;
                emit_close_array(ctx, out)?;
            }
            return Ok(());
        }
        ArraySelect::Filter { pred, each } => {
            out.emit_byte(b'[')?;
            let mut first = true;
            let mut kept: Vec<(usize, usize)> = Vec::new();
            for_each_array_element(json, open, end, side, |_, s, e| {
                let pv = eval_buf(json, s, e, pred, ctx)?;
                if is_truthy(&pv) {
                    kept.push((s, e));
                }
                Ok(())
            })?;
            if compact {
                for (s, e) in kept {
                    stream_projection_elem_compact(
                        json,
                        s,
                        e,
                        each.as_ref(),
                        omit_nulls,
                        ctx,
                        out,
                        &mut first,
                    )?;
                }
                out.emit_byte(b']')?;
            } else {
                for (s, e) in kept {
                    stream_projection_elem(
                        json,
                        s,
                        e,
                        each.as_ref(),
                        omit_nulls,
                        ctx,
                        out,
                        &mut first,
                    )?;
                }
                emit_close_array(ctx, out)?;
            }
        }
        ArraySelect::Slice {
            start: st,
            end: en,
            step,
            each,
        } => {
            if step == &Some(0) {
                return Err(Error::Jmespath {
                    msg: "Invalid slice: step cannot be 0",
                });
            }
            out.emit_byte(b'[')?;
            let mut first = true;
            if let Some(ai) = array_side_table(json, open, ctx.index) {
                let idxs = resolve_slice(ai.len(), *st, *en, *step);
                for i in idxs {
                    let s = ai.element_start(i).unwrap();
                    let e = skip_value_smart(json, s, ctx.index)?;
                    if compact {
                        stream_projection_elem_compact(
                            json,
                            s,
                            e,
                            each.as_ref(),
                            omit_nulls,
                            ctx,
                            out,
                            &mut first,
                        )?;
                    } else {
                        stream_projection_elem(
                            json,
                            s,
                            e,
                            each.as_ref(),
                            omit_nulls,
                            ctx,
                            out,
                            &mut first,
                        )?;
                    }
                }
            } else {
                let starts = array_element_starts_scan(json, open, end)?;
                let idxs = resolve_slice(starts.len(), *st, *en, *step);
                for i in idxs {
                    let s = starts[i] as usize;
                    let e = skip_value(json, s)?;
                    if compact {
                        stream_projection_elem_compact(
                            json,
                            s,
                            e,
                            each.as_ref(),
                            omit_nulls,
                            ctx,
                            out,
                            &mut first,
                        )?;
                    } else {
                        stream_projection_elem(
                            json,
                            s,
                            e,
                            each.as_ref(),
                            omit_nulls,
                            ctx,
                            out,
                            &mut first,
                        )?;
                    }
                }
            }
            if compact {
                out.emit_byte(b']')?;
            } else {
                emit_close_array(ctx, out)?;
            }
        }
        ArraySelect::Indices(_) => unreachable!("indices handled before stream"),
    }
    Ok(())
}

/// Parallel `[*]` over a side-table: each Rayon task owns a contiguous index range and
/// writes its own buffer; the parent concatenates with commas. Domain-agnostic — any
/// array shape that has been indexed.
#[cfg(feature = "parallel")]
fn emit_array_each_parallel(
    json: &[u8],
    open: usize,
    ai: &crate::index::ArrayIndex,
    child: &SelectExpr,
    omit_nulls: bool,
    ctx: &EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    use rayon::prelude::*;

    let _ = open;
    let n = ai.len();
    let threads = rayon::current_num_threads().max(1);
    let chunk = (n + threads - 1) / threads;
    let plan = ctx.plan;
    let index = ctx.index;
    let base_depth = ctx.depth;

    // IndexedDocument / ArrayIndex are read-only here; element spans never overlap writes.
    let parts: Vec<Result<Vec<u8>, Error>> = (0..threads)
        .into_par_iter()
        .map(|c| {
            let lo = c * chunk;
            let hi = (lo + chunk).min(n);
            if lo >= hi {
                return Ok(Vec::new());
            }
            let mut buf = Vec::new();
            let mut local = EmitCtx {
                plan,
                depth: base_depth,
                index,
                allow_parallel: false, // nested parallel not useful
            };
            let mut first = true;
            for i in lo..hi {
                let s = match ai.element_start(i) {
                    Some(s) => s,
                    None => continue,
                };
                let e = skip_value(json, s)?;
                // Parallel gate is Compact-only; skip pretty depth bookkeeping.
                stream_projection_elem_compact(
                    json,
                    s,
                    e,
                    child,
                    omit_nulls,
                    &mut local,
                    &mut buf,
                    &mut first,
                )?;
            }
            Ok(buf)
        })
        .collect();

    out.emit_byte(b'[')?;
    let mut any = false;
    for part in parts {
        let part = part?;
        if part.is_empty() {
            continue;
        }
        if any {
            out.emit_byte(b',')?;
        }
        out.emit_bytes(&part)?;
        any = true;
    }
    // Pretty close not used (Compact only).
    out.emit_byte(b']')?;
    Ok(())
}

/// Compact list-projection element: comma + value only (no indent depth, no pretty hooks).
///
/// Used exclusively when [`ProjectStyle::Compact`]. Parallel Each also lands here via the
/// same Compact-only gate. Pretty / PreserveSource must use [`stream_projection_elem`].
#[inline]
fn stream_projection_elem_compact(
    json: &[u8],
    s: usize,
    e: usize,
    child: &SelectExpr,
    omit_nulls: bool,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
    first: &mut bool,
) -> Result<(), Error> {
    if matches!(child, SelectExpr::Identity | SelectExpr::Current) {
        if omit_nulls && trim_json(&json[s..e]) == b"null" {
            return Ok(());
        }
        if !*first {
            out.emit_byte(b',')?;
        }
        out.emit_bytes(&json[s..e])?;
        *first = false;
        return Ok(());
    }
    // Multi-select / objects are not bare JSON null; stream straight to `out`.
    let stream_direct = !omit_nulls
        || matches!(
            child,
            SelectExpr::MultiSelectHash(_)
                | SelectExpr::MultiSelectList(_)
                | SelectExpr::Object(_)
                | SelectExpr::Literal(_)
        );
    if stream_direct {
        if !*first {
            out.emit_byte(b',')?;
        }
        emit_value(json, s, e, child, ctx, out)?;
        *first = false;
        return Ok(());
    }
    let mut piece = Vec::new();
    emit_value(json, s, e, child, ctx, &mut piece)?;
    if omit_nulls && trim_json(&piece) == b"null" {
        return Ok(());
    }
    if !*first {
        out.emit_byte(b',')?;
    }
    out.emit_bytes(&piece)?;
    *first = false;
    Ok(())
}

/// Emit one list-projection element with pretty / PreserveSource bookkeeping.
///
/// Updates `first` when something is written. Prefer [`stream_projection_elem_compact`]
/// on the default Compact bulk path.
fn stream_projection_elem(
    json: &[u8],
    s: usize,
    e: usize,
    child: &SelectExpr,
    omit_nulls: bool,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
    first: &mut bool,
) -> Result<(), Error> {
    if matches!(child, SelectExpr::Identity | SelectExpr::Current) {
        if omit_nulls && trim_json(&json[s..e]) == b"null" {
            return Ok(());
        }
        if !*first {
            out.emit_byte(b',')?;
        }
        maybe_pretty_newline_indent(ctx, out, true)?;
        ctx.depth += 1;
        out.emit_bytes(&json[s..e])?;
        ctx.depth -= 1;
        *first = false;
        return Ok(());
    }
    // Multi-select / objects are not bare JSON null; stream straight to `out`.
    let stream_direct = !omit_nulls
        || matches!(
            child,
            SelectExpr::MultiSelectHash(_)
                | SelectExpr::MultiSelectList(_)
                | SelectExpr::Object(_)
                | SelectExpr::Literal(_)
        );
    if stream_direct {
        if !*first {
            out.emit_byte(b',')?;
        }
        maybe_pretty_newline_indent(ctx, out, true)?;
        ctx.depth += 1;
        emit_value(json, s, e, child, ctx, out)?;
        ctx.depth -= 1;
        *first = false;
        return Ok(());
    }
    let mut piece = Vec::new();
    emit_value(json, s, e, child, ctx, &mut piece)?;
    if omit_nulls && trim_json(&piece) == b"null" {
        return Ok(());
    }
    if !*first {
        out.emit_byte(b',')?;
    }
    maybe_pretty_newline_indent(ctx, out, true)?;
    ctx.depth += 1;
    out.emit_bytes(&piece)?;
    ctx.depth -= 1;
    *first = false;
    Ok(())
}

fn emit_array(
    json: &[u8],
    start: usize,
    end: usize,
    sel: &ArraySelect,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    // Type-check array open early (soft-null for non-arrays under Skip policy).
    let open = skip_whitespace(json, start);
    if open >= json.len() || json[open] != b'[' {
        if soft_null(ctx) {
            out.emit_bytes(b"null")?;
            return Ok(());
        }
        return Err(Error::TypeMismatch {
            expected: "array",
            found: type_name_at(json, open),
        });
    }

    // Compact/Pretty list projections stream without per-element AST clones or a
    // full `kept` buffer (critical for products[*].{…} on 25k-element catalogs).
    // PreserveSource still materializes for whitespace fidelity.
    if !matches!(ctx.plan.style, ProjectStyle::PreserveSource)
        && matches!(
            sel,
            ArraySelect::Each(_) | ArraySelect::Filter { .. } | ArraySelect::Slice { .. }
        )
    {
        return emit_array_stream(json, open, end, sel, ctx, out);
    }

    let mut kept: Vec<(usize, usize, SelectExpr)> = Vec::new();
    match sel {
        // ── Sparse index: never materialize sibling spans ─────────────────
        ArraySelect::Indices(map) => {
            let mut keys: Vec<i64> = map.keys().copied().collect();
            keys.sort_unstable();
            // JMESPath: a single index expression yields the element, not a 1-array.
            if map.len() == 1 {
                let k = keys[0];
                match nth_array_element(json, open, end, k, ctx.index)? {
                    Some((s, e)) => {
                        return emit_value(json, s, e, map.get(&k).unwrap(), ctx, out);
                    }
                    None if ctx.plan.missing == MissingPolicy::Error => {
                        return Err(Error::PathNotFound);
                    }
                    None => {
                        out.emit_bytes(b"null")?;
                        return Ok(());
                    }
                }
            }
            let resolved = resolve_indices_sparse(json, open, end, &keys, ctx.index)?;
            if ctx.plan.missing == MissingPolicy::Error && resolved.len() != keys.len() {
                return Err(Error::PathNotFound);
            }
            for (s, e, k) in resolved {
                kept.push((s, e, map.get(&k).unwrap().clone()));
            }
        }
        // ── Slice: with side-table only touch selected indices ────────────
        ArraySelect::Slice {
            start: st,
            end: en,
            step,
            each,
        } => {
            if step == &Some(0) {
                return Err(Error::Jmespath {
                    msg: "Invalid slice: step cannot be 0",
                });
            }
            if let Some(ai) = array_side_table(json, open, ctx.index) {
                for i in resolve_slice(ai.len(), *st, *en, *step) {
                    let (s, e) = ai.element_value_span(json, i)?;
                    kept.push((s, e, (*each.as_ref()).clone()));
                }
            } else {
                // No side-table: need length for slice bounds → start offsets once.
                let starts = array_element_starts_scan(json, open, end)?;
                for i in resolve_slice(starts.len(), *st, *en, *step) {
                    let s = starts[i] as usize;
                    let e = skip_value(json, s)?;
                    kept.push((s, e, (*each.as_ref()).clone()));
                }
            }
        }
        // ── Each / filter: stream elements; only store kept ───────────────
        ArraySelect::Each(child) => {
            for_each_array_element(json, open, end, ctx.index, |_i, s, e| {
                kept.push((s, e, (*child.as_ref()).clone()));
                Ok(())
            })?;
        }
        ArraySelect::Filter { pred, each } => {
            for_each_array_element(json, open, end, ctx.index, |_i, s, e| {
                let pv = eval_buf(json, s, e, pred, ctx)?;
                if is_truthy(&pv) {
                    kept.push((s, e, (*each.as_ref()).clone()));
                }
                Ok(())
            })?;
        }
    }

    // JMESPath list projections (`[*]`, slices, filters+project) omit nulls.
    // Index-only multi selections with >1 index still use kept list as-is.
    let omit_nulls = matches!(
        sel,
        ArraySelect::Each(_) | ArraySelect::Slice { .. } | ArraySelect::Filter { .. }
    );
    let arr_open = skip_whitespace(json, start);
    out.emit_byte(b'[')?;
    // PreserveSource: whitespace after `[` up to first kept element (full tree replay).
    if matches!(ctx.plan.style, ProjectStyle::PreserveSource)
        && let Some((fs, _, _)) = kept.first()
    {
        let between = &json[arr_open + 1..*fs];
        if is_ws_only(between) {
            out.emit_bytes(between)?;
        }
    }
    let mut first = true;
    let mut prev_end: Option<usize> = None;
    let mut last_emitted_end: Option<usize> = None;
    for (s, e, child) in &kept {
        // Identity / pure leaves: stream without intermediate piece when possible.
        let is_identity = matches!(child, SelectExpr::Identity | SelectExpr::Current);
        let piece = if is_identity {
            None
        } else {
            let mut piece = Vec::new();
            emit_value(json, *s, *e, child, ctx, &mut piece)?;
            if omit_nulls && trim_json(&piece) == b"null" {
                continue;
            }
            Some(piece)
        };
        if omit_nulls && is_identity && trim_json(&json[*s..*e]) == b"null" {
            continue;
        }
        if !first {
            if matches!(ctx.plan.style, ProjectStyle::PreserveSource)
                && let Some(pe) = prev_end
            {
                // Replay original comma + whitespace between source element ends.
                let p = skip_whitespace(json, pe);
                if p < json.len() && json[p] == b',' {
                    out.emit_byte(b',')?;
                    let between = &json[p + 1..*s];
                    if is_ws_only(between) {
                        out.emit_bytes(between)?;
                    }
                } else {
                    // Non-adjacent kept elements: still emit comma; copy trailing
                    // ws before current element if present.
                    out.emit_byte(b',')?;
                    let between = &json[pe..*s];
                    // Skip non-ws (dropped elements); take only trailing ws before curr.
                    let mut i = between.len();
                    while i > 0 && matches!(between[i - 1], b' ' | b'\t' | b'\n' | b'\r') {
                        i -= 1;
                    }
                    let tail = &between[i..];
                    if !tail.is_empty() {
                        out.emit_bytes(tail)?;
                    }
                }
            } else {
                out.emit_byte(b',')?;
            }
        }
        maybe_pretty_newline_indent(ctx, out, true)?;
        ctx.depth += 1;
        if let Some(ref piece) = piece {
            out.emit_bytes(piece)?;
        } else {
            out.emit_bytes(&json[*s..*e])?;
        }
        ctx.depth -= 1;
        first = false;
        prev_end = Some(*e);
        last_emitted_end = Some(*e);
    }
    // PreserveSource: whitespace after last kept element before `]`.
    if matches!(ctx.plan.style, ProjectStyle::PreserveSource)
        && let Some(le) = last_emitted_end
    {
        let close = {
            let mut p = skip_whitespace(json, le);
            while p < end && json[p] != b']' {
                // skip dropped elements / commas
                if json[p] == b',' {
                    p += 1;
                    p = skip_whitespace(json, p);
                    if p < end && json[p] != b']' {
                        let _ = skip_value(json, p).map(|n| p = n);
                        p = skip_whitespace(json, p);
                    }
                } else {
                    break;
                }
            }
            p
        };
        if close < json.len() && json[close] == b']' {
            // Prefer original trailing ws immediately before `]` from last value.
            let mut ws_start = le;
            let mut ws_end = le;
            let mut p = le;
            while p < close {
                if matches!(json[p], b' ' | b'\t' | b'\n' | b'\r') {
                    if ws_end == ws_start {
                        ws_start = p;
                    }
                    ws_end = p + 1;
                    p += 1;
                } else {
                    break;
                }
            }
            if ws_end > ws_start {
                out.emit_bytes(&json[ws_start..ws_end])?;
            }
        }
    }
    emit_close_array(ctx, out)?;
    let _ = end;
    Ok(())
}

fn emit_call(
    json: &[u8],
    start: usize,
    end: usize,
    name: &str,
    args: &[SelectExpr],
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        "length" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            let n = length_of(&v)?;
            out.emit_bytes(n.to_string().as_bytes())?;
            Ok(())
        }
        "keys" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            keys_of(&v, out)
        }
        "values" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            values_of(&v, out)
        }
        "type" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            let t = type_name_json(&v);
            write_json_string_out(out, t)?;
            Ok(())
        }
        "to_string" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            let t = trim_json(&v);
            if t.starts_with(b"\"") {
                // Already a JSON string.
                out.emit_bytes(t)?;
            } else {
                // Compact form (strip insignificant whitespace) then JSON-string-escape.
                let compact = compact_json(t);
                write_json_string_out(out, std::str::from_utf8(&compact).unwrap_or(""))?;
            }
            Ok(())
        }
        "to_number" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            let t = trim_json(&v);
            if let Some(s) = json_string_content(t) {
                // Unescape for parse (simple path: raw content without escapes common in suite).
                if let Ok(s) = std::str::from_utf8(s) {
                    if s.parse::<f64>().is_ok() {
                        out.emit_bytes(s.as_bytes())?;
                        return Ok(());
                    }
                }
            } else if parse_f64(t).is_ok() {
                out.emit_bytes(t)?;
                return Ok(());
            }
            out.emit_bytes(b"null")?;
            Ok(())
        }
        "starts_with" | "ends_with" => {
            require_arity(&name, args, 2)?;
            let a = eval_buf(json, start, end, &args[0], ctx)?;
            let b = eval_buf(json, start, end, &args[1], ctx)?;
            let at = trim_json(&a);
            let bt = trim_json(&b);
            let as_ = json_string_content(at).ok_or(Error::Jmespath {
                msg: "starts_with/ends_with require string subject",
            })?;
            let bs = json_string_content(bt).ok_or(Error::Jmespath {
                msg: "starts_with/ends_with require string prefix/suffix",
            })?;
            // Compare unescaped logical content when possible
            let as_u = unescape_string_content(as_).unwrap_or_else(|| as_.to_vec());
            let bs_u = unescape_string_content(bs).unwrap_or_else(|| bs.to_vec());
            let ok = if name == "starts_with" {
                as_u.starts_with(&bs_u)
            } else {
                as_u.ends_with(&bs_u)
            };
            out.emit_bytes(if ok { b"true" } else { b"false" })?;
            Ok(())
        }
        "contains" => {
            require_arity(&name, args, 2)?;
            let a = eval_buf(json, start, end, &args[0], ctx)?;
            let b = eval_buf(json, start, end, &args[1], ctx)?;
            let at = trim_json(&a);
            let bt = trim_json(&b);
            let ok = if at.starts_with(b"[") {
                array_contains(at, bt)?
            } else if let Some(as_) = json_string_content(at) {
                let bs = json_string_content(bt).ok_or(Error::Jmespath {
                    msg: "contains on string requires string search",
                })?;
                let as_u = unescape_string_content(as_).unwrap_or_else(|| as_.to_vec());
                let bs_u = unescape_string_content(bs).unwrap_or_else(|| bs.to_vec());
                as_u.windows(bs_u.len()).any(|w| w == bs_u.as_slice())
            } else {
                return Err(Error::Jmespath {
                    msg: "contains requires string or array subject",
                });
            };
            out.emit_bytes(if ok { b"true" } else { b"false" })?;
            Ok(())
        }
        "not_null" => {
            if args.is_empty() {
                return Err(Error::Jmespath {
                    msg: "not_null requires at least one argument",
                });
            }
            for a in args {
                let v = eval_buf(json, start, end, a, ctx)?;
                if trim_json(&v) != b"null" {
                    out.emit_bytes(trim_json(&v))?;
                    return Ok(());
                }
            }
            out.emit_bytes(b"null")?;
            Ok(())
        }
        "reverse" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            reverse_array_or_string(&v, out)
        }
        "sort" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            sort_array(&v, out)
        }
        "join" => {
            require_arity(&name, args, 2)?;
            let sep_v = eval_buf(json, start, end, &args[0], ctx)?;
            let arr_v = eval_buf(json, start, end, &args[1], ctx)?;
            let sep = json_string_content(trim_json(&sep_v)).ok_or(Error::Jmespath {
                msg: "join separator must be a string",
            })?;
            join_array(trim_json(&arr_v), sep, out)
        }
        "max" | "min" | "sum" | "avg" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            array_reduce(trim_json(&v), name.as_str(), out)
        }
        "abs" | "ceil" | "floor" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            let n = parse_f64(trim_json(&v)).map_err(|_| Error::Jmespath {
                msg: "abs/ceil/floor require a number",
            })?;
            let r = match name.as_str() {
                "abs" => n.abs(),
                "ceil" => n.ceil(),
                _ => n.floor(),
            };
            out.emit_bytes(format_number(r).as_bytes())?;
            Ok(())
        }
        "to_array" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            let t = trim_json(&v);
            if t.starts_with(b"[") {
                out.emit_bytes(t)?;
            } else {
                out.emit_byte(b'[')?;
                out.emit_bytes(t)?;
                out.emit_byte(b']')?;
            }
            Ok(())
        }
        "merge" => {
            // shallow merge of objects
            out.emit_byte(b'{')?;
            let mut first = true;
            for a in args {
                let v = eval_buf(json, start, end, a, ctx)?;
                let t = trim_json(&v);
                if !t.starts_with(b"{") {
                    continue;
                }
                let (_, members, _) = collect_object_members(t, 0, t.len())?;
                for m in members {
                    if !first {
                        out.emit_byte(b',')?;
                    }
                    out.emit_bytes(&t[m.key_span.0..m.key_span.1])?;
                    out.emit_byte(b':')?;
                    out.emit_bytes(&t[m.val_start..m.val_end])?;
                    first = false;
                }
            }
            out.emit_byte(b'}')?;
            Ok(())
        }
        // map(&expr, array) — apply expr to each element
        "map" => {
            if args.len() < 2 {
                return Err(Error::Jmespath {
                    msg: "map requires (&expression, array)",
                });
            }
            let mapper = unwrap_expref(&args[0]).clone();
            let arr_v = eval_buf(json, start, end, &args[1], ctx)?;
            let arr = trim_json(&arr_v);
            let elems = collect_array_elems(arr, 0, arr.len())?;
            out.emit_byte(b'[')?;
            for (i, &(s, e)) in elems.iter().enumerate() {
                if i > 0 {
                    out.emit_byte(b',')?;
                }
                let mut piece = Vec::new();
                match emit_value(arr, s, e, &mapper, ctx, &mut piece) {
                    Ok(()) => out.emit_bytes(&piece)?,
                    Err(Error::PathNotFound) => out.emit_bytes(b"null")?,
                    Err(err) => return Err(err),
                }
            }
            out.emit_byte(b']')?;
            Ok(())
        }
        // sort_by(array, &expr)
        "sort_by" => {
            if args.len() < 2 {
                return Err(Error::Jmespath {
                    msg: "sort_by requires (array, &expression)",
                });
            }
            let arr_v = eval_buf(json, start, end, &args[0], ctx)?;
            let key_expr = unwrap_expref(&args[1]).clone();
            let arr = trim_json(&arr_v);
            if !arr.starts_with(b"[") {
                if soft_null(ctx) {
                    out.emit_bytes(b"null")?;
                    return Ok(());
                }
                return Err(Error::TypeMismatch {
                    expected: "array",
                    found: type_name_json(arr),
                });
            }
            let elems = collect_array_elems(arr, 0, arr.len())?;
            let mut keyed: Vec<(Vec<u8>, usize, usize)> = Vec::with_capacity(elems.len());
            for &(s, e) in &elems {
                let key = eval_buf(arr, s, e, &key_expr, ctx)?;
                let key = trim_json(&key).to_vec();
                // sort_by requires comparable non-null keys of a single type.
                if key.as_slice() == b"null" {
                    return Err(Error::Jmespath {
                        msg: "sort_by key evaluated to null",
                    });
                }
                keyed.push((key, s, e));
            }
            for i in 0..keyed.len() {
                for j in i + 1..keyed.len() {
                    let _ = cmp_sort_keys_strict(&keyed[i].0, &keyed[j].0)?;
                }
            }
            keyed.sort_by(|a, b| cmp_sort_keys(&a.0, &b.0));
            out.emit_byte(b'[')?;
            for (i, (_, s, e)) in keyed.iter().enumerate() {
                if i > 0 {
                    out.emit_byte(b',')?;
                }
                out.emit_bytes(&arr[*s..*e])?;
            }
            out.emit_byte(b']')?;
            Ok(())
        }
        // group_by(array, &expr) → array of groups (arrays of original elements)
        "group_by" => {
            if args.len() < 2 {
                return Err(Error::Jmespath {
                    msg: "group_by requires (array, &expression)",
                });
            }
            let arr_v = eval_buf(json, start, end, &args[0], ctx)?;
            let key_expr = unwrap_expref(&args[1]).clone();
            let arr = trim_json(&arr_v);
            if !arr.starts_with(b"[") {
                if soft_null(ctx) {
                    out.emit_bytes(b"null")?;
                    return Ok(());
                }
                return Err(Error::TypeMismatch {
                    expected: "array",
                    found: type_name_json(arr),
                });
            }
            let elems = collect_array_elems(arr, 0, arr.len())?;
            // Preserve first-seen key order
            let mut order: Vec<Vec<u8>> = Vec::new();
            let mut groups: std::collections::HashMap<Vec<u8>, Vec<(usize, usize)>> =
                std::collections::HashMap::new();
            for &(s, e) in &elems {
                let key = eval_buf(arr, s, e, &key_expr, ctx).unwrap_or_else(|_| b"null".to_vec());
                let key = trim_json(&key).to_vec();
                if !groups.contains_key(&key) {
                    order.push(key.clone());
                    groups.insert(key.clone(), Vec::new());
                }
                groups.get_mut(&key).unwrap().push((s, e));
            }
            out.emit_byte(b'[')?;
            for (gi, k) in order.iter().enumerate() {
                if gi > 0 {
                    out.emit_byte(b',')?;
                }
                out.emit_byte(b'[')?;
                let g = groups.get(k).unwrap();
                for (i, &(s, e)) in g.iter().enumerate() {
                    if i > 0 {
                        out.emit_byte(b',')?;
                    }
                    out.emit_bytes(&arr[s..e])?;
                }
                out.emit_byte(b']')?;
            }
            out.emit_byte(b']')?;
            Ok(())
        }
        // max_by(array, &expr) / min_by(array, &expr) → single element or null
        "max_by" | "min_by" => {
            if args.len() < 2 {
                return Err(Error::Jmespath {
                    msg: "max_by/min_by requires (array, &expression)",
                });
            }
            let want_max = name == "max_by";
            let arr_v = eval_buf(json, start, end, &args[0], ctx)?;
            let key_expr = unwrap_expref(&args[1]).clone();
            let arr = trim_json(&arr_v);
            if !arr.starts_with(b"[") {
                if soft_null(ctx) {
                    out.emit_bytes(b"null")?;
                    return Ok(());
                }
                return Err(Error::TypeMismatch {
                    expected: "array",
                    found: type_name_json(arr),
                });
            }
            let elems = collect_array_elems(arr, 0, arr.len())?;
            if elems.is_empty() {
                out.emit_bytes(b"null")?;
                return Ok(());
            }
            let mut best_i = 0usize;
            let mut best_key: Option<Vec<u8>> = None;
            for (i, &(s, e)) in elems.iter().enumerate() {
                let key = eval_buf(arr, s, e, &key_expr, ctx)?;
                let key = trim_json(&key).to_vec();
                // Any null key is a type error (missing fields, etc.).
                if key.as_slice() == b"null" {
                    return Err(Error::Jmespath {
                        msg: "max_by/min_by key evaluated to null",
                    });
                }
                match &best_key {
                    None => {
                        best_key = Some(key);
                        best_i = i;
                    }
                    Some(bk) => {
                        let ord = cmp_sort_keys_strict(&key, bk)?;
                        let better = if want_max {
                            ord == std::cmp::Ordering::Greater
                        } else {
                            ord == std::cmp::Ordering::Less
                        };
                        if better {
                            best_key = Some(key);
                            best_i = i;
                        }
                    }
                }
            }
            if best_key.is_none() {
                out.emit_bytes(b"null")?;
                return Ok(());
            }
            let (s, e) = elems[best_i];
            out.emit_bytes(&arr[s..e])?;
            Ok(())
        }
        _ => Err(Error::Jmespath {
            msg: "Unknown JMESPath function",
        }),
    }
}

fn cmp_sort_keys(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    cmp_sort_keys_strict(a, b).unwrap_or(std::cmp::Ordering::Equal)
}

/// Comparable JMESPath keys: both numbers or both strings; else error.
fn cmp_sort_keys_strict(a: &[u8], b: &[u8]) -> Result<std::cmp::Ordering, Error> {
    if let (Ok(na), Ok(nb)) = (parse_f64(a), parse_f64(b)) {
        return Ok(na
            .partial_cmp(&nb)
            .unwrap_or(std::cmp::Ordering::Equal));
    }
    if let (Some(sa), Some(sb)) = (json_string_content(a), json_string_content(b)) {
        return Ok(sa.cmp(sb));
    }
    // nulls sort equal only to null
    if a == b"null" && b == b"null" {
        return Ok(std::cmp::Ordering::Equal);
    }
    Err(Error::Jmespath {
        msg: "incomparable types in sort_by/max_by/min_by key",
    })
}

fn write_json_string_out(out: &mut impl EmitOut, s: &str) -> Result<(), Error> {
    out.emit_byte(b'"')?;
    for b in s.bytes() {
        match b {
            b'"' | b'\\' => {
                out.emit_byte(b'\\')?;
                out.emit_byte(b)?;
            }
            c if c < 0x20 => {
                out.emit_bytes(format!("\\u{c:04x}").as_bytes())?;
            }
            c => out.emit_byte(c)?,
        }
    }
    out.emit_byte(b'"')?;
    Ok(())
}

fn require_arity(name: &str, args: &[SelectExpr], n: usize) -> Result<(), Error> {
    if args.len() != n {
        return Err(Error::Jmespath {
            msg: "wrong number of arguments to function",
        });
    }
    let _ = name;
    Ok(())
}

fn length_of(v: &[u8]) -> Result<usize, Error> {
    let t = trim_json(v);
    if t.starts_with(b"\"") {
        let raw = json_string_content(t).ok_or(Error::Jmespath {
            msg: "length: invalid string",
        })?;
        let unesc = unescape_string_content(raw).unwrap_or_else(|| raw.to_vec());
        let s = std::str::from_utf8(&unesc).map_err(|_| Error::Jmespath {
            msg: "length: invalid utf-8 string",
        })?;
        // JMESPath counts Unicode code points (chars), not UTF-8 bytes.
        return Ok(s.chars().count());
    }
    if t.starts_with(b"[") {
        return Ok(collect_array_elems(t, 0, t.len())?.len());
    }
    if t.starts_with(b"{") {
        return Ok(collect_object_members(t, 0, t.len())?.1.len());
    }
    Err(Error::Jmespath {
        msg: "length requires string, array, or object",
    })
}

/// Best-effort unescape of JSON string content (handles common escapes).
fn unescape_string_content(content: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(content.len());
    let mut i = 0;
    while i < content.len() {
        if content[i] != b'\\' {
            out.push(content[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= content.len() {
            return None;
        }
        match content[i] {
            b'"' | b'\\' | b'/' => out.push(content[i]),
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'u' if i + 4 < content.len() => {
                let hex = std::str::from_utf8(&content[i + 1..i + 5]).ok()?;
                let cp = u32::from_str_radix(hex, 16).ok()?;
                let ch = char::from_u32(cp)?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                i += 4;
            }
            _ => out.push(content[i]),
        }
        i += 1;
    }
    Some(out)
}

fn keys_of(v: &[u8], out: &mut impl EmitOut) -> Result<(), Error> {
    let t = trim_json(v);
    if !t.starts_with(b"{") {
        return Err(Error::Jmespath {
            msg: "keys requires an object",
        });
    }
    let (_, members, _) = collect_object_members(t, 0, t.len())?;
    out.emit_byte(b'[')?;
    for (i, m) in members.iter().enumerate() {
        if i > 0 {
            out.emit_byte(b',')?;
        }
        out.emit_bytes(&t[m.key_span.0..m.key_span.1])?;
    }
    out.emit_byte(b']')?;
    Ok(())
}

fn values_of(v: &[u8], out: &mut impl EmitOut) -> Result<(), Error> {
    let t = trim_json(v);
    if !t.starts_with(b"{") {
        return Err(Error::Jmespath {
            msg: "values requires an object",
        });
    }
    let (_, members, _) = collect_object_members(t, 0, t.len())?;
    out.emit_byte(b'[')?;
    for (i, m) in members.iter().enumerate() {
        if i > 0 {
            out.emit_byte(b',')?;
        }
        out.emit_bytes(&t[m.val_start..m.val_end])?;
    }
    out.emit_byte(b']')?;
    Ok(())
}

fn type_name_json(v: &[u8]) -> &'static str {
    let t = trim_json(v);
    match t.first() {
        Some(b'{') => "object",
        Some(b'[') => "array",
        Some(b'"') => "string",
        Some(b't') | Some(b'f') => "boolean",
        Some(b'n') => "null",
        Some(b'-') | Some(b'0'..=b'9') => "number",
        _ => "null",
    }
}

fn array_contains(arr: &[u8], item: &[u8]) -> Result<bool, Error> {
    let elems = collect_array_elems(arr, 0, arr.len())?;
    Ok(elems
        .iter()
        .any(|&(s, e)| trim_json(&arr[s..e]) == item))
}

fn reverse_array_or_string(v: &[u8], out: &mut impl EmitOut) -> Result<(), Error> {
    let t = trim_json(v);
    if t.starts_with(b"\"") {
        let s = json_string_content(t).unwrap_or(b"");
        let rev: Vec<u8> = s.iter().rev().copied().collect();
        write_json_string_out(out, std::str::from_utf8(&rev).unwrap_or(""))?;
        return Ok(());
    }
    let elems = collect_array_elems(t, 0, t.len())?;
    out.emit_byte(b'[')?;
    for (i, &(s, e)) in elems.iter().rev().enumerate() {
        if i > 0 {
            out.emit_byte(b',')?;
        }
        out.emit_bytes(&t[s..e])?;
    }
    out.emit_byte(b']')?;
    Ok(())
}


fn sort_array(v: &[u8], out: &mut impl EmitOut) -> Result<(), Error> {
    let t = trim_json(v);
    if !t.starts_with(b"[") {
        return Err(Error::Jmespath {
            msg: "sort requires an array",
        });
    }
    let elems = collect_array_elems(t, 0, t.len())?;
    let mut pieces: Vec<&[u8]> = elems.iter().map(|&(s, e)| &t[s..e]).collect();
    // Strict: all numbers or all strings.
    let mut kind: Option<bool> = None; // true=num, false=str
    for p in &pieces {
        let ta = trim_json(p);
        let is_num = parse_f64(ta).is_ok();
        let is_str = json_string_content(ta).is_some();
        if is_num == is_str {
            return Err(Error::Jmespath {
                msg: "sort: invalid element type",
            });
        }
        match kind {
            None => kind = Some(is_num),
            Some(k) if k != is_num => {
                return Err(Error::Jmespath {
                    msg: "sort: mixed number/string array",
                });
            }
            _ => {}
        }
    }
    pieces.sort_by(|a, b| {
        let ta = trim_json(a);
        let tb = trim_json(b);
        if let (Ok(na), Ok(nb)) = (parse_f64(ta), parse_f64(tb)) {
            na.partial_cmp(&nb)
                .unwrap_or(std::cmp::Ordering::Equal)
        } else if let (Some(sa), Some(sb)) = (json_string_content(ta), json_string_content(tb)) {
            sa.cmp(sb)
        } else {
            ta.cmp(tb)
        }
    });
    out.emit_byte(b'[')?;
    for (i, p) in pieces.iter().enumerate() {
        if i > 0 {
            out.emit_byte(b',')?;
        }
        out.emit_bytes(p)?;
    }
    out.emit_byte(b']')?;
    Ok(())
}

fn join_array(arr: &[u8], sep: &[u8], out: &mut impl EmitOut) -> Result<(), Error> {
    if !arr.starts_with(b"[") {
        return Err(Error::Jmespath {
            msg: "join requires an array",
        });
    }
    let elems = collect_array_elems(arr, 0, arr.len())?;
    let mut s = Vec::new();
    for (i, &(a, b)) in elems.iter().enumerate() {
        let el = trim_json(&arr[a..b]);
        let c = json_string_content(el).ok_or(Error::Jmespath {
            msg: "join array elements must be strings",
        })?;
        if i > 0 {
            // sep is wire content; unescape for output body
            let sep_u = unescape_string_content(sep).unwrap_or_else(|| sep.to_vec());
            s.extend_from_slice(&sep_u);
        }
        let cu = unescape_string_content(c).unwrap_or_else(|| c.to_vec());
        s.extend_from_slice(&cu);
    }
    write_json_string_out(out, std::str::from_utf8(&s).unwrap_or(""))?;
    Ok(())
}

fn array_reduce(arr: &[u8], which: &str, out: &mut impl EmitOut) -> Result<(), Error> {
    if !arr.starts_with(b"[") {
        return Err(Error::Jmespath {
            msg: "max/min/sum/avg require an array",
        });
    }
    let elems = collect_array_elems(arr, 0, arr.len())?;
    if elems.is_empty() {
        // JMESPath: max/min of empty → null; sum of empty → 0; avg of empty → null
        match which {
            "sum" => {
                out.emit_bytes(b"0")?;
                return Ok(());
            }
            _ => {
                out.emit_bytes(b"null")?;
                return Ok(());
            }
        }
    }

    // Classify: all numbers or all strings (for max/min); sum/avg numbers only.
    let mut nums: Vec<f64> = Vec::new();
    let mut strs: Vec<Vec<u8>> = Vec::new();
    let mut saw_num = false;
    let mut saw_str = false;
    for &(s, e) in &elems {
        let t = trim_json(&arr[s..e]);
        if let Ok(n) = parse_f64(t) {
            saw_num = true;
            nums.push(n);
        } else if let Some(c) = json_string_content(t) {
            saw_str = true;
            strs.push(unescape_string_content(c).unwrap_or_else(|| c.to_vec()));
        } else {
            return Err(Error::Jmespath {
                msg: "max/min/sum/avg: invalid array element type",
            });
        }
    }
    if saw_num && saw_str {
        return Err(Error::Jmespath {
            msg: "max/min/sum/avg: mixed number/string array",
        });
    }

    match which {
        "sum" | "avg" => {
            if saw_str {
                return Err(Error::Jmespath {
                    msg: "sum/avg require number array",
                });
            }
            let sum: f64 = nums.iter().sum();
            let r = if which == "avg" {
                sum / nums.len() as f64
            } else {
                sum
            };
            out.emit_bytes(format_number(r).as_bytes())?;
        }
        "max" | "min" => {
            if saw_num {
                let r = if which == "max" {
                    nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                } else {
                    nums.iter().cloned().fold(f64::INFINITY, f64::min)
                };
                out.emit_bytes(format_number(r).as_bytes())?;
            } else {
                let best = if which == "max" {
                    strs.iter().max().cloned()
                } else {
                    strs.iter().min().cloned()
                };
                match best {
                    Some(s) => write_json_string_out(out, std::str::from_utf8(&s).unwrap_or(""))?,
                    None => out.emit_bytes(b"null")?,
                }
            }
        }
        _ => out.emit_bytes(b"null")?,
    }
    Ok(())
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn emit_multi_hash(
    json: &[u8],
    start: usize,
    end: usize,
    fields: &[HashField],
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    // Pure-field multi-select (`{a: a, b: b}` or renames `{out: src}`): **one** object
    // scan for all requested keys (any domain / key names). Avoids N× full key hunts.
    if !fields.is_empty()
        && fields
            .iter()
            .all(|f| matches!(f.expr, SelectExpr::Field(_) | SelectExpr::FieldQuoted(_)))
    {
        return emit_multi_hash_fields_one_pass(json, start, end, fields, ctx, out);
    }

    // Mixed / nested exprs: evaluate each field expression independently.
    let compact = matches!(ctx.plan.style, ProjectStyle::Compact);
    out.emit_byte(b'{')?;
    let mut wrote = false;
    for f in fields {
        let mut val = Vec::new();
        match emit_value(json, start, end, &f.expr, ctx, &mut val) {
            Ok(()) => {}
            Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. })
                if ctx.plan.missing == MissingPolicy::Skip =>
            {
                // Nested expr failed soft → JSON null (JMESPath multi-select).
                if wrote {
                    out.emit_byte(b',')?;
                }
                if !compact {
                    maybe_pretty_newline_indent(ctx, out, true)?;
                }
                write_json_key(out, &f.output_key)?;
                if compact {
                    out.emit_byte(b':')?;
                } else {
                    match ctx.plan.style {
                        ProjectStyle::Pretty { .. } => out.emit_bytes(b": ")?,
                        _ => out.emit_byte(b':')?,
                    }
                }
                out.emit_bytes(b"null")?;
                wrote = true;
                continue;
            }
            Err(e) => return Err(e),
        }
        if wrote {
            out.emit_byte(b',')?;
        }
        if !compact {
            maybe_pretty_newline_indent(ctx, out, true)?;
        }
        write_json_key(out, &f.output_key)?;
        if compact {
            out.emit_byte(b':')?;
            out.emit_bytes(&val)?;
        } else {
            match ctx.plan.style {
                ProjectStyle::Pretty { .. } => out.emit_bytes(b": ")?,
                _ => out.emit_byte(b':')?,
            }
            ctx.depth += 1;
            out.emit_bytes(&val)?;
            ctx.depth -= 1;
        }
        wrote = true;
    }
    if compact {
        out.emit_byte(b'}')?;
    } else {
        emit_close_object(ctx, out)?;
    }
    let _ = end;
    Ok(())
}

/// Collect pure-field multi-select values in **one** left-to-right object scan.
///
/// - Works for any key set / rename map (`{title: name}` reads on-wire `name`).
/// - Stops early once every requested key is found (skips trailing noise fields).
/// - Uses object side-tables when indexed.
/// - Missing keys → `null` under [`MissingPolicy::Skip`] (JMESPath multi-select).
/// - Emit phase: under [`ProjectStyle::Compact`] writes `{k:v,...}` with no indent
///   depth or pretty colon/newline bookkeeping (see [`emit_multi_hash_from_spans`]).
fn emit_multi_hash_fields_one_pass(
    json: &[u8],
    start: usize,
    end: usize,
    fields: &[HashField],
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    use std::collections::HashMap;

    // on-wire key bytes → output slot indices (same source field can map to several outputs).
    let mut want: HashMap<Vec<u8>, Vec<usize>> = HashMap::with_capacity(fields.len());
    for (i, f) in fields.iter().enumerate() {
        let name = match &f.expr {
            SelectExpr::Field(k) | SelectExpr::FieldQuoted(k) => escape_json_key(k),
            _ => unreachable!("caller guarantees pure Field/FieldQuoted"),
        };
        want.entry(name.into_bytes()).or_default().push(i);
    }

    let mut spans: Vec<Option<(usize, usize)>> = vec![None; fields.len()];
    let obj_start = skip_whitespace(json, start);
    if obj_start >= json.len() || json[obj_start] != b'{' {
        if soft_null(ctx) {
            return emit_multi_hash_from_spans(fields, &spans, json, ctx, out);
        }
        return Err(Error::TypeMismatch {
            expected: "object",
            found: type_name_at(json, obj_start),
        });
    }

    // O(k) via object key map when this object was indexed.
    if let Some(doc) = ctx.index {
        if let Some(oi) = doc.object_index_at_open(obj_start) {
            for (wire, idxs) in &want {
                if let Some((vs, ve)) = oi.get(wire) {
                    for &i in idxs {
                        spans[i] = Some((vs, ve));
                    }
                }
            }
            return emit_multi_hash_from_spans(fields, &spans, json, ctx, out);
        }
    }

    // Single linear scan: each member visited at most once.
    let mut pos = obj_start + 1;
    let mut remaining = want.len();
    while remaining > 0 {
        pos = skip_whitespace(json, pos);
        if pos >= end || json.get(pos) == Some(&b'}') {
            break;
        }
        if json.get(pos) != Some(&b'"') {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected object key in multi-select",
            });
        }
        let key_end = find_string_end(json, pos + 1)?;
        let key_on_wire = &json[pos + 1..key_end];
        pos = key_end + 1;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] != b':' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ':' after object key",
            });
        }
        pos += 1;
        pos = skip_whitespace(json, pos);
        let vs = pos;
        let ve = skip_value_smart(json, vs, ctx.index)?;
        if let Some(idxs) = want.get(key_on_wire) {
            for &i in idxs {
                spans[i] = Some((vs, ve));
            }
            remaining -= 1;
        }
        pos = skip_whitespace(json, ve);
        if pos < json.len() && json[pos] == b',' {
            pos += 1;
        }
    }

    emit_multi_hash_from_spans(fields, &spans, json, ctx, out)
}

/// Write a multi-select hash from pre-collected value spans.
///
/// **Compact bulk path:** no `depth` updates, no pretty newline/indent, colon is a
/// single `:` byte, close is a single `}`. This is the hot path for thin-card
/// `[*].{a: a, b: b}` over large arrays. Pretty keeps full indent bookkeeping.
fn emit_multi_hash_from_spans(
    fields: &[HashField],
    spans: &[Option<(usize, usize)>],
    json: &[u8],
    ctx: &EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    if ctx.plan.missing == MissingPolicy::Error {
        for sp in spans.iter() {
            if sp.is_none() {
                return Err(Error::PathNotFound);
            }
        }
    }

    // Fast path: Compact (default) — pure bytes, no style branches per field.
    if matches!(ctx.plan.style, ProjectStyle::Compact) {
        out.emit_byte(b'{')?;
        for (i, f) in fields.iter().enumerate() {
            if i > 0 {
                out.emit_byte(b',')?;
            }
            write_json_key(out, &f.output_key)?;
            out.emit_byte(b':')?;
            match spans[i] {
                Some((vs, ve)) => out.emit_bytes(&json[vs..ve])?,
                None => out.emit_bytes(b"null")?,
            }
        }
        out.emit_byte(b'}')?;
        return Ok(());
    }

    out.emit_byte(b'{')?;
    for (i, f) in fields.iter().enumerate() {
        if i > 0 {
            out.emit_byte(b',')?;
        }
        maybe_pretty_newline_indent(ctx, out, true)?;
        write_json_key(out, &f.output_key)?;
        match ctx.plan.style {
            ProjectStyle::Pretty { .. } => out.emit_bytes(b": ")?,
            ProjectStyle::Compact | ProjectStyle::PreserveSource => out.emit_byte(b':')?,
        }
        match spans[i] {
            Some((vs, ve)) => out.emit_bytes(&json[vs..ve])?,
            // JMESPath multi-select hash: missing → null (Skip policy).
            None => out.emit_bytes(b"null")?,
        }
    }
    emit_close_object(ctx, out)?;
    Ok(())
}

fn emit_multi_list(
    json: &[u8],
    start: usize,
    end: usize,
    items: &[SelectExpr],
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    let compact = matches!(ctx.plan.style, ProjectStyle::Compact);
    out.emit_byte(b'[')?;
    let mut wrote = false;
    for expr in items {
        let mut val = Vec::new();
        match emit_value(json, start, end, expr, ctx, &mut val) {
            Ok(()) => {}
            Err(Error::PathNotFound) if ctx.plan.missing == MissingPolicy::Skip => continue,
            Err(e) => return Err(e),
        }
        if wrote {
            out.emit_byte(b',')?;
        }
        if compact {
            out.emit_bytes(&val)?;
        } else {
            maybe_pretty_newline_indent(ctx, out, true)?;
            ctx.depth += 1;
            out.emit_bytes(&val)?;
            ctx.depth -= 1;
        }
        wrote = true;
    }
    if compact {
        out.emit_byte(b']')?;
    } else {
        emit_close_array(ctx, out)?;
    }
    let _ = end;
    Ok(())
}

fn flatten_emit(mid: &[u8], ctx: &mut EmitCtx<'_>, out: &mut impl EmitOut) -> Result<(), Error> {
    let start = skip_whitespace(mid, 0);
    let end = skip_value(mid, start)?;
    let start = skip_whitespace(mid, start);
    if start >= mid.len() || mid[start] != b'[' {
        // JMESPath: flatten of non-array → null
        out.emit_bytes(b"null")?;
        let _ = end;
        let _ = ctx;
        return Ok(());
    }
    let outer = collect_array_elems(mid, start, end)?;
    out.emit_byte(b'[')?;
    let mut first = true;
    for (s, e) in outer {
        let s = skip_whitespace(mid, s);
        if s < mid.len() && mid[s] == b'[' {
            let inner = collect_array_elems(mid, s, e)?;
            for (is, ie) in inner {
                if !first {
                    out.emit_byte(b',')?;
                }
                maybe_pretty_newline_indent(ctx, out, true)?;
                out.emit_bytes(&mid[is..ie])?;
                first = false;
            }
        } else {
            if !first {
                out.emit_byte(b',')?;
            }
            maybe_pretty_newline_indent(ctx, out, true)?;
            out.emit_bytes(&mid[s..e])?;
            first = false;
        }
    }
    emit_close_array(ctx, out)?;
    Ok(())
}

fn write_json_key(out: &mut impl EmitOut, key: &str) -> Result<(), Error> {
    write_json_string_out(out, key)
}

/// Strip insignificant whitespace outside JSON strings (for `to_string` compactness).
fn compact_json(v: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len());
    let mut i = 0;
    let mut in_string = false;
    let mut escape = false;
    while i < v.len() {
        let b = v[i];
        if in_string {
            out.push(b);
            if escape {
                escape = false;
            } else if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            i += 1;
            continue;
        }
        match b {
            b'"' => {
                in_string = true;
                out.push(b);
            }
            b' ' | b'\t' | b'\n' | b'\r' => {}
            _ => out.push(b),
        }
        i += 1;
    }
    out
}


fn emit_object_key(json: &[u8], m: &Member<'_>, ctx: &EmitCtx<'_>, out: &mut impl EmitOut) -> Result<(), Error> {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { .. }) {
        out.emit_byte(b'\n')?;
        write_indent(ctx.depth + 1, pretty_indent(ctx), out)?;
    }
    out.emit_bytes(&json[m.key_span.0..m.key_span.1])?;
    Ok(())
}

fn emit_colon(json: &[u8], m: &Member<'_>, ctx: &EmitCtx<'_>, out: &mut impl EmitOut) -> Result<(), Error> {
    match ctx.plan.style {
        ProjectStyle::Compact => out.emit_byte(b':')?,
        ProjectStyle::PreserveSource => {
            out.emit_bytes(&json[m.key_span.1..m.colon])?;
            out.emit_byte(b':')?;
            out.emit_bytes(&json[m.colon + 1..m.val_start])?;
        }
        ProjectStyle::Pretty { .. } => out.emit_bytes(b": ")?,
    }
    Ok(())
}

#[inline]
fn emit_close_object(ctx: &EmitCtx<'_>, out: &mut impl EmitOut) -> Result<(), Error> {
    // Compact / PreserveSource: just `}`. Pretty may need a closing indent line.
    if matches!(ctx.plan.style, ProjectStyle::Pretty { indent: n } if n > 0)
        && out.last_byte() != Some(b'{')
    {
        out.emit_byte(b'\n')?;
        write_indent(ctx.depth, pretty_indent(ctx), out)?;
    }
    out.emit_byte(b'}')?;
    Ok(())
}

#[inline]
fn emit_close_array(ctx: &EmitCtx<'_>, out: &mut impl EmitOut) -> Result<(), Error> {
    // Compact / PreserveSource: just `]`. Pretty may need a closing indent line.
    if matches!(ctx.plan.style, ProjectStyle::Pretty { indent: n } if n > 0)
        && out.last_byte() != Some(b'[')
    {
        out.emit_byte(b'\n')?;
        write_indent(ctx.depth, pretty_indent(ctx), out)?;
    }
    out.emit_byte(b']')?;
    Ok(())
}

/// Pretty-only newline + indent before an array/object element. **No-op on Compact**
/// (callers on the Compact bulk path should not invoke this at all).
#[inline]
fn maybe_pretty_newline_indent(ctx: &EmitCtx<'_>, out: &mut impl EmitOut, for_element: bool) -> Result<(), Error> {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { .. }) && for_element {
        out.emit_byte(b'\n')?;
        write_indent(ctx.depth + 1, pretty_indent(ctx), out)?;
    }
    Ok(())
}

fn pretty_indent(ctx: &EmitCtx<'_>) -> usize {
    match ctx.plan.style {
        ProjectStyle::Pretty { indent } => indent as usize,
        _ => 0,
    }
}

fn write_indent(depth: usize, per_level: usize, out: &mut impl EmitOut) -> Result<(), Error> {
    let n = depth.saturating_mul(per_level);
    let spaces = [b' '; 64];
    let mut left = n;
    while left > 0 {
        let chunk = left.min(spaces.len());
        out.emit_bytes(&spaces[..chunk])?;
        left -= chunk;
    }
    Ok(())
}

fn is_ws_only(s: &[u8]) -> bool {
    s.iter()
        .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
}

fn type_name_at(json: &[u8], pos: usize) -> &'static str {
    match json.get(pos).copied() {
        Some(b'{') => "object",
        Some(b'[') => "array",
        Some(b'"') => "string",
        Some(b't') | Some(b'f') => "bool",
        Some(b'n') => "null",
        Some(b'-') | Some(b'0'..=b'9') => "number",
        _ => "unknown",
    }
}

