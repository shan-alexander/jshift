//! Schema projections over JSON bytes (prost-inspired trait surface).
//!
//! A [`JsonView`] is a **partial** Rust type: only the paths you name are read or
//! written. Everything else in the buffer is ignored on read and preserved on write
//! (open-document / "unknown fields" semantics).
//!
//! This is **not** a full JSON DOM. Together with [`crate::TypedDoc`], it is the
//! protocol surface for "this Rust type talks to JSON bytes," enabling generic
//! pipelines:
//!
//! ```
//! use jshift::{
//!     find_value, parse_path, upsert_at_path, FromJsonSlice, JsonView, ToJsonBytes, Error,
//! };
//!
//! struct IdOnly {
//!     id: u64,
//! }
//!
//! impl JsonView for IdOnly {
//!     fn read_from(json: &[u8]) -> Result<Self, Error> {
//!         let slice = find_value(json, &parse_path("id"))?;
//!         let id = u64::from_json_slice(slice).ok_or(Error::TypeMismatch {
//!             expected: "u64",
//!             found: "invalid format",
//!         })?;
//!         Ok(Self { id })
//!     }
//!
//!     fn write_into(&self, json: &mut Vec<u8>) -> Result<(), Error> {
//!         upsert_at_path(json, &parse_path("id"), &self.id.to_json_bytes())
//!     }
//! }
//!
//! fn ingest<T: JsonView>(buf: &[u8]) -> Result<T, Error> {
//!     T::read_from(buf)
//! }
//!
//! let json = br#"{"id":1,"extra":true}"#;
//! let v: IdOnly = ingest(json).unwrap();
//! assert_eq!(v.id, 1);
//! ```
//!
//! With the `derive` feature (default), prefer `#[derive(JsonView)]` /
//! `JsonMutatorSchema` instead of hand-written impls.

use crate::error::Error;
use crate::index::IndexedDocument;
use crate::project::{project, ProjectPlan};

/// A Rust type that is a **projection** of JSON bytes along named paths.
///
/// # Open documents
///
/// Fields (and whole subtrees) you do not name are:
///
/// * **unread** on [`read_from`](Self::read_from): never validated or allocated;
/// * **preserved byte-for-byte** on [`write_into`](Self::write_into): only named
///   paths are upserted.
///
/// That is intentional and first-class: use partial "view structs" the way prost
/// users use messages, not 1:1 mirrors of every JSON key.
///
/// # Indexing
///
/// Prefer [`read_from_indexed`](Self::read_from_indexed) or
/// [`read_from_doc`](Self::read_from_doc) when paths cross large arrays and you have
/// already built (or can build) schema-guided side-tables.
///
/// Indexes bind to a fixed byte snapshot. After in-place mutate/delete, rebuild or
/// drop the index (see [`IndexedDocument`]).
pub trait JsonView: Sized {
    /// Read this projection from raw JSON bytes (linear path scans).
    fn read_from(json: &[u8]) -> Result<Self, Error>;

    /// Read using a schema-guided index plan (array side-tables + optional Stage-1).
    ///
    /// Default implementation falls back to [`read_from`](Self::read_from). Derive
    /// implementations build a minimal index from path analysis.
    fn read_from_indexed(json: &[u8]) -> Result<Self, Error> {
        Self::read_from(json)
    }

    /// Read using a pre-built [`IndexedDocument`] (reuses index across views/tasks).
    ///
    /// Default: linear scan of `doc.as_bytes()`.
    fn read_from_doc(doc: &IndexedDocument<'_>) -> Result<Self, Error> {
        Self::read_from(doc.as_bytes())
    }

    /// Upsert this projection's fields into `json`.
    ///
    /// Unmentioned paths in the buffer are left alone. Missing parents for named
    /// paths may be created (see [`crate::upsert_at_path`]).
    fn write_into(&self, json: &mut Vec<u8>) -> Result<(), Error>;

    /// Schema keep-list as a [`ProjectPlan`] (default: empty object selection).
    ///
    /// Derive implementations build this from `FIELD_PATHS` (including `[]` wildcards
    /// when present in path attributes).
    fn project_plan() -> ProjectPlan {
        ProjectPlan::from_paths(&[]).unwrap_or_else(|_| ProjectPlan::identity())
    }

    /// Project `json` down to this view's keep-list (new buffer).
    fn project_bytes(json: &[u8]) -> Result<Vec<u8>, Error> {
        project(json, &Self::project_plan())
    }

    /// Build a **new** document containing only this schema’s fields (0.6).
    ///
    /// Default implementation:
    /// 1. open-upsert onto a fresh `{}` via [`write_into`](Self::write_into);
    /// 2. [`project_bytes`](Self::project_bytes) to strip accidental parents /
    ///    keep only the schema plan.
    ///
    /// That is two passes but correct for nested paths. Derive can override with
    /// a single-pass field-order writer for flat schemas. Prefer this when you
    /// need a closed card without `serde_json::Value`.
    fn to_schema_bytes(&self) -> Result<Vec<u8>, Error> {
        // Small seed; write_into grows via upsert. Avoid double-zero of large bufs.
        let mut buf = Vec::with_capacity(64);
        buf.extend_from_slice(br#"{"#);
        buf.push(b'}');
        self.write_into(&mut buf)?;
        Self::project_bytes(&buf)
    }

    /// [`to_schema_bytes`](Self::to_schema_bytes) into a [`crate::TypedDoc`].
    fn to_schema_doc(&self) -> Result<crate::typed_doc::TypedDoc, Error> {
        Ok(crate::typed_doc::TypedDoc::from_vec(self.to_schema_bytes()?))
    }
}

/// Build schema-only bytes from any [`JsonView`] (roadmap build-from-schema).
#[inline]
pub fn build_schema_bytes<T: JsonView>(view: &T) -> Result<Vec<u8>, Error> {
    view.to_schema_bytes()
}

/// Stream each element of the array at `array_path` as `T: JsonView`.
///
/// Single-pass, no intermediate `Vec<T>`. Prefer over full deserialize when
/// processing rows one at a time.
///
/// ```
/// use jshift::{project_view_each, FromJsonSlice, JsonView, find_value, parse_path};
///
/// struct Id { n: u64 }
/// impl JsonView for Id {
///     fn read_from(json: &[u8]) -> Result<Self, jshift::Error> {
///         let s = find_value(json, &parse_path("id"))?;
///         Ok(Self { n: u64::from_json_slice(s).unwrap() })
///     }
///     fn write_into(&self, _: &mut Vec<u8>) -> Result<(), jshift::Error> { Ok(()) }
/// }
///
/// let json = br#"{"items":[{"id":1},{"id":2}]}"#;
/// let mut sum = 0u64;
/// project_view_each::<Id, _>(json, "items", |c| {
///     sum += c.n;
///     Ok(())
/// }).unwrap();
/// assert_eq!(sum, 3);
/// ```
pub fn project_view_each<T, F>(json: &[u8], array_path: &str, mut f: F) -> Result<(), Error>
where
    T: JsonView,
    F: FnMut(T) -> Result<(), Error>,
{
    use crate::view_list::ViewList;
    let doc = crate::typed_doc::TypedDocRef::from_slice(json);
    let list = ViewList::<T>::from_doc(&doc, array_path)?;
    list.each(|v| f(v))
}

/// Collect array elements as `Vec<T: JsonView>` (explicit owned collect).
#[inline]
pub fn project_view_collect<T: JsonView>(json: &[u8], array_path: &str) -> Result<Vec<T>, Error> {
    let mut out = Vec::new();
    project_view_each(json, array_path, |v| {
        out.push(v);
        Ok(())
    })?;
    Ok(out)
}

/// Shared helper: read any [`JsonView`] from bytes.
#[inline]
pub fn read_view<T: JsonView>(json: &[u8]) -> Result<T, Error> {
    T::read_from(json)
}

/// Happy-path alias: bytes → `T: JsonView` (roadmap adoption surface).
///
/// Same as [`read_view`] / [`JsonView::read_from`]. Prefer this name in
/// migration guides (`from_jshift_bytes` vs `serde_json::from_slice`).
#[inline]
pub fn from_jshift_bytes<T: JsonView>(json: &[u8]) -> Result<T, Error> {
    T::read_from(json)
}

/// Project `json` down to `T`'s keep-list, then decode as `T`.
///
/// Typed project → T (roadmap query/transform): thin bytes intermediate, no
/// `Value` tree. Useful when the source is huge but the view is sparse.
///
/// ```
/// # #[cfg(feature = "derive")] {
/// use jshift::{project_as_view, JsonView};
///
/// #[derive(JsonView)]
/// struct Card {
///     #[json(path = "id")]
///     id: u64,
/// }
///
/// let json = br#"{"id":7,"blob":[1,2,3],"title":"x"}"#;
/// let card: Card = project_as_view(json).unwrap();
/// assert_eq!(card.id, 7);
/// # }
/// ```
#[inline]
pub fn project_as_view<T: JsonView>(json: &[u8]) -> Result<T, Error> {
    let thin = T::project_bytes(json)?;
    T::read_from(&thin)
}

/// Shared helper: write any [`JsonView`] into a buffer.
#[inline]
pub fn write_view<T: JsonView>(view: &T, json: &mut Vec<u8>) -> Result<(), Error> {
    view.write_into(json)
}
