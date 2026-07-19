use memchr::memchr;

use crate::error::Error;
use crate::path::PathSegment;

/// Locates a JSON value byte-slice within a raw JSON buffer by its path.
///
/// Returns the slice of the raw JSON value (e.g., `b"123"`, `b"\"hello\""`, or `b"{\"nested\": true}"`).
///
/// # Examples
/// ```
/// use jshift::{find_value, parse_path};
///
/// let json = b"{\"metadata\": {\"author\": \"farmer\"}}";
/// let val = find_value(json, &parse_path("metadata.author")).unwrap();
/// assert_eq!(val, b"\"farmer\"");
/// ```
pub fn find_value<'a>(json: &'a [u8], path: &[PathSegment]) -> Result<&'a [u8], Error> {
    let (start, end) = find_value_offsets(json, path)?;
    Ok(&json[start..end])
}

/// Locate a member of the object at `object_path` whose key content equals `key_raw`
/// (escaped form as stored between quotes).
///
/// Returns `(key_open_quote, value_start, value_end)`.
pub(crate) fn find_object_member_offsets(
    json: &[u8],
    object_path: &[PathSegment],
    key_raw: &[u8],
) -> Result<(usize, usize, usize), Error> {
    let (obj_start, obj_end) = find_value_offsets(json, object_path)?;
    if obj_start >= json.len() || json[obj_start] != b'{' {
        return Err(Error::TypeMismatch {
            expected: "object",
            found: "primitive/array",
        });
    }

    let mut pos = obj_start + 1;
    loop {
        pos = skip_whitespace(json, pos);
        if pos >= obj_end {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF inside object",
            });
        }
        if json[pos] == b'}' {
            return Err(Error::PathNotFound);
        }
        if json[pos] != b'"' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected object key string starting with double quote",
            });
        }

        let key_open = pos;
        let key_start = pos + 1;
        let key_end = find_string_end(json, key_start)?;
        let key = &json[key_start..key_end];

        pos = key_end + 1;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] != b':' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected colon ':' key separator",
            });
        }
        pos += 1;
        pos = skip_whitespace(json, pos);
        let val_start = pos;
        let val_end = skip_value(json, val_start)?;

        if key == key_raw {
            return Ok((key_open, val_start, val_end));
        }

        pos = skip_whitespace(json, val_end);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF",
            });
        }
        if json[pos] == b',' {
            pos += 1;
        } else if json[pos] == b'}' {
            return Err(Error::PathNotFound);
        } else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected comma ',' or closing brace '}'",
            });
        }
    }
}

/// Helper that locates a JSON value byte-slice boundaries within a raw JSON buffer by its path.
/// Returns a tuple of `(start_index, end_index)`.
pub(crate) fn find_value_offsets(json: &[u8], path: &[PathSegment]) -> Result<(usize, usize), Error> {
    if path.is_empty() {
        return Ok((0, json.len()));
    }
    let pos = skip_whitespace(json, 0);
    if pos >= json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos,
            msg: "Unexpected EOF",
        });
    }

    match &path[0] {
        PathSegment::Key(_) => {
            if byte_at(json, pos)? != b'{' {
                return Err(Error::InvalidJsonSyntax {
                    pos,
                    msg: "Expected opening brace '{' for object",
                });
            }
            find_in_object_offsets(json, pos + 1, path)
        }
        PathSegment::Index(_) => {
            if byte_at(json, pos)? != b'[' {
                return Err(Error::InvalidJsonSyntax {
                    pos,
                    msg: "Expected opening bracket '[' for array",
                });
            }
            find_in_array_offsets(json, pos + 1, path)
        }
    }
}

/// Resolve `path` starting from a value that begins at `val_start` (absolute offset).
///
/// Used by structural indexes to jump into an array element (or other sub-value)
/// without re-scanning siblings.
pub(crate) fn find_from_value(
    json: &[u8],
    val_start: usize,
    path: &[PathSegment],
) -> Result<(usize, usize), Error> {
    if path.is_empty() {
        let end = skip_value(json, val_start)?;
        return Ok((val_start, end));
    }
    if val_start >= json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos: val_start,
            msg: "Unexpected EOF",
        });
    }
    match &path[0] {
        PathSegment::Key(_) => {
            if json[val_start] != b'{' {
                return Err(Error::TypeMismatch {
                    expected: "object",
                    found: "primitive/array",
                });
            }
            find_in_object_offsets(json, val_start + 1, path)
        }
        PathSegment::Index(_) => {
            if json[val_start] != b'[' {
                return Err(Error::TypeMismatch {
                    expected: "array",
                    found: "primitive/object",
                });
            }
            find_in_array_offsets(json, val_start + 1, path)
        }
    }
}

/// Recursively or iteratively scans an object starting after the '{' or after a key-value comma.
fn find_in_object_offsets(
    json: &[u8],
    mut pos: usize,
    path: &[PathSegment],
) -> Result<(usize, usize), Error> {
    let target_key = match &path[0] {
        PathSegment::Key(key) => key.as_bytes(),
        _ => return Err(Error::PathNotFound),
    };

    loop {
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF",
            });
        }

        // Check if we hit the end of the object before finding the key
        if json[pos] == b'}' {
            return Err(Error::PathNotFound);
        }

        // We expect a string key starting with '"'
        if json[pos] != b'"' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected object key string starting with double quote",
            });
        }

        let key_start = pos + 1;
        let key_end = find_string_end(json, key_start)?;
        let key = &json[key_start..key_end];

        pos = key_end + 1; // move past closing '"'

        // Skip whitespace and locate the ':' delimiter
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] != b':' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected colon ':' key separator",
            });
        }
        pos += 1; // move past ':'

        pos = skip_whitespace(json, pos);
        let val_start = pos;
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF before value",
            });
        }

        if key == target_key {
            // Matching key: only fully skip the value when this is the leaf segment.
            // Descending into a huge array/object must not scan its entire body first.
            if path.len() == 1 {
                let val_end = skip_value(json, val_start)?;
                return Ok((val_start, val_end));
            }
            match &path[1] {
                PathSegment::Key(_) => {
                    if json[val_start] == b'{' {
                        return find_in_object_offsets(json, val_start + 1, &path[1..]);
                    }
                    return Err(Error::TypeMismatch {
                        expected: "object",
                        found: "primitive/array",
                    });
                }
                PathSegment::Index(_) => {
                    if json[val_start] == b'[' {
                        return find_in_array_offsets(json, val_start + 1, &path[1..]);
                    }
                    return Err(Error::TypeMismatch {
                        expected: "array",
                        found: "primitive/object",
                    });
                }
            }
        }

        // Key didn't match — skip this value and continue scanning siblings.
        let val_end = skip_value(json, val_start)?;
        pos = val_end;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF",
            });
        }

        if json[pos] == b',' {
            pos += 1; // Move past comma to scan next key-value pair
        } else if json[pos] == b'}' {
            return Err(Error::PathNotFound); // End of object
        } else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected comma ',' or closing brace '}'",
            });
        }
    }
}

/// Recursively scans an array starting after the '[' or after an element comma.
fn find_in_array_offsets(
    json: &[u8],
    mut pos: usize,
    path: &[PathSegment],
) -> Result<(usize, usize), Error> {
    let target_idx = match path[0] {
        PathSegment::Index(idx) => idx,
        _ => return Err(Error::PathNotFound),
    };

    // Skip elements to reach the target index
    for _ in 0..target_idx {
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] == b']' {
            return Err(Error::IndexOutOfBounds {
                index: target_idx,
            });
        }
        pos = skip_value(json, pos)?;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF",
            });
        }
        if json[pos] != b',' {
            if json[pos] == b']' {
                return Err(Error::IndexOutOfBounds {
                    index: target_idx,
                });
            }
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected comma ',' array element separator",
            });
        }
        pos += 1; // skip comma
    }

    pos = skip_whitespace(json, pos);
    if pos >= json.len() || json[pos] == b']' {
        return Err(Error::IndexOutOfBounds {
            index: target_idx,
        });
    }

    let val_start = pos;
    if path.len() == 1 {
        let val_end = skip_value(json, val_start)?;
        return Ok((val_start, val_end));
    }

    // Descend without scanning the rest of this element when nested paths remain.
    match &path[1] {
        PathSegment::Key(_) => {
            if json[val_start] == b'{' {
                find_in_object_offsets(json, val_start + 1, &path[1..])
            } else {
                Err(Error::TypeMismatch {
                    expected: "object",
                    found: "primitive/array",
                })
            }
        }
        PathSegment::Index(_) => {
            if json[val_start] == b'[' {
                find_in_array_offsets(json, val_start + 1, &path[1..])
            } else {
                Err(Error::TypeMismatch {
                    expected: "array",
                    found: "primitive/object",
                })
            }
        }
    }
}

pub(crate) fn scan_backwards_whitespace(json: &[u8], mut pos: usize) -> usize {
    while pos > 0 {
        match json[pos - 1] {
            b' ' | b'\t' | b'\n' | b'\r' => pos -= 1,
            _ => break,
        }
    }
    pos
}

/// Skips a JSON value (primitive, string, array, or object) starting at `pos`.
/// Returns the index immediately following the value.
pub(crate) fn skip_value(json: &[u8], mut pos: usize) -> Result<usize, Error> {
    pos = skip_whitespace(json, pos);
    if pos >= json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos,
            msg: "Unexpected EOF",
        });
    }

    match json[pos] {
        b'"' => skip_string(json, pos),
        b'{' | b'[' => skip_squash(json, pos),
        _ => {
            // Primitive (number, true, false, null)
            let start = pos;
            while pos < json.len() {
                match json[pos] {
                    b' ' | b'\t' | b'\n' | b'\r' | b',' | b'}' | b']' => break,
                    _ => pos += 1,
                }
            }
            if pos == start {
                return Err(Error::InvalidJsonSyntax {
                    pos: start,
                    msg: "Expected JSON value",
                });
            }
            Ok(pos)
        }
    }
}

/// True for bytes that must interrupt a bulk container/string skip.
#[inline(always)]
fn is_squash_stop(ch: u8) -> bool {
    matches!(ch, b'"' | b'\\' | b'{' | b'}' | b'[' | b']')
}

/// Skip `{...}` / `[...]` with unified nesting depth (gjson `scan_squash`).
///
/// Continuous 16-byte unrolled scan keeps the CPU in one tight loop — important when
/// skipping large arrays of short-string objects (memchr call overhead adds up).
/// Safe Rust only (`forbid(unsafe_code)`).
fn skip_squash(json: &[u8], open_pos: usize) -> Result<usize, Error> {
    let mut depth = 1isize;
    let mut i = open_pos + 1;
    let len = json.len();

    while depth > 0 {
        // Bulk: advance until a structural stop byte (16-byte windows).
        while i + 16 <= len {
            if is_squash_stop(json[i])
                || is_squash_stop(json[i + 1])
                || is_squash_stop(json[i + 2])
                || is_squash_stop(json[i + 3])
                || is_squash_stop(json[i + 4])
                || is_squash_stop(json[i + 5])
                || is_squash_stop(json[i + 6])
                || is_squash_stop(json[i + 7])
                || is_squash_stop(json[i + 8])
                || is_squash_stop(json[i + 9])
                || is_squash_stop(json[i + 10])
                || is_squash_stop(json[i + 11])
                || is_squash_stop(json[i + 12])
                || is_squash_stop(json[i + 13])
                || is_squash_stop(json[i + 14])
                || is_squash_stop(json[i + 15])
            {
                while !is_squash_stop(json[i]) {
                    i += 1;
                }
                break;
            }
            i += 16;
        }
        while i < len && !is_squash_stop(json[i]) {
            i += 1;
        }

        if i >= len {
            return Err(Error::InvalidJsonSyntax {
                pos: i,
                msg: "Unclosed container",
            });
        }

        match json[i] {
            b'"' => i = skip_string(json, i)?,
            b'{' | b'[' => {
                depth += 1;
                i += 1;
            }
            b'}' | b']' => {
                depth -= 1;
                i += 1;
            }
            b'\\' => {
                i += 1;
                if i < len {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    Ok(i)
}

/// Skip a JSON string starting at the opening `"`. Returns index after the closer.
///
/// gjson-style: unrolled scan for `"` / `\`; on `\`, skip the next byte. Correct for
/// locating the terminator in valid JSON (including `\\` and `\"`).
#[inline]
fn skip_string(json: &[u8], quote_pos: usize) -> Result<usize, Error> {
    let mut i = quote_pos + 1;
    let len = json.len();
    loop {
        while i + 8 <= len {
            let c0 = json[i];
            if c0 == b'"' || c0 == b'\\' {
                break;
            }
            let c1 = json[i + 1];
            if c1 == b'"' || c1 == b'\\' {
                i += 1;
                break;
            }
            let c2 = json[i + 2];
            if c2 == b'"' || c2 == b'\\' {
                i += 2;
                break;
            }
            let c3 = json[i + 3];
            if c3 == b'"' || c3 == b'\\' {
                i += 3;
                break;
            }
            let c4 = json[i + 4];
            if c4 == b'"' || c4 == b'\\' {
                i += 4;
                break;
            }
            let c5 = json[i + 5];
            if c5 == b'"' || c5 == b'\\' {
                i += 5;
                break;
            }
            let c6 = json[i + 6];
            if c6 == b'"' || c6 == b'\\' {
                i += 6;
                break;
            }
            let c7 = json[i + 7];
            if c7 == b'"' || c7 == b'\\' {
                i += 7;
                break;
            }
            i += 8;
        }
        while i < len && json[i] != b'"' && json[i] != b'\\' {
            i += 1;
        }
        if i >= len {
            return Err(Error::InvalidJsonSyntax {
                pos: quote_pos,
                msg: "Unclosed string literal",
            });
        }
        if json[i] == b'"' {
            return Ok(i + 1);
        }
        // `\` + following byte
        i += 2;
        if i > len {
            return Err(Error::InvalidJsonSyntax {
                pos: quote_pos,
                msg: "Unclosed string literal",
            });
        }
    }
}

/// Finds the end of a JSON string starting *after* the opening double-quote.
/// Returns the index of the closing double-quote.
///
/// Fast path: if the slice up to the next `"` contains no `\`, accept immediately
/// (common for keys/values without escapes). Otherwise fall back to backslash-parity.
pub(crate) fn find_string_end(json: &[u8], mut pos: usize) -> Result<usize, Error> {
    while pos < json.len() {
        let Some(next_pos) = memchr(b'"', &json[pos..]) else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unclosed string literal",
            });
        };
        let found_idx = pos + next_pos;
        // No backslash in this span ⇒ the quote cannot be escaped.
        if memchr(b'\\', &json[pos..found_idx]).is_none() {
            return Ok(found_idx);
        }
        // Escape present: classic odd/even backslash run before the quote.
        let mut backslashes = 0usize;
        let mut check = found_idx;
        while check > 0 && json[check - 1] == b'\\' {
            backslashes += 1;
            check -= 1;
        }
        if backslashes % 2 == 0 {
            return Ok(found_idx);
        }
        pos = found_idx + 1;
    }
    Err(Error::InvalidJsonSyntax {
        pos,
        msg: "Unclosed string literal",
    })
}

/// Read `json[pos]` or return a syntax error (no panic on empty / short buffers).
#[inline]
pub(crate) fn byte_at(json: &[u8], pos: usize) -> Result<u8, Error> {
    json.get(pos).copied().ok_or(Error::InvalidJsonSyntax {
        pos,
        msg: "Unexpected EOF",
    })
}

/// Skips whitespace characters starting at `pos`.
#[inline(always)]
pub(crate) fn skip_whitespace(json: &[u8], mut pos: usize) -> usize {
    while pos < json.len() {
        match json[pos] {
            b' ' | b'\t' | b'\n' | b'\r' => pos += 1,
            _ => break,
        }
    }
    pos
}
