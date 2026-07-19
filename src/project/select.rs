//! Selection AST for projection (JMESPath / transforms attach here).

use std::collections::HashMap;

/// One step of a keep-list path (plus wildcards / slices).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProjectPathSegment {
    /// Object key (on-wire form).
    Key(String),
    /// Fixed array index.
    Index(usize),
    /// Every array element (`[]` or `[*]`).
    ArrayWildcard,
    /// Half-open slice `[start:end]` (`end = None` means to end).
    ArraySlice {
        start: usize,
        end: Option<usize>,
    },
}

/// Selection expression over a JSON value span.
///
/// Extension point for JMESPath and transforms. New shapes become new arms;
/// [`crate::project`] remains the executor.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectExpr {
    /// Keep the entire current value (raw byte copy).
    Identity,
    /// Current node (`@` in JMESPath); same emit as Identity today.
    Current,
    /// Get a single object field by on-wire key and yield **its value** (not a wrapper object).
    ///
    /// JMESPath identifier `id` compiles to `Field("id")`, whereas keep-list path
    /// merge still uses [`SelectExpr::Object`] subset projection.
    Field(String),
    /// Raw JSON literal bytes (number, string, bool, null, or prebuilt structure).
    Literal(Vec<u8>),
    /// Subset projection of an object (document-order emission of kept keys).
    Object(ObjectSelect),
    /// Array projection (each / indices / slice).
    Array(ArraySelect),
    /// JMESPath multi-select hash: build a **new** object in listed field order.
    ///
    /// Example: `{id: id, title: title, price: variants[0].price}`
    MultiSelectHash(Vec<HashField>),
    /// JMESPath multi-select list: build a **new** array of projected values.
    MultiSelectList(Vec<SelectExpr>),
    /// Pipe: evaluate `left`, then apply `right` to that intermediate JSON.
    Pipe(Box<SelectExpr>, Box<SelectExpr>),
    /// Flatten one level of nested arrays (JMESPath `[]` flatten projection).
    Flatten(Box<SelectExpr>),
    /// Descend a relative path from the current value, then apply `then`.
    Sub(Box<SelectExpr>, Box<SelectExpr>),
}

/// One field in a multi-select hash (`output_key: expr`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashField {
    /// Key written in the output object.
    pub output_key: String,
    /// Expression evaluated against the current node.
    pub expr: SelectExpr,
}

impl HashField {
    pub fn new(output_key: impl Into<String>, expr: SelectExpr) -> Self {
        Self {
            output_key: output_key.into(),
            expr,
        }
    }
}

/// Object field selection for subset projection (keeps keys from the input object).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ObjectSelect {
    pub(crate) fields: HashMap<String, SelectExpr>,
}

impl ObjectSelect {
    pub fn new() -> Self {
        Self {
            fields: HashMap::new(),
        }
    }

    pub fn len(&self) -> usize {
        self.fields.len()
    }

    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.fields.keys().map(|s| s.as_str())
    }

    pub fn get(&self, key: &str) -> Option<&SelectExpr> {
        self.fields.get(key)
    }

    /// Insert or replace a field selection.
    pub fn insert(&mut self, key: impl Into<String>, expr: SelectExpr) {
        self.fields.insert(key.into(), expr);
    }
}

/// Array projection strategy.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArraySelect {
    /// Apply the same expression to every element.
    Each(Box<SelectExpr>),
    /// Project listed indices only (ascending emission order).
    Indices(HashMap<usize, SelectExpr>),
    /// Project a half-open index range, applying `each` to every kept element.
    Slice {
        start: usize,
        end: Option<usize>,
        each: Box<SelectExpr>,
    },
}

/// Helpers for building selection trees programmatically (transforms).
impl SelectExpr {
    /// Identity / keep-all.
    pub fn identity() -> Self {
        Self::Identity
    }

    /// `left | right` pipe.
    pub fn pipe(left: SelectExpr, right: SelectExpr) -> Self {
        SelectExpr::Pipe(Box::new(left), Box::new(right))
    }

    /// Flatten after projecting `inner`.
    pub fn flatten(inner: SelectExpr) -> Self {
        SelectExpr::Flatten(Box::new(inner))
    }
}
