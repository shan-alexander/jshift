//! Explicit transform pipeline (rename / drop / inject / project steps).
//!
//! Complements pure JMESPath with declarative ETL-style steps that compose:
//!
//! ```
//! use jshift::{Transform, TransformPipeline};
//!
//! let json = br#"{"id":1,"title":"Hat","noise":true}"#;
//! let out = TransformPipeline::new()
//!     .then(Transform::KeepPaths(&["id", "title"]))
//!     .then(Transform::Inject {
//!         key: "source",
//!         value: br#""teefury""#,
//!     })
//!     .then(Transform::Rename {
//!         from: "title",
//!         to: "name",
//!     })
//!     .apply(json)
//!     .unwrap();
//! assert!(out.windows(6).any(|w| w == b"\"name\""));
//! assert!(out.windows(8).any(|w| w == b"\"source\""));
//! ```

use crate::error::Error;
use crate::project::plan::{ProjectPlan, ProjectStyle};
use crate::project::project;
use crate::scan::{find_string_end, skip_value, skip_whitespace};

/// One transform step in a pipeline.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum Transform {
    /// Keep-list projection (ancestor keys preserved).
    KeepPaths(&'static [&'static str]),
    /// Owned keep-list (runtime paths).
    KeepPathsOwned(Vec<String>),
    /// JMESPath projection (result shape follows JMESPath).
    Jmes(&'static str),
    /// Owned JMESPath expression.
    JmesOwned(String),
    /// Full [`ProjectPlan`].
    Plan(ProjectPlan),
    /// Rename a top-level object key (after previous steps).
    Rename { from: &'static str, to: &'static str },
    /// Drop a top-level object key.
    Drop(&'static str),
    /// Inject / overwrite a top-level key with raw JSON value bytes (e.g. `br#""api""#`, `b"true"`).
    Inject {
        key: &'static str,
        value: &'static [u8],
    },
    /// Set emit style for subsequent plan-based steps (kept for chaining ergonomics).
    Style(ProjectStyle),
}

/// Ordered list of [`Transform`] steps.
#[derive(Debug, Clone, Default)]
pub struct TransformPipeline {
    steps: Vec<Transform>,
    style: ProjectStyle,
}

impl TransformPipeline {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn then(mut self, step: Transform) -> Self {
        if let Transform::Style(s) = &step {
            self.style = *s;
        }
        self.steps.push(step);
        self
    }

    pub fn push(&mut self, step: Transform) {
        if let Transform::Style(s) = &step {
            self.style = *s;
        }
        self.steps.push(step);
    }

    /// Apply all steps sequentially; each step reads the previous output.
    pub fn apply(&self, json: &[u8]) -> Result<Vec<u8>, Error> {
        let mut cur = json.to_vec();
        for step in &self.steps {
            cur = apply_step(step, &cur, self.style)?;
        }
        Ok(cur)
    }
}

fn apply_step(step: &Transform, json: &[u8], style: ProjectStyle) -> Result<Vec<u8>, Error> {
    match step {
        Transform::Style(_) => Ok(json.to_vec()),
        Transform::KeepPaths(paths) => {
            let plan = ProjectPlan::from_paths(paths)?.style(style);
            project(json, &plan)
        }
        Transform::KeepPathsOwned(paths) => {
            let refs: Vec<&str> = paths.iter().map(|s| s.as_str()).collect();
            let plan = ProjectPlan::from_paths(&refs)?.style(style);
            project(json, &plan)
        }
        Transform::Jmes(expr) => {
            let plan = ProjectPlan::from_jmespath(expr)?.style(style);
            project(json, &plan)
        }
        Transform::JmesOwned(expr) => {
            let plan = ProjectPlan::from_jmespath(expr)?.style(style);
            project(json, &plan)
        }
        Transform::Plan(plan) => project(json, plan),
        Transform::Rename { from, to } => rename_top_level(json, from, to),
        Transform::Drop(key) => drop_top_level(json, key),
        Transform::Inject { key, value } => inject_top_level(json, key, value),
    }
}

fn rename_top_level(json: &[u8], from: &str, to: &str) -> Result<Vec<u8>, Error> {
    rewrite_top_keys(
        json,
        |k| {
            if k == from {
                Some(to.to_string())
            } else {
                Some(k.to_string())
            }
        },
        None,
    )
}

fn drop_top_level(json: &[u8], key: &str) -> Result<Vec<u8>, Error> {
    rewrite_top_keys(
        json,
        |k| {
            if k == key {
                None
            } else {
                Some(k.to_string())
            }
        },
        None,
    )
}

fn inject_top_level(json: &[u8], key: &str, value: &[u8]) -> Result<Vec<u8>, Error> {
    rewrite_top_keys(json, |k| Some(k.to_string()), Some((key, value)))
}

/// Rewrite top-level object keys; `map` returns new key name or None to drop.
/// `inject` adds/overwrites a key with raw JSON value bytes.
fn rewrite_top_keys(
    json: &[u8],
    map: impl Fn(&str) -> Option<String>,
    inject: Option<(&str, &[u8])>,
) -> Result<Vec<u8>, Error> {
    let start = skip_whitespace(json, 0);
    if start >= json.len() || json[start] != b'{' {
        return Err(Error::TypeMismatch {
            expected: "object",
            found: "non-object",
        });
    }
    let mut out = Vec::with_capacity(json.len());
    out.push(b'{');
    let mut pos = start + 1;
    let mut first = true;
    let mut injected = false;
    let inject_key = inject.map(|(k, _)| k);

    loop {
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unclosed object in transform",
            });
        }
        if json[pos] == b'}' {
            break;
        }
        if json[pos] != b'"' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected key in transform",
            });
        }
        let key_open = pos;
        let key_end = find_string_end(json, pos + 1)?;
        let key_wire = &json[pos + 1..key_end];
        let key_str = std::str::from_utf8(key_wire).unwrap_or("");
        pos = key_end + 1;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] != b':' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ':' in transform",
            });
        }
        pos += 1;
        pos = skip_whitespace(json, pos);
        let val_start = pos;
        let val_end = skip_value(json, val_start)?;
        pos = skip_whitespace(json, val_end);
        if pos < json.len() && json[pos] == b',' {
            pos += 1;
        }

        // Inject overwrite: skip original if same key
        if inject_key == Some(key_str) {
            injected = false; // will inject at end with new value; skip old
            continue;
        }

        match map(key_str) {
            None => continue,
            Some(new_key) => {
                if !first {
                    out.push(b',');
                }
                crate::convert::write_json_string(&mut out, &new_key);
                out.push(b':');
                out.extend_from_slice(&json[val_start..val_end]);
                first = false;
                let _ = key_open;
            }
        }
    }

    if let Some((ik, iv)) = inject {
        if !first {
            out.push(b',');
        }
        crate::convert::write_json_string(&mut out, ik);
        out.push(b':');
        out.extend_from_slice(iv);
        let _ = injected;
    }
    out.push(b'}');
    Ok(out)
}
