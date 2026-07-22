//! RFC 7396 JSON Merge Patch — on raw buffers, no `Value` tree.
//!
//! Semantics (RFC 7396):
//! * If the patch is **not** an object → replace the target with the patch.
//! * If the patch **is** an object:
//!   * target becomes `{}` when it is not already an object;
//!   * for each key in the patch:
//!     * value `null` → **delete** that key from the target;
//!     * otherwise → recursive merge into `target[key]`.
//!
//! Differs from [`crate::merge_object_shallow`]: merge patch **deletes** on null
//! and **recurses** into nested objects.
//!
//! ```
//! use jshift::{merge_patch, JsonDoc, TypedDoc};
//!
//! let mut doc = TypedDoc::from_slice(br#"{"a":{"b":1,"c":2},"d":3}"#);
//! merge_patch(doc.as_mut_vec(), br#"{"a":{"b":9,"c":null},"e":4}"#).unwrap();
//! assert_eq!(doc.get::<u64>("a.b").unwrap(), 9);
//! assert!(!doc.contains("a.c").unwrap());
//! assert_eq!(doc.get::<u64>("d").unwrap(), 3);
//! assert_eq!(doc.get::<u64>("e").unwrap(), 4);
//! ```

use crate::error::Error;
use crate::mutate::{delete_key, upsert_at_path, upsert_object_key};
use crate::path::PathSegment;
use crate::scan::{find_value_offsets, skip_value, skip_whitespace};

/// Apply RFC 7396 merge patch to the document root.
#[inline]
pub fn merge_patch(target: &mut Vec<u8>, patch: &[u8]) -> Result<(), Error> {
    merge_patch_at(target, &[], patch)
}

/// Apply merge patch to the value at `path` (empty path = root).
pub fn merge_patch_at(
    target: &mut Vec<u8>,
    path: &[PathSegment],
    patch: &[u8],
) -> Result<(), Error> {
    let patch = trim_value(patch)?;
    if !is_object(patch) {
        // Replace entire target value with patch.
        if path.is_empty() {
            target.clear();
            target.extend_from_slice(patch);
            return Ok(());
        }
        return upsert_at_path(target, path, patch);
    }

    // Patch is object → ensure target is object, then walk members.
    ensure_object_at(target, path)?;

    let entries = collect_patch_entries(patch)?;
    for (key, value) in entries {
        let mut child_path = path.to_vec();
        child_path.push(PathSegment::Key(&key));

        let val = trim_value(&value)?;
        if val == b"null" {
            // RFC: null removes the member (ignore if missing).
            let _ = delete_key(target, path, &key);
            continue;
        }

        if is_object(val) {
            // Recursive merge into target[key] (create {} if missing / non-object).
            if find_value_offsets(target, &child_path).is_err()
                || !value_at_is_object(target, &child_path)?
            {
                upsert_object_key(target, path, &key, b"{}")?;
            }
            // Rebuild child path with owned key for recursion — use path + key via upsert path
            merge_patch_at_owned(target, path, &key, val)?;
        } else {
            upsert_object_key(target, path, &key, val)?;
        }
    }
    Ok(())
}

/// Recurse with owned key string (path segments can't borrow from loop).
fn merge_patch_at_owned(
    target: &mut Vec<u8>,
    parent: &[PathSegment],
    key: &str,
    patch_obj: &[u8],
) -> Result<(), Error> {
    // Build path with Key(key) — need owned path segments for nested recursion
    // using temporary Key borrows is fine within this call.
    let mut segs: Vec<PathSegment<'_>> = parent.to_vec();
    segs.push(PathSegment::Key(key));
    merge_patch_at(target, &segs, patch_obj)
}

fn ensure_object_at(target: &mut Vec<u8>, path: &[PathSegment]) -> Result<(), Error> {
    match find_value_offsets(target, path) {
        Ok((s, _)) if s < target.len() && target[s] == b'{' => Ok(()),
        Ok(_) => {
            // Replace non-object with empty object.
            if path.is_empty() {
                target.clear();
                target.extend_from_slice(b"{}");
                Ok(())
            } else {
                upsert_at_path(target, path, b"{}")
            }
        }
        Err(Error::PathNotFound) if path.is_empty() => {
            target.clear();
            target.extend_from_slice(b"{}");
            Ok(())
        }
        Err(Error::PathNotFound) => upsert_at_path(target, path, b"{}"),
        Err(e) => Err(e),
    }
}

fn value_at_is_object(json: &[u8], path: &[PathSegment]) -> Result<bool, Error> {
    let (s, _) = find_value_offsets(json, path)?;
    Ok(s < json.len() && json[s] == b'{')
}

fn is_object(v: &[u8]) -> bool {
    let s = skip_whitespace(v, 0);
    s < v.len() && v[s] == b'{'
}

fn trim_value(v: &[u8]) -> Result<&[u8], Error> {
    let start = skip_whitespace(v, 0);
    if start >= v.len() {
        return Err(Error::InvalidJsonSyntax {
            pos: start,
            msg: "Empty merge patch value",
        });
    }
    let end = skip_value(v, start)?;
    Ok(&v[start..end])
}

fn collect_patch_entries(patch: &[u8]) -> Result<Vec<(String, Vec<u8>)>, Error> {
    // Reuse shallow merge collector logic (object only).
    // Duplicated lightly to avoid pub(crate) coupling churn.
    use crate::scan::find_string_end;

    let start = skip_whitespace(patch, 0);
    if start >= patch.len() || patch[start] != b'{' {
        return Err(Error::TypeMismatch {
            expected: "object",
            found: "primitive/array",
        });
    }
    let end = skip_value(patch, start)?;
    let mut pos = skip_whitespace(patch, start + 1);
    let mut entries = Vec::new();
    if pos < end && patch[pos] == b'}' {
        return Ok(entries);
    }
    loop {
        pos = skip_whitespace(patch, pos);
        if pos >= end {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF in merge patch",
            });
        }
        if patch[pos] == b'}' {
            break;
        }
        if patch[pos] != b'"' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected key in merge patch",
            });
        }
        let ks = pos + 1;
        let ke = find_string_end(patch, ks)?;
        let key_raw = &patch[ks..ke];
        let key = if !key_raw.contains(&b'\\') {
            std::str::from_utf8(key_raw)
                .map_err(|_| Error::TypeMismatch {
                    expected: "utf-8 key",
                    found: "invalid utf-8",
                })?
                .to_string()
        } else {
            let mut lit = Vec::with_capacity(key_raw.len() + 2);
            lit.push(b'"');
            lit.extend_from_slice(key_raw);
            lit.push(b'"');
            crate::convert::from_json_string(&lit).ok_or(Error::TypeMismatch {
                expected: "string key",
                found: "invalid escape",
            })?
        };
        pos = skip_whitespace(patch, ke + 1);
        if pos >= end || patch[pos] != b':' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected colon in merge patch",
            });
        }
        pos = skip_whitespace(patch, pos + 1);
        let vs = pos;
        let ve = skip_value(patch, vs)?;
        entries.push((key, patch[vs..ve].to_vec()));
        pos = skip_whitespace(patch, ve);
        if pos >= end {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF after merge patch value",
            });
        }
        if patch[pos] == b',' {
            pos += 1;
        } else if patch[pos] == b'}' {
            break;
        } else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected comma or '}' in merge patch",
            });
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::try_parse_path;
    use crate::typed_doc::{JsonDoc, TypedDoc};

    #[test]
    fn rfc7396_examples() {
        // RFC 7396 §3 examples (simplified)
        let mut t = br#"{"a":"b"}"#.to_vec();
        merge_patch(&mut t, br#"{"a":"c"}"#).unwrap();
        assert_eq!(t, br#"{"a":"c"}"#);

        let mut t = br#"{"a":"b"}"#.to_vec();
        merge_patch(&mut t, br#"{"b":"c"}"#).unwrap();
        let doc = TypedDoc::from_vec(t);
        assert_eq!(doc.get_str("a").unwrap(), "b");
        assert_eq!(doc.get_str("b").unwrap(), "c");

        let mut t = br#"{"a":"b"}"#.to_vec();
        merge_patch(&mut t, br#"{"a":null}"#).unwrap();
        assert_eq!(t, br#"{}"#);

        let mut t = br#"{"a":{"b":"c"}}"#.to_vec();
        merge_patch(&mut t, br#"{"a":{"b":"d","c":null}}"#).unwrap();
        let doc = TypedDoc::from_vec(t);
        assert_eq!(doc.get_str("a.b").unwrap(), "d");
        assert!(!doc.contains("a.c").unwrap());
    }

    #[test]
    fn replace_root_with_array() {
        let mut t = br#"{"a":1}"#.to_vec();
        merge_patch(&mut t, br#"[1,2]"#).unwrap();
        assert_eq!(t, br#"[1,2]"#);
    }

    #[test]
    fn path_string_merge() {
        let mut doc = TypedDoc::from_slice(br#"{"user":{"name":"x","age":1}}"#);
        let path = try_parse_path("user").unwrap();
        merge_patch_at(doc.as_mut_vec(), &path, br#"{"age":2,"city":"y"}"#).unwrap();
        assert_eq!(doc.get::<u64>("user.age").unwrap(), 2);
        assert_eq!(doc.get_str("user.name").unwrap(), "x");
        assert_eq!(doc.get_str("user.city").unwrap(), "y");
    }
}
