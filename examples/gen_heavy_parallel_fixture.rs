//! Generate a **large, CPU-heavy** JSON fixture for parallel list-projection benches.
//!
//! Domain-agnostic shape (not a product catalog):
//!
//! ```json
//! { "records": [ { "id", "label", "scores": [f64…], "events": [{t,w}…], "attrs": {…} }, … ] }
//! ```
//!
//! Per-element JMESPath work intentionally walks nested `scores` with filters so
//! the job is **CPU-bound**, not pure DRAM scan of thin cards.
//!
//! ```bash
//! mkdir -p benches/data
//! cargo run --example gen_heavy_parallel_fixture --release -- \
//!   benches/data/heavy_parallel.json
//!
//! # optional knobs (env):
//! #   HEAVY_RECORDS=80000 HEAVY_SCORES=400 HEAVY_EVENTS=40
//! ```
//!
//! Output is gitignored under `benches/data/` (never crates.io).

use std::env;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::Instant;

fn env_usize(key: &str, default: usize) -> usize {
    env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

fn main() -> std::io::Result<()> {
    let out_path = env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("benches/data/heavy_parallel.json"));

    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }

    // Defaults aim for ~400–700 MiB and multi-second sequential project times.
    let n_records = env_usize("HEAVY_RECORDS", 60_000);
    let n_scores = env_usize("HEAVY_SCORES", 350);
    let n_events = env_usize("HEAVY_EVENTS", 35);
    let n_attrs = env_usize("HEAVY_ATTRS", 12);

    eprintln!(
        "generating {} records × {} scores × {} events → {}",
        n_records,
        n_scores,
        n_events,
        out_path.display()
    );

    let t0 = Instant::now();
    let file = File::create(&out_path)?;
    let mut w = BufWriter::with_capacity(8 << 20, file);

    w.write_all(br#"{"records":["#)?;
    for r in 0..n_records {
        if r > 0 {
            w.write_all(b",")?;
        }
        // One record object.
        write!(w, r#"{{"id":{r},"label":"rec-{r}","scores":["#)?;
        for s in 0..n_scores {
            if s > 0 {
                w.write_all(b",")?;
            }
            // Deterministic pseudo-float in [0,1) — filter-friendly.
            let v = ((r.wrapping_mul(1103515245).wrapping_add(s.wrapping_mul(12345))) % 10_000)
                as f64
                / 10_000.0;
            // Compact fixed format without allocation-heavy {:?}
            let whole = (v * 1000.0) as u32;
            write!(w, "0.{whole:03}")?;
        }
        w.write_all(br#"],"events":["#)?;
        for e in 0..n_events {
            if e > 0 {
                w.write_all(b",")?;
            }
            let wgt = ((r + e * 7) % 100) as f64 / 100.0;
            let whole = (wgt * 100.0) as u32;
            write!(w, r#"{{"t":{e},"w":0.{whole:02}}}"#)?;
        }
        w.write_all(br#"],"attrs":{"#)?;
        for a in 0..n_attrs {
            if a > 0 {
                w.write_all(b",")?;
            }
            write!(w, r#""k{a}":"v{r}-{a}""#)?;
        }
        w.write_all(b"}}")?;

        if r % 5_000 == 0 && r > 0 {
            w.flush()?;
            eprintln!(
                "  … {r}/{n_records} ({:.1} MiB written so far)",
                out_path.metadata().map(|m| m.len()).unwrap_or(0) as f64 / 1024.0 / 1024.0
            );
        }
    }
    w.write_all(b"]}")?;
    w.flush()?;

    let len = std::fs::metadata(&out_path)?.len();
    eprintln!(
        "done in {:?} → {} ({:.2} MiB)",
        t0.elapsed(),
        out_path.display(),
        len as f64 / 1024.0 / 1024.0
    );
    eprintln!(
        "suggested project expr:\n  records[*].{{id: id, hi: length(scores[?@ > `0.7`]), lo: length(scores[?@ > `0.3`]), n_ev: length(events), w0: events[0].w, a0: attrs.k0}}"
    );
    Ok(())
}
