//! Fuzz in-place mutation APIs on a copy of the input buffer.
//!
//! Goal: no panics from mutate / upsert / delete / append on adversarial JSON.
//! Structural validity of the result is not required for malformed inputs.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }

    let (meta, rest) = data.split_first().unwrap();
    let json = if rest.len() > 16 * 1024 {
        rest[..16 * 1024].to_vec()
    } else {
        rest.to_vec()
    };

    // Always also try a few well-formed seeds mixed with the fuzzer buffer so
    // the mutators actually exercise happy paths often.
    let seeds: &[&[u8]] = &[
        br#"{"a":1,"b":"x","c":[1,2,3]}"#,
        br#"{"a\"b":1,"c\\d":[true,false]}"#,
        br#"[0,{"k":"v"},[]]"#,
        br#"{"outer":{"inner":1}}"#,
        br#"{}"#,
        br#"[]"#,
    ];

    let mut buffers: Vec<Vec<u8>> = Vec::with_capacity(1 + seeds.len());
    buffers.push(json);
    for s in seeds {
        buffers.push(s.to_vec());
    }

    let op = meta % 7;
    for mut buf in buffers {
        match op {
            0 => {
                let path = jshift::parse_path("a");
                let _ = jshift::mutate_value(&mut buf, &path, b"99");
            }
            1 => {
                let path = jshift::parse_path("c");
                let _ = jshift::append_to_array(&mut buf, &path, b"0");
            }
            2 => {
                let path = jshift::parse_path("");
                let _ = jshift::upsert_object_key(&mut buf, &path, "k", b"true");
            }
            3 => {
                let path = jshift::parse_path("");
                let _ = jshift::upsert_object_key(&mut buf, &path, r#"a"b"#, b"2");
            }
            4 => {
                let path = jshift::parse_path("");
                let _ = jshift::delete_key(&mut buf, &path, "a");
                let _ = jshift::delete_key(&mut buf, &path, r#"a"b"#);
            }
            5 => {
                let path = jshift::parse_path("c");
                let _ = jshift::delete_index(&mut buf, &path, 0);
                let _ = jshift::delete_index(&mut buf, &[], 0);
            }
            _ => {
                let path = jshift::parse_path("outer");
                let _ = jshift::upsert_object_key(&mut buf, &path, "x", b"null");
                let _ = jshift::delete_key(&mut buf, &path, "inner");
            }
        }
        // Keep the buffer "used".
        let _ = buf.len();
    }
});
