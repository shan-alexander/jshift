//! Field projection: keep-list / JMESPath subset → smaller JSON.
//!
//! # Architecture
//!
//! | Layer | Module | Role |
//! | --- | --- | --- |
//! | AST | [`select`] | [`SelectExpr`] (identity, object, array, multi-select, pipe, flatten, …) |
//! | Paths | [`jmespath`] | keep-list paths + JMESPath **subset** parser |
//! | Plan | [`plan`] | [`ProjectPlan`], styles, missing policy |
//! | Emit | [`emit`] | one-pass writer with style fidelity hooks |
//!
//! Future: full JMESPath filters/functions, deeper transforms, richer whitespace
//! all attach as new [`SelectExpr`] arms or style modes without replacing
//! [`project`].

mod emit;
mod jmespath;
mod plan;
mod select;

pub use jmespath::{parse_jmespath_expr, parse_project_path, select_from_project_path};
pub use plan::{MissingPolicy, ProjectPlan, ProjectStyle};
pub use select::{
    ArraySelect, CmpOp, HashField, ObjectSelect, ProjectPathSegment, SelectExpr,
};

use std::io::Write;

use crate::error::Error;
use crate::path::{parse_path, PathSegment};
use crate::scan::{find_value, skip_value, skip_whitespace};

use emit::{emit_value, EmitCtx};

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
    let start = skip_whitespace(json, 0);
    if start >= json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos: start,
            msg: "Unexpected EOF",
        });
    }
    let end = skip_value(json, start)?;
    let mut ctx = EmitCtx { plan, depth: 0 };
    emit_value(json, start, end, plan.select(), &mut ctx, out)
}

/// Convenience: [`ProjectPlan::from_paths`] then [`project`].
pub fn project_paths(json: &[u8], paths: &[&str]) -> Result<Vec<u8>, Error> {
    project(json, &ProjectPlan::from_paths(paths)?)
}

/// Convenience: [`ProjectPlan::from_jmespath`] then [`project`].
pub fn project_jmespath(json: &[u8], expr: &str) -> Result<Vec<u8>, Error> {
    project(json, &ProjectPlan::from_jmespath(expr)?)
}

/// Project to any [`Write`].
pub fn project_write<W: Write>(json: &[u8], plan: &ProjectPlan, mut w: W) -> Result<(), Error> {
    let buf = project(json, plan)?;
    w.write_all(&buf).map_err(|_| Error::InvalidJsonSyntax {
        pos: 0,
        msg: "I/O error while writing projected JSON",
    })?;
    Ok(())
}

/// Exact projected size (runs the projector).
pub fn projected_len(json: &[u8], plan: &ProjectPlan) -> Result<usize, Error> {
    project(json, plan).map(|v| v.len())
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
        let out = project_jmespath(json, "{id: id, source: \"teefury\"}").unwrap();
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
    fn serde_accepts_projection() {
        let json = br#"{"id":7,"title":"Hat","images":[1,2,3],"meta":{"x":1}}"#;
        let out = project_paths(json, &["id", "title", "meta.x"]).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["id"], 7);
        assert!(v.get("images").is_none());
    }
}
