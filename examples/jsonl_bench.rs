//! JSONL performance on a real multi‑MB training-style dump.
//!
//! Fixture (gitignored): `benches/data/3k_lines_4mb.jsonl` (~3.1k lines, ~3.9 MiB).
//! Shape: `{ topic, messages[{role,content}], record_id, source_file }` with fat
//! `messages[].content` strings (lesson text / code).
//!
//! ```bash
//! cargo run --release --example jsonl_bench
//! ```

use std::path::PathBuf;
use std::time::{Duration, Instant};

use jshift::{find_value, json_lines, parse_path, project_jmespath, project_paths, upsert_object_key};

#[cfg(feature = "derive")]
use jshift::JsonMutatorSchema;

#[cfg(feature = "derive")]
#[derive(JsonMutatorSchema)]
struct LessonMeta {
    #[json(path = "topic")]
    topic: String,
    #[json(path = "record_id")]
    record_id: String,
}

fn med(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn time_it(iters: usize, warm: usize, mut f: impl FnMut()) -> Duration {
    for _ in 0..warm {
        f();
    }
    let mut times = Vec::with_capacity(iters);
    for _ in 0..iters {
        let t = Instant::now();
        f();
        times.push(t.elapsed());
    }
    med(times)
}

fn fmt(d: Duration) -> String {
    if d.as_secs_f64() >= 1.0 {
        format!("{:.3} s", d.as_secs_f64())
    } else if d.as_millis() >= 1 {
        format!("{:.2} ms", d.as_secs_f64() * 1000.0)
    } else {
        format!("{:.1} µs", d.as_secs_f64() * 1e6)
    }
}

fn ratio(fast: Duration, slow: Duration) -> String {
    format!("~{:.1}×", slow.as_secs_f64() / fast.as_secs_f64().max(1e-12))
}

fn load() -> Vec<u8> {
    let path = std::env::var_os("JSHIFT_JSONL")
        .map(PathBuf::from)
        .or_else(|| {
            let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/data");
            // Prefer larger generated fixture when present.
            for name in ["jsonl_20mb.jsonl", "3k_lines_4mb.jsonl"] {
                let p = root.join(name);
                if p.is_file() {
                    return Some(p);
                }
            }
            None
        })
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/data/jsonl_20mb.jsonl")
        });
    if !path.is_file() {
        eprintln!(
            "missing {path:?}\n  generate: cargo run --release --example gen_jsonl_fixture -- benches/data/jsonl_20mb.jsonl\n  or place 3k_lines_4mb.jsonl under benches/data/"
        );
        std::process::exit(2);
    }
    let bytes = std::fs::read(&path).expect("read jsonl");
    eprintln!(
        "fixture={} bytes={:.2} MiB lines≈{}",
        path.display(),
        bytes.len() as f64 / 1024.0 / 1024.0,
        bytes.split(|&b| b == b'\n').filter(|l| !l.is_empty()).count()
    );
    bytes
}

fn main() {
    let buf = load();
    let n_lines = json_lines(&buf).count();
    eprintln!("json_lines count={n_lines}\n");

    let path_topic = parse_path("topic");
    let path_rid = parse_path("record_id");
    let path_status = parse_path("status"); // may be absent — we upsert via mutate after inject

    // ── 1) Scan all lines: extract topic (zero-copy / path) ───────────────
    let j_find = time_it(12, 2, || {
        let mut n = 0usize;
        for line in json_lines(&buf) {
            let t = find_value(line, &path_topic).unwrap();
            assert!(t.first() == Some(&b'"'));
            n += 1;
        }
        assert_eq!(n, n_lines);
    });
    let s_find = time_it(8, 1, || {
        let mut n = 0usize;
        for line in json_lines(&buf) {
            let v: serde_json::Value = serde_json::from_slice(line).unwrap();
            let _ = v["topic"].as_str().unwrap();
            n += 1;
        }
        assert_eq!(n, n_lines);
    });

    // ── 2) Derive typed meta read (topic + record_id only) ────────────────
    #[cfg(feature = "derive")]
    let j_view = time_it(12, 2, || {
        let mut n = 0usize;
        for line in json_lines(&buf) {
            let m = LessonMeta::read_from_json(line).unwrap();
            assert!(!m.topic.is_empty());
            n += 1;
        }
        assert_eq!(n, n_lines);
    });
    #[cfg(feature = "derive")]
    let s_view = time_it(8, 1, || {
        #[derive(serde::Deserialize)]
        struct Meta {
            topic: String,
            record_id: String,
        }
        let mut n = 0usize;
        for line in json_lines(&buf) {
            // serde still parses full JSON then drops unknown — full tokenize cost
            let m: Meta = serde_json::from_slice(line).unwrap();
            assert!(!m.topic.is_empty());
            let _ = m.record_id;
            n += 1;
        }
        assert_eq!(n, n_lines);
    });

    // ── 3) Project thin card per line (drop huge messages body) ───────────
    let j_proj = time_it(10, 2, || {
        let mut out_bytes = 0usize;
        for line in json_lines(&buf) {
            let card = project_paths(line, &["topic", "record_id", "source_file"]).unwrap();
            out_bytes += card.len();
        }
        assert!(out_bytes > n_lines * 10);
    });
    let j_jmes = time_it(10, 1, || {
        let mut out_bytes = 0usize;
        for line in json_lines(&buf) {
            // keep topic + first message role only (not full content)
            let card = project_jmespath(line, "{topic: topic, rid: record_id, role0: messages[0].role}")
                .unwrap();
            out_bytes += card.len();
        }
        assert!(out_bytes > n_lines * 10);
    });
    let s_proj = time_it(6, 1, || {
        let mut out_bytes = 0usize;
        for line in json_lines(&buf) {
            let v: serde_json::Value = serde_json::from_slice(line).unwrap();
            let card = serde_json::json!({
                "topic": v["topic"].clone(),
                "record_id": v["record_id"].clone(),
                "source_file": v["source_file"].clone(),
            });
            out_bytes += serde_json::to_vec(&card).unwrap().len();
        }
        assert!(out_bytes > n_lines * 10);
    });

    // ── 4) In-place stamp: inject "status":"ok" via full-line rewrite sim ─
    // Lines lack status; upsert_at_path on root key after parse path "status"
    // For fair compare: clone each line to Vec, mutate, sum lengths.
    let j_mut = time_it(8, 1, || {
        let mut total = 0usize;
        for line in json_lines(&buf) {
            let mut row = line.to_vec();
            // If status missing, upsert at root: use upsert_object_key on "" path
            // upsert_object_key needs object path to parent; root is the line itself.
            // Use upsert_at_path("status") which creates parents if needed — for root key
            // mutate may fail PathNotFound; inject by upsert_object_key with empty path.
            upsert_object_key(&mut row, &[], "status", br#""ok""#).unwrap();
            total += row.len();
        }
        assert!(total > buf.len() / 2);
    });
    let s_mut = time_it(6, 1, || {
        let mut total = 0usize;
        for line in json_lines(&buf) {
            let mut v: serde_json::Value = serde_json::from_slice(line).unwrap();
            v.as_object_mut()
                .unwrap()
                .insert("status".into(), serde_json::json!("ok"));
            total += serde_json::to_vec(&v).unwrap().len();
        }
        assert!(total > buf.len() / 2);
    });

    // ── 5) Count messages length without full tree (array_len) ────────────
    let path_msgs = parse_path("messages");
    let j_len = time_it(12, 2, || {
        let mut sum = 0usize;
        for line in json_lines(&buf) {
            sum += jshift::array_len(line, &path_msgs).unwrap_or(0);
        }
        assert!(sum >= n_lines);
    });
    let s_len = time_it(8, 1, || {
        let mut sum = 0usize;
        for line in json_lines(&buf) {
            let v: serde_json::Value = serde_json::from_slice(line).unwrap();
            sum += v["messages"].as_array().map(|a| a.len()).unwrap_or(0);
        }
        assert!(sum >= n_lines);
    });

    // ── 6) Baseline: just iterate lines ───────────────────────────────────
    let walk = time_it(30, 5, || {
        let mut n = 0usize;
        for line in json_lines(&buf) {
            n += line.len();
        }
        assert!(n > 0);
    });

    println!("=== JSONL bench ({n_lines} lines, ~{:.2} MiB) ===\n", buf.len() as f64 / 1024.0 / 1024.0);
    println!("| Workload | jshift | serde_json | jshift vs serde |");
    println!("| :--- | ---: | ---: | ---: |");
    println!(
        "| Line walk only (`json_lines`) | {} | — | — |",
        fmt(walk)
    );
    println!(
        "| Find `topic` every line | {} | {} | {} |",
        fmt(j_find),
        fmt(s_find),
        ratio(j_find, s_find)
    );
    #[cfg(feature = "derive")]
    println!(
        "| Derive meta read (topic+record_id) | {} | {} | {} |",
        fmt(j_view),
        fmt(s_view),
        ratio(j_view, s_view)
    );
    println!(
        "| Project thin card (3 fields) | {} | {} | {} |",
        fmt(j_proj),
        fmt(s_proj),
        ratio(j_proj, s_proj)
    );
    println!(
        "| JMES thin card + messages[0].role | {} | (see project) | — |",
        fmt(j_jmes)
    );
    println!(
        "| Stamp `status` per line (upsert) | {} | {} | {} |",
        fmt(j_mut),
        fmt(s_mut),
        ratio(j_mut, s_mut)
    );
    println!(
        "| `array_len(messages)` every line | {} | {} | {} |",
        fmt(j_len),
        fmt(s_len),
        ratio(j_len, s_len)
    );

    // Throughput
    let mib = buf.len() as f64 / 1024.0 / 1024.0;
    println!("\nThroughput (input scan rate on find-topic):");
    println!(
        "  jshift  {:.1} MiB/s  ({:.0} lines/s)",
        mib / j_find.as_secs_f64(),
        n_lines as f64 / j_find.as_secs_f64()
    );
    println!(
        "  serde   {:.1} MiB/s  ({:.0} lines/s)",
        mib / s_find.as_secs_f64(),
        n_lines as f64 / s_find.as_secs_f64()
    );

    // Output compression story
    let mut thin = 0usize;
    for line in json_lines(&buf) {
        thin += project_paths(line, &["topic", "record_id", "source_file"])
            .unwrap()
            .len()
            + 1;
    }
    println!(
        "\nProjection size: input {:.2} MiB → thin cards {:.2} MiB ({:.1}% of input)",
        mib,
        thin as f64 / 1024.0 / 1024.0,
        100.0 * thin as f64 / buf.len() as f64
    );

    let _ = (path_rid, path_status);
}
