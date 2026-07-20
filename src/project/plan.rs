//! [`ProjectPlan`] and formatting / missing policies.

use crate::error::Error;
use crate::project::jmespath::{parse_jmespath_expr, parse_project_path};
use crate::project::select::{ArraySelect, ObjectSelect, ProjectPathSegment, SelectExpr};

/// How projected structure is formatted.
///
/// # Compact bulk path
///
/// [`ProjectStyle::Compact`] is the default and the production bulk style for thin
/// cards and list projections. The emitter treats Compact as a **specialized path**:
/// no indent-depth counters, no pretty newline/indent helpers, container close is a
/// single `]` / `}` byte, multi-select hashes write `{k:v,...}` without per-field
/// style branches. Prefer Compact for multi-megabyte rewrites; use
/// [`ProjectStyle::Pretty`] only when human-readable output is required (more bytes
/// and full indent bookkeeping). Input scan usually dominates wall time either way.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ProjectStyle {
    /// Minimal separators: `{"a":1,"b":2}`. Default bulk path (see enum docs).
    #[default]
    Compact,
    /// Prefer source spacing around kept keys, colons, commas, and braces when
    /// subset-projecting objects (and identity leaves).
    PreserveSource,
    /// Multi-line pretty JSON with `indent` spaces per level.
    Pretty {
        /// Spaces per nesting level (prefer ≥ 2).
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

/// Compiled projection plan: selection AST + formatting + missing policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectPlan {
    pub(crate) root: SelectExpr,
    pub(crate) style: ProjectStyle,
    pub(crate) missing: MissingPolicy,
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
    /// Keep the whole document.
    pub fn identity() -> Self {
        Self::default()
    }

    /// Build from path keep-list strings (`id`, `products[].sku`, `a[0:2].b`).
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

    /// Parse a JMESPath **subset** expression as the plan root.
    ///
    /// ```
    /// use jshift::{project, ProjectPlan};
    ///
    /// let json = br#"{"products":[{"id":1,"t":"a"},{"id":2,"t":"b"}]}"#;
    /// let plan = ProjectPlan::from_jmespath(
    ///     "products[*].{id: id, title: t}",
    /// ).unwrap();
    /// // JMESPath result is the projected array (not re-wrapped under products).
    /// let out = project(json, &plan).unwrap();
    /// assert_eq!(out, br#"[{"id":1,"title":"a"},{"id":2,"title":"b"}]"#);
    /// ```
    pub fn from_jmespath(expr: &str) -> Result<Self, Error> {
        Ok(Self {
            root: parse_jmespath_expr(expr)?,
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

    /// Dot-path prefixes of arrays this plan will walk (for [`crate::IndexedDocument`] side-tables).
    ///
    /// Examples:
    /// * keep-list `products[0].title` → `["products"]`
    /// * JMES `products[*].{id:id}` → `["products"]`
    /// * nested `a.b[].c` → `["a.b"]` when representable as field chain + array
    ///
    /// Used by [`crate::IndexedDocument::index_for_plan`] / [`crate::project_auto_indexed`].
    pub fn array_paths_for_index(&self) -> Vec<String> {
        let mut out = Vec::new();
        collect_array_index_paths(&self.root, "", &mut out);
        out.sort();
        out.dedup();
        out
    }
}

/// Walk the selection AST and record field-chain prefixes of array projections.
fn collect_array_index_paths(expr: &SelectExpr, prefix: &str, out: &mut Vec<String>) {
    match expr {
        SelectExpr::Object(obj) => {
            for (k, child) in &obj.fields {
                let p = join_path(prefix, k);
                collect_array_index_paths(child, &p, out);
            }
        }
        SelectExpr::Array(sel) => {
            if !prefix.is_empty() {
                out.push(prefix.to_string());
            }
            match sel {
                ArraySelect::Each(each) | ArraySelect::Slice { each, .. } => {
                    collect_array_index_paths(each, prefix, out);
                }
                ArraySelect::Filter { pred, each } => {
                    collect_array_index_paths(pred, prefix, out);
                    collect_array_index_paths(each, prefix, out);
                }
                ArraySelect::Indices(map) => {
                    for child in map.values() {
                        collect_array_index_paths(child, prefix, out);
                    }
                }
            }
        }
        SelectExpr::Sub(left, right) | SelectExpr::Pipe(left, right) => {
            // Field / field chain on the left, array (or more) on the right.
            if let Some(p) = field_chain_prefix(left, prefix) {
                collect_array_index_paths(right, &p, out);
            } else {
                collect_array_index_paths(left, prefix, out);
                collect_array_index_paths(right, prefix, out);
            }
        }
        SelectExpr::Flatten(inner)
        | SelectExpr::Paren(inner)
        | SelectExpr::Not(inner)
        | SelectExpr::Expref(inner)
        | SelectExpr::ObjectProjection(inner) => {
            collect_array_index_paths(inner, prefix, out);
        }
        SelectExpr::MultiSelectHash(fields) => {
            for f in fields {
                collect_array_index_paths(&f.expr, prefix, out);
            }
        }
        SelectExpr::MultiSelectList(items) => {
            for it in items {
                collect_array_index_paths(it, prefix, out);
            }
        }
        SelectExpr::Cmp { left, right, .. }
        | SelectExpr::And(left, right)
        | SelectExpr::Or(left, right) => {
            collect_array_index_paths(left, prefix, out);
            collect_array_index_paths(right, prefix, out);
        }
        SelectExpr::Call { args, .. } => {
            for a in args {
                collect_array_index_paths(a, prefix, out);
            }
        }
        SelectExpr::Identity
        | SelectExpr::Current
        | SelectExpr::Field(_)
        | SelectExpr::FieldQuoted(_)
        | SelectExpr::Literal(_) => {}
    }
}

fn join_path(prefix: &str, key: &str) -> String {
    if prefix.is_empty() {
        key.to_string()
    } else {
        format!("{prefix}.{key}")
    }
}

/// If `expr` is a pure field / quoted-field chain (optionally under `prefix`), return
/// the dotted path to that focus.
fn field_chain_prefix(expr: &SelectExpr, prefix: &str) -> Option<String> {
    match expr {
        SelectExpr::Field(k) | SelectExpr::FieldQuoted(k) => Some(join_path(prefix, k)),
        SelectExpr::Paren(inner) => field_chain_prefix(inner, prefix),
        SelectExpr::Sub(left, right) | SelectExpr::Pipe(left, right) => {
            let p = field_chain_prefix(left, prefix)?;
            field_chain_prefix(right, &p)
        }
        SelectExpr::Identity | SelectExpr::Current => {
            if prefix.is_empty() {
                None
            } else {
                Some(prefix.to_string())
            }
        }
        _ => None,
    }
}

pub(crate) fn merge_segments(
    node: &mut SelectExpr,
    segs: &[ProjectPathSegment],
) -> Result<(), Error> {
    if segs.is_empty() {
        *node = SelectExpr::Identity;
        return Ok(());
    }
    if matches!(node, SelectExpr::Identity | SelectExpr::Current) {
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
                ArraySelect::Each(each)
                | ArraySelect::Slice { each, .. }
                | ArraySelect::Filter { each, .. } => {
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
            match node {
                SelectExpr::Array(ArraySelect::Each(each))
                | SelectExpr::Array(ArraySelect::Slice { each, .. })
                | SelectExpr::Array(ArraySelect::Filter { each, .. }) => {
                    if segs.len() == 1 {
                        **each = SelectExpr::Identity;
                    } else {
                        merge_segments(each, &segs[1..])?;
                    }
                    Ok(())
                }
                _ => unreachable!(),
            }
        }
        ProjectPathSegment::ArraySlice { start, end, step } => {
            match node {
                SelectExpr::Array(ArraySelect::Slice {
                    start: s,
                    end: e,
                    step: st,
                    each,
                }) if s == start && e == end && st == step => {
                    if segs.len() == 1 {
                        **each = SelectExpr::Identity;
                    } else {
                        merge_segments(each, &segs[1..])?;
                    }
                    Ok(())
                }
                _ => {
                    let mut each = SelectExpr::Object(ObjectSelect::new());
                    if segs.len() == 1 {
                        each = SelectExpr::Identity;
                    } else {
                        merge_segments(&mut each, &segs[1..])?;
                    }
                    *node = SelectExpr::Array(ArraySelect::Slice {
                        start: *start,
                        end: *end,
                        step: *step,
                        each: Box::new(each),
                    });
                    Ok(())
                }
            }
        }
    }
}

fn ensure_array_indices(node: &mut SelectExpr) -> Result<(), Error> {
    match node {
        SelectExpr::Array(ArraySelect::Indices(_))
        | SelectExpr::Array(ArraySelect::Each(_))
        | SelectExpr::Array(ArraySelect::Slice { .. })
        | SelectExpr::Array(ArraySelect::Filter { .. }) => Ok(()),
        SelectExpr::Identity | SelectExpr::Current => {
            *node = SelectExpr::Array(ArraySelect::Indices(Default::default()));
            Ok(())
        }
        SelectExpr::Object(obj) if obj.is_empty() => {
            *node = SelectExpr::Array(ArraySelect::Indices(Default::default()));
            Ok(())
        }
        SelectExpr::Object(_) => Err(Error::InvalidPath {
            msg: "Project path conflict: array index under object selection",
        }),
        _ => {
            *node = SelectExpr::Array(ArraySelect::Indices(Default::default()));
            Ok(())
        }
    }
}

fn ensure_array_each(node: &mut SelectExpr) -> Result<(), Error> {
    match node {
        SelectExpr::Array(ArraySelect::Each(_)) => Ok(()),
        SelectExpr::Array(ArraySelect::Filter { each, .. }) => {
            let e = each.clone();
            *node = SelectExpr::Array(ArraySelect::Each(e));
            Ok(())
        }
        SelectExpr::Array(ArraySelect::Slice { each, .. }) => {
            let e = each.clone();
            *node = SelectExpr::Array(ArraySelect::Each(e));
            Ok(())
        }
        SelectExpr::Array(ArraySelect::Indices(map)) => {
            let mut each = SelectExpr::Object(ObjectSelect::new());
            let old = std::mem::take(map);
            for (_, child) in old {
                merge_expr_union(&mut each, child)?;
            }
            *node = SelectExpr::Array(ArraySelect::Each(Box::new(each)));
            Ok(())
        }
        SelectExpr::Identity | SelectExpr::Current => {
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
        _ => {
            *node = SelectExpr::Array(ArraySelect::Each(Box::new(SelectExpr::Object(
                ObjectSelect::new(),
            ))));
            Ok(())
        }
    }
}

fn merge_expr_union(into: &mut SelectExpr, other: SelectExpr) -> Result<(), Error> {
    if matches!(into, SelectExpr::Identity | SelectExpr::Current)
        || matches!(other, SelectExpr::Identity | SelectExpr::Current)
    {
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
            if matches!(dst, SelectExpr::Object(o) if o.is_empty()) {
                *dst = src;
            }
            Ok(())
        }
    }
}
