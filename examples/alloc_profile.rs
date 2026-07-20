//! Allocator / heap profiling for thin-card project shapes.
//!
//! ## dhat (precise allocation counts — in-process)
//!
//! ```bash
//! cargo run --release --example alloc_profile --features dhat-heap -- project
//! # writes dhat-heap.json in CWD; open with https://nnethercote.github.io/dh_view/
//! ```
//!
//! ## heaptrack (external, full call-tree — Linux)
//!
//! ```bash
//! cargo build --release --example alloc_profile
//! heaptrack ./target/release/examples/alloc_profile project
//! heaptrack_gui heaptrack.alloc_profile.*.gz   # or heaptrack_print
//! ```
//!
//! Modes: `project` | `project_write` | `project_jsonl` | `project_each` | `hold`
//! Optional second arg: path to JSON (default: synthetic ~2 MiB catalog if no large.json).

#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

use std::env;
use std::io;
use std::path::PathBuf;

use jshift::{
    project, project_each, project_jsonl_write, project_write, ProjectPlan,
};

fn synthetic_catalog(n: usize) -> Vec<u8> {
    let mut s = String::from(r#"{"products":["#);
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(
            r#"{{"id":{i},"title":"Product {i}","handle":"h-{i}","noise":"xxxxxxxxxxxxxxxx","variants":[{{"price":"9.99"}},{{"price":"10.99"}}]}}"#
        ));
    }
    s.push_str("]}");
    s.into_bytes()
}

fn load_json() -> Vec<u8> {
    if let Some(p) = env::args().nth(2).map(PathBuf::from) {
        return std::fs::read(p).expect("read arg path");
    }
    if let Some(p) = env::var_os("JSHIFT_LARGE_JSON").map(PathBuf::from) {
        if p.is_file() {
            return std::fs::read(p).expect("read JSHIFT_LARGE_JSON");
        }
    }
    let large = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/data/large.json");
    if large.is_file() {
        eprintln!("using {}", large.display());
        return std::fs::read(large).expect("read large.json");
    }
    let n: usize = env::var("ALLOC_PROFILE_N")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5_000);
    eprintln!("no large.json; synthetic products n={n}");
    synthetic_catalog(n)
}

fn main() {
    #[cfg(feature = "dhat-heap")]
    let _profiler = dhat::Profiler::new_heap();

    let mode = env::args().nth(1).unwrap_or_else(|| "project".into());
    let json = load_json();
    eprintln!(
        "mode={mode} input_bytes={} input_mib={:.2}",
        json.len(),
        json.len() as f64 / 1024.0 / 1024.0
    );

    let plan = ProjectPlan::from_jmespath("products[*].{id: id, title: title}").unwrap();

    match mode.as_str() {
        "hold" => {
            let _ = json.first();
            eprintln!("hold only");
        }
        "project" => {
            let out = project(&json, &plan).unwrap();
            eprintln!("out_bytes={}", out.len());
        }
        "project_write" => {
            let n = project_write(&json, &plan, io::sink()).unwrap();
            eprintln!("out_bytes={n}");
        }
        "project_jsonl" => {
            let n = project_jsonl_write(&json, &plan, io::sink()).unwrap();
            eprintln!("out_bytes={n}");
        }
        "project_each" => {
            let mut n = 0usize;
            let mut bytes = 0usize;
            project_each(&json, &plan, |_, card| {
                n += 1;
                bytes += card.len();
                Ok(())
            })
            .unwrap();
            eprintln!("cards={n} total_card_bytes={bytes}");
        }
        other => {
            eprintln!("unknown mode {other}");
            std::process::exit(2);
        }
    }

    #[cfg(feature = "dhat-heap")]
    {
        // Profiler drops at end of main → dhat-heap.json
        eprintln!("dhat: dropping profiler → dhat-heap.json (open with dh_view)");
    }
}
