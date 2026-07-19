//! Structural indexing for fast path navigation (safe Rust).
//!
//! This is **not** a full simdjson-style DOM. It builds read-only metadata that
//! accelerates jshift's own job: path finds and bulk iteration.
//!
//! # Layers
//!
//! 1. **Array side-tables** — `element_starts[i]` for `path[i].…` in O(1).
//! 2. **Object key maps** — key → value span for wide / hot objects.
//! 3. **Stage-1 structural list** — offsets of `{ } [ ] : ,` outside strings; used to
//!    skip large containers by walking structurals instead of every byte.
//!
//! # Mutation
//!
//! Indexes bind to a fixed `&[u8]` snapshot. After in-place mutate/delete, rebuild
//! (or drop). Preferred ETL: index → many reads → stream new bytes → reindex.
//!
//! # Safety
//!
//! Fully safe: `Vec<u32>`, `HashMap`, existing cursors. No `unsafe`.

use std::collections::HashMap;

use crate::error::Error;
use crate::path::{OwnedPathSegment, PathSegment};
use crate::scan::{
    byte_at, find_from_value, find_string_end, find_value_offsets, skip_value, skip_whitespace,
};

// ─── Stage-1 structural list ────────────────────────────────────────────────

/// Sorted absolute offsets of structural characters **outside** JSON strings:
/// `{ } [ ] : ,`
///
/// Built with the same escape-aware string rules as the rest of jshift (safe loops /
/// `memchr` under the hood via existing string helpers).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StructuralIndex {
    structurals: Vec<u32>,
}

impl StructuralIndex {
    /// Number of structural positions.
    #[inline]
    pub fn len(&self) -> usize {
        self.structurals.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.structurals.is_empty()
    }

    /// All structural offsets (sorted ascending).
    #[inline]
    pub fn offsets(&self) -> &[u32] {
        &self.structurals
    }

    /// Index of the first structural at or after `pos`, if any.
    pub fn first_at_or_after(&self, pos: usize) -> Option<usize> {
        let p = pos.min(u32::MAX as usize) as u32;
        match self.structurals.binary_search(&p) {
            Ok(i) => Some(i),
            Err(i) if i < self.structurals.len() => Some(i),
            Err(_) => None,
        }
    }

    /// Skip a container starting at `open_pos` (`[` or `{`) using only the
    /// structural list (not a full byte scan of the interior).
    pub fn skip_container(&self, json: &[u8], open_pos: usize) -> Result<usize, Error> {
        if open_pos >= json.len() || !matches!(json[open_pos], b'{' | b'[') {
            return Err(Error::InvalidJsonSyntax {
                pos: open_pos,
                msg: "Expected container open for structural skip",
            });
        }
        let mut depth = 1isize;
        let mut si = self
            .first_at_or_after(open_pos + 1)
            .ok_or(Error::InvalidJsonSyntax {
                pos: open_pos,
                msg: "Unclosed container (structural index)",
            })?;
        while si < self.structurals.len() {
            let off = self.structurals[si] as usize;
            if off >= json.len() {
                break;
            }
            match json[off] {
                b'{' | b'[' => depth += 1,
                b'}' | b']' => {
                    depth -= 1;
                    if depth == 0 {
                        return Ok(off + 1);
                    }
                }
                b':' | b',' => {}
                _ => {}
            }
            si += 1;
        }
        Err(Error::InvalidJsonSyntax {
            pos: open_pos,
            msg: "Unclosed container (structural index)",
        })
    }
}

/// Build a document-wide Stage-1 structural index (safe).
pub fn build_structural_index(json: &[u8]) -> Result<StructuralIndex, Error> {
    let mut structurals = Vec::new();
    let mut i = 0usize;
    let len = json.len();
    while i < len {
        match json[i] {
            b'"' => {
                // Jump past the whole string (closing quote + 1).
                let end_q = find_string_end(json, i + 1)?;
                i = end_q + 1;
            }
            b'{' | b'}' | b'[' | b']' | b':' | b',' => {
                if i > u32::MAX as usize {
                    return Err(Error::InvalidJsonSyntax {
                        pos: i,
                        msg: "Document too large for u32 structural index",
                    });
                }
                structurals.push(i as u32);
                i += 1;
            }
            _ => {
                // Bulk skip non-interesting bytes.
                let start = i;
                i += 1;
                while i < len {
                    match json[i] {
                        b'"' | b'{' | b'}' | b'[' | b']' | b':' | b',' => break,
                        _ => i += 1,
                    }
                }
                let _ = start;
            }
        }
    }
    Ok(StructuralIndex { structurals })
}

// ─── Array side-table ───────────────────────────────────────────────────────

/// Side-table for one array value: start offset of each element.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArrayIndex {
    element_starts: Vec<u32>,
    open: u32,
    close: u32,
}

impl ArrayIndex {
    #[inline]
    pub fn len(&self) -> usize {
        self.element_starts.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.element_starts.is_empty()
    }

    #[inline]
    pub fn open(&self) -> usize {
        self.open as usize
    }

    #[inline]
    pub fn close(&self) -> usize {
        self.close as usize
    }

    #[inline]
    pub fn element_start(&self, i: usize) -> Option<usize> {
        self.element_starts.get(i).map(|&o| o as usize)
    }

    pub fn element_end(&self, i: usize) -> Option<usize> {
        if i >= self.element_starts.len() {
            return None;
        }
        if i + 1 < self.element_starts.len() {
            Some(self.element_starts[i + 1] as usize)
        } else {
            Some(self.close as usize)
        }
    }

    pub fn starts(&self) -> &[u32] {
        &self.element_starts
    }
}

// ─── Object key map ─────────────────────────────────────────────────────────

/// Map of object keys (on-wire bytes between quotes) → value span.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectKeyIndex {
    /// Key content as stored between quotes (escaped form).
    entries: HashMap<Vec<u8>, (u32, u32)>,
    open: u32,
    close: u32,
}

impl ObjectKeyIndex {
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[inline]
    pub fn open(&self) -> usize {
        self.open as usize
    }

    #[inline]
    pub fn close(&self) -> usize {
        self.close as usize
    }

    /// Lookup by on-wire key bytes (as in a path segment / JSON key content).
    pub fn get(&self, key: &[u8]) -> Option<(usize, usize)> {
        self.entries
            .get(key)
            .map(|&(s, e)| (s as usize, e as usize))
    }

    /// All keys (on-wire form).
    pub fn keys(&self) -> impl Iterator<Item = &[u8]> {
        self.entries.keys().map(|k| k.as_slice())
    }
}

// ─── Indexed document ───────────────────────────────────────────────────────

/// A JSON buffer plus structural / array / object indexes for path-accelerated reads.
#[derive(Debug, Clone)]
pub struct IndexedDocument<'a> {
    json: &'a [u8],
    /// Optional Stage-1 list for the whole document.
    structural: Option<StructuralIndex>,
    arrays: Vec<(Vec<OwnedPathSegment>, ArrayIndex)>,
    objects: Vec<(Vec<OwnedPathSegment>, ObjectKeyIndex)>,
}

impl<'a> IndexedDocument<'a> {
    /// Empty index over `json` (no side-tables yet).
    pub fn empty(json: &'a [u8]) -> Self {
        Self {
            json,
            structural: None,
            arrays: Vec::new(),
            objects: Vec::new(),
        }
    }

    /// Build array side-tables for each path (lenient path parse).
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

    /// Build array side-tables from segment slices.
    pub fn build_paths(json: &'a [u8], array_paths: &[&[PathSegment<'_>]]) -> Result<Self, Error> {
        let mut doc = Self::empty(json);
        for p in array_paths {
            doc.index_array(p)?;
        }
        Ok(doc)
    }

    /// Build Stage-1 structural index only (no array/object tables yet).
    pub fn build_structural(json: &'a [u8]) -> Result<Self, Error> {
        let mut doc = Self::empty(json);
        doc.index_structural()?;
        Ok(doc)
    }

    /// Full convenience: structural + arrays + objects.
    pub fn build_full(
        json: &'a [u8],
        array_paths: &[&str],
        object_paths: &[&str],
    ) -> Result<Self, Error> {
        let mut doc = Self::empty(json);
        doc.index_structural()?;
        for p in array_paths {
            doc.index_array_str(p)?;
        }
        for p in object_paths {
            doc.index_object_str(p)?;
        }
        Ok(doc)
    }

    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.json
    }

    /// Build or replace the document-wide Stage-1 structural list.
    pub fn index_structural(&mut self) -> Result<(), Error> {
        self.structural = Some(build_structural_index(self.json)?);
        Ok(())
    }

    pub fn structural(&self) -> Option<&StructuralIndex> {
        self.structural.as_ref()
    }

    /// Index the array at `path`.
    pub fn index_array(&mut self, path: &[PathSegment]) -> Result<(), Error> {
        let owned = path_to_owned(path);
        self.arrays.retain(|(p, _)| p != &owned);
        let table = build_array_index(self.json, path, self.structural.as_ref())?;
        self.arrays.push((owned, table));
        Ok(())
    }

    pub fn index_array_str(&mut self, path: &str) -> Result<(), Error> {
        let segs = crate::path::parse_path(path);
        self.index_array(&segs)
    }

    /// Index all keys of the object at `path` (value must be `{...}`).
    pub fn index_object(&mut self, path: &[PathSegment]) -> Result<(), Error> {
        let owned = path_to_owned(path);
        self.objects.retain(|(p, _)| p != &owned);
        let table = build_object_key_index(self.json, path, self.structural.as_ref())?;
        self.objects.push((owned, table));
        Ok(())
    }

    pub fn index_object_str(&mut self, path: &str) -> Result<(), Error> {
        let segs = crate::path::parse_path(path);
        self.index_object(&segs)
    }

    pub fn array_len(&self, path: &[PathSegment]) -> Option<usize> {
        self.lookup_array(path).map(|(_, t)| t.len())
    }

    pub fn array_index(&self, path: &[PathSegment]) -> Option<&ArrayIndex> {
        self.lookup_array(path).map(|(_, t)| t)
    }

    pub fn object_index(&self, path: &[PathSegment]) -> Option<&ObjectKeyIndex> {
        self.lookup_object(path).map(|(_, t)| t)
    }

    /// Find using array / object indexes when applicable; else normal path scan.
    pub fn find(&self, path: &[PathSegment]) -> Result<&'a [u8], Error> {
        let (start, end) = self.find_offsets(path)?;
        Ok(&self.json[start..end])
    }

    pub fn find_offsets(&self, path: &[PathSegment]) -> Result<(usize, usize), Error> {
        if let Some((rest_start, rest_path)) = self.try_accelerated_jump(path)? {
            return find_from_value(self.json, rest_start, rest_path);
        }
        find_value_offsets(self.json, path)
    }

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
            let end = skip_value_maybe_structural(self.json, start, self.structural.as_ref())?;
            f(i, &self.json[start..end])?;
        }
        Ok(())
    }

    pub fn for_each_element_str<F>(&self, array_path: &str, f: F) -> Result<(), Error>
    where
        F: FnMut(usize, &'a [u8]) -> Result<(), Error>,
    {
        let segs = crate::path::parse_path(array_path);
        self.for_each_element(&segs, f)
    }

    pub fn indexed_array_count(&self) -> usize {
        self.arrays.len()
    }

    pub fn indexed_object_count(&self) -> usize {
        self.objects.len()
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

    fn lookup_object(&self, path: &[PathSegment]) -> Option<(usize, &ObjectKeyIndex)> {
        for (i, (owned, table)) in self.objects.iter().enumerate() {
            if path_eq_owned(path, owned) {
                return Some((i, table));
            }
        }
        None
    }

    /// Array jump: `array_path + Index(i) + rest` → element start + rest.  
    /// Object jump: `object_path + Key(k) + rest` → value start + rest.
    fn try_accelerated_jump<'p>(
        &self,
        path: &'p [PathSegment<'p>],
    ) -> Result<Option<(usize, &'p [PathSegment<'p>])>, Error> {
        let mut best: Option<(usize, usize)> = None; // (prefix_len, value_start)

        for (owned, table) in &self.arrays {
            let plen = owned.len();
            if path.len() <= plen || !path_prefix_eq_owned(&path[..plen], owned) {
                continue;
            }
            let PathSegment::Index(idx) = path[plen] else {
                continue;
            };
            let Some(start) = table.element_start(idx) else {
                return Err(Error::IndexOutOfBounds { index: idx });
            };
            if best.is_none_or(|(bp, _)| plen > bp) {
                best = Some((plen + 1, start)); // consume the Index segment
            }
        }

        for (owned, table) in &self.objects {
            let plen = owned.len();
            if path.len() <= plen || !path_prefix_eq_owned(&path[..plen], owned) {
                continue;
            }
            let PathSegment::Key(k) = path[plen] else {
                continue;
            };
            let Some((vs, _ve)) = table.get(k.as_bytes()) else {
                // Key missing in map → treat as path not found (object was fully indexed).
                return Err(Error::PathNotFound);
            };
            if best.is_none_or(|(bp, _)| plen > bp) {
                best = Some((plen + 1, vs));
            }
        }

        if let Some((consumed, start)) = best {
            Ok(Some((start, &path[consumed..])))
        } else {
            Ok(None)
        }
    }
}

// ─── Builders ───────────────────────────────────────────────────────────────

/// Build an [`ArrayIndex`] for the array at `path`.
///
/// When `structural` is provided, container close is still taken from
/// `find_value_offsets`; element walk uses structural-accelerated `skip_value`
/// when beneficial.
pub fn build_array_index(
    json: &[u8],
    path: &[PathSegment],
    structural: Option<&StructuralIndex>,
) -> Result<ArrayIndex, Error> {
    let (start, end) = find_value_offsets(json, path)?;
    if start >= json.len() || byte_at(json, start)? != b'[' {
        return Err(Error::TypeMismatch {
            expected: "array",
            found: "primitive/object",
        });
    }
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
        pos = skip_value_maybe_structural(json, pos, structural)?;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF inside array",
            });
        }
        match json[pos] {
            b',' => pos = skip_whitespace(json, pos + 1),
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

/// Build an [`ObjectKeyIndex`] for the object at `path`.
pub fn build_object_key_index(
    json: &[u8],
    path: &[PathSegment],
    structural: Option<&StructuralIndex>,
) -> Result<ObjectKeyIndex, Error> {
    let (start, end) = find_value_offsets(json, path)?;
    if start >= json.len() || byte_at(json, start)? != b'{' {
        return Err(Error::TypeMismatch {
            expected: "object",
            found: "primitive/array",
        });
    }
    let close = end.saturating_sub(1);
    if close < start || json.get(close) != Some(&b'}') {
        return Err(Error::InvalidJsonSyntax {
            pos: end,
            msg: "Object value missing closing brace",
        });
    }

    let mut entries = HashMap::new();
    let mut pos = skip_whitespace(json, start + 1);
    if pos >= json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos,
            msg: "Unexpected EOF inside object",
        });
    }
    if json[pos] == b'}' {
        return Ok(ObjectKeyIndex {
            entries,
            open: start as u32,
            close: pos as u32,
        });
    }

    loop {
        pos = skip_whitespace(json, pos);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF inside object",
            });
        }
        if json[pos] == b'}' {
            return Ok(ObjectKeyIndex {
                entries,
                open: start as u32,
                close: pos as u32,
            });
        }
        if json[pos] != b'"' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected object key string",
            });
        }
        let key_start = pos + 1;
        let key_end = find_string_end(json, key_start)?;
        let key = json[key_start..key_end].to_vec();
        pos = key_end + 1;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] != b':' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected colon after object key",
            });
        }
        pos += 1;
        pos = skip_whitespace(json, pos);
        let val_start = pos;
        let val_end = skip_value_maybe_structural(json, val_start, structural)?;
        if val_start > u32::MAX as usize || val_end > u32::MAX as usize {
            return Err(Error::InvalidJsonSyntax {
                pos: val_start,
                msg: "Document too large for u32 structural index",
            });
        }
        entries.insert(key, (val_start as u32, val_end as u32));
        pos = skip_whitespace(json, val_end);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF inside object",
            });
        }
        match json[pos] {
            b',' => pos += 1,
            b'}' => {
                return Ok(ObjectKeyIndex {
                    entries,
                    open: start as u32,
                    close: pos as u32,
                });
            }
            _ => {
                return Err(Error::InvalidJsonSyntax {
                    pos,
                    msg: "Expected comma or closing brace in object",
                });
            }
        }
    }
}

fn skip_value_maybe_structural(
    json: &[u8],
    pos: usize,
    structural: Option<&StructuralIndex>,
) -> Result<usize, Error> {
    let pos = skip_whitespace(json, pos);
    if pos >= json.len() {
        return Err(Error::InvalidJsonSyntax {
            pos,
            msg: "Unexpected EOF",
        });
    }
    if let Some(st) = structural {
        if matches!(json[pos], b'{' | b'[') {
            // Prefer structural skip for containers when Stage-1 is available.
            return st.skip_container(json, pos);
        }
    }
    skip_value(json, pos)
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

/// Extract static array path prefixes from a field path for derive auto-index.
///
/// For `products[0].title` → `["products"]`.  
/// For `a.b[0].c` → `["a.b"]`.  
/// Dynamic nested arrays (`items[0].tags[1]`) only contribute the **static**
/// prefix before the first index (`items`).
pub fn static_array_prefixes_from_path(path: &str) -> Vec<String> {
    let segs = crate::path::parse_path(path);
    let mut out = Vec::new();
    let mut keys: Vec<&str> = Vec::new();
    for s in &segs {
        match s {
            PathSegment::Key(k) => keys.push(*k),
            PathSegment::Index(_) => {
                if !keys.is_empty() {
                    out.push(keys.join("."));
                }
                // Stop at first index for static auto-index (dynamic nested arrays
                // need per-element indexes built at runtime).
                break;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::path::parse_path;

    #[test]
    fn index_jump_mid_array() {
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
        assert_eq!(
            doc.find(&parse_path("products[250].id")).unwrap(),
            b"250"
        );
        assert_eq!(
            doc.find(&parse_path("products[499].title")).unwrap(),
            br#""item_499""#
        );
    }

    #[test]
    fn structural_skip_matches_skip_value() {
        let json = br#"{"a":[1,{"x":2},[3,4]],"b":true}"#;
        let st = build_structural_index(json).unwrap();
        assert!(!st.is_empty());
        // Skip the array value of "a"
        let (start, end) = find_value_offsets(json, &parse_path("a")).unwrap();
        assert_eq!(json[start], b'[');
        let via_st = st.skip_container(json, start).unwrap();
        let via_scan = skip_value(json, start).unwrap();
        assert_eq!(via_st, via_scan);
        assert_eq!(via_st, end);
    }

    #[test]
    fn object_key_map() {
        let json = br#"{"cfg":{"alpha":1,"beta":true,"gamma":"z"}}"#;
        let mut doc = IndexedDocument::empty(json);
        doc.index_object_str("cfg").unwrap();
        assert_eq!(doc.find(&parse_path("cfg.beta")).unwrap(), b"true");
        assert_eq!(doc.find(&parse_path("cfg.gamma")).unwrap(), br#""z""#);
        assert!(matches!(
            doc.find(&parse_path("cfg.missing")),
            Err(Error::PathNotFound)
        ));
        let oi = doc.object_index(&parse_path("cfg")).unwrap();
        assert_eq!(oi.len(), 3);
    }

    #[test]
    fn build_full_and_for_each() {
        let json = br#"{"a":[1,2,3],"o":{"k":9}}"#;
        let doc = IndexedDocument::build_full(json, &["a"], &["o"]).unwrap();
        assert!(doc.structural().is_some());
        assert_eq!(doc.indexed_array_count(), 1);
        assert_eq!(doc.indexed_object_count(), 1);
        let mut n = 0;
        doc.for_each_element(&parse_path("a"), |_, _| {
            n += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(n, 3);
        assert_eq!(doc.find(&parse_path("o.k")).unwrap(), b"9");
    }

    #[test]
    fn static_array_prefixes() {
        assert_eq!(
            static_array_prefixes_from_path("products[0].title"),
            vec!["products".to_string()]
        );
        assert_eq!(
            static_array_prefixes_from_path("a.b[0].c"),
            vec!["a.b".to_string()]
        );
        assert!(static_array_prefixes_from_path("plain.field").is_empty());
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
}
