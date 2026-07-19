use crate::error::Error;

/// Represents a segment in a JSON path (either a string key or a zero-indexed array index).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathSegment<'a> {
    /// A string key representing an object field (e.g., `user` in `{"user": 1}`).
    ///
    /// For keys that contain special JSON characters (`"`, `\`, controls), the segment
    /// must match the **escaped** form as stored between quotes in the document
    /// (e.g. `a\"b` for the logical key `a"b`). Prefer [`crate::upsert_object_key`] /
    /// [`crate::delete_key`] with logical keys when mutating such members.
    Key(&'a str),
    /// A numeric index representing an array position (e.g., `0` in `[10, 20]`).
    Index(usize),
}

/// Owned path segment (see [`Path`]).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum OwnedPathSegment {
    /// Object key (logical / on-wire form as used by [`PathSegment::Key`]).
    Key(String),
    /// Array index.
    Index(usize),
}

impl OwnedPathSegment {
    /// Borrow as a zero-copy [`PathSegment`].
    pub fn as_ref(&self) -> PathSegment<'_> {
        match self {
            OwnedPathSegment::Key(k) => PathSegment::Key(k.as_str()),
            OwnedPathSegment::Index(i) => PathSegment::Index(*i),
        }
    }
}

/// Owned, reusable JSON path.
///
/// Prefer this when the same path is applied many times (avoids re-tokenizing a
/// string on every call). Derive-generated mutators use `'static` segment slices
/// instead for zero runtime parse cost.
///
/// # Examples
/// ```
/// use jshift::{Path, find_value};
///
/// let path = Path::parse("user.score");
/// let json = br#"{"user":{"score":9.5}}"#;
/// assert_eq!(path.find(json).unwrap(), b"9.5");
/// assert_eq!(find_value(json, &path.borrowed()).unwrap(), b"9.5");
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default)]
pub struct Path {
    segments: Vec<OwnedPathSegment>,
}

impl Path {
    /// Empty path (the whole document).
    pub fn new() -> Self {
        Self {
            segments: Vec::new(),
        }
    }

    /// Build from owned segments.
    pub fn from_owned(segments: Vec<OwnedPathSegment>) -> Self {
        Self { segments }
    }

    /// Lenient parse (same rules as [`parse_path`]).
    pub fn parse(s: &str) -> Self {
        Self {
            segments: owned_from_borrowed(parse_path(s)),
        }
    }

    /// Strict parse (same rules as [`try_parse_path`]).
    pub fn try_parse(s: &str) -> Result<Self, Error> {
        Ok(Self {
            segments: owned_from_borrowed(try_parse_path(s)?),
        })
    }

    /// Parse an RFC 6901 JSON Pointer (`""`, `"/a~1b/0"`).
    ///
    /// Purely numeric tokens become [`OwnedPathSegment::Index`] (no leading zeros
    /// except `"0"`); all other tokens become keys after `~0`/`~1` unescaping.
    pub fn from_json_pointer(pointer: &str) -> Result<Self, Error> {
        if pointer.is_empty() {
            return Ok(Self::new());
        }
        if !pointer.starts_with('/') {
            return Err(Error::InvalidPath {
                msg: "JSON Pointer must be empty or start with '/'",
            });
        }
        let mut segments = Vec::new();
        for raw in pointer[1..].split('/') {
            let token = unescape_pointer_token(raw)?;
            segments.push(pointer_token_to_segment(token));
        }
        Ok(Self { segments })
    }

    /// Number of segments.
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    /// Whether this path is empty (whole document).
    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Owned segments.
    pub fn owned_segments(&self) -> &[OwnedPathSegment] {
        &self.segments
    }

    /// Borrow as a `Vec` of zero-copy [`PathSegment`]s for APIs that take `&[PathSegment]`.
    pub fn borrowed(&self) -> Vec<PathSegment<'_>> {
        self.segments.iter().map(OwnedPathSegment::as_ref).collect()
    }

    /// [`crate::find_value`] using this path.
    pub fn find<'a>(&self, json: &'a [u8]) -> Result<&'a [u8], Error> {
        crate::scan::find_value(json, &self.borrowed())
    }

    /// [`crate::mutate_value`] using this path.
    pub fn mutate(&self, json: &mut Vec<u8>, new_value: &[u8]) -> Result<(), Error> {
        crate::mutate::mutate_value(json, &self.borrowed(), new_value)
    }

    /// [`crate::mutate_value_checked`] using this path.
    pub fn mutate_checked(&self, json: &mut Vec<u8>, new_value: &[u8]) -> Result<(), Error> {
        crate::mutate::mutate_value_checked(json, &self.borrowed(), new_value)
    }
}

fn owned_from_borrowed(segs: Vec<PathSegment<'_>>) -> Vec<OwnedPathSegment> {
    segs.into_iter()
        .map(|s| match s {
            PathSegment::Key(k) => OwnedPathSegment::Key(k.to_string()),
            PathSegment::Index(i) => OwnedPathSegment::Index(i),
        })
        .collect()
}

fn unescape_pointer_token(raw: &str) -> Result<String, Error> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '~' {
            match chars.next() {
                Some('0') => out.push('~'),
                Some('1') => out.push('/'),
                _ => {
                    return Err(Error::InvalidPath {
                        msg: "Invalid JSON Pointer escape (expected ~0 or ~1)",
                    });
                }
            }
        } else {
            out.push(c);
        }
    }
    Ok(out)
}

fn pointer_token_to_segment(token: String) -> OwnedPathSegment {
    if token == "0"
        || (!token.is_empty()
            && !token.starts_with('0')
            && token.bytes().all(|b| b.is_ascii_digit()))
    {
        if let Ok(idx) = token.parse::<usize>() {
            return OwnedPathSegment::Index(idx);
        }
    }
    OwnedPathSegment::Key(token)
}

/// Parses a dot-and-bracket notation path string into zero-copy path segments.
///
/// This is the **lenient** parser: invalid index brackets (`[x]`, `[]`, unclosed `[`)
/// are skipped or stop parsing rather than returning an error. Prefer
/// [`try_parse_path`] when you need to detect bad path strings.
///
/// Rules:
/// * `.` separates object keys.
/// * `[N]` selects a zero-based array index (`N` must be a decimal `usize`).
/// * Empty key segments (from leading/trailing/duplicate dots) are skipped.
/// * Unclosed `[` stops parsing (no partial index is emitted).
/// * Non-numeric `[...]` content is skipped past the closing `]` without emitting a segment.
///
/// # Examples
/// ```
/// use jshift::{parse_path, PathSegment};
///
/// let path = parse_path("metadata.tags[0].name");
/// assert_eq!(path, vec![
///     PathSegment::Key("metadata"),
///     PathSegment::Key("tags"),
///     PathSegment::Index(0),
///     PathSegment::Key("name")
/// ]);
/// ```
pub fn parse_path(s: &str) -> Vec<PathSegment<'_>> {
    parse_path_inner(s, false).unwrap_or_default()
}

/// Strict path parser. Returns [`Error::InvalidPath`] for malformed path syntax
/// instead of silently dropping segments.
///
/// # Examples
/// ```
/// use jshift::{try_parse_path, PathSegment, Error};
///
/// assert_eq!(
///     try_parse_path("a[0].b").unwrap(),
///     vec![PathSegment::Key("a"), PathSegment::Index(0), PathSegment::Key("b")]
/// );
/// assert!(matches!(
///     try_parse_path("a[x]"),
///     Err(Error::InvalidPath { .. })
/// ));
/// ```
pub fn try_parse_path(s: &str) -> Result<Vec<PathSegment<'_>>, Error> {
    parse_path_inner(s, true)
}

fn parse_path_inner(mut s: &str, strict: bool) -> Result<Vec<PathSegment<'_>>, Error> {
    let mut segments = Vec::new();
    while !s.is_empty() {
        if s.starts_with('.') {
            s = &s[1..];
            continue;
        }
        if s.is_empty() {
            break;
        }
        if s.starts_with('[') {
            match s.find(']') {
                Some(end_idx) => {
                    let idx_str = &s[1..end_idx];
                    if idx_str.is_empty() {
                        if strict {
                            return Err(Error::InvalidPath {
                                msg: "Empty array index brackets",
                            });
                        }
                    } else if !idx_str.bytes().all(|b| b.is_ascii_digit()) {
                        if strict {
                            return Err(Error::InvalidPath {
                                msg: "Non-numeric array index",
                            });
                        }
                    } else {
                        match idx_str.parse::<usize>() {
                            Ok(idx) => segments.push(PathSegment::Index(idx)),
                            Err(_) if strict => {
                                return Err(Error::InvalidPath {
                                    msg: "Array index out of range for usize",
                                });
                            }
                            Err(_) => {}
                        }
                    }
                    s = &s[end_idx + 1..];
                }
                None => {
                    if strict {
                        return Err(Error::InvalidPath {
                            msg: "Unclosed array index bracket '['",
                        });
                    }
                    break;
                }
            }
        } else {
            let end_key = s.find(['.', '[']).unwrap_or(s.len());
            let key = &s[..end_key];
            if !key.is_empty() {
                segments.push(PathSegment::Key(key));
            }
            s = &s[end_key..];
        }
    }
    Ok(segments)
}
