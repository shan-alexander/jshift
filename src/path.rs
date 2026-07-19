use crate::error::Error;

/// Represents a segment in a JSON path (either a string key or a zero-indexed array index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment<'a> {
    /// A string key representing an object field (e.g., `user` in `{"user": 1}`).
    ///
    /// For keys that contain special JSON characters (`"`, `\`, controls), the segment
    /// must match the **escaped** form as stored between quotes in the document
    /// (e.g. `a\"b` for the logical key `a"b`). Prefer [`crate::upsert_object_key`] /
    /// [`crate::delete_key`] with logical keys when mutating such members.
    Key(&'a str),
    /// A numeric index representing an array position (e.g., `0` in `[10, 20]`).
    Index(usize),
}

/// Parses a dot-and-bracket notation path string into zero-copy path segments.
///
/// This is the **lenient** parser: invalid index brackets (`[x]`, `[]`, unclosed `[`)
/// are skipped or stop parsing rather than returning an error. Prefer
/// [`try_parse_path`] when you need to detect bad path strings.
///
/// Rules:
/// * `.` separates object keys.
/// * `[N]` selects a zero-based array index (`N` must be a decimal `usize`).
/// * Empty key segments (from leading/trailing/duplicate dots) are skipped.
/// * Unclosed `[` stops parsing (no partial index is emitted).
/// * Non-numeric `[...]` content is skipped past the closing `]` without emitting a segment.
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
pub fn parse_path(s: &str) -> Vec<PathSegment<'_>> {
    parse_path_inner(s, false).unwrap_or_default()
}

/// Strict path parser. Returns [`Error::InvalidPath`] for malformed path syntax
/// instead of silently dropping segments.
///
/// Rejects:
/// * Non-numeric index brackets (`[x]`, `[1a]`, `[]`)
/// * Unclosed `[`
/// * Index values that overflow `usize`
///
/// Empty key segments from consecutive dots are still skipped (not an error).
///
/// # Examples
/// ```
/// use jshift::{try_parse_path, PathSegment, Error};
///
/// assert_eq!(
///     try_parse_path("a[0].b").unwrap(),
///     vec![PathSegment::Key("a"), PathSegment::Index(0), PathSegment::Key("b")]
/// );
/// assert!(matches!(
///     try_parse_path("a[x]"),
///     Err(Error::InvalidPath { .. })
/// ));
/// ```
pub fn try_parse_path(s: &str) -> Result<Vec<PathSegment<'_>>, Error> {
    parse_path_inner(s, true)
}

fn parse_path_inner(mut s: &str, strict: bool) -> Result<Vec<PathSegment<'_>>, Error> {
    let mut segments = Vec::new();
    while !s.is_empty() {
        if s.starts_with('.') {
            s = &s[1..];
            continue;
        }
        if s.is_empty() {
            break;
        }
        if s.starts_with('[') {
            match s.find(']') {
                Some(end_idx) => {
                    let idx_str = &s[1..end_idx];
                    if idx_str.is_empty() {
                        if strict {
                            return Err(Error::InvalidPath {
                                msg: "Empty array index brackets",
                            });
                        }
                    } else if !idx_str.bytes().all(|b| b.is_ascii_digit()) {
                        if strict {
                            return Err(Error::InvalidPath {
                                msg: "Non-numeric array index",
                            });
                        }
                    } else {
                        match idx_str.parse::<usize>() {
                            Ok(idx) => segments.push(PathSegment::Index(idx)),
                            Err(_) if strict => {
                                return Err(Error::InvalidPath {
                                    msg: "Array index out of range for usize",
                                });
                            }
                            Err(_) => {}
                        }
                    }
                    s = &s[end_idx + 1..];
                }
                None => {
                    if strict {
                        return Err(Error::InvalidPath {
                            msg: "Unclosed array index bracket '['",
                        });
                    }
                    // Lenient: stop rather than silently consuming the rest as a key.
                    break;
                }
            }
        } else {
            let end_key = s.find(['.', '[']).unwrap_or(s.len());
            let key = &s[..end_key];
            if !key.is_empty() {
                segments.push(PathSegment::Key(key));
            }
            s = &s[end_key..];
        }
    }
    Ok(segments)
}
