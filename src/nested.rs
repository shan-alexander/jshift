//! Nested views — typed subtrees without cloning the parent document.
//!
//! Roadmap 0.6: `view_at` is the primitive; [`NestedView`] is the ergonomic
//! cursor for “this span is a root for relative paths / child lists.”
//!
//! ```
//! use jshift::{JsonDoc, NestedView, TypedDoc, ViewList};
//!
//! let doc = TypedDoc::from_slice(
//!     br#"{"user":{"id":7,"tags":["a","b"]},"noise":true}"#,
//! );
//! let user = NestedView::from_doc(&doc, "user").unwrap();
//! assert_eq!(user.get::<u64>("id").unwrap(), 7);
//! let tags = user.value_list::<String>("tags").unwrap();
//! assert_eq!(tags.collect_owned().unwrap(), vec!["a".to_string(), "b".to_string()]);
//! ```

use crate::convert::FromJsonSlice;
use crate::error::Error;
use crate::path::try_parse_path;
use crate::typed_doc::{JsonDoc, TypedDocRef};
use crate::view::JsonView;
use crate::view_list::{ValueList, ViewList};
use crate::map_view::MapView;

/// Borrowed JSON subtree treated as a document root for relative access.
#[derive(Clone, Copy, Debug)]
pub struct NestedView<'a> {
    bytes: &'a [u8],
}

impl<'a> NestedView<'a> {
    /// Wrap a raw value span (object, array, or primitive).
    #[inline]
    pub fn from_span(bytes: &'a [u8]) -> Self {
        Self { bytes }
    }

    /// Resolve `path` on `doc` and borrow that span as a nested root.
    pub fn from_doc(doc: &'a impl JsonDoc, path: &str) -> Result<Self, Error> {
        let segs = try_parse_path(path)?;
        let slice = doc.get_raw_path(&segs)?;
        Ok(Self::from_span(slice))
    }

    /// Raw subtree bytes.
    #[inline]
    pub fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }

    /// [`TypedDocRef`] over this span (`JsonDoc` methods).
    #[inline]
    pub fn as_doc(&self) -> TypedDocRef<'a> {
        TypedDocRef::from_slice(self.bytes)
    }

    /// Decode the entire subtree as `T: JsonView`.
    #[inline]
    pub fn view<T: JsonView>(&self) -> Result<T, Error> {
        T::read_from(self.bytes)
    }

    /// Relative path get (same as `as_doc().get`).
    #[inline]
    pub fn get<T: FromJsonSlice>(&self, path: &str) -> Result<T, Error> {
        self.as_doc().get(path)
    }

    /// Relative raw span.
    #[inline]
    pub fn get_raw(&self, path: &str) -> Result<&'a [u8], Error> {
        // Lifetime: get_raw returns tied to self.bytes via TypedDocRef
        let segs = try_parse_path(path)?;
        crate::scan::find_value(self.bytes, &segs)
    }

    /// Nested object/array one level deeper.
    pub fn nest(&self, path: &str) -> Result<NestedView<'a>, Error> {
        Ok(NestedView::from_span(self.get_raw(path)?))
    }

    /// Child array of views at a relative path.
    pub fn view_list<T: JsonView>(&self, path: &str) -> Result<ViewList<'a, T>, Error> {
        ViewList::from_array_bytes(self.get_raw(path)?)
    }

    /// Child array of values at a relative path.
    pub fn value_list<T: FromJsonSlice>(&self, path: &str) -> Result<ValueList<'a, T>, Error> {
        ValueList::from_array_bytes(self.get_raw(path)?)
    }

    /// Child object as a string-key map of `T` values.
    pub fn map<T: FromJsonSlice>(&self, path: &str) -> Result<MapView<'a, T>, Error> {
        MapView::from_object_bytes(self.get_raw(path)?)
    }

    /// This span is itself a JSON object map of `T` values.
    pub fn as_map<T: FromJsonSlice>(&self) -> Result<MapView<'a, T>, Error> {
        MapView::from_object_bytes(self.bytes)
    }

    /// This span is itself a view-list array.
    pub fn as_view_list<T: JsonView>(&self) -> Result<ViewList<'a, T>, Error> {
        ViewList::from_array_bytes(self.bytes)
    }

    /// This span is itself a value-list array.
    pub fn as_value_list<T: FromJsonSlice>(&self) -> Result<ValueList<'a, T>, Error> {
        ValueList::from_array_bytes(self.bytes)
    }
}

impl<'a> JsonDoc for NestedView<'a> {
    #[inline]
    fn as_json_bytes(&self) -> &[u8] {
        self.bytes
    }
}

impl<'a> From<TypedDocRef<'a>> for NestedView<'a> {
    #[inline]
    fn from(d: TypedDocRef<'a>) -> Self {
        Self::from_span(d.as_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typed_doc::TypedDoc;

    #[test]
    fn nest_user_tags() {
        let doc = TypedDoc::from_slice(br#"{"user":{"id":7,"tags":["a","b"]}}"#);
        let user = NestedView::from_doc(&doc, "user").unwrap();
        assert_eq!(user.get::<u64>("id").unwrap(), 7);
        assert_eq!(
            user.value_list::<String>("tags")
                .unwrap()
                .collect_owned()
                .unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
        let deeper = user.nest("tags").unwrap();
        assert_eq!(deeper.as_value_list::<String>().unwrap().get(1).unwrap(), "b");
    }
}
