//! Dynamic pocket: hold a JSON subtree as raw bytes (not a DOM).
//!
//! Rule from the roadmap: dynamic is a **typed field kind**, not the host
//! architecture. Use [`RawJson`] when a region is unschema’d; decode later with
//! [`JsonDoc`] / [`crate::JsonView`] on the span.

use crate::convert::{FromJsonSlice, ToJsonBytes};
use crate::error::Error;
use crate::scan::skip_whitespace;
use crate::typed_doc::{JsonDoc, TypedDocRef};
use crate::view::JsonView;

/// Owned raw JSON value bytes (object, array, string, number, bool, or null).
///
/// Preserves the on-wire form; no parse tree. Cheap to pass through open
/// documents and re-attach with mutators / builders.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct RawJson {
    bytes: Vec<u8>,
}

impl RawJson {
    /// Wrap owned bytes (caller ensures a single JSON value).
    #[inline]
    pub fn from_vec(bytes: Vec<u8>) -> Self {
        Self { bytes }
    }

    /// Copy a value span.
    #[inline]
    pub fn from_slice(slice: &[u8]) -> Self {
        Self {
            bytes: slice.to_vec(),
        }
    }

    /// Borrow the raw value bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Consume into bytes.
    #[inline]
    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Length of the value on the wire.
    #[inline]
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Read as a [`TypedDocRef`] (path ops relative to this subtree).
    #[inline]
    pub fn as_doc(&self) -> TypedDocRef<'_> {
        TypedDocRef::from_slice(&self.bytes)
    }

    /// Decode this subtree as a [`JsonView`].
    #[inline]
    pub fn as_view<V: JsonView>(&self) -> Result<V, Error> {
        V::read_from(&self.bytes)
    }

    /// Decode a path inside this subtree.
    #[inline]
    pub fn get<T: FromJsonSlice>(&self, path: &str) -> Result<T, Error> {
        self.as_doc().get(path)
    }
}

impl AsRef<[u8]> for RawJson {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl From<Vec<u8>> for RawJson {
    #[inline]
    fn from(bytes: Vec<u8>) -> Self {
        Self::from_vec(bytes)
    }
}

impl From<&[u8]> for RawJson {
    #[inline]
    fn from(slice: &[u8]) -> Self {
        Self::from_slice(slice)
    }
}

impl FromJsonSlice for RawJson {
    /// Copy the value span (including surrounding structure). Trims outer
    /// whitespace only.
    fn from_json_slice(slice: &[u8]) -> Option<Self> {
        let start = skip_whitespace(slice, 0);
        let mut end = slice.len();
        while end > start && matches!(slice[end - 1], b' ' | b'\t' | b'\n' | b'\r') {
            end -= 1;
        }
        if start >= end {
            return None;
        }
        Some(Self::from_slice(&slice[start..end]))
    }
}

impl ToJsonBytes for RawJson {
    fn to_json_bytes(&self) -> Vec<u8> {
        self.bytes.clone()
    }

    fn write_json_bytes(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.bytes);
    }
}

impl JsonDoc for RawJson {
    #[inline]
    fn as_json_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typed_doc::TypedDoc;

    #[test]
    fn raw_round_trip_and_nested_get() {
        let doc = TypedDoc::from_slice(br#"{"meta":{"x":1,"y":true},"id":7}"#);
        let raw: RawJson = doc.get("meta").unwrap();
        assert_eq!(raw.as_bytes(), br#"{"x":1,"y":true}"#);
        assert_eq!(raw.get::<u64>("x").unwrap(), 1);
        assert_eq!(raw.get::<bool>("y").unwrap(), true);
        // re-emit
        assert_eq!(raw.to_json_bytes(), br#"{"x":1,"y":true}"#);
    }
}
