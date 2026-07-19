//! Benchmarks for jshift path scan / mutate.
//!
//! Groups:
//! * **JSON Find/Mutate Value (10MB)** — historical suite: target key *after* a large array
//!   (favors path engines that skip the bulk without full parse). Unchanged.
//! * **Fair Find (key-first 10MB)** — same payload size but `target` is the *first* key so
//!   competitors that scan from the start are not artificially hurt by key placement.
//! * **Fair Find (1KB hot path)** — small document, typical API payload; less I/O noise.
//! * Concurrent reads — unchanged.

use criterion::{criterion_group, criterion_main, Criterion};
use rayon::prelude::*;
use sonic_rs::{get as sonic_get, pointer};

/// ~10MB JSON with `target` **after** a large array (original layout).
fn generate_large_json_target_last() -> Vec<u8> {
    let mut s = String::new();
    s.push_str("{\"data\":[");
    for i in 0..160_000 {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"id\":");
        s.push_str(&i.to_string());
        s.push_str(",\"name\":\"user_");
        s.push_str(&i.to_string());
        s.push_str("\",\"active\":true,\"score\":9.9}");
    }
    s.push_str("],\"target\":123456}");
    s.into_bytes()
}

/// Same bulk, but `target` is the **first** key (fairer vs full parsers / left-to-right scanners).
fn generate_large_json_target_first() -> Vec<u8> {
    let mut s = String::new();
    s.push_str("{\"target\":123456,\"data\":[");
    for i in 0..160_000 {
        if i > 0 {
            s.push(',');
        }
        s.push_str("{\"id\":");
        s.push_str(&i.to_string());
        s.push_str(",\"name\":\"user_");
        s.push_str(&i.to_string());
        s.push_str("\",\"active\":true,\"score\":9.9}");
    }
    s.push_str("]}");
    s.into_bytes()
}

/// Compact ~1KB document (hot path / API-sized).
fn generate_small_json() -> Vec<u8> {
    let mut s = String::from(r#"{"target":123456,"meta":{"ver":1,"env":"prod"},"items":["#);
    for i in 0..20 {
        if i > 0 {
            s.push(',');
        }
        s.push_str(r#"{"id":"#);
        s.push_str(&i.to_string());
        s.push_str(r#","ok":true}"#);
    }
    s.push_str("]}");
    s.into_bytes()
}

// --- original groups (kept) -------------------------------------------------

fn bench_find(c: &mut Criterion) {
    let json = generate_large_json_target_last();
    let path = jshift::parse_path("target");
    let json_str = std::str::from_utf8(&json).unwrap();

    let mut group = c.benchmark_group("JSON Find Value (10MB)");
    group.sample_size(10);

    group.bench_function("jshift", |b| {
        b.iter(|| {
            let res = jshift::find_value(&json, &path).unwrap();
            assert_eq!(res, b"123456");
        })
    });

    group.bench_function("serde_json", |b| {
        b.iter(|| {
            let val: serde_json::Value = serde_json::from_slice(&json).unwrap();
            let res = val.get("target").unwrap();
            assert_eq!(res.as_u64().unwrap(), 123456);
        })
    });

    // Path engines on the same “key last” workload (for reference, not “fair”).
    group.bench_function("gjson", |b| {
        b.iter(|| {
            let v = gjson::get(json_str, "target");
            assert_eq!(v.u64(), 123456);
        })
    });

    group.bench_function("sonic_rs", |b| {
        let ptr = pointer!["target"];
        b.iter(|| {
            let v = sonic_get(json.as_slice(), &ptr).unwrap();
            assert_eq!(v.as_raw_str(), "123456");
        })
    });

    group.finish();
}

fn bench_mutate(c: &mut Criterion) {
    let json = generate_large_json_target_last();
    let path = jshift::parse_path("target");
    let new_val = b"999999";

    let mut group = c.benchmark_group("JSON Mutate Value (10MB)");
    group.sample_size(10);

    group.bench_function("jshift", |b| {
        b.iter_with_setup(
            || json.clone(),
            |mut json_copy| {
                jshift::mutate_value(&mut json_copy, &path, new_val).unwrap();
            },
        )
    });

    group.bench_function("serde_json", |b| {
        b.iter_with_setup(
            || json.clone(),
            |json_copy| {
                let mut val: serde_json::Value = serde_json::from_slice(&json_copy).unwrap();
                val["target"] = serde_json::Value::from(999999);
                let _out = serde_json::to_vec(&val).unwrap();
            },
        )
    });

    group.finish();
}

fn bench_concurrency(c: &mut Criterion) {
    let json = generate_large_json_target_last();
    let path = jshift::parse_path("target");
    let json_str = std::str::from_utf8(&json).unwrap();

    let mut group = c.benchmark_group("JSON Concurrent Reads (10MB key-last)");
    group.sample_size(10);

    // Same model for all: 8 independent workers each extract `target` from the
    // shared buffer (no shared parse tree). serde therefore re-parses 8×.
    group.bench_function("jshift_x8", |b| {
        b.iter(|| {
            (0..8).into_par_iter().for_each(|_| {
                let res = jshift::find_value(&json, &path).unwrap();
                assert_eq!(res, b"123456");
            });
        })
    });

    group.bench_function("gjson_x8", |b| {
        b.iter(|| {
            (0..8).into_par_iter().for_each(|_| {
                assert_eq!(gjson::get(json_str, "target").u64(), 123456);
            });
        })
    });

    group.bench_function("serde_json_x8", |b| {
        b.iter(|| {
            (0..8).into_par_iter().for_each(|_| {
                let val: serde_json::Value = serde_json::from_slice(&json).unwrap();
                assert_eq!(val["target"].as_u64().unwrap(), 123456);
            });
        })
    });

    group.finish();
}

// --- fairer groups ----------------------------------------------------------

fn bench_fair_find_key_first(c: &mut Criterion) {
    let json = generate_large_json_target_first();
    let path = jshift::parse_path("target");
    let json_str = std::str::from_utf8(&json).unwrap();
    let sonic_ptr = pointer!["target"];

    let mut group = c.benchmark_group("Fair Find key-first (10MB)");
    group.sample_size(10);

    group.bench_function("jshift", |b| {
        b.iter(|| {
            let res = jshift::find_value(&json, &path).unwrap();
            assert_eq!(res, b"123456");
        })
    });

    group.bench_function("gjson", |b| {
        b.iter(|| {
            let v = gjson::get(json_str, "target");
            assert_eq!(v.u64(), 123456);
        })
    });

    group.bench_function("sonic_rs", |b| {
        b.iter(|| {
            let v = sonic_get(json.as_slice(), &sonic_ptr).unwrap();
            assert_eq!(v.as_raw_str(), "123456");
        })
    });

    group.bench_function("serde_json", |b| {
        b.iter(|| {
            let val: serde_json::Value = serde_json::from_slice(&json).unwrap();
            assert_eq!(val["target"].as_u64().unwrap(), 123456);
        })
    });

    group.finish();
}

fn bench_fair_find_small(c: &mut Criterion) {
    let json = generate_small_json();
    let path = jshift::parse_path("target");
    let nested = jshift::parse_path("meta.ver");
    let json_str = std::str::from_utf8(&json).unwrap();
    let sonic_target = pointer!["target"];
    let sonic_nested = pointer!["meta", "ver"];

    let mut group = c.benchmark_group("Fair Find small (1KB)");
    group.sample_size(50);

    group.bench_function("jshift_target", |b| {
        b.iter(|| {
            assert_eq!(jshift::find_value(&json, &path).unwrap(), b"123456");
        })
    });
    group.bench_function("gjson_target", |b| {
        b.iter(|| {
            assert_eq!(gjson::get(json_str, "target").u64(), 123456);
        })
    });
    group.bench_function("sonic_rs_target", |b| {
        b.iter(|| {
            assert_eq!(
                sonic_get(json.as_slice(), &sonic_target)
                    .unwrap()
                    .as_raw_str(),
                "123456"
            );
        })
    });
    group.bench_function("serde_json_target", |b| {
        b.iter(|| {
            let val: serde_json::Value = serde_json::from_slice(&json).unwrap();
            assert_eq!(val["target"].as_u64().unwrap(), 123456);
        })
    });

    group.bench_function("jshift_nested", |b| {
        b.iter(|| {
            assert_eq!(jshift::find_value(&json, &nested).unwrap(), b"1");
        })
    });
    group.bench_function("gjson_nested", |b| {
        b.iter(|| {
            assert_eq!(gjson::get(json_str, "meta.ver").u64(), 1);
        })
    });
    group.bench_function("sonic_rs_nested", |b| {
        b.iter(|| {
            assert_eq!(
                sonic_get(json.as_slice(), &sonic_nested)
                    .unwrap()
                    .as_raw_str(),
                "1"
            );
        })
    });

    group.finish();
}

fn bench_fair_mutate_small(c: &mut Criterion) {
    let json = generate_small_json();
    let path = jshift::parse_path("target");

    let mut group = c.benchmark_group("Fair Mutate small (1KB)");
    group.sample_size(50);

    group.bench_function("jshift", |b| {
        b.iter_with_setup(
            || json.clone(),
            |mut buf| {
                jshift::mutate_value(&mut buf, &path, b"999999").unwrap();
            },
        )
    });

    group.bench_function("serde_json", |b| {
        b.iter_with_setup(
            || json.clone(),
            |buf| {
                let mut val: serde_json::Value = serde_json::from_slice(&buf).unwrap();
                val["target"] = serde_json::json!(999999);
                let _ = serde_json::to_vec(&val).unwrap();
            },
        )
    });

    group.finish();
}

/// Head-to-head path-engine showcase (higher sample count for tighter CIs).
fn bench_compete_path_engines(c: &mut Criterion) {
    let last = generate_large_json_target_last();
    let first = generate_large_json_target_first();
    let small = generate_small_json();
    let path = jshift::parse_path("target");
    let nested = jshift::parse_path("meta.ver");
    let last_str = std::str::from_utf8(&last).unwrap();
    let first_str = std::str::from_utf8(&first).unwrap();
    let small_str = std::str::from_utf8(&small).unwrap();
    let sonic_target = pointer!["target"];
    let sonic_nested = pointer!["meta", "ver"];

    // --- key last: must skip ~10MB array ---
    {
        let mut group = c.benchmark_group("Compete Find key-last 10MB");
        group.sample_size(20);
        group.bench_function("jshift", |b| {
            b.iter(|| assert_eq!(jshift::find_value(&last, &path).unwrap(), b"123456"))
        });
        group.bench_function("gjson", |b| {
            b.iter(|| assert_eq!(gjson::get(last_str, "target").u64(), 123456))
        });
        group.bench_function("sonic_rs", |b| {
            b.iter(|| {
                assert_eq!(
                    sonic_get(last.as_slice(), &sonic_target)
                        .unwrap()
                        .as_raw_str(),
                    "123456"
                )
            })
        });
        group.finish();
    }

    // --- key first: early exit ---
    {
        let mut group = c.benchmark_group("Compete Find key-first 10MB");
        group.sample_size(50);
        group.bench_function("jshift", |b| {
            b.iter(|| assert_eq!(jshift::find_value(&first, &path).unwrap(), b"123456"))
        });
        group.bench_function("gjson", |b| {
            b.iter(|| assert_eq!(gjson::get(first_str, "target").u64(), 123456))
        });
        group.bench_function("sonic_rs", |b| {
            b.iter(|| {
                assert_eq!(
                    sonic_get(first.as_slice(), &sonic_target)
                        .unwrap()
                        .as_raw_str(),
                    "123456"
                )
            })
        });
        group.finish();
    }

    // --- small + nested ---
    {
        let mut group = c.benchmark_group("Compete Find small+nested");
        group.sample_size(100);
        group.bench_function("jshift_top", |b| {
            b.iter(|| assert_eq!(jshift::find_value(&small, &path).unwrap(), b"123456"))
        });
        group.bench_function("gjson_top", |b| {
            b.iter(|| assert_eq!(gjson::get(small_str, "target").u64(), 123456))
        });
        group.bench_function("sonic_top", |b| {
            b.iter(|| {
                assert_eq!(
                    sonic_get(small.as_slice(), &sonic_target)
                        .unwrap()
                        .as_raw_str(),
                    "123456"
                )
            })
        });
        group.bench_function("jshift_nested", |b| {
            b.iter(|| assert_eq!(jshift::find_value(&small, &nested).unwrap(), b"1"))
        });
        group.bench_function("gjson_nested", |b| {
            b.iter(|| assert_eq!(gjson::get(small_str, "meta.ver").u64(), 1))
        });
        group.bench_function("sonic_nested", |b| {
            b.iter(|| {
                assert_eq!(
                    sonic_get(small.as_slice(), &sonic_nested)
                        .unwrap()
                        .as_raw_str(),
                    "1"
                )
            })
        });
        group.finish();
    }
}

criterion_group!(
    benches,
    bench_find,
    bench_mutate,
    bench_concurrency,
    bench_fair_find_key_first,
    bench_fair_find_small,
    bench_fair_mutate_small,
    bench_compete_path_engines,
);
criterion_main!(benches);
