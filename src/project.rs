//! Projection size estimates (prost `encoded_len` analogue).
//!
//! Useful before large stream jobs: pre-size output buffers, or decide whether a
//! field projection is worth it vs a full serde round-trip by size ratio.

use crate::error::Error;
use crate::path::{parse_path, PathSegment};
use crate::scan::find_value;

/// Estimate the byte length of a minimal object that keeps only `paths`.
///
/// This is a **planning** estimate (keys + values + structural punctuation), not a
/// guarantee of exact `project` output. Missing paths return [`Error::PathNotFound`].
///
/// Overhead model (flat object of named leaves):
/// `{` + for each path: optional comma, `"key":`, value bytes + `}`.
/// Nested path keys use only the **last** object key segment for the estimate
/// (suitable for top-level projections and ballpark capacity).
///
/// ```
/// use jshift::estimate_projected_len;
///
/// let json = br#"{"id":1,"title":"hi","blob":{"x":1,"y":2}}"#;
/// let n = estimate_projected_len(json, &["id", "title"]).unwrap();
/// // {"id":1,"title":"hi"} → small
/// assert!(n < json.len());
/// assert!(n >= br#"{"id":1,"title":"hi"}"#.len());
/// ```
pub fn estimate_projected_len(json: &[u8], paths: &[&str]) -> Result<usize, Error> {
    if paths.is_empty() {
        return Ok(2); // {}
    }

    let mut total = 1usize; // '{'
    for (i, p) in paths.iter().enumerate() {
        if i > 0 {
            total += 1; // ','
        }
        let segs = parse_path(p);
        let val = find_value(json, &segs)?;
        total += key_wire_len(&segs);
        total += 1; // ':'
        total += val.len();
    }
    total += 1; // '}'
    Ok(total)
}

/// Sum of on-wire value lengths for `paths` (no object framing).
///
/// Cheaper signal for “how much payload do these fields represent?”
pub fn estimate_values_len(json: &[u8], paths: &[&str]) -> Result<usize, Error> {
    let mut total = 0usize;
    for p in paths {
        let segs = parse_path(p);
        total += find_value(json, &segs)?.len();
    }
    Ok(total)
}

fn key_wire_len(segs: &[PathSegment<'_>]) -> usize {
    // "key" including quotes — use last Key segment if any.
    for seg in segs.iter().rev() {
        if let PathSegment::Key(k) = seg {
            return 2 + k.len();
        }
    }
    // Array-only path (rare for projection keys): synthetic short key.
    2
}
