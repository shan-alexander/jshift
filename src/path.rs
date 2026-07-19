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

/// Parses a dot-and-bracket notation path string into a vector of zero-copy path segments.
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
pub fn parse_path(mut s: &str) -> Vec<PathSegment<'_>> {
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
                    // Accept only non-empty ASCII digit runs so we never emit a segment for
                    // `[foo]`, `[]`, or `[1x]` (the latter fails `parse` after the digit check).
                    if !idx_str.is_empty()
                        && idx_str.bytes().all(|b| b.is_ascii_digit())
                        && let Ok(idx) = idx_str.parse::<usize>()
                    {
                        segments.push(PathSegment::Index(idx));
                    }
                    s = &s[end_idx + 1..];
                }
                None => {
                    // Unclosed bracket — stop rather than silently consuming the rest.
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
    segments
}
