//! Projection size **estimates** (planning only; not a stream projector).
//!
//! These helpers answer "about how big would a field subset be?" for capacity
//! and triage. They are **not** a substitute for a full `project_paths` / stream
//! projector (deferred; see changelog). Do not treat the return value as an
//! exact output length for production writers.
//!
//! Useful before large jobs: pre-size buffers with headroom, or compare "keep
//! these fields" size ratio vs full serde re-serialize.

use crate::error::Error;
use crate::path::{parse_path, PathSegment};
use crate::scan::find_value;

/// **Ballpark** byte length of a minimal flat object keeping only `paths`.
///
/// # What this is
///
/// A **planning estimate**: sum of on-wire value lengths plus a simple model for
/// keys and object punctuation (`{`, `"key":`, `,`, `}`). Good for capacity
/// hints and "is projection worth it?" ratios.
///
/// # What this is not
///
/// * **Not** exact output of a real projector (whitespace, key order, nested
///   reshaping, and pretty-print differ).
/// * **Not** a stream `project()` API; that is intentionally out of scope for
///   this helper.
/// * Nested path keys contribute only the **last** object key segment to the
///   key-length model (flat-leaf bias).
///
/// Missing paths return [`Error::PathNotFound`].
///
/// ```
/// use jshift::estimate_projected_len;
///
/// let json = br#"{"id":1,"title":"hi","blob":{"x":1,"y":2}}"#;
/// let n = estimate_projected_len(json, &["id", "title"]).unwrap();
/// // Smaller than the full document; roughly {"id":1,"title":"hi"}.
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
/// Cheaper signal for "how much payload do these fields represent?" Still a
/// planning metric, not a projector.
pub fn estimate_values_len(json: &[u8], paths: &[&str]) -> Result<usize, Error> {
    let mut total = 0usize;
    for p in paths {
        let segs = parse_path(p);
        total += find_value(json, &segs)?.len();
    }
    Ok(total)
}

fn key_wire_len(segs: &[PathSegment<'_>]) -> usize {
    // "key" including quotes: use last Key segment if any.
    for seg in segs.iter().rev() {
        if let PathSegment::Key(k) = seg {
            return 2 + k.len();
        }
    }
    // Array-only path (rare for projection keys): synthetic short key.
    2
}
