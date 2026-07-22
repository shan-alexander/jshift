//! JSON Lines (JSONL) framing helpers: message-at-a-time, not one giant index.
//!
//! Large multi-document workloads (Shopify pages, training dumps) are usually
//! better as **one index per line / page** than a single index over a merged blob.
//!
//! ```
//! use jshift::json_lines;
//!
//! let buf = br#"{"id":1}
//! {"id":2}
//! "#;
//! let lines: Vec<&[u8]> = json_lines(buf).collect();
//! assert_eq!(lines.len(), 2);
//! assert_eq!(lines[0], br#"{"id":1}"#);
//! ```
//!
//! With a [`crate::JsonView`] (manual or derive), use [`read_jsonl`] for typed rows.
//! Write lines with [`write_jsonl_views`] / [`write_jsonl_docs`] (NDJSON out).

use std::io::Write;

use crate::error::Error;
use crate::index::IndexedDocument;
use crate::typed_doc::{TypedDoc, TypedDocRef};
use crate::view::JsonView;

/// Iterate non-empty lines in a JSONL / NDJSON buffer (zero-copy slices).
///
/// Lines are split on `\n`. A trailing `\r` is stripped (CRLF). Empty lines are
/// skipped. No UTF-8 validation.
#[inline]
pub fn json_lines(buf: &[u8]) -> JsonLines<'_> {
    JsonLines { rest: buf }
}

/// Iterator over non-empty JSONL lines as byte slices.
#[derive(Debug, Clone)]
pub struct JsonLines<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for JsonLines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        while !self.rest.is_empty() {
            let nl = self
                .rest
                .iter()
                .position(|&b| b == b'\n')
                .unwrap_or(self.rest.len());
            let mut line = &self.rest[..nl];
            self.rest = if nl < self.rest.len() {
                &self.rest[nl + 1..]
            } else {
                &[]
            };
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
            if !line.is_empty() {
                return Some(line);
            }
        }
        None
    }
}

impl<'a> JsonLines<'a> {
    /// Remaining unparsed bytes (including the next line if any).
    #[inline]
    pub fn rest(&self) -> &'a [u8] {
        self.rest
    }
}

/// Map each non-empty JSONL line through a [`JsonView`] reader.
pub fn read_jsonl<'a, T: JsonView + 'a>(
    buf: &'a [u8],
) -> impl Iterator<Item = Result<T, Error>> + 'a {
    json_lines(buf).map(T::read_from)
}

/// Map each line with schema-guided indexing (rebuild index per line).
pub fn read_jsonl_indexed<'a, T: JsonView + 'a>(
    buf: &'a [u8],
) -> impl Iterator<Item = Result<T, Error>> + 'a {
    json_lines(buf).map(T::read_from_indexed)
}

/// Index a single JSONL line (array paths), then read a view.
pub fn read_line_indexed<T: JsonView>(
    line: &[u8],
    array_paths: &[&str],
) -> Result<T, Error> {
    let doc = IndexedDocument::build(line, array_paths)?;
    T::read_from_doc(&doc)
}

/// Zero-copy [`TypedDocRef`] per non-empty JSONL line.
///
/// ```
/// use jshift::{jsonl_docs, JsonDoc};
///
/// let buf = br#"{"id":1}
/// {"id":2}
/// "#;
/// let ids: Vec<u64> = jsonl_docs(buf)
///     .map(|d| d.get::<u64>("id").unwrap())
///     .collect();
/// assert_eq!(ids, vec![1, 2]);
/// ```
pub fn jsonl_docs(buf: &[u8]) -> impl Iterator<Item = TypedDocRef<'_>> + '_ {
    json_lines(buf).map(TypedDocRef::from_slice)
}

/// Owned [`TypedDoc`] per line (copies each line into a `Vec<u8>`).
pub fn jsonl_docs_owned(buf: &[u8]) -> impl Iterator<Item = TypedDoc> + '_ {
    json_lines(buf).map(TypedDoc::from_slice)
}

/// Write one JSON value as a JSONL line (`raw` + `\n`).
///
/// `raw` must be a single complete JSON value (no embedded newlines required by
/// the format, but callers should pass compact one-liners for interoperability).
pub fn write_jsonl_line(out: &mut impl Write, raw: &[u8]) -> Result<(), Error> {
    out.write_all(raw).map_err(io_err)?;
    out.write_all(b"\n").map_err(io_err)?;
    Ok(())
}

/// Write each view as one NDJSON line via [`TypedDoc::from_view`].
///
/// Useful for ETL / training dumps: typed rows out without a `Value` tree.
pub fn write_jsonl_views<'a, T: JsonView + 'a>(
    out: &mut impl Write,
    views: impl IntoIterator<Item = &'a T>,
) -> Result<(), Error> {
    for v in views {
        let doc = TypedDoc::from_view(v)?;
        write_jsonl_line(out, doc.as_bytes())?;
    }
    Ok(())
}

/// Write each document’s bytes as one NDJSON line.
pub fn write_jsonl_docs<'a>(
    out: &mut impl Write,
    docs: impl IntoIterator<Item = &'a (impl AsRef<[u8]> + ?Sized + 'a)>,
) -> Result<(), Error> {
    for d in docs {
        write_jsonl_line(out, d.as_ref())?;
    }
    Ok(())
}

/// Map JSONL lines with a fallible callback (stream; no intermediate `Vec`).
pub fn for_each_jsonl_line<F>(buf: &[u8], mut f: F) -> Result<(), Error>
where
    F: FnMut(&[u8]) -> Result<(), Error>,
{
    for line in json_lines(buf) {
        f(line)?;
    }
    Ok(())
}

/// Map each line as [`TypedDocRef`] with a fallible callback.
pub fn for_each_jsonl_doc<F>(buf: &[u8], mut f: F) -> Result<(), Error>
where
    F: FnMut(TypedDocRef<'_>) -> Result<(), Error>,
{
    for line in json_lines(buf) {
        f(TypedDocRef::from_slice(line))?;
    }
    Ok(())
}

fn io_err(_e: std::io::Error) -> Error {
    Error::InvalidJsonSyntax {
        pos: 0,
        msg: "I/O error writing JSONL",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::ObjectBuilder;
    use crate::convert::{FromJsonSlice, ToJsonBytes};
    use crate::path::parse_path;
    use crate::scan::find_value;
    use crate::typed_doc::JsonDoc;
    use crate::upsert_at_path;

    struct IdOnly {
        id: u64,
    }

    impl JsonView for IdOnly {
        fn read_from(json: &[u8]) -> Result<Self, Error> {
            let s = find_value(json, &parse_path("id"))?;
            Ok(Self {
                id: u64::from_json_slice(s).ok_or(Error::TypeMismatch {
                    expected: "u64",
                    found: "bad",
                })?,
            })
        }
        fn write_into(&self, json: &mut Vec<u8>) -> Result<(), Error> {
            upsert_at_path(json, &parse_path("id"), &self.id.to_json_bytes())
        }
    }

    #[test]
    fn jsonl_docs_and_write_views() {
        let buf = br#"{"id":1}
{"id":2}
"#;
        let ids: Vec<u64> = jsonl_docs(buf)
            .map(|d| d.get::<u64>("id").unwrap())
            .collect();
        assert_eq!(ids, vec![1, 2]);

        let views = [IdOnly { id: 3 }, IdOnly { id: 4 }];
        let mut out = Vec::new();
        write_jsonl_views(&mut out, &views).unwrap();
        let back: Vec<u64> = read_jsonl::<IdOnly>(&out)
            .map(|r| r.unwrap().id)
            .collect();
        assert_eq!(back, vec![3, 4]);
    }

    #[test]
    fn write_jsonl_docs_from_builder() {
        let d1 = ObjectBuilder::new().field("x", &1u64).into_doc();
        let d2 = ObjectBuilder::new().field("x", &2u64).into_doc();
        let mut out = Vec::new();
        write_jsonl_docs(&mut out, [d1.as_bytes(), d2.as_bytes()]).unwrap();
        assert_eq!(out, b"{\"x\":1}\n{\"x\":2}\n");
    }
}
