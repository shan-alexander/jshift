//! Fuzz `parse_path` with arbitrary UTF-8-ish input.
//!
//! Goal: no panics on adversarial path strings (unclosed brackets, empty
//! segments, huge inputs, mixed separators).

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(s) = std::str::from_utf8(data) else {
        return;
    };
    // Bound work so pathological multi-megabyte inputs stay practical.
    let s = if s.len() > 4096 { &s[..4096] } else { s };
    let path = jshift::parse_path(s);
    let _ = jshift::try_parse_path(s);
    // Touch results so LLVM keeps the work.
    let _ = path.len();
    for seg in &path {
        match seg {
            jshift::PathSegment::Key(k) => {
                let _ = k.len();
            }
            jshift::PathSegment::Index(i) => {
                let _ = i.wrapping_add(1);
            }
        }
    }
});
