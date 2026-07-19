use crate::convert::{escape_json_key, write_json_string};
use crate::error::Error;
use crate::path::PathSegment;
use crate::scan::{
    find_object_member_offsets, find_value_offsets, scan_backwards_whitespace, skip_value,
    skip_whitespace,
};

/// Mutates a JSON value in-place inside a `Vec<u8>` buffer by its path.
///
/// If the new value's length is different from the old value's length,
/// this function shifts the remaining part of the JSON buffer using an
/// optimized, safe slice rotation.
///
/// Returns `Ok(())` on success, or `Err(Error)` on failure.
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
    // 1. Locate the value offsets
    let (start, end) = find_value_offsets(json, path)?;

    let old_len = end - start;
    let new_len = new_value.len();

    if old_len == new_len {
        // Simple case: same length, just overwrite the slice
        json[start..end].copy_from_slice(new_value);
    } else {
        let old_total_len = json.len();

        if new_len > old_len {
            let delta = new_len - old_len;
            json.resize(old_total_len + delta, 0);

            // Shift the tail to the right using safe slice rotation
            let tail_slice = &mut json[end..];
            tail_slice.rotate_right(delta);
        } else {
            let delta = old_len - new_len;

            // Shift the tail to the left using safe slice rotation
            let tail_slice = &mut json[start + new_len..];
            tail_slice.rotate_left(delta);

            // Shrink the vector to remove trailing garbage
            json.truncate(old_total_len - delta);
        }

        // Write the new value into the gap
        json[start..start + new_len].copy_from_slice(new_value);
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
    let (start, end) = find_value_offsets(json, path)?;

    if json[start] != b'[' {
        return Err(Error::TypeMismatch {
            expected: "array",
            found: "primitive/object",
        });
    }

    let insertion_point = end - 1;
    let is_empty = is_array_empty(json, start, end)?;
    let old_total_len = json.len();

    let delta = if is_empty {
        new_element.len()
    } else {
        1 + new_element.len()
    };

    json.resize(old_total_len + delta, 0);

    // Shift the closing bracket and everything after it to the right
    let tail_slice = &mut json[insertion_point..];
    tail_slice.rotate_right(delta);

    // Write the new element (and comma if not empty)
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
    let (start, _end) = find_value_offsets(json, path)?;
    if json[start] != b'[' {
        return Err(Error::TypeMismatch {
            expected: "array",
            found: "primitive/object",
        });
    }

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

    let mut count = 1;
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
            count += 1;
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
    // Match the escaped on-wire key form so logical keys with `"`, `\`, etc. update
    // correctly instead of inserting duplicates.
    let escaped_key = escape_json_key(key);

    match find_object_member_offsets(json, path, escaped_key.as_bytes()) {
        Ok((_key_start, val_start, val_end)) => {
            // Key exists: splice the new value over the old value span.
            let old_len = val_end - val_start;
            let new_len = new_value.len();
            if old_len == new_len {
                json[val_start..val_end].copy_from_slice(new_value);
            } else {
                let old_total_len = json.len();
                if new_len > old_len {
                    let delta = new_len - old_len;
                    json.resize(old_total_len + delta, 0);
                    json[val_end..].rotate_right(delta);
                } else {
                    let delta = old_len - new_len;
                    json[val_start + new_len..].rotate_left(delta);
                    json.truncate(old_total_len - delta);
                }
                json[val_start..val_start + new_len].copy_from_slice(new_value);
            }
            return Ok(());
        }
        Err(Error::PathNotFound) => {
            // Insert below.
        }
        Err(e) => return Err(e),
    }

    let (start, end) = find_value_offsets(json, path)?;
    if json[start] != b'{' {
        return Err(Error::TypeMismatch {
            expected: "object",
            found: "primitive/array",
        });
    }

    let insertion_point = end - 1;
    let is_empty = is_object_empty(json, start, end)?;
    let old_total_len = json.len();

    let mut insertion_content = Vec::new();
    if !is_empty {
        insertion_content.push(b',');
    }
    write_json_string(&mut insertion_content, key);
    insertion_content.push(b':');
    insertion_content.extend_from_slice(new_value);

    let delta = insertion_content.len();
    json.resize(old_total_len + delta, 0);

    let tail_slice = &mut json[insertion_point..];
    tail_slice.rotate_right(delta);

    json[insertion_point..insertion_point + delta].copy_from_slice(&insertion_content);

    Ok(())
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

    if prev_comma_pos > 0 && json[prev_comma_pos - 1] == b',' {
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

    let delta = delete_end - delete_start;
    let old_total_len = json.len();

    let tail_slice = &mut json[delete_start..];
    tail_slice.rotate_left(delta);
    json.truncate(old_total_len - delta);

    Ok(())
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

    let mut prev_comma_pos = val_start;
    prev_comma_pos = scan_backwards_whitespace(json, prev_comma_pos);

    let delete_start;
    let delete_end;

    if prev_comma_pos > 0 && json[prev_comma_pos - 1] == b',' {
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

    let delta = delete_end - delete_start;
    let old_total_len = json.len();

    let tail_slice = &mut json[delete_start..];
    tail_slice.rotate_left(delta);
    json.truncate(old_total_len - delta);

    Ok(())
}
