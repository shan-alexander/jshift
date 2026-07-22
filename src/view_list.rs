//! Typed list cursors over JSON arrays — collections without a DOM.
//!
//! Roadmap L3: lists are streams. [`ViewList`] / [`ValueList`] feel like
//! collections (`len`, `get`, `iter`) but decode **one element at a time** from
//! raw spans. Call [`ViewList::collect_owned`] only when you need `Vec<T>`.
//!
//! ```
//! use jshift::{JsonDoc, JsonView, TypedDoc, ViewList, FromJsonSlice, parse_path, find_value};
//!
//! struct IdOnly { id: u64 }
//! impl JsonView for IdOnly {
//!     fn read_from(json: &[u8]) -> Result<Self, jshift::Error> {
//!         let s = find_value(json, &parse_path("id"))?;
//!         Ok(Self { id: u64::from_json_slice(s).unwrap() })
//!     }
//!     fn write_into(&self, json: &mut Vec<u8>) -> Result<(), jshift::Error> {
//!         jshift::upsert_at_path(json, &parse_path("id"), &self.id.to_json_bytes())
//!     }
//! }
//! use jshift::ToJsonBytes;
//!
//! let doc = TypedDoc::from_slice(br#"{"items":[{"id":1},{"id":2}]}"#);
//! let list: ViewList<'_, IdOnly> = doc.view_list("items").unwrap();
//! assert_eq!(list.len().unwrap(), 2);
//! assert_eq!(list.get(1).unwrap().id, 2);
//!
//! let owned = list.collect_owned().unwrap();
//! assert_eq!(owned[0].id, 1);
//! ```

use std::marker::PhantomData;

use crate::convert::FromJsonSlice;
use crate::error::Error;
use crate::path::{try_parse_path, PathSegment};
use crate::scan::{find_value_offsets, skip_value, skip_whitespace};
use crate::typed_doc::{ArrayElems, JsonDoc};
use crate::view::JsonView;

// ─── shared array span ───────────────────────────────────────────────────────

/// Borrowed JSON array bytes (`[...]`) used by list cursors.
#[derive(Clone, Copy, Debug)]
struct ArraySpan<'a> {
    bytes: &'a [u8],
}

impl<'a> ArraySpan<'a> {
    fn from_bytes(bytes: &'a [u8]) -> Result<Self, Error> {
        let start = skip_whitespace(bytes, 0);
        if start >= bytes.len() || bytes[start] != b'[' {
            return Err(Error::TypeMismatch {
                expected: "array",
                found: "primitive/object",
            });
        }
        let end = skip_value(bytes, start)?;
        // trim trailing whitespace outside the array is fine; require closed array
        if end == 0 || bytes[end - 1] != b']' {
            // skip_value on array ends after `]`
            return Err(Error::InvalidJsonSyntax {
                pos: end.saturating_sub(1),
                msg: "Expected array",
            });
        }
        Ok(Self {
            bytes: &bytes[start..end],
        })
    }

    fn from_doc_path(json: &'a [u8], path: &[PathSegment]) -> Result<Self, Error> {
        let (start, end) = find_value_offsets(json, path)?;
        if start >= json.len() || json[start] != b'[' {
            return Err(Error::TypeMismatch {
                expected: "array",
                found: "primitive/object",
            });
        }
        Ok(Self {
            bytes: &json[start..end],
        })
    }

    fn elems(&self) -> ArrayElems<'a> {
        // ArrayElems::open_span
        ArrayElems::open_span(self.bytes)
    }

    fn len(&self) -> Result<usize, Error> {
        let mut n = 0usize;
        for item in self.elems() {
            item?;
            n = n.checked_add(1).ok_or(Error::InvalidJsonSyntax {
                pos: 0,
                msg: "Array length overflow",
            })?;
        }
        Ok(n)
    }

    fn get_raw(&self, index: usize) -> Result<&'a [u8], Error> {
        for (i, item) in self.elems().enumerate() {
            let item = item?;
            if i == index {
                return Ok(item);
            }
        }
        Err(Error::IndexOutOfBounds { index })
    }

    /// One-pass collect of element spans for O(1) random access.
    fn index_elems(&self) -> Result<Vec<&'a [u8]>, Error> {
        let mut out = Vec::new();
        for item in self.elems() {
            out.push(item?);
        }
        Ok(out)
    }
}

// ─── ViewList ────────────────────────────────────────────────────────────────

/// Cursor over a JSON array of objects decoded as `T: JsonView`.
///
/// Does **not** allocate a `Vec<T>` until [`collect_owned`](Self::collect_owned).
/// `get(i)` is O(i) (walks preceding siblings); prefer sequential
/// [`iter`](Self::iter) / [`each`](Self::each) for full scans, or
/// [`index`](Self::index) for multi-get random access (O(n) prepare, O(1) get).
#[derive(Clone, Copy, Debug)]
pub struct ViewList<'a, T: JsonView> {
    span: ArraySpan<'a>,
    _marker: PhantomData<T>,
}

impl<'a, T: JsonView> ViewList<'a, T> {
    /// Treat `bytes` as a JSON array document (root `[...]` or a sub-array span).
    pub fn from_array_bytes(bytes: &'a [u8]) -> Result<Self, Error> {
        Ok(Self {
            span: ArraySpan::from_bytes(bytes)?,
            _marker: PhantomData,
        })
    }

    /// Open the array at `path` inside `doc`.
    pub fn from_doc(doc: &'a impl JsonDoc, path: &str) -> Result<Self, Error> {
        let segs = try_parse_path(path)?;
        Ok(Self {
            span: ArraySpan::from_doc_path(doc.as_json_bytes(), &segs)?,
            _marker: PhantomData,
        })
    }

    /// Raw array bytes (including `[` / `]`).
    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.span.bytes
    }

    /// Number of elements (full scan).
    #[inline]
    pub fn len(&self) -> Result<usize, Error> {
        self.span.len()
    }

    /// Whether the array is empty.
    pub fn is_empty(&self) -> Result<bool, Error> {
        Ok(self.len()? == 0)
    }

    /// Decode element `index` as `T` (O(index) sibling skip).
    pub fn get(&self, index: usize) -> Result<T, Error> {
        let raw = self.span.get_raw(index)?;
        T::read_from(raw)
    }

    /// Raw element span at `index`.
    pub fn get_raw(&self, index: usize) -> Result<&'a [u8], Error> {
        self.span.get_raw(index)
    }

    /// Fallible iterator decoding each element as `T`.
    pub fn iter(&self) -> ViewListIter<'a, T> {
        ViewListIter {
            elems: self.span.elems(),
            _marker: PhantomData,
        }
    }

    /// Call `f` for each decoded element (stream; no `Vec`).
    pub fn each<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(T) -> Result<(), Error>,
    {
        for item in self.iter() {
            f(item?)?;
        }
        Ok(())
    }

    /// Explicit materialization into `Vec<T>` (collect policy: owned).
    pub fn collect_owned(&self) -> Result<Vec<T>, Error> {
        let mut out = Vec::new();
        self.each(|v| {
            out.push(v);
            Ok(())
        })?;
        Ok(out)
    }

    /// Build a side-table of element spans (one array walk) for O(1) [`IndexedViewList::get`].
    ///
    /// Allocates `Vec` of borrowed spans only — still no `Vec<T>` until you decode.
    pub fn index(&self) -> Result<IndexedViewList<'a, T>, Error> {
        Ok(IndexedViewList {
            elems: self.span.index_elems()?,
            _marker: PhantomData,
        })
    }
}

/// [`ViewList`] after [`ViewList::index`] — O(1) element access by index.
///
/// Spans stay zero-copy into the parent buffer; only the offset table is owned.
#[derive(Clone, Debug)]
pub struct IndexedViewList<'a, T: JsonView> {
    elems: Vec<&'a [u8]>,
    _marker: PhantomData<T>,
}

impl<'a, T: JsonView> IndexedViewList<'a, T> {
    /// Number of elements (cached; no rescan).
    #[inline]
    pub fn len(&self) -> usize {
        self.elems.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.elems.is_empty()
    }

    /// Raw element span at `index` (O(1)).
    #[inline]
    pub fn get_raw(&self, index: usize) -> Result<&'a [u8], Error> {
        self.elems
            .get(index)
            .copied()
            .ok_or(Error::IndexOutOfBounds { index })
    }

    /// Decode element `index` as `T` (O(1) hop + decode).
    #[inline]
    pub fn get(&self, index: usize) -> Result<T, Error> {
        T::read_from(self.get_raw(index)?)
    }

    /// Borrow the span table.
    #[inline]
    pub fn as_slices(&self) -> &[&'a [u8]] {
        &self.elems
    }

    /// Iterate decoded elements (same order as the array).
    pub fn iter(&self) -> impl Iterator<Item = Result<T, Error>> + '_ {
        self.elems.iter().map(|raw| T::read_from(raw))
    }

    pub fn each<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(T) -> Result<(), Error>,
    {
        for raw in &self.elems {
            f(T::read_from(raw)?)?;
        }
        Ok(())
    }

    pub fn collect_owned(&self) -> Result<Vec<T>, Error> {
        let mut out = Vec::with_capacity(self.elems.len());
        self.each(|v| {
            out.push(v);
            Ok(())
        })?;
        Ok(out)
    }
}

/// Iterator yielding `Result<T, Error>` for each array element.
pub struct ViewListIter<'a, T: JsonView> {
    elems: ArrayElems<'a>,
    _marker: PhantomData<T>,
}

impl<'a, T: JsonView> Iterator for ViewListIter<'a, T> {
    type Item = Result<T, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.elems.next()? {
            Ok(raw) => Some(T::read_from(raw)),
            Err(e) => Some(Err(e)),
        }
    }
}

// ─── ValueList ───────────────────────────────────────────────────────────────

/// Cursor over a JSON array of values decoded via [`FromJsonSlice`].
///
/// Prefer this for primitive arrays (`[1,2,3]`, `["a","b"]`). For object cards,
/// use [`ViewList`].
#[derive(Clone, Copy, Debug)]
pub struct ValueList<'a, T: FromJsonSlice> {
    span: ArraySpan<'a>,
    _marker: PhantomData<T>,
}

impl<'a, T: FromJsonSlice> ValueList<'a, T> {
    /// Treat `bytes` as a JSON array.
    pub fn from_array_bytes(bytes: &'a [u8]) -> Result<Self, Error> {
        Ok(Self {
            span: ArraySpan::from_bytes(bytes)?,
            _marker: PhantomData,
        })
    }

    /// Open the array at `path` inside `doc`.
    pub fn from_doc(doc: &'a impl JsonDoc, path: &str) -> Result<Self, Error> {
        let segs = try_parse_path(path)?;
        Ok(Self {
            span: ArraySpan::from_doc_path(doc.as_json_bytes(), &segs)?,
            _marker: PhantomData,
        })
    }

    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.span.bytes
    }

    #[inline]
    pub fn len(&self) -> Result<usize, Error> {
        self.span.len()
    }

    pub fn is_empty(&self) -> Result<bool, Error> {
        Ok(self.len()? == 0)
    }

    pub fn get(&self, index: usize) -> Result<T, Error> {
        let raw = self.span.get_raw(index)?;
        T::from_json_slice(raw).ok_or(Error::TypeMismatch {
            expected: std::any::type_name::<T>(),
            found: "invalid format",
        })
    }

    pub fn get_raw(&self, index: usize) -> Result<&'a [u8], Error> {
        self.span.get_raw(index)
    }

    pub fn iter(&self) -> ValueListIter<'a, T> {
        ValueListIter {
            elems: self.span.elems(),
            _marker: PhantomData,
        }
    }

    pub fn each<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(T) -> Result<(), Error>,
    {
        for item in self.iter() {
            f(item?)?;
        }
        Ok(())
    }

    /// Explicit materialization into `Vec<T>`.
    pub fn collect_owned(&self) -> Result<Vec<T>, Error> {
        let mut out = Vec::new();
        self.each(|v| {
            out.push(v);
            Ok(())
        })?;
        Ok(out)
    }

    /// O(n) prepare → O(1) [`IndexedValueList::get`].
    pub fn index(&self) -> Result<IndexedValueList<'a, T>, Error> {
        Ok(IndexedValueList {
            elems: self.span.index_elems()?,
            _marker: PhantomData,
        })
    }
}

/// [`ValueList`] after [`ValueList::index`] — O(1) element access.
#[derive(Clone, Debug)]
pub struct IndexedValueList<'a, T: FromJsonSlice> {
    elems: Vec<&'a [u8]>,
    _marker: PhantomData<T>,
}

impl<'a, T: FromJsonSlice> IndexedValueList<'a, T> {
    #[inline]
    pub fn len(&self) -> usize {
        self.elems.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.elems.is_empty()
    }

    #[inline]
    pub fn get_raw(&self, index: usize) -> Result<&'a [u8], Error> {
        self.elems
            .get(index)
            .copied()
            .ok_or(Error::IndexOutOfBounds { index })
    }

    #[inline]
    pub fn get(&self, index: usize) -> Result<T, Error> {
        let raw = self.get_raw(index)?;
        T::from_json_slice(raw).ok_or(Error::TypeMismatch {
            expected: std::any::type_name::<T>(),
            found: "invalid format",
        })
    }

    #[inline]
    pub fn as_slices(&self) -> &[&'a [u8]] {
        &self.elems
    }

    pub fn collect_owned(&self) -> Result<Vec<T>, Error> {
        let mut out = Vec::with_capacity(self.elems.len());
        for raw in &self.elems {
            out.push(T::from_json_slice(raw).ok_or(Error::TypeMismatch {
                expected: std::any::type_name::<T>(),
                found: "invalid format",
            })?);
        }
        Ok(out)
    }
}

/// Iterator yielding `Result<T, Error>` for each primitive/array element.
pub struct ValueListIter<'a, T: FromJsonSlice> {
    elems: ArrayElems<'a>,
    _marker: PhantomData<T>,
}

impl<'a, T: FromJsonSlice> Iterator for ValueListIter<'a, T> {
    type Item = Result<T, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.elems.next()? {
            Ok(raw) => Some(
                T::from_json_slice(raw).ok_or(Error::TypeMismatch {
                    expected: std::any::type_name::<T>(),
                    found: "invalid format",
                }),
            ),
            Err(e) => Some(Err(e)),
        }
    }
}

// Convenience inherent methods (same crate) so users need not name ViewList::from_doc.
use crate::typed_doc::{TypedDoc, TypedDocRef};

impl TypedDoc {
    /// Open a [`ViewList`] of `T: JsonView` at `path`.
    #[inline]
    pub fn view_list<T: JsonView>(&self, path: &str) -> Result<ViewList<'_, T>, Error> {
        ViewList::from_doc(self, path)
    }

    /// Open a [`ValueList`] of `T: FromJsonSlice` at `path`.
    #[inline]
    pub fn value_list<T: FromJsonSlice>(&self, path: &str) -> Result<ValueList<'_, T>, Error> {
        ValueList::from_doc(self, path)
    }

    /// Root document is a JSON array of views (`[{...}, ...]`).
    #[inline]
    pub fn root_view_list<T: JsonView>(&self) -> Result<ViewList<'_, T>, Error> {
        ViewList::from_array_bytes(self.as_bytes())
    }

    /// Root document is a JSON array of values (`[1,2,3]`).
    #[inline]
    pub fn root_value_list<T: FromJsonSlice>(&self) -> Result<ValueList<'_, T>, Error> {
        ValueList::from_array_bytes(self.as_bytes())
    }
}

impl TypedDocRef<'_> {
    /// Open a [`ViewList`] of `T: JsonView` at `path`.
    #[inline]
    pub fn view_list<T: JsonView>(&self, path: &str) -> Result<ViewList<'_, T>, Error> {
        ViewList::from_doc(self, path)
    }

    /// Open a [`ValueList`] of `T: FromJsonSlice` at `path`.
    #[inline]
    pub fn value_list<T: FromJsonSlice>(&self, path: &str) -> Result<ValueList<'_, T>, Error> {
        ValueList::from_doc(self, path)
    }

    /// Root is a JSON array of views.
    #[inline]
    pub fn root_view_list<T: JsonView>(&self) -> Result<ViewList<'_, T>, Error> {
        ViewList::from_array_bytes(self.as_bytes())
    }

    /// Root is a JSON array of values.
    #[inline]
    pub fn root_value_list<T: FromJsonSlice>(&self) -> Result<ValueList<'_, T>, Error> {
        ValueList::from_array_bytes(self.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::ToJsonBytes;
    use crate::path::parse_path;
    use crate::scan::find_value;
    use crate::typed_doc::TypedDoc;
    use crate::upsert_at_path;

    struct Card {
        id: u64,
    }

    impl JsonView for Card {
        fn read_from(json: &[u8]) -> Result<Self, Error> {
            let s = find_value(json, &parse_path("id"))?;
            Ok(Self {
                id: u64::from_json_slice(s).ok_or(Error::TypeMismatch {
                    expected: "u64",
                    found: "bad",
                })?,
            })
        }
        fn write_into(&self, json: &mut Vec<u8>) -> Result<(), Error> {
            upsert_at_path(json, &parse_path("id"), &self.id.to_json_bytes())
        }
    }

    #[test]
    fn view_list_get_iter_collect() {
        let doc = TypedDoc::from_slice(br#"{"items":[{"id":1},{"id":2},{"id":3}]}"#);
        let list = ViewList::<Card>::from_doc(&doc, "items").unwrap();
        assert_eq!(list.len().unwrap(), 3);
        assert_eq!(list.get(0).unwrap().id, 1);
        assert_eq!(list.get(2).unwrap().id, 3);
        let ids: Vec<_> = list.iter().map(|r| r.unwrap().id).collect();
        assert_eq!(ids, vec![1, 2, 3]);
        assert_eq!(list.collect_owned().unwrap().len(), 3);
    }

    #[test]
    fn value_list_primitives() {
        let doc = TypedDoc::from_slice(br#"[10,20,30]"#);
        let list = ValueList::<u64>::from_array_bytes(doc.as_bytes()).unwrap();
        assert_eq!(list.collect_owned().unwrap(), vec![10, 20, 30]);
        assert_eq!(list.get(1).unwrap(), 20);
    }

    #[test]
    fn root_array_view_list() {
        let json = br#"[{"id":9}]"#;
        let list = ViewList::<Card>::from_array_bytes(json).unwrap();
        assert_eq!(list.get(0).unwrap().id, 9);
    }

    #[test]
    fn indexed_view_list_o1_get() {
        let doc = TypedDoc::from_slice(br#"{"items":[{"id":0},{"id":1},{"id":2},{"id":3}]}"#);
        let idx = doc.view_list::<Card>("items").unwrap().index().unwrap();
        assert_eq!(idx.len(), 4);
        assert_eq!(idx.get(3).unwrap().id, 3);
        assert_eq!(idx.get(0).unwrap().id, 0);
        assert!(matches!(
            idx.get(99),
            Err(Error::IndexOutOfBounds { index: 99 })
        ));
    }

    #[test]
    fn indexed_value_list() {
        let list = ValueList::<u64>::from_array_bytes(br#"[5,6,7]"#)
            .unwrap()
            .index()
            .unwrap();
        assert_eq!(list.get(1).unwrap(), 6);
        assert_eq!(list.collect_owned().unwrap(), vec![5, 6, 7]);
    }
}
