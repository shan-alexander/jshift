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
            if json[pos] != b'{' {
                return Err(Error::InvalidJsonSyntax {
                    pos,
                    msg: "Expected opening brace '{' for object",
                });
            }
            find_in_object_offsets(json, pos + 1, path)
        }
        PathSegment::Index(_) => {
            if json[pos] != b'[' {
                return Err(Error::InvalidJsonSyntax {
                    pos,
                    msg: "Expected opening bracket '[' for array",
                });
            }
            find_in_array_offsets(json, pos + 1, path)
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
        let val_end = skip_value(json, val_start)?;

        if key == target_key {
            // We found the matching key!
            if path.len() == 1 {
                return Ok((val_start, val_end));
            } else {
                // We need to go deeper
                match &path[1] {
                    PathSegment::Key(_) => {
                        if json[val_start] == b'{' {
                            return find_in_object_offsets(json, val_start + 1, &path[1..]);
                        } else {
                            return Err(Error::TypeMismatch {
                                expected: "object",
                                found: "primitive/array",
                            });
                        }
                    }
                    PathSegment::Index(_) => {
                        if json[val_start] == b'[' {
                            return find_in_array_offsets(json, val_start + 1, &path[1..]);
                        } else {
                            return Err(Error::TypeMismatch {
                                expected: "array",
                                found: "primitive/object",
                            });
                        }
                    }
                }
            }
        }

        // Key didn't match, skip this value and look for the next comma ',' or object end '}'
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
    let val_end = skip_value(json, val_start)?;

    if path.len() == 1 {
        Ok((val_start, val_end))
    } else {
        // Go deeper
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
        b'"' => {
            let end = find_string_end(json, pos + 1)?;
            Ok(end + 1)
        }
        b'{' => {
            // Scan and balance curly braces, taking string escapes into account
            let mut depth = 1;
            pos += 1;
            while depth > 0 && pos < json.len() {
                match json[pos] {
                    b'"' => {
                        pos = find_string_end(json, pos + 1)? + 1;
                    }
                    b'{' => {
                        depth += 1;
                        pos += 1;
                    }
                    b'}' => {
                        depth -= 1;
                        pos += 1;
                    }
                    _ => pos += 1,
                }
            }
            if depth == 0 {
                Ok(pos)
            } else {
                Err(Error::InvalidJsonSyntax {
                    pos,
                    msg: "Unclosed object brace '}'",
                })
            }
        }
        b'[' => {
            // Scan and balance square brackets
            let mut depth = 1;
            pos += 1;
            while depth > 0 && pos < json.len() {
                match json[pos] {
                    b'"' => {
                        pos = find_string_end(json, pos + 1)? + 1;
                    }
                    b'[' => {
                        depth += 1;
                        pos += 1;
                    }
                    b']' => {
                        depth -= 1;
                        pos += 1;
                    }
                    _ => pos += 1,
                }
            }
            if depth == 0 {
                Ok(pos)
            } else {
                Err(Error::InvalidJsonSyntax {
                    pos,
                    msg: "Unclosed array bracket ']'",
                })
            }
        }
        _ => {
            // Primitive (number, true, false, null)
            // Stop at structural JSON characters or whitespace
            while pos < json.len() {
                match json[pos] {
                    b' ' | b'\t' | b'\n' | b'\r' | b',' | b'}' | b']' => break,
                    _ => pos += 1,
                }
            }
            Ok(pos)
        }
    }
}

/// Finds the end of a JSON string starting *after* the opening double-quote.
/// Returns the index of the closing double-quote.
pub(crate) fn find_string_end(json: &[u8], mut pos: usize) -> Result<usize, Error> {
    while pos < json.len() {
        // Fast scan for next quote or backslash
        if let Some(next_pos) = memchr(b'"', &json[pos..]) {
            let found_idx = pos + next_pos;
            // Check if quote is escaped by counting backslashes before it
            let mut backslashes = 0;
            let mut check_idx = found_idx as isize - 1;
            while check_idx >= 0 && json[check_idx as usize] == b'\\' {
                backslashes += 1;
                check_idx -= 1;
            }
            if backslashes % 2 == 0 {
                return Ok(found_idx); // unescaped quote
            } else {
                pos = found_idx + 1; // escaped quote, keep scanning
            }
        } else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unclosed string literal",
            });
        }
    }
    Err(Error::InvalidJsonSyntax {
        pos,
        msg: "Unclosed string literal",
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
