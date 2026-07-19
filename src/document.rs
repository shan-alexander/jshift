//! Buffer-centric shared documents (cheap clone, many readers).
//!
//! Inspired by prost’s buffer traits and `bytes::Bytes`: keep the **data**
//! shareable, keep **mutation** on owned `Vec<u8>`.
//!
//! ```
//! use jshift::{JsonMutatorSchema, JsonView, SharedDocument};
//! use std::sync::Arc;
//!
//! #[derive(JsonMutatorSchema)]
//! struct Card {
//!     #[json(path = "id")]
//!     id: u64,
//! }
//!
//! let doc = SharedDocument::from_slice(br#"{"id":1,"extra":true}"#);
//! let a = doc.clone();
//! let b = doc.clone();
//! assert_eq!(Card::read_from(a.as_bytes()).unwrap().id, 1);
//! assert_eq!(Card::read_from(b.as_bytes()).unwrap().id, 1);
//! let _shared: Arc<[u8]> = doc.into_arc();
//! ```

use std::sync::Arc;

use crate::error::Error;
use crate::index::IndexedDocument;
use crate::view::JsonView;

/// Cheaply cloneable, read-only JSON buffer.
///
/// Internally `Arc<[u8]>`: clone is an atomic refcount bump. Build one document,
/// share across threads / rayon tasks, each calling path finds or [`JsonView`]
/// reads. For mutation, copy out to `Vec<u8>` (or start from an owned buffer).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SharedDocument {
    data: Arc<[u8]>,
}

impl SharedDocument {
    /// Copy `slice` into a new shared buffer.
    #[inline]
    pub fn from_slice(slice: &[u8]) -> Self {
        Self {
            data: Arc::from(slice),
        }
    }

    /// Take ownership of a `Vec<u8>` without copying the payload.
    #[inline]
    pub fn from_vec(vec: Vec<u8>) -> Self {
        Self {
            data: Arc::from(vec),
        }
    }

    /// Borrow the JSON bytes.
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.data
    }

    /// Length in bytes.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Clone the inner `Arc` (same as `Clone` for the document).
    #[inline]
    pub fn arc(&self) -> Arc<[u8]> {
        Arc::clone(&self.data)
    }

    /// Consume into the inner `Arc`.
    #[inline]
    pub fn into_arc(self) -> Arc<[u8]> {
        self.data
    }

    /// Copy into an owned buffer for mutation.
    #[inline]
    pub fn to_vec(&self) -> Vec<u8> {
        self.data.to_vec()
    }

    /// Read a [`JsonView`] projection (linear paths).
    #[inline]
    pub fn read<T: JsonView>(&self) -> Result<T, Error> {
        T::read_from(self.as_bytes())
    }

    /// Read a [`JsonView`] with schema-guided indexing.
    #[inline]
    pub fn read_indexed<T: JsonView>(&self) -> Result<T, Error> {
        T::read_from_indexed(self.as_bytes())
    }

    /// Build array side-tables for the given paths over this buffer.
    #[inline]
    pub fn indexed(&self, array_paths: &[&str]) -> Result<IndexedDocument<'_>, Error> {
        IndexedDocument::build(self.as_bytes(), array_paths)
    }

    /// Structural + arrays convenience builder.
    #[inline]
    pub fn indexed_full(
        &self,
        array_paths: &[&str],
        object_paths: &[&str],
    ) -> Result<IndexedDocument<'_>, Error> {
        IndexedDocument::build_full(self.as_bytes(), array_paths, object_paths)
    }
}

impl AsRef<[u8]> for SharedDocument {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        self.as_bytes()
    }
}

impl From<Vec<u8>> for SharedDocument {
    #[inline]
    fn from(vec: Vec<u8>) -> Self {
        Self::from_vec(vec)
    }
}

impl From<&[u8]> for SharedDocument {
    #[inline]
    fn from(slice: &[u8]) -> Self {
        Self::from_slice(slice)
    }
}

impl From<Arc<[u8]>> for SharedDocument {
    #[inline]
    fn from(data: Arc<[u8]>) -> Self {
        Self { data }
    }
}
