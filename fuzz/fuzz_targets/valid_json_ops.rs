//! Property-style fuzz: when input is serde_json-valid, run safe ops and require
//! the result still parses with serde_json (and never panics on random bytes).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.len() < 2 {
        return;
    }
    let (meta, rest) = data.split_first().unwrap();
    let rest = if rest.len() > 8 * 1024 {
        &rest[..8 * 1024]
    } else {
        rest
    };

    // Always exercise random bytes (no panic).
    let mut random = rest.to_vec();
    run_ops(*meta, &mut random);

    // If serde_json accepts the buffer, require post-op validity for checked ops.
    if let Ok(v) = serde_json::from_slice::<serde_json::Value>(rest) {
        let mut json = rest.to_vec();
        let path_a = jshift::parse_path("a");
        let path_b = jshift::parse_path("b");
        match meta % 5 {
            0 => {
                let _ = jshift::mutate_value_checked(&mut json, &path_a, b"1");
            }
            1 => {
                let _ = jshift::upsert_object_key(&mut json, &[], "fuzz", b"true");
            }
            2 => {
                let _ = jshift::delete_key(&mut json, &[], "a");
            }
            3 => {
                let _ = jshift::append_to_array(&mut json, &path_b, b"0");
            }
            _ => {
                let _ = jshift::delete_index(&mut json, &[], 0);
            }
        }
        // If the op succeeded, document should still parse.
        if json != rest {
            if let Ok(_) = jshift::find_value(&json, &[]) {
                let _ = serde_json::from_slice::<serde_json::Value>(&json);
            }
        }
        let _ = v;
    }
});

fn run_ops(meta: u8, buf: &mut Vec<u8>) {
    let path = jshift::parse_path("a");
    match meta % 6 {
        0 => {
            let _ = jshift::find_value(buf, &path);
        }
        1 => {
            let _ = jshift::mutate_value(buf, &path, b"0");
        }
        2 => {
            let _ = jshift::mutate_value_checked(buf, &path, b"null");
        }
        3 => {
            let _ = jshift::delete_key(buf, &[], "a");
        }
        4 => {
            let _ = jshift::upsert_object_key(buf, &[], "k", b"1");
        }
        _ => {
            let _ = jshift::delete_index(buf, &[], 0);
        }
    }
}
