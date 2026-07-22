//! Explicit collect policies for typed lists (roadmap 0.6).
//!
//! Lists are **streams by default**. Materialization is always an explicit
//! method or [`CollectPolicy`] choice — never the silent `Vec<T>` trap of
//! `serde_json`.
//!
//! # Low-level notes
//!
//! * `each_*` / `collect_projected` walk the array **once** via [`ArrayElems`]
//!   — no intermediate `Vec<&[u8]>` of all spans.
//! * Prefer `each_field` over `collect_owned` when you only need one path per
//!   element (avoids full `JsonView` decode).
//! * `CollectPolicy::Stream` is intentionally a no-op materialize (forces the
//!   caller to use streaming APIs rather than “collect by habit”).
//!
//! | Policy | Result |
//! | --- | --- |
//! | [`CollectPolicy::Stream`] | Do not collect; use `each` / `iter` |
//! | [`CollectPolicy::Owned`] | `Vec<T>` fully decoded |
//! | [`CollectPolicy::Projected`] | `Vec<Vec<u8>>` thin cards via `T::project_bytes` |

use crate::error::Error;
use crate::view::JsonView;
use crate::view_list::ViewList;

/// How to materialize a [`ViewList`] (or similar cursor).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
pub enum CollectPolicy {
    /// Keep streaming; [`ViewList::collect`] returns [`Collected::Stream`].
    #[default]
    Stream,
    /// Decode every element into an owned `Vec<T>`.
    Owned,
    /// Project each element to the view’s keep-list (`T::project_bytes`).
    Projected,
}

/// Result of [`ViewList::collect`] under a [`CollectPolicy`].
#[derive(Debug, Clone)]
pub enum Collected<T> {
    /// No allocation of elements (caller should have used `each` / `iter`).
    Stream,
    /// Fully decoded elements.
    Owned(Vec<T>),
    /// Thin projected card buffers (one per element).
    Projected(Vec<Vec<u8>>),
}

impl<'a, T: JsonView> ViewList<'a, T> {
    /// Materialize according to `policy`.
    pub fn collect(&self, policy: CollectPolicy) -> Result<Collected<T>, Error> {
        match policy {
            CollectPolicy::Stream => Ok(Collected::Stream),
            CollectPolicy::Owned => Ok(Collected::Owned(self.collect_owned()?)),
            CollectPolicy::Projected => Ok(Collected::Projected(self.collect_projected()?)),
        }
    }

    /// Project each element to schema keep-list bytes — **one array walk**, no
    /// intermediate span table.
    pub fn collect_projected(&self) -> Result<Vec<Vec<u8>>, Error> {
        let mut out = Vec::new();
        self.for_each_raw(|raw| {
            out.push(T::project_bytes(raw)?);
            Ok(())
        })?;
        Ok(out)
    }

    /// Stream only elements for which `pred` returns true (still no `Vec` unless you push).
    ///
    /// Note: `pred` receives `&T` after decode; for field-only filters prefer
    /// [`each_field`](Self::each_field) then decide (cheaper).
    pub fn each_filtered<F, P>(&self, mut pred: P, mut f: F) -> Result<(), Error>
    where
        P: FnMut(&T) -> bool,
        F: FnMut(T) -> Result<(), Error>,
    {
        self.each(|item| {
            if pred(&item) {
                f(item)?;
            }
            Ok(())
        })
    }

    /// Decode only a relative field from each element (path parsed **once**).
    ///
    /// Single array walk; no `Vec` of spans and no full `JsonView` decode.
    pub fn each_field<U, F>(&self, field_path: &str, mut f: F) -> Result<(), Error>
    where
        U: crate::convert::FromJsonSlice,
        F: FnMut(U) -> Result<(), Error>,
    {
        let field = crate::path::try_parse_path(field_path)?;
        self.for_each_raw(|raw| {
            let slice = crate::scan::find_value(raw, &field)?;
            let v = U::from_json_slice(slice).ok_or(Error::TypeMismatch {
                expected: std::any::type_name::<U>(),
                found: "invalid format",
            })?;
            f(v)
        })
    }

    /// Collect a relative field from each element into `Vec<U>`.
    pub fn collect_field<U: crate::convert::FromJsonSlice>(
        &self,
        field_path: &str,
    ) -> Result<Vec<U>, Error> {
        let mut out = Vec::new();
        self.each_field(field_path, |v: U| {
            out.push(v);
            Ok(())
        })?;
        Ok(out)
    }

    /// Zero-copy walk of raw element spans (no intermediate table).
    #[inline]
    pub fn for_each_raw<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(&'a [u8]) -> Result<(), Error>,
    {
        for item in self.raw_elems() {
            f(item?)?;
        }
        Ok(())
    }

    /// For each element, open a [`NestedView`] and run `f` (one array walk).
    ///
    /// Prefer this over `get(i)` + nest when scanning the whole list.
    pub fn each_nested<F>(&self, mut f: F) -> Result<(), Error>
    where
        F: FnMut(crate::nested::NestedView<'a>) -> Result<(), Error>,
    {
        self.for_each_raw(|raw| f(crate::nested::NestedView::from_span(raw)))
    }

    /// Relative field under each element via nested path (e.g. `"meta.rank"`).
    ///
    /// Single array walk; path parsed once. Cheaper than full `JsonView` decode
    /// when you only need one nested scalar.
    pub fn each_nested_field<U, F>(&self, field_path: &str, mut f: F) -> Result<(), Error>
    where
        U: crate::convert::FromJsonSlice,
        F: FnMut(U) -> Result<(), Error>,
    {
        let field = crate::path::try_parse_path(field_path)?;
        self.for_each_raw(|raw| {
            let slice = crate::scan::find_value(raw, &field)?;
            let v = U::from_json_slice(slice).ok_or(Error::TypeMismatch {
                expected: std::any::type_name::<U>(),
                found: "invalid format",
            })?;
            f(v)
        })
    }

    /// Collect nested relative fields into `Vec<U>`.
    pub fn collect_nested_field<U: crate::convert::FromJsonSlice>(
        &self,
        field_path: &str,
    ) -> Result<Vec<U>, Error> {
        let mut out = Vec::new();
        self.each_nested_field(field_path, |v: U| {
            out.push(v);
            Ok(())
        })?;
        Ok(out)
    }

    /// Sum a numeric nested/relative field (`u64`) without building a `Vec`.
    pub fn sum_field_u64(&self, field_path: &str) -> Result<u64, Error> {
        let mut sum = 0u64;
        self.each_field(field_path, |v: u64| {
            sum = sum.wrapping_add(v);
            Ok(())
        })?;
        Ok(sum)
    }

    /// Sum a nested path as `u64` (e.g. `"meta.rank"`).
    pub fn sum_nested_field_u64(&self, field_path: &str) -> Result<u64, Error> {
        let mut sum = 0u64;
        self.each_nested_field(field_path, |v: u64| {
            sum = sum.wrapping_add(v);
            Ok(())
        })?;
        Ok(sum)
    }
}
