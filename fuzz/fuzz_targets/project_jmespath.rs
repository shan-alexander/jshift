//! Fuzz JMESPath parse + project: no panics on adversarial expr/JSON pairs.
//!
//! Input layout (when long enough):
//! * first 2 bytes → expression length N (mod remaining)
//! * next N bytes → expression (UTF-8 lossy)
//! * rest → JSON document bytes

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    let data = if data.len() > 16_384 {
        &data[..16_384]
    } else {
        data
    };

    let (expr, json) = if data.len() < 4 {
        ("@", data)
    } else {
        let n = (u16::from_le_bytes([data[0], data[1]]) as usize) % (data.len().saturating_sub(2)).max(1);
        let n = n.min(data.len().saturating_sub(2));
        let expr_bytes = &data[2..2 + n];
        let json = &data[2 + n..];
        let expr = std::str::from_utf8(expr_bytes).unwrap_or("@");
        // Cap expression length for practical fuzzing.
        let expr = if expr.len() > 512 { &expr[..512] } else { expr };
        (expr, json)
    };

    // Parse must not panic.
    let _ = jshift::parse_jmespath_expr(expr);
    let _ = jshift::parse_project_path(expr);

    // Project must not panic (errors are fine).
    let _ = jshift::project_jmespath(json, expr);
    if let Ok(plan) = jshift::ProjectPlan::from_paths(&[expr]) {
        let _ = jshift::project(json, &plan);
    }
    let _ = jshift::project_paths(json, &["a", "b.c", "items[0].x"]);
});
