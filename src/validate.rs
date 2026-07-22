//! Lightweight schema checks on raw bytes — no DOM.
//!
//! Roadmap: open vs closed documents. These helpers type-check **presence** and
//! **unknown keys** without building a value tree.
//!
//! ```
//! use jshift::{require_paths, deny_unknown_keys, Error};
//!
//! let json = br#"{"id":1,"title":"Hat","noise":true}"#;
//! require_paths(json, &["id", "title"]).unwrap();
//! assert!(matches!(
//!     deny_unknown_keys(json, &[], &["id", "title"]),
//!     Err(Error::UnknownField { .. })
//! ));
//! deny_unknown_keys(json, &[], &["id", "title", "noise"]).unwrap();
//! ```

use crate::error::Error;
use crate::path::{try_parse_path, PathSegment};
use crate::scan::{find_value, find_value_offsets};
use crate::typed_doc::{ObjectEntries, Presence, TypedDocRef};
use crate::typed_doc::JsonDoc;

/// Ensure every path exists (any non-missing value, including JSON `null`).
pub fn require_paths(json: &[u8], paths: &[&str]) -> Result<(), Error> {
    let doc = TypedDocRef::from_slice(json);
    for p in paths {
        match doc.presence(p)? {
            Presence::Missing => {
                return Err(Error::MissingField {
                    path: (*p).to_string(),
                });
            }
            Presence::Null | Presence::Value => {}
        }
    }
    Ok(())
}

/// Ensure every path exists and is **not** JSON `null`.
pub fn require_paths_non_null(json: &[u8], paths: &[&str]) -> Result<(), Error> {
    let doc = TypedDocRef::from_slice(json);
    for p in paths {
        match doc.presence(p)? {
            Presence::Missing => {
                return Err(Error::MissingField {
                    path: (*p).to_string(),
                });
            }
            Presence::Null => {
                return Err(Error::Decode {
                    path: (*p).to_string(),
                    expected: "non-null value",
                    found: "null",
                    pos: None,
                });
            }
            Presence::Value => {}
        }
    }
    Ok(())
}

/// Closed-object check: every key on the object at `object_path` must be in
/// `allowed` (logical unescaped keys for unescaped on-wire keys).
///
/// `object_path` empty = document root. Unknown key → [`Error::UnknownField`].
pub fn deny_unknown_keys(
    json: &[u8],
    object_path: &[PathSegment],
    allowed: &[&str],
) -> Result<(), Error> {
    let (start, end) = if object_path.is_empty() {
        let s = crate::scan::skip_whitespace(json, 0);
        if s >= json.len() || json[s] != b'{' {
            return Err(Error::TypeMismatch {
                expected: "object",
                found: "primitive/array",
            });
        }
        let e = crate::scan::skip_value(json, s)?;
        (s, e)
    } else {
        find_value_offsets(json, object_path)?
    };
    if start >= json.len() || json[start] != b'{' {
        return Err(Error::TypeMismatch {
            expected: "object",
            found: "primitive/array",
        });
    }
    let _ = end;

    let span = &json[start..end];
    for ent in ObjectEntries::open(span)? {
        let ent = ent?;
        let key = ent.key_str().map_err(|_| Error::Decode {
            path: String::new(),
            expected: "unescaped object key",
            found: "escaped key",
            pos: Some(start),
        })?;
        if !allowed.iter().any(|&a| a == key) {
            return Err(Error::UnknownField {
                path: if object_path.is_empty() {
                    key.to_string()
                } else {
                    format!("(object).{key}")
                },
            });
        }
    }
    Ok(())
}

/// Like [`deny_unknown_keys`] but `object_path` is a string path (`""` or omit root).
pub fn deny_unknown_keys_at(json: &[u8], object_path: &str, allowed: &[&str]) -> Result<(), Error> {
    if object_path.is_empty() {
        deny_unknown_keys(json, &[], allowed)
    } else {
        let segs = try_parse_path(object_path)?;
        deny_unknown_keys(json, &segs, allowed)
    }
}

/// Open mode: only check that required paths exist; unknowns are ignored.
pub fn validate_open(json: &[u8], required: &[&str]) -> Result<(), Error> {
    require_paths(json, required)
}

/// Closed mode: required paths present **and** no unknown keys on the root object.
pub fn validate_closed(json: &[u8], required_and_allowed: &[&str]) -> Result<(), Error> {
    require_paths(json, required_and_allowed)?;
    deny_unknown_keys(json, &[], required_and_allowed)
}

/// Type-check a path as a JSON string / number / bool / object / array without full decode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JsonType {
    Object,
    Array,
    String,
    Number,
    Bool,
    Null,
}

/// Sniff the JSON type at `path`.
pub fn type_at(json: &[u8], path: &str) -> Result<JsonType, Error> {
    let segs = try_parse_path(path)?;
    let slice = find_value(json, &segs)?;
    let pos = crate::scan::skip_whitespace(slice, 0);
    if pos >= slice.len() {
        return Err(Error::InvalidJsonSyntax {
            pos: 0,
            msg: "Empty value",
        });
    }
    Ok(match slice[pos] {
        b'{' => JsonType::Object,
        b'[' => JsonType::Array,
        b'"' => JsonType::String,
        b't' | b'f' => JsonType::Bool,
        b'n' => JsonType::Null,
        b'-' | b'0'..=b'9' => JsonType::Number,
        _ => {
            return Err(Error::InvalidJsonSyntax {
                pos: 0,
                msg: "Unknown JSON type",
            })
        }
    })
}

/// Require `path` to have type `expected`.
pub fn require_type(json: &[u8], path: &str, expected: JsonType) -> Result<(), Error> {
    let found = type_at(json, path)?;
    if found != expected {
        return Err(Error::Decode {
            path: path.to_string(),
            expected: match expected {
                JsonType::Object => "object",
                JsonType::Array => "array",
                JsonType::String => "string",
                JsonType::Number => "number",
                JsonType::Bool => "bool",
                JsonType::Null => "null",
            },
            found: match found {
                JsonType::Object => "object",
                JsonType::Array => "array",
                JsonType::String => "string",
                JsonType::Number => "number",
                JsonType::Bool => "bool",
                JsonType::Null => "null",
            },
            pos: None,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn require_and_closed() {
        let json = br#"{"id":1,"title":"x"}"#;
        require_paths(json, &["id", "title"]).unwrap();
        validate_closed(json, &["id", "title"]).unwrap();
        assert!(matches!(
            validate_closed(json, &["id"]),
            Err(Error::UnknownField { .. })
        ));
        assert!(matches!(
            require_paths(json, &["id", "missing"]),
            Err(Error::MissingField { .. })
        ));
    }

    #[test]
    fn type_at_path() {
        let json = br#"{"n":1,"s":"a","a":[],"o":{},"b":true,"z":null}"#;
        assert_eq!(type_at(json, "n").unwrap(), JsonType::Number);
        assert_eq!(type_at(json, "s").unwrap(), JsonType::String);
        assert_eq!(type_at(json, "a").unwrap(), JsonType::Array);
        assert_eq!(type_at(json, "o").unwrap(), JsonType::Object);
        assert_eq!(type_at(json, "b").unwrap(), JsonType::Bool);
        assert_eq!(type_at(json, "z").unwrap(), JsonType::Null);
        require_type(json, "n", JsonType::Number).unwrap();
        assert!(require_type(json, "n", JsonType::String).is_err());
    }
}
