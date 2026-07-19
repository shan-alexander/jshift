//! Schema projections over JSON bytes (prost-inspired trait surface).
//!
//! A [`JsonView`] is a **partial** Rust type: only the paths you name are read or
//! written. Everything else in the buffer is ignored on read and preserved on write
//! (open-document / "unknown fields" semantics).
//!
//! This is **not** a full JSON DOM and **not** a serde replacement. It is the single
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
}

/// Shared helper: read any [`JsonView`] from bytes.
#[inline]
pub fn read_view<T: JsonView>(json: &[u8]) -> Result<T, Error> {
    T::read_from(json)
}

/// Shared helper: write any [`JsonView`] into a buffer.
#[inline]
pub fn write_view<T: JsonView>(view: &T, json: &mut Vec<u8>) -> Result<(), Error> {
    view.write_into(json)
}
