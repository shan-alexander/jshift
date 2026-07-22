//! TypedDoc vs serde_json — sparse get, stream each, open mutate.
//!
//! Run: `cargo run --release --example typed_doc_bench`
//!
//! Fairness notes:
//! - **Sparse get**: jshift decodes one path; serde builds a full `Value` tree.
//! - **Each**: jshift streams element spans; serde materializes the whole doc then walks.
//! - **Mutate**: jshift splices in place; serde parse → edit → `to_vec`.

use std::time::{Duration, Instant};

use jshift::{
    parse_path, ArrayBuilder, FromJsonSlice, JsonDoc, JsonView, ObjectBuilder, RawJson, TypedDoc,
};

const WARMUP: usize = 3;
const ITERS_LARGE: usize = 20;
const ITERS_MED: usize = 50;
const ITERS_SMALL: usize = 5_000;

fn mean_ns(iters: usize, mut body: impl FnMut()) -> f64 {
    for _ in 0..WARMUP {
        body();
    }
    let t0 = Instant::now();
    for _ in 0..iters {
        body();
    }
    t0.elapsed().as_secs_f64() * 1e9 / iters as f64
}

fn fmt_time(ns: f64) -> String {
    if ns < 1_000.0 {
        format!("{ns:.0} ns")
    } else if ns < 1_000_000.0 {
        format!("{:.1} µs", ns / 1_000.0)
    } else if ns < 1_000_000_000.0 {
        format!("{:.2} ms", ns / 1_000_000.0)
    } else {
        format!("{:.2} s", ns / 1_000_000_000.0)
    }
}

fn ratio(serde_ns: f64, jshift_ns: f64) -> String {
    if jshift_ns <= 0.0 {
        return "∞".into();
    }
    format!("{:.1}×", serde_ns / jshift_ns)
}

fn gen_catalog(n: usize, key_first: bool) -> Vec<u8> {
    let mut s = String::with_capacity(n * 48 + 64);
    if key_first {
        s.push_str(r#"{"status":"ok","products":["#);
    } else {
        s.push_str(r#"{"products":["#);
    }
    for i in 0..n {
        if i > 0 {
            s.push(',');
        }
        s.push_str(r#"{"id":"#);
        s.push_str(&i.to_string());
        s.push_str(r#","title":"item_"#);
        s.push_str(&i.to_string());
        s.push_str(r#"","price":9.99,"noise":true}"#);
    }
    if key_first {
        s.push_str("]}");
    } else {
        s.push_str(r#"],"status":"ok"}"#);
    }
    s.into_bytes()
}

fn gen_small() -> Vec<u8> {
    br#"{"status":"ok","id":7,"items":[{"id":1,"t":"a"},{"id":2,"t":"b"},{"id":3,"t":"c"}],"extra":true}"#
        .to_vec()
}

// Manual card views (no derive needed in example).
struct Card {
    id: u64,
    title: String,
}

impl JsonView for Card {
    fn read_from(json: &[u8]) -> Result<Self, jshift::Error> {
        let id_s = jshift::find_value(json, &parse_path("id"))?;
        let t_s = jshift::find_value(json, &parse_path("title"))?;
        let id = u64::from_json_slice(id_s).ok_or(jshift::Error::TypeMismatch {
            expected: "u64",
            found: "bad",
        })?;
        let title = String::from_json_slice(t_s).ok_or(jshift::Error::TypeMismatch {
            expected: "String",
            found: "bad",
        })?;
        Ok(Self { id, title })
    }

    fn write_into(&self, json: &mut Vec<u8>) -> Result<(), jshift::Error> {
        jshift::upsert_at_path(json, &parse_path("id"), &self.id.to_json_bytes())?;
        jshift::upsert_at_path(json, &parse_path("title"), &self.title.to_json_bytes())?;
        Ok(())
    }
}

/// Sparse card (id only) — preferred jshift shape for streams.
struct IdCard {
    id: u64,
}

impl JsonView for IdCard {
    fn read_from(json: &[u8]) -> Result<Self, jshift::Error> {
        let id_s = jshift::find_value(json, &parse_path("id"))?;
        let id = u64::from_json_slice(id_s).ok_or(jshift::Error::TypeMismatch {
            expected: "u64",
            found: "bad",
        })?;
        Ok(Self { id })
    }

    fn write_into(&self, json: &mut Vec<u8>) -> Result<(), jshift::Error> {
        jshift::upsert_at_path(json, &parse_path("id"), &self.id.to_json_bytes())
    }
}

use jshift::ToJsonBytes;

fn print_row(name: &str, j_ns: f64, s_ns: f64) {
    println!(
        "  {:<42}  jshift {:>12}   serde {:>12}   serde/jshift {:>8}",
        name,
        fmt_time(j_ns),
        fmt_time(s_ns),
        ratio(s_ns, j_ns)
    );
}

fn bench_sparse_get_u64(label: &str, json: &[u8], path: &str, iters: usize) {
    let doc = TypedDoc::from_slice(json);
    let j = mean_ns(iters, || {
        let v: u64 = doc.get(path).unwrap();
        std::hint::black_box(v);
    });

    let s = mean_ns(iters, || {
        let val: serde_json::Value = serde_json::from_slice(json).unwrap();
        let v = walk_serde_u64(&val, path);
        std::hint::black_box(v);
    });

    print_row(label, j, s);
}

fn walk_serde_u64(val: &serde_json::Value, path: &str) -> u64 {
    match path {
        "id" => val["id"].as_u64().unwrap(),
        "products[0].id" => val["products"][0]["id"].as_u64().unwrap(),
        _ => panic!("unknown u64 path {path}"),
    }
}

fn bench_get_str(label: &str, json: &[u8], path: &str, iters: usize) {
    let doc = TypedDoc::from_slice(json);
    let j = mean_ns(iters, || {
        let v = doc.get_str(path).unwrap();
        std::hint::black_box(v);
    });
    let s = mean_ns(iters, || {
        let val: serde_json::Value = serde_json::from_slice(json).unwrap();
        let v = val["status"].as_str().unwrap();
        std::hint::black_box(v);
    });
    print_row(label, j, s);
}

fn bench_each_sum(label: &str, json: &[u8], n_expect: usize, iters: usize) {
    let doc = TypedDoc::from_slice(json);
    // Naive: re-parse "id" path string inside each element (old style)
    let j_naive = mean_ns(iters, || {
        let mut sum = 0u64;
        let mut count = 0usize;
        doc.each_with("products", |elem| {
            let id = u64::from_json_slice(jshift::find_value(elem, &parse_path("id")).unwrap())
                .unwrap();
            sum = sum.wrapping_add(id);
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, n_expect);
        std::hint::black_box(sum);
    });

    // Hot path: each_get parses relative field path once
    let j = mean_ns(iters, || {
        let mut sum = 0u64;
        let mut count = 0usize;
        doc.each_get("products", "id", |id: u64| {
            sum = sum.wrapping_add(id);
            count += 1;
            Ok(())
        })
        .unwrap();
        assert_eq!(count, n_expect);
        std::hint::black_box(sum);
    });

    let s = mean_ns(iters, || {
        let val: serde_json::Value = serde_json::from_slice(json).unwrap();
        let mut sum = 0u64;
        for p in val["products"].as_array().unwrap() {
            sum = sum.wrapping_add(p["id"].as_u64().unwrap());
        }
        std::hint::black_box(sum);
    });
    print_row(&format!("{label} (each_get)"), j, s);
    println!(
        "  {:<42}  each_get {:>12}   naive each {:>12}   naive/get {:>8}",
        "(internal) path reuse",
        fmt_time(j),
        fmt_time(j_naive),
        ratio(j_naive, j)
    );
}

fn bench_each_view(label: &str, json: &[u8], n_expect: usize, iters: usize) {
    let doc = TypedDoc::from_slice(json);
    let j_view = mean_ns(iters, || {
        let mut n = 0usize;
        doc.each_view_with("products", |c: Card| {
            n += 1;
            std::hint::black_box(c.id);
            Ok(())
        })
        .unwrap();
        assert_eq!(n, n_expect);
    });

    // Fair sparse: only id field (what most pipelines need)
    let j_sparse = mean_ns(iters, || {
        let mut n = 0usize;
        doc.each_get("products", "id", |id: u64| {
            n += 1;
            std::hint::black_box(id);
            Ok(())
        })
        .unwrap();
        assert_eq!(n, n_expect);
    });

    let s = mean_ns(iters, || {
        #[derive(serde::Deserialize)]
        struct SCard {
            id: u64,
            #[allow(dead_code)]
            title: String,
        }
        #[derive(serde::Deserialize)]
        struct Root {
            products: Vec<SCard>,
        }
        let root: Root = serde_json::from_slice(json).unwrap();
        assert_eq!(root.products.len(), n_expect);
        std::hint::black_box(root.products[0].id);
    });
    print_row(&format!("{label} full Card view"), j_view, s);
    print_row(&format!("{label} sparse id only"), j_sparse, s);
}

fn bench_mutate(label: &str, json: &[u8], iters: usize) {
    let j = mean_ns(iters, || {
        let mut doc = TypedDoc::from_slice(json);
        doc.mutate().set("status", "done").unwrap();
        std::hint::black_box(doc.as_bytes().len());
    });

    let s = mean_ns(iters, || {
        let mut val: serde_json::Value = serde_json::from_slice(json).unwrap();
        val["status"] = serde_json::Value::String("done".into());
        let out = serde_json::to_vec(&val).unwrap();
        std::hint::black_box(out.len());
    });
    print_row(label, j, s);
}

fn bench_path_reuse(label: &str, json: &[u8], iters: usize) {
    let doc = TypedDoc::from_slice(json);
    let path = jshift::Path::parse("products[0].id");

    let j_str = mean_ns(iters, || {
        let v: u64 = doc.get("products[0].id").unwrap();
        std::hint::black_box(v);
    });
    let j_path = mean_ns(iters, || {
        let v: u64 = doc.get_path(&path.borrowed()).unwrap();
        std::hint::black_box(v);
    });
    println!(
        "  {:<42}  path-str {:>12}   Path reuse {:>12}   str/Path {:>8}",
        label,
        fmt_time(j_str),
        fmt_time(j_path),
        ratio(j_str, j_path)
    );
}

/// Random mid-element access: linear ViewList::get vs IndexedViewList vs serde.
fn bench_view_list_index(label: &str, json: &[u8], n: usize, mid: usize, iters: usize) {
    let doc = TypedDoc::from_slice(json);
    // Sparse IdCard so decode cost is fair for multi-get (title not needed).
    let list = doc.view_list::<IdCard>("products").unwrap();

    let j_linear = mean_ns(iters, || {
        let c = list.get(mid).unwrap();
        std::hint::black_box(c.id);
    });

    let j_prepare = mean_ns(iters.min(20).max(5), || {
        let indexed = list.index().unwrap();
        std::hint::black_box(indexed.len());
    });

    let indexed = list.index().unwrap();
    let j_idx = mean_ns(iters, || {
        let c = indexed.get(mid).unwrap();
        std::hint::black_box(c.id);
    });

    let probes: Vec<usize> = (0..64).map(|i| (i * 997 + mid) % n).collect();
    let j_multi = mean_ns(iters, || {
        let mut sum = 0u64;
        for &i in &probes {
            sum = sum.wrapping_add(indexed.get(i).unwrap().id);
        }
        std::hint::black_box(sum);
    });

    let s = mean_ns(iters, || {
        #[derive(serde::Deserialize)]
        struct SCard {
            id: u64,
        }
        #[derive(serde::Deserialize)]
        struct Root {
            products: Vec<SCard>,
        }
        let root: Root = serde_json::from_slice(json).unwrap();
        std::hint::black_box(root.products[mid].id);
    });

    let s_multi = mean_ns(iters, || {
        #[derive(serde::Deserialize)]
        struct SCard {
            id: u64,
        }
        #[derive(serde::Deserialize)]
        struct Root {
            products: Vec<SCard>,
        }
        let root: Root = serde_json::from_slice(json).unwrap();
        let mut sum = 0u64;
        for &i in &probes {
            sum = sum.wrapping_add(root.products[i].id);
        }
        std::hint::black_box(sum);
    });

    print_row(&format!("{label} linear get[{mid}]"), j_linear, s);
    print_row(
        &format!("{label} indexed get[{mid}] (after index)"),
        j_idx,
        s,
    );
    print_row(&format!("{label} 64× indexed get"), j_multi, s_multi);
    println!(
        "  {:<42}  linear {:>12}   indexed {:>12}   linear/idx {:>8}",
        "(internal) mid get",
        fmt_time(j_linear),
        fmt_time(j_idx),
        ratio(j_linear, j_idx)
    );
    println!(
        "  {:<42}  index() prepare {:>12}  (one array walk)",
        "(internal) ViewList::index",
        fmt_time(j_prepare)
    );
}

fn bench_build(n_fields: usize, iters: usize) {
    let j_fluent = mean_ns(iters, || {
        let mut b = ObjectBuilder::new();
        b = b.field("status", "ok");
        b = b.array_field("ids", |a| {
            let mut a = a;
            for i in 0..n_fields {
                a = a.item(&(i as u64));
            }
            a
        });
        // nested object in-place
        b = b.object_field("meta", |o| o.field("n", &(n_fields as u64)).null_field("x"));
        let out = b.finish();
        std::hint::black_box(out.len());
    });

    let j_writer = mean_ns(iters, || {
        let mut w = jshift::JsonWriter::with_capacity(n_fields * 4 + 64);
        w.begin_object().unwrap();
        w.field("status", "ok").unwrap();
        w.key("ids").unwrap();
        w.begin_array().unwrap();
        for i in 0..n_fields {
            w.value(&(i as u64)).unwrap();
        }
        w.end_array().unwrap();
        w.key("meta").unwrap();
        w.begin_object().unwrap();
        w.field("n", &(n_fields as u64)).unwrap();
        w.null_field("x").unwrap();
        w.end_object().unwrap();
        let out = w.finish().unwrap();
        std::hint::black_box(out.len());
    });

    let s = mean_ns(iters, || {
        let mut map = serde_json::Map::new();
        map.insert("status".into(), serde_json::Value::String("ok".into()));
        let ids: Vec<serde_json::Value> = (0..n_fields)
            .map(|i| serde_json::Value::Number(i.into()))
            .collect();
        map.insert("ids".into(), serde_json::Value::Array(ids));
        let mut meta = serde_json::Map::new();
        meta.insert("n".into(), serde_json::Value::Number(n_fields.into()));
        meta.insert("x".into(), serde_json::Value::Null);
        map.insert("meta".into(), serde_json::Value::Object(meta));
        let out = serde_json::to_vec(&serde_json::Value::Object(map)).unwrap();
        std::hint::black_box(out.len());
    });

    print_row(
        &format!("fluent ObjectBuilder + nest ({n_fields} ids)"),
        j_fluent,
        s,
    );
    print_row(
        &format!("JsonWriter imperative ({n_fields} ids)"),
        j_writer,
        s,
    );
    println!(
        "  {:<42}  fluent {:>12}   writer {:>12}   fluent/writer {:>8}",
        "(internal) build style",
        fmt_time(j_fluent),
        fmt_time(j_writer),
        ratio(j_fluent, j_writer)
    );
}

fn bench_raw_json(json: &[u8], iters: usize) {
    let doc = TypedDoc::from_slice(json);
    let j = mean_ns(iters, || {
        let raw: RawJson = doc.get("products[0]").unwrap();
        let id: u64 = raw.get("id").unwrap();
        std::hint::black_box(id);
    });
    let s = mean_ns(iters, || {
        let val: serde_json::Value = serde_json::from_slice(json).unwrap();
        let id = val["products"][0]["id"].as_u64().unwrap();
        std::hint::black_box(id);
    });
    print_row("RawJson get products[0] then id", j, s);
}

fn bench_object_entries(json: &[u8], iters: usize) {
    // flat object with many keys
    let mut s = String::from("{");
    for i in 0..200 {
        if i > 0 {
            s.push(',');
        }
        s.push_str(&format!(r#""k{i}":{i}"#));
    }
    s.push('}');
    let flat = s.into_bytes();

    let doc = TypedDoc::from_slice(&flat);
    let j = mean_ns(iters, || {
        let mut sum = 0u64;
        for e in doc.object_entries().unwrap() {
            let e = e.unwrap();
            sum = sum.wrapping_add(e.get::<u64>().unwrap());
        }
        std::hint::black_box(sum);
    });
    let s_ns = mean_ns(iters, || {
        let val: serde_json::Value = serde_json::from_slice(&flat).unwrap();
        let mut sum = 0u64;
        for (_k, v) in val.as_object().unwrap() {
            sum = sum.wrapping_add(v.as_u64().unwrap());
        }
        std::hint::black_box(sum);
    });
    print_row("object_entries sum 200 keys", j, s_ns);
    let _ = json; // silence if unused when only flat used
}

fn bench_nullable(iters: usize) {
    let json = br#"{"a":1,"b":null,"c":2}"#;
    let doc = TypedDoc::from_slice(json);
    let j = mean_ns(iters, || {
        let a = doc.get_nullable::<u64>("a").unwrap();
        let b = doc.get_nullable::<u64>("b").unwrap();
        let d = doc.get_nullable::<u64>("missing").unwrap();
        std::hint::black_box((a, b, d));
    });
    let s = mean_ns(iters, || {
        let val: serde_json::Value = serde_json::from_slice(json).unwrap();
        let a = val.get("a").and_then(|v| v.as_u64());
        let b = val.get("b").and_then(|v| {
            if v.is_null() {
                None
            } else {
                v.as_u64()
            }
        });
        let d = val.get("missing").and_then(|v| v.as_u64());
        std::hint::black_box((a, b, d));
    });
    print_row("get_nullable a/b/missing", j, s);
}

fn bench_root_array(n: usize, iters: usize) {
    let mut ab = ArrayBuilder::new();
    for i in 0..n {
        ab = ab.object(|o| o.field("id", &(i as u64)).field("noise", &true));
    }
    let bytes = ab.finish();

    let doc = TypedDoc::from_slice(&bytes);
    let j = mean_ns(iters, || {
        let mut sum = 0u64;
        doc.root_view_list::<IdCard>()
            .unwrap()
            .each(|c| {
                sum = sum.wrapping_add(c.id);
                Ok(())
            })
            .unwrap();
        std::hint::black_box(sum);
    });
    let s = mean_ns(iters, || {
        #[derive(serde::Deserialize)]
        struct SCard {
            id: u64,
            #[allow(dead_code)]
            noise: bool,
        }
        let v: Vec<SCard> = serde_json::from_slice(&bytes).unwrap();
        let sum: u64 = v.iter().map(|c| c.id).sum();
        std::hint::black_box(sum);
    });
    print_row(&format!("root array {n} IdCard stream"), j, s);
}

fn main() {
    println!("jshift TypedDoc vs serde_json\n");

    let small = gen_small();
    let med = gen_catalog(2_000, true); // ~key-first, ~100KB-class
    let large_first = gen_catalog(50_000, true);
    let large_last = gen_catalog(50_000, false);

    println!(
        "payloads: small={} B, med={} KB, large≈{} MB",
        small.len(),
        med.len() / 1024,
        large_first.len() / (1024 * 1024)
    );
    println!();

    println!("── Sparse get (decode one field) ──");
    bench_sparse_get_u64("small get id", &small, "id", ITERS_SMALL);
    bench_get_str("small get_str status", &small, "status", ITERS_SMALL);
    bench_sparse_get_u64(
        "med products[0].id (key-first status)",
        &med,
        "products[0].id",
        ITERS_MED,
    );
    bench_sparse_get_u64(
        "large products[0].id key-first",
        &large_first,
        "products[0].id",
        ITERS_LARGE,
    );
    bench_get_str(
        "large status (key-last after products)",
        &large_last,
        "status",
        ITERS_LARGE,
    );

    println!();
    println!("── Stream each (sum product ids) ──");
    bench_each_sum("med each sum ids (2k)", &med, 2_000, ITERS_MED);
    bench_each_sum("large each sum ids (50k)", &large_first, 50_000, ITERS_LARGE.min(10));

    println!();
    println!("── each_view Card vs serde typed Vec ──");
    // smaller product count for view decode cost focus
    let cards = gen_catalog(500, true);
    bench_each_view("500 cards stream view vs full typed", &cards, 500, ITERS_MED);

    println!();
    println!("── Open mutate status (preserve unknowns) ──");
    bench_mutate("small mutate status", &small, ITERS_SMALL);
    bench_mutate("med mutate status", &med, ITERS_MED);
    bench_mutate("large mutate status (key-first)", &large_first, ITERS_LARGE.min(15));

    println!();
    println!("── Path parse overhead (jshift internal) ──");
    bench_path_reuse("large products[0].id", &large_first, ITERS_LARGE * 5);

    println!();
    println!("── ViewList::index O(1) mid / multi-get ──");
    let idx_cat = gen_catalog(5_000, true);
    bench_view_list_index("5k products", &idx_cat, 5_000, 2_500, ITERS_MED);

    println!();
    println!("── ObjectBuilder vs serde Map+to_vec ──");
    bench_build(64, ITERS_SMALL);
    bench_build(2_000, ITERS_MED);

    println!();
    println!("── RawJson dynamic pocket ──");
    bench_raw_json(&med, ITERS_MED);

    println!();
    println!("── object_entries (DynObject cursor) ──");
    bench_object_entries(&med, ITERS_MED);

    println!();
    println!("── get_nullable (missing vs null) ──");
    bench_nullable(ITERS_SMALL);

    println!();
    println!("── Root array ViewList ──");
    bench_root_array(500, ITERS_MED);

    println!();
    println!("done ({:?})", Duration::from_secs(0));
}
