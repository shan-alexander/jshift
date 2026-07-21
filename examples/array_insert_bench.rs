//! Bench array splice + derive mutator (`set_*` / `append_*` / `prepend_*` / `insert_*`)
//! and JMES field read on a ~50 MiB document vs serde_json.
//!
//! ```bash
//! cargo run --release --example array_insert_bench
//! # ARRAY_BENCH_MIB=50 ARRAY_BENCH_ITERS=12 cargo run --release --example array_insert_bench
//! ```

use std::env;
use std::time::{Duration, Instant};

use jshift::{
    append_to_array, array_len, insert_array_element, parse_path, prepend_to_array, project_jmespath,
    JsonMutatorSchema,
};

/// Open view over root metadata — does **not** load the huge `items` array.
#[derive(JsonMutatorSchema)]
struct CatalogMeta {
    status: String,
    tags: Vec<String>,
}

/// Sparse JMES read of the first catalog row (no full items materialization).
#[derive(JsonMutatorSchema)]
struct FirstItem {
    #[json(path = "id", jmes = "items[0].id")]
    id: u64,
    #[json(path = "t", jmes = "items[0].t")]
    t: String,
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

fn ratio(js: Duration, se: Duration) -> String {
    format!("~{:.1}×", se.as_secs_f64() / js.as_secs_f64().max(1e-12))
}

/// ~target_mib of `{"status":"ok","tags":["seed"],"items":[…]}`.
fn gen_catalog(target_mib: usize) -> Vec<u8> {
    let target = target_mib.saturating_mul(1024 * 1024);
    let mut out = Vec::with_capacity(target + 1024);
    out.extend_from_slice(br#"{"status":"ok","tags":["seed"],"items":["#);
    let mut i = 0u64;
    while out.len() < target {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(br#"{"id":"#);
        out.extend_from_slice(i.to_string().as_bytes());
        out.extend_from_slice(br#","t":"item-"#);
        out.extend_from_slice(i.to_string().as_bytes());
        out.extend_from_slice(br#"-xxxxxxxxxxxxxxxx","n":true}"#);
        i += 1;
    }
    out.extend_from_slice(br#"]}"#);
    out
}

fn main() {
    let mib: usize = env::var("ARRAY_BENCH_MIB")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(50);
    let iters: usize = env::var("ARRAY_BENCH_ITERS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(if mib >= 40 { 12 } else { 25 });
    let warm = 2usize;

    eprintln!("generating ~{mib} MiB catalog…");
    let base = gen_catalog(mib);
    let items_path = parse_path("items");
    let n = array_len(&base, &items_path).unwrap();
    let mid = n / 2;
    eprintln!(
        "size={:.2} MiB  items={n}  mid={mid}  iters={iters}\n",
        base.len() as f64 / 1024.0 / 1024.0
    );

    let elem = br#"{"id":999999,"t":"NEW","n":false}"#;

    // ── free-function array splice on huge `items` ───────────────────────
    let j_pre = time_it(iters, warm, || {
        let mut json = base.clone();
        prepend_to_array(&mut json, &items_path, elem).unwrap();
        assert_eq!(array_len(&json, &items_path).unwrap(), n + 1);
    });
    let s_pre = time_it(iters, 1, || {
        let mut v: serde_json::Value = serde_json::from_slice(&base).unwrap();
        v["items"]
            .as_array_mut()
            .unwrap()
            .insert(0, serde_json::json!({"id":999999,"t":"NEW","n":false}));
        let _ = serde_json::to_vec(&v).unwrap();
    });

    let j_mid = time_it(iters, warm, || {
        let mut json = base.clone();
        insert_array_element(&mut json, &items_path, mid, elem).unwrap();
        assert_eq!(array_len(&json, &items_path).unwrap(), n + 1);
    });
    let s_mid = time_it(iters, 1, || {
        let mut v: serde_json::Value = serde_json::from_slice(&base).unwrap();
        v["items"].as_array_mut().unwrap().insert(
            mid,
            serde_json::json!({"id":999999,"t":"NEW","n":false}),
        );
        let _ = serde_json::to_vec(&v).unwrap();
    });

    let j_end = time_it(iters, warm, || {
        let mut json = base.clone();
        insert_array_element(&mut json, &items_path, n, elem).unwrap();
        assert_eq!(array_len(&json, &items_path).unwrap(), n + 1);
    });
    let s_end = time_it(iters, 1, || {
        let mut v: serde_json::Value = serde_json::from_slice(&base).unwrap();
        v["items"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"id":999999,"t":"NEW","n":false}));
        let _ = serde_json::to_vec(&v).unwrap();
    });

    let j_app = time_it(iters, warm, || {
        let mut json = base.clone();
        append_to_array(&mut json, &items_path, elem).unwrap();
        assert_eq!(array_len(&json, &items_path).unwrap(), n + 1);
    });

    // ── derive mutator on open view (status + tags only; items unread) ───
    let j_set = time_it(iters, warm, || {
        let mut json = base.clone();
        let mut m = CatalogMeta::mutator(&mut json);
        m.set_status("skipped").unwrap();
        let meta = CatalogMeta::read_from_json(&json).unwrap();
        assert_eq!(meta.status, "skipped");
        assert_eq!(meta.tags, vec!["seed".to_string()]);
    });
    let s_set = time_it(iters, 1, || {
        let mut v: serde_json::Value = serde_json::from_slice(&base).unwrap();
        v["status"] = serde_json::json!("skipped");
        let out = serde_json::to_vec(&v).unwrap();
        assert!(out.windows(9).any(|w| w == b"\"skipped\""));
    });

    let j_m_app = time_it(iters, warm, || {
        let mut json = base.clone();
        let mut m = CatalogMeta::mutator(&mut json);
        m.append_tags("new").unwrap();
        let meta = CatalogMeta::read_from_json(&json).unwrap();
        assert_eq!(meta.tags, vec!["seed".to_string(), "new".to_string()]);
    });
    let s_m_app = time_it(iters, 1, || {
        let mut v: serde_json::Value = serde_json::from_slice(&base).unwrap();
        v["tags"].as_array_mut().unwrap().push(serde_json::json!("new"));
        let _ = serde_json::to_vec(&v).unwrap();
    });

    let j_m_pre = time_it(iters, warm, || {
        let mut json = base.clone();
        let mut m = CatalogMeta::mutator(&mut json);
        m.prepend_tags("hot").unwrap();
        let meta = CatalogMeta::read_from_json(&json).unwrap();
        assert_eq!(meta.tags[0], "hot");
    });
    let s_m_pre = time_it(iters, 1, || {
        let mut v: serde_json::Value = serde_json::from_slice(&base).unwrap();
        v["tags"]
            .as_array_mut()
            .unwrap()
            .insert(0, serde_json::json!("hot"));
        let _ = serde_json::to_vec(&v).unwrap();
    });

    let j_m_ins = time_it(iters, warm, || {
        let mut json = base.clone();
        let mut m = CatalogMeta::mutator(&mut json);
        m.insert_tags(1, "mid").unwrap(); // after "seed"
        let meta = CatalogMeta::read_from_json(&json).unwrap();
        assert_eq!(
            meta.tags,
            vec!["seed".to_string(), "mid".to_string()]
        );
    });
    let s_m_ins = time_it(iters, 1, || {
        let mut v: serde_json::Value = serde_json::from_slice(&base).unwrap();
        v["tags"]
            .as_array_mut()
            .unwrap()
            .insert(1, serde_json::json!("mid"));
        let _ = serde_json::to_vec(&v).unwrap();
    });

    // ── JMES sparse read of items[0] via derive + free project_jmespath ──
    let j_jmes_view = time_it(iters, warm, || {
        let first = FirstItem::read_from_json(&base).unwrap();
        assert_eq!(first.id, 0);
        assert!(first.t.starts_with("item-0"));
    });
    let j_jmes_proj = time_it(iters, warm, || {
        let out = project_jmespath(&base, "items[0].{id: id, t: t}").unwrap();
        assert!(out.windows(6).any(|w| w == b"\"id\":0") || out.contains(&b'0'));
    });
    let s_jmes = time_it(iters, 1, || {
        let v: serde_json::Value = serde_json::from_slice(&base).unwrap();
        let id = v["items"][0]["id"].as_u64().unwrap();
        let t = v["items"][0]["t"].as_str().unwrap().to_string();
        assert_eq!(id, 0);
        assert!(t.starts_with("item-0"));
    });

    println!("=== Array + mutator + JMES bench (~{mib} MiB, {n} items) ===\n");
    println!("| Workload | jshift | serde parse+edit+to_vec* | **jshift vs serde** |");
    println!("| :--- | ---: | ---: | ---: |");
    println!(
        "| Free **prepend** `items` | {} | {} | {} |",
        fmt(j_pre),
        fmt(s_pre),
        ratio(j_pre, s_pre)
    );
    println!(
        "| Free **insert mid** `items[{mid}]` | {} | {} | {} |",
        fmt(j_mid),
        fmt(s_mid),
        ratio(j_mid, s_mid)
    );
    println!(
        "| Free **insert end** / **append** `items` | {} / {} | {} | {} / {} |",
        fmt(j_end),
        fmt(j_app),
        fmt(s_end),
        ratio(j_end, s_end),
        ratio(j_app, s_end)
    );
    println!(
        "| Mutator **`set_status`** (open view) | {} | {} | {} |",
        fmt(j_set),
        fmt(s_set),
        ratio(j_set, s_set)
    );
    println!(
        "| Mutator **`append_tags`** | {} | {} | {} |",
        fmt(j_m_app),
        fmt(s_m_app),
        ratio(j_m_app, s_m_app)
    );
    println!(
        "| Mutator **`prepend_tags`** | {} | {} | {} |",
        fmt(j_m_pre),
        fmt(s_m_pre),
        ratio(j_m_pre, s_m_pre)
    );
    println!(
        "| Mutator **`insert_tags(1, …)`** | {} | {} | {} |",
        fmt(j_m_ins),
        fmt(s_m_ins),
        ratio(j_m_ins, s_m_ins)
    );
    println!(
        "| Derive **JMES** `items[0].id`/`t` read | {} | {} | {} |",
        fmt(j_jmes_view),
        fmt(s_jmes),
        ratio(j_jmes_view, s_jmes)
    );
    println!(
        "| Free **JMES** `items[0].{{id,t}}` project | {} | {} | {} |",
        fmt(j_jmes_proj),
        fmt(s_jmes),
        ratio(j_jmes_proj, s_jmes)
    );

    eprintln!(
        "\n*Serde column is full-document parse + edit + `to_vec` (except JMES rows: parse + index).\n\
         Mutator/JMES views never load `items` into a Rust `Vec`.\n\
         Clone/memmove of ~{mib} MiB still appears in splice-heavy free-function rows."
    );
}
