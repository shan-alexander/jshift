# jshift

**jshift** is a schema-guided, 100% safe Rust JSON path reader and **in-place** mutator.

It is built for high-performance middleboxes, API gateways, webhook routers, and data pipelines that need to selectively query and modify JSON documents without the full AST allocate / serialize cycle of traditional parsers.

## Key Features

* **Path-selective scans:** Read and write targeted fields on raw bytes without deserializing the whole document.
* **100% Safe Rust:** `#![forbid(unsafe_code)]`. Resizes use optimized safe slice rotations (compiles down to `memmove`-class moves).
* **Zero-copy reads:** `find_value` returns a slice into the original buffer.
* **Type-safe derive macros:** `#[derive(JsonMutatorSchema)]` generates readers and in-place mutators.
* **Object & array CRUD:** Update, upsert, delete keys; append, index, and delete array elements.
* **Correct string encoding:** `ToJsonBytes` and key upserts escape `"`, `\`, and control characters.

> **Contract:** jshift is a **non-validating** path engine. It assumes mostly well-formed JSON along the path you traverse. Callers must supply complete JSON value bytes for raw mutations (or use `ToJsonBytes` for primitives/strings).

---

## Performance (illustrative)

Measured with Criterion on this repo’s `benches/json_benchmark.rs` (release, one Linux host).
Means are approximate; re-run on your machine before publishing claims.

Path engines: **jshift** (safe Rust), **gjson**, **sonic-rs**.  
Full parse baseline: **serde_json** (`Value` parse + field access; mutate = parse + set + re-serialize).

### Compete Find — path engines + serde

| Workload | jshift | gjson | sonic-rs | serde_json |
| :--- | ---: | ---: | ---: | ---: |
| **Key-last ~10MB** (target after huge array) | ~10.9 ms | ~5.2 ms | ~27.5 ms | ~220–360 ms |
| **Key-first ~10MB** (target is first field) | ~37 ns | ~89 ns | ~90 ns | ~220 ms |
| **Small ~1KB** top-level key | ~38 ns | ~88 ns | ~93 ns | ~6 µs |
| **Small ~1KB** nested `meta.ver` | ~107 ns | ~145 ns | ~162 ns | ~6 µs |

**How to read this**

* **vs serde_json:** path scans avoid building an AST. On large docs the gap is often **10–1000×+** depending on key placement; on small docs still **~100×** for a single field.
* **Key position matters:** with the key first, jshift/gjson/sonic finish in **nanoseconds** while serde still pays a full ~10MB parse (**milliseconds**).
* **Key-last large array:** gjson can lead on pure find (~2× here). jshift stays ahead of sonic-rs and far ahead of serde; jshift’s niche is **in-place mutate**, not being a pure finder.
* jshift is **`#![forbid(unsafe_code)]`**; gjson’s hot skip path uses unchecked loads.

### Mutate (jshift’s product story)

| Workload | jshift | serde_json (parse + set + `to_vec`) |
| :--- | ---: | ---: |
| **Key-last ~10MB** (same-length overwrite) | ~13 ms | ~200 ms |
| **Small ~1KB** | ~74 ns | ~7.5 µs |

### Concurrent find (8 workers, key-last 10MB)

Each worker independently extracts `target` from the same buffer (serde re-parses per worker):

| Engine | Mean (8-way wall) |
| :--- | ---: |
| gjson ×8 | ~9.6 ms |
| jshift ×8 | ~21 ms |
| serde_json ×8 | ~460 ms |

```bash
# Full suite
cargo bench

# Head-to-head find groups (includes serde)
cargo bench --bench json_benchmark -- "Compete Find"
```

These are not a claim that jshift is always fastest for every JSON task. For full typed deserialization, use serde.

---

## Installation

```toml
[dependencies]
jshift = "0.2"
```

---

## Quick Start: AI dataset cleaning (JSONL)

In pipelines that filter or tag training samples, records are often JSON Lines. Instead of parse → allocate → re-serialize millions of lines, update fields in-place on raw bytes.

### 1. Define a schema

```rust
use jshift::JsonMutatorSchema;

#[derive(JsonMutatorSchema)]
struct TrainingRecord {
    #[json(path = "tokens")]
    tokens: usize,
    #[json(path = "status")]
    status: String,
    #[json(path = "tags")]
    tags: Vec<String>,
}
```

### 2. Selective reads

```rust
let mut line = b"{\"instruction\": \"Translate...\", \"tokens\": 1024, \"status\": \"pending\", \"tags\": [\"llm\"]}".to_vec();

let record = TrainingRecord::read_from_json(&line).unwrap();
assert_eq!(record.tokens, 1024);
assert_eq!(record.tags[0], "llm");
```

### 3. In-place mutations & array append

```rust
let mut mutator = TrainingRecord::mutator(&mut line);

if record.tokens > 512 {
    mutator.set_status("skipped").unwrap();
    mutator.append_tags("oversized").unwrap();
}

let updated = TrainingRecord::read_from_json(&line).unwrap();
assert_eq!(updated.status, "skipped");
assert_eq!(updated.tags, vec!["llm".to_string(), "oversized".to_string()]);
```

### Low-level API

```rust
use jshift::{find_value, mutate_value, parse_path, ToJsonBytes};

let mut json = b"{\"user\": \"farmer\", \"score\": 9.5}".to_vec();
let path = parse_path("score");

assert_eq!(find_value(&json, &path).unwrap(), b"9.5");
mutate_value(&mut json, &path, b"10.0").unwrap();
assert_eq!(json, b"{\"user\": \"farmer\", \"score\": 10.0}");

// Strings are escaped for you:
mutate_value(&mut json, &parse_path("user"), &r#"o'reilly "x""#.to_json_bytes()).unwrap();
```

---

## How mutations work

When a replacement changes size, jshift resizes the `Vec<u8>` and rotates the tail:

**Upsize:** resize → `tail.rotate_right(delta)` → write new value  
**Downsize:** `tail.rotate_left(delta)` → truncate

---

## Capabilities

### Object operations

* `mutate_value` — overwrite a value at a path  
* `upsert_object_key` — insert or update a key (keys are JSON-escaped)  
* `delete_key` — remove a key/value and fix commas  

### Array operations

* `append_to_array` — append with comma injection  
* `array_len` — count elements without allocating an array  
* `delete_index` — remove an element and fix commas  

### Paths

Dot and bracket paths, e.g. `metadata.tags[0].name`.

---

## Related crates

| Crate | Role |
| :--- | :--- |
| `serde_json` | Full AST / typed serde |
| `gjson` | Fast path **reads** |
| `simd-json` / `sonic-rs` | High-performance parsers |
| **jshift** | Path scan + **in-place buffer mutate** + schema derive |

---

## License

Licensed under either of:

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
* MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
