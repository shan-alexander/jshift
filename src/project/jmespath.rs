//! JMESPath **subset** parser → [`SelectExpr`].
//!
//! Supported (intentionally growing):
//! * identifiers, `.` descent, `@` current
//! * `[N]`, `[*]` / `[]` (wildcard list projection), `[start:end]` slices
//! * multi-select hash `{k: expr, ...}` and list `[expr, ...]`
//! * pipe `|`
//! * flatten projection trailing `[]` after a pipe: `a[*].b | []`
//! * literals: numbers, `"strings"`, `true` / `false` / `null`
//!
//! Not yet: filters `[?...]`, functions, `||` / `&&`, raw string literals, slices
//! with steps / negatives (partial), parent / root refs beyond `@`.

use crate::error::Error;
use crate::project::select::{ArraySelect, HashField, ProjectPathSegment, SelectExpr};

/// Parse a JMESPath subset expression into a [`SelectExpr`].
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

/// Build [`SelectExpr`] from a keep-list path (`products[].title`) via project path parse.
#[allow(dead_code)] // public helper for tooling / future transforms
pub fn select_from_project_path(path: &str) -> Result<SelectExpr, Error> {
    let segs = parse_project_path(path)?;
    segs_to_select(&segs)
}

/// Parse projection path segments including slices and wildcards.
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
            if inner.is_empty() || inner == "*" {
                segments.push(ProjectPathSegment::ArrayWildcard);
            } else if let Some((a, b)) = inner.split_once(':') {
                let start = if a.trim().is_empty() {
                    0usize
                } else {
                    a.trim().parse().map_err(|_| Error::InvalidPath {
                        msg: "Invalid slice start",
                    })?
                };
                let end = if b.trim().is_empty() {
                    None
                } else {
                    Some(b.trim().parse().map_err(|_| Error::InvalidPath {
                        msg: "Invalid slice end",
                    })?)
                };
                segments.push(ProjectPathSegment::ArraySlice { start, end });
            } else if inner.bytes().all(|b| b.is_ascii_digit()) {
                let idx = inner.parse::<usize>().map_err(|_| Error::InvalidPath {
                    msg: "Array index out of range",
                })?;
                segments.push(ProjectPathSegment::Index(idx));
            } else {
                return Err(Error::InvalidPath {
                    msg: "Invalid array selector (use [N], [], [*], or [start:end])",
                });
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
            ProjectPathSegment::ArraySlice { start, end } => SelectExpr::Array(ArraySelect::Slice {
                start: *start,
                end: *end,
                each: Box::new(expr),
            }),
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

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += c.len_utf8();
        Some(c)
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.bump();
            } else {
                break;
            }
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

    fn parse_pipe(&mut self) -> Result<SelectExpr, Error> {
        let mut left = self.parse_project()?;
        loop {
            self.skip_ws();
            if self.peek() == Some('|') {
                self.bump();
                self.skip_ws();
                // flatten idiom: `| []`
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
                let right = self.parse_project()?;
                left = SelectExpr::Pipe(Box::new(left), Box::new(right));
            } else {
                break;
            }
        }
        Ok(left)
    }

    fn parse_project(&mut self) -> Result<SelectExpr, Error> {
        self.skip_ws();
        // multi-select at start
        if self.peek() == Some('{') {
            return self.parse_multi_hash();
        }
        if self.peek() == Some('[') {
            // could be multi-list, index, wildcard, slice, or literal-ish
            return self.parse_bracket_or_list();
        }
        let mut expr = self.parse_atom()?;
        loop {
            self.skip_ws();
            match self.peek() {
                Some('.') => {
                    self.bump();
                    self.skip_ws();
                    let right = self.parse_atom_key_or_multi()?;
                    expr = chain_sub(expr, right);
                }
                Some('[') => {
                    let bracket = self.parse_bracket_suffix()?;
                    expr = chain_sub(expr, bracket);
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
            Some('"') => self.parse_string_literal(),
            Some(c) if c == '-' || c.is_ascii_digit() => self.parse_number_literal(),
            Some('{') => self.parse_multi_hash(),
            Some('[') => self.parse_bracket_or_list(),
            Some('t') | Some('f') | Some('n') => self.parse_keyword_literal(),
            Some(c) if is_ident_start(c) => {
                let name = self.parse_ident()?;
                Ok(SelectExpr::Field(name))
            }
            Some('(') => {
                self.bump();
                let e = self.parse_pipe()?;
                self.eat(')')?;
                Ok(e)
            }
            _ => Err(Error::InvalidPath {
                msg: "Expected JMESPath atom",
            }),
        }
    }

    fn parse_atom_key_or_multi(&mut self) -> Result<SelectExpr, Error> {
        self.skip_ws();
        if self.peek() == Some('{') {
            return self.parse_multi_hash();
        }
        if self.peek() == Some('[') {
            return self.parse_bracket_or_list();
        }
        let name = self.parse_ident()?;
        Ok(SelectExpr::Field(name))
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
        while let Some(c) = self.peek() {
            if is_ident_continue(c) {
                self.bump();
            } else {
                break;
            }
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
                self.parse_string_raw()?
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

    fn parse_bracket_or_list(&mut self) -> Result<SelectExpr, Error> {
        let save = self.i;
        self.eat('[')?;
        self.skip_ws();
        // empty / wildcard
        if self.peek() == Some(']') {
            self.bump();
            // bare `[]` as expression is flatten of current (rare); treat as identity array each
            return Ok(SelectExpr::Array(ArraySelect::Each(Box::new(
                SelectExpr::Identity,
            ))));
        }
        if self.peek() == Some('*') {
            self.bump();
            self.eat(']')?;
            return Ok(SelectExpr::Array(ArraySelect::Each(Box::new(
                SelectExpr::Identity,
            ))));
        }
        // slice or index or multi-list
        if self.peek().is_some_and(|c| c == '-' || c.is_ascii_digit() || c == ':') {
            // try number / slice
            let inner_start = self.i;
            // read until ]
            while self.peek().is_some_and(|c| c != ']') {
                self.bump();
            }
            let inner = self.s[inner_start..self.i].trim();
            self.eat(']')?;
            if let Some((a, b)) = inner.split_once(':') {
                let start = if a.trim().is_empty() {
                    0
                } else {
                    a.trim().parse().map_err(|_| Error::InvalidPath {
                        msg: "Invalid slice start",
                    })?
                };
                let end = if b.trim().is_empty() {
                    None
                } else {
                    Some(b.trim().parse().map_err(|_| Error::InvalidPath {
                        msg: "Invalid slice end",
                    })?)
                };
                return Ok(SelectExpr::Array(ArraySelect::Slice {
                    start,
                    end,
                    each: Box::new(SelectExpr::Identity),
                }));
            }
            if inner.bytes().all(|b| b.is_ascii_digit()) {
                let idx: usize = inner.parse().map_err(|_| Error::InvalidPath {
                    msg: "Invalid index",
                })?;
                let mut map = std::collections::HashMap::new();
                map.insert(idx, SelectExpr::Identity);
                return Ok(SelectExpr::Array(ArraySelect::Indices(map)));
            }
            // fall through to multi-list — restore and reparse
            self.i = save;
        } else {
            self.i = save;
        }
        // multi-select list
        self.eat('[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(']') {
            self.bump();
            return Ok(SelectExpr::MultiSelectList(items));
        }
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

    fn parse_bracket_suffix(&mut self) -> Result<SelectExpr, Error> {
        // Always a projection suffix: [n], [*], [], [a:b]
        self.eat('[')?;
        self.skip_ws();
        if self.peek() == Some(']') {
            self.bump();
            return Ok(SelectExpr::Array(ArraySelect::Each(Box::new(
                SelectExpr::Identity,
            ))));
        }
        if self.peek() == Some('*') {
            self.bump();
            self.eat(']')?;
            return Ok(SelectExpr::Array(ArraySelect::Each(Box::new(
                SelectExpr::Identity,
            ))));
        }
        let inner_start = self.i;
        while self.peek().is_some_and(|c| c != ']') {
            self.bump();
        }
        let inner = self.s[inner_start..self.i].trim();
        self.eat(']')?;
        if let Some((a, b)) = inner.split_once(':') {
            let start = if a.trim().is_empty() {
                0
            } else {
                a.trim().parse().map_err(|_| Error::InvalidPath {
                    msg: "Invalid slice start",
                })?
            };
            let end = if b.trim().is_empty() {
                None
            } else {
                Some(b.trim().parse().map_err(|_| Error::InvalidPath {
                    msg: "Invalid slice end",
                })?)
            };
            return Ok(SelectExpr::Array(ArraySelect::Slice {
                start,
                end,
                each: Box::new(SelectExpr::Identity),
            }));
        }
        if inner.bytes().all(|b| b.is_ascii_digit()) {
            let idx: usize = inner.parse().map_err(|_| Error::InvalidPath {
                msg: "Invalid index",
            })?;
            let mut map = std::collections::HashMap::new();
            map.insert(idx, SelectExpr::Identity);
            return Ok(SelectExpr::Array(ArraySelect::Indices(map)));
        }
        Err(Error::InvalidPath {
            msg: "Invalid bracket suffix in JMESPath",
        })
    }

    fn parse_string_literal(&mut self) -> Result<SelectExpr, Error> {
        let raw = self.parse_string_raw()?;
        // emit as JSON string
        let mut bytes = Vec::with_capacity(raw.len() + 2);
        bytes.push(b'"');
        for b in raw.bytes() {
            match b {
                b'"' | b'\\' => {
                    bytes.push(b'\\');
                    bytes.push(b);
                }
                c if c < 0x20 => {
                    // simple \u00XX
                    bytes.extend_from_slice(format!("\\u{c:04x}").as_bytes());
                }
                c => bytes.push(c),
            }
        }
        bytes.push(b'"');
        Ok(SelectExpr::Literal(bytes))
    }

    fn parse_string_raw(&mut self) -> Result<String, Error> {
        self.eat('"')?;
        let mut out = String::new();
        while let Some(c) = self.bump() {
            match c {
                '"' => return Ok(out),
                '\\' => {
                    let e = self.bump().ok_or(Error::InvalidPath {
                        msg: "Bad escape in string",
                    })?;
                    out.push(match e {
                        '"' | '\\' | '/' => e,
                        'n' => '\n',
                        't' => '\t',
                        'r' => '\r',
                        'b' => '\u{0008}',
                        'f' => '\u{000c}',
                        _ => e,
                    });
                }
                c => out.push(c),
            }
        }
        Err(Error::InvalidPath {
            msg: "Unclosed string in JMESPath",
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

    fn parse_keyword_literal(&mut self) -> Result<SelectExpr, Error> {
        self.skip_ws();
        if self.s[self.i..].starts_with("true") {
            self.i += 4;
            return Ok(SelectExpr::Literal(b"true".to_vec()));
        }
        if self.s[self.i..].starts_with("false") {
            self.i += 5;
            return Ok(SelectExpr::Literal(b"false".to_vec()));
        }
        if self.s[self.i..].starts_with("null") {
            self.i += 4;
            return Ok(SelectExpr::Literal(b"null".to_vec()));
        }
        // identifier starting with t/f/n
        let name = self.parse_ident()?;
        Ok(SelectExpr::Field(name))
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// Chain `prefix` then `suffix` as nested selection.
///
/// `products` is [`SelectExpr::Field`]; `products[*]` becomes Field then Each;
/// `products[*].{…}` applies multi-hash per element.
fn chain_sub(prefix: SelectExpr, suffix: SelectExpr) -> SelectExpr {
    match prefix {
        SelectExpr::Field(name) => {
            // Field then suffix: descend into field, then apply suffix (via Sub).
            SelectExpr::Sub(Box::new(SelectExpr::Field(name)), Box::new(suffix))
        }
        SelectExpr::Object(mut obj) if obj.len() == 1 => {
            let key = obj.keys().next().unwrap().to_string();
            let inner = obj.fields.remove(&key).unwrap_or(SelectExpr::Identity);
            let child = chain_into(inner, suffix);
            obj.insert(key, child);
            SelectExpr::Object(obj)
        }
        SelectExpr::Array(ArraySelect::Each(inner)) => {
            SelectExpr::Array(ArraySelect::Each(Box::new(chain_into(*inner, suffix))))
        }
        SelectExpr::Array(ArraySelect::Slice { start, end, each }) => {
            SelectExpr::Array(ArraySelect::Slice {
                start,
                end,
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
