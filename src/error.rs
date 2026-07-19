/// Errors returned by scanning and mutating operations.
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
        }
    }
}

impl std::error::Error for Error {}
