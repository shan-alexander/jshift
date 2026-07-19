//! Projection emitter: walk input spans, write output per [`SelectExpr`].

use crate::error::Error;
use crate::project::plan::{MissingPolicy, ProjectPlan, ProjectStyle};
use crate::project::select::{
    resolve_index, resolve_slice, ArraySelect, CmpOp, HashField, ObjectSelect, SelectExpr,
};
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
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    match expr {
        SelectExpr::Identity | SelectExpr::Current => {
            out.extend_from_slice(&json[start..end]);
            Ok(())
        }
        SelectExpr::Field(key) => {
            let (s, e) = find_object_value_span(json, start, end, key.as_bytes())?;
            out.extend_from_slice(&json[s..e]);
            Ok(())
        }
        SelectExpr::Literal(bytes) => {
            out.extend_from_slice(bytes);
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
        SelectExpr::Sub(left, right) => {
            if let Some((s, e)) = resolve_focus(json, start, end, left)? {
                emit_value(json, s, e, right, ctx, out)
            } else {
                let mut mid = Vec::new();
                emit_value(json, start, end, left, ctx, &mut mid)?;
                let m0 = skip_whitespace(&mid, 0);
                let m1 = skip_value(&mid, m0)?;
                emit_value(&mid, m0, m1, right, ctx, out)
            }
        }
        SelectExpr::Cmp { op, left, right } => {
            let lv = eval_buf(json, start, end, left, ctx)?;
            let rv = eval_buf(json, start, end, right, ctx)?;
            let t = cmp_values(&lv, &rv, *op);
            out.extend_from_slice(if t { b"true" } else { b"false" });
            Ok(())
        }
        SelectExpr::And(a, b) => {
            let av = eval_buf(json, start, end, a, ctx)?;
            if !is_truthy(&av) {
                out.extend_from_slice(b"false");
                return Ok(());
            }
            emit_value(json, start, end, b, ctx, out)
        }
        SelectExpr::Or(a, b) => {
            let av = eval_buf(json, start, end, a, ctx)?;
            if is_truthy(&av) {
                out.extend_from_slice(&av);
                return Ok(());
            }
            emit_value(json, start, end, b, ctx, out)
        }
        SelectExpr::Not(inner) => {
            let v = eval_buf(json, start, end, inner, ctx)?;
            out.extend_from_slice(if is_truthy(&v) { b"false" } else { b"true" });
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
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    let (_, members, _) = collect_object_members(json, start, end)?;
    out.push(b'[');
    for (i, m) in members.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        maybe_pretty_newline_indent(ctx, out, true);
        ctx.depth += 1;
        emit_value(json, m.val_start, m.val_end, each, ctx, out)?;
        ctx.depth -= 1;
    }
    emit_close_array(ctx, out);
    Ok(())
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
        Err(Error::PathNotFound) if ctx.plan.missing == MissingPolicy::Skip => Ok(b"null".to_vec()),
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

fn cmp_values(left: &[u8], right: &[u8], op: CmpOp) -> bool {
    let l = trim_json(left);
    let r = trim_json(right);
    // null equality
    if l == b"null" || r == b"null" {
        return match op {
            CmpOp::Eq => l == r,
            CmpOp::Ne => l != r,
            _ => false,
        };
    }
    // boolean
    if (l == b"true" || l == b"false") && (r == b"true" || r == b"false") {
        let lb = l == b"true";
        let rb = r == b"true";
        return match op {
            CmpOp::Eq => lb == rb,
            CmpOp::Ne => lb != rb,
            _ => false,
        };
    }
    // numbers
    if let (Ok(ln), Ok(rn)) = (parse_f64(l), parse_f64(r)) {
        return match op {
            CmpOp::Eq => ln == rn,
            CmpOp::Ne => ln != rn,
            CmpOp::Lt => ln < rn,
            CmpOp::Le => ln <= rn,
            CmpOp::Gt => ln > rn,
            CmpOp::Ge => ln >= rn,
        };
    }
    // strings (JSON quoted)
    if let (Some(ls), Some(rs)) = (json_string_content(l), json_string_content(r)) {
        return match op {
            CmpOp::Eq => ls == rs,
            CmpOp::Ne => ls != rs,
            CmpOp::Lt => ls < rs,
            CmpOp::Le => ls <= rs,
            CmpOp::Gt => ls > rs,
            CmpOp::Ge => ls >= rs,
        };
    }
    // raw byte equality fallback
    match op {
        CmpOp::Eq => l == r,
        CmpOp::Ne => l != r,
        _ => false,
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
        SelectExpr::Field(key) => {
            let (s, e) = find_object_value_span(json, start, end, key.as_bytes())?;
            Ok(Some((s, e)))
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
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    let (obj_start, members, close_pos) = collect_object_members(json, start, end)?;

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

    out.push(b'{');
    // PreserveSource: leading ws after `{` when first kept is first member.
    if matches!(ctx.plan.style, ProjectStyle::PreserveSource)
        && let Some((first_m, _)) = kept.first()
        && members
            .first()
            .is_some_and(|m| m.member_start == first_m.member_start)
    {
        let between = &json[obj_start + 1..first_m.member_start];
        if is_ws_only(between) {
            out.extend_from_slice(between);
        }
    }

    for (i, (m, child)) in kept.iter().enumerate() {
        if i > 0 {
            emit_member_sep(json, &kept, i, ctx, out);
        }
        emit_object_key(json, m, ctx, out);
        emit_colon(json, m, ctx, out);
        ctx.depth += 1;
        emit_value(json, m.val_start, m.val_end, child, ctx, out)?;
        ctx.depth -= 1;
        // PreserveSource: trailing space after value before comma (not last)
        if matches!(ctx.plan.style, ProjectStyle::PreserveSource) && i + 1 < kept.len() {
            // copy ws after value up to comma if original had comma after this member
            if let Some(c) = m.comma {
                let mid = &json[m.after_value..c];
                if is_ws_only(mid) {
                    out.extend_from_slice(mid);
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
                out.extend_from_slice(between);
            }
        } else {
            let between = &json[last_m.after_value..close_pos];
            if is_ws_only(between) {
                out.extend_from_slice(between);
            }
        }
    }

    emit_close_object(ctx, out);
    let _ = end;
    Ok(())
}

fn emit_member_sep(
    json: &[u8],
    kept: &[(&Member<'_>, &SelectExpr)],
    i: usize,
    ctx: &EmitCtx<'_>,
    out: &mut Vec<u8>,
) {
    match ctx.plan.style {
        ProjectStyle::PreserveSource => {
            // Prefer original comma between adjacent original neighbors.
            let prev = kept[i - 1].0;
            let curr = kept[i].0;
            if let Some(c) = prev.comma {
                // from comma through whitespace before curr key
                out.push(b',');
                let between = &json[c + 1..curr.member_start];
                if is_ws_only(between) {
                    out.extend_from_slice(between);
                }
            } else {
                out.push(b',');
            }
        }
        ProjectStyle::Compact => out.push(b','),
        ProjectStyle::Pretty { .. } => out.push(b','),
    }
}

fn emit_array(
    json: &[u8],
    start: usize,
    end: usize,
    sel: &ArraySelect,
    ctx: &mut EmitCtx<'_>,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    let elems = collect_array_elems(json, start, end)?;
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
                        out.extend_from_slice(b"null");
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

    out.push(b'[');
    for (i, (s, e, child)) in kept.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        maybe_pretty_newline_indent(ctx, out, true);
        ctx.depth += 1;
        emit_value(json, *s, *e, child, ctx, out)?;
        ctx.depth -= 1;
    }
    emit_close_array(ctx, out);
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
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    let name = name.to_ascii_lowercase();
    match name.as_str() {
        "length" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            let n = length_of(&v)?;
            out.extend_from_slice(n.to_string().as_bytes());
            Ok(())
        }
        "keys" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            keys_of(&v, out)
        }
        "values" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            values_of(&v, out)
        }
        "type" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            let t = type_name_json(&v);
            write_json_string_out(out, t);
            Ok(())
        }
        "to_string" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            let t = trim_json(&v);
            if t.starts_with(b"\"") {
                out.extend_from_slice(t);
            } else {
                write_json_string_out(out, std::str::from_utf8(t).unwrap_or(""));
            }
            Ok(())
        }
        "to_number" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            let t = trim_json(&v);
            if let Some(s) = json_string_content(t) {
                let s = std::str::from_utf8(s).unwrap_or("");
                if s.parse::<f64>().is_ok() {
                    out.extend_from_slice(s.as_bytes());
                    return Ok(());
                }
            } else if parse_f64(t).is_ok() {
                out.extend_from_slice(t);
                return Ok(());
            }
            out.extend_from_slice(b"null");
            Ok(())
        }
        "starts_with" | "ends_with" | "contains" => {
            if args.len() < 2 {
                return Err(Error::InvalidPath {
                    msg: "starts_with/ends_with/contains need 2 args",
                });
            }
            let a = eval_buf(json, start, end, &args[0], ctx)?;
            let b = eval_buf(json, start, end, &args[1], ctx)?;
            let as_ = json_string_content(trim_json(&a)).unwrap_or(trim_json(&a));
            let bs = json_string_content(trim_json(&b)).unwrap_or(trim_json(&b));
            let ok = match name.as_str() {
                "starts_with" => as_.starts_with(bs),
                "ends_with" => as_.ends_with(bs),
                _ => {
                    // contains: string substring or array membership (byte equality)
                    if trim_json(&a).starts_with(b"[") {
                        array_contains(trim_json(&a), trim_json(&b))?
                    } else {
                        as_.windows(bs.len()).any(|w| w == bs)
                    }
                }
            };
            out.extend_from_slice(if ok { b"true" } else { b"false" });
            Ok(())
        }
        "not_null" => {
            for a in args {
                let v = eval_buf(json, start, end, a, ctx)?;
                if trim_json(&v) != b"null" {
                    out.extend_from_slice(trim_json(&v));
                    return Ok(());
                }
            }
            out.extend_from_slice(b"null");
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
            if args.len() < 2 {
                return Err(Error::InvalidPath {
                    msg: "join needs separator and array",
                });
            }
            let sep_v = eval_buf(json, start, end, &args[0], ctx)?;
            let arr_v = eval_buf(json, start, end, &args[1], ctx)?;
            let sep = json_string_content(trim_json(&sep_v)).unwrap_or(b"");
            join_array(trim_json(&arr_v), sep, out)
        }
        "max" | "min" | "sum" | "avg" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            numeric_reduce(trim_json(&v), name.as_str(), out)
        }
        "abs" | "ceil" | "floor" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            let n = parse_f64(trim_json(&v)).map_err(|_| Error::TypeMismatch {
                expected: "number",
                found: "other",
            })?;
            let r = match name.as_str() {
                "abs" => n.abs(),
                "ceil" => n.ceil(),
                _ => n.floor(),
            };
            out.extend_from_slice(format_number(r).as_bytes());
            Ok(())
        }
        "to_array" => {
            let arg = args.first().cloned().unwrap_or(SelectExpr::Current);
            let v = eval_buf(json, start, end, &arg, ctx)?;
            let t = trim_json(&v);
            if t.starts_with(b"[") {
                out.extend_from_slice(t);
            } else {
                out.push(b'[');
                out.extend_from_slice(t);
                out.push(b']');
            }
            Ok(())
        }
        "merge" => {
            // shallow merge of objects
            out.push(b'{');
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
                        out.push(b',');
                    }
                    out.extend_from_slice(&t[m.key_span.0..m.key_span.1]);
                    out.push(b':');
                    out.extend_from_slice(&t[m.val_start..m.val_end]);
                    first = false;
                }
            }
            out.push(b'}');
            Ok(())
        }
        // map(&expr, array) — apply expr to each element
        "map" => {
            if args.len() < 2 {
                return Err(Error::InvalidPath {
                    msg: "map requires (&expression, array)",
                });
            }
            let mapper = unwrap_expref(&args[0]).clone();
            let arr_v = eval_buf(json, start, end, &args[1], ctx)?;
            let arr = trim_json(&arr_v);
            let elems = collect_array_elems(arr, 0, arr.len())?;
            out.push(b'[');
            for (i, &(s, e)) in elems.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                let mut piece = Vec::new();
                match emit_value(arr, s, e, &mapper, ctx, &mut piece) {
                    Ok(()) => out.extend_from_slice(&piece),
                    Err(Error::PathNotFound) => out.extend_from_slice(b"null"),
                    Err(err) => return Err(err),
                }
            }
            out.push(b']');
            Ok(())
        }
        // sort_by(array, &expr)
        "sort_by" => {
            if args.len() < 2 {
                return Err(Error::InvalidPath {
                    msg: "sort_by requires (array, &expression)",
                });
            }
            let arr_v = eval_buf(json, start, end, &args[0], ctx)?;
            let key_expr = unwrap_expref(&args[1]).clone();
            let arr = trim_json(&arr_v);
            let elems = collect_array_elems(arr, 0, arr.len())?;
            let mut keyed: Vec<(Vec<u8>, usize, usize)> = Vec::with_capacity(elems.len());
            for &(s, e) in &elems {
                let key = eval_buf(arr, s, e, &key_expr, ctx).unwrap_or_else(|_| b"null".to_vec());
                keyed.push((trim_json(&key).to_vec(), s, e));
            }
            keyed.sort_by(|a, b| cmp_sort_keys(&a.0, &b.0));
            out.push(b'[');
            for (i, (_, s, e)) in keyed.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                out.extend_from_slice(&arr[*s..*e]);
            }
            out.push(b']');
            Ok(())
        }
        // group_by(array, &expr) → array of groups (arrays of original elements)
        "group_by" => {
            if args.len() < 2 {
                return Err(Error::InvalidPath {
                    msg: "group_by requires (array, &expression)",
                });
            }
            let arr_v = eval_buf(json, start, end, &args[0], ctx)?;
            let key_expr = unwrap_expref(&args[1]).clone();
            let arr = trim_json(&arr_v);
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
            out.push(b'[');
            for (gi, k) in order.iter().enumerate() {
                if gi > 0 {
                    out.push(b',');
                }
                out.push(b'[');
                let g = groups.get(k).unwrap();
                for (i, &(s, e)) in g.iter().enumerate() {
                    if i > 0 {
                        out.push(b',');
                    }
                    out.extend_from_slice(&arr[s..e]);
                }
                out.push(b']');
            }
            out.push(b']');
            Ok(())
        }
        _ => Err(Error::InvalidPath {
            msg: "Unknown JMESPath function",
        }),
    }
}

fn cmp_sort_keys(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    if let (Ok(na), Ok(nb)) = (parse_f64(a), parse_f64(b)) {
        return na
            .partial_cmp(&nb)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(b));
    }
    if let (Some(sa), Some(sb)) = (json_string_content(a), json_string_content(b)) {
        return sa.cmp(sb);
    }
    a.cmp(b)
}

fn write_json_string_out(out: &mut Vec<u8>, s: &str) {
    out.push(b'"');
    for b in s.bytes() {
        match b {
            b'"' | b'\\' => {
                out.push(b'\\');
                out.push(b);
            }
            c if c < 0x20 => out.extend_from_slice(format!("\\u{c:04x}").as_bytes()),
            c => out.push(c),
        }
    }
    out.push(b'"');
}

fn length_of(v: &[u8]) -> Result<usize, Error> {
    let t = trim_json(v);
    if t.starts_with(b"\"") {
        return Ok(json_string_content(t).map(|s| s.len()).unwrap_or(0));
    }
    if t.starts_with(b"[") {
        return Ok(collect_array_elems(t, 0, t.len())?.len());
    }
    if t.starts_with(b"{") {
        return Ok(collect_object_members(t, 0, t.len())?.1.len());
    }
    Ok(0)
}

fn keys_of(v: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
    let t = trim_json(v);
    let (_, members, _) = collect_object_members(t, 0, t.len())?;
    out.push(b'[');
    for (i, m) in members.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(&t[m.key_span.0..m.key_span.1]);
    }
    out.push(b']');
    Ok(())
}

fn values_of(v: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
    let t = trim_json(v);
    let (_, members, _) = collect_object_members(t, 0, t.len())?;
    out.push(b'[');
    for (i, m) in members.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(&t[m.val_start..m.val_end]);
    }
    out.push(b']');
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

fn reverse_array_or_string(v: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
    let t = trim_json(v);
    if t.starts_with(b"\"") {
        let s = json_string_content(t).unwrap_or(b"");
        let rev: Vec<u8> = s.iter().rev().copied().collect();
        write_json_string_out(out, std::str::from_utf8(&rev).unwrap_or(""));
        return Ok(());
    }
    let elems = collect_array_elems(t, 0, t.len())?;
    out.push(b'[');
    for (i, &(s, e)) in elems.iter().rev().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(&t[s..e]);
    }
    out.push(b']');
    Ok(())
}


fn sort_array(v: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
    let t = trim_json(v);
    let elems = collect_array_elems(t, 0, t.len())?;
    let mut pieces: Vec<&[u8]> = elems.iter().map(|&(s, e)| &t[s..e]).collect();
    pieces.sort_by(|a, b| {
        let ta = trim_json(a);
        let tb = trim_json(b);
        if let (Ok(na), Ok(nb)) = (parse_f64(ta), parse_f64(tb)) {
            na.partial_cmp(&nb).unwrap_or(std::cmp::Ordering::Equal)
        } else {
            ta.cmp(tb)
        }
    });
    out.push(b'[');
    for (i, p) in pieces.iter().enumerate() {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(p);
    }
    out.push(b']');
    Ok(())
}

fn join_array(arr: &[u8], sep: &[u8], out: &mut Vec<u8>) -> Result<(), Error> {
    let elems = collect_array_elems(arr, 0, arr.len())?;
    let mut s = Vec::new();
    for (i, &(a, b)) in elems.iter().enumerate() {
        if i > 0 {
            s.extend_from_slice(sep);
        }
        let el = trim_json(&arr[a..b]);
        if let Some(c) = json_string_content(el) {
            s.extend_from_slice(c);
        } else {
            s.extend_from_slice(el);
        }
    }
    write_json_string_out(out, std::str::from_utf8(&s).unwrap_or(""));
    Ok(())
}

fn numeric_reduce(arr: &[u8], which: &str, out: &mut Vec<u8>) -> Result<(), Error> {
    let elems = collect_array_elems(arr, 0, arr.len())?;
    let mut nums = Vec::new();
    for &(s, e) in &elems {
        if let Ok(n) = parse_f64(trim_json(&arr[s..e])) {
            nums.push(n);
        }
    }
    if nums.is_empty() {
        out.extend_from_slice(b"null");
        return Ok(());
    }
    let r = match which {
        "max" => nums.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
        "min" => nums.iter().cloned().fold(f64::INFINITY, f64::min),
        "sum" => nums.iter().sum(),
        "avg" => nums.iter().sum::<f64>() / nums.len() as f64,
        _ => 0.0,
    };
    out.extend_from_slice(format_number(r).as_bytes());
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
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    out.push(b'{');
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
            out.push(b',');
        }
        maybe_pretty_newline_indent(ctx, out, true);
        // key
        write_json_key(out, &f.output_key);
        match ctx.plan.style {
            ProjectStyle::Pretty { .. } => out.extend_from_slice(b": "),
            _ => out.push(b':'),
        }
        ctx.depth += 1;
        out.extend_from_slice(&val);
        ctx.depth -= 1;
        wrote = true;
    }
    emit_close_object(ctx, out);
    let _ = end;
    Ok(())
}

fn emit_multi_list(
    json: &[u8],
    start: usize,
    end: usize,
    items: &[SelectExpr],
    ctx: &mut EmitCtx<'_>,
    out: &mut Vec<u8>,
) -> Result<(), Error> {
    out.push(b'[');
    let mut wrote = false;
    for expr in items {
        let mut val = Vec::new();
        match emit_value(json, start, end, expr, ctx, &mut val) {
            Ok(()) => {}
            Err(Error::PathNotFound) if ctx.plan.missing == MissingPolicy::Skip => continue,
            Err(e) => return Err(e),
        }
        if wrote {
            out.push(b',');
        }
        maybe_pretty_newline_indent(ctx, out, true);
        ctx.depth += 1;
        out.extend_from_slice(&val);
        ctx.depth -= 1;
        wrote = true;
    }
    emit_close_array(ctx, out);
    let _ = end;
    Ok(())
}

fn flatten_emit(mid: &[u8], ctx: &mut EmitCtx<'_>, out: &mut Vec<u8>) -> Result<(), Error> {
    let start = skip_whitespace(mid, 0);
    let end = skip_value(mid, start)?;
    let start = skip_whitespace(mid, start);
    if start >= mid.len() || mid[start] != b'[' {
        // non-array: identity
        out.extend_from_slice(&mid[start..end]);
        return Ok(());
    }
    let outer = collect_array_elems(mid, start, end)?;
    out.push(b'[');
    let mut first = true;
    for (s, e) in outer {
        let s = skip_whitespace(mid, s);
        if s < mid.len() && mid[s] == b'[' {
            let inner = collect_array_elems(mid, s, e)?;
            for (is, ie) in inner {
                if !first {
                    out.push(b',');
                }
                maybe_pretty_newline_indent(ctx, out, true);
                out.extend_from_slice(&mid[is..ie]);
                first = false;
            }
        } else {
            if !first {
                out.push(b',');
            }
            maybe_pretty_newline_indent(ctx, out, true);
            out.extend_from_slice(&mid[s..e]);
            first = false;
        }
    }
    emit_close_array(ctx, out);
    Ok(())
}

fn write_json_key(out: &mut Vec<u8>, key: &str) {
    out.push(b'"');
    for b in key.bytes() {
        match b {
            b'"' | b'\\' => {
                out.push(b'\\');
                out.push(b);
            }
            c if c < 0x20 => {
                out.extend_from_slice(format!("\\u{c:04x}").as_bytes());
            }
            c => out.push(c),
        }
    }
    out.push(b'"');
}

fn emit_object_key(json: &[u8], m: &Member<'_>, ctx: &EmitCtx<'_>, out: &mut Vec<u8>) {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { .. }) {
        out.push(b'\n');
        write_indent(ctx.depth + 1, pretty_indent(ctx), out);
    }
    out.extend_from_slice(&json[m.key_span.0..m.key_span.1]);
}

fn emit_colon(json: &[u8], m: &Member<'_>, ctx: &EmitCtx<'_>, out: &mut Vec<u8>) {
    match ctx.plan.style {
        ProjectStyle::Compact => out.push(b':'),
        ProjectStyle::PreserveSource => {
            out.extend_from_slice(&json[m.key_span.1..m.colon]);
            out.push(b':');
            out.extend_from_slice(&json[m.colon + 1..m.val_start]);
        }
        ProjectStyle::Pretty { .. } => out.extend_from_slice(b": "),
    }
}

fn emit_close_object(ctx: &EmitCtx<'_>, out: &mut Vec<u8>) {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { indent: n } if n > 0)
        && out.last() != Some(&b'{')
    {
        out.push(b'\n');
        write_indent(ctx.depth, pretty_indent(ctx), out);
    }
    out.push(b'}');
}

fn emit_close_array(ctx: &EmitCtx<'_>, out: &mut Vec<u8>) {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { indent: n } if n > 0)
        && out.last() != Some(&b'[')
    {
        out.push(b'\n');
        write_indent(ctx.depth, pretty_indent(ctx), out);
    }
    out.push(b']');
}

fn maybe_pretty_newline_indent(ctx: &EmitCtx<'_>, out: &mut Vec<u8>, for_element: bool) {
    if matches!(ctx.plan.style, ProjectStyle::Pretty { .. }) && for_element {
        out.push(b'\n');
        write_indent(ctx.depth + 1, pretty_indent(ctx), out);
    }
}

fn pretty_indent(ctx: &EmitCtx<'_>) -> usize {
    match ctx.plan.style {
        ProjectStyle::Pretty { indent } => indent as usize,
        _ => 0,
    }
}

fn write_indent(depth: usize, per_level: usize, out: &mut Vec<u8>) {
    let n = depth.saturating_mul(per_level);
    out.resize(out.len() + n, b' ');
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

