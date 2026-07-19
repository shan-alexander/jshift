//! Fuzz path scans over arbitrary byte buffers treated as JSON.
//!
//! Goal: no panics when `find_value` / `array_len` walk malformed or adversarial
//! documents (unbalanced braces, escaped quotes, truncated values).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }

    // Split input: first byte selects a canned path; rest is the buffer.
    let (meta, json) = data.split_first().unwrap();
    let json = if json.len() > 64 * 1024 {
        &json[..64 * 1024]
    } else {
        json
    };

    let path_str = match meta % 8 {
        0 => "",
        1 => "a",
        2 => "a.b",
        3 => "a[0]",
        4 => "a.b[1].c",
        5 => "data[0].id",
        6 => r#"a\"b"#,
        _ => "list",
    };
    let path = jshift::parse_path(path_str);

    let _ = jshift::find_value(json, &path);
    let _ = jshift::array_len(json, &path);
});
