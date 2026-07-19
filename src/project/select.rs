//! Selection AST for projection (JMESPath / transforms attach here).

use std::collections::HashMap;

/// One step of a keep-list path (plus wildcards / slices).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum ProjectPathSegment {
    /// Object key (on-wire form).
    Key(String),
    /// Array index (may be negative: `-1` is last element).
    Index(i64),
    /// Every array element (`[]` or `[*]`).
    ArrayWildcard,
    /// Slice `[start:end:step]` with optional signed bounds (JMESPath rules).
    ArraySlice {
        start: Option<i64>,
        end: Option<i64>,
        step: Option<i64>,
    },
}

/// Selection / expression node over a JSON value span.
///
/// Extension point for JMESPath. New shapes become new arms; [`crate::project`]
/// remains the executor.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectExpr {
    /// Keep the entire current value (raw byte copy).
    Identity,
    /// Current node (`@` in JMESPath).
    Current,
    /// Get a single object field by on-wire key and yield **its value**.
    Field(String),
    /// Raw JSON literal bytes.
    Literal(Vec<u8>),
    /// Subset projection of an object (document-order emission of kept keys).
    Object(ObjectSelect),
    /// Array projection (each / indices / slice / filter).
    Array(ArraySelect),
    /// Multi-select hash: build a **new** object in listed field order.
    MultiSelectHash(Vec<HashField>),
    /// Multi-select list: build a **new** array of projected values.
    MultiSelectList(Vec<SelectExpr>),
    /// Pipe: evaluate `left`, then apply `right` to that intermediate JSON.
    Pipe(Box<SelectExpr>, Box<SelectExpr>),
    /// Flatten one level of nested arrays.
    Flatten(Box<SelectExpr>),
    /// Descend via `left` focus, then apply `right`.
    Sub(Box<SelectExpr>, Box<SelectExpr>),
    /// Comparison → JSON `true` / `false`.
    Cmp {
        op: CmpOp,
        left: Box<SelectExpr>,
        right: Box<SelectExpr>,
    },
    /// Logical AND (JMESPath `&&`) → truthy left or false.
    And(Box<SelectExpr>, Box<SelectExpr>),
    /// Logical OR (JMESPath `||`).
    Or(Box<SelectExpr>, Box<SelectExpr>),
    /// Logical NOT (JMESPath `!`).
    Not(Box<SelectExpr>),
    /// Function call (`length(@)`, `map(&foo, arr)`, …).
    Call {
        name: String,
        args: Vec<SelectExpr>,
    },
    /// Expression reference (`&foo`) for higher-order functions (`map`, `sort_by`, `group_by`).
    /// Not evaluated alone; the callee applies it per element.
    Expref(Box<SelectExpr>),
    /// Object value projection (`*` / `foo.*`): all object values as an array, then `each`.
    ObjectProjection(Box<SelectExpr>),
    /// Parenthesized expression (preserved for AST clarity; emit = inner).
    Paren(Box<SelectExpr>),
}

/// Comparison operators (JMESPath).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CmpOp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// One field in a multi-select hash (`output_key: expr`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HashField {
    pub output_key: String,
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

/// Object field selection for subset projection.
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
    /// Project listed indices only (keys may be negative before resolve).
    Indices(HashMap<i64, SelectExpr>),
    /// Slice with optional signed bounds and step.
    Slice {
        start: Option<i64>,
        end: Option<i64>,
        step: Option<i64>,
        each: Box<SelectExpr>,
    },
    /// Filter projection: keep elements where `pred` is truthy, then apply `each`.
    Filter {
        pred: Box<SelectExpr>,
        each: Box<SelectExpr>,
    },
}

impl SelectExpr {
    pub fn identity() -> Self {
        Self::Identity
    }

    pub fn pipe(left: SelectExpr, right: SelectExpr) -> Self {
        SelectExpr::Pipe(Box::new(left), Box::new(right))
    }

    pub fn flatten(inner: SelectExpr) -> Self {
        SelectExpr::Flatten(Box::new(inner))
    }
}

/// Resolve a JMESPath-style signed index against `len`.
pub fn resolve_index(index: i64, len: usize) -> Option<usize> {
    if index >= 0 {
        let i = index as usize;
        if i < len {
            Some(i)
        } else {
            None
        }
    } else {
        let n = (-index) as usize;
        if n == 0 || n > len {
            None
        } else {
            Some(len - n)
        }
    }
}

/// Expand a JMESPath slice to concrete ascending/stepped indices.
pub fn resolve_slice(
    len: usize,
    start: Option<i64>,
    end: Option<i64>,
    step: Option<i64>,
) -> Vec<usize> {
    let step = step.unwrap_or(1);
    if step == 0 {
        return Vec::new();
    }
    let len_i = len as i64;

    // JMESPath / Python-like normalization.
    let mut s = start.unwrap_or(if step > 0 { 0 } else { len_i - 1 });
    let mut e = end.unwrap_or(if step > 0 { len_i } else { -len_i - 1 });

    if s < 0 {
        s += len_i;
    }
    if e < 0 {
        e += len_i;
    }
    if step > 0 {
        s = s.clamp(0, len_i);
        e = e.clamp(0, len_i);
        let mut out = Vec::new();
        let mut i = s;
        while i < e {
            out.push(i as usize);
            i += step;
        }
        out
    } else {
        s = s.clamp(-1, len_i - 1);
        e = e.clamp(-1, len_i - 1);
        let mut out = Vec::new();
        let mut i = s;
        while i > e {
            if i >= 0 && i < len_i {
                out.push(i as usize);
            }
            i += step;
        }
        out
    }
}
