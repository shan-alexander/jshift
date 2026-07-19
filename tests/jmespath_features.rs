//! Exhaustive feature tests for jshift's JMESPath surface.
//!
//! Covers: fields, wildcards, slices, filters, multi-select, pipe/flatten,
//! comparisons/logic, literals, functions (incl. map / sort_by / group_by),
//! and object multi-select wildcards (`*`).

use jshift::{parse_jmespath_expr, project_jmespath, project_paths, ProjectPlan};

fn jp(json: &[u8], expr: &str) -> Vec<u8> {
    project_jmespath(json, expr).unwrap_or_else(|e| {
        panic!("project_jmespath({expr:?}) failed: {e:?}");
    })
}

fn jp_str(json: &str, expr: &str) -> String {
    String::from_utf8(jp(json.as_bytes(), expr)).unwrap()
}

// ─── Core navigation ─────────────────────────────────────────────────────────

#[test]
fn field_and_subexpr() {
    let j = br#"{"a":{"b":1},"c":2}"#;
    assert_eq!(jp(j, "c"), b"2");
    assert_eq!(jp(j, "a.b"), b"1");
    assert_eq!(jp(j, "@"), j);
}

#[test]
fn array_index_and_negative() {
    let j = br#"{"a":[10,20,30,40]}"#;
    assert_eq!(jp(j, "a[0]"), b"10");
    assert_eq!(jp(j, "a[-1]"), b"40");
    assert_eq!(jp(j, "a[-2]"), b"30");
}

#[test]
fn array_wildcard_and_slice() {
    let j = br#"{"a":[10,20,30,40]}"#;
    assert_eq!(jp(j, "a[*]"), br#"[10,20,30,40]"#);
    assert_eq!(jp(j, "a[]"), br#"[10,20,30,40]"#);
    assert_eq!(jp(j, "a[1:3]"), br#"[20,30]"#);
    assert_eq!(jp(j, "a[::2]"), br#"[10,30]"#);
    assert_eq!(jp(j, "a[::-1]"), br#"[40,30,20,10]"#);
}

#[test]
fn object_wildcard_projection() {
    let j = br#"{"o":{"x":1,"y":2,"z":3}}"#;
    // All values of object (document order)
    assert_eq!(jp(j, "o.*"), br#"[1,2,3]"#);
    let j2 = br#"{"o":{"a":{"n":1},"b":{"n":2}}}"#;
    assert_eq!(jp(j2, "o.*.n"), br#"[1,2]"#);
    // Leading *
    let j3 = br#"{"a":{"v":1},"b":{"v":2}}"#;
    assert_eq!(jp(j3, "*.v"), br#"[1,2]"#);
}

#[test]
fn multi_select_list_and_hash() {
    let j = br#"{"id":7,"title":"Hat","extra":true}"#;
    assert_eq!(jp(j, "[id, title]"), br#"[7,"Hat"]"#);
    assert_eq!(
        jp(j, "{id: id, name: title}"),
        br#"{"id":7,"name":"Hat"}"#
    );
}

#[test]
fn multi_select_hash_with_nested_and_literals() {
    let j = br#"{"user":{"name":"a"},"n":1}"#;
    assert_eq!(
        jp(j, "{u: user.name, n: n, src: 'api', flag: `true`}"),
        br#"{"u":"a","n":1,"src":"api","flag":true}"#
    );
}

#[test]
fn pipe_and_flatten() {
    let j = br#"{"x":[[1,2],[3],[4,5]]}"#;
    assert_eq!(jp(j, "x | []"), br#"[1,2,3,4,5]"#);
    let j2 = br#"{"a":{"b":{"c":9}}}"#;
    assert_eq!(jp(j2, "a | b | c"), b"9");
}

#[test]
fn filter_projection() {
    let j = br#"{"items":[{"n":1,"ok":true},{"n":2,"ok":false},{"n":3,"ok":true}]}"#;
    assert_eq!(jp(j, "items[?ok == `true`].n"), br#"[1,3]"#);
    assert_eq!(jp(j, "items[?n > `1`].n"), br#"[2,3]"#);
    assert_eq!(jp(j, "items[?n == `2`].ok"), br#"[false]"#);
}

#[test]
fn comparisons_and_logic() {
    let j = br#"{"x":1,"y":0,"s":"ab"}"#;
    assert_eq!(jp(j, "x == `1`"), b"true");
    assert_eq!(jp(j, "x != `1`"), b"false");
    assert_eq!(jp(j, "x > `0`"), b"true");
    assert_eq!(jp(j, "x >= `1`"), b"true");
    assert_eq!(jp(j, "x < `2`"), b"true");
    assert_eq!(jp(j, "x <= `0`"), b"false");
    assert_eq!(jp(j, "(x == `1`) && (y == `0`)"), b"true");
    assert_eq!(jp(j, "x == `2` || y == `0`"), b"true");
    assert_eq!(jp(j, "!(x == `2`)"), b"true");
    assert_eq!(jp(j, "s == 'ab'"), b"true");
}

#[test]
fn literals_styles() {
    let j = br#"{"a":1}"#;
    assert_eq!(jp(j, "{a: a, b: \"hi\"}"), br#"{"a":1,"b":"hi"}"#);
    assert_eq!(jp(j, "{a: a, b: 'hi'}"), br#"{"a":1,"b":"hi"}"#);
    assert_eq!(jp(j, "{a: a, b: `null`}"), br#"{"a":1,"b":null}"#);
    assert_eq!(jp(j, "{a: a, b: `true`}"), br#"{"a":1,"b":true}"#);
    assert_eq!(jp(j, "{a: a, b: `2.5`}"), br#"{"a":1,"b":2.5}"#);
}

// ─── Functions ───────────────────────────────────────────────────────────────

#[test]
fn fn_length_keys_values_type() {
    let j = br#"{"a":[1,2,3],"o":{"x":1,"y":2},"t":"hi"}"#;
    assert_eq!(jp(j, "length(a)"), b"3");
    assert_eq!(jp(j, "length(t)"), b"2");
    assert_eq!(jp(j, "length(o)"), b"2");
    assert_eq!(jp(j, "keys(o)"), br#"["x","y"]"#);
    assert_eq!(jp(j, "values(o)"), br#"[1,2]"#);
    assert_eq!(jp(j, "type(a)"), br#""array""#);
    assert_eq!(jp(j, "type(o)"), br#""object""#);
    assert_eq!(jp(j, "type(t)"), br#""string""#);
}

#[test]
fn fn_string_and_number_coercion() {
    let j = br#"{"t":"Hello","n":"42"}"#;
    assert_eq!(jp(j, "starts_with(t, 'He')"), b"true");
    assert_eq!(jp(j, "ends_with(t, 'lo')"), b"true");
    assert_eq!(jp(j, "contains(t, 'ell')"), b"true");
    assert_eq!(jp(j, "to_number(n)"), b"42");
    assert_eq!(jp(j, "to_string(`9`)"), br#""9""#);
}

#[test]
fn fn_array_numeric_and_misc() {
    let j = br#"{"a":[3,1,2], "b":[1,[2],null]}"#;
    assert_eq!(jp(j, "sort(a)"), br#"[1,2,3]"#);
    assert_eq!(jp(j, "reverse(a)"), br#"[2,1,3]"#);
    assert_eq!(jp(j, "max(a)"), b"3");
    assert_eq!(jp(j, "min(a)"), b"1");
    assert_eq!(jp(j, "sum(a)"), b"6");
    assert_eq!(jp(j, "avg(a)"), b"2");
    assert_eq!(jp(j, "abs(`-3`)"), b"3");
    assert_eq!(jp(j, "ceil(`1.2`)"), b"2");
    assert_eq!(jp(j, "floor(`1.8`)"), b"1");
    assert_eq!(jp(j, "to_array(`1`)"), br#"[1]"#);
    assert_eq!(jp(j, "not_null(b[2], b[0])"), b"1");
    assert_eq!(jp(j, "join(',', a)"), br#""3,1,2""#);
    assert_eq!(jp(j, "contains(a, `1`)"), b"true");
}

#[test]
fn fn_merge() {
    let j = br#"{"a":{"x":1},"b":{"y":2}}"#;
    assert_eq!(jp(j, "merge(a, b)"), br#"{"x":1,"y":2}"#);
}

// ─── Higher-order: map / sort_by / group_by ──────────────────────────────────

#[test]
fn fn_map() {
    let j = br#"{"people":[{"name":"a","age":30},{"name":"b","age":20}]}"#;
    assert_eq!(
        jp(j, "map(&name, people)"),
        br#"["a","b"]"#
    );
    assert_eq!(
        jp(j, "map(&age, people)"),
        br#"[30,20]"#
    );
    assert_eq!(
        jp(j, "map(&{n: name, a: age}, people)"),
        br#"[{"n":"a","a":30},{"n":"b","a":20}]"#
    );
}

#[test]
fn fn_sort_by() {
    let j = br#"{"people":[{"name":"c","age":30},{"name":"a","age":20},{"name":"b","age":25}]}"#;
    assert_eq!(
        jp(j, "sort_by(people, &age)"),
        br#"[{"name":"a","age":20},{"name":"b","age":25},{"name":"c","age":30}]"#
    );
    assert_eq!(
        jp(j, "sort_by(people, &name)"),
        br#"[{"name":"a","age":20},{"name":"b","age":25},{"name":"c","age":30}]"#
    );
}

#[test]
fn fn_group_by() {
    let j = br#"{"items":[
        {"k":"red","v":1},
        {"k":"blue","v":2},
        {"k":"red","v":3},
        {"k":"blue","v":4}
    ]}"#;
    let out = jp_str(
        std::str::from_utf8(j).unwrap(),
        "group_by(items, &k)",
    );
    // Two groups, first-seen key order: red then blue
    assert_eq!(
        out,
        r#"[[{"k":"red","v":1},{"k":"red","v":3}],[{"k":"blue","v":2},{"k":"blue","v":4}]]"#
    );
}

#[test]
fn higher_order_pipeline() {
    let j = br#"{"people":[{"name":"c","age":30},{"name":"a","age":20},{"name":"b","age":25}]}"#;
    // sort then map names
    assert_eq!(
        jp(j, "map(&name, sort_by(people, &age))"),
        br#"["a","b","c"]"#
    );
}

// ─── Array projection + multi-select ─────────────────────────────────────────

#[test]
fn array_projection_multi_select() {
    let j = br#"{"products":[{"id":1,"t":"a","blob":9},{"id":2,"t":"b","blob":8}]}"#;
    assert_eq!(
        jp(j, "products[*].{id: id, title: t}"),
        br#"[{"id":1,"title":"a"},{"id":2,"title":"b"}]"#
    );
    assert_eq!(jp(j, "products[*].id"), br#"[1,2]"#);
}

#[test]
fn keep_list_vs_jmespath_shape() {
    let j = br#"{"products":[{"id":1,"t":"a"},{"id":2,"t":"b"}]}"#;
    // keep-list preserves wrapper
    assert_eq!(
        project_paths(j, &["products[].id"]).unwrap(),
        br#"{"products":[{"id":1},{"id":2}]}"#
    );
    // jmespath yields bare array
    assert_eq!(jp(j, "products[*].id"), br#"[1,2]"#);
}

// ─── Parse API ───────────────────────────────────────────────────────────────

#[test]
fn parse_and_plan_roundtrip() {
    let expr = parse_jmespath_expr("map(&foo, bar)").unwrap();
    let plan = ProjectPlan::from_select(expr);
    let json = br#"{"bar":[{"foo":1},{"foo":2}]}"#;
    assert_eq!(
        jshift::project(json, &plan).unwrap(),
        br#"[1,2]"#
    );
}

#[test]
fn expref_is_parsed() {
    assert!(parse_jmespath_expr("&a").is_ok());
    assert!(parse_jmespath_expr("sort_by(@, &foo)").is_ok());
    assert!(parse_jmespath_expr("group_by(items, &k)").is_ok());
}
