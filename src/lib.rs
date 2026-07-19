#![forbid(unsafe_code)]

//! `jshift` — Schema-guided, safe in-place JSON path reader and mutator.
//!
//! This crate provides a 100% safe Rust engine to selectively read, mutate, upsert,
//! and delete values inside raw JSON byte buffers (`&[u8]` and `Vec<u8>`) without
//! building a full AST. Path scans return zero-copy slices; mutations resize the
//! buffer and shift the tail with safe slice rotations.
//!
//! # Features
//! * **Zero-copy reads:** Find values as slices into the raw buffer.
//! * **In-place mutations:** Safe byte-shifting (including resize) via slice rotations.
//! * **Macro-generated schemas:** `#[derive(JsonMutatorSchema)]` for typed readers and mutators.
//! * **Array and object CRUD:** Insert, update, append, and delete dynamically.
//! * **JSON string escaping:** `ToJsonBytes` and key upserts escape special characters.
//!
//! # Quick Start
//! ```
//! use jshift::{find_value, mutate_value, parse_path};
//!
//! let mut json = b"{\"user\": \"farmer\", \"score\": 9.5}".to_vec();
//! let path = parse_path("score");
//!
//! // Read value
//! let score_bytes = find_value(&json, &path).unwrap();
//! assert_eq!(score_bytes, b"9.5");
//!
//! // Mutate in-place
//! mutate_value(&mut json, &path, b"10.0").unwrap();
//! assert_eq!(json, b"{\"user\": \"farmer\", \"score\": 10.0}".to_vec());
//! ```
//!
//! # High-Impact Real-World Use Case: LLM Dataset Processing (JSONL)
//! In AI training pipelines (e.g., LoRA finetuning), datasets are stored as JSONL files.
//! You can inspect token lengths and mark records as skipped or cleaned in-place:
//!
//! ```
//! use jshift::JsonMutatorSchema;
//!
//! #[derive(JsonMutatorSchema)]
//! struct TrainingRecord {
//!     #[json(path = "tokens")]
//!     tokens: usize,
//!     #[json(path = "status")]
//!     status: String,
//! }
//!
//! let mut line = b"{\"instruction\": \"Translate...\", \"tokens\": 1024, \"status\": \"pending\"}".to_vec();
//!
//! // Parse selectively
//! let record = TrainingRecord::read_from_json(&line).unwrap();
//!
//! // Skip long contexts in-place!
//! if record.tokens > 512 {
//!     let mut mutator = TrainingRecord::mutator(&mut line);
//!     mutator.set_status("skipped").unwrap();
//! }
//!
//! assert_eq!(
//!     line,
//!     b"{\"instruction\": \"Translate...\", \"tokens\": 1024, \"status\": \"skipped\"}".to_vec()
//! );
//! ```

use memchr::memchr;

pub use jshift_derive::JsonMutatorSchema;

/// Errors returned by scanning and mutating operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The specified JSON path was not found in the document.
    PathNotFound,
    /// The JSON document is structurally malformed.
    InvalidJsonSyntax { 
        /// Byte offset in the JSON buffer where the syntax error was detected.
        pos: usize, 
        /// Informative message describing the syntax error.
        msg: &'static str 
    },
    /// The array index is larger than the number of elements in the array.
    IndexOutOfBounds { 
        /// The index that was queried.
        index: usize 
    },
    /// The parsed type does not match the JSON value format.
    TypeMismatch { 
        /// Expected type name (e.g. `"array"`, `"object"`).
        expected: &'static str, 
        /// Encountered type name (e.g. `"primitive"`).
        found: &'static str 
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::PathNotFound => write!(f, "Path not found in JSON"),
            Error::InvalidJsonSyntax { pos, msg } => {
                write!(f, "Invalid JSON syntax at position {}: {}", pos, msg)
            }
            Error::IndexOutOfBounds { index } => {
                write!(f, "Array index out of bounds: {}", index)
            }
            Error::TypeMismatch { expected, found } => {
                write!(
                    f,
                    "Type mismatch: expected '{}', found '{}'",
                    expected, found
                )
            }
        }
    }
}

impl std::error::Error for Error {}

/// Represents a segment in a JSON path (either a string key or a zero-indexed array index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment<'a> {
    /// A string key representing an object field (e.g., `user` in `{"user": 1}`).
    Key(&'a str),
    /// A numeric index representing an array position (e.g., `0` in `[10, 20]`).
    Index(usize),
}

/// Parses a dot-and-bracket notation path string into a vector of zero-copy path segments.
///
/// # Examples
/// ```
/// use jshift::{parse_path, PathSegment};
///
/// let path = parse_path("metadata.tags[0].name");
/// assert_eq!(path, vec![
///     PathSegment::Key("metadata"),
///     PathSegment::Key("tags"),
///     PathSegment::Index(0),
///     PathSegment::Key("name")
/// ]);
/// ```
pub fn parse_path(mut s: &str) -> Vec<PathSegment<'_>> {
    let mut segments = Vec::new();
    while !s.is_empty() {
        if s.starts_with('.') {
            s = &s[1..];
        }
        if s.is_empty() {
            break;
        }
        if s.starts_with('[') {
            let end_idx = s.find(']').unwrap_or(s.len());
            let idx_str = &s[1..end_idx];
            if let Ok(idx) = idx_str.parse::<usize>() {
                segments.push(PathSegment::Index(idx));
            }
            s = if end_idx < s.len() { &s[end_idx + 1..] } else { "" };
        } else {
            let end_key = s.find(|c| c == '.' || c == '[').unwrap_or(s.len());
            let key = &s[..end_key];
            segments.push(PathSegment::Key(key));
            s = &s[end_key..];
        }
    }
    segments
}

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

/// Helper that locates a JSON value byte-slice boundaries within a raw JSON buffer by its path.
/// Returns a tuple of `(start_index, end_index)`.
fn find_value_offsets(json: &[u8], path: &[PathSegment]) -> Result<(usize, usize), Error> {
    if path.is_empty() {
        return Ok((0, json.len()));
    }
    let pos = skip_whitespace(json, 0);
    if pos >= json.len() {
        return Err(Error::InvalidJsonSyntax { pos, msg: "Unexpected EOF" });
    }
    
    match &path[0] {
        PathSegment::Key(_) => {
            if json[pos] != b'{' {
                return Err(Error::InvalidJsonSyntax { pos, msg: "Expected opening brace '{' for object" });
            }
            find_in_object_offsets(json, pos + 1, path)
        }
        PathSegment::Index(_) => {
            if json[pos] != b'[' {
                return Err(Error::InvalidJsonSyntax { pos, msg: "Expected opening bracket '[' for array" });
            }
            find_in_array_offsets(json, pos + 1, path)
        }
    }
}

/// Recursively or iteratively scans an object starting after the '{' or after a key-value comma.
fn find_in_object_offsets(json: &[u8], mut pos: usize, path: &[PathSegment]) -> Result<(usize, usize), Error> {
    let target_key = match &path[0] {
        PathSegment::Key(key) => key.as_bytes(),
        _ => return Err(Error::PathNotFound),
    };
    
    loop {
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax { pos, msg: "Unexpected EOF" });
        }

        // Check if we hit the end of the object before finding the key
        if json[pos] == b'}' {
            return Err(Error::PathNotFound);
        }

        // We expect a string key starting with '"'
        if json[pos] != b'"' {
            return Err(Error::InvalidJsonSyntax { pos, msg: "Expected object key string starting with double quote" });
        }
        
        let key_start = pos + 1;
        let key_end = find_string_end(json, key_start)?;
        let key = &json[key_start..key_end];
        
        pos = key_end + 1; // move past closing '"'
        
        // Skip whitespace and locate the ':' delimiter
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] != b':' {
            return Err(Error::InvalidJsonSyntax { pos, msg: "Expected colon ':' key separator" });
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
                            return Err(Error::TypeMismatch { expected: "object", found: "primitive/array" });
                        }
                    }
                    PathSegment::Index(_) => {
                        if json[val_start] == b'[' {
                            return find_in_array_offsets(json, val_start + 1, &path[1..]);
                        } else {
                            return Err(Error::TypeMismatch { expected: "array", found: "primitive/object" });
                        }
                    }
                }
            }
        }
        
        // Key didn't match, skip this value and look for the next comma ',' or object end '}'
        pos = val_end;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax { pos, msg: "Unexpected EOF" });
        }
        
        if json[pos] == b',' {
            pos += 1; // Move past comma to scan next key-value pair
        } else if json[pos] == b'}' {
            return Err(Error::PathNotFound); // End of object
        } else {
            return Err(Error::InvalidJsonSyntax { pos, msg: "Expected comma ',' or closing brace '}'" });
        }
    }
}

/// Recursively scans an array starting after the '[' or after an element comma.
fn find_in_array_offsets(json: &[u8], mut pos: usize, path: &[PathSegment]) -> Result<(usize, usize), Error> {
    let target_idx = match path[0] {
        PathSegment::Index(idx) => idx,
        _ => return Err(Error::PathNotFound),
    };
    
    // Skip elements to reach the target index
    for _ in 0..target_idx {
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] == b']' {
            return Err(Error::IndexOutOfBounds { index: target_idx });
        }
        pos = skip_value(json, pos)?;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax { pos, msg: "Unexpected EOF" });
        }
        if json[pos] != b',' {
            if json[pos] == b']' {
                return Err(Error::IndexOutOfBounds { index: target_idx });
            }
            return Err(Error::InvalidJsonSyntax { pos, msg: "Expected comma ',' array element separator" });
        }
        pos += 1; // skip comma
    }
    
    pos = skip_whitespace(json, pos);
    if pos >= json.len() || json[pos] == b']' {
        return Err(Error::IndexOutOfBounds { index: target_idx });
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
                    Err(Error::TypeMismatch { expected: "object", found: "primitive/array" })
                }
            }
            PathSegment::Index(_) => {
                if json[val_start] == b'[' {
                    find_in_array_offsets(json, val_start + 1, &path[1..])
                } else {
                    Err(Error::TypeMismatch { expected: "array", found: "primitive/object" })
                }
            }
        }
    }
}

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
pub fn append_to_array(json: &mut Vec<u8>, path: &[PathSegment], new_element: &[u8]) -> Result<(), Error> {
    let (start, end) = find_value_offsets(json, path)?;
    
    if json[start] != b'[' {
        return Err(Error::TypeMismatch { expected: "array", found: "primitive/object" });
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
        json[insertion_point + 1..insertion_point + 1 + new_element.len()].copy_from_slice(new_element);
    }
    
    Ok(())
}

fn is_array_empty(json: &[u8], start: usize, end: usize) -> Result<bool, Error> {
    if start + 1 >= end {
        return Err(Error::InvalidJsonSyntax { pos: start, msg: "Empty or truncated array offsets" });
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
        return Err(Error::TypeMismatch { expected: "array", found: "primitive/object" });
    }
    
    let mut pos = skip_whitespace(json, start + 1);
    if pos >= json.len() {
        return Err(Error::InvalidJsonSyntax { pos, msg: "Unexpected EOF" });
    }
    if json[pos] == b']' {
        return Ok(0);
    }
    
    let mut count = 1;
    loop {
        pos = skip_value(json, pos)?;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax { pos, msg: "Unexpected EOF" });
        }
        if json[pos] == b',' {
            count += 1;
            pos += 1;
        } else if json[pos] == b']' {
            break;
        } else {
            return Err(Error::InvalidJsonSyntax { pos, msg: "Expected comma ',' or closing bracket ']'" });
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
pub fn upsert_object_key(json: &mut Vec<u8>, path: &[PathSegment], key: &str, new_value: &[u8]) -> Result<(), Error> {
    let mut target_path = path.to_vec();
    target_path.push(PathSegment::Key(key));
    
    if find_value_offsets(json, &target_path).is_ok() {
        // Key exists, perform standard mutation
        return mutate_value(json, &target_path, new_value);
    }
    
    let (start, end) = find_value_offsets(json, path)?;
    if json[start] != b'{' {
        return Err(Error::TypeMismatch { expected: "object", found: "primitive/array" });
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
        return Err(Error::InvalidJsonSyntax { pos: start, msg: "Empty or truncated object offsets" });
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
    let mut target_path = path.to_vec();
    target_path.push(PathSegment::Key(key));
    
    let (val_start, val_end) = find_value_offsets(json, &target_path)?;
    
    // Scan backwards from val_start to locate the key string and quotes
    let mut pos = val_start;
    pos = scan_backwards_whitespace(json, pos);
    if pos == 0 || json[pos - 1] != b':' {
        return Err(Error::InvalidJsonSyntax { pos, msg: "Expected colon key separator" });
    }
    pos -= 1; // skip ':'
    
    pos = scan_backwards_whitespace(json, pos);
    if pos == 0 || json[pos - 1] != b'"' {
        return Err(Error::InvalidJsonSyntax { pos, msg: "Expected closing quote of key" });
    }
    pos -= 1; // skip '"'
    
    while pos > 0 && json[pos - 1] != b'"' {
        pos -= 1;
    }
    if pos == 0 {
        return Err(Error::InvalidJsonSyntax { pos: 0, msg: "Unclosed key string" });
    }
    let key_start = pos - 1;
    
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

fn scan_backwards_whitespace(json: &[u8], mut pos: usize) -> usize {
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
fn skip_value(json: &[u8], mut pos: usize) -> Result<usize, Error> {
    pos = skip_whitespace(json, pos);
    if pos >= json.len() {
        return Err(Error::InvalidJsonSyntax { pos, msg: "Unexpected EOF" });
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
                Err(Error::InvalidJsonSyntax { pos, msg: "Unclosed object brace '}'" })
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
                Err(Error::InvalidJsonSyntax { pos, msg: "Unclosed array bracket ']'" })
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
fn find_string_end(json: &[u8], mut pos: usize) -> Result<usize, Error> {
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
            return Err(Error::InvalidJsonSyntax { pos, msg: "Unclosed string literal" });
        }
    }
    Err(Error::InvalidJsonSyntax { pos, msg: "Unclosed string literal" })
}

/// Skips whitespace characters starting at `pos`.
#[inline(always)]
fn skip_whitespace(json: &[u8], mut pos: usize) -> usize {
    while pos < json.len() {
        match json[pos] {
            b' ' | b'\t' | b'\n' | b'\r' => pos += 1,
            _ => break,
        }
    }
    pos
}

/// Trait implemented by types that can be parsed directly from a raw JSON byte slice.
pub trait FromJsonSlice: Sized {
    /// Attempts to parse an instance of `Self` from the provided raw JSON byte slice.
    fn from_json_slice(slice: &[u8]) -> Option<Self>;
}

impl FromJsonSlice for String {
    fn from_json_slice(slice: &[u8]) -> Option<Self> {
        if slice.len() >= 2 && slice[0] == b'"' && slice[slice.len() - 1] == b'"' {
            std::str::from_utf8(&slice[1..slice.len() - 1]).ok().map(String::from)
        } else {
            std::str::from_utf8(slice).ok().map(String::from)
        }
    }
}

impl FromJsonSlice for bool {
    fn from_json_slice(slice: &[u8]) -> Option<Self> {
        match slice {
            b"true" => Some(true),
            b"false" => Some(false),
            _ => None,
        }
    }
}

impl<T: FromJsonSlice> FromJsonSlice for Vec<T> {
    fn from_json_slice(slice: &[u8]) -> Option<Self> {
        let mut pos = skip_whitespace(slice, 0);
        if pos >= slice.len() || slice[pos] != b'[' {
            return None;
        }
        pos += 1;
        
        let mut vec = Vec::new();
        loop {
            pos = skip_whitespace(slice, pos);
            if pos >= slice.len() {
                return None;
            }
            if slice[pos] == b']' {
                break;
            }
            let val_start = pos;
            let val_end = skip_value(slice, val_start).ok()?;
            let val_slice = &slice[val_start..val_end];
            let item = T::from_json_slice(val_slice)?;
            vec.push(item);
            
            pos = val_end;
            pos = skip_whitespace(slice, pos);
            if pos >= slice.len() {
                return None;
            }
            if slice[pos] == b',' {
                pos += 1;
            } else if slice[pos] == b']' {
                // Handled in next loop iteration
            } else {
                return None;
            }
        }
        Some(vec)
    }
}

macro_rules! impl_from_json_numeric {
    ($($t:ty),*) => {
        $(
            impl FromJsonSlice for $t {
                fn from_json_slice(slice: &[u8]) -> Option<Self> {
                    std::str::from_utf8(slice).ok()?.parse().ok()
                }
            }
        )*
    };
}

impl_from_json_numeric!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);

/// Trait implemented by types that can be serialized directly into a JSON byte representation.
pub trait ToJsonBytes {
    /// Serializes the value into a raw JSON byte vector.
    ///
    /// String implementations produce a JSON string literal with required escapes
    /// (`"`, `\`, and control characters).
    fn to_json_bytes(&self) -> Vec<u8>;
}

/// Append a JSON string literal (including surrounding quotes) for `s` into `out`.
///
/// Escapes `"`, `\`, and ASCII control characters per RFC 8259.
pub fn write_json_string(out: &mut Vec<u8>, s: &str) {
    out.reserve(s.len() + 2);
    out.push(b'"');
    for &b in s.as_bytes() {
        match b {
            b'"' => out.extend_from_slice(br#"\""#),
            b'\\' => out.extend_from_slice(br#"\\"#),
            b'\n' => out.extend_from_slice(br#"\n"#),
            b'\r' => out.extend_from_slice(br#"\r"#),
            b'\t' => out.extend_from_slice(br#"\t"#),
            b'\x08' => out.extend_from_slice(br#"\b"#),
            b'\x0c' => out.extend_from_slice(br#"\f"#),
            c if c < 0x20 => {
                const HEX: &[u8; 16] = b"0123456789abcdef";
                out.extend_from_slice(br#"\u00"#);
                out.push(HEX[(c >> 4) as usize]);
                out.push(HEX[(c & 0xf) as usize]);
            }
            c => out.push(c),
        }
    }
    out.push(b'"');
}

/// Serialize `s` as a JSON string literal (including surrounding quotes).
pub fn escape_json_string(s: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(s.len() + 2);
    write_json_string(&mut v, s);
    v
}

impl ToJsonBytes for String {
    fn to_json_bytes(&self) -> Vec<u8> {
        escape_json_string(self)
    }
}

impl ToJsonBytes for str {
    fn to_json_bytes(&self) -> Vec<u8> {
        escape_json_string(self)
    }
}

impl ToJsonBytes for bool {
    fn to_json_bytes(&self) -> Vec<u8> {
        if *self { b"true".to_vec() } else { b"false".to_vec() }
    }
}

macro_rules! impl_to_json_numeric {
    ($($t:ty),*) => {
        $(
            impl ToJsonBytes for $t {
                fn to_json_bytes(&self) -> Vec<u8> {
                    self.to_string().into_bytes()
                }
            }
        )*
    };
}

impl_to_json_numeric!(u8, u16, u32, u64, usize, i8, i16, i32, i64, isize, f32, f64);

impl<T: ToJsonBytes> ToJsonBytes for Vec<T> {
    fn to_json_bytes(&self) -> Vec<u8> {
        let mut v = Vec::new();
        v.push(b'[');
        for (i, item) in self.iter().enumerate() {
            if i > 0 {
                v.push(b',');
            }
            v.extend_from_slice(&item.to_json_bytes());
        }
        v.push(b']');
        v
    }
}

impl<T: ToJsonBytes> ToJsonBytes for [T] {
    fn to_json_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.len() * 10 + 2);
        v.push(b'[');
        for (i, item) in self.iter().enumerate() {
            if i > 0 {
                v.push(b',');
            }
            v.extend_from_slice(&item.to_json_bytes());
        }
        v.push(b']');
        v
    }
}

#[cfg(test)]
mod tests {
    extern crate self as jshift;
    use super::*;

    #[test]
    fn test_find_simple_values() {
        let json = b"{\"a\": 123, \"b\": \"hello\", \"c\": true}";
        
        assert_eq!(find_value(json, &parse_path("a")), Ok(&b"123"[..]));
        assert_eq!(find_value(json, &parse_path("b")), Ok(&b"\"hello\""[..]));
        assert_eq!(find_value(json, &parse_path("c")), Ok(&b"true"[..]));
        assert_eq!(find_value(json, &parse_path("d")), Err(Error::PathNotFound));
    }

    #[test]
    fn test_find_nested_values() {
        let json = b"{\"metadata\": {\"version\": 1, \"author\": \"farmer\"}, \"data\": [1,2,3]}";
        
        assert_eq!(find_value(json, &parse_path("metadata.version")), Ok(&b"1"[..]));
        assert_eq!(find_value(json, &parse_path("metadata.author")), Ok(&b"\"farmer\""[..]));
        assert_eq!(find_value(json, &parse_path("data")), Ok(&b"[1,2,3]"[..]));
    }

    #[test]
    fn test_mutate_equal_size() {
        let mut json = b"{\"a\": 123, \"b\": \"hello\"}".to_vec();
        mutate_value(&mut json, &parse_path("a"), b"999").unwrap();
        assert_eq!(json, b"{\"a\": 999, \"b\": \"hello\"}");
    }

    #[test]
    fn test_mutate_smaller_size() {
        let mut json = b"{\"a\": 12345, \"b\": \"hello\"}".to_vec();
        mutate_value(&mut json, &parse_path("a"), b"9").unwrap();
        assert_eq!(json, b"{\"a\": 9, \"b\": \"hello\"}");
    }

    #[test]
    fn test_mutate_larger_size() {
        let mut json = b"{\"a\": 1, \"b\": \"hello\"}".to_vec();
        mutate_value(&mut json, &parse_path("a"), b"99999").unwrap();
        assert_eq!(json, b"{\"a\": 99999, \"b\": \"hello\"}");
    }

    #[test]
    fn test_mutate_nested() {
        let mut json = b"{\"meta\": {\"ver\": 1}, \"data\": true}".to_vec();
        mutate_value(&mut json, &parse_path("meta.ver"), b"100").unwrap();
        assert_eq!(json, b"{\"meta\": {\"ver\": 100}, \"data\": true}");
    }

    #[test]
    fn test_array_indexing() {
        let json = b"{\"data\": [{\"id\": 1}, {\"id\": 2}], \"tags\": [\"a\", \"b\"]}";
        
        assert_eq!(find_value(json, &parse_path("data[0].id")), Ok(&b"1"[..]));
        assert_eq!(find_value(json, &parse_path("data[1].id")), Ok(&b"2"[..]));
        assert_eq!(find_value(json, &parse_path("tags[1]")), Ok(&b"\"b\""[..]));
        assert_eq!(find_value(json, &parse_path("tags[2]")), Err(Error::IndexOutOfBounds { index: 2 }));
    }

    #[test]
    fn test_array_append_raw() {
        let mut json = b"{\"list\": []}".to_vec();
        append_to_array(&mut json, &parse_path("list"), b"1").unwrap();
        assert_eq!(json, b"{\"list\": [1]}");
        
        append_to_array(&mut json, &parse_path("list"), b"2").unwrap();
        assert_eq!(json, b"{\"list\": [1,2]}");
    }

    #[test]
    fn test_array_len() {
        let json = b"{\"empty\": [], \"list\": [1, 2, 3]}";
        assert_eq!(array_len(json, &parse_path("empty")), Ok(0));
        assert_eq!(array_len(json, &parse_path("list")), Ok(3));
    }

    #[test]
    fn test_upsert_object_key() {
        let mut json = b"{\"a\": 1}".to_vec();
        // Insert new key
        upsert_object_key(&mut json, &[], "b", b"2").unwrap();
        assert_eq!(json, b"{\"a\": 1,\"b\":2}");
        
        // Update existing key
        upsert_object_key(&mut json, &[], "a", b"99").unwrap();
        assert_eq!(json, b"{\"a\": 99,\"b\":2}");
    }

    #[test]
    fn test_delete_key() {
        let mut json = b"{\"a\": 1, \"b\": 2, \"c\": 3}".to_vec();
        delete_key(&mut json, &[], "b").unwrap();
        assert_eq!(json, b"{\"a\": 1, \"c\": 3}");
        
        delete_key(&mut json, &[], "a").unwrap();
        assert_eq!(json, b"{ \"c\": 3}");
        
        delete_key(&mut json, &[], "c").unwrap();
        assert_eq!(json, b"{ }");
    }

    #[test]
    fn test_delete_index() {
        let mut json = b"[10, 20, 30]".to_vec();
        delete_index(&mut json, &[], 1).unwrap();
        assert_eq!(json, b"[10, 30]");
        
        delete_index(&mut json, &[], 0).unwrap();
        assert_eq!(json, b"[ 30]");
        
        delete_index(&mut json, &[], 0).unwrap();
        assert_eq!(json, b"[ ]");
    }

    #[derive(JsonMutatorSchema)]
    struct Config {
        #[json(path = "metadata.version")]
        version: u32,
        #[json(path = "user.score")]
        score: f64,
        #[json(path = "user.name")]
        name: String,
        #[json(path = "user.tags")]
        tags: Vec<String>,
    }

    #[test]
    fn test_procedural_macro() {
        let mut json = b"{\"metadata\": {\"version\": 1}, \"user\": {\"score\": 9.5, \"name\": \"farmer\", \"tags\": [\"rust\", \"fast\"]}}".to_vec();
        
        let config = Config::read_from_json(&json).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.score, 9.5);
        assert_eq!(config.name, "farmer");
        assert_eq!(config.tags, vec!["rust".to_string(), "fast".to_string()]);
        
        let mut mutator = Config::mutator(&mut json);
        mutator.set_version(&2).unwrap();
        mutator.set_score(&99.9).unwrap();
        mutator.set_name("new_name").unwrap();
        mutator.append_tags("cool").unwrap();
        
        let updated = Config::read_from_json(&json).unwrap();
        assert_eq!(updated.version, 2);
        assert_eq!(updated.score, 99.9);
        assert_eq!(updated.name, "new_name");
        assert_eq!(updated.tags, vec!["rust".to_string(), "fast".to_string(), "cool".to_string()]);
    }

    #[test]
    fn test_escape_json_string() {
        assert_eq!(escape_json_string("plain"), br#""plain""#);
        assert_eq!(escape_json_string(r#"say "hi""#), br#""say \"hi\"""#);
        assert_eq!(escape_json_string("a\\b"), br#""a\\b""#);
        assert_eq!(escape_json_string("a\nb\tc"), br#""a\nb\tc""#);
        assert_eq!(escape_json_string("\u{0001}"), br#""\u0001""#);
    }

    #[test]
    fn test_to_json_bytes_escapes_strings() {
        assert_eq!(r#"he"llo"#.to_json_bytes(), br#""he\"llo""#);
        assert_eq!(String::from("x\ny").to_json_bytes(), br#""x\ny""#);
    }

    #[test]
    fn test_upsert_escapes_keys() {
        let mut json = b"{}".to_vec();
        upsert_object_key(&mut json, &[], r#"a"b"#, b"1").unwrap();
        assert_eq!(json, br#"{"a\"b":1}"#);

        // Path matching compares raw key bytes inside quotes (escaped form).
        assert_eq!(find_value(&json, &parse_path(r#"a\"b"#)), Ok(&b"1"[..]));
    }

    #[test]
    fn test_mutate_via_to_json_bytes_keeps_valid_json() {
        let mut json = br#"{"msg":"old"}"#.to_vec();
        let bytes = r#"say "hi""#.to_json_bytes();
        mutate_value(&mut json, &parse_path("msg"), &bytes).unwrap();
        assert_eq!(json, br#"{"msg":"say \"hi\""}"#);
        assert_eq!(find_value(&json, &parse_path("msg")), Ok(&br#""say \"hi\"""#[..]));
    }
}
