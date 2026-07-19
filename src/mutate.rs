use crate::convert::{escape_json_key, write_json_string};
use crate::error::Error;
use crate::path::PathSegment;
use crate::scan::{
    byte_at, find_object_member_offsets, find_value_offsets, scan_backwards_whitespace, skip_value,
    skip_whitespace,
};

/// Mutates a JSON value in-place inside a `Vec<u8>` buffer by its path.
///
/// If the new value's length is different from the old value's length,
/// this function shifts the remaining part of the JSON buffer using an
/// optimized, safe slice rotation.
///
/// **Contract:** `new_value` is spliced **raw** — jshift does not validate that it is a
/// well-formed JSON value. Garbage bytes will corrupt the surrounding document. Use
/// [`mutate_value_checked`] when you want a structural sniff of `new_value`, or
/// [`crate::ToJsonBytes`] to build values safely.
///
/// # Examples
/// ```
/// use jshift::{mutate_value, parse_path};
///
/// let mut json = b"{\"status\": \"idle\"}".to_vec();
/// mutate_value(&mut json, &parse_path("status"), b"\"running\"").unwrap();
/// assert_eq!(json, b"{\"status\": \"running\"}".to_vec());
/// ```
pub fn mutate_value(json: &mut Vec<u8>, path: &[PathSegment], new_value: &[u8]) -> Result<(), Error> {
    if new_value.is_empty() {
        return Err(Error::InvalidJsonSyntax {
            pos: 0,
            msg: "Replacement value must not be empty",
        });
    }
    let (start, end) = find_value_offsets(json, path)?;
    validate_span(json, start, end)?;
    splice_range(json, start, end, new_value)
}

/// Like [`mutate_value`], but requires `new_value` to parse as exactly one complete
/// JSON value (object, array, string, number, `true`/`false`/`null`) with no trailing
/// junk after optional whitespace.
///
/// This is a **structural sniff**, not full RFC 8259 validation (e.g. number grammar
/// is not fully checked). It still rejects empty payloads, unclosed containers, and
/// multi-value garbage such as `1 2` or `{}}`.
///
/// # Examples
/// ```
/// use jshift::{mutate_value_checked, parse_path, Error};
///
/// let mut json = b"{\"n\":1}".to_vec();
/// mutate_value_checked(&mut json, &parse_path("n"), b"2").unwrap();
/// assert!(matches!(
///     mutate_value_checked(&mut json, &parse_path("n"), b"1,2"),
///     Err(Error::InvalidJsonSyntax { .. })
/// ));
/// ```
pub fn mutate_value_checked(
    json: &mut Vec<u8>,
    path: &[PathSegment],
    new_value: &[u8],
) -> Result<(), Error> {
    assert_single_json_value(new_value)?;
    mutate_value(json, path, new_value)
}

/// Ensure `bytes` is exactly one complete JSON value (sniff via [`skip_value`]).
pub(crate) fn assert_single_json_value(bytes: &[u8]) -> Result<(), Error> {
    if bytes.is_empty() {
        return Err(Error::InvalidJsonSyntax {
            pos: 0,
            msg: "JSON value must not be empty",
        });
    }
    let end = skip_value(bytes, 0)?;
    let after = skip_whitespace(bytes, end);
    if after != bytes.len() {
        return Err(Error::InvalidJsonSyntax {
            pos: after,
            msg: "Trailing junk after JSON value",
        });
    }
    Ok(())
}

/// Appends a new value to the end of a JSON array located at the specified path.
///
/// If the array is currently empty, the value is written directly. If it contains
/// elements, a separating comma is injected before the new value.
///
/// # Examples
/// ```
/// use jshift::{append_to_array, parse_path};
///
/// let mut json = b"{\"list\": [10, 20]}".to_vec();
/// append_to_array(&mut json, &parse_path("list"), b"30").unwrap();
/// assert_eq!(json, b"{\"list\": [10, 20,30]}".to_vec());
/// ```
pub fn append_to_array(
    json: &mut Vec<u8>,
    path: &[PathSegment],
    new_element: &[u8],
) -> Result<(), Error> {
    if new_element.is_empty() {
        return Err(Error::InvalidJsonSyntax {
            pos: 0,
            msg: "Appended value must not be empty",
        });
    }

    let (start, end) = find_value_offsets(json, path)?;
    require_container(json, start, end, b'[', b']', "array", "primitive/object")?;

    let insertion_point = end - 1;
    let is_empty = is_array_empty(json, start, end)?;

    let delta = if is_empty {
        new_element.len()
    } else {
        new_element
            .len()
            .checked_add(1)
            .ok_or(Error::InvalidJsonSyntax {
                pos: insertion_point,
                msg: "Buffer size overflow",
            })?
    };

    grow_and_shift_right(json, insertion_point, delta)?;

    if is_empty {
        json[insertion_point..insertion_point + new_element.len()].copy_from_slice(new_element);
    } else {
        json[insertion_point] = b',';
        json[insertion_point + 1..insertion_point + 1 + new_element.len()]
            .copy_from_slice(new_element);
    }

    Ok(())
}

fn is_array_empty(json: &[u8], start: usize, end: usize) -> Result<bool, Error> {
    if start + 1 >= end {
        return Err(Error::InvalidJsonSyntax {
            pos: start,
            msg: "Empty or truncated array offsets",
        });
    }
    let mut pos = start + 1;
    while pos < end - 1 {
        match json[pos] {
            b' ' | b'\t' | b'\n' | b'\r' => pos += 1,
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Returns the number of elements in the array located at the specified path.
///
/// # Examples
/// ```
/// use jshift::{array_len, parse_path};
///
/// let json = b"{\"list\": [1, 2, 3]}";
/// assert_eq!(array_len(json, &parse_path("list")).unwrap(), 3);
/// ```
pub fn array_len(json: &[u8], path: &[PathSegment]) -> Result<usize, Error> {
    let (start, end) = find_value_offsets(json, path)?;
    require_container(json, start, end, b'[', b']', "array", "primitive/object")?;

    let mut pos = skip_whitespace(json, start + 1);
    if pos >= json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos,
            msg: "Unexpected EOF",
        });
    }
    if json[pos] == b']' {
        return Ok(0);
    }

    let mut count = 1usize;
    loop {
        pos = skip_value(json, pos)?;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF",
            });
        }
        if json[pos] == b',' {
            count = count.checked_add(1).ok_or(Error::InvalidJsonSyntax {
                pos,
                msg: "Array length overflow",
            })?;
            pos += 1;
        } else if json[pos] == b']' {
            break;
        } else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected comma ',' or closing bracket ']'",
            });
        }
    }
    Ok(count)
}

/// Upserts (inserts or updates) a key-value pair in a JSON object located at the specified path.
///
/// If the key already exists, its value is overwritten. If it does not exist,
/// it is appended to the object with a comma prefix (if the object is not empty).
///
/// # Examples
/// ```
/// use jshift::{upsert_object_key, parse_path};
///
/// let mut json = b"{\"a\": 1}".to_vec();
/// upsert_object_key(&mut json, &[], "b", b"2").unwrap();
/// assert_eq!(json, b"{\"a\": 1,\"b\":2}".to_vec());
/// ```
pub fn upsert_object_key(
    json: &mut Vec<u8>,
    path: &[PathSegment],
    key: &str,
    new_value: &[u8],
) -> Result<(), Error> {
    if new_value.is_empty() {
        return Err(Error::InvalidJsonSyntax {
            pos: 0,
            msg: "Upsert value must not be empty",
        });
    }

    // Match the escaped on-wire key form so logical keys with `"`, `\`, etc. update
    // correctly instead of inserting duplicates.
    let escaped_key = escape_json_key(key);

    match find_object_member_offsets(json, path, escaped_key.as_bytes()) {
        Ok((_key_start, val_start, val_end)) => {
            validate_span(json, val_start, val_end)?;
            return splice_range(json, val_start, val_end, new_value);
        }
        Err(Error::PathNotFound) => {
            // Insert below.
        }
        Err(e) => return Err(e),
    }

    let (start, end) = find_value_offsets(json, path)?;
    require_container(json, start, end, b'{', b'}', "object", "primitive/array")?;

    let insertion_point = end - 1;
    let is_empty = is_object_empty(json, start, end)?;

    let mut insertion_content = Vec::new();
    if !is_empty {
        insertion_content.push(b',');
    }
    write_json_string(&mut insertion_content, key);
    insertion_content.push(b':');
    insertion_content.extend_from_slice(new_value);

    let delta = insertion_content.len();
    grow_and_shift_right(json, insertion_point, delta)?;
    json[insertion_point..insertion_point + delta].copy_from_slice(&insertion_content);

    Ok(())
}

/// Upsert `new_value` at `path`, creating missing **object** parents as `{}`.
///
/// - If the full path already exists, behaves like [`mutate_value`].
/// - Intermediate segments must be object keys ([`PathSegment::Key`]); array indexes
///   are only allowed as the **final** segment (and the parent array must already exist).
/// - Root document must be an object when creating top-level keys (`{}` is fine).
///
/// # Examples
/// ```
/// use jshift::{upsert_at_path, parse_path, find_value};
///
/// let mut json = b"{}".to_vec();
/// upsert_at_path(&mut json, &parse_path("a.b.c"), b"1").unwrap();
/// assert_eq!(find_value(&json, &parse_path("a.b.c")).unwrap(), b"1");
/// ```
pub fn upsert_at_path(
    json: &mut Vec<u8>,
    path: &[PathSegment],
    new_value: &[u8],
) -> Result<(), Error> {
    if path.is_empty() {
        return mutate_value(json, path, new_value);
    }
    if new_value.is_empty() {
        return Err(Error::InvalidJsonSyntax {
            pos: 0,
            msg: "Upsert value must not be empty",
        });
    }

    // Already present → plain mutate.
    if find_value_offsets(json, path).is_ok() {
        return mutate_value(json, path, new_value);
    }

    // Ensure each prefix exists and has the container type required by the next segment.
    for i in 0..path.len() - 1 {
        let prefix = &path[..=i];
        let next = &path[i + 1];
        match find_value_offsets(json, prefix) {
            Ok((start, _end)) => match next {
                PathSegment::Key(_) => {
                    if byte_at(json, start)? != b'{' {
                        return Err(Error::TypeMismatch {
                            expected: "object",
                            found: "primitive/array",
                        });
                    }
                }
                PathSegment::Index(_) => {
                    if byte_at(json, start)? != b'[' {
                        return Err(Error::TypeMismatch {
                            expected: "array",
                            found: "primitive/object",
                        });
                    }
                }
            },
            Err(Error::PathNotFound) => {
                // Only auto-create object parents for a following key.
                let PathSegment::Key(k) = &path[i] else {
                    return Err(Error::InvalidPath {
                        msg: "upsert_at_path cannot create missing array indexes",
                    });
                };
                match next {
                    PathSegment::Key(_) => {
                        upsert_object_key(json, &path[..i], k, b"{}")?;
                    }
                    PathSegment::Index(_) => {
                        return Err(Error::InvalidPath {
                            msg: "upsert_at_path cannot create array parents; only object keys",
                        });
                    }
                }
            }
            Err(e) => return Err(e),
        }
    }

    let parent = &path[..path.len() - 1];
    match &path[path.len() - 1] {
        PathSegment::Key(k) => upsert_object_key(json, parent, k, new_value),
        PathSegment::Index(idx) => {
            // Parent array must already exist; only in-range mutate or append if idx == len.
            let (start, _end) = find_value_offsets(json, parent)?;
            if byte_at(json, start)? != b'[' {
                return Err(Error::TypeMismatch {
                    expected: "array",
                    found: "primitive/object",
                });
            }
            let len = array_len(json, parent)?;
            if *idx < len {
                let mut full = parent.to_vec();
                full.push(PathSegment::Index(*idx));
                mutate_value(json, &full, new_value)
            } else if *idx == len {
                append_to_array(json, parent, new_value)
            } else {
                Err(Error::IndexOutOfBounds { index: *idx })
            }
        }
    }
}

fn is_object_empty(json: &[u8], start: usize, end: usize) -> Result<bool, Error> {
    if start + 1 >= end {
        return Err(Error::InvalidJsonSyntax {
            pos: start,
            msg: "Empty or truncated object offsets",
        });
    }
    let mut pos = start + 1;
    while pos < end - 1 {
        match json[pos] {
            b' ' | b'\t' | b'\n' | b'\r' => pos += 1,
            _ => return Ok(false),
        }
    }
    Ok(true)
}

/// Deletes a key-value pair from a JSON object located at the specified path.
///
/// Automatically adjusts commas surrounding the deleted key-value pair.
///
/// # Examples
/// ```
/// use jshift::{delete_key, parse_path};
///
/// let mut json = b"{\"a\": 1, \"b\": 2}".to_vec();
/// delete_key(&mut json, &[], "b").unwrap();
/// assert_eq!(json, b"{\"a\": 1}".to_vec());
/// ```
pub fn delete_key(json: &mut Vec<u8>, path: &[PathSegment], key: &str) -> Result<(), Error> {
    // Escape the logical key so keys containing `"`, `\`, etc. are found and the
    // recorded opening-quote offset is used (avoids reverse-scan escape bugs).
    let escaped_key = escape_json_key(key);
    let (key_start, _val_start, val_end) =
        find_object_member_offsets(json, path, escaped_key.as_bytes())?;

    // Preceding comma detection
    let mut prev_comma_pos = key_start;
    prev_comma_pos = scan_backwards_whitespace(json, prev_comma_pos);

    let delete_start;
    let delete_end;

    if prev_comma_pos > 0 && byte_at(json, prev_comma_pos - 1)? == b',' {
        delete_start = prev_comma_pos - 1;
        delete_end = val_end;
    } else {
        // Trailing comma detection
        let mut next_comma_pos = val_end;
        next_comma_pos = skip_whitespace(json, next_comma_pos);
        if next_comma_pos < json.len() && json[next_comma_pos] == b',' {
            delete_start = key_start;
            delete_end = next_comma_pos + 1;
        } else {
            delete_start = key_start;
            delete_end = val_end;
        }
    }

    delete_range(json, delete_start, delete_end)
}

/// Deletes an element from a JSON array located at the specified path by its index.
///
/// Automatically adjusts commas surrounding the deleted array element.
///
/// # Examples
/// ```
/// use jshift::{delete_index, parse_path};
///
/// let mut json = b"[10, 20, 30]".to_vec();
/// delete_index(&mut json, &[], 1).unwrap();
/// assert_eq!(json, b"[10, 30]".to_vec());
/// ```
pub fn delete_index(json: &mut Vec<u8>, path: &[PathSegment], index: usize) -> Result<(), Error> {
    let mut target_path = path.to_vec();
    target_path.push(PathSegment::Index(index));

    let (val_start, val_end) = find_value_offsets(json, &target_path)?;
    validate_span(json, val_start, val_end)?;

    let mut prev_comma_pos = val_start;
    prev_comma_pos = scan_backwards_whitespace(json, prev_comma_pos);

    let delete_start;
    let delete_end;

    if prev_comma_pos > 0 && byte_at(json, prev_comma_pos - 1)? == b',' {
        delete_start = prev_comma_pos - 1;
        delete_end = val_end;
    } else {
        let mut next_comma_pos = val_end;
        next_comma_pos = skip_whitespace(json, next_comma_pos);
        if next_comma_pos < json.len() && json[next_comma_pos] == b',' {
            delete_start = val_start;
            delete_end = next_comma_pos + 1;
        } else {
            delete_start = val_start;
            delete_end = val_end;
        }
    }

    delete_range(json, delete_start, delete_end)
}

// --- buffer helpers ---------------------------------------------------------

fn validate_span(json: &[u8], start: usize, end: usize) -> Result<(), Error> {
    if start > end || end > json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos: start,
            msg: "Invalid value span",
        });
    }
    if start == end {
        return Err(Error::InvalidJsonSyntax {
            pos: start,
            msg: "Empty value span",
        });
    }
    Ok(())
}

fn require_container(
    json: &[u8],
    start: usize,
    end: usize,
    open: u8,
    close: u8,
    expected: &'static str,
    found: &'static str,
) -> Result<(), Error> {
    validate_span(json, start, end)?;
    if byte_at(json, start)? != open {
        return Err(Error::TypeMismatch { expected, found });
    }
    if json[end - 1] != close {
        return Err(Error::InvalidJsonSyntax {
            pos: end.saturating_sub(1),
            msg: "Mismatched container delimiters",
        });
    }
    Ok(())
}

/// Replace `json[start..end]` with `new_value`, growing or shrinking as needed.
fn splice_range(
    json: &mut Vec<u8>,
    start: usize,
    end: usize,
    new_value: &[u8],
) -> Result<(), Error> {
    let old_len = end - start;
    let new_len = new_value.len();

    if old_len == new_len {
        json[start..end].copy_from_slice(new_value);
        return Ok(());
    }

    let old_total_len = json.len();
    if new_len > old_len {
        let delta = new_len - old_len;
        grow_and_shift_right(json, end, delta)?;
    } else {
        let delta = old_len - new_len;
        json[start + new_len..].rotate_left(delta);
        json.truncate(old_total_len - delta);
    }
    json[start..start + new_len].copy_from_slice(new_value);
    Ok(())
}

fn grow_and_shift_right(json: &mut Vec<u8>, at: usize, delta: usize) -> Result<(), Error> {
    if delta == 0 {
        return Ok(());
    }
    if at > json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos: at,
            msg: "Insert position out of bounds",
        });
    }
    let new_len = json.len().checked_add(delta).ok_or(Error::InvalidJsonSyntax {
        pos: at,
        msg: "Buffer size overflow",
    })?;
    json.resize(new_len, 0);
    json[at..].rotate_right(delta);
    Ok(())
}

fn delete_range(json: &mut Vec<u8>, delete_start: usize, delete_end: usize) -> Result<(), Error> {
    if delete_start > delete_end || delete_end > json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos: delete_start,
            msg: "Invalid delete span",
        });
    }
    let delta = delete_end - delete_start;
    if delta == 0 {
        return Ok(());
    }
    let old_total_len = json.len();
    json[delete_start..].rotate_left(delta);
    json.truncate(old_total_len - delta);
    Ok(())
}
