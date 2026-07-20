//! Generate a multi-topic training-style JSONL fixture (gitignored under benches/data/).
//!
//! Shape matches real dumps: `{ topic, messages[{role,content}], record_id, source_file }`.
//!
//! ```bash
//! cargo run --release --example gen_jsonl_fixture -- benches/data/jsonl_20mb.jsonl
//! # env: JSONL_LINES=30000 JSONL_TARGET_MIB=20
//! ```

use std::env;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;

const TOPICS: &[&str] = &[
    "lakekeeper",
    "spider-crawling",
    "egui",
    "duckdb",
    "iceberg",
    "arrow",
    "parquet",
    "tokio",
    "axum",
    "serde",
    "jmespath",
    "jsonl-pipelines",
    "llm-finetune",
    "rag-chunking",
    "vector-search",
    "wasm",
    "nix",
    "kubernetes",
    "observability",
    "feature-flags",
];

fn main() {
    let out = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/data/jsonl_20mb.jsonl")
        });
    let n_lines: usize = env::var("JSONL_LINES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(30_000);
    let target_mib: f64 = env::var("JSONL_TARGET_MIB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20.0);
    let target_bytes = (target_mib * 1024.0 * 1024.0) as usize;

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).ok();
    }

    let f = File::create(&out).expect("create output");
    let mut w = BufWriter::new(f);
    let mut written = 0usize;
    let mut i = 0usize;

    // Size content so we land near target_mib given n_lines (with floor for readability).
    let approx_per_line = (target_bytes / n_lines.max(1)).max(400);
    let content_budget = approx_per_line.saturating_sub(180); // overhead for keys/ids
    let user_len = (content_budget / 4).clamp(80, 2_000);
    let asst_len = (content_budget - user_len).clamp(200, 8_000);

    while i < n_lines {
        let topic = TOPICS[i % TOPICS.len()];
        let rid = format!("{:016x}", (i as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15));
        let src = format!("topics/{topic}/lessons.jsonl");
        let user = make_content("Exercise", topic, i, user_len);
        let asst = make_content("Speaker notes", topic, i.wrapping_mul(7), asst_len);

        // Manual JSON to avoid serde dep in example path (use serde_json if available — it's dev-dep)
        let line = format!(
            r#"{{"topic":"{topic}","messages":[{{"role":"user","content":{user_json}}},{{"role":"assistant","content":{asst_json}}}],"record_id":"{rid}","source_file":"{src}"}}"#,
            user_json = escape_json_string(&user),
            asst_json = escape_json_string(&asst),
        );
        writeln!(w, "{line}").unwrap();
        written += line.len() + 1;
        i += 1;
        if i % 5000 == 0 {
            eprintln!("  wrote {i} lines ({:.2} MiB)…", written as f64 / 1024.0 / 1024.0);
        }
    }
    w.flush().unwrap();
    eprintln!(
        "done: {} lines → {} ({:.2} MiB, target ~{:.1} MiB)",
        i,
        out.display(),
        written as f64 / 1024.0 / 1024.0,
        target_mib
    );
}

fn make_content(prefix: &str, topic: &str, seed: usize, len: usize) -> String {
    let mut s = format!(
        "{prefix} ({topic} #{seed}): Write Rust that processes JSONL with path-selective scans. "
    );
    let pad = "The quick brown fox jumps over lazy pipeline stages; measure RSS and keep silver thin. ";
    while s.len() < len {
        s.push_str(pad);
    }
    s.truncate(len);
    s
}

fn escape_json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
