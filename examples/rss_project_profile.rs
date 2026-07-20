//! Peak-RSS style profiling for large-file project shapes.
//!
//! Run under GNU time for max RSS:
//! ```bash
//! cargo build --release --example rss_project_profile
//! /usr/bin/time -v ./target/release/examples/rss_project_profile project
//! # or: ./scripts/measure_rss.sh
//! ```
//!
//! Modes (argv[1]):
//! * `hold_input` — load fixture, touch first/last byte, sleep briefly (baseline RSS)
//! * `project` — thin cards into a `Vec` (full projected array in RAM)
//! * `project_write` — same plan to `io::sink()` (no retained output Vec)
//! * `project_jsonl` — NDJSON cards to `io::sink()` (one card buffer)
//!
//! Set `JSHIFT_LARGE_JSON` or place `benches/data/large.json`.

use std::env;
use std::io;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use jshift::{
    project, project_jsonl_write, project_object_fields, project_write, ProjectPlan,
};

fn load_json() -> (PathBuf, Vec<u8>) {
    let path = env::var_os("JSHIFT_LARGE_JSON")
        .map(PathBuf::from)
        .or_else(|| {
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/data/large.json");
            p.is_file().then_some(p)
        })
        .unwrap_or_else(|| {
            eprintln!("missing large.json — run scripts/build_large_catalog.sh");
            std::process::exit(2);
        });
    let bytes = std::fs::read(&path).expect("read large.json");
    (path, bytes)
}

fn main() {
    let mode = env::args().nth(1).unwrap_or_else(|| "project".into());
    let (path, json) = load_json();
    let input_mib = json.len() as f64 / 1024.0 / 1024.0;
    // Touch ends so the OS actually faults pages in before we measure work.
    let _ = json.first().copied();
    let _ = json.last().copied();

    let plan = ProjectPlan::from_jmespath("products[*].{id: id, title: title}").unwrap();

    eprintln!(
        "mode={mode} path={} input_mib={input_mib:.2} json_len={}",
        path.display(),
        json.len()
    );

    let t0 = Instant::now();
    let out_bytes = match mode.as_str() {
        "hold_input" => {
            // Keep the buffer live; give time -v a moment to sample.
            thread::sleep(Duration::from_millis(200));
            0usize
        }
        "project" => {
            let out = project(&json, &plan).expect("project");
            out.len()
        }
        "project_write" => project_write(&json, &plan, io::sink()).expect("project_write"),
        "project_jsonl" => {
            project_jsonl_write(&json, &plan, io::sink()).expect("project_jsonl")
        }
        "project_object_fields" => {
            let out = project_object_fields(&json, "products", &["id", "title"]).expect("fields");
            out.len()
        }
        other => {
            eprintln!(
                "unknown mode {other:?}; use hold_input|project|project_write|project_jsonl|project_object_fields"
            );
            std::process::exit(2);
        }
    };
    let elapsed = t0.elapsed();
    // Keep `json` alive until process end so RSS includes input for all modes.
    std::mem::forget(json);
    eprintln!("out_bytes={out_bytes} elapsed={elapsed:?}");
}
