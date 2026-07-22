//! Batch / multi-op mutation plans.
//!
//! Roadmap track C: apply a sequence of path ops through one exclusive buffer
//! borrow. Ops run **in order** (later ops see earlier splices). Prefer packing
//! deletes toward the **end** of the document when you control key order — each
//! delete still memmoves the tail.
//!
//! ```
//! use jshift::{TypedDoc, MutateOp, JsonDoc};
//!
//! let mut doc = TypedDoc::from_slice(br#"{"a":1,"b":2,"c":3}"#);
//! doc.apply_ops(&[
//!     MutateOp::set("a", &10u64),
//!     MutateOp::delete("b"),
//!     MutateOp::upsert("d", &4u64),
//! ]).unwrap();
//! assert_eq!(doc.get::<u64>("a").unwrap(), 10);
//! assert!(!doc.contains("b").unwrap());
//! assert_eq!(doc.get::<u64>("d").unwrap(), 4);
//! ```

use crate::convert::ToJsonBytes;
use crate::error::Error;
use crate::mutate::{
    delete_index, delete_key, merge_object_shallow, mutate_value, rename_key, upsert_at_path,
};
use crate::path::{try_parse_path, Path, PathSegment};
use crate::typed_doc::TypedDoc;

/// One mutation in a batch plan.
#[derive(Clone, Debug)]
pub enum MutateOp {
    /// Overwrite an existing path (fails if missing).
    Set {
        path: String,
        value: Vec<u8>,
    },
    /// Insert or overwrite a path (creates parents as needed).
    Upsert {
        path: String,
        value: Vec<u8>,
    },
    /// Delete object member or array element at path.
    Delete {
        path: String,
    },
    /// Rename a key on the object at `object_path` (`""` = root).
    Rename {
        object_path: String,
        from: String,
        to: String,
    },
    /// Shallow-merge a patch object into the object at `path`.
    MergeShallow {
        path: String,
        patch: Vec<u8>,
    },
}

impl MutateOp {
    /// [`Set`](Self::Set) with a typed value.
    pub fn set(path: impl Into<String>, value: &(impl ToJsonBytes + ?Sized)) -> Self {
        Self::Set {
            path: path.into(),
            value: value.to_json_bytes(),
        }
    }

    /// [`Upsert`](Self::Upsert) with a typed value.
    pub fn upsert(path: impl Into<String>, value: &(impl ToJsonBytes + ?Sized)) -> Self {
        Self::Upsert {
            path: path.into(),
            value: value.to_json_bytes(),
        }
    }

    /// [`Delete`](Self::Delete) path.
    pub fn delete(path: impl Into<String>) -> Self {
        Self::Delete { path: path.into() }
    }

    /// [`Rename`](Self::Rename) key on object at `object_path` (empty = root).
    pub fn rename(
        object_path: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Self {
        Self::Rename {
            object_path: object_path.into(),
            from: from.into(),
            to: to.into(),
        }
    }

    /// [`MergeShallow`](Self::MergeShallow) patch object into path.
    pub fn merge_shallow(path: impl Into<String>, patch: impl Into<Vec<u8>>) -> Self {
        Self::MergeShallow {
            path: path.into(),
            patch: patch.into(),
        }
    }
}

/// Apply `ops` in order to a raw buffer.
pub fn apply_ops(json: &mut Vec<u8>, ops: &[MutateOp]) -> Result<(), Error> {
    for op in ops {
        apply_one(json, op)?;
    }
    Ok(())
}

fn apply_one(json: &mut Vec<u8>, op: &MutateOp) -> Result<(), Error> {
    match op {
        MutateOp::Set { path, value } => {
            let segs = try_parse_path(path)?;
            mutate_value(json, &segs, value)
        }
        MutateOp::Upsert { path, value } => {
            let segs = try_parse_path(path)?;
            upsert_at_path(json, &segs, value)
        }
        MutateOp::Delete { path } => {
            let segs = try_parse_path(path)?;
            delete_at_path(json, &segs)
        }
        MutateOp::Rename {
            object_path,
            from,
            to,
        } => {
            let segs = if object_path.is_empty() {
                Vec::new()
            } else {
                try_parse_path(object_path)?
            };
            rename_key(json, &segs, from, to)
        }
        MutateOp::MergeShallow { path, patch } => {
            let segs = if path.is_empty() {
                Vec::new()
            } else {
                try_parse_path(path)?
            };
            merge_object_shallow(json, &segs, patch)
        }
    }
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

/// Fluent collector for [`MutateOp`]s, then [`commit`](Self::commit) onto a buffer.
#[derive(Clone, Debug, Default)]
pub struct BatchPlan {
    ops: Vec<MutateOp>,
}

impl BatchPlan {
    pub fn new() -> Self {
        Self { ops: Vec::new() }
    }

    pub fn set(mut self, path: impl Into<String>, value: &(impl ToJsonBytes + ?Sized)) -> Self {
        self.ops.push(MutateOp::set(path, value));
        self
    }

    pub fn upsert(mut self, path: impl Into<String>, value: &(impl ToJsonBytes + ?Sized)) -> Self {
        self.ops.push(MutateOp::upsert(path, value));
        self
    }

    pub fn delete(mut self, path: impl Into<String>) -> Self {
        self.ops.push(MutateOp::delete(path));
        self
    }

    pub fn rename(
        mut self,
        object_path: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Self {
        self.ops.push(MutateOp::rename(object_path, from, to));
        self
    }

    pub fn merge_shallow(mut self, path: impl Into<String>, patch: impl Into<Vec<u8>>) -> Self {
        self.ops.push(MutateOp::merge_shallow(path, patch));
        self
    }

    pub fn push(mut self, op: MutateOp) -> Self {
        self.ops.push(op);
        self
    }

    pub fn ops(&self) -> &[MutateOp] {
        &self.ops
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Apply all ops to `json`.
    pub fn commit(self, json: &mut Vec<u8>) -> Result<(), Error> {
        apply_ops(json, &self.ops)
    }

    /// Apply all ops to a [`TypedDoc`].
    pub fn commit_doc(self, doc: &mut TypedDoc) -> Result<(), Error> {
        apply_ops(doc.as_mut_vec(), &self.ops)
    }
}

/// Convenience: build from a pre-parsed [`Path`] set (avoids re-tokenize in hot loops).
pub fn set_at(json: &mut Vec<u8>, path: &Path, value: &[u8]) -> Result<(), Error> {
    mutate_value(json, &path.borrowed(), value)
}

pub fn upsert_at(json: &mut Vec<u8>, path: &Path, value: &[u8]) -> Result<(), Error> {
    upsert_at_path(json, &path.borrowed(), value)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typed_doc::{JsonDoc, TypedDoc};

    #[test]
    fn batch_set_delete_upsert() {
        let mut doc = TypedDoc::from_slice(br#"{"a":1,"b":2}"#);
        BatchPlan::new()
            .set("a", &9u64)
            .delete("b")
            .upsert("c", &3u64)
            .commit_doc(&mut doc)
            .unwrap();
        assert_eq!(doc.get::<u64>("a").unwrap(), 9);
        assert!(!doc.contains("b").unwrap());
        assert_eq!(doc.get::<u64>("c").unwrap(), 3);
    }

    #[test]
    fn batch_rename_and_merge() {
        let mut json = br#"{"old":1,"x":true}"#.to_vec();
        apply_ops(
            &mut json,
            &[
                MutateOp::rename("", "old", "new"),
                MutateOp::merge_shallow("", br#"{"y":2}"#.to_vec()),
            ],
        )
        .unwrap();
        let doc = TypedDoc::from_vec(json);
        assert_eq!(doc.get::<u64>("new").unwrap(), 1);
        assert!(!doc.contains("old").unwrap());
        assert_eq!(doc.get::<u64>("y").unwrap(), 2);
        assert!(doc.get::<bool>("x").unwrap());
    }
}
