//! JSON Lines (JSONL) framing helpers — message-at-a-time, not one giant index.
//!
//! Large multi-document workloads (Shopify pages, training dumps) are usually
//! better as **one index per line / page** than a single index over a merged blob.
//!
//! ```
//! use jshift::{json_lines, JsonMutatorSchema, JsonView};
//!
//! #[derive(JsonMutatorSchema)]
//! struct Row {
//!     #[json(path = "id")]
//!     id: u64,
//! }
//!
//! let buf = br#"{"id":1}
//! {"id":2}
//! "#;
//! let ids: Vec<u64> = json_lines(buf)
//!     .map(|line| Row::read_from(line).unwrap().id)
//!     .collect();
//! assert_eq!(ids, vec![1, 2]);
//! ```

use crate::error::Error;
use crate::index::IndexedDocument;
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
