#![forbid(unsafe_code)]

//! `jshift` — Schema-guided, safe in-place JSON path reader and mutator.
//!
//! This crate provides a 100% safe Rust engine to selectively read, mutate, upsert,
//! and delete values inside raw JSON byte buffers (`&[u8]` and `Vec<u8>`) without
//! building a full AST. Path scans return zero-copy slices; mutations resize the
//! buffer and shift the tail with safe slice rotations.
//!
//! # Features
//! * **Zero-copy reads:** Find values as slices into the raw buffer.
//! * **In-place mutations:** Safe byte-shifting (including resize) via slice rotations.
//! * **Macro-generated schemas:** `#[derive(JsonMutatorSchema)]` for typed readers and mutators.
//! * **Array and object CRUD:** Insert, update, append, and delete dynamically.
//! * **JSON string escaping:** `ToJsonBytes` and key upserts escape special characters.
//!
//! # Quick Start
//! ```
//! use jshift::{find_value, mutate_value, parse_path};
//!
//! let mut json = b"{\"user\": \"farmer\", \"score\": 9.5}".to_vec();
//! let path = parse_path("score");
//!
//! // Read value
//! let score_bytes = find_value(&json, &path).unwrap();
//! assert_eq!(score_bytes, b"9.5");
//!
//! // Mutate in-place
//! mutate_value(&mut json, &path, b"10.0").unwrap();
//! assert_eq!(json, b"{\"user\": \"farmer\", \"score\": 10.0}".to_vec());
//! ```
//!
//! # High-Impact Real-World Use Case: LLM Dataset Processing (JSONL)
//! In AI training pipelines (e.g., LoRA finetuning), datasets are stored as JSONL files.
//! You can inspect token lengths and mark records as skipped or cleaned in-place:
//!
//! ```
//! use jshift::JsonMutatorSchema;
//!
//! #[derive(JsonMutatorSchema)]
//! struct TrainingRecord {
//!     #[json(path = "tokens")]
//!     tokens: usize,
//!     #[json(path = "status")]
//!     status: String,
//! }
//!
//! let mut line = b"{\"instruction\": \"Translate...\", \"tokens\": 1024, \"status\": \"pending\"}".to_vec();
//!
//! // Parse selectively
//! let record = TrainingRecord::read_from_json(&line).unwrap();
//!
//! // Skip long contexts in-place!
//! if record.tokens > 512 {
//!     let mut mutator = TrainingRecord::mutator(&mut line);
//!     mutator.set_status("skipped").unwrap();
//! }
//!
//! assert_eq!(
//!     line,
//!     b"{\"instruction\": \"Translate...\", \"tokens\": 1024, \"status\": \"skipped\"}".to_vec()
//! );
//! ```

mod convert;
mod error;
mod mutate;
mod path;
mod scan;

pub use convert::{
    escape_json_key, escape_json_string, write_json_string, write_json_string_content,
    FromJsonSlice, ToJsonBytes,
};
pub use error::Error;
pub use jshift_derive::JsonMutatorSchema;
pub use mutate::{
    append_to_array, array_len, delete_index, delete_key, mutate_value, upsert_object_key,
};
pub use path::{parse_path, PathSegment};
pub use scan::find_value;

#[cfg(test)]
mod tests {
    extern crate self as jshift;
    use super::*;

    #[test]
    fn test_find_simple_values() {
        let json = b"{\"a\": 123, \"b\": \"hello\", \"c\": true}";

        assert_eq!(find_value(json, &parse_path("a")), Ok(&b"123"[..]));
        assert_eq!(find_value(json, &parse_path("b")), Ok(&b"\"hello\""[..]));
        assert_eq!(find_value(json, &parse_path("c")), Ok(&b"true"[..]));
        assert_eq!(find_value(json, &parse_path("d")), Err(Error::PathNotFound));
    }

    #[test]
    fn test_find_nested_values() {
        let json = b"{\"metadata\": {\"version\": 1, \"author\": \"farmer\"}, \"data\": [1,2,3]}";

        assert_eq!(
            find_value(json, &parse_path("metadata.version")),
            Ok(&b"1"[..])
        );
        assert_eq!(
            find_value(json, &parse_path("metadata.author")),
            Ok(&b"\"farmer\""[..])
        );
        assert_eq!(
            find_value(json, &parse_path("data")),
            Ok(&b"[1,2,3]"[..])
        );
    }

    #[test]
    fn test_mutate_equal_size() {
        let mut json = b"{\"a\": 123, \"b\": \"hello\"}".to_vec();
        mutate_value(&mut json, &parse_path("a"), b"999").unwrap();
        assert_eq!(json, b"{\"a\": 999, \"b\": \"hello\"}");
    }

    #[test]
    fn test_mutate_smaller_size() {
        let mut json = b"{\"a\": 12345, \"b\": \"hello\"}".to_vec();
        mutate_value(&mut json, &parse_path("a"), b"9").unwrap();
        assert_eq!(json, b"{\"a\": 9, \"b\": \"hello\"}");
    }

    #[test]
    fn test_mutate_larger_size() {
        let mut json = b"{\"a\": 1, \"b\": \"hello\"}".to_vec();
        mutate_value(&mut json, &parse_path("a"), b"99999").unwrap();
        assert_eq!(json, b"{\"a\": 99999, \"b\": \"hello\"}");
    }

    #[test]
    fn test_mutate_nested() {
        let mut json = b"{\"meta\": {\"ver\": 1}, \"data\": true}".to_vec();
        mutate_value(&mut json, &parse_path("meta.ver"), b"100").unwrap();
        assert_eq!(json, b"{\"meta\": {\"ver\": 100}, \"data\": true}");
    }

    #[test]
    fn test_array_indexing() {
        let json = b"{\"data\": [{\"id\": 1}, {\"id\": 2}], \"tags\": [\"a\", \"b\"]}";

        assert_eq!(find_value(json, &parse_path("data[0].id")), Ok(&b"1"[..]));
        assert_eq!(find_value(json, &parse_path("data[1].id")), Ok(&b"2"[..]));
        assert_eq!(find_value(json, &parse_path("tags[1]")), Ok(&b"\"b\""[..]));
        assert_eq!(
            find_value(json, &parse_path("tags[2]")),
            Err(Error::IndexOutOfBounds { index: 2 })
        );
    }

    #[test]
    fn test_array_append_raw() {
        let mut json = b"{\"list\": []}".to_vec();
        append_to_array(&mut json, &parse_path("list"), b"1").unwrap();
        assert_eq!(json, b"{\"list\": [1]}");

        append_to_array(&mut json, &parse_path("list"), b"2").unwrap();
        assert_eq!(json, b"{\"list\": [1,2]}");
    }

    #[test]
    fn test_array_len() {
        let json = b"{\"empty\": [], \"list\": [1, 2, 3]}";
        assert_eq!(array_len(json, &parse_path("empty")), Ok(0));
        assert_eq!(array_len(json, &parse_path("list")), Ok(3));
    }

    #[test]
    fn test_upsert_object_key() {
        let mut json = b"{\"a\": 1}".to_vec();
        // Insert new key
        upsert_object_key(&mut json, &[], "b", b"2").unwrap();
        assert_eq!(json, b"{\"a\": 1,\"b\":2}");

        // Update existing key
        upsert_object_key(&mut json, &[], "a", b"99").unwrap();
        assert_eq!(json, b"{\"a\": 99,\"b\":2}");
    }

    #[test]
    fn test_delete_key() {
        let mut json = b"{\"a\": 1, \"b\": 2, \"c\": 3}".to_vec();
        delete_key(&mut json, &[], "b").unwrap();
        assert_eq!(json, b"{\"a\": 1, \"c\": 3}");

        delete_key(&mut json, &[], "a").unwrap();
        assert_eq!(json, b"{ \"c\": 3}");

        delete_key(&mut json, &[], "c").unwrap();
        assert_eq!(json, b"{ }");
    }

    #[test]
    fn test_delete_index() {
        let mut json = b"[10, 20, 30]".to_vec();
        delete_index(&mut json, &[], 1).unwrap();
        assert_eq!(json, b"[10, 30]");

        delete_index(&mut json, &[], 0).unwrap();
        assert_eq!(json, b"[ 30]");

        delete_index(&mut json, &[], 0).unwrap();
        assert_eq!(json, b"[ ]");
    }

    #[derive(JsonMutatorSchema)]
    struct Config {
        #[json(path = "metadata.version")]
        version: u32,
        #[json(path = "user.score")]
        score: f64,
        #[json(path = "user.name")]
        name: String,
        #[json(path = "user.tags")]
        tags: Vec<String>,
    }

    #[test]
    fn test_procedural_macro() {
        let mut json = b"{\"metadata\": {\"version\": 1}, \"user\": {\"score\": 9.5, \"name\": \"farmer\", \"tags\": [\"rust\", \"fast\"]}}".to_vec();

        let config = Config::read_from_json(&json).unwrap();
        assert_eq!(config.version, 1);
        assert_eq!(config.score, 9.5);
        assert_eq!(config.name, "farmer");
        assert_eq!(
            config.tags,
            vec!["rust".to_string(), "fast".to_string()]
        );

        let mut mutator = Config::mutator(&mut json);
        mutator.set_version(&2).unwrap();
        mutator.set_score(&99.9).unwrap();
        mutator.set_name("new_name").unwrap();
        mutator.append_tags("cool").unwrap();

        let updated = Config::read_from_json(&json).unwrap();
        assert_eq!(updated.version, 2);
        assert_eq!(updated.score, 99.9);
        assert_eq!(updated.name, "new_name");
        assert_eq!(
            updated.tags,
            vec![
                "rust".to_string(),
                "fast".to_string(),
                "cool".to_string()
            ]
        );
    }

    #[test]
    fn test_escape_json_string() {
        assert_eq!(escape_json_string("plain"), br#""plain""#);
        assert_eq!(escape_json_string(r#"say "hi""#), br#""say \"hi\"""#);
        assert_eq!(escape_json_string("a\\b"), br#""a\\b""#);
        assert_eq!(escape_json_string("a\nb\tc"), br#""a\nb\tc""#);
        assert_eq!(escape_json_string("\u{0001}"), br#""\u0001""#);
    }

    #[test]
    fn test_to_json_bytes_escapes_strings() {
        assert_eq!(r#"he"llo"#.to_json_bytes(), br#""he\"llo""#);
        assert_eq!(String::from("x\ny").to_json_bytes(), br#""x\ny""#);
    }

    #[test]
    fn test_upsert_escapes_keys() {
        let mut json = b"{}".to_vec();
        upsert_object_key(&mut json, &[], r#"a"b"#, b"1").unwrap();
        assert_eq!(json, br#"{"a\"b":1}"#);

        // Path matching compares raw key bytes inside quotes (escaped form).
        assert_eq!(find_value(&json, &parse_path(r#"a\"b"#)), Ok(&b"1"[..]));
    }

    #[test]
    fn test_upsert_updates_escaped_key_without_duplicate() {
        let mut json = b"{}".to_vec();
        upsert_object_key(&mut json, &[], r#"a"b"#, b"1").unwrap();
        upsert_object_key(&mut json, &[], r#"a"b"#, b"2").unwrap();
        assert_eq!(json, br#"{"a\"b":2}"#);
        // Must not have duplicated the key (escaped form is 4 bytes: a \ " b).
        let needle = br#"a\"b"#;
        assert_eq!(
            json.windows(needle.len()).filter(|w| *w == needle).count(),
            1
        );
    }

    #[test]
    fn test_delete_key_with_escapes() {
        let mut json = br#"{"plain":1,"a\"b":2,"c\\d":3}"#.to_vec();
        delete_key(&mut json, &[], r#"a"b"#).unwrap();
        assert_eq!(json, br#"{"plain":1,"c\\d":3}"#);

        delete_key(&mut json, &[], r#"c\d"#).unwrap();
        assert_eq!(json, br#"{"plain":1}"#);

        // First key with escapes-only remaining siblings already covered above.
        let mut json2 = br#"{"x\"y":10,"z":20}"#.to_vec();
        delete_key(&mut json2, &[], r#"x"y"#).unwrap();
        assert_eq!(json2, br#"{"z":20}"#);
    }

    #[test]
    fn test_parse_path_hardens_invalid_segments() {
        assert_eq!(
            parse_path("a.b[0].c"),
            vec![
                PathSegment::Key("a"),
                PathSegment::Key("b"),
                PathSegment::Index(0),
                PathSegment::Key("c"),
            ]
        );
        // Empty keys from dots are skipped.
        assert_eq!(
            parse_path("..a..b."),
            vec![PathSegment::Key("a"), PathSegment::Key("b")]
        );
        // Unclosed bracket stops parsing (does not emit a fake key of the rest).
        assert_eq!(parse_path("a[1"), vec![PathSegment::Key("a")]);
        // Non-numeric index is skipped.
        assert_eq!(
            parse_path("a[foo].b"),
            vec![PathSegment::Key("a"), PathSegment::Key("b")]
        );
        // Empty brackets skipped.
        assert_eq!(
            parse_path("a[].b"),
            vec![PathSegment::Key("a"), PathSegment::Key("b")]
        );
    }

    #[test]
    fn test_from_json_slice_unescapes_strings() {
        assert_eq!(
            String::from_json_slice(br#""say \"hi\"""#).as_deref(),
            Some(r#"say "hi""#)
        );
        assert_eq!(
            String::from_json_slice(br#""a\\b\n""#).as_deref(),
            Some("a\\b\n")
        );
        assert_eq!(
            String::from_json_slice(br#""\u0041""#).as_deref(),
            Some("A")
        );
    }

    #[test]
    fn test_escape_json_key_matches_on_wire_form() {
        assert_eq!(escape_json_key(r#"a"b"#), r#"a\"b"#);
        assert_eq!(escape_json_key("a\\b"), r#"a\\b"#);
        assert_eq!(escape_json_key("a\nb"), r#"a\nb"#);
    }

    #[test]
    fn test_mutate_via_to_json_bytes_keeps_valid_json() {
        let mut json = br#"{"msg":"old"}"#.to_vec();
        let bytes = r#"say "hi""#.to_json_bytes();
        mutate_value(&mut json, &parse_path("msg"), &bytes).unwrap();
        assert_eq!(json, br#"{"msg":"say \"hi\""}"#);
        assert_eq!(
            find_value(&json, &parse_path("msg")),
            Ok(&br#""say \"hi\"""#[..])
        );
    }

    #[test]
    fn test_nested_upsert_delete() {
        let mut json = br#"{"outer":{"inner":1}}"#.to_vec();
        upsert_object_key(&mut json, &parse_path("outer"), "x", b"true").unwrap();
        assert_eq!(
            find_value(&json, &parse_path("outer.x")),
            Ok(&b"true"[..])
        );
        delete_key(&mut json, &parse_path("outer"), "inner").unwrap();
        assert_eq!(json, br#"{"outer":{"x":true}}"#);
    }

    #[test]
    fn test_array_ops_nested() {
        let mut json = br#"{"a":{"b":[1,2]}}"#.to_vec();
        assert_eq!(array_len(&json, &parse_path("a.b")), Ok(2));
        append_to_array(&mut json, &parse_path("a.b"), b"3").unwrap();
        assert_eq!(array_len(&json, &parse_path("a.b")), Ok(3));
        delete_index(&mut json, &parse_path("a.b"), 0).unwrap();
        assert_eq!(find_value(&json, &parse_path("a.b[0]")), Ok(&b"2"[..]));
        assert_eq!(find_value(&json, &parse_path("a.b[1]")), Ok(&b"3"[..]));
    }

    #[test]
    fn test_strings_with_escaped_quotes_inside_values() {
        let json = br#"{"k":"before \"mid\" after","n":1}"#;
        assert_eq!(
            find_value(json, &parse_path("k")),
            Ok(&br#""before \"mid\" after""#[..])
        );
        assert_eq!(find_value(json, &parse_path("n")), Ok(&b"1"[..]));
    }

    #[test]
    fn test_type_mismatch_and_errors() {
        let json = br#"{"a":1,"b":[1]}"#;
        assert_eq!(
            array_len(json, &parse_path("a")),
            Err(Error::TypeMismatch {
                expected: "array",
                found: "primitive/object"
            })
        );
        let mut json = br#"{"a":1}"#.to_vec();
        assert_eq!(
            append_to_array(&mut json, &parse_path("a"), b"2"),
            Err(Error::TypeMismatch {
                expected: "array",
                found: "primitive/object"
            })
        );
        assert_eq!(
            delete_key(&mut json, &[], "missing"),
            Err(Error::PathNotFound)
        );
    }
}
