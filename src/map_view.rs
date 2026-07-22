//! String-key object cursors — maps without `HashMap<String, Value>`.
//!
//! Roadmap dyn pocket: iterate / look up object members as typed values decoded
//! from spans. Keys are borrowed when unescaped; otherwise use owned decode.
//!
//! # Low-level notes
//!
//! * Lookup matches **on-wire** key content (bytes between quotes). Simple keys
//!   (`[A-Za-z0-9_./-…]` without `"` / `\` / controls) skip `escape_json_key`
//!   allocation and compare UTF-8 bytes directly.
//! * `each` / `each_str_key` are single-pass; prefer them over repeated `get` on
//!   large objects when visiting all members.
//!
//! ```
//! use jshift::{JsonDoc, MapView, TypedDoc};
//!
//! let doc = TypedDoc::from_slice(br#"{"scores":{"alice":10,"bob":20}}"#);
//! let map = MapView::<u64>::from_doc(&doc, "scores").unwrap();
//! assert_eq!(map.get("bob").unwrap(), 20);
//! let mut sum = 0u64;
//! map.each(|_k, v| {
//!     sum += v;
//!     Ok(())
//! }).unwrap();
//! assert_eq!(sum, 30);
//! ```

use crate::convert::FromJsonSlice;
use crate::error::Error;
use crate::path::try_parse_path;
use crate::scan::{find_object_member_offsets, find_value_offsets, skip_whitespace};
use crate::typed_doc::{JsonDoc, ObjectEntries, ObjectEntry};

/// Cursor over a JSON object whose values decode as `T: FromJsonSlice`.
#[derive(Clone, Copy, Debug)]
pub struct MapView<'a, T: FromJsonSlice> {
    bytes: &'a [u8],
    _marker: std::marker::PhantomData<T>,
}

/// True if the logical key needs JSON string escapes on the wire.
#[inline]
fn key_needs_escape(key: &str) -> bool {
    key.as_bytes()
        .iter()
        .any(|&b| b == b'"' || b == b'\\' || b < 0x20)
}

impl<'a, T: FromJsonSlice> MapView<'a, T> {
    /// Treat `bytes` as a JSON object.
    pub fn from_object_bytes(bytes: &'a [u8]) -> Result<Self, Error> {
        let start = skip_whitespace(bytes, 0);
        if start >= bytes.len() || bytes[start] != b'{' {
            return Err(Error::TypeMismatch {
                expected: "object",
                found: "primitive/array",
            });
        }
        let end = crate::scan::skip_value(bytes, start)?;
        Ok(Self {
            bytes: &bytes[start..end],
            _marker: std::marker::PhantomData,
        })
    }

    /// Open the object at `path` inside `doc`.
    ///
    /// Lifetime is tied to `doc`'s borrow of the underlying buffer (standard
    /// lending pattern: hold `&TypedDoc` for the map’s lifetime).
    pub fn from_doc(doc: &'a impl JsonDoc, path: &str) -> Result<Self, Error> {
        let json = doc.as_json_bytes();
        let segs = try_parse_path(path)?;
        let (start, end) = find_value_offsets(json, &segs)?;
        if start >= json.len() || json[start] != b'{' {
            return Err(Error::TypeMismatch {
                expected: "object",
                found: "primitive/array",
            });
        }
        Ok(Self {
            bytes: &json[start..end],
            _marker: std::marker::PhantomData,
        })
    }

    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// Lookup by logical key.
    #[inline]
    pub fn get(&self, key: &str) -> Result<T, Error> {
        let raw = self.get_raw(key)?;
        T::from_json_slice(raw).ok_or(Error::TypeMismatch {
            expected: std::any::type_name::<T>(),
            found: "invalid format",
        })
    }

    /// Raw value span for `key` (no decode).
    ///
    /// Fast path: keys without escapes compare as UTF-8 against on-wire content
    /// with **zero allocation**. Escaped keys allocate once via `escape_json_key`.
    pub fn get_raw(&self, key: &str) -> Result<&'a [u8], Error> {
        let (_k, vs, ve) = if key_needs_escape(key) {
            let escaped = crate::convert::escape_json_key(key);
            find_object_member_offsets(self.bytes, &[], escaped.as_bytes())?
        } else {
            find_object_member_offsets(self.bytes, &[], key.as_bytes())?
        };
        Ok(&self.bytes[vs..ve])
    }

    /// Whether the key exists.
    pub fn contains(&self, key: &str) -> Result<bool, Error> {
        match self.get_raw(key) {
            Ok(_) => Ok(true),
            Err(Error::PathNotFound) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Number of members (full scan).
    pub fn len(&self) -> Result<usize, Error> {
        let mut n = 0usize;
        for e in ObjectEntries::open(self.bytes)? {
            e?;
            n += 1;
        }
        Ok(n)
    }

    pub fn is_empty(&self) -> Result<bool, Error> {
        Ok(self.len()? == 0)
    }

    /// Stream each entry (key on-wire content + decoded value).
    pub fn each<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(ObjectEntry<'a>, T) -> Result<(), Error>,
    {
        for ent in ObjectEntries::open(self.bytes)? {
            let ent = ent?;
            let v = T::from_json_slice(ent.value).ok_or(Error::TypeMismatch {
                expected: std::any::type_name::<T>(),
                found: "invalid format",
            })?;
            f(ent, v)?;
        }
        Ok(())
    }

    /// Stream with unescaped key when possible (`key_str`).
    pub fn each_str_key<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(&str, T) -> Result<(), Error>,
    {
        self.each(|ent, v| {
            let k = ent.key_str()?;
            f(k, v)
        })
    }

    /// Materialize as `Vec<(String, T)>` (owned keys; collect policy: owned).
    pub fn collect_owned(&self) -> Result<Vec<(String, T)>, Error> {
        let mut out = Vec::new();
        for ent in ObjectEntries::open(self.bytes)? {
            let ent = ent?;
            let key = if let Ok(s) = ent.key_str() {
                s.to_string()
            } else {
                let mut lit = Vec::with_capacity(ent.key_raw.len() + 2);
                lit.push(b'"');
                lit.extend_from_slice(ent.key_raw);
                lit.push(b'"');
                crate::convert::from_json_string(&lit).ok_or(Error::TypeMismatch {
                    expected: "string key",
                    found: "invalid escape",
                })?
            };
            let v = T::from_json_slice(ent.value).ok_or(Error::TypeMismatch {
                expected: std::any::type_name::<T>(),
                found: "invalid format",
            })?;
            out.push((key, v));
        }
        Ok(out)
    }

    /// One object walk → hash table of unescaped keys for O(1) multi-get.
    ///
    /// Escaped keys (rare) are kept in a linear side table and still found via
    /// [`IndexedMapView::get`] (O(k) only for those).
    pub fn index(&self) -> Result<IndexedMapView<'a, T>, Error> {
        use std::collections::HashMap;
        let mut by_key = HashMap::new();
        let mut escaped = Vec::new();
        for ent in ObjectEntries::open(self.bytes)? {
            let ent = ent?;
            if let Ok(k) = ent.key_str() {
                by_key.insert(k, ent.value);
            } else {
                escaped.push((ent.key_raw, ent.value));
            }
        }
        Ok(IndexedMapView {
            by_key,
            escaped,
            _marker: std::marker::PhantomData,
        })
    }
}

/// [`MapView`] after [`MapView::index`] — O(1) lookup for unescaped keys.
#[derive(Clone, Debug)]
pub struct IndexedMapView<'a, T: FromJsonSlice> {
    by_key: std::collections::HashMap<&'a str, &'a [u8]>,
    /// On-wire key content + value for keys that contain escapes.
    escaped: Vec<(&'a [u8], &'a [u8])>,
    _marker: std::marker::PhantomData<T>,
}

impl<'a, T: FromJsonSlice> IndexedMapView<'a, T> {
    #[inline]
    pub fn len(&self) -> usize {
        self.by_key.len() + self.escaped.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// O(1) for simple keys; O(escaped) only if the key needed JSON escapes.
    pub fn get_raw(&self, key: &str) -> Result<&'a [u8], Error> {
        if let Some(v) = self.by_key.get(key) {
            return Ok(*v);
        }
        if key_needs_escape(key) {
            let escaped = crate::convert::escape_json_key(key);
            for (k, v) in &self.escaped {
                if *k == escaped.as_bytes() {
                    return Ok(*v);
                }
            }
        }
        Err(Error::PathNotFound)
    }

    #[inline]
    pub fn get(&self, key: &str) -> Result<T, Error> {
        let raw = self.get_raw(key)?;
        T::from_json_slice(raw).ok_or(Error::TypeMismatch {
            expected: std::any::type_name::<T>(),
            found: "invalid format",
        })
    }

    #[inline]
    pub fn contains(&self, key: &str) -> bool {
        self.get_raw(key).is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typed_doc::TypedDoc;

    #[test]
    fn map_get_and_collect() {
        let doc = TypedDoc::from_slice(br#"{"m":{"x":1,"y":2}}"#);
        let m = MapView::<u64>::from_doc(&doc, "m").unwrap();
        assert_eq!(m.get("x").unwrap(), 1);
        assert!(m.contains("y").unwrap());
        assert!(!m.contains("z").unwrap());
        let pairs = m.collect_owned().unwrap();
        assert_eq!(pairs, vec![("x".into(), 1), ("y".into(), 2)]);
    }

    #[test]
    fn indexed_map_multi_get() {
        let doc = TypedDoc::from_slice(br#"{"m":{"a":1,"b":2,"c":3}}"#);
        let idx = MapView::<u64>::from_doc(&doc, "m").unwrap().index().unwrap();
        assert_eq!(idx.len(), 3);
        assert_eq!(idx.get("b").unwrap(), 2);
        assert!(idx.contains("c"));
        assert!(!idx.contains("z"));
    }
}
