use criterion::{criterion_group, criterion_main, Criterion};
use rayon::prelude::*;

fn generate_large_json() -> Vec<u8> {
    // Generate a ~10MB JSON string
    // Let's create an object: {"data": [ ... 160,000 objects ... ], "target": 123456}
    // Each object in array is about 60 bytes.
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

fn bench_find(c: &mut Criterion) {
    let json = generate_large_json();
    let path = jshift::parse_path("target");
    
    // Warn: 10MB JSON parsing is slow, let's limit sample size to run in reasonable time
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
    
    group.finish();
}

fn bench_mutate(c: &mut Criterion) {
    let json = generate_large_json();
    let path = jshift::parse_path("target");
    let new_val = b"999999";
    
    let mut group = c.benchmark_group("JSON Mutate Value (10MB)");
    group.sample_size(10);
    
    group.bench_function("jshift", |b| {
        b.iter_with_setup(
            || json.clone(),
            |mut json_copy| {
                jshift::mutate_value(&mut json_copy, &path, new_val).unwrap();
            }
        )
    });
    
    group.bench_function("serde_json", |b| {
        b.iter_with_setup(
            || json.clone(),
            |json_copy| {
                let mut val: serde_json::Value = serde_json::from_slice(&json_copy).unwrap();
                val["target"] = serde_json::Value::from(999999);
                let _out = serde_json::to_vec(&val).unwrap();
            }
        )
    });
    
    group.finish();
}

fn bench_concurrency(c: &mut Criterion) {
    let json = generate_large_json();
    let path = jshift::parse_path("target");
    
    let mut group = c.benchmark_group("JSON Concurrent Reads (10MB)");
    group.sample_size(10);
    
    group.bench_function("jshift_parallel", |b| {
        b.iter(|| {
            // Read target in parallel across 8 workers
            (0..8).into_par_iter().for_each(|_| {
                let res = jshift::find_value(&json, &path).unwrap();
                assert_eq!(res, b"123456");
            });
        })
    });
    
    group.finish();
}

criterion_group!(benches, bench_find, bench_mutate, bench_concurrency);
criterion_main!(benches);
