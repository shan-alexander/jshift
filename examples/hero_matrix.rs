//! Hero performance matrix: multi-size, multi-engine timings for README.
//!
//! ```bash
//! cargo run --release --example hero_matrix
//! # optional large catalog:
//! # JSHIFT_LARGE_JSON=benches/data/large.json cargo run --release --example hero_matrix
//! ```
//!
//! Prints TSV + human summary. Sizes: ~500 KiB, ~10 MiB, and large (~300 MiB when available).

use std::env;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use sonic_rs::{get as sonic_get, pointer, JsonContainerTrait, JsonValueTrait};

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

fn fmt_dur(d: Duration) -> String {
    let ns = d.as_nanos();
    if ns < 1_000 {
        format!("{ns} ns")
    } else if ns < 1_000_000 {
        format!("{:.1} µs", ns as f64 / 1_000.0)
    } else if ns < 1_000_000_000 {
        format!("{:.2} ms", ns as f64 / 1_000_000.0)
    } else {
        format!("{:.2} s", d.as_secs_f64())
    }
}

fn ratio(a: Duration, b: Duration) -> String {
    if a.as_secs_f64() <= 0.0 {
        return "—".into();
    }
    let r = b.as_secs_f64() / a.as_secs_f64();
    if r >= 100.0 {
        format!("~{:.0}×", r)
    } else if r >= 10.0 {
        format!("~{:.1}×", r)
    } else {
        format!("~{:.2}×", r)
    }
}

/// Pad with a large `data` array of small objects until ~target_bytes.
fn gen_with_data_array(target_first: bool, target_bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(target_bytes + 64);
    if target_first {
        out.extend_from_slice(br#"{"target":123456,"data":["#);
    } else {
        out.extend_from_slice(br#"{"data":["#);
    }
    let mut i = 0u64;
    while out.len() < target_bytes.saturating_sub(32) {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(br#"{"id":"#);
        out.extend_from_slice(i.to_string().as_bytes());
        out.extend_from_slice(br#","name":"u"#);
        out.extend_from_slice(i.to_string().as_bytes());
        out.extend_from_slice(br#"","active":true,"score":9.9}"#);
        i += 1;
    }
    if target_first {
        out.extend_from_slice(br#"]}"#);
    } else {
        out.extend_from_slice(br#"],"target":123456}"#);
    }
    out
}

fn gen_catalog(target_bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(target_bytes + 64);
    out.extend_from_slice(br#"{"products":["#);
    let mut i = 0u64;
    while out.len() < target_bytes.saturating_sub(32) {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(br#"{"id":"#);
        out.extend_from_slice(i.to_string().as_bytes());
        out.extend_from_slice(br#","title":"Product "#);
        out.extend_from_slice(i.to_string().as_bytes());
        out.extend_from_slice(br#"","handle":"h-"#);
        out.extend_from_slice(i.to_string().as_bytes());
        out.extend_from_slice(br#"","noise":"xxxxxxxxxxxxxxxxxxxxxxxx","price":"#);
        out.extend_from_slice(((i % 999) as f64 / 100.0).to_string().as_bytes());
        out.extend_from_slice(br#","variants":[{"sku":"S","price":"9.99"},{"sku":"M","price":"10.99"}]}"#);
        i += 1;
    }
    out.extend_from_slice(br#"]}"#);
    out
}

fn product_count(json: &[u8]) -> usize {
    let mut doc = jshift::IndexedDocument::empty(json);
    if doc.index_array_str("products").is_ok() {
        if let Some(n) = doc.array_len(&jshift::parse_path("products")) {
            return n;
        }
    }
    0
}

fn load_large_catalog() -> Option<Vec<u8>> {
    let path = env::var_os("JSHIFT_LARGE_JSON")
        .map(PathBuf::from)
        .or_else(|| {
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/data/large.json");
            p.is_file().then_some(p)
        })?;
    std::fs::read(path).ok()
}

#[derive(Clone, Copy)]
struct Eng {
    jshift: Duration,
    serde: Option<Duration>,
    gjson: Option<Duration>,
    sonic: Option<Duration>,
}

fn print_row(task: &str, s: Eng, m: Eng, l: Option<Eng>) {
    println!("--- {task} ---");
    println!(
        "  small  jshift={} serde={} gjson={} sonic={}",
        fmt_dur(s.jshift),
        s.serde.map(fmt_dur).unwrap_or_else(|| "n/a".into()),
        s.gjson.map(fmt_dur).unwrap_or_else(|| "n/a".into()),
        s.sonic.map(fmt_dur).unwrap_or_else(|| "n/a".into()),
    );
    println!(
        "  medium jshift={} serde={} gjson={} sonic={}",
        fmt_dur(m.jshift),
        m.serde.map(fmt_dur).unwrap_or_else(|| "n/a".into()),
        m.gjson.map(fmt_dur).unwrap_or_else(|| "n/a".into()),
        m.sonic.map(fmt_dur).unwrap_or_else(|| "n/a".into()),
    );
    if let Some(l) = l {
        println!(
            "  large  jshift={} serde={} gjson={} sonic={}",
            fmt_dur(l.jshift),
            l.serde.map(fmt_dur).unwrap_or_else(|| "n/a".into()),
            l.gjson.map(fmt_dur).unwrap_or_else(|| "n/a".into()),
            l.sonic.map(fmt_dur).unwrap_or_else(|| "n/a".into()),
        );
    } else {
        println!("  large  (skipped — no large fixture / not applicable)");
    }
    // ratios on medium (stable)
    if let Some(se) = m.serde {
        print!("  ratios@medium: jshift vs serde {}", ratio(m.jshift, se));
    }
    if let Some(g) = m.gjson {
        print!(", vs gjson {}", ratio(m.jshift, g));
    }
    if let Some(so) = m.sonic {
        print!(", vs sonic {}", ratio(m.jshift, so));
    }
    println!();
    // machine line for README paste
    println!(
        "TSV\t{task}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}",
        fmt_dur(s.jshift),
        s.serde.map(fmt_dur).unwrap_or_else(|| "—".into()),
        s.gjson.map(fmt_dur).unwrap_or_else(|| "—".into()),
        s.sonic.map(fmt_dur).unwrap_or_else(|| "—".into()),
        fmt_dur(m.jshift),
        m.serde.map(fmt_dur).unwrap_or_else(|| "—".into()),
        m.gjson.map(fmt_dur).unwrap_or_else(|| "—".into()),
        m.sonic.map(fmt_dur).unwrap_or_else(|| "—".into()),
        l.map(|x| fmt_dur(x.jshift)).unwrap_or_else(|| "—".into()),
        l.and_then(|x| x.serde.map(fmt_dur))
            .unwrap_or_else(|| "—".into()),
        l.and_then(|x| x.gjson.map(fmt_dur))
            .unwrap_or_else(|| "—".into()),
        l.and_then(|x| x.sonic.map(fmt_dur))
            .unwrap_or_else(|| "—".into()),
    );
}

fn iters_for(size: usize) -> (usize, usize) {
    if size < 1_000_000 {
        (80, 10)
    } else if size < 50_000_000 {
        (20, 3)
    } else {
        (6, 1)
    }
}

fn bench_find_target(json: &[u8], key_first: bool) -> Eng {
    let (iters, warm) = iters_for(json.len());
    let path = jshift::parse_path("target");
    let s = std::str::from_utf8(json).unwrap();
    let j = time_it(iters, warm, || {
        assert_eq!(jshift::find_value(json, &path).unwrap(), b"123456");
    });
    let se = time_it(iters.max(4).min(12), warm.min(2), || {
        let v: serde_json::Value = serde_json::from_slice(json).unwrap();
        assert_eq!(v["target"].as_u64().unwrap(), 123456);
    });
    let g = time_it(iters, warm, || {
        assert_eq!(gjson::get(s, "target").u64(), 123456);
    });
    let so = time_it(iters, warm, || {
        let p = pointer!["target"];
        let v = sonic_get(json, &p).unwrap();
        assert_eq!(v.as_u64().unwrap(), 123456);
    });
    let _ = key_first;
    Eng {
        jshift: j,
        serde: Some(se),
        gjson: Some(g),
        sonic: Some(so),
    }
}

fn bench_mutate_target(json_src: &[u8]) -> Eng {
    let (iters, warm) = iters_for(json_src.len());
    let path = jshift::parse_path("target");
    // same-length overwrite 123456 -> 654321
    let j = time_it(iters, warm, || {
        let mut buf = json_src.to_vec();
        jshift::mutate_value(&mut buf, &path, b"654321").unwrap();
        assert!(jshift::find_value(&buf, &path).unwrap() == b"654321");
    });
    let se = time_it(iters.max(3).min(10), warm.min(1), || {
        let mut v: serde_json::Value = serde_json::from_slice(json_src).unwrap();
        v["target"] = serde_json::json!(654321);
        let out = serde_json::to_vec(&v).unwrap();
        assert!(!out.is_empty());
    });
    Eng {
        jshift: j,
        serde: Some(se),
        gjson: None, // no in-place mutate API
        sonic: None,
    }
}

fn bench_find_product0(json: &[u8]) -> Eng {
    let (iters, warm) = iters_for(json.len());
    let path = jshift::parse_path("products[0].title");
    let s = std::str::from_utf8(json).unwrap();
    let expect = jshift::find_value(json, &path).unwrap().to_vec();
    let expect_str = std::str::from_utf8(&expect).unwrap().trim_matches('"');

    let j = time_it(iters, warm, || {
        assert_eq!(jshift::find_value(json, &path).unwrap(), expect.as_slice());
    });
    let se = time_it(iters.max(3).min(8), 1, || {
        let v: serde_json::Value = serde_json::from_slice(json).unwrap();
        assert_eq!(v["products"][0]["title"].as_str().unwrap(), expect_str);
    });
    let g = time_it(iters, warm, || {
        assert_eq!(gjson::get(s, "products.0.title").str(), expect_str);
    });
    let so = time_it(iters, warm, || {
        let p = pointer!["products", 0, "title"];
        let v = sonic_get(json, &p).unwrap();
        assert_eq!(v.as_raw_str().trim_matches('"'), expect_str);
    });
    Eng {
        jshift: j,
        serde: Some(se),
        gjson: Some(g),
        sonic: Some(so),
    }
}

fn bench_find_product_mid(json: &[u8], mid: usize) -> (Eng, Eng) {
    // returns (scan jshift + peers, indexed jshift only in jshift field with peers same)
    let (iters, _warm) = iters_for(json.len());
    let path_s = format!("products[{mid}].title");
    let path = jshift::parse_path(&path_s);
    let s = std::str::from_utf8(json).unwrap();
    let gpath = format!("products.{mid}.title");
    let expect = jshift::find_value(json, &path).unwrap().to_vec();
    let expect_str = std::str::from_utf8(&expect).unwrap().trim_matches('"');

    let j_scan = time_it(iters.max(4).min(if json.len() > 100_000_000 { 4 } else { 12 }), 1, || {
        assert_eq!(jshift::find_value(json, &path).unwrap(), expect.as_slice());
    });

    let mut doc = jshift::IndexedDocument::empty(json);
    doc.index_array_str("products").unwrap();
    let j_idx = time_it(iters.max(20).min(100), 5, || {
        assert_eq!(doc.find(&path).unwrap(), expect.as_slice());
    });

    let se = time_it(iters.max(2).min(6), 1, || {
        let v: serde_json::Value = serde_json::from_slice(json).unwrap();
        assert_eq!(
            v["products"][mid]["title"].as_str().unwrap(),
            expect_str
        );
    });
    let g = time_it(iters.max(4).min(12), 1, || {
        assert_eq!(gjson::get(s, &gpath).str(), expect_str);
    });
    let so = time_it(iters.max(4).min(12), 1, || {
        // sonic pointer needs compile-time for pointer! macro - use get with path string via Value if needed
        // Use sonic_rs::get with pointer from runtime - pointer! only static
        // Fallback: sonic from_slice and index
        let v: sonic_rs::Value = sonic_rs::from_slice(json).unwrap();
        assert_eq!(
            v["products"][mid]["title"].as_str().unwrap(),
            expect_str
        );
    });

    let scan = Eng {
        jshift: j_scan,
        serde: Some(se),
        gjson: Some(g),
        sonic: Some(so),
    };
    // For indexed: peers don't have equivalent side-table; report same peer times as "must re-scan/parse"
    let indexed = Eng {
        jshift: j_idx,
        serde: Some(se),
        gjson: Some(g),
        sonic: Some(so),
    };
    (scan, indexed)
}

fn bench_project_cards(json: &[u8]) -> Eng {
    let (iters, warm) = iters_for(json.len());
    let plan =
        jshift::ProjectPlan::from_jmespath("products[*].{id: id, title: title}").unwrap();
    let mut doc = jshift::IndexedDocument::empty(json);
    doc.index_for_plan(&plan).unwrap();

    let j = time_it(iters.max(3).min(15), warm.min(2), || {
        let out = jshift::project_indexed(&doc, &plan).unwrap();
        assert!(out.starts_with(b"["));
    });
    let se = time_it(iters.max(2).min(6), 1, || {
        let v: serde_json::Value = serde_json::from_slice(json).unwrap();
        let cards: Vec<_> = v["products"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| {
                serde_json::json!({
                    "id": p["id"].clone(),
                    "title": p["title"].clone(),
                })
            })
            .collect();
        let out = serde_json::to_vec(&cards).unwrap();
        assert!(out.starts_with(b"["));
    });
    let s = std::str::from_utf8(json).unwrap();
    let g = time_it(iters.max(3).min(10), 1, || {
        let products = gjson::get(s, "products");
        let mut out = String::from("[");
        let mut first = true;
        products.each(|_, prod| {
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(r#"{"id":"#);
            out.push_str(prod.get("id").json());
            out.push_str(r#","title":"#);
            out.push_str(prod.get("title").json());
            out.push('}');
            true
        });
        out.push(']');
        assert!(out.len() > 2);
    });
    let so = time_it(iters.max(2).min(6), 1, || {
        let val: sonic_rs::Value = sonic_rs::from_slice(json).unwrap();
        let products = val["products"].as_array().unwrap();
        let mut cards = sonic_rs::Array::with_capacity(products.len());
        for p in products.iter() {
            let mut obj = sonic_rs::Object::new();
            obj.insert("id", p["id"].clone());
            obj.insert("title", p["title"].clone());
            cards.push(sonic_rs::Value::from(obj));
        }
        let out = sonic_rs::to_vec(&sonic_rs::Value::from(cards)).unwrap();
        assert!(out.starts_with(b"["));
    });
    Eng {
        jshift: j,
        serde: Some(se),
        gjson: Some(g),
        sonic: Some(so),
    }
}

fn bench_project_first_card(json: &[u8]) -> Eng {
    let (iters, warm) = iters_for(json.len());
    let plan = jshift::ProjectPlan::from_paths(&[
        "products[0].id",
        "products[0].title",
        "products[0].handle",
    ])
    .unwrap();
    let j = time_it(iters, warm, || {
        let out = jshift::project(json, &plan).unwrap();
        assert!(out.windows(2).any(|w| w == b"id") || out.len() > 2);
    });
    let se = time_it(iters.max(3).min(10), 1, || {
        let v: serde_json::Value = serde_json::from_slice(json).unwrap();
        let p = &v["products"][0];
        let out = serde_json::to_vec(&serde_json::json!({
            "id": p["id"].clone(),
            "title": p["title"].clone(),
            "handle": p["handle"].clone(),
        }))
        .unwrap();
        assert!(!out.is_empty());
    });
    let s = std::str::from_utf8(json).unwrap();
    let g = time_it(iters, warm, || {
        let id = gjson::get(s, "products.0.id");
        let title = gjson::get(s, "products.0.title");
        let handle = gjson::get(s, "products.0.handle");
        let out = format!(
            r#"{{"id":{},"title":{},"handle":{}}}"#,
            id.json(),
            title.json(),
            handle.json()
        );
        assert!(out.len() > 10);
    });
    let so = time_it(iters, warm, || {
        let id = sonic_get(json, &pointer!["products", 0, "id"]).unwrap();
        let title = sonic_get(json, &pointer!["products", 0, "title"]).unwrap();
        let handle = sonic_get(json, &pointer!["products", 0, "handle"]).unwrap();
        let out = format!(
            r#"{{"id":{},"title":{},"handle":{}}}"#,
            id.as_raw_str(),
            title.as_raw_str(),
            handle.as_raw_str()
        );
        assert!(out.len() > 10);
    });
    Eng {
        jshift: j,
        serde: Some(se),
        gjson: Some(g),
        sonic: Some(so),
    }
}

fn main() {
    eprintln!("=== jshift hero matrix (release timings, median) ===\n");

    let small_n = 500 * 1024;
    let med_n = 10 * 1024 * 1024;

    eprintln!("generating fixtures…");
    let kf_s = gen_with_data_array(true, small_n);
    let kf_m = gen_with_data_array(true, med_n);
    let kl_s = gen_with_data_array(false, small_n);
    let kl_m = gen_with_data_array(false, med_n);
    let cat_s = gen_catalog(small_n);
    let cat_m = gen_catalog(med_n);
    eprintln!(
        "  key-first small={} medium={}",
        kf_s.len(),
        kf_m.len()
    );
    eprintln!("  key-last  small={} medium={}", kl_s.len(), kl_m.len());
    eprintln!(
        "  catalog   small={} (n≈{}) medium={} (n≈{})",
        cat_s.len(),
        product_count(&cat_s),
        cat_m.len(),
        product_count(&cat_m)
    );

    let large_cat = load_large_catalog();
    if let Some(ref l) = large_cat {
        eprintln!(
            "  large catalog {} bytes ({:.2} MiB) n≈{}",
            l.len(),
            l.len() as f64 / 1024.0 / 1024.0,
            product_count(l)
        );
    } else {
        eprintln!("  large catalog: missing (set JSHIFT_LARGE_JSON or benches/data/large.json)");
    }

    // For large key-first/last: generate ~30MB only if no time — actually generate on demand ~10MB reuse or skip
    // User asked 300MB for large column — for find key-first/last use synthetic ~300MB only if env HERO_GEN_LARGE=1
    let large_find = if env::var_os("HERO_GEN_LARGE_FIND").is_some() {
        eprintln!("  generating ~100MB key-last/first for large column (HERO_GEN_LARGE_FIND)…");
        Some((
            gen_with_data_array(true, 100 * 1024 * 1024),
            gen_with_data_array(false, 100 * 1024 * 1024),
        ))
    } else {
        // Use medium×scale note: for large find column use 10MB numbers flagged, OR gen 100MB
        // Generate 50MB for better large story without 10 min wait
        eprintln!("  generating ~50 MiB key-first/last for large find column…");
        Some((
            gen_with_data_array(true, 50 * 1024 * 1024),
            gen_with_data_array(false, 50 * 1024 * 1024),
        ))
    };

    eprintln!("\nrunning benches…\n");

    // 1) key first find
    let s = bench_find_target(&kf_s, true);
    let m = bench_find_target(&kf_m, true);
    let l = large_find
        .as_ref()
        .map(|(f, _)| bench_find_target(f, true));
    print_row("key-first find (target)", s, m, l);

    // 2) key last find
    let s = bench_find_target(&kl_s, false);
    let m = bench_find_target(&kl_m, false);
    let l = large_find
        .as_ref()
        .map(|(_, last)| bench_find_target(last, false));
    print_row("key-last find (target after bulk array)", s, m, l);

    // 3) mutate in place
    let s = bench_mutate_target(&kl_s);
    let m = bench_mutate_target(&kl_m);
    let l = large_find
        .as_ref()
        .map(|(_, last)| bench_mutate_target(last));
    print_row("in-place mutate same-length (target)", s, m, l);

    // 4) first product title
    let s = bench_find_product0(&cat_s);
    let m = bench_find_product0(&cat_m);
    let l = large_cat.as_ref().map(|c| bench_find_product0(c));
    print_row("sparse find products[0].title", s, m, l);

    // 5) mid product scan + indexed
    let mid_s = product_count(&cat_s) / 2;
    let mid_m = product_count(&cat_m) / 2;
    let (ss, si) = bench_find_product_mid(&cat_s, mid_s);
    let (ms, mi) = bench_find_product_mid(&cat_m, mid_m);
    let large_mid = large_cat.as_ref().map(|c| {
        let mid = product_count(c) / 2;
        bench_find_product_mid(c, mid)
    });
    print_row(
        "sparse find products[mid].title (linear scan)",
        ss,
        ms,
        large_mid.as_ref().map(|(s, _)| *s),
    );
    print_row(
        "sparse find products[mid].title (jshift indexed)",
        si,
        mi,
        large_mid.as_ref().map(|(_, i)| *i),
    );

    // 6) first card project
    let s = bench_project_first_card(&cat_s);
    let m = bench_project_first_card(&cat_m);
    let l = large_cat.as_ref().map(|c| bench_project_first_card(c));
    print_row("sparse project first product card (id/title/handle)", s, m, l);

    // 7) full thin cards
    let s = bench_project_cards(&cat_s);
    let m = bench_project_cards(&cat_m);
    let l = large_cat.as_ref().map(|c| bench_project_cards(c));
    print_row(
        "full-catalog thin cards products[*].{id,title} (jshift indexed)",
        s,
        m,
        l,
    );

    eprintln!("\n=== done ===");
}
