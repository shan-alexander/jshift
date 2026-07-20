//! Projection benches: synthetic large docs + optional on-disk 100–300 MiB fixtures.
//!
//! Competitive group (large file only) compares jshift against:
//! * `serde_json` — full DOM parse then index / rebuild
//! * `gjson` — path get on UTF-8 string view
//! * `sonic_rs` — pointer get on bytes
//!
//! ```bash
//! cargo bench --bench project_benchmark
//! cargo bench --bench project_benchmark -- "large compete"
//! ```

use std::path::PathBuf;
use std::time::Duration;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use jshift::{
    project, project_indexed, project_jmespath, project_paths, project_write, projected_len,
    IndexedDocument, ProjectPlan,
};
#[cfg(feature = "parallel")]
use jshift::project_indexed_parallel;
use sonic_rs::{get as sonic_get, pointer, JsonContainerTrait};

/// ~N MiB of catalog-like JSON: `{ "products": [ {id,title,price,tags,variants:[{sku,price}]} ] }`.
fn generate_catalog_mb(target_mb: usize) -> Vec<u8> {
    let target = target_mb.saturating_mul(1024 * 1024);
    let mut out = Vec::with_capacity(target + 1024);
    out.extend_from_slice(br#"{"products":["#);
    let mut i = 0u64;
    while out.len() < target {
        if i > 0 {
            out.push(b',');
        }
        out.extend_from_slice(br#"{"id":"#);
        out.extend_from_slice(i.to_string().as_bytes());
        out.extend_from_slice(br#","title":"Product "#);
        out.extend_from_slice(i.to_string().as_bytes());
        out.extend_from_slice(br#"","price":"#);
        out.extend_from_slice(((i % 9999) as f64 / 100.0).to_string().as_bytes());
        out.extend_from_slice(br#","tags":["a","b","c"],"variants":[{"sku":"S-"#);
        out.extend_from_slice(i.to_string().as_bytes());
        out.extend_from_slice(br#"","price":9.99},{"sku":"M-"#);
        out.extend_from_slice(i.to_string().as_bytes());
        out.extend_from_slice(br#"","price":10.99}]}"#);
        i += 1;
    }
    out.extend_from_slice(br#"]}"#);
    out
}

fn load_optional_large() -> Option<(PathBuf, Vec<u8>)> {
    let path = std::env::var_os("JSHIFT_LARGE_JSON")
        .map(PathBuf::from)
        .or_else(|| {
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/data/large.json");
            p.is_file().then_some(p)
        })
        .or_else(|| {
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("benches/datasets/large.json");
            p.is_file().then_some(p)
        })?;
    let bytes = std::fs::read(&path).ok()?;
    Some((path, bytes))
}

fn product_count(json: &[u8]) -> usize {
    let mut doc = IndexedDocument::empty(json);
    if doc.index_array_str("products").is_ok() {
        if let Some(n) = doc.array_len(&jshift::parse_path("products")) {
            return n;
        }
    }
    25_000
}

fn bench_project_synthetic(c: &mut Criterion) {
    for mb in [10usize, 50, 100] {
        let json = generate_catalog_mb(mb);
        let mut group = c.benchmark_group(format!("project synthetic {mb}MB"));
        group.throughput(Throughput::Bytes(json.len() as u64));
        group.sample_size(10);
        group.measurement_time(Duration::from_secs(if mb >= 100 { 20 } else { 10 }));
        group.warm_up_time(Duration::from_secs(2));

        let paths = ["products[0].id", "products[0].title", "products[0].price"];
        let plan = ProjectPlan::from_paths(&paths).unwrap();
        let jmes = "products[*].{id: id, title: title, price: price}";

        group.bench_function("keep_list_paths", |b| {
            b.iter(|| project_paths(&json, &paths).unwrap())
        });
        group.bench_function("project_plan", |b| {
            b.iter(|| project(&json, &plan).unwrap())
        });
        group.bench_function("project_write", |b| {
            b.iter(|| {
                let mut sink = std::io::sink();
                project_write(&json, &plan, &mut sink).unwrap()
            })
        });
        group.bench_function("projected_len", |b| {
            b.iter(|| projected_len(&json, &plan).unwrap())
        });
        group.bench_function("jmes_multi_select", |b| {
            b.iter(|| project_jmespath(&json, jmes).unwrap())
        });

        let mut doc = IndexedDocument::empty(&json);
        doc.index_structural().unwrap();
        doc.index_array_str("products").unwrap();
        group.bench_function("project_indexed_keep", |b| {
            b.iter(|| project_indexed(&doc, &plan).unwrap())
        });

        group.finish();
    }
}

fn bench_project_large_file(c: &mut Criterion) {
    let Some((path, json)) = load_optional_large() else {
        let mut group = c.benchmark_group("project large file (skipped)");
        group.bench_function("no_large_json", |b| b.iter(|| 0usize));
        group.finish();
        eprintln!(
            "note: place large.json under benches/data/ or set JSHIFT_LARGE_JSON for large-file benches"
        );
        return;
    };

    let mut group = c.benchmark_group("project large file");
    group.throughput(Throughput::Bytes(json.len() as u64));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(20));
    group.warm_up_time(Duration::from_secs(2));

    let plan = ProjectPlan::from_paths(&["products[0].id", "products[0].title"]).unwrap_or_else(
        |_| ProjectPlan::from_jmespath("@").unwrap(),
    );

    group.bench_with_input(
        BenchmarkId::new("project_plan", path.display().to_string()),
        &json,
        |b, json| {
            b.iter(|| project(json, &plan).unwrap())
        },
    );
    group.bench_function("project_write", |b| {
        b.iter(|| {
            let mut sink = std::io::sink();
            project_write(&json, &plan, &mut sink).unwrap()
        })
    });
    group.bench_function("projected_len", |b| {
        b.iter(|| projected_len(&json, &plan).unwrap())
    });
    group.bench_function("jmes:products[0].id", |b| {
        b.iter(|| project_jmespath(&json, "products[0].id").unwrap())
    });
    group.bench_function("jmes:products[*].{id,title}", |b| {
        b.iter(|| {
            project_jmespath(&json, "products[*].{id: id, title: title}").unwrap()
        })
    });

    let mut doc = IndexedDocument::empty(&json);
    let _ = doc.index_structural();
    let _ = doc.index_array_str("products");
    group.bench_function("project_indexed_first", |b| {
        b.iter(|| project_indexed(&doc, &plan).unwrap())
    });

    group.finish();
}

/// Head-to-head on the real large fixture (jshift vs serde_json / gjson / sonic_rs).
fn bench_large_compete(c: &mut Criterion) {
    let Some((path, json)) = load_optional_large() else {
        let mut group = c.benchmark_group("large compete (skipped)");
        group.bench_function("no_large_json", |b| b.iter(|| 0usize));
        group.finish();
        return;
    };

    let json_str = std::str::from_utf8(&json).expect("large.json is UTF-8");
    let n = product_count(&json);
    let mid = n / 2;
    let last = n.saturating_sub(1);

    eprintln!(
        "large compete: path={} size_mib={:.2} products≈{n} mid={mid} last={last}",
        path.display(),
        json.len() as f64 / 1024.0 / 1024.0
    );

    // ── 1) Sparse FIND: first / mid / last product title ─────────────────
    {
        let mut group = c.benchmark_group("large compete find first title");
        group.throughput(Throughput::Bytes(json.len() as u64));
        group.sample_size(20);
        group.measurement_time(Duration::from_secs(15));
        group.warm_up_time(Duration::from_secs(2));

        let jpath = jshift::parse_path("products[0].title");
        let gpath = "products.0.title";
        let sptr = pointer!["products", 0, "title"];

        // Warm expectations
        let expect = jshift::find_value(&json, &jpath).unwrap().to_vec();
        let expect_s = std::str::from_utf8(&expect).unwrap().trim_matches('"');

        group.bench_function("jshift_find", |b| {
            b.iter(|| {
                let v = jshift::find_value(&json, &jpath).unwrap();
                assert_eq!(v, expect.as_slice());
            })
        });
        group.bench_function("gjson", |b| {
            b.iter(|| {
                let v = gjson::get(json_str, gpath);
                assert_eq!(v.str(), expect_s);
            })
        });
        group.bench_function("sonic_rs", |b| {
            b.iter(|| {
                let v = sonic_get(json.as_slice(), &sptr).unwrap();
                assert_eq!(v.as_raw_str().trim_matches('"'), expect_s);
            })
        });
        group.bench_function("serde_json", |b| {
            b.iter(|| {
                let val: serde_json::Value = serde_json::from_slice(&json).unwrap();
                let t = val["products"][0]["title"].as_str().unwrap();
                assert_eq!(t, expect_s);
            })
        });
        group.finish();
    }

    {
        let mut group = c.benchmark_group("large compete find mid title");
        group.throughput(Throughput::Bytes(json.len() as u64));
        group.sample_size(10);
        group.measurement_time(Duration::from_secs(25));
        group.warm_up_time(Duration::from_secs(2));

        let jpath_s = format!("products[{mid}].title");
        let jpath = jshift::parse_path(&jpath_s);
        let gpath = format!("products.{mid}.title");
        let sptr = pointer!["products", mid, "title"];

        let mut doc = IndexedDocument::empty(&json);
        doc.index_array_str("products").unwrap();

        let expect = jshift::find_value(&json, &jpath).unwrap().to_vec();
        let expect_s = std::str::from_utf8(&expect).unwrap().trim_matches('"');

        group.bench_function("jshift_find_scan", |b| {
            b.iter(|| {
                let v = jshift::find_value(&json, &jpath).unwrap();
                assert_eq!(v, expect.as_slice());
            })
        });
        group.bench_function("jshift_find_indexed", |b| {
            b.iter(|| {
                let v = doc.find(&jpath).unwrap();
                assert_eq!(v, expect.as_slice());
            })
        });
        group.bench_function("gjson", |b| {
            b.iter(|| {
                let v = gjson::get(json_str, &gpath);
                assert_eq!(v.str(), expect_s);
            })
        });
        group.bench_function("sonic_rs", |b| {
            b.iter(|| {
                let v = sonic_get(json.as_slice(), &sptr).unwrap();
                assert_eq!(v.as_raw_str().trim_matches('"'), expect_s);
            })
        });
        group.bench_function("serde_json", |b| {
            b.iter(|| {
                let val: serde_json::Value = serde_json::from_slice(&json).unwrap();
                let t = val["products"][mid]["title"].as_str().unwrap();
                assert_eq!(t, expect_s);
            })
        });
        group.finish();
    }

    {
        let mut group = c.benchmark_group("large compete find last title");
        group.throughput(Throughput::Bytes(json.len() as u64));
        group.sample_size(10);
        group.measurement_time(Duration::from_secs(25));
        group.warm_up_time(Duration::from_secs(2));

        let jpath_s = format!("products[{last}].title");
        let jpath = jshift::parse_path(&jpath_s);
        let gpath = format!("products.{last}.title");
        let sptr = pointer!["products", last, "title"];

        let mut doc = IndexedDocument::empty(&json);
        doc.index_array_str("products").unwrap();

        let expect = doc.find(&jpath).unwrap().to_vec();
        let expect_s = std::str::from_utf8(&expect).unwrap().trim_matches('"');

        group.bench_function("jshift_find_scan", |b| {
            b.iter(|| {
                let v = jshift::find_value(&json, &jpath).unwrap();
                assert_eq!(v, expect.as_slice());
            })
        });
        group.bench_function("jshift_find_indexed", |b| {
            b.iter(|| {
                let v = doc.find(&jpath).unwrap();
                assert_eq!(v, expect.as_slice());
            })
        });
        group.bench_function("gjson", |b| {
            b.iter(|| {
                let v = gjson::get(json_str, &gpath);
                assert_eq!(v.str(), expect_s);
            })
        });
        group.bench_function("sonic_rs", |b| {
            b.iter(|| {
                let v = sonic_get(json.as_slice(), &sptr).unwrap();
                assert_eq!(v.as_raw_str().trim_matches('"'), expect_s);
            })
        });
        group.bench_function("serde_json", |b| {
            b.iter(|| {
                let val: serde_json::Value = serde_json::from_slice(&json).unwrap();
                let t = val["products"][last]["title"].as_str().unwrap();
                assert_eq!(t, expect_s);
            })
        });
        group.finish();
    }

    // ── 2) Sparse PROJECT: first product card ────────────────────────────
    {
        let mut group = c.benchmark_group("large compete project first card");
        group.throughput(Throughput::Bytes(json.len() as u64));
        group.sample_size(20);
        group.measurement_time(Duration::from_secs(15));
        group.warm_up_time(Duration::from_secs(2));

        let plan =
            ProjectPlan::from_paths(&["products[0].id", "products[0].title", "products[0].handle"])
                .unwrap();
        let jmes = "products[0].{id: id, title: title, handle: handle}";

        group.bench_function("jshift_keep_list", |b| {
            b.iter(|| project(&json, &plan).unwrap())
        });
        group.bench_function("jshift_jmes", |b| {
            b.iter(|| project_jmespath(&json, jmes).unwrap())
        });
        group.bench_function("jshift_project_write", |b| {
            b.iter(|| {
                let mut sink = std::io::sink();
                project_write(&json, &plan, &mut sink).unwrap()
            })
        });

        // gjson: path gets + manual JSON build (fair "extract fields" analog)
        group.bench_function("gjson_fields_rebuild", |b| {
            b.iter(|| {
                let id = gjson::get(json_str, "products.0.id");
                let title = gjson::get(json_str, "products.0.title");
                let handle = gjson::get(json_str, "products.0.handle");
                // `.json()` is the raw on-wire fragment (quoted strings keep quotes).
                let out = format!(
                    r#"{{"id":{},"title":{},"handle":{}}}"#,
                    id.json(),
                    title.json(),
                    handle.json()
                );
                assert!(out.contains("id"));
                out
            })
        });

        group.bench_function("sonic_rs_fields_rebuild", |b| {
            let p_id = pointer!["products", 0, "id"];
            let p_title = pointer!["products", 0, "title"];
            let p_handle = pointer!["products", 0, "handle"];
            b.iter(|| {
                let id = sonic_get(json.as_slice(), &p_id).unwrap();
                let title = sonic_get(json.as_slice(), &p_title).unwrap();
                let handle = sonic_get(json.as_slice(), &p_handle).unwrap();
                let out = format!(
                    r#"{{"id":{},"title":{},"handle":{}}}"#,
                    id.as_raw_str(),
                    title.as_raw_str(),
                    handle.as_raw_str()
                );
                assert!(out.contains("id"));
                out
            })
        });

        group.bench_function("serde_json_project", |b| {
            b.iter(|| {
                let val: serde_json::Value = serde_json::from_slice(&json).unwrap();
                let p = &val["products"][0];
                let out = serde_json::json!({
                    "id": p["id"].clone(),
                    "title": p["title"].clone(),
                    "handle": p["handle"].clone(),
                });
                serde_json::to_vec(&out).unwrap()
            })
        });
        group.finish();
    }

    // ── 3) Full-catalog PROJECT: listing cards ───────────────────────────
    {
        let mut group = c.benchmark_group("large compete project all cards");
        group.throughput(Throughput::Bytes(json.len() as u64));
        group.sample_size(10);
        group.measurement_time(Duration::from_secs(45));
        group.warm_up_time(Duration::from_secs(3));

        let jmes = "products[*].{id: id, title: title}";

        group.bench_function("jshift_jmes_cards", |b| {
            b.iter(|| project_jmespath(&json, jmes).unwrap())
        });

        group.bench_function("serde_json_map_cards", |b| {
            b.iter(|| {
                let val: serde_json::Value = serde_json::from_slice(&json).unwrap();
                let arr = val["products"].as_array().unwrap();
                let cards: Vec<serde_json::Value> = arr
                    .iter()
                    .map(|p| {
                        serde_json::json!({
                            "id": p["id"].clone(),
                            "title": p["title"].clone(),
                        })
                    })
                    .collect();
                serde_json::to_vec(&cards).unwrap()
            })
        });

        // gjson: iterate products array via gjson array API
        group.bench_function("gjson_each_cards", |b| {
            b.iter(|| {
                let products = gjson::get(json_str, "products");
                let mut out = String::from("[");
                let mut first = true;
                products.each(|_, prod| {
                    if !first {
                        out.push(',');
                    }
                    first = false;
                    let id = prod.get("id");
                    let title = prod.get("title");
                    out.push_str(r#"{"id":"#);
                    out.push_str(id.json());
                    out.push_str(r#","title":"#);
                    out.push_str(title.json());
                    out.push('}');
                    true
                });
                out.push(']');
                assert!(out.len() > 2);
                out
            })
        });

        // sonic_rs: full Value parse then map (DOM-class competitor).
        group.bench_function("sonic_rs_value_map_cards", |b| {
            b.iter(|| {
                let val: sonic_rs::Value = sonic_rs::from_slice(&json).unwrap();
                let products = val["products"].as_array().expect("products array");
                let mut cards = sonic_rs::Array::with_capacity(products.len());
                for p in products.iter() {
                    let mut obj = sonic_rs::Object::new();
                    obj.insert("id", p["id"].clone());
                    obj.insert("title", p["title"].clone());
                    cards.push(sonic_rs::Value::from(obj));
                }
                sonic_rs::to_vec(&sonic_rs::Value::from(cards)).unwrap()
            })
        });
        group.finish();
    }

    // ── 4) Hygiene / shape benches: index build, filter, flatten, first after P0 ─
    {
        let mut group = c.benchmark_group("large hygiene index build");
        group.throughput(Throughput::Bytes(json.len() as u64));
        group.sample_size(10);
        group.measurement_time(Duration::from_secs(25));
        group.warm_up_time(Duration::from_secs(2));

        let plan = ProjectPlan::from_jmespath("products[*].{id: id, title: title}").unwrap();
        group.bench_function("index_for_plan_products_cards", |b| {
            b.iter(|| {
                let mut doc = IndexedDocument::empty(&json);
                doc.index_for_plan(&plan).unwrap();
                doc.indexed_array_count()
            })
        });
        group.bench_function("index_array_str_products", |b| {
            b.iter(|| {
                let mut doc = IndexedDocument::empty(&json);
                doc.index_array_str("products").unwrap();
                doc.array_len(&jshift::parse_path("products")).unwrap_or(0)
            })
        });
        group.finish();
    }

    {
        // Sparse first-element project after P0 short-circuit (keep-list + multi-select).
        let mut group = c.benchmark_group("large hygiene project first after P0");
        group.throughput(Throughput::Bytes(json.len() as u64));
        group.sample_size(20);
        group.measurement_time(Duration::from_secs(12));
        group.warm_up_time(Duration::from_secs(1));

        let keep = ProjectPlan::from_paths(&[
            "products[0].id",
            "products[0].title",
            "products[0].handle",
        ])
        .unwrap();
        let jmes = ProjectPlan::from_jmespath("products[0].{id: id, title: title, handle: handle}")
            .unwrap();

        group.bench_function("jshift_keep_list_products0", |b| {
            b.iter(|| project(&json, &keep).unwrap())
        });
        group.bench_function("jshift_jmes_multi_select_products0", |b| {
            b.iter(|| project(&json, &jmes).unwrap())
        });
        group.finish();
    }

    {
        // Filter: products with a positive id (cheap predicate; still walks the catalog).
        let mut group = c.benchmark_group("large hygiene filter cards");
        group.throughput(Throughput::Bytes(json.len() as u64));
        group.sample_size(10);
        group.measurement_time(Duration::from_secs(40));
        group.warm_up_time(Duration::from_secs(2));

        // Shopify ids are large positive integers; keep cards thin.
        let filter_expr = "products[?id > `0`].{id: id, title: title}";
        let plan = ProjectPlan::from_jmespath(filter_expr).unwrap();
        let mut doc = IndexedDocument::empty(&json);
        doc.index_for_plan(&plan).unwrap();

        group.bench_function("jshift_filter_cards", |b| {
            b.iter(|| project(&json, &plan).unwrap())
        });
        group.bench_function("jshift_filter_cards_indexed", |b| {
            b.iter(|| project_indexed(&doc, &plan).unwrap())
        });
        group.bench_function("serde_json_filter_map", |b| {
            b.iter(|| {
                let val: serde_json::Value = serde_json::from_slice(&json).unwrap();
                let cards: Vec<_> = val["products"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|p| p["id"].as_i64().unwrap_or(0) > 0)
                    .map(|p| {
                        serde_json::json!({
                            "id": p["id"].clone(),
                            "title": p["title"].clone(),
                        })
                    })
                    .collect();
                serde_json::to_vec(&cards).unwrap()
            })
        });
        group.finish();
    }

    {
        // Flatten: all variant prices as one array (heavier structural walk).
        let mut group = c.benchmark_group("large hygiene flatten variant prices");
        group.throughput(Throughput::Bytes(json.len() as u64));
        group.sample_size(10);
        group.measurement_time(Duration::from_secs(45));
        group.warm_up_time(Duration::from_secs(2));

        // products[].variants[].price → flatten one level of arrays of prices
        let flatten_expr = "products[].variants[].price";
        let plan = ProjectPlan::from_jmespath(flatten_expr).unwrap();

        group.bench_function("jshift_flatten_variant_prices", |b| {
            b.iter(|| project(&json, &plan).unwrap())
        });
        group.bench_function("serde_json_flatten_prices", |b| {
            b.iter(|| {
                let val: serde_json::Value = serde_json::from_slice(&json).unwrap();
                let mut prices = Vec::new();
                if let Some(products) = val["products"].as_array() {
                    for p in products {
                        if let Some(vars) = p["variants"].as_array() {
                            for v in vars {
                                prices.push(v["price"].clone());
                            }
                        }
                    }
                }
                serde_json::to_vec(&prices).unwrap()
            })
        });
        group.finish();
    }
}

/// Path to the CPU-heavy parallel fixture (see `examples/gen_heavy_parallel_fixture.rs`).
fn load_heavy_parallel() -> Option<(PathBuf, Vec<u8>)> {
    let path = std::env::var_os("JSHIFT_HEAVY_PARALLEL_JSON")
        .map(PathBuf::from)
        .or_else(|| {
            let p = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("benches/data/heavy_parallel.json");
            p.is_file().then_some(p)
        })?;
    let bytes = std::fs::read(&path).ok()?;
    Some((path, bytes))
}

/// JMESPath designed so **each array element** does non-trivial nested work
/// (filter + length on `scores`, nested field access). Domain-agnostic record shape.
fn heavy_list_expr() -> &'static str {
    "records[*].{id: id, hi: length(scores[?@ > `0.7`]), lo: length(scores[?@ > `0.3`]), n_ev: length(events), w0: events[0].w, a0: attrs.k0}"
}

/// Sequential vs parallel list project on a fixture where per-element expr is CPU-heavy.
///
/// Generate fixture first:
/// ```bash
/// cargo run --example gen_heavy_parallel_fixture --release -- benches/data/heavy_parallel.json
/// cargo bench --features parallel --bench project_benchmark -- "heavy parallel"
/// ```
#[cfg(feature = "parallel")]
fn bench_heavy_parallel_list(c: &mut Criterion) {
    let Some((path, json)) = load_heavy_parallel() else {
        let mut group = c.benchmark_group("heavy parallel list (skipped)");
        group.bench_function("missing_heavy_parallel_json", |b| {
            b.iter(|| {
                eprintln!(
                    "generate with: cargo run --example gen_heavy_parallel_fixture --release -- benches/data/heavy_parallel.json"
                );
                0usize
            })
        });
        group.finish();
        return;
    };

    let expr = heavy_list_expr();
    let plan = ProjectPlan::from_jmespath(expr).expect("heavy expr");
    let mut doc = IndexedDocument::empty(&json);
    let t0 = std::time::Instant::now();
    doc.index_for_plan(&plan).expect("index_for_plan");
    eprintln!(
        "heavy parallel: path={} size_mib={:.2} index_for_plan={:?} expr={expr}",
        path.display(),
        json.len() as f64 / 1024.0 / 1024.0,
        t0.elapsed()
    );

    // Correctness once
    let seq = project_indexed(&doc, &plan).expect("seq");
    let par = project_indexed_parallel(&doc, &plan).expect("par");
    assert_eq!(seq, par, "parallel must match sequential bytes");
    eprintln!("output_bytes={} (parallel==sequential ok)", seq.len());

    let mut group = c.benchmark_group("heavy parallel list project");
    group.throughput(Throughput::Bytes(json.len() as u64));
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(40));
    group.warm_up_time(Duration::from_secs(3));

    group.bench_function("jshift_indexed_sequential", |b| {
        b.iter(|| project_indexed(&doc, &plan).unwrap())
    });
    group.bench_function("jshift_indexed_parallel", |b| {
        b.iter(|| project_indexed_parallel(&doc, &plan).unwrap())
    });

    // Competitors: full parse then per-record work (serde) / path tools less natural for this expr
    group.bench_function("serde_json_manual_heavy", |b| {
        b.iter(|| {
            let val: serde_json::Value = serde_json::from_slice(&json).unwrap();
            let records = val["records"].as_array().unwrap();
            let out: Vec<serde_json::Value> = records
                .iter()
                .map(|r| {
                    let scores = r["scores"].as_array().unwrap();
                    let hi = scores
                        .iter()
                        .filter(|v| v.as_f64().unwrap_or(0.0) > 0.7)
                        .count();
                    let lo = scores
                        .iter()
                        .filter(|v| v.as_f64().unwrap_or(0.0) > 0.3)
                        .count();
                    let n_ev = r["events"].as_array().map(|a| a.len()).unwrap_or(0);
                    let w0 = r["events"][0]["w"].clone();
                    let a0 = r["attrs"]["k0"].clone();
                    serde_json::json!({
                        "id": r["id"].clone(),
                        "hi": hi,
                        "lo": lo,
                        "n_ev": n_ev,
                        "w0": w0,
                        "a0": a0,
                    })
                })
                .collect();
            serde_json::to_vec(&out).unwrap()
        })
    });

    group.finish();
}

#[cfg(not(feature = "parallel"))]
fn bench_heavy_parallel_list(c: &mut Criterion) {
    let mut group = c.benchmark_group("heavy parallel list (enable --features parallel)");
    group.bench_function("skipped", |b| b.iter(|| 0usize));
    group.finish();
    let _ = c;
    let _ = load_heavy_parallel;
    let _ = heavy_list_expr;
}

criterion_group!(
    benches,
    bench_project_synthetic,
    bench_project_large_file,
    bench_large_compete,
    bench_heavy_parallel_list
);
criterion_main!(benches);
