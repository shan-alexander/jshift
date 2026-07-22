//! Typed documents over raw JSON bytes — the center of gravity for “typed without Value.”
//!
//! # Model
//!
//! ```text
//! bytes ──► TypedDoc / TypedDocRef  (impl JsonDoc)
//!             │
//!             ├─ get::<T>(path) / get_at(&Path)   decode only that span
//!             ├─ get_opt / contains               missing-friendly
//!             ├─ get_str / get_raw                zero-copy (die on mutate)
//!             ├─ view_at(path)                    nested TypedDocRef
//!             ├─ as_view::<V>()                   open JsonView projection
//!             ├─ elems / each_* / each_get        stream arrays (no Vec by default)
//!             └─ mutate()                         exclusive TypedMutator (splice)
//! ```
//!
//! Unknown fields stay unread bytes. Mutation is exclusive: while a
//! [`TypedMutator`] (or any `&mut TypedDoc`) is live, borrowed slices from
//! [`JsonDoc::get_raw`] / [`JsonDoc::get_str`] cannot exist. That is the borrow
//! law — enforced by the type system, not by epochs.
//!
//! # When jshift wins vs serde (from benches)
//!
//! | Workload | Typical edge |
//! | --- | --- |
//! | Sparse `get` on large doc | orders of magnitude (no tree) |
//! | Open in-place `mutate` | large × (splice vs parse+serialize) |
//! | Stream **one field** per array element (`each_get`) | solid × vs full parse |
//! | Materialize **all fields** of every element | serde typed decode can win — use sparse views |
//!
//! # Examples
//!
//! ```
//! use jshift::{JsonDoc, TypedDoc};
//!
//! let mut doc = TypedDoc::from_slice(
//!     br#"{"status":"ok","items":[{"id":1},{"id":2}],"noise":true}"#,
//! );
//!
//! assert_eq!(doc.get::<u64>("items[0].id").unwrap(), 1);
//! assert_eq!(doc.get_str("status").unwrap(), "ok");
//!
//! // Sparse field stream (path relative to each element, parsed once)
//! let ids: Vec<u64> = doc.collect_each_get("items", "id").unwrap();
//! assert_eq!(ids, vec![1, 2]);
//!
//! {
//!     let mut m = doc.mutate();
//!     m.set("status", "accepted").unwrap();
//! }
//! assert_eq!(doc.get_str("status").unwrap(), "accepted");
//! assert!(doc.contains("noise").unwrap());
//! ```

use crate::convert::{FromJsonSlice, ToJsonBytes};
use crate::error::Error;
use crate::mutate::{array_len, delete_index, delete_key, mutate_value, upsert_at_path};
use crate::path::{try_parse_path, Path, PathSegment};
use crate::scan::{
    find_string_end, find_value, find_value_offsets, skip_value, skip_whitespace,
};
use crate::view::JsonView;

// ─── JsonDoc trait ───────────────────────────────────────────────────────────

/// Shared typed **read** surface over any JSON byte buffer.
///
/// Implemented for [`TypedDoc`], [`TypedDocRef`], [`crate::SharedDocument`], and
/// raw `&[u8]` (via blanket impl on types that deref to bytes — see impls below).
///
/// Prefer this trait in generic pipelines:
///
/// ```
/// use jshift::JsonDoc;
///
/// fn status_ok(doc: &impl JsonDoc) -> bool {
///     doc.get_str("status").ok() == Some("ok")
/// }
/// ```
pub trait JsonDoc {
    /// Raw JSON bytes for this document (or subdocument).
    fn as_json_bytes(&self) -> &[u8];

    /// Locate a path and return the raw value span (zero-copy).
    fn get_raw(&self, path: &str) -> Result<&[u8], Error> {
        let segs = try_parse_path(path)?;
        self.get_raw_path(&segs)
    }

    /// Locate using pre-parsed path segments (hot loops / reused [`Path`]).
    fn get_raw_path(&self, path: &[PathSegment]) -> Result<&[u8], Error> {
        find_value(self.as_json_bytes(), path)
    }

    /// Locate using an owned [`Path`].
    fn get_raw_at(&self, path: &Path) -> Result<&[u8], Error> {
        self.get_raw_path(&path.borrowed())
    }

    /// Decode a path into an owned Rust value via [`FromJsonSlice`].
    fn get<T: FromJsonSlice>(&self, path: &str) -> Result<T, Error> {
        let segs = try_parse_path(path)?;
        self.get_path(&segs)
    }

    /// Decode using pre-parsed segments.
    fn get_path<T: FromJsonSlice>(&self, path: &[PathSegment]) -> Result<T, Error> {
        let slice = self.get_raw_path(path)?;
        decode_slice(slice)
    }

    /// Decode using an owned [`Path`].
    fn get_at<T: FromJsonSlice>(&self, path: &Path) -> Result<T, Error> {
        self.get_path(&path.borrowed())
    }

    /// Like [`get`](Self::get), but [`Error::PathNotFound`] → `Ok(None)`.
    ///
    /// Other errors (type mismatch, syntax) still propagate.
    fn get_opt<T: FromJsonSlice>(&self, path: &str) -> Result<Option<T>, Error> {
        match self.get(path) {
            Ok(v) => Ok(Some(v)),
            Err(Error::PathNotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `true` if the path exists (value span found), without decoding.
    fn contains(&self, path: &str) -> Result<bool, Error> {
        match self.get_raw(path) {
            Ok(_) => Ok(true),
            Err(Error::PathNotFound) => Ok(false),
            Err(Error::IndexOutOfBounds { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Zero-copy string content for a JSON string **without** escapes.
    ///
    /// Use [`get`](Self::get)`::<String>` when the payload may contain escapes.
    fn get_str(&self, path: &str) -> Result<&str, Error> {
        let segs = try_parse_path(path)?;
        self.get_str_path(&segs)
    }

    /// Zero-copy string using pre-parsed segments.
    fn get_str_path(&self, path: &[PathSegment]) -> Result<&str, Error> {
        let slice = self.get_raw_path(path)?;
        borrow_json_string(slice)
    }

    /// Zero-copy string using an owned [`Path`].
    fn get_str_at(&self, path: &Path) -> Result<&str, Error> {
        self.get_str_path(&path.borrowed())
    }

    /// RFC 6901 JSON Pointer decode (`"/a~1b/0"`).
    fn get_pointer<T: FromJsonSlice>(&self, pointer: &str) -> Result<T, Error> {
        let path = Path::from_json_pointer(pointer)?;
        self.get_at(&path)
    }

    /// Open-document [`JsonView`] projection of the whole buffer.
    fn as_view<V: JsonView>(&self) -> Result<V, Error> {
        V::read_from(self.as_json_bytes())
    }

    /// Borrow the value at `path` as a nested [`TypedDocRef`] (subtree as root).
    fn view_at(&self, path: &str) -> Result<TypedDocRef<'_>, Error> {
        let slice = self.get_raw(path)?;
        Ok(TypedDocRef::from_slice(slice))
    }

    /// Nested view using pre-parsed segments.
    fn view_at_path(&self, path: &[PathSegment]) -> Result<TypedDocRef<'_>, Error> {
        let slice = self.get_raw_path(path)?;
        Ok(TypedDocRef::from_slice(slice))
    }

    /// Number of elements in the array at `path`.
    fn array_len(&self, path: &str) -> Result<usize, Error> {
        let segs = try_parse_path(path)?;
        array_len(self.as_json_bytes(), &segs)
    }

    /// Iterator over raw element spans of the array at `path`.
    ///
    /// Yields `Result<&[u8], Error>` so a mid-stream syntax error is visible.
    fn elems(&self, path: &str) -> Result<ArrayElems<'_>, Error> {
        let segs = try_parse_path(path)?;
        ArrayElems::open(self.as_json_bytes(), &segs)
    }

    /// Stream each array element as a raw span into `f`.
    fn each_with<F>(&self, path: &str, mut f: F) -> Result<(), Error>
    where
        F: FnMut(&[u8]) -> Result<(), Error>,
    {
        for item in self.elems(path)? {
            f(item?)?;
        }
        Ok(())
    }

    /// Stream each array element decoded as `T: FromJsonSlice` (whole element).
    fn each_with_decoded<T, F>(&self, path: &str, mut f: F) -> Result<(), Error>
    where
        T: FromJsonSlice,
        F: FnMut(T) -> Result<(), Error>,
    {
        self.each_with(path, |elem| f(decode_slice(elem)?))
    }

    /// Stream each array element as a [`JsonView`].
    ///
    /// **Perf note:** decoding *every* field of every element approaches serde
    /// full-parse cost. Prefer [`each_get`](Self::each_get) for sparse cards.
    fn each_view_with<V, F>(&self, path: &str, mut f: F) -> Result<(), Error>
    where
        V: JsonView,
        F: FnMut(V) -> Result<(), Error>,
    {
        self.each_with(path, |elem| f(V::read_from(elem)?))
    }

    /// Stream a **relative** field from each array element (path parsed once).
    ///
    /// This is the high-leverage array path: e.g. all `products[].id` without
    /// building `Vec<Product>` or a DOM.
    fn each_get<T, F>(&self, array_path: &str, field_path: &str, mut f: F) -> Result<(), Error>
    where
        T: FromJsonSlice,
        F: FnMut(T) -> Result<(), Error>,
    {
        let field = try_parse_path(field_path)?;
        self.each_with(array_path, |elem| {
            let slice = find_value(elem, &field)?;
            f(decode_slice(slice)?)
        })
    }

    /// Collect array elements into `Vec<T>` (explicit materialize).
    fn collect_each<T: FromJsonSlice>(&self, path: &str) -> Result<Vec<T>, Error> {
        let mut out = Vec::new();
        self.each_with_decoded(path, |v: T| {
            out.push(v);
            Ok(())
        })?;
        Ok(out)
    }

    /// Collect a relative field from each array element.
    fn collect_each_get<T: FromJsonSlice>(
        &self,
        array_path: &str,
        field_path: &str,
    ) -> Result<Vec<T>, Error> {
        let mut out = Vec::new();
        self.each_get(array_path, field_path, |v: T| {
            out.push(v);
            Ok(())
        })?;
        Ok(out)
    }

    /// Collect array elements as views into `Vec<V>`.
    fn collect_each_view<V: JsonView>(&self, path: &str) -> Result<Vec<V>, Error> {
        let mut out = Vec::new();
        self.each_view_with(path, |v: V| {
            out.push(v);
            Ok(())
        })?;
        Ok(out)
    }

    // ── root kind / null–missing ─────────────────────────────────────────

    /// Classify the root JSON value (object, array, primitive, …).
    fn root_kind(&self) -> Result<RootKind, Error> {
        root_kind_of(self.as_json_bytes())
    }

    /// `true` when the root value is a JSON object.
    fn is_object(&self) -> Result<bool, Error> {
        Ok(self.root_kind()? == RootKind::Object)
    }

    /// `true` when the root value is a JSON array.
    fn is_array(&self) -> Result<bool, Error> {
        Ok(self.root_kind()? == RootKind::Array)
    }

    /// Stream root array elements (document is `[...]`).
    fn root_elems(&self) -> Result<ArrayElems<'_>, Error> {
        let bytes = self.as_json_bytes();
        let start = skip_whitespace(bytes, 0);
        if start >= bytes.len() || bytes[start] != b'[' {
            return Err(Error::TypeMismatch {
                expected: "array",
                found: "primitive/object",
            });
        }
        let end = crate::scan::skip_value(bytes, start)?;
        ArrayElems::open_range(bytes, start, end)
    }

    /// Presence of a path: missing, JSON `null`, or a real value.
    fn presence(&self, path: &str) -> Result<Presence, Error> {
        match self.get_raw(path) {
            Ok(slice) => {
                let start = skip_whitespace(slice, 0);
                let mut end = slice.len();
                while end > start && matches!(slice[end - 1], b' ' | b'\t' | b'\n' | b'\r') {
                    end -= 1;
                }
                if &slice[start..end] == b"null" {
                    Ok(Presence::Null)
                } else {
                    Ok(Presence::Value)
                }
            }
            Err(Error::PathNotFound) | Err(Error::IndexOutOfBounds { .. }) => {
                Ok(Presence::Missing)
            }
            Err(e) => Err(e),
        }
    }

    /// `true` if the path exists and is JSON `null`.
    fn is_null(&self, path: &str) -> Result<bool, Error> {
        Ok(self.presence(path)? == Presence::Null)
    }

    /// Decode a path, mapping **missing** and **null** to `Ok(None)`.
    ///
    /// Type/syntax errors still fail. Use when APIs treat absent and null the same.
    fn get_nullable<T: FromJsonSlice>(&self, path: &str) -> Result<Option<T>, Error> {
        match self.presence(path)? {
            Presence::Missing | Presence::Null => Ok(None),
            Presence::Value => self.get(path).map(Some),
        }
    }

    /// Iterate object members at the document root (`{"k":...}`).
    ///
    /// Yields `(key_raw, value_span)` where `key_raw` is the on-wire key content
    /// between quotes (no unescaping). Dynamic pocket without a DOM.
    fn object_entries(&self) -> Result<ObjectEntries<'_>, Error> {
        ObjectEntries::open(self.as_json_bytes())
    }

    /// Iterate object members at `path`.
    fn object_entries_at(&self, path: &str) -> Result<ObjectEntries<'_>, Error> {
        let segs = try_parse_path(path)?;
        let (start, end) = find_value_offsets(self.as_json_bytes(), &segs)?;
        ObjectEntries::open_range(self.as_json_bytes(), start, end)
    }
}

// ─── RootKind / Presence ─────────────────────────────────────────────────────

/// Kind of a JSON value at the document root (or any single-value span).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RootKind {
    Object,
    Array,
    String,
    Number,
    Bool,
    Null,
}

/// Whether a path is absent, JSON `null`, or a non-null value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Presence {
    /// Path not found (or index out of bounds).
    Missing,
    /// Path exists and the value is `null`.
    Null,
    /// Path exists with a non-null JSON value.
    Value,
}

/// One object member: on-wire key content + raw value span.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ObjectEntry<'a> {
    /// Key bytes between quotes (escaped form as stored; may contain `\\`, `\"`, …).
    pub key_raw: &'a [u8],
    /// Full JSON value span (object, array, string, number, bool, null).
    pub value: &'a [u8],
}

impl<'a> ObjectEntry<'a> {
    /// Borrow key as `&str` when it has no escapes and is valid UTF-8.
    pub fn key_str(&self) -> Result<&'a str, Error> {
        if self.key_raw.contains(&b'\\') {
            return Err(Error::TypeMismatch {
                expected: "unescaped key (decode manually for escapes)",
                found: "escaped key",
            });
        }
        std::str::from_utf8(self.key_raw).map_err(|_| Error::TypeMismatch {
            expected: "utf-8 key",
            found: "invalid utf-8",
        })
    }

    /// Decode the value with [`FromJsonSlice`].
    pub fn get<T: FromJsonSlice>(&self) -> Result<T, Error> {
        T::from_json_slice(self.value).ok_or(Error::TypeMismatch {
            expected: std::any::type_name::<T>(),
            found: "invalid format",
        })
    }

    /// Nested document over the value span.
    #[inline]
    pub fn as_doc(&self) -> TypedDocRef<'a> {
        TypedDocRef::from_slice(self.value)
    }
}

/// Fallible iterator over object key/value spans (DynObject cursor).
///
/// Created via [`JsonDoc::object_entries`] / [`JsonDoc::object_entries_at`].
#[derive(Debug, Clone)]
pub struct ObjectEntries<'a> {
    json: &'a [u8],
    pos: usize,
    end: usize,
    finished: bool,
}

impl<'a> ObjectEntries<'a> {
    /// Open over a whole buffer that is a JSON object (possibly with outer whitespace).
    pub fn open(bytes: &'a [u8]) -> Result<Self, Error> {
        let start = skip_whitespace(bytes, 0);
        if start >= bytes.len() || bytes[start] != b'{' {
            return Err(Error::TypeMismatch {
                expected: "object",
                found: "primitive/array",
            });
        }
        let end = skip_value(bytes, start)?;
        Self::open_range(bytes, start, end)
    }

    fn open_range(json: &'a [u8], start: usize, end: usize) -> Result<Self, Error> {
        if start >= json.len() || json[start] != b'{' {
            return Err(Error::TypeMismatch {
                expected: "object",
                found: "primitive/array",
            });
        }
        if end == 0 || json[end - 1] != b'}' {
            return Err(Error::InvalidJsonSyntax {
                pos: end.saturating_sub(1),
                msg: "Expected closing brace '}' for object",
            });
        }
        Ok(Self {
            json,
            pos: skip_whitespace(json, start + 1),
            end,
            finished: false,
        })
    }
}

impl<'a> Iterator for ObjectEntries<'a> {
    type Item = Result<ObjectEntry<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        self.pos = skip_whitespace(self.json, self.pos);
        if self.pos >= self.end {
            self.finished = true;
            return Some(Err(Error::InvalidJsonSyntax {
                pos: self.pos,
                msg: "Unexpected EOF inside object",
            }));
        }
        if self.json[self.pos] == b'}' {
            self.finished = true;
            return None;
        }
        if self.json[self.pos] != b'"' {
            self.finished = true;
            return Some(Err(Error::InvalidJsonSyntax {
                pos: self.pos,
                msg: "Expected object key string",
            }));
        }
        let key_start = self.pos + 1;
        let key_end = match find_string_end(self.json, key_start) {
            Ok(e) => e,
            Err(e) => {
                self.finished = true;
                return Some(Err(e));
            }
        };
        let key_raw = &self.json[key_start..key_end];
        self.pos = skip_whitespace(self.json, key_end + 1);
        if self.pos >= self.end || self.json[self.pos] != b':' {
            self.finished = true;
            return Some(Err(Error::InvalidJsonSyntax {
                pos: self.pos,
                msg: "Expected colon after object key",
            }));
        }
        self.pos = skip_whitespace(self.json, self.pos + 1);
        let val_start = self.pos;
        let val_end = match skip_value(self.json, val_start) {
            Ok(e) => e,
            Err(e) => {
                self.finished = true;
                return Some(Err(e));
            }
        };
        if val_end > self.end {
            self.finished = true;
            return Some(Err(Error::InvalidJsonSyntax {
                pos: val_end,
                msg: "Object value extends past object end",
            }));
        }
        let value = &self.json[val_start..val_end];
        self.pos = skip_whitespace(self.json, val_end);
        if self.pos >= self.end {
            self.finished = true;
            return Some(Err(Error::InvalidJsonSyntax {
                pos: self.pos,
                msg: "Unexpected EOF after object value",
            }));
        }
        if self.json[self.pos] == b',' {
            self.pos += 1;
        } else if self.json[self.pos] == b'}' {
            // close on next next()
        } else {
            self.finished = true;
            return Some(Err(Error::InvalidJsonSyntax {
                pos: self.pos,
                msg: "Expected comma ',' or closing brace '}'",
            }));
        }
        Some(Ok(ObjectEntry { key_raw, value }))
    }
}

fn root_kind_of(bytes: &[u8]) -> Result<RootKind, Error> {
    let pos = skip_whitespace(bytes, 0);
    if pos >= bytes.len() {
        return Err(Error::InvalidJsonSyntax {
            pos,
            msg: "Unexpected EOF",
        });
    }
    Ok(match bytes[pos] {
        b'{' => RootKind::Object,
        b'[' => RootKind::Array,
        b'"' => RootKind::String,
        b't' | b'f' => RootKind::Bool,
        b'n' => RootKind::Null,
        b'-' | b'0'..=b'9' => RootKind::Number,
        _ => {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected JSON value",
            })
        }
    })
}

// ─── ArrayElems iterator ─────────────────────────────────────────────────────

/// Fallible iterator over JSON array element spans.
///
/// Created via [`JsonDoc::elems`]. Each item is the raw bytes of one element
/// (object, array, or primitive) borrowed from the parent buffer.
#[derive(Debug, Clone)]
pub struct ArrayElems<'a> {
    json: &'a [u8],
    pos: usize,
    end: usize,
    finished: bool,
}

impl<'a> ArrayElems<'a> {
    fn open(json: &'a [u8], path: &[PathSegment]) -> Result<Self, Error> {
        let (start, end) = find_value_offsets(json, path)?;
        Self::open_range(json, start, end)
    }

    /// Open an iterator over a raw array value span (including `[` / `]`).
    pub(crate) fn open_span(array_bytes: &'a [u8]) -> Self {
        let start = 0;
        let end = array_bytes.len();
        let pos = skip_whitespace(array_bytes, start + 1);
        Self {
            json: array_bytes,
            pos,
            end,
            finished: false,
        }
    }

    fn open_range(json: &'a [u8], start: usize, end: usize) -> Result<Self, Error> {
        if start >= json.len() || json[start] != b'[' {
            return Err(Error::TypeMismatch {
                expected: "array",
                found: "primitive/object",
            });
        }
        if end == 0 || json[end - 1] != b']' {
            return Err(Error::InvalidJsonSyntax {
                pos: end.saturating_sub(1),
                msg: "Expected closing bracket ']' for array",
            });
        }
        let pos = skip_whitespace(json, start + 1);
        Ok(Self {
            json,
            pos,
            end,
            finished: false,
        })
    }
}

impl<'a> Iterator for ArrayElems<'a> {
    type Item = Result<&'a [u8], Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.finished {
            return None;
        }
        self.pos = skip_whitespace(self.json, self.pos);
        if self.pos >= self.end {
            self.finished = true;
            return Some(Err(Error::InvalidJsonSyntax {
                pos: self.pos,
                msg: "Unexpected EOF inside array",
            }));
        }
        if self.json[self.pos] == b']' {
            self.finished = true;
            return None;
        }
        let val_start = self.pos;
        let val_end = match skip_value(self.json, val_start) {
            Ok(e) => e,
            Err(e) => {
                self.finished = true;
                return Some(Err(e));
            }
        };
        if val_end > self.end {
            self.finished = true;
            return Some(Err(Error::InvalidJsonSyntax {
                pos: val_end,
                msg: "Array element extends past array end",
            }));
        }
        let item = &self.json[val_start..val_end];
        self.pos = skip_whitespace(self.json, val_end);
        if self.pos >= self.end {
            self.finished = true;
            return Some(Err(Error::InvalidJsonSyntax {
                pos: self.pos,
                msg: "Unexpected EOF after array element",
            }));
        }
        if self.json[self.pos] == b',' {
            self.pos += 1;
        } else if self.json[self.pos] == b']' {
            // consume closing on next next()
        } else {
            self.finished = true;
            return Some(Err(Error::InvalidJsonSyntax {
                pos: self.pos,
                msg: "Expected comma ',' or closing bracket ']'",
            }));
        }
        Some(Ok(item))
    }
}

// ─── TypedDoc (owned) ────────────────────────────────────────────────────────

/// Owned JSON buffer with typed path access and exclusive in-place mutation.
///
/// Prefer this as the single object you hold in pipelines instead of
/// `serde_json::Value`. Reads decode only the spans you ask for; writes splice
/// the same `Vec<u8>` and leave unmentioned keys intact (open document).
///
/// Read methods come from [`JsonDoc`]; mutation via [`TypedDoc::mutate`].
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct TypedDoc {
    bytes: Vec<u8>,
}

impl TypedDoc {
    /// Empty buffer (not valid JSON until you write something).
    #[inline]
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    /// Take ownership of a buffer without copying.
    #[inline]
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Copy `slice` into a new owned document.
    #[inline]
    pub fn from_slice(slice: &[u8]) -> Self {
        Self {
            bytes: slice.to_vec(),
        }
    }

    /// Borrow the raw bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Length in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Consume into the underlying buffer.
    #[inline]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Mutable access to the buffer (invalidates external indexes; prefer
    /// [`mutate`](Self::mutate) for path ops).
    #[inline]
    pub fn as_mut_vec(&mut self) -> &mut Vec<u8> {
        &mut self.bytes
    }

    /// Borrowed view of the same bytes.
    #[inline]
    pub fn as_ref_doc(&self) -> TypedDocRef<'_> {
        TypedDocRef::from_slice(&self.bytes)
    }

    /// Begin exclusive mutation of this document.
    ///
    /// Holds `&mut self` for the lifetime of the mutator, so any zero-copy
    /// borrows from [`JsonDoc`] reads cannot coexist with active mutation.
    #[inline]
    pub fn mutate(&mut self) -> TypedMutator<'_> {
        TypedMutator { doc: self }
    }

    /// Open-document write of a [`JsonView`] (only named paths are upserted).
    #[inline]
    pub fn write_view<V: JsonView>(&mut self, view: &V) -> Result<(), Error> {
        view.write_into(&mut self.bytes)
    }

    /// Nested subtree cursor at `path` (0.6).
    #[inline]
    pub fn nest(&self, path: &str) -> Result<crate::nested::NestedView<'_>, Error> {
        crate::nested::NestedView::from_doc(self, path)
    }

    /// Object-as-map cursor at `path` (0.6).
    #[inline]
    pub fn map_view<T: FromJsonSlice>(
        &self,
        path: &str,
    ) -> Result<crate::map_view::MapView<'_, T>, Error> {
        crate::map_view::MapView::from_doc(self, path)
    }

    /// Apply a batch of [`crate::MutateOp`]s (see [`crate::apply_ops`]).
    #[inline]
    pub fn apply_ops(&mut self, ops: &[crate::MutateOp]) -> Result<(), Error> {
        crate::batch::apply_ops(&mut self.bytes, ops)
    }

    /// Shallow-merge a patch object into the object at `path` (`""` = root).
    pub fn merge_shallow(&mut self, path: &str, patch: &[u8]) -> Result<(), Error> {
        let segs = if path.is_empty() {
            Vec::new()
        } else {
            try_parse_path(path)?
        };
        crate::mutate::merge_object_shallow(&mut self.bytes, &segs, patch)
    }

    /// RFC 7396 JSON Merge Patch at document root.
    #[inline]
    pub fn merge_patch(&mut self, patch: &[u8]) -> Result<(), Error> {
        crate::merge_patch::merge_patch(&mut self.bytes, patch)
    }

    /// RFC 7396 merge patch at `path` (`""` = root).
    pub fn merge_patch_at(&mut self, path: &str, patch: &[u8]) -> Result<(), Error> {
        let segs = if path.is_empty() {
            Vec::new()
        } else {
            try_parse_path(path)?
        };
        crate::merge_patch::merge_patch_at(&mut self.bytes, &segs, patch)
    }

    /// Reject oversized / over-deep documents ([`crate::Limits`]).
    #[inline]
    pub fn check_limits(&self, limits: &crate::limits::Limits) -> Result<(), Error> {
        crate::limits::check_document(self.as_bytes(), limits)
    }

    /// Build a new document by open-upserting `view` onto `{}`.
    ///
    /// Result contains only the paths the view writes (plus any parents created
    /// by upsert). Prefer [`crate::ObjectBuilder`] when you need full control of
    /// key order without a view type.
    ///
    /// ```
    /// # #[cfg(feature = "derive")] {
    /// use jshift::{JsonView, TypedDoc, JsonDoc};
    ///
    /// #[derive(JsonView)]
    /// struct Card {
    ///     #[json(path = "id")]
    ///     id: u64,
    ///     #[json(path = "title")]
    ///     title: String,
    /// }
    ///
    /// let doc = TypedDoc::from_view(&Card { id: 1, title: "Hat".into() }).unwrap();
    /// assert_eq!(doc.get::<u64>("id").unwrap(), 1);
    /// # }
    /// ```
    pub fn from_view<V: JsonView>(view: &V) -> Result<Self, Error> {
        let mut bytes = Vec::from(br#"{}"#);
        view.write_into(&mut bytes)?;
        Ok(Self::from_vec(bytes))
    }
}

impl JsonDoc for TypedDoc {
    #[inline]
    fn as_json_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

impl AsRef<[u8]> for TypedDoc {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl From<Vec<u8>> for TypedDoc {
    #[inline]
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_vec(bytes)
    }
}

impl From<&[u8]> for TypedDoc {
    #[inline]
    fn from(slice: &[u8]) -> Self {
        Self::from_slice(slice)
    }
}

impl From<TypedDoc> for Vec<u8> {
    #[inline]
    fn from(doc: TypedDoc) -> Self {
        doc.into_bytes()
    }
}

// ─── TypedDocRef (borrowed) ──────────────────────────────────────────────────

/// Borrowed, read-only typed document over `&[u8]`.
///
/// Same [`JsonDoc`] read API as [`TypedDoc`] without ownership or mutation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypedDocRef<'a> {
    bytes: &'a [u8],
}

impl<'a> TypedDocRef<'a> {
    /// Wrap an existing slice (no copy).
    #[inline]
    pub fn from_slice(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Borrow the raw bytes with the full lifetime `'a`.
    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Copy into an owned [`TypedDoc`].
    #[inline]
    pub fn to_owned(&self) -> TypedDoc {
        TypedDoc::from_slice(self.bytes)
    }
}

impl JsonDoc for TypedDocRef<'_> {
    #[inline]
    fn as_json_bytes(&self) -> &[u8] {
        self.bytes
    }
}

impl AsRef<[u8]> for TypedDocRef<'_> {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.bytes
    }
}

impl<'a> From<&'a [u8]> for TypedDocRef<'a> {
    #[inline]
    fn from(bytes: &'a [u8]) -> Self {
        Self::from_slice(bytes)
    }
}

impl<'a> From<&'a TypedDoc> for TypedDocRef<'a> {
    #[inline]
    fn from(doc: &'a TypedDoc) -> Self {
        doc.as_ref_doc()
    }
}

impl JsonDoc for Vec<u8> {
    #[inline]
    fn as_json_bytes(&self) -> &[u8] {
        self.as_slice()
    }
}

// Note: no `impl JsonDoc for [u8]` — it collides with slice’s inherent `get`.
// Wrap bytes with [`TypedDocRef::from_slice`] for the typed API.

// ─── TypedMutator ────────────────────────────────────────────────────────────

/// Exclusive mutator for a [`TypedDoc`].
///
/// Obtained via [`TypedDoc::mutate`]. While this exists, the type system
/// prevents zero-copy borrows into the same document — the mutate epoch /
/// borrow law for jshift typed documents.
pub struct TypedMutator<'a> {
    doc: &'a mut TypedDoc,
}

impl TypedMutator<'_> {
    /// Borrow the current bytes (read-only while mutator is active).
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        self.doc.as_bytes()
    }

    /// Direct access for advanced splice / free-function use.
    #[inline]
    pub fn as_mut_vec(&mut self) -> &mut Vec<u8> {
        self.doc.as_mut_vec()
    }

    /// Overwrite an existing path with raw JSON bytes (path must already exist).
    pub fn set_raw(&mut self, path: &str, value: &[u8]) -> Result<(), Error> {
        let segs = try_parse_path(path)?;
        mutate_value(&mut self.doc.bytes, &segs, value)
    }

    /// Overwrite an existing path with a typed value.
    ///
    /// Uses a scratch buffer + [`ToJsonBytes::write_json_bytes`] so string/bool
    /// values avoid a redundant intermediate when the impl overrides write.
    pub fn set(&mut self, path: &str, value: &(impl ToJsonBytes + ?Sized)) -> Result<(), Error> {
        let mut buf = Vec::new();
        value.write_json_bytes(&mut buf);
        self.set_raw(path, &buf)
    }

    /// Insert or update a path (creates missing object parents / keys as needed).
    pub fn upsert_raw(&mut self, path: &str, value: &[u8]) -> Result<(), Error> {
        let segs = try_parse_path(path)?;
        upsert_at_path(&mut self.doc.bytes, &segs, value)
    }

    /// Insert or update a path with a typed value.
    pub fn upsert(&mut self, path: &str, value: &(impl ToJsonBytes + ?Sized)) -> Result<(), Error> {
        let mut buf = Vec::new();
        value.write_json_bytes(&mut buf);
        self.upsert_raw(path, &buf)
    }

    /// Delete the member or array element at `path`.
    pub fn delete(&mut self, path: &str) -> Result<(), Error> {
        let segs = try_parse_path(path)?;
        delete_at_path(&mut self.doc.bytes, &segs)
    }

    /// Apply an open-document [`JsonView`] write.
    #[inline]
    pub fn write_view<V: JsonView>(&mut self, view: &V) -> Result<(), Error> {
        view.write_into(&mut self.doc.bytes)
    }

    /// Set using a pre-parsed [`Path`] (no re-tokenize).
    pub fn set_at_raw(&mut self, path: &Path, value: &[u8]) -> Result<(), Error> {
        mutate_value(&mut self.doc.bytes, &path.borrowed(), value)
    }

    /// Upsert using a pre-parsed [`Path`].
    pub fn upsert_at_raw(&mut self, path: &Path, value: &[u8]) -> Result<(), Error> {
        upsert_at_path(&mut self.doc.bytes, &path.borrowed(), value)
    }

    /// Set typed value at a pre-parsed [`Path`].
    pub fn set_at(&mut self, path: &Path, value: &(impl ToJsonBytes + ?Sized)) -> Result<(), Error> {
        let mut buf = Vec::new();
        value.write_json_bytes(&mut buf);
        self.set_at_raw(path, &buf)
    }

    /// Upsert typed value at a pre-parsed [`Path`].
    pub fn upsert_at(
        &mut self,
        path: &Path,
        value: &(impl ToJsonBytes + ?Sized),
    ) -> Result<(), Error> {
        let mut buf = Vec::new();
        value.write_json_bytes(&mut buf);
        self.upsert_at_raw(path, &buf)
    }

    /// Fluent: set then return `&mut self` for chaining (errors still `Result`).
    pub fn and_set(
        &mut self,
        path: &str,
        value: &(impl ToJsonBytes + ?Sized),
    ) -> Result<&mut Self, Error> {
        self.set(path, value)?;
        Ok(self)
    }

    /// Fluent upsert chain.
    pub fn and_upsert(
        &mut self,
        path: &str,
        value: &(impl ToJsonBytes + ?Sized),
    ) -> Result<&mut Self, Error> {
        self.upsert(path, value)?;
        Ok(self)
    }

    /// Rename a key on the object at `object_path` (`""` = root).
    pub fn rename_key(
        &mut self,
        object_path: &str,
        from: &str,
        to: &str,
    ) -> Result<(), Error> {
        let segs = if object_path.is_empty() {
            Vec::new()
        } else {
            try_parse_path(object_path)?
        };
        crate::mutate::rename_key(&mut self.doc.bytes, &segs, from, to)
    }

    /// Shallow-merge `patch` into the object at `path`.
    pub fn merge_shallow(&mut self, path: &str, patch: &[u8]) -> Result<(), Error> {
        let segs = if path.is_empty() {
            Vec::new()
        } else {
            try_parse_path(path)?
        };
        crate::mutate::merge_object_shallow(&mut self.doc.bytes, &segs, patch)
    }

    /// Apply batch ops through this exclusive mutator.
    pub fn apply_ops(&mut self, ops: &[crate::MutateOp]) -> Result<(), Error> {
        crate::batch::apply_ops(&mut self.doc.bytes, ops)
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

#[inline]
fn decode_slice<T: FromJsonSlice>(slice: &[u8]) -> Result<T, Error> {
    T::from_json_slice(slice).ok_or(Error::TypeMismatch {
        expected: std::any::type_name::<T>(),
        found: "invalid format",
    })
}

/// Borrow JSON string content when there are no escapes; else error.
fn borrow_json_string(slice: &[u8]) -> Result<&str, Error> {
    if slice.len() < 2 || slice[0] != b'"' || slice[slice.len() - 1] != b'"' {
        return Err(Error::TypeMismatch {
            expected: "string",
            found: "non-string",
        });
    }
    let content = &slice[1..slice.len() - 1];
    if content.contains(&b'\\') {
        return Err(Error::TypeMismatch {
            expected: "unescaped string (use get::<String> for escapes)",
            found: "escaped string",
        });
    }
    if content.iter().any(|&b| b < 0x20) {
        return Err(Error::TypeMismatch {
            expected: "string",
            found: "invalid string content",
        });
    }
    std::str::from_utf8(content).map_err(|_| Error::TypeMismatch {
        expected: "utf-8 string",
        found: "invalid utf-8",
    })
}

fn delete_at_path(json: &mut Vec<u8>, path: &[PathSegment]) -> Result<(), Error> {
    match path.split_last() {
        None => Err(Error::InvalidPath {
            msg: "Cannot delete empty path (whole document)",
        }),
        Some((PathSegment::Key(key), parent)) => delete_key(json, parent, key),
        Some((PathSegment::Index(index), parent)) => delete_index(json, parent, *index),
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{find_value, parse_path};

    #[test]
    fn get_and_get_str() {
        let doc = TypedDoc::from_slice(br#"{"status":"ok","n":42,"flag":true}"#);
        assert_eq!(doc.get::<u64>("n").unwrap(), 42);
        assert_eq!(doc.get::<bool>("flag").unwrap(), true);
        assert_eq!(doc.get_str("status").unwrap(), "ok");
        assert_eq!(doc.get_raw("n").unwrap(), b"42");
    }

    #[test]
    fn get_opt_and_contains() {
        let doc = TypedDoc::from_slice(br#"{"a":1}"#);
        assert_eq!(doc.get_opt::<u64>("a").unwrap(), Some(1));
        assert_eq!(doc.get_opt::<u64>("b").unwrap(), None);
        assert!(doc.contains("a").unwrap());
        assert!(!doc.contains("b").unwrap());
    }

    #[test]
    fn get_str_rejects_escapes() {
        let doc = TypedDoc::from_slice(br#"{"s":"a\"b"}"#);
        assert!(matches!(doc.get_str("s"), Err(Error::TypeMismatch { .. })));
        assert_eq!(doc.get::<String>("s").unwrap(), "a\"b");
    }

    #[test]
    fn each_get_and_elems() {
        let doc = TypedDoc::from_slice(br#"{"items":[{"id":10},{"id":20},{"id":30}]}"#);
        let ids: Vec<u64> = doc.collect_each_get("items", "id").unwrap();
        assert_eq!(ids, vec![10, 20, 30]);

        let mut n = 0;
        for el in doc.elems("items").unwrap() {
            let el = el.unwrap();
            assert!(find_value(el, &parse_path("id")).is_ok());
            n += 1;
        }
        assert_eq!(n, 3);
    }

    #[test]
    fn view_at_nested() {
        let doc = TypedDoc::from_slice(br#"{"user":{"id":7,"name":"x"},"z":1}"#);
        let user = doc.view_at("user").unwrap();
        assert_eq!(user.get::<u64>("id").unwrap(), 7);
        assert_eq!(user.get_str("name").unwrap(), "x");
    }

    #[test]
    fn path_reuse_and_pointer() {
        let doc = TypedDoc::from_slice(br#"{"a":{"b":9}}"#);
        let p = Path::parse("a.b");
        assert_eq!(doc.get_at::<u64>(&p).unwrap(), 9);
        assert_eq!(doc.get_pointer::<u64>("/a/b").unwrap(), 9);
    }

    #[test]
    fn mutate_chain_preserves_unknowns() {
        let mut doc = TypedDoc::from_slice(br#"{"status":"new","id":7,"extra":true}"#);
        {
            let mut m = doc.mutate();
            m.and_set("status", "accepted")
                .unwrap()
                .and_upsert("score", &99u64)
                .unwrap();
            m.delete("id").unwrap();
        }
        assert_eq!(doc.get_str("status").unwrap(), "accepted");
        assert_eq!(doc.get::<u64>("score").unwrap(), 99);
        assert!(doc.get_raw("id").is_err());
        assert_eq!(doc.get::<bool>("extra").unwrap(), true);
    }

    #[test]
    fn borrowed_ref_and_vec_trait() {
        let bytes = br#"{"a":1}"#.to_vec();
        assert_eq!(bytes.get::<u64>("a").unwrap(), 1);
        let r = TypedDocRef::from_slice(&bytes);
        assert_eq!(r.get::<u64>("a").unwrap(), 1);
        let mut owned = r.to_owned();
        owned.mutate().set("a", &2u64).unwrap();
        assert_eq!(owned.get::<u64>("a").unwrap(), 2);
        assert_eq!(bytes, br#"{"a":1}"#);
    }

    #[test]
    fn write_view_open() {
        struct IdOnly {
            id: u64,
        }
        impl JsonView for IdOnly {
            fn read_from(json: &[u8]) -> Result<Self, Error> {
                let slice = find_value(json, &parse_path("id"))?;
                Ok(Self {
                    id: decode_slice(slice)?,
                })
            }
            fn write_into(&self, json: &mut Vec<u8>) -> Result<(), Error> {
                upsert_at_path(json, &parse_path("id"), &self.id.to_json_bytes())
            }
        }

        let mut doc = TypedDoc::from_slice(br#"{"id":1,"keep":true}"#);
        let v = doc.as_view::<IdOnly>().unwrap();
        assert_eq!(v.id, 1);
        doc.write_view(&IdOnly { id: 9 }).unwrap();
        assert_eq!(doc.get::<u64>("id").unwrap(), 9);
        assert_eq!(doc.get::<bool>("keep").unwrap(), true);
    }

    #[test]
    fn delete_array_index() {
        let mut doc = TypedDoc::from_slice(br#"{"list":[10,20,30]}"#);
        doc.mutate().delete("list[1]").unwrap();
        let nums: Vec<u64> = doc.collect_each("list").unwrap();
        assert_eq!(nums, vec![10, 30]);
    }

    #[test]
    fn empty_array_each() {
        let doc = TypedDoc::from_slice(br#"{"list":[]}"#);
        let v: Vec<u64> = doc.collect_each("list").unwrap();
        assert!(v.is_empty());
        assert_eq!(doc.elems("list").unwrap().count(), 0);
    }

    #[test]
    fn type_mismatch_on_wrong_decode() {
        let doc = TypedDoc::from_slice(br#"{"x":"hi"}"#);
        assert!(matches!(
            doc.get::<u64>("x"),
            Err(Error::TypeMismatch { .. })
        ));
    }

    #[test]
    fn generic_json_doc_pipeline() {
        fn first_id(doc: &impl JsonDoc) -> Result<u64, Error> {
            doc.get("items[0].id")
        }
        let doc = TypedDoc::from_slice(br#"{"items":[{"id":3}]}"#);
        assert_eq!(first_id(&doc).unwrap(), 3);
        assert_eq!(first_id(&doc.as_ref_doc()).unwrap(), 3);
        let shared = crate::SharedDocument::from_slice(doc.as_bytes());
        assert_eq!(first_id(&shared).unwrap(), 3);
    }

    #[test]
    fn root_kind_and_root_array() {
        let obj = TypedDoc::from_slice(br#"{"a":1}"#);
        assert_eq!(obj.root_kind().unwrap(), RootKind::Object);
        assert!(obj.is_object().unwrap());

        let arr = TypedDoc::from_slice(br#"[1,2,3]"#);
        assert_eq!(arr.root_kind().unwrap(), RootKind::Array);
        assert!(arr.is_array().unwrap());
        let mut sum = 0u64;
        for el in arr.root_elems().unwrap() {
            sum += u64::from_json_slice(el.unwrap()).unwrap();
        }
        assert_eq!(sum, 6);
    }

    #[test]
    fn presence_and_get_nullable() {
        let doc = TypedDoc::from_slice(br#"{"a":1,"b":null}"#);
        assert_eq!(doc.presence("a").unwrap(), Presence::Value);
        assert_eq!(doc.presence("b").unwrap(), Presence::Null);
        assert_eq!(doc.presence("c").unwrap(), Presence::Missing);
        assert!(doc.is_null("b").unwrap());
        assert_eq!(doc.get_nullable::<u64>("a").unwrap(), Some(1));
        assert_eq!(doc.get_nullable::<u64>("b").unwrap(), None);
        assert_eq!(doc.get_nullable::<u64>("c").unwrap(), None);
        // get_opt: null is still a value for Option decode
        assert_eq!(doc.get_opt::<Option<u64>>("b").unwrap(), Some(None));
    }

    #[test]
    fn object_entries_cursor() {
        let doc = TypedDoc::from_slice(br#"{"id":7,"meta":{"x":1},"z":true}"#);
        let mut keys = Vec::new();
        for e in doc.object_entries().unwrap() {
            let e = e.unwrap();
            keys.push(e.key_str().unwrap().to_string());
        }
        assert_eq!(keys, vec!["id", "meta", "z"]);

        let meta = doc.object_entries_at("meta").unwrap().next().unwrap().unwrap();
        assert_eq!(meta.key_str().unwrap(), "x");
        assert_eq!(meta.get::<u64>().unwrap(), 1);
    }
}
