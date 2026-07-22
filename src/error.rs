/// Errors returned by scanning and mutating operations.
///
/// Marked `#[non_exhaustive]` so new variants can be added in minor releases
/// without breaking downstream `match` expressions (use a wildcard arm).
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The specified JSON path was not found in the document.
    PathNotFound,
    /// The JSON document is structurally malformed.
    InvalidJsonSyntax {
        /// Byte offset in the JSON buffer where the syntax error was detected.
        pos: usize,
        /// Informative message describing the syntax error.
        msg: &'static str,
    },
    /// The path string could not be parsed (see [`crate::try_parse_path`]).
    InvalidPath {
        /// Informative message describing the path error.
        msg: &'static str,
    },
    /// The array index is larger than the number of elements in the array.
    IndexOutOfBounds {
        /// The index that was queried.
        index: usize,
    },
    /// The parsed type does not match the JSON value format.
    TypeMismatch {
        /// Expected type name (e.g. `"array"`, `"object"`).
        expected: &'static str,
        /// Encountered type name (e.g. `"primitive"`).
        found: &'static str,
    },
    /// JMESPath evaluation error (invalid function use, incomparable types, …).
    ///
    /// Distinct from “no value” outcomes: under default [`crate::MissingPolicy::Skip`],
    /// missing paths project to JSON `null` rather than this error.
    Jmespath {
        /// Informative message.
        msg: &'static str,
    },
    /// Typed decode / validation failed at a known path (richer than bare
    /// [`TypeMismatch`](Self::TypeMismatch)).
    Decode {
        /// Dot/bracket path where the failure was detected.
        path: String,
        /// Expected type or shape description.
        expected: &'static str,
        /// Encountered type or shape description.
        found: &'static str,
        /// Optional byte offset in the buffer.
        pos: Option<usize>,
    },
    /// Required field / path was absent ([`crate::validate::require_paths`]).
    MissingField {
        /// Path that was required.
        path: String,
    },
    /// Closed schema rejected an unknown object key.
    UnknownField {
        /// Key or path that was not allowed.
        path: String,
    },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::PathNotFound => write!(f, "Path not found in JSON"),
            Error::InvalidJsonSyntax { pos, msg } => {
                write!(f, "Invalid JSON syntax at position {}: {}", pos, msg)
            }
            Error::InvalidPath { msg } => write!(f, "Invalid JSON path: {}", msg),
            Error::IndexOutOfBounds { index } => {
                write!(f, "Array index out of bounds: {}", index)
            }
            Error::TypeMismatch { expected, found } => {
                write!(
                    f,
                    "Type mismatch: expected '{}', found '{}'",
                    expected, found
                )
            }
            Error::Jmespath { msg } => write!(f, "JMESPath error: {}", msg),
            Error::Decode {
                path,
                expected,
                found,
                pos,
            } => {
                if let Some(p) = pos {
                    write!(
                        f,
                        "Decode error at '{path}' (pos {p}): expected '{expected}', found '{found}'"
                    )
                } else {
                    write!(
                        f,
                        "Decode error at '{path}': expected '{expected}', found '{found}'"
                    )
                }
            }
            Error::MissingField { path } => write!(f, "Missing required field '{path}'"),
            Error::UnknownField { path } => write!(f, "Unknown field '{path}'"),
        }
    }
}

impl std::error::Error for Error {}
