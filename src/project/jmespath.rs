//! JMESPath parser → [`SelectExpr`].
//!
//! Supported surface (growing):
//! * identifiers, `.` descent, `@` current, parentheses
//! * `[N]` / `[-N]`, `[*]` / `[]`, slices `[start:end:step]` (signed)
//! * filters `[?expr]` with `== != < <= > >=`, `&&`, `||`, `!`
//! * multi-select hash / list, pipe `|`, flatten `| []`
//! * functions: `length`, `keys`, `values`, `type`, `to_string`, `to_number`,
//!   `starts_with`, `ends_with`, `contains`, `not_null`, `reverse`, `sort`,
//!   `sort_by`, `max_by`, `min_by`, `map`, `group_by`, `join`, `max`, `min`,
//!   `sum`, `avg`, `abs`, `ceil`, `floor`, `to_array`, `merge`
//! * expression references `&expr` for higher-order functions
//! * object projection `*` / `foo.*` / `*.bar` (multi-select wildcards on objects)
//! * literals: numbers, `"…"` / `'…'` (raw), `` `…` `` (JSON literal), true/false/null
//!
//! JMESPath has no parent axis (`..`); use pipe from an outer context.

use crate::error::Error;
use crate::project::select::{
    ArraySelect, CmpOp, HashField, ProjectPathSegment, SelectExpr,
};

/// Parse a JMESPath expression into a [`SelectExpr`].
pub fn parse_jmespath_expr(input: &str) -> Result<SelectExpr, Error> {
    let mut p = Parser::new(input);
    let expr = p.parse_pipe()?;
    p.skip_ws();
    if !p.eof() {
        return Err(Error::InvalidPath {
            msg: "Trailing junk after JMESPath expression",
        });
    }
    Ok(expr)
}

/// Build [`SelectExpr`] from a keep-list path.
pub fn select_from_project_path(path: &str) -> Result<SelectExpr, Error> {
    segs_to_select(&parse_project_path(path)?)
}

/// Parse projection path segments including signed slices and wildcards.
pub fn parse_project_path(s: &str) -> Result<Vec<ProjectPathSegment>, Error> {
    let mut rest = s.trim();
    let mut segments = Vec::new();
    if rest.is_empty() {
        return Err(Error::InvalidPath {
            msg: "Empty project path",
        });
    }
    while !rest.is_empty() {
        if rest.starts_with('.') {
            rest = &rest[1..];
            continue;
        }
        if rest.starts_with('[') {
            let end_idx = rest.find(']').ok_or(Error::InvalidPath {
                msg: "Unclosed '[' in project path",
            })?;
            let inner = rest[1..end_idx].trim();
            if inner.is_empty() {
                // Keep-list `[]` means every element (wildcard), not flatten.
                segments.push(ProjectPathSegment::ArrayWildcard);
            } else if inner == "*" {
                segments.push(ProjectPathSegment::ArrayWildcard);
            } else if inner.contains(':') {
                segments.push(parse_slice_inner(inner)?);
            } else {
                let idx = parse_signed(inner).map_err(|_| Error::InvalidPath {
                    msg: "Invalid array index",
                })?;
                segments.push(ProjectPathSegment::Index(idx));
            }
            rest = &rest[end_idx + 1..];
        } else {
            let end_key = rest.find(['.', '[']).unwrap_or(rest.len());
            let key = &rest[..end_key];
            if key.is_empty() {
                return Err(Error::InvalidPath {
                    msg: "Empty key segment in project path",
                });
            }
            segments.push(ProjectPathSegment::Key(key.to_string()));
            rest = &rest[end_key..];
        }
    }
    Ok(segments)
}

fn parse_slice_inner(inner: &str) -> Result<ProjectPathSegment, Error> {
    let parts: Vec<&str> = inner.split(':').collect();
    if parts.len() > 3 {
        return Err(Error::InvalidPath {
            msg: "Slice has too many components",
        });
    }
    let parse_opt = |s: &str| -> Result<Option<i64>, Error> {
        let s = s.trim();
        if s.is_empty() {
            Ok(None)
        } else {
            Ok(Some(parse_signed(s).map_err(|_| Error::InvalidPath {
                msg: "Invalid slice bound",
            })?))
        }
    };
    let start = parse_opt(parts.first().copied().unwrap_or(""))?;
    let end = parse_opt(parts.get(1).copied().unwrap_or(""))?;
    let step = if parts.len() == 3 {
        parse_opt(parts[2])?
    } else {
        None
    };
    Ok(ProjectPathSegment::ArraySlice { start, end, step })
}

fn parse_signed(s: &str) -> Result<i64, ()> {
    s.trim().parse().map_err(|_| ())
}

fn segs_to_select(segs: &[ProjectPathSegment]) -> Result<SelectExpr, Error> {
    if segs.is_empty() {
        return Ok(SelectExpr::Identity);
    }
    let mut expr = SelectExpr::Identity;
    for seg in segs.iter().rev() {
        expr = match seg {
            ProjectPathSegment::Key(k) => {
                if matches!(expr, SelectExpr::Identity | SelectExpr::Current) {
                    SelectExpr::Field(k.clone())
                } else {
                    SelectExpr::Sub(Box::new(SelectExpr::Field(k.clone())), Box::new(expr))
                }
            }
            ProjectPathSegment::Index(i) => {
                let mut map = std::collections::HashMap::new();
                map.insert(*i, expr);
                SelectExpr::Array(ArraySelect::Indices(map))
            }
            ProjectPathSegment::ArrayWildcard => {
                SelectExpr::Array(ArraySelect::Each(Box::new(expr)))
            }
            ProjectPathSegment::ArraySlice { start, end, step } => {
                SelectExpr::Array(ArraySelect::Slice {
                    start: *start,
                    end: *end,
                    step: *step,
                    each: Box::new(expr),
                })
            }
        };
    }
    Ok(expr)
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Parser<'a> {
    fn new(s: &'a str) -> Self {
        Self { s, i: 0 }
    }

    fn eof(&self) -> bool {
        self.i >= self.s.len()
    }

    fn peek(&self) -> Option<char> {
        self.s[self.i..].chars().next()
    }

    fn peek_str(&self) -> &str {
        &self.s[self.i..]
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += c.len_utf8();
        Some(c)
    }

    fn skip_ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_whitespace()) {
            self.bump();
        }
    }

    fn eat(&mut self, expected: char) -> Result<(), Error> {
        self.skip_ws();
        match self.bump() {
            Some(c) if c == expected => Ok(()),
            _ => Err(Error::InvalidPath {
                msg: "Unexpected character in JMESPath",
            }),
        }
    }

    fn starts_with(&self, lit: &str) -> bool {
        self.peek_str().starts_with(lit)
    }

    // pipe → or → and → not → cmp → project
    fn parse_pipe(&mut self) -> Result<SelectExpr, Error> {
        let mut left = self.parse_or()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('|') && !self.starts_with("||") {
                self.bump();
                self.skip_ws();
                if self.peek() == Some('[') {
                    let save = self.i;
                    self.bump();
                    self.skip_ws();
                    if self.peek() == Some(']') {
                        self.bump();
                        left = SelectExpr::Flatten(Box::new(left));
                        continue;
                    }
                    self.i = save;
                }
                let right = self.parse_or()?;
                left = SelectExpr::Pipe(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_or(&mut self) -> Result<SelectExpr, Error> {
        let mut left = self.parse_and()?;
        loop {
            self.skip_ws();
            if self.starts_with("||") {
                self.i += 2;
                let right = self.parse_and()?;
                left = SelectExpr::Or(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_and(&mut self) -> Result<SelectExpr, Error> {
        let mut left = self.parse_not()?;
        loop {
            self.skip_ws();
            if self.starts_with("&&") {
                self.i += 2;
                let right = self.parse_not()?;
                left = SelectExpr::And(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_not(&mut self) -> Result<SelectExpr, Error> {
        self.skip_ws();
        if self.peek() == Some('!') && !self.starts_with("!=") {
            self.bump();
            let inner = self.parse_not()?;
            return Ok(SelectExpr::Not(Box::new(inner)));
        }
        self.parse_compare()
    }

    fn parse_compare(&mut self) -> Result<SelectExpr, Error> {
        let left = self.parse_project()?;
        self.skip_ws();
        let op = if self.starts_with("==") {
            self.i += 2;
            Some(CmpOp::Eq)
        } else if self.starts_with("!=") {
            self.i += 2;
            Some(CmpOp::Ne)
        } else if self.starts_with("<=") {
            self.i += 2;
            Some(CmpOp::Le)
        } else if self.starts_with(">=") {
            self.i += 2;
            Some(CmpOp::Ge)
        } else if self.peek() == Some('<') {
            self.bump();
            Some(CmpOp::Lt)
        } else if self.peek() == Some('>') {
            self.bump();
            Some(CmpOp::Gt)
        } else {
            None
        };
        if let Some(op) = op {
            let right = self.parse_project()?;
            Ok(SelectExpr::Cmp {
                op,
                left: Box::new(left),
                right: Box::new(right),
            })
        } else {
            Ok(left)
        }
    }

    fn parse_project(&mut self) -> Result<SelectExpr, Error> {
        self.skip_ws();
        // Expression reference binds tighter than pipe: &foo.bar
        if self.peek() == Some('&') {
            self.bump();
            let inner = self.parse_project()?;
            return Ok(SelectExpr::Expref(Box::new(inner)));
        }
        if self.peek() == Some('{') {
            let expr = self.parse_multi_hash()?;
            return self.parse_projection_suffixes(expr);
        }
        // leading bracket: multi-list / index / filter / slice (+ further suffixes)
        if self.peek() == Some('[') {
            let expr = self.parse_bracket_expr()?;
            return self.parse_projection_suffixes(expr);
        }
        // leading object projection
        if self.peek() == Some('*') {
            self.bump();
            let expr = SelectExpr::ObjectProjection(Box::new(SelectExpr::Identity));
            return self.parse_projection_suffixes(expr);
        }
        let mut expr = self.parse_atom()?;
        expr = self.parse_projection_suffixes(expr)?;
        Ok(expr)
    }

    /// `.field`, `.*`, `[…]`, `(args)` suffixes after a projectable expr.
    fn parse_projection_suffixes(&mut self, mut expr: SelectExpr) -> Result<SelectExpr, Error> {
        loop {
            self.skip_ws();
            match self.peek() {
                Some('.') => {
                    self.bump();
                    self.skip_ws();
                    if self.peek() == Some('*') {
                        self.bump();
                        // object value projection after current
                        expr = chain_sub(
                            expr,
                            SelectExpr::ObjectProjection(Box::new(SelectExpr::Identity)),
                        );
                    } else if self.peek() == Some('{') {
                        let right = self.parse_multi_hash()?;
                        // Empty multi-hash after `.` is a syntax error (`a.{}`).
                        if matches!(&right, SelectExpr::MultiSelectHash(f) if f.is_empty()) {
                            return Err(Error::InvalidPath {
                                msg: "Empty multi-select hash after '.'",
                            });
                        }
                        expr = chain_sub(expr, right);
                    } else if self.peek() == Some('[') {
                        // Multi-select list after `.` cannot start with a bare number
                        // (jmespath: `foo.[0]` / `foo.[0,1]` are invalid).
                        let save = self.i;
                        self.bump(); // '['
                        self.skip_ws();
                        let bad_number = self
                            .peek()
                            .is_some_and(|c| c == '-' || c.is_ascii_digit());
                        self.i = save;
                        if bad_number {
                            return Err(Error::InvalidPath {
                                msg: "Multi-select list after '.' cannot start with a number",
                            });
                        }
                        let right = self.parse_bracket_expr()?;
                        // Reject multi-list items that are bare number literals mixed in.
                        if let SelectExpr::MultiSelectList(items) = &right {
                            for it in items {
                                if matches!(it, SelectExpr::Literal(b) if is_json_number_lit(b)) {
                                    return Err(Error::InvalidPath {
                                        msg: "Number not allowed as multi-select list item here",
                                    });
                                }
                            }
                        }
                        expr = chain_sub(expr, right);
                    } else if self.peek() == Some('"') {
                        // Quoted identifier: "foo.bar", "foo bar", escapes
                        let name = self.parse_dq_string_raw()?;
                        expr = chain_sub(expr, SelectExpr::FieldQuoted(name));
                    } else {
                        let name = self.parse_ident()?;
                        if self.peek() == Some('(') {
                            let args = self.parse_arg_list()?;
                            expr = chain_sub(
                                expr,
                                SelectExpr::Call { name, args },
                            );
                        } else {
                            expr = chain_sub(expr, SelectExpr::Field(name));
                        }
                    }
                }
                Some('[') => {
                    let bracket = self.parse_bracket_suffix()?;
                    expr = chain_sub(expr, bracket);
                }
                Some('(') => {
                    // Only bare identifiers are function names (not quoted identifiers).
                    if let SelectExpr::Field(name) = expr {
                        let args = self.parse_arg_list()?;
                        expr = SelectExpr::Call { name, args };
                    } else if matches!(expr, SelectExpr::FieldQuoted(_)) {
                        return Err(Error::InvalidPath {
                            msg: "Quoted identifier is not a function name",
                        });
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }
        Ok(expr)
    }

    fn parse_atom(&mut self) -> Result<SelectExpr, Error> {
        self.skip_ws();
        match self.peek() {
            Some('@') => {
                self.bump();
                Ok(SelectExpr::Current)
            }
            Some('&') => {
                self.bump();
                let inner = self.parse_project()?;
                Ok(SelectExpr::Expref(Box::new(inner)))
            }
            Some('*') => {
                self.bump();
                Ok(SelectExpr::ObjectProjection(Box::new(SelectExpr::Identity)))
            }
            Some('(') => {
                self.bump();
                let e = self.parse_pipe()?;
                self.eat(')')?;
                Ok(SelectExpr::Paren(Box::new(e)))
            }
            // Double-quoted = identifier (may contain dots/spaces). Single-quoted = string literal.
            Some('"') => {
                // Quoted identifier (not a callable function name).
                let name = self.parse_dq_string_raw()?;
                Ok(SelectExpr::FieldQuoted(name))
            }
            Some('\'') => self.parse_raw_string_literal(),
            Some('`') => self.parse_backtick_literal(),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number_literal(),
            Some('{') => self.parse_multi_hash(),
            Some('[') => self.parse_bracket_expr(),
            Some(c) if is_ident_start(c) => {
                let name = self.parse_ident()?;
                self.skip_ws();
                if self.peek() == Some('(') {
                    let args = self.parse_arg_list()?;
                    Ok(SelectExpr::Call { name, args })
                } else if name == "true" {
                    Ok(SelectExpr::Literal(b"true".to_vec()))
                } else if name == "false" {
                    Ok(SelectExpr::Literal(b"false".to_vec()))
                } else if name == "null" {
                    Ok(SelectExpr::Literal(b"null".to_vec()))
                } else {
                    Ok(SelectExpr::Field(name))
                }
            }
            _ => Err(Error::InvalidPath {
                msg: "Expected JMESPath atom",
            }),
        }
    }

    fn parse_arg_list(&mut self) -> Result<Vec<SelectExpr>, Error> {
        self.eat('(')?;
        let mut args = Vec::new();
        self.skip_ws();
        if self.peek() == Some(')') {
            self.bump();
            return Ok(args);
        }
        loop {
            args.push(self.parse_pipe()?);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.bump();
                    continue;
                }
                Some(')') => {
                    self.bump();
                    break;
                }
                _ => {
                    return Err(Error::InvalidPath {
                        msg: "Expected ',' or ')' in function args",
                    });
                }
            }
        }
        Ok(args)
    }

    fn parse_ident(&mut self) -> Result<String, Error> {
        self.skip_ws();
        let start = self.i;
        match self.peek() {
            Some(c) if is_ident_start(c) => {
                self.bump();
            }
            _ => {
                return Err(Error::InvalidPath {
                    msg: "Expected identifier",
                });
            }
        }
        while self.peek().is_some_and(is_ident_continue) {
            self.bump();
        }
        Ok(self.s[start..self.i].to_string())
    }

    fn parse_multi_hash(&mut self) -> Result<SelectExpr, Error> {
        self.eat('{')?;
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some('}') {
            self.bump();
            return Ok(SelectExpr::MultiSelectHash(fields));
        }
        loop {
            self.skip_ws();
            let key = if self.peek() == Some('"') {
                self.parse_dq_string_raw()?
            } else if self.peek() == Some('\'') {
                self.parse_raw_string_raw()?
            } else {
                self.parse_ident()?
            };
            self.skip_ws();
            self.eat(':')?;
            let expr = self.parse_pipe()?;
            fields.push(HashField::new(key, expr));
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.bump();
                    continue;
                }
                Some('}') => {
                    self.bump();
                    break;
                }
                _ => {
                    return Err(Error::InvalidPath {
                        msg: "Expected ',' or '}' in multi-select hash",
                    });
                }
            }
        }
        Ok(SelectExpr::MultiSelectHash(fields))
    }

    /// Bracket as a full expression (start of atom/project): list, index, filter, slice.
    fn parse_bracket_expr(&mut self) -> Result<SelectExpr, Error> {
        self.parse_bracket_common(true)
    }

    /// Bracket as a suffix after an expression.
    fn parse_bracket_suffix(&mut self) -> Result<SelectExpr, Error> {
        self.parse_bracket_common(false)
    }

    fn parse_bracket_common(&mut self, allow_multi_list: bool) -> Result<SelectExpr, Error> {
        self.eat('[')?;
        // Filter `?` must be immediately after `[` (no whitespace): `foo[?x]` ok, `foo[ ?x]` error.
        if self.peek() == Some('?') {
            self.bump();
            let pred = self.parse_pipe()?;
            self.eat(']')?;
            return Ok(SelectExpr::Array(ArraySelect::Filter {
                pred: Box::new(pred),
                each: Box::new(SelectExpr::Identity),
            }));
        }
        self.skip_ws();
        // empty `[]` — JMESPath flatten operator / flatten-projection (not `[*]`).
        // Represented as Flatten(Identity); chain_sub wraps prior expr as Flatten(prefix)
        // and subsequent `.field` as Sub(Flatten(prefix), Each(field)).
        if self.peek() == Some(']') {
            self.bump();
            return Ok(SelectExpr::Flatten(Box::new(SelectExpr::Identity)));
        }
        // `[*]` — list projection (no flatten). `[*.*]` is a multi-select list.
        if self.peek() == Some('*') {
            let save = self.i;
            self.bump();
            if self.peek() == Some(']') {
                self.bump();
                return Ok(SelectExpr::Array(ArraySelect::Each(Box::new(
                    SelectExpr::Identity,
                ))));
            }
            self.i = save;
            // fall through to multi-select list / other forms
        }
        // peek if slice/index-only: number, colon, minus
        let save = self.i;
        if self.peek().is_some_and(|c| c == '-' || c == ':' || c.is_ascii_digit()) {
            let inner_start = self.i;
            while self.peek().is_some_and(|c| c != ']') {
                self.bump();
            }
            let inner = self.s[inner_start..self.i].trim();
            if self.peek() == Some(']') {
                self.bump();
                if inner.contains(':') {
                    return match parse_slice_inner(inner)? {
                        ProjectPathSegment::ArraySlice { start, end, step } => {
                            Ok(SelectExpr::Array(ArraySelect::Slice {
                                start,
                                end,
                                step,
                                each: Box::new(SelectExpr::Identity),
                            }))
                        }
                        _ => unreachable!(),
                    };
                }
                if let Ok(idx) = parse_signed(inner) {
                    let mut map = std::collections::HashMap::new();
                    map.insert(idx, SelectExpr::Identity);
                    return Ok(SelectExpr::Array(ArraySelect::Indices(map)));
                }
            }
            self.i = save;
        }
        if !allow_multi_list {
            return Err(Error::InvalidPath {
                msg: "Invalid bracket suffix",
            });
        }
        // multi-select list
        let mut items = Vec::new();
        loop {
            items.push(self.parse_pipe()?);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.bump();
                    continue;
                }
                Some(']') => {
                    self.bump();
                    break;
                }
                _ => {
                    return Err(Error::InvalidPath {
                        msg: "Expected ',' or ']' in multi-select list",
                    });
                }
            }
        }
        Ok(SelectExpr::MultiSelectList(items))
    }

    fn parse_dq_string_raw(&mut self) -> Result<String, Error> {
        self.eat('"')?;
        let mut out = String::new();
        while let Some(c) = self.bump() {
            match c {
                '"' => return Ok(out),
                '\\' => {
                    let e = self.bump().ok_or(Error::InvalidPath {
                        msg: "Bad escape",
                    })?;
                    match e {
                        '"' | '\\' | '/' => out.push(e),
                        'b' => out.push('\u{0008}'),
                        'f' => out.push('\u{000c}'),
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'u' => {
                            let ch = self.parse_unicode_escape()?;
                            out.push(ch);
                        }
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
        Err(Error::InvalidPath {
            msg: "Unclosed string",
        })
    }

    /// Parse `\uXXXX` (and surrogate pairs) after the `u` has already been consumed.
    fn parse_unicode_escape(&mut self) -> Result<char, Error> {
        let hi = self.read_u4_hex()?;
        if (0xD800..=0xDBFF).contains(&hi) {
            // High surrogate — require `\u` low surrogate.
            if self.peek() != Some('\\') {
                return Err(Error::InvalidPath {
                    msg: "Lone UTF-16 high surrogate in quoted identifier",
                });
            }
            self.bump();
            if self.peek() != Some('u') {
                return Err(Error::InvalidPath {
                    msg: "Expected \\u after high surrogate",
                });
            }
            self.bump();
            let lo = self.read_u4_hex()?;
            if !(0xDC00..=0xDFFF).contains(&lo) {
                return Err(Error::InvalidPath {
                    msg: "Invalid UTF-16 low surrogate",
                });
            }
            let cp = 0x10000 + (((u32::from(hi) - 0xD800) << 10) | (u32::from(lo) - 0xDC00));
            char::from_u32(cp).ok_or(Error::InvalidPath {
                msg: "Invalid Unicode code point",
            })
        } else if (0xDC00..=0xDFFF).contains(&hi) {
            Err(Error::InvalidPath {
                msg: "Lone UTF-16 low surrogate in quoted identifier",
            })
        } else {
            char::from_u32(u32::from(hi)).ok_or(Error::InvalidPath {
                msg: "Invalid Unicode code point",
            })
        }
    }

    fn read_u4_hex(&mut self) -> Result<u16, Error> {
        let mut v = 0u16;
        for _ in 0..4 {
            let c = self.bump().ok_or(Error::InvalidPath {
                msg: "Incomplete \\u escape",
            })?;
            let dig = c.to_digit(16).ok_or(Error::InvalidPath {
                msg: "Invalid hex in \\u escape",
            })?;
            v = (v << 4) | dig as u16;
        }
        Ok(v)
    }

    fn parse_raw_string_literal(&mut self) -> Result<SelectExpr, Error> {
        let raw = self.parse_raw_string_raw()?;
        Ok(SelectExpr::Literal(encode_json_string(&raw)))
    }

    fn parse_raw_string_raw(&mut self) -> Result<String, Error> {
        self.eat('\'')?;
        let mut out = String::new();
        while let Some(c) = self.bump() {
            // Match jmespath.rs: on `\`, always take the next char into the buffer
            // (so `\'` keeps both chars, `\\` keeps both). Closing `'` is only when
            // unescaped. Then replace `\'` → `'`.
            if c == '\\' {
                out.push('\\');
                if let Some(n) = self.bump() {
                    out.push(n);
                }
            } else if c == '\'' {
                return Ok(out.replace("\\'", "'"));
            } else {
                out.push(c);
            }
        }
        Err(Error::InvalidPath {
            msg: "Unclosed raw string",
        })
    }

    fn parse_backtick_literal(&mut self) -> Result<SelectExpr, Error> {
        self.eat('`')?;
        let mut raw = String::new();
        while let Some(c) = self.peek() {
            if c == '`' {
                self.bump();
                let inner = raw.trim();
                // JSON fragment on the wire. Only `\`` is a JMESPath escape inside
                // backticks; other backslashes are JSON escapes and must be preserved.
                return Ok(SelectExpr::Literal(inner.as_bytes().to_vec()));
            }
            if c == '\\' {
                self.bump();
                match self.peek() {
                    Some('`') => {
                        self.bump();
                        raw.push('`');
                    }
                    Some(e) => {
                        // Preserve JSON escape sequences (\u, \", \\, …).
                        raw.push('\\');
                        raw.push(e);
                        self.bump();
                    }
                    None => raw.push('\\'),
                }
            } else {
                raw.push(c);
                self.bump();
            }
        }
        Err(Error::InvalidPath {
            msg: "Unclosed backtick literal",
        })
    }

    fn parse_number_literal(&mut self) -> Result<SelectExpr, Error> {
        self.skip_ws();
        let start = self.i;
        if self.peek() == Some('-') {
            self.bump();
        }
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.bump();
        }
        if self.peek() == Some('.') {
            self.bump();
            while self.peek().is_some_and(|c| c.is_ascii_digit()) {
                self.bump();
            }
        }
        let lit = self.s.as_bytes()[start..self.i].to_vec();
        if lit.is_empty() || lit == b"-" {
            return Err(Error::InvalidPath {
                msg: "Invalid number literal",
            });
        }
        Ok(SelectExpr::Literal(lit))
    }
}

fn encode_json_string(raw: &str) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(raw.len() + 2);
    bytes.push(b'"');
    for b in raw.bytes() {
        match b {
            b'"' | b'\\' => {
                bytes.push(b'\\');
                bytes.push(b);
            }
            c if c < 0x20 => bytes.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
            c => bytes.push(c),
        }
    }
    bytes.push(b'"');
    bytes
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_json_number_lit(b: &[u8]) -> bool {
    let t = b.trim_ascii();
    if t.is_empty() {
        return false;
    }
    t.iter()
        .all(|c| c.is_ascii_digit() || *c == b'-' || *c == b'+' || *c == b'.' || *c == b'e' || *c == b'E')
        && t.iter().any(|c| c.is_ascii_digit())
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn chain_sub(prefix: SelectExpr, suffix: SelectExpr) -> SelectExpr {
    // `expr[]` → Flatten(expr)  (jmespath: Projection(Flatten(expr), Identity))
    if let SelectExpr::Flatten(inner) = &suffix {
        if matches!(inner.as_ref(), SelectExpr::Identity | SelectExpr::Current) {
            return SelectExpr::Flatten(Box::new(prefix));
        }
        return SelectExpr::Flatten(Box::new(chain_sub(prefix, *inner.clone())));
    }
    match prefix {
        SelectExpr::Field(name) => {
            SelectExpr::Sub(Box::new(SelectExpr::Field(name)), Box::new(suffix))
        }
        SelectExpr::FieldQuoted(name) => {
            SelectExpr::Sub(Box::new(SelectExpr::FieldQuoted(name)), Box::new(suffix))
        }
        SelectExpr::ObjectProjection(inner) => {
            // foo.*.bar → project each object value with .bar
            SelectExpr::ObjectProjection(Box::new(chain_into(*inner, suffix)))
        }
        // `foo[].bar` → Sub(Flatten(foo), Each(bar))
        // (jmespath: Projection(Flatten(foo), bar))
        SelectExpr::Flatten(inner) => SelectExpr::Sub(
            Box::new(SelectExpr::Flatten(inner)),
            Box::new(SelectExpr::Array(ArraySelect::Each(Box::new(suffix)))),
        ),
        SelectExpr::Array(ArraySelect::Each(inner)) => {
            SelectExpr::Array(ArraySelect::Each(Box::new(chain_into(*inner, suffix))))
        }
        SelectExpr::Array(ArraySelect::Slice {
            start,
            end,
            step,
            each,
        }) => SelectExpr::Array(ArraySelect::Slice {
            start,
            end,
            step,
            each: Box::new(chain_into(*each, suffix)),
        }),
        SelectExpr::Array(ArraySelect::Filter { pred, each }) => {
            SelectExpr::Array(ArraySelect::Filter {
                pred,
                each: Box::new(chain_into(*each, suffix)),
            })
        }
        SelectExpr::Array(ArraySelect::Indices(mut map)) if map.len() == 1 => {
            let (k, v) = map.drain().next().unwrap();
            map.insert(k, chain_into(v, suffix));
            SelectExpr::Array(ArraySelect::Indices(map))
        }
        SelectExpr::Sub(left, right) => {
            SelectExpr::Sub(left, Box::new(chain_into(*right, suffix)))
        }
        SelectExpr::Expref(inner) => SelectExpr::Expref(Box::new(chain_into(*inner, suffix))),
        SelectExpr::Paren(inner) => chain_sub(*inner, suffix),
        other => SelectExpr::Sub(Box::new(other), Box::new(suffix)),
    }
}

fn chain_into(inner: SelectExpr, suffix: SelectExpr) -> SelectExpr {
    if matches!(inner, SelectExpr::Identity | SelectExpr::Current) {
        suffix
    } else {
        chain_sub(inner, suffix)
    }
}
