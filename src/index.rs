//! Structural indexing for fast path navigation (safe Rust).
//!
//! This is **not** a full simdjson-style DOM. It builds read-only metadata that
//! accelerates jshift's own job: path finds and bulk iteration over large arrays.
//!
//! # Array side-tables (primary design)
//!
//! For a path prefix that points at an array (e.g. `products`), one linear scan
//! records the absolute start offset of every element. Later queries such as
//! `products[12500].title` jump in O(1) to that element, then run a local object
//! scan — instead of `skip_value`-ing 12 499 siblings.
//!
//! # Mutation
//!
//! Indexes bind to a fixed `&[u8]` snapshot. After any in-place mutate/delete that
//! shifts bytes, **rebuild** the index (or drop it). Preferred ETL pattern:
//! index → many reads / project → stream a new buffer → optional reindex.
//!
//! # Safety
//!
//! Fully safe: `Vec<u32>` offsets and existing cursor helpers. No `unsafe`, no
//! unchecked loads. Stage-1 structural lists / SIMD bitmaps can layer later on
//! the same API surface.

use crate::error::Error;
use crate::path::{OwnedPathSegment, PathSegment};
use crate::scan::{
    byte_at, find_from_value, find_value_offsets, skip_value, skip_whitespace,
};

/// Side-table for one array value: start offset of each element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayIndex {
    /// Absolute byte offset of each element's first non-whitespace value byte.
    element_starts: Vec<u32>,
    /// Offset of the opening `[`.
    open: u32,
    /// Offset of the closing `]`.
    close: u32,
}

impl ArrayIndex {
    /// Number of elements.
    #[inline]
    pub fn len(&self) -> usize {
        self.element_starts.len()
    }

    /// Whether the array is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.element_starts.is_empty()
    }

    /// Opening `[` offset.
    #[inline]
    pub fn open(&self) -> usize {
        self.open as usize
    }

    /// Closing `]` offset.
    #[inline]
    pub fn close(&self) -> usize {
        self.close as usize
    }

    /// Absolute start offset of element `i`, if in range.
    #[inline]
    pub fn element_start(&self, i: usize) -> Option<usize> {
        self.element_starts.get(i).map(|&o| o as usize)
    }

    /// Absolute end offset of element `i` (exclusive): start of next element, or `close`.
    pub fn element_end(&self, i: usize) -> Option<usize> {
        if i >= self.element_starts.len() {
            return None;
        }
        if i + 1 < self.element_starts.len() {
            // Walk back over `,` and whitespace between elements is not needed for
            // end of value — callers that need exact ends should use `skip_value`
            // from `element_start`. This returns the next recorded start as an
            // upper bound exclusive of commas (may include trailing ws of value).
            Some(self.element_starts[i + 1] as usize)
        } else {
            Some(self.close as usize)
        }
    }

    /// All element start offsets.
    pub fn starts(&self) -> &[u32] {
        &self.element_starts
    }
}

/// A JSON buffer plus zero or more array side-tables for path-accelerated reads.
///
/// Does **not** own the buffer; any mutation of the underlying `Vec` invalidates
/// the index until rebuilt.
#[derive(Debug, Clone)]
pub struct IndexedDocument<'a> {
    json: &'a [u8],
    /// Path to the array value → side-table (path ends at the array, not an element).
    arrays: Vec<(Vec<OwnedPathSegment>, ArrayIndex)>,
}

impl<'a> IndexedDocument<'a> {
    /// Empty index over `json` (no side-tables yet).
    pub fn empty(json: &'a [u8]) -> Self {
        Self {
            json,
            arrays: Vec::new(),
        }
    }

    /// Build side-tables for each array path (string paths, lenient parse).
    ///
    /// ```
    /// use jshift::IndexedDocument;
    ///
    /// let json = br#"{"products":[{"id":1},{"id":2},{"id":3}]}"#;
    /// let doc = IndexedDocument::build(json, &["products"]).unwrap();
    /// assert_eq!(doc.array_len(&jshift::parse_path("products")).unwrap(), 3);
    /// assert_eq!(
    ///     doc.find(&jshift::parse_path("products[2].id")).unwrap(),
    ///     b"3"
    /// );
    /// ```
    pub fn build(json: &'a [u8], array_paths: &[&str]) -> Result<Self, Error> {
        let mut doc = Self::empty(json);
        for p in array_paths {
            doc.index_array_str(p)?;
        }
        Ok(doc)
    }

    /// Build side-tables for array paths given as segment slices.
    pub fn build_paths(json: &'a [u8], array_paths: &[&[PathSegment<'_>]]) -> Result<Self, Error> {
        let mut doc = Self::empty(json);
        for p in array_paths {
            doc.index_array(p)?;
        }
        Ok(doc)
    }

    /// Underlying JSON bytes.
    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.json
    }

    /// Index the array at `path` (must resolve to a `[...]` value).
    pub fn index_array(&mut self, path: &[PathSegment]) -> Result<(), Error> {
        let owned = path_to_owned(path);
        // Replace existing index for the same path.
        self.arrays.retain(|(p, _)| p != &owned);
        let table = build_array_index(self.json, path)?;
        self.arrays.push((owned, table));
        Ok(())
    }

    /// [`index_array`] with a path string (`parse_path` rules).
    pub fn index_array_str(&mut self, path: &str) -> Result<(), Error> {
        let segs = crate::path::parse_path(path);
        self.index_array(&segs)
    }

    /// Length of an indexed array, or `None` if that path is not indexed.
    pub fn array_len(&self, path: &[PathSegment]) -> Option<usize> {
        self.lookup_array(path).map(|(_, t)| t.len())
    }

    /// Borrow the side-table for `path` if present.
    pub fn array_index(&self, path: &[PathSegment]) -> Option<&ArrayIndex> {
        self.lookup_array(path).map(|(_, t)| t)
    }

    /// Find a value using indexes when the path walks through a known array.
    ///
    /// Falls back to a normal [`crate::find_value`] scan when no index applies.
    pub fn find(&self, path: &[PathSegment]) -> Result<&'a [u8], Error> {
        let (start, end) = self.find_offsets(path)?;
        Ok(&self.json[start..end])
    }

    /// Like [`find`], returning absolute offsets.
    pub fn find_offsets(&self, path: &[PathSegment]) -> Result<(usize, usize), Error> {
        if let Some((rest_start, rest_path)) = self.try_index_jump(path)? {
            return find_from_value(self.json, rest_start, rest_path);
        }
        find_value_offsets(self.json, path)
    }

    /// Iterate every element of an indexed array as a raw JSON value slice.
    ///
    /// Build cost is paid once at index time; this is O(n) slice bounds only.
    pub fn for_each_element<F>(&self, array_path: &[PathSegment], mut f: F) -> Result<(), Error>
    where
        F: FnMut(usize, &'a [u8]) -> Result<(), Error>,
    {
        let table = self
            .lookup_array(array_path)
            .map(|(_, t)| t)
            .ok_or(Error::PathNotFound)?;
        for i in 0..table.len() {
            let start = table.element_start(i).unwrap();
            let end = skip_value(self.json, start)?;
            f(i, &self.json[start..end])?;
        }
        Ok(())
    }

    /// Same as [`for_each_element`] with a path string.
    pub fn for_each_element_str<F>(&self, array_path: &str, f: F) -> Result<(), Error>
    where
        F: FnMut(usize, &'a [u8]) -> Result<(), Error>,
    {
        let segs = crate::path::parse_path(array_path);
        self.for_each_element(&segs, f)
    }

    /// Number of array side-tables stored.
    pub fn indexed_array_count(&self) -> usize {
        self.arrays.len()
    }
}

impl<'a> IndexedDocument<'a> {
    fn lookup_array(&self, path: &[PathSegment]) -> Option<(usize, &ArrayIndex)> {
        for (i, (owned, table)) in self.arrays.iter().enumerate() {
            if path_eq_owned(path, owned) {
                return Some((i, table));
            }
        }
        None
    }

    /// If `path` is `array_path + Index(i) + rest`, return `(element_start, rest)`.
    fn try_index_jump<'p>(
        &self,
        path: &'p [PathSegment<'p>],
    ) -> Result<Option<(usize, &'p [PathSegment<'p>])>, Error> {
        // Longest matching indexed prefix ending before an Index segment.
        // Prefer longer prefixes if multiple indexes exist.
        let mut best: Option<(usize, usize, usize)> = None; // (prefix_len, elem_start, table_idx)
        for (ti, (owned, table)) in self.arrays.iter().enumerate() {
            let plen = owned.len();
            if path.len() <= plen {
                continue;
            }
            if !path_prefix_eq_owned(&path[..plen], owned) {
                continue;
            }
            let PathSegment::Index(idx) = path[plen] else {
                continue;
            };
            let Some(start) = table.element_start(idx) else {
                return Err(Error::IndexOutOfBounds { index: idx });
            };
            if best.is_none_or(|(bp, _, _)| plen > bp) {
                best = Some((plen, start, ti));
            }
        }
        if let Some((plen, start, _)) = best {
            // rest = path[plen+1..]  (after the Index segment)
            Ok(Some((start, &path[plen + 1..])))
        } else {
            Ok(None)
        }
    }
}

/// Build an [`ArrayIndex`] for the array at `path`.
pub fn build_array_index(json: &[u8], path: &[PathSegment]) -> Result<ArrayIndex, Error> {
    let (start, end) = find_value_offsets(json, path)?;
    if start >= json.len() || byte_at(json, start)? != b'[' {
        return Err(Error::TypeMismatch {
            expected: "array",
            found: "primitive/object",
        });
    }
    // Prefer structural close from scan when possible.
    let close = end.saturating_sub(1);
    if close < start || json.get(close) != Some(&b']') {
        return Err(Error::InvalidJsonSyntax {
            pos: end,
            msg: "Array value missing closing bracket",
        });
    }

    let mut element_starts = Vec::new();
    let mut pos = skip_whitespace(json, start + 1);
    if pos >= json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos,
            msg: "Unexpected EOF inside array",
        });
    }
    if json[pos] == b']' {
        return Ok(ArrayIndex {
            element_starts,
            open: start as u32,
            close: pos as u32,
        });
    }

    loop {
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF inside array",
            });
        }
        if pos > u32::MAX as usize {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Document too large for u32 structural index",
            });
        }
        element_starts.push(pos as u32);
        pos = skip_value(json, pos)?;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF inside array",
            });
        }
        match json[pos] {
            b',' => {
                pos = skip_whitespace(json, pos + 1);
            }
            b']' => {
                return Ok(ArrayIndex {
                    element_starts,
                    open: start as u32,
                    close: pos as u32,
                });
            }
            _ => {
                return Err(Error::InvalidJsonSyntax {
                    pos,
                    msg: "Expected comma ',' or closing bracket ']'",
                });
            }
        }
    }
}

fn path_to_owned(path: &[PathSegment]) -> Vec<OwnedPathSegment> {
    path.iter()
        .map(|s| match s {
            PathSegment::Key(k) => OwnedPathSegment::Key((*k).to_string()),
            PathSegment::Index(i) => OwnedPathSegment::Index(*i),
        })
        .collect()
}

fn path_eq_owned(path: &[PathSegment], owned: &[OwnedPathSegment]) -> bool {
    path.len() == owned.len() && path_prefix_eq_owned(path, owned)
}

fn path_prefix_eq_owned(path: &[PathSegment], owned: &[OwnedPathSegment]) -> bool {
    if path.len() != owned.len() {
        return false;
    }
    path.iter().zip(owned.iter()).all(|(a, b)| match (a, b) {
        (PathSegment::Key(k), OwnedPathSegment::Key(ok)) => *k == ok.as_str(),
        (PathSegment::Index(i), OwnedPathSegment::Index(oi)) => i == oi,
        _ => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::parse_path;

    #[test]
    fn index_jump_mid_array() {
        // Build a modest array so the test stays fast.
        let mut s = String::from(r#"{"products":["#);
        for i in 0..500 {
            if i > 0 {
                s.push(',');
            }
            s.push_str(&format!(r#"{{"id":{i},"title":"item_{i}"}}"#));
        }
        s.push_str("]}");
        let json = s.into_bytes();
        let doc = IndexedDocument::build(&json, &["products"]).unwrap();
        assert_eq!(doc.array_len(&parse_path("products")).unwrap(), 500);

        let v = doc.find(&parse_path("products[0].title")).unwrap();
        assert_eq!(v, br#""item_0""#);
        let v = doc.find(&parse_path("products[250].id")).unwrap();
        assert_eq!(v, b"250");
        let v = doc.find(&parse_path("products[499].title")).unwrap();
        assert_eq!(v, br#""item_499""#);

        assert!(matches!(
            doc.find(&parse_path("products[500].id")),
            Err(Error::IndexOutOfBounds { index: 500 })
        ));
    }

    #[test]
    fn for_each_counts() {
        let json = br#"{"a":[1,2,3,4]}"#;
        let doc = IndexedDocument::build(json, &["a"]).unwrap();
        let mut n = 0;
        doc.for_each_element(&parse_path("a"), |i, slice| {
            assert_eq!(slice, [b'1' + i as u8].as_slice());
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 4);
    }

    #[test]
    fn empty_and_nested_paths() {
        let json = br#"{"outer":{"items":[]}}"#;
        let doc = IndexedDocument::build(json, &["outer.items"]).unwrap();
        assert_eq!(doc.array_len(&parse_path("outer.items")).unwrap(), 0);
        assert!(doc.find(&parse_path("outer.items[0]")).is_err());
    }

    #[test]
    fn fallback_without_index() {
        let json = br#"{"x":{"y":1}}"#;
        let doc = IndexedDocument::empty(json);
        assert_eq!(doc.find(&parse_path("x.y")).unwrap(), b"1");
    }
}
