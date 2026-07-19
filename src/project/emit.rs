//! Projection emitter: walk input spans, write output per [`SelectExpr`].

use crate::error::Error;
use crate::project::plan::{MissingPolicy, ProjectPlan, ProjectStyle};
use crate::project::select::{ArraySelect, HashField, ObjectSelect, SelectExpr};
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
            // Evaluate left as a descent that produces a sub-value, then right.
            // For Object/Array left, emit left into mid then apply right... but left
            // might be a subset object wrapping the target. Prefer: if left is a
            // single-key object keep or index array, resolve child span and apply right.
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
            let (s, e) = elems.get(*idx).copied().ok_or(Error::PathNotFound)?;
            if matches!(child, SelectExpr::Identity | SelectExpr::Current) {
                Ok(Some((s, e)))
            } else {
                resolve_focus(json, s, e, child)
            }
        }
        SelectExpr::Array(ArraySelect::Each(_)) | SelectExpr::Array(ArraySelect::Slice { .. }) => {
            Ok(None)
        }
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
    let mut kept: Vec<(usize, usize, &SelectExpr)> = Vec::new();
    match sel {
        ArraySelect::Each(child) => {
            for &(s, e) in &elems {
                kept.push((s, e, child));
            }
        }
        ArraySelect::Indices(map) => {
            let mut idxs: Vec<usize> = map.keys().copied().collect();
            idxs.sort_unstable();
            for i in idxs {
                match elems.get(i) {
                    Some(&(s, e)) => kept.push((s, e, map.get(&i).unwrap())),
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
            each,
        } => {
            let hi = en.unwrap_or(elems.len()).min(elems.len());
            let lo = (*st).min(hi);
            for &(s, e) in &elems[lo..hi] {
                kept.push((s, e, each));
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

