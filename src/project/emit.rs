//! Projection emitter: walk input spans, write output per [`SelectExpr`].

use crate::convert::escape_json_key;
use crate::error::Error;
use crate::project::plan::{MissingPolicy, ProjectPlan, ProjectStyle};
use crate::project::select::{
    resolve_index, resolve_slice, ArraySelect, CmpOp, HashField, ObjectSelect, SelectExpr,
};
use crate::project::sink::EmitOut;
use crate::scan::{find_string_end, skip_value, skip_whitespace};

pub(crate) struct EmitCtx<'a> {
    pub plan: &'a ProjectPlan,
    pub depth: usize,
}

pub(crate) fn emit_value(
    json: &[u8],
    start: usize,
    end: usize,
    expr: &SelectExpr,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    match expr {
        SelectExpr::Identity | SelectExpr::Current => {
            out.emit_bytes(&json[start..end])?;
            Ok(())
        }
        SelectExpr::Field(key) | SelectExpr::FieldQuoted(key) => {
            // Logical key → on-wire escaped form for matching document bytes.
            let wire = escape_json_key(key);
            match find_object_value_span(json, start, end, wire.as_bytes()) {
                Ok((s, e)) => {
                    out.emit_bytes(&json[s..e])?;
                    Ok(())
                }
                // JMESPath: missing field / non-object → null (when soft policy).
                Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. }) if soft_null(ctx) => {
                    out.emit_bytes(b"null")?;
                    Ok(())
                }
                Err(e) => Err(e),
            }
        }
        SelectExpr::Literal(bytes) => {
            out.emit_bytes(bytes)?;
            Ok(())
        }
        SelectExpr::Object(sel) => emit_object(json, start, end, sel, ctx, out),
        SelectExpr::Array(sel) => emit_array(json, start, end, sel, ctx, out),
        SelectExpr::MultiSelectHash(fields) => emit_multi_hash(json, start, end, fields, ctx, out),
        SelectExpr::MultiSelectList(items) => emit_multi_list(json, start, end, items, ctx, out),
        SelectExpr::Pipe(left, right) => {
            let mut mid = Vec::new();
            emit_value(json, start, end, left, ctx, &mut mid)?;
            let m0 = skip_whitespace(&mid, 0);
            if m0 >= mid.len() {
                return Err(Error::InvalidJsonSyntax {
                    pos: 0,
                    msg: "Empty pipe intermediate",
                });
            }
            let m1 = skip_value(&mid, m0)?;
            emit_value(&mid, m0, m1, right, ctx, out)
        }
        SelectExpr::Flatten(inner) => {
            let mut mid = Vec::new();
            emit_value(json, start, end, inner, ctx, &mut mid)?;
            flatten_emit(&mid, ctx, out)
        }
        SelectExpr::Sub(left, right) => match resolve_focus(json, start, end, left) {
            Ok(Some((s, e))) => emit_value(json, s, e, right, ctx, out),
            Ok(None) if soft_null(ctx) => {
                // Missing intermediate (JMESPath → null).
                out.emit_bytes(b"null")?;
                Ok(())
            }
            Ok(None) => Err(Error::PathNotFound),
            Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. }) if soft_null(ctx) => {
                out.emit_bytes(b"null")?;
                Ok(())
            }
            Err(e) => Err(e),
        },
        SelectExpr::Cmp { op, left, right } => {
            let lv = eval_buf(json, start, end, left, ctx)?;
            let rv = eval_buf(json, start, end, right, ctx)?;
            // JMESPath: incomparable types → null (not false).
            match cmp_values(&lv, &rv, *op) {
                Some(t) => {
                    out.emit_bytes(if t { b"true" } else { b"false" })?;
                    Ok(())
                }
                None => {
                    out.emit_bytes(b"null")?;
                    Ok(())
                }
            }
        }
        SelectExpr::And(a, b) => {
            // JMESPath && : if left is falsey return **left value**; else return right.
            let av = eval_buf(json, start, end, a, ctx)?;
            if !is_truthy(&av) {
                out.emit_bytes(trim_json(&av))?;
                return Ok(());
            }
            emit_value(json, start, end, b, ctx, out)
        }
        SelectExpr::Or(a, b) => {
            // JMESPath || : if left is truthy return left; else return right.
            let av = eval_buf(json, start, end, a, ctx)?;
            if is_truthy(&av) {
                out.emit_bytes(trim_json(&av))?;
                return Ok(());
            }
            emit_value(json, start, end, b, ctx, out)
        }
        SelectExpr::Not(inner) => {
            let v = eval_buf(json, start, end, inner, ctx)?;
            out.emit_bytes(if is_truthy(&v) { b"false" } else { b"true" })?;
            Ok(())
        }
        SelectExpr::Call { name, args } => emit_call(json, start, end, name, args, ctx, out),
        SelectExpr::Expref(inner) => {
            // Bare expref is invalid at runtime; treat as identity of current for debug.
            emit_value(json, start, end, inner, ctx, out)
        }
        SelectExpr::ObjectProjection(each) => emit_object_projection(json, start, end, each, ctx, out),
        SelectExpr::Paren(inner) => emit_value(json, start, end, inner, ctx, out),
    }
}

fn unwrap_expref(expr: &SelectExpr) -> &SelectExpr {
    match expr {
        SelectExpr::Expref(inner) | SelectExpr::Paren(inner) => unwrap_expref(inner),
        other => other,
    }
}

fn emit_object_projection(
    json: &[u8],
    start: usize,
    end: usize,
    each: &SelectExpr,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    let (_, members, _) = match collect_object_members(json, start, end) {
        Ok(x) => x,
        Err(Error::TypeMismatch { .. }) if soft_null(ctx) => {
            out.emit_bytes(b"null")?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    // JMESPath projections omit null results.
    emit_projection_list(
        members
            .iter()
            .map(|m| (json, m.val_start, m.val_end))
            .collect(),
        each,
        ctx,
        out,
    )
}

/// Emit `[...]` from evaluating `each` on each span; skip JSON null (projection rule).
fn emit_projection_list(
    spans: Vec<(&[u8], usize, usize)>,
    each: &SelectExpr,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    out.emit_byte(b'[')?;
    let mut first = true;
    for (buf, s, e) in spans {
        let mut piece = Vec::new();
        emit_value(buf, s, e, each, ctx, &mut piece)?;
        if trim_json(&piece) == b"null" {
            continue;
        }
        if !first {
            out.emit_byte(b',')?;
        }
        maybe_pretty_newline_indent(ctx, out, true)?;
        ctx.depth += 1;
        out.emit_bytes(&piece)?;
        ctx.depth -= 1;
        first = false;
    }
    emit_close_array(ctx, out)?;
    Ok(())
}

fn soft_null(ctx: &EmitCtx<'_>) -> bool {
    ctx.plan.missing == MissingPolicy::Skip
}

fn eval_buf(
    json: &[u8],
    start: usize,
    end: usize,
    expr: &SelectExpr,
    ctx: &mut EmitCtx<'_>,
) -> Result<Vec<u8>, Error> {
    let mut v = Vec::new();
    match emit_value(json, start, end, expr, ctx, &mut v) {
        Ok(()) => Ok(v),
        // Soft failures → JSON null (JMESPath “no value”).
        Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. }) | Err(Error::IndexOutOfBounds { .. })
            if soft_null(ctx) =>
        {
            Ok(b"null".to_vec())
        }
        Err(e) => Err(e),
    }
}

/// JMESPath truthiness: false, null, empty string/array/object are falsey.
pub(crate) fn is_truthy(v: &[u8]) -> bool {
    let s = skip_whitespace(v, 0);
    let e = match skip_value(v, s) {
        Ok(x) => x,
        Err(_) => return false,
    };
    let slice = &v[s..e];
    match slice {
        b"null" | b"false" => false,
        b"true" => true,
        b"\"\"" => false,
        b"[]" | b"{}" => false,
        x if x.starts_with(b"[") || x.starts_with(b"{") || x.starts_with(b"\"") => true,
        x if x.first().is_some_and(|c| c.is_ascii_digit() || *c == b'-') => {
            // numbers: 0 is truthy in JMESPath
            true
        }
        _ => !slice.is_empty(),
    }
}

/// Compare two JSON values. `None` means incomparable → JMESPath `null`.
fn cmp_values(left: &[u8], right: &[u8], op: CmpOp) -> Option<bool> {
    let l = trim_json(left);
    let r = trim_json(right);

    // Structural equality for objects/arrays (filters like `key == \`{"a":1}\``).
    if matches!(op, CmpOp::Eq | CmpOp::Ne)
        && (l.starts_with(b"{") || l.starts_with(b"[") || r.starts_with(b"{") || r.starts_with(b"["))
    {
        let eq = json_struct_equal(l, r);
        return Some(match op {
            CmpOp::Eq => eq,
            CmpOp::Ne => !eq,
            _ => unreachable!(),
        });
    }

    // null equality / inequality only; order with null → null
    if l == b"null" || r == b"null" {
        return match op {
            CmpOp::Eq => Some(l == r),
            CmpOp::Ne => Some(l != r),
            _ => None,
        };
    }
    // boolean — only eq/ne with booleans
    if (l == b"true" || l == b"false") && (r == b"true" || r == b"false") {
        let lb = l == b"true";
        let rb = r == b"true";
        return match op {
            CmpOp::Eq => Some(lb == rb),
            CmpOp::Ne => Some(lb != rb),
            _ => None,
        };
    }
    // numbers
    if let (Ok(ln), Ok(rn)) = (parse_f64(l), parse_f64(r)) {
        return Some(match op {
            CmpOp::Eq => ln == rn,
            CmpOp::Ne => ln != rn,
            CmpOp::Lt => ln < rn,
            CmpOp::Le => ln <= rn,
            CmpOp::Gt => ln > rn,
            CmpOp::Ge => ln >= rn,
        });
    }
    // strings (JSON quoted)
    if let (Some(ls), Some(rs)) = (json_string_content(l), json_string_content(r)) {
        return Some(match op {
            CmpOp::Eq => ls == rs,
            CmpOp::Ne => ls != rs,
            CmpOp::Lt => ls < rs,
            CmpOp::Le => ls <= rs,
            CmpOp::Gt => ls > rs,
            CmpOp::Ge => ls >= rs,
        });
    }
    // incomparable types
    None
}

/// Deep structural equality for JSON objects/arrays (order-sensitive for arrays;
/// objects compared by key multiset of wire form).
fn json_struct_equal(a: &[u8], b: &[u8]) -> bool {
    let a = trim_json(a);
    let b = trim_json(b);
    if a == b {
        return true;
    }
    if a.first() != b.first() {
        return false;
    }
    match a.first() {
        Some(b'[') => {
            let ea = match collect_array_elems(a, 0, a.len()) {
                Ok(v) => v,
                Err(_) => return false,
            };
            let eb = match collect_array_elems(b, 0, b.len()) {
                Ok(v) => v,
                Err(_) => return false,
            };
            if ea.len() != eb.len() {
                return false;
            }
            ea.iter().zip(eb.iter()).all(|(&(sa, ea_), &(sb, eb_))| {
                json_struct_equal(&a[sa..ea_], &b[sb..eb_])
            })
        }
        Some(b'{') => {
            let (_, ma, _) = match collect_object_members(a, 0, a.len()) {
                Ok(v) => v,
                Err(_) => return false,
            };
            let (_, mb, _) = match collect_object_members(b, 0, b.len()) {
                Ok(v) => v,
                Err(_) => return false,
            };
            if ma.len() != mb.len() {
                return false;
            }
            // Compare as sets of (key, value)
            for ma_ in &ma {
                let found = mb.iter().any(|mb_| {
                    ma_.key_on_wire == mb_.key_on_wire
                        && json_struct_equal(
                            &a[ma_.val_start..ma_.val_end],
                            &b[mb_.val_start..mb_.val_end],
                        )
                });
                if !found {
                    return false;
                }
            }
            true
        }
        _ => a == b,
    }
}

fn trim_json(v: &[u8]) -> &[u8] {
    let s = skip_whitespace(v, 0);
    let e = skip_value(v, s).unwrap_or(v.len());
    let mut end = e;
    while end > s && matches!(v[end - 1], b' ' | b'\t' | b'\n' | b'\r') {
        end -= 1;
    }
    &v[s..end]
}

fn parse_f64(v: &[u8]) -> Result<f64, ()> {
    let s = std::str::from_utf8(v).map_err(|_| ())?;
    s.parse().map_err(|_| ())
}

fn json_string_content(v: &[u8]) -> Option<&[u8]> {
    if v.len() >= 2 && v[0] == b'"' && v[v.len() - 1] == b'"' {
        Some(&v[1..v.len() - 1])
    } else {
        None
    }
}

/// If `expr` is a pure focus (single key identity / single index), return child span.
fn resolve_focus(
    json: &[u8],
    start: usize,
    end: usize,
    expr: &SelectExpr,
) -> Result<Option<(usize, usize)>, Error> {
    match expr {
        SelectExpr::Identity | SelectExpr::Current => Ok(Some((start, end))),
        SelectExpr::Paren(inner) => resolve_focus(json, start, end, inner),
        SelectExpr::Field(key) | SelectExpr::FieldQuoted(key) => {
            let wire = escape_json_key(key);
            match find_object_value_span(json, start, end, wire.as_bytes()) {
                Ok((s, e)) => Ok(Some((s, e))),
                Err(Error::PathNotFound) | Err(Error::TypeMismatch { .. }) => Ok(None),
                Err(e) => Err(e),
            }
        }
        SelectExpr::Object(obj) if obj.len() == 1 => {
            let key = obj.keys().next().unwrap();
            let child = obj.get(key).unwrap();
            let (s, e) = find_object_value_span(json, start, end, key.as_bytes())?;
            if matches!(child, SelectExpr::Identity | SelectExpr::Current) {
                Ok(Some((s, e)))
            } else {
                resolve_focus(json, s, e, child)
            }
        }
        SelectExpr::Array(ArraySelect::Indices(map)) if map.len() == 1 => {
            let (idx, child) = map.iter().next().unwrap();
            let elems = collect_array_elems(json, start, end)?;
            let i = resolve_index(*idx, elems.len()).ok_or(Error::PathNotFound)?;
            let (s, e) = elems[i];
            if matches!(child, SelectExpr::Identity | SelectExpr::Current) {
                Ok(Some((s, e)))
            } else {
                resolve_focus(json, s, e, child)
            }
        }
        SelectExpr::Array(ArraySelect::Each(_))
        | SelectExpr::Array(ArraySelect::Slice { .. })
        | SelectExpr::Array(ArraySelect::Filter { .. }) => Ok(None),
        SelectExpr::Sub(left, right) => {
            if let Some((s, e)) = resolve_focus(json, start, end, left)? {
                resolve_focus(json, s, e, right)
            } else {
                Ok(None)
            }
        }
        _ => Ok(None),
    }
}

struct Member<'a> {
    key_on_wire: &'a [u8],
    key_span: (usize, usize),
    colon: usize,
    val_start: usize,
    val_end: usize,
    member_start: usize,
    /// End of member value; next is comma or `}`.
    after_value: usize,
    /// Inclusive end of trailing comma if present (for preserve).
    comma: Option<usize>,
}

fn collect_object_members<'a>(
    json: &'a [u8],
    start: usize,
    end: usize,
) -> Result<(usize, Vec<Member<'a>>, usize), Error> {
    let start = skip_whitespace(json, start);
    if start >= json.len() || json[start] != b'{' {
        return Err(Error::TypeMismatch {
            expected: "object",
            found: type_name_at(json, start),
        });
    }
    let mut members = Vec::new();
    let mut pos = start + 1;
    loop {
        let ws_before = pos;
        pos = skip_whitespace(json, pos);
        if pos >= end {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unclosed object in project",
            });
        }
        if json[pos] == b'}' {
            return Ok((start, members, pos));
        }
        if json[pos] != b'"' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected object key in project",
            });
        }
        let key_open = pos;
        let key_inner_end = find_string_end(json, pos + 1)?;
        let key_on_wire = &json[pos + 1..key_inner_end];
        pos = key_inner_end + 1;
        pos = skip_whitespace(json, pos);
        if pos >= json.len() || json[pos] != b':' {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ':' in project object",
            });
        }
        let colon = pos;
        pos += 1;
        pos = skip_whitespace(json, pos);
        let val_start = pos;
        let val_end = skip_value(json, val_start)?;
        let after_value = val_end;
        pos = skip_whitespace(json, val_end);
        let comma = if pos < json.len() && json[pos] == b',' {
            let c = pos;
            pos += 1;
            Some(c)
        } else {
            None
        };
        members.push(Member {
            key_on_wire,
            key_span: (key_open, key_inner_end + 1),
            colon,
            val_start,
            val_end,
            member_start: key_open,
            after_value,
            comma,
        });
        let _ = ws_before;
        if comma.is_none() && pos < json.len() && json[pos] == b'}' {
            // continue to see }
        } else if comma.is_none() && (pos >= json.len() || json[pos] != b'}') {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ',' or '}' in project object",
            });
        }
    }
}

fn find_object_value_span(
    json: &[u8],
    start: usize,
    end: usize,
    key: &[u8],
) -> Result<(usize, usize), Error> {
    let (_, members, _) = collect_object_members(json, start, end)?;
    for m in members {
        if m.key_on_wire == key {
            return Ok((m.val_start, m.val_end));
        }
    }
    Err(Error::PathNotFound)
}

fn collect_array_elems(json: &[u8], start: usize, end: usize) -> Result<Vec<(usize, usize)>, Error> {
    let start = skip_whitespace(json, start);
    if start >= json.len() || json[start] != b'[' {
        return Err(Error::TypeMismatch {
            expected: "array",
            found: type_name_at(json, start),
        });
    }
    let mut elems = Vec::new();
    let mut pos = start + 1;
    pos = skip_whitespace(json, pos);
    if pos < end && json[pos] == b']' {
        return Ok(elems);
    }
    loop {
        pos = skip_whitespace(json, pos);
        if pos >= end {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unclosed array in project",
            });
        }
        if json[pos] == b']' {
            break;
        }
        let e_start = pos;
        let e_end = skip_value(json, e_start)?;
        elems.push((e_start, e_end));
        pos = skip_whitespace(json, e_end);
        if pos >= json.len() {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Unexpected EOF in project array",
            });
        }
        if json[pos] == b',' {
            pos += 1;
        } else if json[pos] == b']' {
            // next
        } else {
            return Err(Error::InvalidJsonSyntax {
                pos,
                msg: "Expected ',' or ']' in project array",
            });
        }
    }
    Ok(elems)
}

fn emit_object(
    json: &[u8],
    start: usize,
    end: usize,
    sel: &ObjectSelect,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    let (obj_start, members, close_pos) = match collect_object_members(json, start, end) {
        Ok(x) => x,
        Err(Error::TypeMismatch { .. }) if soft_null(ctx) => {
            out.emit_bytes(b"null")?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };

    let mut kept: Vec<(&Member<'_>, &SelectExpr)> = Vec::new();
    for m in &members {
        let key_str = std::str::from_utf8(m.key_on_wire).map_err(|_| Error::InvalidJsonSyntax {
            pos: m.key_span.0,
            msg: "Object key is not UTF-8",
        })?;
        if let Some(child) = sel.fields.get(key_str) {
            kept.push((m, child));
        }
    }

    if ctx.plan.missing == MissingPolicy::Error {
        for key in sel.fields.keys() {
            if !members.iter().any(|m| m.key_on_wire == key.as_bytes()) {
                return Err(Error::PathNotFound);
            }
        }
    }

    out.emit_byte(b'{')?;
    // PreserveSource: leading ws after `{` when first kept is first member.
    if matches!(ctx.plan.style, ProjectStyle::PreserveSource)
        && let Some((first_m, _)) = kept.first()
        && members
            .first()
            .is_some_and(|m| m.member_start == first_m.member_start)
    {
        let between = &json[obj_start + 1..first_m.member_start];
        if is_ws_only(between) {
            out.emit_bytes(between)?;
        }
    }

    for (i, (m, child)) in kept.iter().enumerate() {
        if i > 0 {
            emit_member_sep(json, &kept, i, ctx, out)?;
        }
        emit_object_key(json, m, ctx, out)?;
        emit_colon(json, m, ctx, out)?;
        ctx.depth += 1;
        emit_value(json, m.val_start, m.val_end, child, ctx, out)?;
        ctx.depth -= 1;
        // PreserveSource: trailing space after value before comma (not last)
        if matches!(ctx.plan.style, ProjectStyle::PreserveSource) && i + 1 < kept.len() {
            // copy ws after value up to comma if original had comma after this member
            if let Some(c) = m.comma {
                let mid = &json[m.after_value..c];
                if is_ws_only(mid) {
                    out.emit_bytes(mid)?;
                }
            }
        }
    }

    // PreserveSource: whitespace after last kept value (before its comma or `}`).
    if matches!(ctx.plan.style, ProjectStyle::PreserveSource)
        && let Some((last_m, _)) = kept.last()
    {
        if let Some(c) = last_m.comma {
            let between = &json[last_m.after_value..c];
            if is_ws_only(between) {
                out.emit_bytes(between)?;
            }
        } else {
            let between = &json[last_m.after_value..close_pos];
            if is_ws_only(between) {
                out.emit_bytes(between)?;
            }
        }
    }

    emit_close_object(ctx, out)?;
    let _ = end;
    Ok(())
}

fn emit_member_sep(
    json: &[u8],
    kept: &[(&Member<'_>, &SelectExpr)],
    i: usize,
    ctx: &EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    match ctx.plan.style {
        ProjectStyle::PreserveSource => {
            // Prefer original comma between adjacent original neighbors.
            let prev = kept[i - 1].0;
            let curr = kept[i].0;
            if let Some(c) = prev.comma {
                // from comma through whitespace before curr key
                out.emit_byte(b',')?;
                let between = &json[c + 1..curr.member_start];
                if is_ws_only(between) {
                    out.emit_bytes(between)?;
                }
            } else {
                out.emit_byte(b',')?;
            }
        }
        ProjectStyle::Compact | ProjectStyle::Pretty { .. } => out.emit_byte(b',')?,
    }
    Ok(())
}

fn emit_array(
    json: &[u8],
    start: usize,
    end: usize,
    sel: &ArraySelect,
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    let elems = match collect_array_elems(json, start, end) {
        Ok(e) => e,
        Err(Error::TypeMismatch { .. }) if soft_null(ctx) => {
            out.emit_bytes(b"null")?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let mut kept: Vec<(usize, usize, SelectExpr)> = Vec::new();
    match sel {
        ArraySelect::Each(child) => {
            for &(s, e) in &elems {
                kept.push((s, e, (*child.as_ref()).clone()));
            }
        }
        ArraySelect::Indices(map) => {
            let mut keys: Vec<i64> = map.keys().copied().collect();
            keys.sort_unstable();
            // JMESPath: a single index expression yields the element, not a 1-array.
            if map.len() == 1 {
                let k = keys[0];
                match resolve_index(k, elems.len()) {
                    Some(i) => {
                        let (s, e) = elems[i];
                        return emit_value(json, s, e, map.get(&k).unwrap(), ctx, out);
                    }
                    None if ctx.plan.missing == MissingPolicy::Error => {
                        return Err(Error::PathNotFound);
                    }
                    None => {
                        out.emit_bytes(b"null")?;
                        return Ok(());
                    }
                }
            }
            for k in keys {
                match resolve_index(k, elems.len()) {
                    Some(i) => {
                        let (s, e) = elems[i];
                        kept.push((s, e, map.get(&k).unwrap().clone()));
                    }
                    None if ctx.plan.missing == MissingPolicy::Error => {
                        return Err(Error::PathNotFound);
                    }
                    None => {}
                }
            }
        }
        ArraySelect::Slice {
            start: st,
            end: en,
            step,
            each,
        } => {
            for i in resolve_slice(elems.len(), *st, *en, *step) {
                let (s, e) = elems[i];
                kept.push((s, e, (*each.as_ref()).clone()));
            }
        }
        ArraySelect::Filter { pred, each } => {
            for &(s, e) in &elems {
                let pv = eval_buf(json, s, e, pred, ctx)?;
                if is_truthy(&pv) {
                    kept.push((s, e, (*each.as_ref()).clone()));
                }
            }
        }
    }

    // JMESPath list projections (`[*]`, slices, filters+project) omit nulls.
    // Index-only multi selections with >1 index still use kept list as-is.
    let omit_nulls = matches!(
        sel,
        ArraySelect::Each(_) | ArraySelect::Slice { .. } | ArraySelect::Filter { .. }
    );
    let arr_open = skip_whitespace(json, start);
    out.emit_byte(b'[')?;
    // PreserveSource: whitespace after `[` when first kept is first element.
    if matches!(ctx.plan.style, ProjectStyle::PreserveSource)
        && let Some((fs, _, _)) = kept.first()
        && elems.first().is_some_and(|&(s, _)| s == *fs)
    {
        let between = &json[arr_open + 1..*fs];
        if is_ws_only(between) {
            out.emit_bytes(between)?;
        }
    }
    let mut first = true;
    let mut prev_end: Option<usize> = None;
    for (s, e, child) in &kept {
        let mut piece = Vec::new();
        emit_value(json, *s, *e, child, ctx, &mut piece)?;
        if omit_nulls && trim_json(&piece) == b"null" {
            continue;
        }
        if !first {
            if matches!(ctx.plan.style, ProjectStyle::PreserveSource)
                && let Some(pe) = prev_end
            {
                // Copy original comma + whitespace between consecutive source elements.
                let p = skip_whitespace(json, pe);
                if p < json.len() && json[p] == b',' {
                    out.emit_byte(b',')?;
                    let between = &json[p + 1..*s];
                    if is_ws_only(between) {
                        out.emit_bytes(between)?;
                    }
                } else {
                    out.emit_byte(b',')?;
                }
            } else {
                out.emit_byte(b',')?;
            }
        }
        maybe_pretty_newline_indent(ctx, out, true)?;
        ctx.depth += 1;
        out.emit_bytes(&piece)?;
        ctx.depth -= 1;
        first = false;
        prev_end = Some(*e);
    }
    emit_close_array(ctx, out)?;
    let _ = end;
    Ok(())
}

fn emit_call(
    json: &[u8],
    start: usize,
    end: usize,
    name: &str,
    args: &[SelectExpr],
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        "length" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            let n = length_of(&v)?;
            out.emit_bytes(n.to_string().as_bytes())?;
            Ok(())
        }
        "keys" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            keys_of(&v, out)
        }
        "values" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            values_of(&v, out)
        }
        "type" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            let t = type_name_json(&v);
            write_json_string_out(out, t)?;
            Ok(())
        }
        "to_string" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            let t = trim_json(&v);
            if t.starts_with(b"\"") {
                // Already a JSON string.
                out.emit_bytes(t)?;
            } else {
                // Numbers, bools, null, objects, arrays → JSON text as a string value.
                write_json_string_out(out, std::str::from_utf8(t).unwrap_or(""))?;
            }
            Ok(())
        }
        "to_number" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            let t = trim_json(&v);
            if let Some(s) = json_string_content(t) {
                // Unescape for parse (simple path: raw content without escapes common in suite).
                if let Ok(s) = std::str::from_utf8(s) {
                    if s.parse::<f64>().is_ok() {
                        out.emit_bytes(s.as_bytes())?;
                        return Ok(());
                    }
                }
            } else if parse_f64(t).is_ok() {
                out.emit_bytes(t)?;
                return Ok(());
            }
            out.emit_bytes(b"null")?;
            Ok(())
        }
        "starts_with" | "ends_with" => {
            require_arity(&name, args, 2)?;
            let a = eval_buf(json, start, end, &args[0], ctx)?;
            let b = eval_buf(json, start, end, &args[1], ctx)?;
            let at = trim_json(&a);
            let bt = trim_json(&b);
            let as_ = json_string_content(at).ok_or(Error::Jmespath {
                msg: "starts_with/ends_with require string subject",
            })?;
            let bs = json_string_content(bt).ok_or(Error::Jmespath {
                msg: "starts_with/ends_with require string prefix/suffix",
            })?;
            // Compare unescaped logical content when possible
            let as_u = unescape_string_content(as_).unwrap_or_else(|| as_.to_vec());
            let bs_u = unescape_string_content(bs).unwrap_or_else(|| bs.to_vec());
            let ok = if name == "starts_with" {
                as_u.starts_with(&bs_u)
            } else {
                as_u.ends_with(&bs_u)
            };
            out.emit_bytes(if ok { b"true" } else { b"false" })?;
            Ok(())
        }
        "contains" => {
            require_arity(&name, args, 2)?;
            let a = eval_buf(json, start, end, &args[0], ctx)?;
            let b = eval_buf(json, start, end, &args[1], ctx)?;
            let at = trim_json(&a);
            let bt = trim_json(&b);
            let ok = if at.starts_with(b"[") {
                array_contains(at, bt)?
            } else if let Some(as_) = json_string_content(at) {
                let bs = json_string_content(bt).ok_or(Error::Jmespath {
                    msg: "contains on string requires string search",
                })?;
                let as_u = unescape_string_content(as_).unwrap_or_else(|| as_.to_vec());
                let bs_u = unescape_string_content(bs).unwrap_or_else(|| bs.to_vec());
                as_u.windows(bs_u.len()).any(|w| w == bs_u.as_slice())
            } else {
                return Err(Error::Jmespath {
                    msg: "contains requires string or array subject",
                });
            };
            out.emit_bytes(if ok { b"true" } else { b"false" })?;
            Ok(())
        }
        "not_null" => {
            if args.is_empty() {
                return Err(Error::Jmespath {
                    msg: "not_null requires at least one argument",
                });
            }
            for a in args {
                let v = eval_buf(json, start, end, a, ctx)?;
                if trim_json(&v) != b"null" {
                    out.emit_bytes(trim_json(&v))?;
                    return Ok(());
                }
            }
            out.emit_bytes(b"null")?;
            Ok(())
        }
        "reverse" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            reverse_array_or_string(&v, out)
        }
        "sort" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            sort_array(&v, out)
        }
        "join" => {
            require_arity(&name, args, 2)?;
            let sep_v = eval_buf(json, start, end, &args[0], ctx)?;
            let arr_v = eval_buf(json, start, end, &args[1], ctx)?;
            let sep = json_string_content(trim_json(&sep_v)).ok_or(Error::Jmespath {
                msg: "join separator must be a string",
            })?;
            join_array(trim_json(&arr_v), sep, out)
        }
        "max" | "min" | "sum" | "avg" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            array_reduce(trim_json(&v), name.as_str(), out)
        }
        "abs" | "ceil" | "floor" => {
            require_arity(&name, args, 1)?;
            let v = eval_buf(json, start, end, &args[0], ctx)?;
            let n = parse_f64(trim_json(&v)).map_err(|_| Error::Jmespath {
                msg: "abs/ceil/floor require a number",
            })?;
            let r = match name.as_str() {
                "abs" => n.abs(),
                "ceil" => n.ceil(),
                _ => n.floor(),
            };
            out.emit_bytes(format_number(r).as_bytes())?;
            Ok(())
        }
        "to_array" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            let t = trim_json(&v);
            if t.starts_with(b"[") {
                out.emit_bytes(t)?;
            } else {
                out.emit_byte(b'[')?;
                out.emit_bytes(t)?;
                out.emit_byte(b']')?;
            }
            Ok(())
        }
        "merge" => {
            // shallow merge of objects
            out.emit_byte(b'{')?;
            let mut first = true;
            for a in args {
                let v = eval_buf(json, start, end, a, ctx)?;
                let t = trim_json(&v);
                if !t.starts_with(b"{") {
                    continue;
                }
                let (_, members, _) = collect_object_members(t, 0, t.len())?;
                for m in members {
                    if !first {
                        out.emit_byte(b',')?;
                    }
                    out.emit_bytes(&t[m.key_span.0..m.key_span.1])?;
                    out.emit_byte(b':')?;
                    out.emit_bytes(&t[m.val_start..m.val_end])?;
                    first = false;
                }
            }
            out.emit_byte(b'}')?;
            Ok(())
        }
        // map(&expr, array) — apply expr to each element
        "map" => {
            if args.len() < 2 {
                return Err(Error::Jmespath {
                    msg: "map requires (&expression, array)",
                });
            }
            let mapper = unwrap_expref(&args[0]).clone();
            let arr_v = eval_buf(json, start, end, &args[1], ctx)?;
            let arr = trim_json(&arr_v);
            let elems = collect_array_elems(arr, 0, arr.len())?;
            out.emit_byte(b'[')?;
            for (i, &(s, e)) in elems.iter().enumerate() {
                if i > 0 {
                    out.emit_byte(b',')?;
                }
                let mut piece = Vec::new();
                match emit_value(arr, s, e, &mapper, ctx, &mut piece) {
                    Ok(()) => out.emit_bytes(&piece)?,
                    Err(Error::PathNotFound) => out.emit_bytes(b"null")?,
                    Err(err) => return Err(err),
                }
            }
            out.emit_byte(b']')?;
            Ok(())
        }
        // sort_by(array, &expr)
        "sort_by" => {
            if args.len() < 2 {
                return Err(Error::Jmespath {
                    msg: "sort_by requires (array, &expression)",
                });
            }
            let arr_v = eval_buf(json, start, end, &args[0], ctx)?;
            let key_expr = unwrap_expref(&args[1]).clone();
            let arr = trim_json(&arr_v);
            if !arr.starts_with(b"[") {
                if soft_null(ctx) {
                    out.emit_bytes(b"null")?;
                    return Ok(());
                }
                return Err(Error::TypeMismatch {
                    expected: "array",
                    found: type_name_json(arr),
                });
            }
            let elems = collect_array_elems(arr, 0, arr.len())?;
            let mut keyed: Vec<(Vec<u8>, usize, usize)> = Vec::with_capacity(elems.len());
            for &(s, e) in &elems {
                let key = eval_buf(arr, s, e, &key_expr, ctx).unwrap_or_else(|_| b"null".to_vec());
                keyed.push((trim_json(&key).to_vec(), s, e));
            }
            for i in 0..keyed.len() {
                for j in i + 1..keyed.len() {
                    if keyed[i].0 != b"null" && keyed[j].0 != b"null" {
                        let _ = cmp_sort_keys_strict(&keyed[i].0, &keyed[j].0)?;
                    }
                }
            }
            keyed.sort_by(|a, b| {
                match (a.0.as_slice() == b"null", b.0.as_slice() == b"null") {
                    (true, true) => std::cmp::Ordering::Equal,
                    (true, false) => std::cmp::Ordering::Greater,
                    (false, true) => std::cmp::Ordering::Less,
                    (false, false) => cmp_sort_keys(&a.0, &b.0),
                }
            });
            out.emit_byte(b'[')?;
            for (i, (_, s, e)) in keyed.iter().enumerate() {
                if i > 0 {
                    out.emit_byte(b',')?;
                }
                out.emit_bytes(&arr[*s..*e])?;
            }
            out.emit_byte(b']')?;
            Ok(())
        }
        // group_by(array, &expr) → array of groups (arrays of original elements)
        "group_by" => {
            if args.len() < 2 {
                return Err(Error::Jmespath {
                    msg: "group_by requires (array, &expression)",
                });
            }
            let arr_v = eval_buf(json, start, end, &args[0], ctx)?;
            let key_expr = unwrap_expref(&args[1]).clone();
            let arr = trim_json(&arr_v);
            if !arr.starts_with(b"[") {
                if soft_null(ctx) {
                    out.emit_bytes(b"null")?;
                    return Ok(());
                }
                return Err(Error::TypeMismatch {
                    expected: "array",
                    found: type_name_json(arr),
                });
            }
            let elems = collect_array_elems(arr, 0, arr.len())?;
            // Preserve first-seen key order
            let mut order: Vec<Vec<u8>> = Vec::new();
            let mut groups: std::collections::HashMap<Vec<u8>, Vec<(usize, usize)>> =
                std::collections::HashMap::new();
            for &(s, e) in &elems {
                let key = eval_buf(arr, s, e, &key_expr, ctx).unwrap_or_else(|_| b"null".to_vec());
                let key = trim_json(&key).to_vec();
                if !groups.contains_key(&key) {
                    order.push(key.clone());
                    groups.insert(key.clone(), Vec::new());
                }
                groups.get_mut(&key).unwrap().push((s, e));
            }
            out.emit_byte(b'[')?;
            for (gi, k) in order.iter().enumerate() {
                if gi > 0 {
                    out.emit_byte(b',')?;
                }
                out.emit_byte(b'[')?;
                let g = groups.get(k).unwrap();
                for (i, &(s, e)) in g.iter().enumerate() {
                    if i > 0 {
                        out.emit_byte(b',')?;
                    }
                    out.emit_bytes(&arr[s..e])?;
                }
                out.emit_byte(b']')?;
            }
            out.emit_byte(b']')?;
            Ok(())
        }
        // max_by(array, &expr) / min_by(array, &expr) → single element or null
        "max_by" | "min_by" => {
            if args.len() < 2 {
                return Err(Error::Jmespath {
                    msg: "max_by/min_by requires (array, &expression)",
                });
            }
            let want_max = name == "max_by";
            let arr_v = eval_buf(json, start, end, &args[0], ctx)?;
            let key_expr = unwrap_expref(&args[1]).clone();
            let arr = trim_json(&arr_v);
            if !arr.starts_with(b"[") {
                if soft_null(ctx) {
                    out.emit_bytes(b"null")?;
                    return Ok(());
                }
                return Err(Error::TypeMismatch {
                    expected: "array",
                    found: type_name_json(arr),
                });
            }
            let elems = collect_array_elems(arr, 0, arr.len())?;
            if elems.is_empty() {
                out.emit_bytes(b"null")?;
                return Ok(());
            }
            let mut best_i = 0usize;
            let mut best_key: Option<Vec<u8>> = None;
            for (i, &(s, e)) in elems.iter().enumerate() {
                let key = eval_buf(arr, s, e, &key_expr, ctx).unwrap_or_else(|_| b"null".to_vec());
                let key = trim_json(&key).to_vec();
                if key.as_slice() == b"null" {
                    continue;
                }
                match &best_key {
                    None => {
                        best_key = Some(key);
                        best_i = i;
                    }
                    Some(bk) => {
                        let ord = cmp_sort_keys_strict(&key, bk)?;
                        let better = if want_max {
                            ord == std::cmp::Ordering::Greater
                        } else {
                            ord == std::cmp::Ordering::Less
                        };
                        if better {
                            best_key = Some(key);
                            best_i = i;
                        }
                    }
                }
            }
            if best_key.is_none() {
                out.emit_bytes(b"null")?;
                return Ok(());
            }
            let (s, e) = elems[best_i];
            out.emit_bytes(&arr[s..e])?;
            Ok(())
        }
        _ => Err(Error::Jmespath {
            msg: "Unknown JMESPath function",
        }),
    }
}

fn cmp_sort_keys(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    cmp_sort_keys_strict(a, b).unwrap_or(std::cmp::Ordering::Equal)
}

/// Comparable JMESPath keys: both numbers or both strings; else error.
fn cmp_sort_keys_strict(a: &[u8], b: &[u8]) -> Result<std::cmp::Ordering, Error> {
    if let (Ok(na), Ok(nb)) = (parse_f64(a), parse_f64(b)) {
        return Ok(na
            .partial_cmp(&nb)
            .unwrap_or(std::cmp::Ordering::Equal));
    }
    if let (Some(sa), Some(sb)) = (json_string_content(a), json_string_content(b)) {
        return Ok(sa.cmp(sb));
    }
    // nulls sort equal only to null
    if a == b"null" && b == b"null" {
        return Ok(std::cmp::Ordering::Equal);
    }
    Err(Error::Jmespath {
        msg: "incomparable types in sort_by/max_by/min_by key",
    })
}

fn write_json_string_out(out: &mut impl EmitOut, s: &str) -> Result<(), Error> {
    out.emit_byte(b'"')?;
    for b in s.bytes() {
        match b {
            b'"' | b'\\' => {
                out.emit_byte(b'\\')?;
                out.emit_byte(b)?;
            }
            c if c < 0x20 => {
                out.emit_bytes(format!("\\u{c:04x}").as_bytes())?;
            }
            c => out.emit_byte(c)?,
        }
    }
    out.emit_byte(b'"')?;
    Ok(())
}

fn require_arity(name: &str, args: &[SelectExpr], n: usize) -> Result<(), Error> {
    if args.len() != n {
        return Err(Error::Jmespath {
            msg: "wrong number of arguments to function",
        });
    }
    let _ = name;
    Ok(())
}

fn length_of(v: &[u8]) -> Result<usize, Error> {
    let t = trim_json(v);
    if t.starts_with(b"\"") {
        let raw = json_string_content(t).ok_or(Error::Jmespath {
            msg: "length: invalid string",
        })?;
        let unesc = unescape_string_content(raw).unwrap_or_else(|| raw.to_vec());
        let s = std::str::from_utf8(&unesc).map_err(|_| Error::Jmespath {
            msg: "length: invalid utf-8 string",
        })?;
        // JMESPath counts Unicode code points (chars), not UTF-8 bytes.
        return Ok(s.chars().count());
    }
    if t.starts_with(b"[") {
        return Ok(collect_array_elems(t, 0, t.len())?.len());
    }
    if t.starts_with(b"{") {
        return Ok(collect_object_members(t, 0, t.len())?.1.len());
    }
    Err(Error::Jmespath {
        msg: "length requires string, array, or object",
    })
}

/// Best-effort unescape of JSON string content (handles common escapes).
fn unescape_string_content(content: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(content.len());
    let mut i = 0;
    while i < content.len() {
        if content[i] != b'\\' {
            out.push(content[i]);
            i += 1;
            continue;
        }
        i += 1;
        if i >= content.len() {
            return None;
        }
        match content[i] {
            b'"' | b'\\' | b'/' => out.push(content[i]),
            b'n' => out.push(b'\n'),
            b't' => out.push(b'\t'),
            b'r' => out.push(b'\r'),
            b'b' => out.push(0x08),
            b'f' => out.push(0x0c),
            b'u' if i + 4 < content.len() => {
                let hex = std::str::from_utf8(&content[i + 1..i + 5]).ok()?;
                let cp = u32::from_str_radix(hex, 16).ok()?;
                let ch = char::from_u32(cp)?;
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
                i += 4;
            }
            _ => out.push(content[i]),
        }
        i += 1;
    }
    Some(out)
}

fn keys_of(v: &[u8], out: &mut impl EmitOut) -> Result<(), Error> {
    let t = trim_json(v);
    if !t.starts_with(b"{") {
        return Err(Error::Jmespath {
            msg: "keys requires an object",
        });
    }
    let (_, members, _) = collect_object_members(t, 0, t.len())?;
    out.emit_byte(b'[')?;
    for (i, m) in members.iter().enumerate() {
        if i > 0 {
            out.emit_byte(b',')?;
        }
        out.emit_bytes(&t[m.key_span.0..m.key_span.1])?;
    }
    out.emit_byte(b']')?;
    Ok(())
}

fn values_of(v: &[u8], out: &mut impl EmitOut) -> Result<(), Error> {
    let t = trim_json(v);
    if !t.starts_with(b"{") {
        return Err(Error::Jmespath {
            msg: "values requires an object",
        });
    }
    let (_, members, _) = collect_object_members(t, 0, t.len())?;
    out.emit_byte(b'[')?;
    for (i, m) in members.iter().enumerate() {
        if i > 0 {
            out.emit_byte(b',')?;
        }
        out.emit_bytes(&t[m.val_start..m.val_end])?;
    }
    out.emit_byte(b']')?;
    Ok(())
}

fn type_name_json(v: &[u8]) -> &'static str {
    let t = trim_json(v);
    match t.first() {
        Some(b'{') => "object",
        Some(b'[') => "array",
        Some(b'"') => "string",
        Some(b't') | Some(b'f') => "boolean",
        Some(b'n') => "null",
        Some(b'-') | Some(b'0'..=b'9') => "number",
        _ => "null",
    }
}

fn array_contains(arr: &[u8], item: &[u8]) -> Result<bool, Error> {
    let elems = collect_array_elems(arr, 0, arr.len())?;
    Ok(elems
        .iter()
        .any(|&(s, e)| trim_json(&arr[s..e]) == item))
}

fn reverse_array_or_string(v: &[u8], out: &mut impl EmitOut) -> Result<(), Error> {
    let t = trim_json(v);
    if t.starts_with(b"\"") {
        let s = json_string_content(t).unwrap_or(b"");
        let rev: Vec<u8> = s.iter().rev().copied().collect();
        write_json_string_out(out, std::str::from_utf8(&rev).unwrap_or(""))?;
        return Ok(());
    }
    let elems = collect_array_elems(t, 0, t.len())?;
    out.emit_byte(b'[')?;
    for (i, &(s, e)) in elems.iter().rev().enumerate() {
        if i > 0 {
            out.emit_byte(b',')?;
        }
        out.emit_bytes(&t[s..e])?;
    }
    out.emit_byte(b']')?;
    Ok(())
}


fn sort_array(v: &[u8], out: &mut impl EmitOut) -> Result<(), Error> {
    let t = trim_json(v);
    if !t.starts_with(b"[") {
        return Err(Error::Jmespath {
            msg: "sort requires an array",
        });
    }
    let elems = collect_array_elems(t, 0, t.len())?;
    let mut pieces: Vec<&[u8]> = elems.iter().map(|&(s, e)| &t[s..e]).collect();
    // Strict: all numbers or all strings.
    let mut kind: Option<bool> = None; // true=num, false=str
    for p in &pieces {
        let ta = trim_json(p);
        let is_num = parse_f64(ta).is_ok();
        let is_str = json_string_content(ta).is_some();
        if is_num == is_str {
            return Err(Error::Jmespath {
                msg: "sort: invalid element type",
            });
        }
        match kind {
            None => kind = Some(is_num),
            Some(k) if k != is_num => {
                return Err(Error::Jmespath {
                    msg: "sort: mixed number/string array",
                });
            }
            _ => {}
        }
    }
    pieces.sort_by(|a, b| {
        let ta = trim_json(a);
        let tb = trim_json(b);
        if let (Ok(na), Ok(nb)) = (parse_f64(ta), parse_f64(tb)) {
            na.partial_cmp(&nb)
                .unwrap_or(std::cmp::Ordering::Equal)
        } else if let (Some(sa), Some(sb)) = (json_string_content(ta), json_string_content(tb)) {
            sa.cmp(sb)
        } else {
            ta.cmp(tb)
        }
    });
    out.emit_byte(b'[')?;
    for (i, p) in pieces.iter().enumerate() {
        if i > 0 {
            out.emit_byte(b',')?;
        }
        out.emit_bytes(p)?;
    }
    out.emit_byte(b']')?;
    Ok(())
}

fn join_array(arr: &[u8], sep: &[u8], out: &mut impl EmitOut) -> Result<(), Error> {
    if !arr.starts_with(b"[") {
        return Err(Error::Jmespath {
            msg: "join requires an array",
        });
    }
    let elems = collect_array_elems(arr, 0, arr.len())?;
    let mut s = Vec::new();
    for (i, &(a, b)) in elems.iter().enumerate() {
        let el = trim_json(&arr[a..b]);
        let c = json_string_content(el).ok_or(Error::Jmespath {
            msg: "join array elements must be strings",
        })?;
        if i > 0 {
            // sep is wire content; unescape for output body
            let sep_u = unescape_string_content(sep).unwrap_or_else(|| sep.to_vec());
            s.extend_from_slice(&sep_u);
        }
        let cu = unescape_string_content(c).unwrap_or_else(|| c.to_vec());
        s.extend_from_slice(&cu);
    }
    write_json_string_out(out, std::str::from_utf8(&s).unwrap_or(""))?;
    Ok(())
}

fn array_reduce(arr: &[u8], which: &str, out: &mut impl EmitOut) -> Result<(), Error> {
    if !arr.starts_with(b"[") {
        return Err(Error::Jmespath {
            msg: "max/min/sum/avg require an array",
        });
    }
    let elems = collect_array_elems(arr, 0, arr.len())?;
    if elems.is_empty() {
        // JMESPath: max/min of empty → null; sum of empty → 0; avg of empty → null
        match which {
            "sum" => {
                out.emit_bytes(b"0")?;
                return Ok(());
            }
            _ => {
                out.emit_bytes(b"null")?;
                return Ok(());
            }
        }
    }

    // Classify: all numbers or all strings (for max/min); sum/avg numbers only.
    let mut nums: Vec<f64> = Vec::new();
    let mut strs: Vec<Vec<u8>> = Vec::new();
    let mut saw_num = false;
    let mut saw_str = false;
    for &(s, e) in &elems {
        let t = trim_json(&arr[s..e]);
        if let Ok(n) = parse_f64(t) {
            saw_num = true;
            nums.push(n);
        } else if let Some(c) = json_string_content(t) {
            saw_str = true;
            strs.push(unescape_string_content(c).unwrap_or_else(|| c.to_vec()));
        } else {
            return Err(Error::Jmespath {
                msg: "max/min/sum/avg: invalid array element type",
            });
        }
    }
    if saw_num && saw_str {
        return Err(Error::Jmespath {
            msg: "max/min/sum/avg: mixed number/string array",
        });
    }

    match which {
        "sum" | "avg" => {
            if saw_str {
                return Err(Error::Jmespath {
                    msg: "sum/avg require number array",
                });
            }
            let sum: f64 = nums.iter().sum();
            let r = if which == "avg" {
                sum / nums.len() as f64
            } else {
                sum
            };
            out.emit_bytes(format_number(r).as_bytes())?;
        }
        "max" | "min" => {
            if saw_num {
                let r = if which == "max" {
                    nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                } else {
                    nums.iter().cloned().fold(f64::INFINITY, f64::min)
                };
                out.emit_bytes(format_number(r).as_bytes())?;
            } else {
                let best = if which == "max" {
                    strs.iter().max().cloned()
                } else {
                    strs.iter().min().cloned()
                };
                match best {
                    Some(s) => write_json_string_out(out, std::str::from_utf8(&s).unwrap_or(""))?,
                    None => out.emit_bytes(b"null")?,
                }
            }
        }
        _ => out.emit_bytes(b"null")?,
    }
    Ok(())
}

fn format_number(n: f64) -> String {
    if n.fract() == 0.0 && n.abs() < 1e15 {
        format!("{}", n as i64)
    } else {
        format!("{n}")
    }
}

fn emit_multi_hash(
    json: &[u8],
    start: usize,
    end: usize,
    fields: &[HashField],
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    out.emit_byte(b'{')?;
    let mut wrote = false;
    for f in fields {
        let mut val = Vec::new();
        match emit_value(json, start, end, &f.expr, ctx, &mut val) {
            Ok(()) => {}
            Err(Error::PathNotFound) if ctx.plan.missing == MissingPolicy::Skip => continue,
            Err(e) => return Err(e),
        }
        if val.is_empty() && ctx.plan.missing == MissingPolicy::Skip {
            // treat empty as skip only if PathNotFound was converted — keep empty literals
        }
        if wrote {
            out.emit_byte(b',')?;
        }
        maybe_pretty_newline_indent(ctx, out, true)?;
        // key
        write_json_key(out, &f.output_key)?;
        match ctx.plan.style {
            ProjectStyle::Pretty { .. } => out.emit_bytes(b": ")?,
            _ => out.emit_byte(b':')?,
        }
        ctx.depth += 1;
        out.emit_bytes(&val)?;
        ctx.depth -= 1;
        wrote = true;
    }
    emit_close_object(ctx, out)?;
    let _ = end;
    Ok(())
}

fn emit_multi_list(
    json: &[u8],
    start: usize,
    end: usize,
    items: &[SelectExpr],
    ctx: &mut EmitCtx<'_>,
    out: &mut impl EmitOut,
) -> Result<(), Error> {
    out.emit_byte(b'[')?;
    let mut wrote = false;
    for expr in items {
        let mut val = Vec::new();
        match emit_value(json, start, end, expr, ctx, &mut val) {
            Ok(()) => {}
            Err(Error::PathNotFound) if ctx.plan.missing == MissingPolicy::Skip => continue,
            Err(e) => return Err(e),
        }
        if wrote {
            out.emit_byte(b',')?;
        }
        maybe_pretty_newline_indent(ctx, out, true)?;
        ctx.depth += 1;
        out.emit_bytes(&val)?;
        ctx.depth -= 1;
        wrote = true;
    }
    emit_close_array(ctx, out)?;
    let _ = end;
    Ok(())
}

fn flatten_emit(mid: &[u8], ctx: &mut EmitCtx<'_>, out: &mut impl EmitOut) -> Result<(), Error> {
    let start = skip_whitespace(mid, 0);
    let end = skip_value(mid, start)?;
    let start = skip_whitespace(mid, start);
    if start >= mid.len() || mid[start] != b'[' {
        // non-array: identity
        out.emit_bytes(&mid[start..end])?;
        return Ok(());
    }
    let outer = collect_array_elems(mid, start, end)?;
    out.emit_byte(b'[')?;
    let mut first = true;
    for (s, e) in outer {
        let s = skip_whitespace(mid, s);
        if s < mid.len() && mid[s] == b'[' {
            let inner = collect_array_elems(mid, s, e)?;
            for (is, ie) in inner {
                if !first {
                    out.emit_byte(b',')?;
                }
                maybe_pretty_newline_indent(ctx, out, true)?;
                out.emit_bytes(&mid[is..ie])?;
                first = false;
            }
        } else {
            if !first {
                out.emit_byte(b',')?;
            }
            maybe_pretty_newline_indent(ctx, out, true)?;
            out.emit_bytes(&mid[s..e])?;
            first = false;
        }
    }
    emit_close_array(ctx, out)?;
    Ok(())
}

fn write_json_key(out: &mut impl EmitOut, key: &str) -> Result<(), Error> {
    write_json_string_out(out, key)
}


fn emit_object_key(json: &[u8], m: &Member<'_>, ctx: &EmitCtx<'_>, out: &mut impl EmitOut) -> Result<(), Error> {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { .. }) {
        out.emit_byte(b'\n')?;
        write_indent(ctx.depth + 1, pretty_indent(ctx), out)?;
    }
    out.emit_bytes(&json[m.key_span.0..m.key_span.1])?;
    Ok(())
}

fn emit_colon(json: &[u8], m: &Member<'_>, ctx: &EmitCtx<'_>, out: &mut impl EmitOut) -> Result<(), Error> {
    match ctx.plan.style {
        ProjectStyle::Compact => out.emit_byte(b':')?,
        ProjectStyle::PreserveSource => {
            out.emit_bytes(&json[m.key_span.1..m.colon])?;
            out.emit_byte(b':')?;
            out.emit_bytes(&json[m.colon + 1..m.val_start])?;
        }
        ProjectStyle::Pretty { .. } => out.emit_bytes(b": ")?,
    }
    Ok(())
}

fn emit_close_object(ctx: &EmitCtx<'_>, out: &mut impl EmitOut) -> Result<(), Error> {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { indent: n } if n > 0)
        && out.last_byte() != Some(b'{')
    {
        out.emit_byte(b'\n')?;
        write_indent(ctx.depth, pretty_indent(ctx), out)?;
    }
    out.emit_byte(b'}')?;
    Ok(())
}

fn emit_close_array(ctx: &EmitCtx<'_>, out: &mut impl EmitOut) -> Result<(), Error> {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { indent: n } if n > 0)
        && out.last_byte() != Some(b'[')
    {
        out.emit_byte(b'\n')?;
        write_indent(ctx.depth, pretty_indent(ctx), out)?;
    }
    out.emit_byte(b']')?;
    Ok(())
}

fn maybe_pretty_newline_indent(ctx: &EmitCtx<'_>, out: &mut impl EmitOut, for_element: bool) -> Result<(), Error> {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { .. }) && for_element {
        out.emit_byte(b'\n')?;
        write_indent(ctx.depth + 1, pretty_indent(ctx), out)?;
    }
    Ok(())
}

fn pretty_indent(ctx: &EmitCtx<'_>) -> usize {
    match ctx.plan.style {
        ProjectStyle::Pretty { indent } => indent as usize,
        _ => 0,
    }
}

fn write_indent(depth: usize, per_level: usize, out: &mut impl EmitOut) -> Result<(), Error> {
    let n = depth.saturating_mul(per_level);
    let spaces = [b' '; 64];
    let mut left = n;
    while left > 0 {
        let chunk = left.min(spaces.len());
        out.emit_bytes(&spaces[..chunk])?;
        left -= chunk;
    }
    Ok(())
}

fn is_ws_only(s: &[u8]) -> bool {
    s.iter()
        .all(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
}

fn type_name_at(json: &[u8], pos: usize) -> &'static str {
    match json.get(pos).copied() {
        Some(b'{') => "object",
        Some(b'[') => "array",
        Some(b'"') => "string",
        Some(b't') | Some(b'f') => "bool",
        Some(b'n') => "null",
        Some(b'-') | Some(b'0'..=b'9') => "number",
        _ => "unknown",
    }
}

