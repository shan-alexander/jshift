//! v0.6 collections vs serde_json: NestedView, MapView, collect policies, schema emit.
//!
//! ```sh
//! cargo run --release --example v06_collections_bench
//! ```
//!
//! Fairness:
//! - **Sparse / nested get**: jshift path scan vs full `Value` or typed deserialize.
//! - **Map each**: single-pass span walk vs parse whole object map.
//! - **collect_field / projected**: stream one field or thin cards vs `Vec<Struct>`.
//! - **to_schema_bytes**: closed card emit vs `serde_json::to_vec`.

use std::time::Instant;

use jshift::{
    build_schema_bytes, CollectPolicy, Collected, FromJsonSlice, JsonDoc, JsonView, MapView,
    NestedView, TypedDoc, ViewList,
};

const WARMUP: usize = 3;

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
    } else {
        format!("{:.2} ms", ns / 1_000_000.0)
    }
}

fn ratio(serde_ns: f64, j_ns: f64) -> String {
    if j_ns <= 0.0 {
        "∞".into()
    } else {
        format!("{:.1}×", serde_ns / j_ns)
    }
}

fn row(name: &str, j: f64, s: f64) {
    println!(
        "  {:<48}  jshift {:>12}   serde {:>12}   serde/jshift {:>8}",
        name,
        fmt_time(j),
        fmt_time(s),
        ratio(s, j)
    );
}

fn gen_catalog(n: usize) -> Vec<u8> {
    use jshift::{ArrayBuilder, ObjectBuilder};
    let mut products = ArrayBuilder::with_capacity(n * 64);
    for i in 0..n {
        products = products.object(|o| {
            o.field("id", &(i as u64))
                .field("title", &format!("item_{i}"))
                .field("price", &9.99f64)
                .field("noise", &true)
                .object_field("meta", |m| {
                    m.field("sku", &format!("SKU{i}")).field("rank", &(i as u64 % 100))
                })
        });
    }
    ObjectBuilder::with_capacity(n * 80)
        .field("status", "ok")
        .raw_field("products", &products.finish())
        .object_field("scores", |m| {
            let mut m = m;
            // fixed small map for MapView benches (also embedded large map separately)
            for i in 0..64u64 {
                m = m.field(&format!("k{i}"), &i);
            }
            m
        })
        .finish()
}

fn gen_score_map(n: usize) -> Vec<u8> {
    use jshift::ObjectBuilder;
    let mut b = ObjectBuilder::with_capacity(n * 12);
    for i in 0..n {
        b = b.field(&format!("user_{i}"), &(i as u64));
    }
    b.finish()
}

// Manual cards (no derive dependency for the example binary logic).
struct IdCard {
    id: u64,
}

impl JsonView for IdCard {
    fn read_from(json: &[u8]) -> Result<Self, jshift::Error> {
        let s = jshift::find_value(json, &jshift::parse_path("id"))?;
        Ok(Self {
            id: u64::from_json_slice(s).ok_or(jshift::Error::TypeMismatch {
                expected: "u64",
                found: "bad",
            })?,
        })
    }
    fn write_into(&self, json: &mut Vec<u8>) -> Result<(), jshift::Error> {
        jshift::upsert_at_path(json, &jshift::parse_path("id"), &self.id.to_json_bytes())
    }
    fn project_plan() -> jshift::ProjectPlan {
        jshift::ProjectPlan::from_paths(&["id"]).unwrap()
    }
}

struct MetaCard {
    sku: String,
    rank: u64,
}

impl JsonView for MetaCard {
    fn read_from(json: &[u8]) -> Result<Self, jshift::Error> {
        let sku = String::from_json_slice(jshift::find_value(json, &jshift::parse_path("sku"))?)
            .ok_or(jshift::Error::TypeMismatch {
                expected: "String",
                found: "bad",
            })?;
        let rank = u64::from_json_slice(jshift::find_value(json, &jshift::parse_path("rank"))?)
            .ok_or(jshift::Error::TypeMismatch {
                expected: "u64",
                found: "bad",
            })?;
        Ok(Self { sku, rank })
    }
    fn write_into(&self, json: &mut Vec<u8>) -> Result<(), jshift::Error> {
        jshift::upsert_at_path(json, &jshift::parse_path("sku"), &self.sku.to_json_bytes())?;
        jshift::upsert_at_path(json, &jshift::parse_path("rank"), &self.rank.to_json_bytes())?;
        Ok(())
    }
    fn project_plan() -> jshift::ProjectPlan {
        jshift::ProjectPlan::from_paths(&["sku", "rank"]).unwrap()
    }
    /// Flat keys: one-pass ObjectBuilder (same path derive uses for flat schemas).
    fn to_schema_bytes(&self) -> Result<Vec<u8>, jshift::Error> {
        Ok(jshift::ObjectBuilder::new()
            .field("sku", &self.sku)
            .field("rank", &self.rank)
            .finish())
    }
}

use jshift::ToJsonBytes;

fn main() {
    println!("jshift v0.6 collections vs serde_json\n");

    let n_prod = 5_000;
    let catalog = gen_catalog(n_prod);
    let score_map = gen_score_map(2_000);
    println!(
        "payloads: catalog={} KB ({} products), score_map={} KB (2000 keys)\n",
        catalog.len() / 1024,
        n_prod,
        score_map.len() / 1024
    );

    let doc = TypedDoc::from_slice(&catalog);
    let iters_med = 80;
    let iters_hot = 2_000;

    // ── NestedView: products[0].meta.rank ──
    println!("── NestedView sparse path ──");
    {
        let j = mean_ns(iters_hot, || {
            let nest = NestedView::from_doc(&doc, "products[0].meta").unwrap();
            let r: u64 = nest.get("rank").unwrap();
            std::hint::black_box(r);
        });
        let s = mean_ns(iters_hot, || {
            let v: serde_json::Value = serde_json::from_slice(&catalog).unwrap();
            let r = v["products"][0]["meta"]["rank"].as_u64().unwrap();
            std::hint::black_box(r);
        });
        row("products[0].meta.rank (NestedView vs Value)", j, s);

        let j2 = mean_ns(iters_hot, || {
            let r: u64 = doc.get("products[0].meta.rank").unwrap();
            std::hint::black_box(r);
        });
        row("products[0].meta.rank (TypedDoc.get vs Value)", j2, s);
    }

    // ── MapView ──
    println!("\n── MapView (string-key object) ──");
    {
        let map_doc = TypedDoc::from_slice(&score_map);
        let map = MapView::<u64>::from_object_bytes(map_doc.as_bytes()).unwrap();

        let j_get = mean_ns(iters_hot, || {
            let v = map.get("user_1000").unwrap();
            std::hint::black_box(v);
        });
        let s_get = mean_ns(iters_hot, || {
            let v: serde_json::Value = serde_json::from_slice(&score_map).unwrap();
            let x = v["user_1000"].as_u64().unwrap();
            std::hint::black_box(x);
        });
        row("map get user_1000 (2k keys)", j_get, s_get);

        let j_each = mean_ns(iters_med, || {
            let mut sum = 0u64;
            map.each_str_key(|_k, v| {
                sum = sum.wrapping_add(v);
                Ok(())
            })
            .unwrap();
            std::hint::black_box(sum);
        });
        let s_each = mean_ns(iters_med, || {
            let v: serde_json::Value = serde_json::from_slice(&score_map).unwrap();
            let mut sum = 0u64;
            for (_k, val) in v.as_object().unwrap() {
                sum = sum.wrapping_add(val.as_u64().unwrap());
            }
            std::hint::black_box(sum);
        });
        row("map each sum 2000 keys", j_each, s_each);

        // serde typed HashMap (fairer full load)
        let j_col = mean_ns(iters_med, || {
            let pairs = map.collect_owned().unwrap();
            std::hint::black_box(pairs.len());
        });
        let s_hm = mean_ns(iters_med, || {
            let m: std::collections::HashMap<String, u64> =
                serde_json::from_slice(&score_map).unwrap();
            std::hint::black_box(m.len());
        });
        row("map collect_owned vs HashMap deserialize", j_col, s_hm);
    }

    // ── ViewList collect policies ──
    println!("\n── ViewList collect policies ({} products) ──", n_prod);
    {
        let list = ViewList::<IdCard>::from_doc(&doc, "products").unwrap();

        let j_field = mean_ns(iters_med, || {
            let mut sum = 0u64;
            list.each_field("id", |id: u64| {
                sum = sum.wrapping_add(id);
                Ok(())
            })
            .unwrap();
            std::hint::black_box(sum);
        });
        let s_field = mean_ns(iters_med, || {
            #[derive(serde::Deserialize)]
            struct P {
                id: u64,
            }
            #[derive(serde::Deserialize)]
            struct Root {
                products: Vec<P>,
            }
            let root: Root = serde_json::from_slice(&catalog).unwrap();
            let sum: u64 = root.products.iter().map(|p| p.id).sum();
            std::hint::black_box(sum);
        });
        row("each_field id sum (stream)", j_field, s_field);

        let j_cf = mean_ns(iters_med, || {
            let ids = list.collect_field::<u64>("id").unwrap();
            std::hint::black_box(ids.len());
        });
        row("collect_field id → Vec (vs same serde)", j_cf, s_field);

        let j_own = mean_ns(iters_med.min(40), || {
            let v = list.collect_owned().unwrap();
            std::hint::black_box(v.len());
        });
        let s_own = mean_ns(iters_med.min(40), || {
            #[derive(serde::Deserialize)]
            struct P {
                id: u64,
            }
            #[derive(serde::Deserialize)]
            struct Root {
                products: Vec<P>,
            }
            let root: Root = serde_json::from_slice(&catalog).unwrap();
            std::hint::black_box(root.products.len());
        });
        row("collect_owned IdCard vs serde Vec", j_own, s_own);

        let j_proj = mean_ns(iters_med.min(40), || {
            let cards = list.collect_projected().unwrap();
            std::hint::black_box(cards.len());
        });
        row("collect_projected thin cards vs serde Vec", j_proj, s_own);

        // policy Stream is no-op — just sanity
        assert!(matches!(
            list.collect(CollectPolicy::Stream).unwrap(),
            Collected::Stream
        ));
    }

    // ── Nested meta rank via list + nest ──
    println!("\n── Nested list walk (meta.rank sum) ──");
    {
        let list = doc.view_list::<IdCard>("products").unwrap();
        let j_manual = mean_ns(iters_med, || {
            let mut sum = 0u64;
            list.for_each_raw(|raw| {
                let nest = NestedView::from_span(raw).nest("meta").unwrap();
                sum = sum.wrapping_add(nest.get::<u64>("rank").unwrap());
                Ok(())
            })
            .unwrap();
            std::hint::black_box(sum);
        });
        let j_api = mean_ns(iters_med, || {
            let sum = list.sum_nested_field_u64("meta.rank").unwrap();
            std::hint::black_box(sum);
        });
        let s = mean_ns(iters_med, || {
            #[derive(serde::Deserialize)]
            struct Meta {
                rank: u64,
            }
            #[derive(serde::Deserialize)]
            struct P {
                meta: Meta,
            }
            #[derive(serde::Deserialize)]
            struct Root {
                products: Vec<P>,
            }
            let root: Root = serde_json::from_slice(&catalog).unwrap();
            let sum: u64 = root.products.iter().map(|p| p.meta.rank).sum();
            std::hint::black_box(sum);
        });
        row("products[*].meta.rank (manual nest)", j_manual, s);
        row("products[*].meta.rank (sum_nested_field_u64)", j_api, s);
    }

    // ── IndexedMapView multi-get ──
    println!("\n── IndexedMapView multi-get ──");
    {
        let map_doc = TypedDoc::from_slice(&score_map);
        let map = MapView::<u64>::from_object_bytes(map_doc.as_bytes()).unwrap();
        let j_prep = mean_ns(iters_med, || {
            let idx = map.index().unwrap();
            std::hint::black_box(idx.len());
        });
        let indexed = map.index().unwrap();
        let keys: Vec<String> = (0..64).map(|i| format!("user_{}", i * 30)).collect();
        let j_multi = mean_ns(iters_hot, || {
            let mut sum = 0u64;
            for k in &keys {
                sum = sum.wrapping_add(indexed.get(k).unwrap());
            }
            std::hint::black_box(sum);
        });
        let j_linear = mean_ns(iters_hot, || {
            let mut sum = 0u64;
            for k in &keys {
                sum = sum.wrapping_add(map.get(k).unwrap());
            }
            std::hint::black_box(sum);
        });
        let s = mean_ns(iters_hot, || {
            let m: std::collections::HashMap<String, u64> =
                serde_json::from_slice(&score_map).unwrap();
            let mut sum = 0u64;
            for k in &keys {
                sum = sum.wrapping_add(*m.get(k).unwrap());
            }
            std::hint::black_box(sum);
        });
        println!(
            "  {:<48}  index() prepare {:>12}",
            "(internal) MapView::index",
            fmt_time(j_prep)
        );
        row("64× map get linear", j_linear, s);
        row("64× map get after IndexedMapView", j_multi, s);
        println!(
            "  {:<48}  linear {:>12}   indexed {:>12}   linear/idx {:>8}",
            "(internal) multi-get",
            fmt_time(j_linear),
            fmt_time(j_multi),
            ratio(j_linear, j_multi)
        );
    }

    // ── Schema emit ──
    println!("\n── to_schema_bytes / build_schema_bytes ──");
    {
        let card = MetaCard {
            sku: "SKU42".into(),
            rank: 7,
        };
        let j = mean_ns(iters_hot, || {
            let b = build_schema_bytes(&card).unwrap();
            std::hint::black_box(b.len());
        });
        let s = mean_ns(iters_hot, || {
            #[derive(serde::Serialize)]
            struct M {
                sku: String,
                rank: u64,
            }
            let b = serde_json::to_vec(&M {
                sku: "SKU42".into(),
                rank: 7,
            })
            .unwrap();
            std::hint::black_box(b.len());
        });
        row("schema card emit (2 fields)", j, s);
    }

    // ── Indexed multi-get after ViewList::index ──
    println!("\n── ViewList::index multi-get ──");
    {
        let list = ViewList::<IdCard>::from_doc(&doc, "products").unwrap();
        let indexed = list.index().unwrap();
        let probes: Vec<usize> = (0..64).map(|i| (i * 97 + 13) % n_prod).collect();
        let j = mean_ns(iters_hot, || {
            let mut sum = 0u64;
            for &i in &probes {
                sum = sum.wrapping_add(indexed.get(i).unwrap().id);
            }
            std::hint::black_box(sum);
        });
        let s = mean_ns(iters_hot, || {
            #[derive(serde::Deserialize)]
            struct P {
                id: u64,
            }
            #[derive(serde::Deserialize)]
            struct Root {
                products: Vec<P>,
            }
            let root: Root = serde_json::from_slice(&catalog).unwrap();
            let mut sum = 0u64;
            for &i in &probes {
                sum = sum.wrapping_add(root.products[i].id);
            }
            std::hint::black_box(sum);
        });
        row("64× random get after index (vs full deserialize)", j, s);
    }

    // ── RFC 7396 merge patch + limits + project_view_each + pretty ──
    println!("\n── merge_patch / limits / project_view_each / pretty ──");
    {
        let base = br#"{"a":{"b":1,"c":2},"d":3}"#;
        let patch = br#"{"a":{"b":9,"c":null},"e":4}"#;
        let j_mp = mean_ns(iters_hot, || {
            let mut buf = base.to_vec();
            jshift::merge_patch(&mut buf, patch).unwrap();
            std::hint::black_box(buf.len());
        });
        let s_mp = mean_ns(iters_hot, || {
            let mut v: serde_json::Value = serde_json::from_slice(base).unwrap();
            let p: serde_json::Value = serde_json::from_slice(patch).unwrap();
            // serde has no std merge-patch; simulate via Value object merge-ish
            if let (Some(to), Some(from)) = (v.as_object_mut(), p.as_object()) {
                for (k, pv) in from {
                    if pv.is_null() {
                        to.remove(k);
                    } else if let (Some(tv), true) = (to.get_mut(k), pv.is_object()) {
                        if let (Some(to2), Some(from2)) = (tv.as_object_mut(), pv.as_object()) {
                            for (k2, pv2) in from2 {
                                if pv2.is_null() {
                                    to2.remove(k2);
                                } else {
                                    to2.insert(k2.clone(), pv2.clone());
                                }
                            }
                        }
                    } else {
                        to.insert(k.clone(), pv.clone());
                    }
                }
            }
            let out = serde_json::to_vec(&v).unwrap();
            std::hint::black_box(out.len());
        });
        row("RFC 7396 merge_patch (small)", j_mp, s_mp);

        let j_lim = mean_ns(iters_hot, || {
            jshift::check_document(&catalog, &jshift::Limits::default()).unwrap();
        });
        let s_lim = mean_ns(iters_hot, || {
            let _: serde_json::Value = serde_json::from_slice(&catalog).unwrap();
        });
        row("check_document depth+len vs full parse", j_lim, s_lim);

        let j_pve = mean_ns(iters_med, || {
            let mut sum = 0u64;
            jshift::project_view_each::<IdCard, _>(&catalog, "products", |c| {
                sum = sum.wrapping_add(c.id);
                Ok(())
            })
            .unwrap();
            std::hint::black_box(sum);
        });
        row("project_view_each IdCard sum vs serde Vec", j_pve, {
            mean_ns(iters_med, || {
                #[derive(serde::Deserialize)]
                struct P {
                    id: u64,
                }
                #[derive(serde::Deserialize)]
                struct Root {
                    products: Vec<P>,
                }
                let root: Root = serde_json::from_slice(&catalog).unwrap();
                std::hint::black_box(root.products.iter().map(|p| p.id).sum::<u64>());
            })
        });

        let j_pretty = mean_ns(iters_hot, || {
            let mut w = jshift::JsonWriter::new_object().pretty(2);
            w.field("id", &1u64).unwrap();
            w.field("ok", &true).unwrap();
            let b = w.finish().unwrap();
            std::hint::black_box(b.len());
        });
        let s_pretty = mean_ns(iters_hot, || {
            let v = serde_json::json!({"id": 1, "ok": true});
            let b = serde_json::to_vec_pretty(&v).unwrap();
            std::hint::black_box(b.len());
        });
        row("JsonWriter pretty vs to_vec_pretty", j_pretty, s_pretty);
    }

    println!("\ndone.");
}
