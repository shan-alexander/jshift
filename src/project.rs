//! Field projection: build a smaller JSON document from a keep-list.
//!
//! # Architecture (designed to grow)
//!
//! Projection is a **selection AST** ([`SelectExpr`]) applied to a value span,
//! writing into a buffer. Today that AST is built from jshift path strings
//! ([`ProjectPlan::from_paths`]). Later iterations plug in without rewriting
//! the emitter:
//!
//! | Future surface | How it attaches |
//! | --- | --- |
//! | Full JMESPath | Parse → [`SelectExpr`] (pipe, flatten, filters, multi-select, …) |
//! | Deep transforms | New [`SelectExpr`] arms (map, rename, literals, functions) |
//! | Whitespace fidelity | [`ProjectStyle::PreserveSource`] / [`ProjectStyle::Pretty`] |
//!
//! Kept leaf values are copied as **raw on-wire spans** (no re-encode of numbers
//! or strings). Structural framing (`{ } [ ] , :`) is emitted according to
//! [`ProjectStyle`].
//!
//! # Missing paths
//!
//! By default ([`MissingPolicy::Skip`]), absent keys/indices are omitted from
//! the output. Use [`MissingPolicy::Error`] to fail with [`Error::PathNotFound`].

use std::collections::HashMap;
use std::io::Write;

use crate::error::Error;
use crate::path::{parse_path, PathSegment};
use crate::scan::{find_string_end, find_value, skip_value, skip_whitespace};

// ─── Public style / policy ───────────────────────────────────────────────────

/// How projected structure is formatted.
///
/// Marked `#[non_exhaustive]`: pretty-print knobs and additional fidelity modes
/// will land without a major bump.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectStyle {
    /// Minimal separators: `{"a":1,"b":2}`. Default.
    #[default]
    Compact,
    /// Prefer source spacing around kept keys / colons / values when copying
    /// object members (and identity leaves). Structural gaps between non-adjacent
    /// kept members use compact commas.
    ///
    /// Further fidelity (full pretty replay, comment-like gaps) extends this mode.
    PreserveSource,
    /// Emit multi-line pretty JSON with `indent` spaces per level.
    Pretty {
        /// Spaces per nesting level (clamped in the emitter; 0 behaves like compact
        /// with newlines only if needed — prefer ≥ 2).
        indent: u8,
    },
}

/// Behavior when a selected path is missing in the input.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum MissingPolicy {
    /// Omit the missing field/element from the output (default).
    #[default]
    Skip,
    /// Fail with [`Error::PathNotFound`].
    Error,
}

// ─── Selection AST ───────────────────────────────────────────────────────────

/// One step of a projection path (extends ordinary path segments with wildcards).
///
/// Built by [`parse_project_path`]. Ordinary [`parse_path`] does not accept `[]` /
/// `[*]`; use this for projection keep-lists.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProjectPathSegment {
    /// Object key (on-wire form, same rules as [`PathSegment::Key`]).
    Key(String),
    /// Fixed array index.
    Index(usize),
    /// Every array element (`[]` or `[*]`).
    ArrayWildcard,
}

/// Selection expression over a JSON value.
///
/// This is the stable extension point for JMESPath / transforms: new query
/// shapes become new arms (or nested structs), while [`project`] / [`project_into`]
/// stay the execution entry points.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectExpr {
    /// Keep the entire current value (raw byte copy of the value span).
    Identity,
    /// Project an object by selecting named fields (each with a nested expr).
    Object(ObjectSelect),
    /// Project an array (every element, or specific indices).
    Array(ArraySelect),
    // Future (documented, not yet variants):
    // Pipe(Box<SelectExpr>, Box<SelectExpr>),
    // Flatten(Box<SelectExpr>),
    // MultiSelectList(Vec<SelectExpr>),
    // Literal(Vec<u8>),
    // Function { name: String, args: Vec<SelectExpr> },
}

/// Object field selection: key → nested expression.
///
/// Keys are stored as **on-wire** content (between quotes), matching path keys.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObjectSelect {
    /// Field selections. Lookup is by key; emission order follows **document order**
    /// of keys that appear in the input (stable, fidelity-friendly).
    fields: HashMap<String, SelectExpr>,
}

impl ObjectSelect {
    /// Empty object selection (projects to `{}` if the value is an object).
    pub fn new() -> Self {
        Self {
            fields: HashMap::new(),
        }
    }

    /// Number of selected fields.
    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Iterate selected keys (arbitrary hash order).
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(|s| s.as_str())
    }

    /// Nested selection for `key`, if any.
    pub fn get(&self, key: &str) -> Option<&SelectExpr> {
        self.fields.get(key)
    }
}

/// Array projection strategy.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArraySelect {
    /// Apply the same expression to every element (`products[].title`).
    Each(Box<SelectExpr>),
    /// Project only listed indices (`products[0].id`, `products[2].id`).
    /// Emission order is ascending index order among those present.
    Indices(HashMap<usize, SelectExpr>),
}

// ─── Plan ────────────────────────────────────────────────────────────────────

/// Compiled projection plan: selection AST + formatting + missing policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPlan {
    root: SelectExpr,
    style: ProjectStyle,
    missing: MissingPolicy,
}

impl Default for ProjectPlan {
    fn default() -> Self {
        Self {
            root: SelectExpr::Identity,
            style: ProjectStyle::Compact,
            missing: MissingPolicy::Skip,
        }
    }
}

impl ProjectPlan {
    /// Keep the whole document (identity projection).
    pub fn identity() -> Self {
        Self::default()
    }

    /// Build a plan from path keep-list strings (jshift path syntax + `[]` / `[*]`).
    ///
    /// Paths are **merged** into one tree (e.g. `a.b` + `a.c` → one object `a`).
    ///
    /// ```
    /// use jshift::{project, ProjectPlan};
    ///
    /// let plan = ProjectPlan::from_paths(&["id", "title"]).unwrap();
    /// let out = project(
    ///     br#"{"id":1,"title":"x","blob":[1,2,3]}"#,
    ///     &plan,
    /// ).unwrap();
    /// assert_eq!(out, br#"{"id":1,"title":"x"}"#);
    /// ```
    pub fn from_paths(paths: &[&str]) -> Result<Self, Error> {
        let mut root = SelectExpr::Object(ObjectSelect::new());
        if paths.is_empty() {
            return Ok(Self {
                root,
                style: ProjectStyle::Compact,
                missing: MissingPolicy::Skip,
            });
        }
        for p in paths {
            let segs = parse_project_path(p)?;
            merge_segments(&mut root, &segs)?;
        }
        Ok(Self {
            root,
            style: ProjectStyle::Compact,
            missing: MissingPolicy::Skip,
        })
    }

    /// Build from pre-parsed project paths.
    pub fn from_project_paths(paths: &[Vec<ProjectPathSegment>]) -> Result<Self, Error> {
        let mut root = SelectExpr::Object(ObjectSelect::new());
        if paths.is_empty() {
            return Ok(Self {
                root,
                style: ProjectStyle::Compact,
                missing: MissingPolicy::Skip,
            });
        }
        for segs in paths {
            merge_segments(&mut root, segs)?;
        }
        Ok(Self {
            root,
            style: ProjectStyle::Compact,
            missing: MissingPolicy::Skip,
        })
    }

    /// Wrap an existing selection expression.
    pub fn from_select(root: SelectExpr) -> Self {
        Self {
            root,
            style: ProjectStyle::Compact,
            missing: MissingPolicy::Skip,
        }
    }

    pub fn style(mut self, style: ProjectStyle) -> Self {
        self.style = style;
        self
    }

    pub fn missing_policy(mut self, missing: MissingPolicy) -> Self {
        self.missing = missing;
        self
    }

    pub fn select(&self) -> &SelectExpr {
        &self.root
    }

    pub fn project_style(&self) -> ProjectStyle {
        self.style
    }

    pub fn missing(&self) -> MissingPolicy {
        self.missing
    }
}

// ─── Path parse (projection) ─────────────────────────────────────────────────

/// Parse a projection path: ordinary keys/indexes plus `[]` / `[*]` wildcards.
///
/// Examples: `id`, `user.name`, `products[0].title`, `products[].sku`, `items[*].id`.
pub fn parse_project_path(s: &str) -> Result<Vec<ProjectPathSegment>, Error> {
    let mut rest = s;
    let mut segments = Vec::new();
    while !rest.is_empty() {
        if rest.starts_with('.') {
            rest = &rest[1..];
            continue;
        }
        if rest.starts_with('[') {
            let end_idx = rest.find(']').ok_or(Error::InvalidPath {
                msg: "Unclosed array index bracket '[' in project path",
            })?;
            let inner = &rest[1..end_idx];
            if inner.is_empty() || inner == "*" {
                segments.push(ProjectPathSegment::ArrayWildcard);
            } else if inner.bytes().all(|b| b.is_ascii_digit()) {
                let idx = inner.parse::<usize>().map_err(|_| Error::InvalidPath {
                    msg: "Array index out of range for usize",
                })?;
                segments.push(ProjectPathSegment::Index(idx));
            } else {
                return Err(Error::InvalidPath {
                    msg: "Invalid array selector in project path (use [N], [], or [*])",
                });
            }
            rest = &rest[end_idx + 1..];
        } else {
            let end_key = rest.find(['.', '[']).unwrap_or(rest.len());
            let key = &rest[..end_key];
            if key.is_empty() {
                return Err(Error::InvalidPath {
                    msg: "Empty key segment in project path",
                });
            }
            segments.push(ProjectPathSegment::Key(key.to_string()));
            rest = &rest[end_key..];
        }
    }
    if segments.is_empty() {
        return Err(Error::InvalidPath {
            msg: "Empty project path",
        });
    }
    Ok(segments)
}

fn merge_segments(node: &mut SelectExpr, segs: &[ProjectPathSegment]) -> Result<(), Error> {
    if segs.is_empty() {
        *node = SelectExpr::Identity;
        return Ok(());
    }
    // Identity already keeps everything; deeper paths are redundant.
    if matches!(node, SelectExpr::Identity) {
        return Ok(());
    }

    match &segs[0] {
        ProjectPathSegment::Key(k) => {
            if !matches!(node, SelectExpr::Object(_)) {
                if matches!(node, SelectExpr::Array(_)) {
                    return Err(Error::InvalidPath {
                        msg: "Project path conflict: object key under array selection",
                    });
                }
                *node = SelectExpr::Object(ObjectSelect::new());
            }
            let SelectExpr::Object(obj) = node else {
                unreachable!();
            };
            let child = obj
                .fields
                .entry(k.clone())
                .or_insert(SelectExpr::Object(ObjectSelect::new()));
            if segs.len() == 1 {
                *child = SelectExpr::Identity;
            } else {
                merge_segments(child, &segs[1..])?;
            }
            Ok(())
        }
        ProjectPathSegment::Index(i) => {
            ensure_array_indices(node)?;
            let SelectExpr::Array(arr) = node else {
                unreachable!();
            };
            match arr {
                ArraySelect::Each(each) => {
                    // Wildcard already present: merge remaining into each-child.
                    if segs.len() == 1 {
                        **each = SelectExpr::Identity;
                    } else {
                        merge_segments(each, &segs[1..])?;
                    }
                }
                ArraySelect::Indices(map) => {
                    let child = map
                        .entry(*i)
                        .or_insert(SelectExpr::Object(ObjectSelect::new()));
                    if segs.len() == 1 {
                        *child = SelectExpr::Identity;
                    } else {
                        merge_segments(child, &segs[1..])?;
                    }
                }
            }
            Ok(())
        }
        ProjectPathSegment::ArrayWildcard => {
            ensure_array_each(node)?;
            let SelectExpr::Array(ArraySelect::Each(each)) = node else {
                unreachable!();
            };
            if segs.len() == 1 {
                **each = SelectExpr::Identity;
            } else {
                merge_segments(each, &segs[1..])?;
            }
            Ok(())
        }
    }
}

fn ensure_array_indices(node: &mut SelectExpr) -> Result<(), Error> {
    match node {
        SelectExpr::Array(ArraySelect::Indices(_)) => Ok(()),
        SelectExpr::Array(ArraySelect::Each(_)) => Ok(()), // keep each; indices merge into each
        SelectExpr::Identity => {
            *node = SelectExpr::Array(ArraySelect::Indices(HashMap::new()));
            Ok(())
        }
        SelectExpr::Object(obj) if obj.is_empty() => {
            *node = SelectExpr::Array(ArraySelect::Indices(HashMap::new()));
            Ok(())
        }
        SelectExpr::Object(_) => Err(Error::InvalidPath {
            msg: "Project path conflict: array index under object selection",
        }),
    }
}

fn ensure_array_each(node: &mut SelectExpr) -> Result<(), Error> {
    match node {
        SelectExpr::Array(ArraySelect::Each(_)) => Ok(()),
        SelectExpr::Array(ArraySelect::Indices(map)) => {
            // Promote sparse indices to Each by merging all index children.
            let mut each = SelectExpr::Object(ObjectSelect::new());
            let old = std::mem::take(map);
            for (_, child) in old {
                merge_expr_union(&mut each, child)?;
            }
            *node = SelectExpr::Array(ArraySelect::Each(Box::new(each)));
            Ok(())
        }
        SelectExpr::Identity => {
            *node = SelectExpr::Array(ArraySelect::Each(Box::new(SelectExpr::Identity)));
            Ok(())
        }
        SelectExpr::Object(obj) if obj.is_empty() => {
            *node = SelectExpr::Array(ArraySelect::Each(Box::new(SelectExpr::Object(
                ObjectSelect::new(),
            ))));
            Ok(())
        }
        SelectExpr::Object(_) => Err(Error::InvalidPath {
            msg: "Project path conflict: array wildcard under object selection",
        }),
    }
}

/// Union two selection trees (for promoting indices → each).
fn merge_expr_union(into: &mut SelectExpr, other: SelectExpr) -> Result<(), Error> {
    if matches!(into, SelectExpr::Identity) || matches!(other, SelectExpr::Identity) {
        *into = SelectExpr::Identity;
        return Ok(());
    }
    match (into, other) {
        (SelectExpr::Object(a), SelectExpr::Object(b)) => {
            for (k, v) in b.fields {
                let entry = a
                    .fields
                    .entry(k)
                    .or_insert(SelectExpr::Object(ObjectSelect::new()));
                merge_expr_union(entry, v)?;
            }
            Ok(())
        }
        (SelectExpr::Array(ArraySelect::Each(a)), SelectExpr::Array(ArraySelect::Each(b))) => {
            merge_expr_union(a, *b)
        }
        (dst, src) => {
            // Fallback: replace with src if dst is empty object placeholder.
            if matches!(dst, SelectExpr::Object(o) if o.is_empty()) {
                *dst = src;
                Ok(())
            } else if matches!(&src, SelectExpr::Object(o) if o.is_empty()) {
                Ok(())
            } else {
                // Prefer keeping dst; best-effort.
                let _ = src;
                Ok(())
            }
        }
    }
}

// ─── Public project entry points ─────────────────────────────────────────────

/// Project `json` according to `plan`, returning a new buffer.
///
/// ```
/// use jshift::{project, ProjectPlan, ProjectStyle};
///
/// let json = br#"{"id":7,"title":"Hat","images":[1,2,3]}"#;
/// let plan = ProjectPlan::from_paths(&["id", "title"]).unwrap();
/// assert_eq!(project(json, &plan).unwrap(), br#"{"id":7,"title":"Hat"}"#);
///
/// let nested = br#"{"user":{"name":"a","age":2},"x":1}"#;
/// let plan = ProjectPlan::from_paths(&["user.name"]).unwrap();
/// assert_eq!(project(nested, &plan).unwrap(), br#"{"user":{"name":"a"}}"#);
/// ```
pub fn project(json: &[u8], plan: &ProjectPlan) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    project_into(json, plan, &mut out)?;
    Ok(out)
}

/// Project into an existing buffer (appends). Clears nothing; call
/// `out.clear()` first if reusing.
pub fn project_into(json: &[u8], plan: &ProjectPlan, out: &mut Vec<u8>) -> Result<(), Error> {
    let start = skip_whitespace(json, 0);
    if start >= json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos: start,
            msg: "Unexpected EOF",
        });
    }
    let end = skip_value(json, start)?;
    let mut ctx = EmitCtx {
        plan,
        depth: 0,
    };
    emit_value(json, start, end, &plan.root, &mut ctx, out)
}

/// Convenience: [`ProjectPlan::from_paths`] then [`project`].
pub fn project_paths(json: &[u8], paths: &[&str]) -> Result<Vec<u8>, Error> {
    let plan = ProjectPlan::from_paths(paths)?;
    project(json, &plan)
}

/// Project to any [`Write`] (stream-friendly). Same bytes as [`project_into`].
pub fn project_write<W: Write>(json: &[u8], plan: &ProjectPlan, mut w: W) -> Result<(), Error> {
    let buf = project(json, plan)?;
    w.write_all(&buf).map_err(|_| Error::InvalidJsonSyntax {
        pos: 0,
        msg: "I/O error while writing projected JSON",
    })?;
    Ok(())
}

// ─── Emitter ─────────────────────────────────────────────────────────────────

struct EmitCtx<'a> {
    plan: &'a ProjectPlan,
    depth: usize,
}

fn emit_value(
    json: &[u8],
    start: usize,
    end: usize,
    expr: &SelectExpr,
    ctx: &mut EmitCtx<'_>,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    match expr {
        SelectExpr::Identity => {
            out.extend_from_slice(&json[start..end]);
            Ok(())
        }
        SelectExpr::Object(sel) => emit_object(json, start, end, sel, ctx, out),
        SelectExpr::Array(sel) => emit_array(json, start, end, sel, ctx, out),
    }
}

struct Member<'a> {
    key_on_wire: &'a [u8],
    /// Inclusive span of `"key"` including quotes.
    key_span: (usize, usize),
    /// Byte index of `:`.
    colon: usize,
    val_start: usize,
    val_end: usize,
    /// Start of this member's key quote (for preserve).
    member_start: usize,
}

fn emit_object(
    json: &[u8],
    start: usize,
    end: usize,
    sel: &ObjectSelect,
    ctx: &mut EmitCtx<'_>,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    let start = skip_whitespace(json, start);
    if start >= json.len() || json[start] != b'{' {
        return Err(Error::TypeMismatch {
            expected: "object",
            found: type_name_at(json, start),
        });
    }

    // Collect members in document order.
    let mut members: Vec<Member<'_>> = Vec::new();
    let mut pos = start + 1;
    loop {
        pos = skip_whitespace(json, pos);
        if pos >= end {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unclosed object in project",
            });
        }
        if json[pos] == b'}' {
            break;
        }
        if json[pos] != b'"' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected object key in project",
            });
        }
        let key_open = pos;
        let key_inner_start = pos + 1;
        let key_inner_end = find_string_end(json, key_inner_start)?;
        let key_close = key_inner_end; // index of closing quote
        let key_on_wire = &json[key_inner_start..key_inner_end];

        pos = key_close + 1;
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

        members.push(Member {
            key_on_wire,
            key_span: (key_open, key_close + 1),
            colon,
            val_start,
            val_end,
            member_start: key_open,
        });

        pos = skip_whitespace(json, val_end);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF in project object",
            });
        }
        if json[pos] == b',' {
            pos += 1;
        } else if json[pos] == b'}' {
            // loop will see }
        } else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ',' or '}' in project object",
            });
        }
    }

    // Which selected fields appear?
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
            let found = members.iter().any(|m| m.key_on_wire == key.as_bytes());
            if !found {
                return Err(Error::PathNotFound);
            }
        }
    }

    emit_open_object(ctx, out);
    // PreserveSource: keep whitespace between `{` and first kept key when that key
    // is the first member (no dropped content in between).
    if matches!(ctx.plan.style, ProjectStyle::PreserveSource)
        && let Some((first_m, _)) = kept.first()
        && members
            .first()
            .is_some_and(|m| m.member_start == first_m.member_start)
    {
        let between = &json[start + 1..first_m.member_start];
        if between
            .iter()
            .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
        {
            out.extend_from_slice(between);
        }
    }
    for (i, (m, child)) in kept.iter().enumerate() {
        if i > 0 {
            emit_member_sep(ctx, out);
        }
        emit_object_key(json, m, ctx, out)?;
        emit_colon(json, m, ctx, out);
        ctx.depth += 1;
        emit_value(json, m.val_start, m.val_end, child, ctx, out)?;
        ctx.depth -= 1;
    }
    emit_close_object(ctx, out);
    let _ = end;
    Ok(())
}

fn emit_array(
    json: &[u8],
    start: usize,
    end: usize,
    sel: &ArraySelect,
    ctx: &mut EmitCtx<'_>,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    let start = skip_whitespace(json, start);
    if start >= json.len() || json[start] != b'[' {
        return Err(Error::TypeMismatch {
            expected: "array",
            found: type_name_at(json, start),
        });
    }

    // Collect element spans.
    let mut elems: Vec<(usize, usize)> = Vec::new();
    let mut pos = start + 1;
    pos = skip_whitespace(json, pos);
    if pos < end && json[pos] == b']' {
        // empty
    } else {
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
                // done next iter
            } else {
                return Err(Error::InvalidJsonSyntax {
                    pos,
                    msg: "Expected ',' or ']' in project array",
                });
            }
        }
    }

    let mut kept: Vec<(usize, usize, &SelectExpr)> = Vec::new();
    match sel {
        ArraySelect::Each(child) => {
            for &(s, e) in &elems {
                kept.push((s, e, child));
            }
        }
        ArraySelect::Indices(map) => {
            let mut idxs: Vec<usize> = map.keys().copied().collect();
            idxs.sort_unstable();
            for i in idxs {
                match elems.get(i) {
                    Some(&(s, e)) => {
                        let child = map.get(&i).expect("key from map");
                        kept.push((s, e, child));
                    }
                    None => {
                        if ctx.plan.missing == MissingPolicy::Error {
                            return Err(Error::PathNotFound);
                        }
                    }
                }
            }
        }
    }

    emit_open_array(ctx, out);
    for (i, (s, e, child)) in kept.iter().enumerate() {
        if i > 0 {
            emit_member_sep(ctx, out);
        }
        maybe_pretty_indent(ctx, out);
        ctx.depth += 1;
        emit_value(json, *s, *e, child, ctx, out)?;
        ctx.depth -= 1;
    }
    emit_close_array(ctx, out);
    let _ = end;
    Ok(())
}

fn emit_open_object(ctx: &EmitCtx<'_>, out: &mut Vec<u8>) {
    out.push(b'{');
    if matches!(ctx.plan.style, ProjectStyle::Pretty { .. }) {
        // newline after { when non-empty handled by keys; empty stays {}
    }
}

fn emit_close_object(ctx: &EmitCtx<'_>, out: &mut Vec<u8>) {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { indent: n } if n > 0)
        && out.last() != Some(&b'{')
    {
        out.push(b'\n');
        write_indent(ctx.depth, pretty_indent(ctx), out);
    }
    out.push(b'}');
}

fn emit_open_array(ctx: &EmitCtx<'_>, out: &mut Vec<u8>) {
    out.push(b'[');
    let _ = ctx;
}

fn emit_close_array(ctx: &EmitCtx<'_>, out: &mut Vec<u8>) {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { indent: n } if n > 0)
        && out.last() != Some(&b'[')
    {
        out.push(b'\n');
        write_indent(ctx.depth, pretty_indent(ctx), out);
    }
    out.push(b']');
}

fn emit_member_sep(ctx: &EmitCtx<'_>, out: &mut Vec<u8>) {
    // Newlines for Pretty are emitted with each key (see emit_object_key).
    let _ = ctx;
    out.push(b',');
}

fn emit_object_key(
    json: &[u8],
    m: &Member<'_>,
    ctx: &EmitCtx<'_>,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    match ctx.plan.style {
        ProjectStyle::Pretty { .. } => {
            out.push(b'\n');
            write_indent(ctx.depth + 1, pretty_indent(ctx), out);
            out.extend_from_slice(&json[m.key_span.0..m.key_span.1]);
        }
        ProjectStyle::PreserveSource | ProjectStyle::Compact => {
            // Always copy key bytes from source (escaped form intact).
            out.extend_from_slice(&json[m.key_span.0..m.key_span.1]);
        }
    }
    let _ = m.member_start;
    Ok(())
}

fn emit_colon(json: &[u8], m: &Member<'_>, ctx: &EmitCtx<'_>, out: &mut Vec<u8>) {
    match ctx.plan.style {
        ProjectStyle::Compact => {
            out.push(b':');
        }
        ProjectStyle::PreserveSource => {
            // whitespace between key close and colon, colon, whitespace before value
            let after_key = m.key_span.1;
            out.extend_from_slice(&json[after_key..m.colon]);
            out.push(b':');
            out.extend_from_slice(&json[m.colon + 1..m.val_start]);
        }
        ProjectStyle::Pretty { .. } => {
            out.extend_from_slice(b": ");
        }
    }
}

fn maybe_pretty_indent(ctx: &EmitCtx<'_>, out: &mut Vec<u8>) {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { .. }) {
        out.push(b'\n');
        write_indent(ctx.depth + 1, pretty_indent(ctx), out);
    }
}

fn pretty_indent(ctx: &EmitCtx<'_>) -> usize {
    match ctx.plan.style {
        ProjectStyle::Pretty { indent } => indent as usize,
        _ => 0,
    }
}

fn write_indent(depth: usize, per_level: usize, out: &mut Vec<u8>) {
    let n = depth.saturating_mul(per_level);
    out.resize(out.len() + n, b' ');
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

// ─── Estimates (planning) ────────────────────────────────────────────────────

/// **Ballpark** byte length of a minimal object keeping only `paths`.
///
/// Prefer [`project`] / [`project_paths`] for real output. This remains a cheap
/// capacity hint (flat-leaf model).
pub fn estimate_projected_len(json: &[u8], paths: &[&str]) -> Result<usize, Error> {
    if paths.is_empty() {
        return Ok(2); // {}
    }

    let mut total = 1usize; // '{'
    for (i, p) in paths.iter().enumerate() {
        if i > 0 {
            total += 1; // ','
        }
        let segs = parse_path(p);
        let val = find_value(json, &segs)?;
        total += key_wire_len(&segs);
        total += 1; // ':'
        total += val.len();
    }
    total += 1; // '}'
    Ok(total)
}

/// Sum of on-wire value lengths for `paths` (no object framing).
pub fn estimate_values_len(json: &[u8], paths: &[&str]) -> Result<usize, Error> {
    let mut total = 0usize;
    for p in paths {
        let segs = parse_path(p);
        total += find_value(json, &segs)?.len();
    }
    Ok(total)
}

/// Exact size of a projection (runs the projector into a sink that counts).
pub fn projected_len(json: &[u8], plan: &ProjectPlan) -> Result<usize, Error> {
    project(json, plan).map(|v| v.len())
}

fn key_wire_len(segs: &[PathSegment<'_>]) -> usize {
    for seg in segs.iter().rev() {
        if let PathSegment::Key(k) = seg {
            return 2 + k.len();
        }
    }
    2
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_top_level_keys() {
        let json = br#"{"id":1,"title":"x","blob":true}"#;
        let out = project_paths(json, &["id", "title"]).unwrap();
        assert_eq!(out, br#"{"id":1,"title":"x"}"#);
    }

    #[test]
    fn project_preserves_document_key_order() {
        let json = br#"{"z":1,"a":2,"m":3}"#;
        let out = project_paths(json, &["m", "z"]).unwrap();
        // document order: z then m (a dropped)
        assert_eq!(out, br#"{"z":1,"m":3}"#);
    }

    #[test]
    fn project_nested() {
        let json = br#"{"user":{"name":"a","age":9},"x":0}"#;
        let out = project_paths(json, &["user.name"]).unwrap();
        assert_eq!(out, br#"{"user":{"name":"a"}}"#);
    }

    #[test]
    fn project_merge_sibling_fields() {
        let json = br#"{"user":{"name":"a","age":9,"city":"z"},"x":0}"#;
        let out = project_paths(json, &["user.name", "user.age"]).unwrap();
        assert_eq!(out, br#"{"user":{"name":"a","age":9}}"#);
    }

    #[test]
    fn project_array_index() {
        let json = br#"{"products":[{"id":1,"t":"a"},{"id":2,"t":"b"}]}"#;
        let out = project_paths(json, &["products[0].id"]).unwrap();
        assert_eq!(out, br#"{"products":[{"id":1}]}"#);
    }

    #[test]
    fn project_array_wildcard() {
        let json = br#"{"products":[{"id":1,"t":"a"},{"id":2,"t":"b"}]}"#;
        let out = project_paths(json, &["products[].id"]).unwrap();
        assert_eq!(out, br#"{"products":[{"id":1},{"id":2}]}"#);
    }

    #[test]
    fn project_array_star_wildcard() {
        let json = br#"{"items":[{"x":1},{"x":2}]}"#;
        let out = project_paths(json, &["items[*].x"]).unwrap();
        assert_eq!(out, br#"{"items":[{"x":1},{"x":2}]}"#);
    }

    #[test]
    fn project_skip_missing() {
        let json = br#"{"a":1}"#;
        let out = project_paths(json, &["a", "b"]).unwrap();
        assert_eq!(out, br#"{"a":1}"#);
    }

    #[test]
    fn project_error_missing() {
        let plan = ProjectPlan::from_paths(&["a", "b"])
            .unwrap()
            .missing_policy(MissingPolicy::Error);
        let err = project(br#"{"a":1}"#, &plan).unwrap_err();
        assert_eq!(err, Error::PathNotFound);
    }

    #[test]
    fn project_preserve_source_colon_spacing() {
        let json = br#"{ "id" : 1 , "drop": 2 }"#;
        let plan = ProjectPlan::from_paths(&["id"])
            .unwrap()
            .style(ProjectStyle::PreserveSource);
        let out = project(json, &plan).unwrap();
        assert_eq!(out, br#"{ "id" : 1}"#);
    }

    #[test]
    fn project_pretty() {
        let json = br#"{"a":1,"b":2}"#;
        let plan = ProjectPlan::from_paths(&["a", "b"])
            .unwrap()
            .style(ProjectStyle::Pretty { indent: 2 });
        let out = project(json, &plan).unwrap();
        assert_eq!(
            std::str::from_utf8(&out).unwrap(),
            "{\n  \"a\": 1,\n  \"b\": 2\n}"
        );
    }

    #[test]
    fn project_identity() {
        let json = br#"{"a":1}"#;
        let out = project(json, &ProjectPlan::identity()).unwrap();
        assert_eq!(out, json);
    }

    #[test]
    fn project_empty_paths() {
        let out = project_paths(br#"{"a":1}"#, &[]).unwrap();
        assert_eq!(out, br#"{}"#);
    }

    #[test]
    fn project_copies_raw_string_escapes() {
        let json = br#"{"s":"a\"b","n":1}"#;
        let out = project_paths(json, &["s"]).unwrap();
        assert_eq!(out, br#"{"s":"a\"b"}"#);
    }

    #[test]
    fn parse_project_path_wildcard() {
        let p = parse_project_path("a[].b").unwrap();
        assert_eq!(
            p,
            vec![
                ProjectPathSegment::Key("a".into()),
                ProjectPathSegment::ArrayWildcard,
                ProjectPathSegment::Key("b".into()),
            ]
        );
    }

    #[test]
    fn projected_len_matches_project() {
        let json = br#"{"a":1,"b":2,"c":3}"#;
        let plan = ProjectPlan::from_paths(&["a", "c"]).unwrap();
        assert_eq!(projected_len(json, &plan).unwrap(), project(json, &plan).unwrap().len());
    }

    #[test]
    fn serde_accepts_projection() {
        let json = br#"{"id":7,"title":"Hat","images":[1,2,3],"meta":{"x":1}}"#;
        let out = project_paths(json, &["id", "title", "meta.x"]).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["id"], 7);
        assert_eq!(v["title"], "Hat");
        assert_eq!(v["meta"]["x"], 1);
        assert!(v.get("images").is_none());
    }
}
