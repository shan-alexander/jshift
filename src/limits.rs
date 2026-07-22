//! Depth / size limits — DoS-oriented guards (roadmap correctness track).
//!
//! Optional checks for untrusted payloads. Default APIs remain unlimited;
//! call [`check_document`] / [`check_depth`] when you need a hard ceiling.
//!
//! ```
//! use jshift::{check_document, Limits, Error};
//!
//! let deep = br#"[[[1]]]"#;
//! assert!(check_document(deep, &Limits { max_depth: 10, max_bytes: 1024 }).is_ok());
//! assert!(matches!(
//!     check_document(deep, &Limits { max_depth: 1, max_bytes: 1024 }),
//!     Err(Error::LimitExceeded { .. })
//! ));
//! ```

use crate::error::Error;
use crate::scan::skip_whitespace;

/// Limits applied by [`check_document`] / [`check_depth`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Maximum container nesting depth (`[` / `{`).
    pub max_depth: usize,
    /// Maximum input length in bytes.
    pub max_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_depth: 128,
            max_bytes: 64 * 1024 * 1024, // 64 MiB
        }
    }
}

impl Limits {
    /// Strict-ish defaults for untrusted network JSON.
    pub fn strict() -> Self {
        Self {
            max_depth: 32,
            max_bytes: 1024 * 1024, // 1 MiB
        }
    }

    pub fn unlimited() -> Self {
        Self {
            max_depth: usize::MAX,
            max_bytes: usize::MAX,
        }
    }
}

/// Reject oversized buffers and excessive nesting (single structural pass).
pub fn check_document(json: &[u8], limits: &Limits) -> Result<(), Error> {
    if json.len() > limits.max_bytes {
        return Err(Error::LimitExceeded {
            kind: "bytes",
            limit: limits.max_bytes,
            found: json.len(),
        });
    }
    check_depth(json, limits.max_depth)
}

/// Measure max nesting depth of the root value; error if `> max_depth`.
pub fn check_depth(json: &[u8], max_depth: usize) -> Result<(), Error> {
    let start = skip_whitespace(json, 0);
    if start >= json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos: start,
            msg: "Empty document",
        });
    }
    let mut peak = 0usize;
    let _end = walk_depth(json, start, 0, max_depth, &mut peak)?;
    Ok(())
}

/// Report peak nesting depth without failing (still errors on syntax).
pub fn measure_depth(json: &[u8]) -> Result<usize, Error> {
    let start = skip_whitespace(json, 0);
    if start >= json.len() {
        return Ok(0);
    }
    let mut peak = 0usize;
    walk_depth(json, start, 0, usize::MAX, &mut peak)?;
    Ok(peak)
}

fn walk_depth(
    json: &[u8],
    mut pos: usize,
    depth: usize,
    max_depth: usize,
    peak: &mut usize,
) -> Result<usize, Error> {
    pos = skip_whitespace(json, pos);
    if pos >= json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos,
            msg: "Unexpected EOF",
        });
    }
    *peak = (*peak).max(depth);
    if depth > max_depth {
        return Err(Error::LimitExceeded {
            kind: "depth",
            limit: max_depth,
            found: depth,
        });
    }
    match json[pos] {
        b'"' => crate::scan::skip_value(json, pos),
        b'{' => walk_object(json, pos, depth, max_depth, peak),
        b'[' => walk_array(json, pos, depth, max_depth, peak),
        _ => crate::scan::skip_value(json, pos),
    }
}

fn walk_object(
    json: &[u8],
    open: usize,
    depth: usize,
    max_depth: usize,
    peak: &mut usize,
) -> Result<usize, Error> {
    let mut pos = open + 1;
    pos = skip_whitespace(json, pos);
    if pos < json.len() && json[pos] == b'}' {
        return Ok(pos + 1);
    }
    loop {
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unclosed object",
            });
        }
        if json[pos] != b'"' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected object key",
            });
        }
        pos = crate::scan::skip_value(json, pos)?; // key string
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] != b':' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected colon",
            });
        }
        pos += 1;
        pos = walk_depth(json, pos, depth + 1, max_depth, peak)?;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unclosed object",
            });
        }
        if json[pos] == b',' {
            pos += 1;
            continue;
        }
        if json[pos] == b'}' {
            return Ok(pos + 1);
        }
        return Err(Error::InvalidJsonSyntax {
            pos,
            msg: "Expected comma or '}'",
        });
    }
}

fn walk_array(
    json: &[u8],
    open: usize,
    depth: usize,
    max_depth: usize,
    peak: &mut usize,
) -> Result<usize, Error> {
    let mut pos = open + 1;
    pos = skip_whitespace(json, pos);
    if pos < json.len() && json[pos] == b']' {
        return Ok(pos + 1);
    }
    loop {
        pos = walk_depth(json, pos, depth + 1, max_depth, peak)?;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unclosed array",
            });
        }
        if json[pos] == b',' {
            pos += 1;
            pos = skip_whitespace(json, pos);
            continue;
        }
        if json[pos] == b']' {
            return Ok(pos + 1);
        }
        return Err(Error::InvalidJsonSyntax {
            pos,
            msg: "Expected comma or ']'",
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depth_and_bytes() {
        assert_eq!(measure_depth(br#"{"a":1}"#).unwrap(), 1);
        assert_eq!(measure_depth(br#"[[[1]]]"#).unwrap(), 3);
        assert!(check_document(
            br#"[[[1]]]"#,
            &Limits {
                max_depth: 2,
                max_bytes: 100
            }
        )
        .is_err());
        assert!(check_document(
            br#"{"a":1}"#,
            &Limits {
                max_depth: 10,
                max_bytes: 3
            }
        )
        .is_err());
    }
}
