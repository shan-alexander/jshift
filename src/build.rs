//! Schema-free JSON **build** without a `Value` tree.
//!
//! Emits compact JSON into a single `Vec<u8>` by appending keys and values in
//! call order. Nested objects/arrays are written **in place** (one buffer, no
//! nested `Vec` + copy). Prefer this when you want safe, explicit construction
//! instead of `serde_json::Value` / `json!`.
//!
//! Open patch remains on [`crate::TypedDoc::mutate`]. Closed “from scratch”
//! emit is this module + [`crate::TypedDoc::from_view`].
//!
//! # Layers
//!
//! | API | Style |
//! | --- | --- |
//! | [`ObjectBuilder`] / [`ArrayBuilder`] | Fluent, owned builders |
//! | [`JsonWriter`] | Imperative, mutable, full control |
//!
//! ```
//! use jshift::{ObjectBuilder, ArrayBuilder, JsonDoc, TypedDoc};
//!
//! let doc = ObjectBuilder::new()
//!     .field("id", &7u64)
//!     .field("name", "hat")
//!     .array_field("tags", |a| a.item("a").item("b"))
//!     .object_field("meta", |o| o.field("ok", &true).null_field("note"))
//!     .field_opt("sku", None::<&str>)
//!     .into_doc();
//!
//! assert_eq!(doc.get::<u64>("id").unwrap(), 7);
//! assert_eq!(doc.get_str("tags[1]").unwrap(), "b");
//! assert!(doc.is_null("meta.note").unwrap());
//! assert!(!doc.contains("sku").unwrap());
//! ```
//!
//! # Design notes
//!
//! * **Usable & safe over pure speed** — integers use stack itoa; strings escape
//!   correctly; nesting never leaves a half-open container if you only use the
//!   fluent APIs.
//! * **Not a DOM** — you cannot re-open a finished value; build top-down.
//! * **Serde may still win raw encode throughput** on huge homogeneous trees;
//!   jshift wins when you also need open docs, patches, or to avoid `Value`.

use crate::convert::{write_json_string, ToJsonBytes};
use crate::error::Error;
use crate::typed_doc::TypedDoc;

// ─── JsonWriter (imperative core) ────────────────────────────────────────────

#[derive(Clone, Debug)]
enum Frame {
    /// Object: `first` member not yet written; `need_value` after a key.
    Object { first: bool, need_value: bool },
    /// Array: `first` element not yet written.
    Array { first: bool },
}

/// Imperative JSON encoder into one growable buffer.
///
/// Lower-level than [`ObjectBuilder`]: useful for loops, streaming records, or
/// when fluent chaining is awkward. Still 100% safe Rust, no `Value`.
///
/// ```
/// use jshift::JsonWriter;
///
/// let mut w = JsonWriter::new_object();
/// w.key("ids").unwrap();
/// w.begin_array().unwrap();
/// for i in 0..3u64 {
///     w.value(&i).unwrap();
/// }
/// w.end_array().unwrap();
/// w.key("ok").unwrap();
/// w.value(&true).unwrap();
/// let bytes = w.finish().unwrap();
/// assert_eq!(bytes, br#"{"ids":[0,1,2],"ok":true}"#);
/// ```
#[derive(Clone, Debug)]
pub struct JsonWriter {
    buf: Vec<u8>,
    stack: Vec<Frame>,
    /// When set, emit `\n` + spaces after `{`/`[`/`,` and before `}`/`]`.
    pretty_indent: Option<u8>,
}

impl JsonWriter {
    /// Empty writer (call [`begin_object`](Self::begin_object) or
    /// [`begin_array`](Self::begin_array) first).
    pub fn new() -> Self {
        Self {
            buf: Vec::with_capacity(64),
            stack: Vec::with_capacity(4),
            pretty_indent: None,
        }
    }

    /// Start a root object.
    pub fn new_object() -> Self {
        let mut w = Self::with_capacity(64);
        // Empty buffer: begin_object cannot fail.
        let _ = w.begin_object();
        w
    }

    /// Start a root array.
    pub fn new_array() -> Self {
        let mut w = Self::with_capacity(64);
        let _ = w.begin_array();
        w
    }

    /// Pre-size the buffer.
    pub fn with_capacity(cap: usize) -> Self {
        Self {
            buf: Vec::with_capacity(cap),
            stack: Vec::with_capacity(4),
            pretty_indent: None,
        }
    }

    /// Pretty-print with `indent` spaces per nesting level (compact if 0).
    pub fn pretty(mut self, indent: u8) -> Self {
        self.pretty_indent = if indent == 0 { None } else { Some(indent) };
        self
    }

    fn pretty_nl(&mut self) {
        if let Some(ind) = self.pretty_indent {
            self.buf.push(b'\n');
            let n = self.stack.len().saturating_mul(ind as usize);
            self.buf.resize(self.buf.len() + n, b' ');
        }
    }

    /// Borrow the bytes written so far (may be incomplete).
    #[inline]
    pub fn as_bytes(&self) -> &[u8] {
        &self.buf
    }

    /// Bytes written so far.
    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Nesting depth (0 = nothing open).
    #[inline]
    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    fn err(msg: &'static str) -> Error {
        Error::InvalidJsonSyntax { pos: 0, msg }
    }

    /// Prepare to write a value (array element or object value after key).
    fn prepare_value(&mut self) -> Result<(), Error> {
        match self.stack.last_mut() {
            None => {
                if !self.buf.is_empty() {
                    return Err(Self::err("Root value already written"));
                }
                Ok(())
            }
            Some(Frame::Array { first }) => {
                let need_comma = !*first;
                let need_nl = self.pretty_indent.is_some();
                *first = false;
                if need_comma {
                    self.buf.push(b',');
                }
                if need_nl {
                    self.pretty_nl();
                }
                Ok(())
            }
            Some(Frame::Object {
                first: _,
                need_value,
            }) => {
                if !*need_value {
                    return Err(Self::err("Object value requires a key() first"));
                }
                *need_value = false;
                Ok(())
            }
        }
    }

    /// Write an object key (`"k":`). Must be inside an object.
    pub fn key(&mut self, key: &str) -> Result<(), Error> {
        let (need_comma, need_nl, pretty_space) = match self.stack.last_mut() {
            Some(Frame::Object { first, need_value }) => {
                if *need_value {
                    return Err(Self::err("Cannot write key while value is pending"));
                }
                let need_comma = !*first;
                let need_nl = self.pretty_indent.is_some();
                *first = false;
                *need_value = true;
                (need_comma, need_nl, self.pretty_indent.is_some())
            }
            Some(Frame::Array { .. }) => {
                return Err(Self::err("key() is only valid inside an object"));
            }
            None => return Err(Self::err("key() with no open object")),
        };
        if need_comma {
            self.buf.push(b',');
        }
        if need_nl {
            self.pretty_nl();
        }
        write_json_string(&mut self.buf, key);
        self.buf.push(b':');
        if pretty_space {
            self.buf.push(b' ');
        }
        Ok(())
    }

    /// Write a typed value at the current position.
    pub fn value(&mut self, value: &(impl ToJsonBytes + ?Sized)) -> Result<(), Error> {
        self.prepare_value()?;
        value.write_json_bytes(&mut self.buf);
        Ok(())
    }

    /// Write already-encoded JSON value bytes.
    pub fn raw_value(&mut self, raw_json: &[u8]) -> Result<(), Error> {
        if raw_json.is_empty() {
            return Err(Self::err("raw value must not be empty"));
        }
        self.prepare_value()?;
        self.buf.extend_from_slice(raw_json);
        Ok(())
    }

    /// Write JSON `null`.
    pub fn null(&mut self) -> Result<(), Error> {
        self.raw_value(b"null")
    }

    /// Write `"key": value` in one step (object only).
    pub fn field(
        &mut self,
        key: &str,
        value: &(impl ToJsonBytes + ?Sized),
    ) -> Result<(), Error> {
        self.key(key)?;
        self.value(value)
    }

    /// Write `"key": <raw>`.
    pub fn raw_field(&mut self, key: &str, raw_json: &[u8]) -> Result<(), Error> {
        self.key(key)?;
        self.raw_value(raw_json)
    }

    /// Write `"key": null`.
    pub fn null_field(&mut self, key: &str) -> Result<(), Error> {
        self.key(key)?;
        self.null()
    }

    /// Write `"key": value` only when `value` is `Some`.
    pub fn field_opt<T: ToJsonBytes + ?Sized>(
        &mut self,
        key: &str,
        value: Option<&T>,
    ) -> Result<(), Error> {
        if let Some(v) = value {
            self.field(key, v)?;
        }
        Ok(())
    }

    /// Begin `{` as the current value (or root).
    pub fn begin_object(&mut self) -> Result<(), Error> {
        self.prepare_value()?;
        self.buf.push(b'{');
        self.stack.push(Frame::Object {
            first: true,
            need_value: false,
        });
        Ok(())
    }

    /// Begin `[` as the current value (or root).
    pub fn begin_array(&mut self) -> Result<(), Error> {
        self.prepare_value()?;
        self.buf.push(b'[');
        self.stack.push(Frame::Array { first: true });
        Ok(())
    }

    /// Close the innermost `{`.
    pub fn end_object(&mut self) -> Result<(), Error> {
        match self.stack.pop() {
            Some(Frame::Object { need_value, first }) => {
                if need_value {
                    return Err(Self::err("Unfinished object field (value missing)"));
                }
                if self.pretty_indent.is_some() && !first {
                    self.pretty_nl();
                }
                self.buf.push(b'}');
                Ok(())
            }
            Some(Frame::Array { .. }) => Err(Self::err("end_object() but array is open")),
            None => Err(Self::err("end_object() with nothing open")),
        }
    }

    /// Close the innermost `[`.
    pub fn end_array(&mut self) -> Result<(), Error> {
        match self.stack.pop() {
            Some(Frame::Array { first }) => {
                if self.pretty_indent.is_some() && !first {
                    self.pretty_nl();
                }
                self.buf.push(b']');
                Ok(())
            }
            Some(Frame::Object { .. }) => Err(Self::err("end_array() but object is open")),
            None => Err(Self::err("end_array() with nothing open")),
        }
    }

    /// Finish the document.
    ///
    /// Open containers are closed automatically (root `new_object` / nested
    /// `begin_*` without matching `end_*`). Fails if a key was written without
    /// a value, or if the buffer is empty.
    pub fn finish(mut self) -> Result<Vec<u8>, Error> {
        while !self.stack.is_empty() {
            match self.stack.last() {
                Some(Frame::Object { need_value, .. }) if *need_value => {
                    return Err(Self::err("Unfinished object field (value missing)"));
                }
                Some(Frame::Object { .. }) => self.end_object()?,
                Some(Frame::Array { .. }) => self.end_array()?,
                None => break,
            }
        }
        if self.buf.is_empty() {
            return Err(Self::err("Empty writer"));
        }
        Ok(self.buf)
    }

    /// Finish into a [`TypedDoc`].
    pub fn into_doc(self) -> Result<TypedDoc, Error> {
        Ok(TypedDoc::from_vec(self.finish()?))
    }

    /// Append finished JSON into `out` (same rules as [`finish`](Self::finish)).
    pub fn finish_into(self, out: &mut Vec<u8>) -> Result<(), Error> {
        let v = self.finish()?;
        out.extend_from_slice(&v);
        Ok(())
    }
}

impl Default for JsonWriter {
    fn default() -> Self {
        Self::new()
    }
}

// ─── ObjectBuilder (fluent) ──────────────────────────────────────────────────

/// Fluent builder for a JSON object (`{...}`).
///
/// Nested [`object_field`](Self::object_field) / [`array_field`](Self::array_field)
/// write into the **same** buffer (no temporary nested `Vec`).
#[derive(Clone, Debug)]
pub struct ObjectBuilder {
    buf: Vec<u8>,
    first: bool,
}

impl ObjectBuilder {
    /// Start an empty object (`{` written immediately).
    pub fn new() -> Self {
        Self {
            buf: {
                let mut v = Vec::with_capacity(64);
                v.push(b'{');
                v
            },
            first: true,
        }
    }

    /// Pre-size the internal buffer (hint only).
    pub fn with_capacity(cap: usize) -> Self {
        let mut buf = Vec::with_capacity(cap.max(2));
        buf.push(b'{');
        Self { buf, first: true }
    }

    fn sep(&mut self) {
        if !self.first {
            self.buf.push(b',');
        }
        self.first = false;
    }

    /// Append `"key": <value>` using [`ToJsonBytes`].
    pub fn field(mut self, key: &str, value: &(impl ToJsonBytes + ?Sized)) -> Self {
        self.sep();
        write_json_string(&mut self.buf, key);
        self.buf.push(b':');
        value.write_json_bytes(&mut self.buf);
        self
    }

    /// Append `"key":` followed by already-encoded JSON value bytes.
    pub fn raw_field(mut self, key: &str, raw_json: &[u8]) -> Self {
        self.sep();
        write_json_string(&mut self.buf, key);
        self.buf.push(b':');
        self.buf.extend_from_slice(raw_json);
        self
    }

    /// Append `"key": null`.
    pub fn null_field(mut self, key: &str) -> Self {
        self.sep();
        write_json_string(&mut self.buf, key);
        self.buf.push(b':');
        self.buf.extend_from_slice(b"null");
        self
    }

    /// Append `"key": value` only when `Some` (omit key when `None`).
    pub fn field_opt<T: ToJsonBytes + ?Sized>(self, key: &str, value: Option<&T>) -> Self {
        match value {
            Some(v) => self.field(key, v),
            None => self,
        }
    }

    /// Append `"key": value` when `cond` is true.
    pub fn field_if(
        self,
        cond: bool,
        key: &str,
        value: &(impl ToJsonBytes + ?Sized),
    ) -> Self {
        if cond {
            self.field(key, value)
        } else {
            self
        }
    }

    /// Nested object field written **in place** (same buffer).
    pub fn object_field(
        mut self,
        key: &str,
        build: impl FnOnce(ObjectBuilder) -> ObjectBuilder,
    ) -> Self {
        self.sep();
        write_json_string(&mut self.buf, key);
        self.buf.push(b':');
        // Move buffer into nested builder; finish reclaims with trailing `}`.
        let nested = ObjectBuilder {
            buf: {
                let mut b = std::mem::take(&mut self.buf);
                b.push(b'{');
                b
            },
            first: true,
        };
        let nested = build(nested);
        self.buf = {
            let mut b = nested.buf;
            b.push(b'}');
            b
        };
        self
    }

    /// Nested array field written **in place**.
    pub fn array_field(
        mut self,
        key: &str,
        build: impl FnOnce(ArrayBuilder) -> ArrayBuilder,
    ) -> Self {
        self.sep();
        write_json_string(&mut self.buf, key);
        self.buf.push(b':');
        let nested = ArrayBuilder {
            buf: {
                let mut b = std::mem::take(&mut self.buf);
                b.push(b'[');
                b
            },
            first: true,
        };
        let nested = build(nested);
        self.buf = {
            let mut b = nested.buf;
            b.push(b']');
            b
        };
        self
    }

    /// Finish and return compact JSON bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.buf.push(b'}');
        self.buf
    }

    /// Append the finished object onto `out`.
    pub fn finish_into(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.finish());
    }

    /// Finish into a [`TypedDoc`].
    #[inline]
    pub fn into_doc(self) -> TypedDoc {
        TypedDoc::from_vec(self.finish())
    }

    /// Current unfinished buffer length (including opening `{`).
    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }
}

impl Default for ObjectBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ToJsonBytes for ObjectBuilder {
    /// Snapshot + close (original builder unchanged). Prefer [`finish`](Self::finish).
    fn to_json_bytes(&self) -> Vec<u8> {
        let mut v = self.buf.clone();
        v.push(b'}');
        v
    }

    fn write_json_bytes(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.buf);
        out.push(b'}');
    }
}

// ─── ArrayBuilder (fluent) ───────────────────────────────────────────────────

/// Fluent builder for a JSON array (`[...]`).
///
/// Nested objects/arrays are written in place into the same buffer.
#[derive(Clone, Debug)]
pub struct ArrayBuilder {
    buf: Vec<u8>,
    first: bool,
}

impl ArrayBuilder {
    /// Start an empty array.
    pub fn new() -> Self {
        Self {
            buf: {
                let mut v = Vec::with_capacity(64);
                v.push(b'[');
                v
            },
            first: true,
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        let mut buf = Vec::with_capacity(cap.max(2));
        buf.push(b'[');
        Self { buf, first: true }
    }

    fn sep(&mut self) {
        if !self.first {
            self.buf.push(b',');
        }
        self.first = false;
    }

    /// Append a typed value.
    pub fn item(mut self, value: &(impl ToJsonBytes + ?Sized)) -> Self {
        self.sep();
        value.write_json_bytes(&mut self.buf);
        self
    }

    /// Append already-encoded JSON value bytes.
    pub fn raw_item(mut self, raw_json: &[u8]) -> Self {
        self.sep();
        self.buf.extend_from_slice(raw_json);
        self
    }

    /// Append JSON `null`.
    pub fn null_item(self) -> Self {
        self.raw_item(b"null")
    }

    /// Append `value` when `Some`.
    pub fn item_opt<T: ToJsonBytes + ?Sized>(self, value: Option<&T>) -> Self {
        match value {
            Some(v) => self.item(v),
            None => self,
        }
    }

    /// Nested object element written in place.
    pub fn object(mut self, build: impl FnOnce(ObjectBuilder) -> ObjectBuilder) -> Self {
        self.sep();
        let nested = ObjectBuilder {
            buf: {
                let mut b = std::mem::take(&mut self.buf);
                b.push(b'{');
                b
            },
            first: true,
        };
        let nested = build(nested);
        self.buf = {
            let mut b = nested.buf;
            b.push(b'}');
            b
        };
        self
    }

    /// Nested array element written in place.
    pub fn array(mut self, build: impl FnOnce(ArrayBuilder) -> ArrayBuilder) -> Self {
        self.sep();
        let nested = ArrayBuilder {
            buf: {
                let mut b = std::mem::take(&mut self.buf);
                b.push(b'[');
                b
            },
            first: true,
        };
        let nested = build(nested);
        self.buf = {
            let mut b = nested.buf;
            b.push(b']');
            b
        };
        self
    }

    /// Append all items from an iterator.
    pub fn extend<I, T>(mut self, iter: I) -> Self
    where
        I: IntoIterator<Item = T>,
        T: ToJsonBytes,
    {
        for item in iter {
            self = self.item(&item);
        }
        self
    }

    /// Finish and return compact JSON bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.buf.push(b']');
        self.buf
    }

    /// Append the finished array onto `out`.
    pub fn finish_into(self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.finish());
    }

    /// Finish into a [`TypedDoc`].
    #[inline]
    pub fn into_doc(self) -> TypedDoc {
        TypedDoc::from_vec(self.finish())
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.buf.len()
    }
}

impl Default for ArrayBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ToJsonBytes for ArrayBuilder {
    fn to_json_bytes(&self) -> Vec<u8> {
        let mut v = self.buf.clone();
        v.push(b']');
        v
    }

    fn write_json_bytes(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.buf);
        out.push(b']');
    }
}

// ─── helpers ─────────────────────────────────────────────────────────────────

/// Convenience: build an object from `(key, value)` pairs (call order preserved).
pub fn object_from_iter<'a, I, V>(fields: I) -> Vec<u8>
where
    I: IntoIterator<Item = (&'a str, V)>,
    V: ToJsonBytes,
{
    let mut b = ObjectBuilder::new();
    for (k, v) in fields {
        b = b.field(k, &v);
    }
    b.finish()
}

/// Convenience: build an array from values.
pub fn array_from_iter<I, T>(items: I) -> Vec<u8>
where
    I: IntoIterator<Item = T>,
    T: ToJsonBytes,
{
    ArrayBuilder::new().extend(items).finish()
}

/// Build a root object by driving a [`JsonWriter`] callback.
pub fn build_object(f: impl FnOnce(&mut JsonWriter) -> Result<(), Error>) -> Result<Vec<u8>, Error> {
    let mut w = JsonWriter::new_object();
    f(&mut w)?;
    w.finish()
}

/// Build a root array by driving a [`JsonWriter`] callback.
pub fn build_array(f: impl FnOnce(&mut JsonWriter) -> Result<(), Error>) -> Result<Vec<u8>, Error> {
    let mut w = JsonWriter::new_array();
    f(&mut w)?;
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::typed_doc::JsonDoc;

    #[test]
    fn build_object_array_nested_inplace() {
        let doc = ObjectBuilder::new()
            .field("id", &1u64)
            .field("ok", &true)
            .array_field("nums", |a| a.item(&1u64).item(&2u64))
            .object_field("meta", |o| o.field("v", &3u64).null_field("n"))
            .field_opt("skip", None::<&str>)
            .field_if(true, "on", &true)
            .field_if(false, "off", &true)
            .into_doc();
        assert_eq!(doc.get::<u64>("id").unwrap(), 1);
        assert_eq!(doc.get::<bool>("ok").unwrap(), true);
        assert_eq!(doc.get::<u64>("nums[1]").unwrap(), 2);
        assert_eq!(doc.get::<u64>("meta.v").unwrap(), 3);
        assert!(doc.is_null("meta.n").unwrap());
        assert!(!doc.contains("skip").unwrap());
        assert!(doc.get::<bool>("on").unwrap());
        assert!(!doc.contains("off").unwrap());
    }

    #[test]
    fn deep_nest_single_buffer() {
        let bytes = ObjectBuilder::with_capacity(128)
            .object_field("a", |o| {
                o.object_field("b", |o| o.array_field("c", |a| a.item(&9u64).object(|o| o.field("z", &1u64))))
            })
            .finish();
        assert_eq!(bytes, br#"{"a":{"b":{"c":[9,{"z":1}]}}}"#);
    }

    #[test]
    fn empty_containers() {
        assert_eq!(ObjectBuilder::new().finish(), b"{}");
        assert_eq!(ArrayBuilder::new().finish(), b"[]");
    }

    #[test]
    fn object_from_iter_helper() {
        let v = object_from_iter([("a", 1u64), ("b", 2u64)]);
        assert_eq!(v, br#"{"a":1,"b":2}"#);
        assert_eq!(array_from_iter([1u64, 2, 3]), br#"[1,2,3]"#);
    }

    #[test]
    fn json_writer_imperative() {
        let mut w = JsonWriter::new_object();
        w.field("ids", &ArrayBuilder::new().extend([1u64, 2, 3]))
            .unwrap();
        w.key("nested").unwrap();
        w.begin_object().unwrap();
        w.null_field("x").unwrap();
        w.end_object().unwrap();
        let bytes = w.finish().unwrap();
        assert_eq!(bytes, br#"{"ids":[1,2,3],"nested":{"x":null}}"#);
    }

    #[test]
    fn json_writer_errors_on_pending_value() {
        let mut w = JsonWriter::new_object();
        w.key("a").unwrap();
        assert!(w.finish().is_err()); // key without value
    }

    #[test]
    fn json_writer_auto_closes_root() {
        let mut w = JsonWriter::new_object();
        w.field("a", &1u64).unwrap();
        // no end_object — finish closes root
        assert_eq!(w.finish().unwrap(), br#"{"a":1}"#);
    }

    #[test]
    fn build_object_helper() {
        let v = build_object(|w| {
            w.field("n", &1u64)?;
            w.key("xs")?;
            w.begin_array()?;
            w.value(&10u64)?;
            w.value(&20u64)?;
            w.end_array()?;
            Ok(())
        })
        .unwrap();
        assert_eq!(v, br#"{"n":1,"xs":[10,20]}"#);
    }

    #[test]
    fn finish_into() {
        let mut out = Vec::new();
        ObjectBuilder::new().field("a", &1u64).finish_into(&mut out);
        assert_eq!(out, br#"{"a":1}"#);
    }

    #[test]
    fn array_extend_and_null() {
        let v = ArrayBuilder::new()
            .extend([1u64, 2])
            .null_item()
            .item_opt(Some(&3u64))
            .finish();
        assert_eq!(v, br#"[1,2,null,3]"#);
    }

    #[test]
    fn escaped_keys() {
        let v = ObjectBuilder::new().field("a\"b", &1u64).finish();
        assert_eq!(v, br#"{"a\"b":1}"#);
    }

    #[test]
    fn json_writer_pretty() {
        let mut w = JsonWriter::new_object().pretty(2);
        w.field("a", &1u64).unwrap();
        w.key("b").unwrap();
        w.begin_array().unwrap();
        w.value(&2u64).unwrap();
        w.value(&3u64).unwrap();
        w.end_array().unwrap();
        let s = String::from_utf8(w.finish().unwrap()).unwrap();
        assert!(s.contains('\n'));
        assert!(s.contains("  \"a\": 1"));
    }
}
