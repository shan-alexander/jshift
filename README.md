# jshift

**jshift** is a schema-guided, **100% safe Rust** JSON path reader and **in-place** mutator.

It is built for high-performance middleboxes, API gateways, webhook routers, edge filters, and data pipelines that need to **selectively query and modify** JSON on raw byte buffers—without the full AST allocate / serialize cycle of traditional parsers.

If you only need “read one field,” other path engines exist.  
If you need **read + rewrite the same buffer** under real production constraints (safe Rust, no second serialize pass), **jshift is the product**.

---

## Why jshift exists

Typical stack today:

```text
bytes → serde_json::from_slice → Value tree → change a field → to_vec → bytes
```

That is correct and ergonomic. It is also expensive when:

* documents are large (MBs of payload, one hot field),
* you process millions of JSONL lines,
* you run many concurrent workers on the same shape of traffic,
* you only care about **one path** (or a small schema of paths).

jshift flips the model:

```text
bytes → path scan to byte offsets → splice / shift bytes in place → same Vec<u8>
```

No tree. No second full document serialize. Reads return **zero-copy** slices into the original buffer.

---

## Key Features

* **Path-selective scans:** Walk only the path you need on raw `&[u8]` / `Vec<u8>`.
* **In-place mutations:** Upsize and downsize with safe slice rotations (compiles to `memmove`-class moves).
* **100% Safe Rust:** `#![forbid(unsafe_code)]`—no `get_unchecked` in the hot path.
* **Zero-copy reads:** `find_value` returns a subslice of the input buffer.
* **Schema derive:** `#[derive(JsonMutatorSchema)]` generates typed readers and mutators with compile-time path constants.
* **Object & array CRUD:** Update, upsert, delete keys; append, index, delete elements; nested `upsert_at_path`.
* **Correct string encoding:** `ToJsonBytes` and key upserts escape `"`, `\`, and control characters.
* **Owned + pointer paths:** `Path`, `try_parse_path`, JSON Pointer (`Path::from_json_pointer`).
* **Option / null:** first-class for training JSONL and partial records.

> **Contract:** jshift is a **non-validating** path engine. It assumes mostly well-formed JSON along the path you traverse. Callers must supply complete JSON value bytes for raw mutations (or use `ToJsonBytes` / `mutate_value_checked`).

---

## When to use jshift vs serde_json

| You should use… | When… |
| :--- | :--- |
| **jshift** | You touch **few fields** on large or high-volume JSON; you want **in-place** updates; you control or trust path shape; latency / throughput matter. |
| **serde_json** | You need a **full typed model**, validation of the whole document, arbitrary transforms, or you already pay for a complete parse. |
| **Both** | Parse selectively with jshift for hot paths; use serde when a request actually needs the full document. |

**Rule of thumb**

* “Filter / tag / rewrite `status` on every JSONL line” → **jshift**.
* “Deserialize into `struct Request { … }` with dozens of fields and nested enums” → **serde**.
* “Gateway: inspect `headers.x-request-id`, maybe set `status`, forward body” → **jshift**.

---

## How it works under the hood (byte shifts, not magic)

You do not need a systems-programming background to use jshift—but understanding the mechanism explains the speed.

### 1. Find = locate byte offsets, not build a tree

`find_value(json, path)` walks the raw bytes along a path like `user.score` or `tags[0]`:

1. Skip whitespace and nested structures you do not need (including large arrays/objects on the way).
2. Match object keys (as on-wire bytes between quotes) or array indexes.
3. Return `(start, end)` into the original buffer—or just the slice `json[start..end]`.

So for:

```json
{"data":[ … megabytes … ],"target":123456}
```

a path `"target"` means: skip the entire `data` array as one bulk value, then read the number.  
No heap tree of every object in `data`.

### 2. Mutate = splice into the same `Vec<u8>`

When you replace a value:

| Case | What jshift does |
| :--- | :--- |
| **Same length** (e.g. `123456` → `999999`) | Overwrite bytes in place. |
| **Longer** | `Vec::resize`, then `tail.rotate_right(delta)` to open a gap, write the new value. |
| **Shorter** | `tail.rotate_left(delta)` to close the gap, then `truncate`. |

`rotate_left` / `rotate_right` on a slice are **safe** APIs. LLVM typically lowers them to the same class of bulk memory moves as `memmove`. You get high performance **without** writing `unsafe` pointer arithmetic.

Deletes work the same way: compute a span (including commas), expand over adjacent whitespace for tidy output (“pretty delete”), then shift the tail left.

### 3. Why that is faster than serde for selective work

serde’s happy path is:

1. Parse **every** token into a `Value` (or a typed struct).
2. Mutate the tree.
3. Walk the tree and **write a new document**.

jshift’s happy path is:

1. Scan until the path of interest.
2. Move only the **tail after the edit**.

If the document is 10MB and you change six bytes near the end, serde still rebuilds ~10MB of output. jshift moves a tail and rewrites the field.

### 4. Safe vs “unchecked” path engines (gjson)

**gjson** is an excellent, battle-tested **read** engine. Its hottest skip loops use techniques like continuous bulk scans—and in the Rust port, **unchecked indexing** (`get_unchecked`) in the inner loop. That can buy speed on “skip a huge array to a trailing key.”

**jshift deliberately does not.**

| Choice | jshift | Typical max-speed finder (e.g. gjson hot path) |
| :--- | :--- | :--- |
| `unsafe` | **Forbidden** (`forbid(unsafe_code)`) | Used in hot loops for fewer bounds checks |
| Primary goal | Find **and mutate** with byte offsets | Find / query |
| Failure mode on bad input | `Result` / syntax errors along the path | Often best-effort on malformed JSON |
| Dependency / trust surface | One crate, auditable safe code | Speed via unsafe assumptions |

We **did** steal the *ideas* that transfer cleanly to safe Rust:

* bulk “squash” of nested containers,
* tight string skipping,
* unrolled scans over non-structural bytes,

…and left the remaining gap on pure “key-last + 10MB array” finds as a conscious trade for **memory safety and mutator correctness**. If your only requirement is the absolute fastest read and you accept unsafe, evaluate gjson. If you need **safe in-place mutation**, jshift is the fit.

---

## Performance (illustrative — with marketing multipliers)

Measured with Criterion on this repo’s `benches/json_benchmark.rs` (release build, one Linux host). Numbers are approximate; re-run before putting them on a slide deck.

**Legend**

* **jshift** — path scan / in-place mutate, safe Rust  
* **gjson** — path **read** (hot path may use unchecked loads)  
* **sonic-rs** — high-performance JSON library (pointer get)  
* **serde_json** — full `Value` parse (+ re-serialize on mutate)

### Find: path engines + full parse

| Workload | jshift | gjson | sonic-rs | serde_json | **jshift vs serde** |
| :--- | ---: | ---: | ---: | ---: | ---: |
| **Key-last ~10MB** (target after huge array) | ~10.9 ms | ~5.2 ms | ~27.5 ms | ~220–360 ms | **~20–33× faster** |
| **Key-first ~10MB** (target is first field) | ~37 ns | ~89 ns | ~90 ns | ~220 ms | **~6,000,000× faster** |
| **Small ~1KB** top-level key | ~38 ns | ~88 ns | ~93 ns | ~6 µs | **~160× faster** |
| **Small ~1KB** nested `meta.ver` | ~107 ns | ~145 ns | ~162 ns | ~6 µs | **~55× faster** |

Yes, the key-first row looks absurd—that is the point. Serde still parses the entire multi‑megabyte document. jshift matches the first key and stops.

**Path-engine honesty**

* On **key-last + giant array**, **gjson** can win pure find (~2× here). We care about that benchmark, we optimize for it in **safe** code, and we still beat sonic-rs and destroy full parse.
* On **key-first** and **small / nested** finds in these runs, **jshift leads** gjson and sonic-rs while remaining safe.
* jshift’s killer feature is not “always #1 find”; it is **find → mutate same buffer**.

### Mutate: the product story

| Workload | jshift | serde_json (parse + set + `to_vec`) | **Speedup** |
| :--- | ---: | ---: | ---: |
| **Key-last ~10MB** (same-length overwrite) | ~13 ms | ~200 ms | **~15×** |
| **Small ~1KB** | ~74 ns | ~7.5 µs | **~100×** |

This is the “gateway / JSONL cleaner / feature-flag rewrite” workload: change a field, keep shipping the rest of the bytes.

### Concurrent find — 8 independent workers

Same model for every engine: **eight workers**, each extracts `target` from a **shared** buffer. No shared parse tree. That means **serde re-parses eight times**.

#### Key-last ~10MB (must skip the array)

| Engine | Mean (8-way wall) | **vs serde** |
| :--- | ---: | ---: |
| gjson ×8 | ~10 ms | **~44×** |
| jshift ×8 | ~21 ms | **~21×** |
| serde_json ×8 | ~443 ms | 1× |

#### Key-first ~10MB (early exit)

| Engine | Mean (8-way wall) | **vs serde** |
| :--- | ---: | ---: |
| **jshift ×8** | **~19 µs** | **~23,000×** |
| gjson ×8 | ~19 µs | **~23,000×** |
| serde_json ×8 | ~439 ms | 1× |

When the hot field sits near the front of the document—the common case for many APIs—path engines stay in the **microsecond** regime under parallel load while full parse stays in the **hundreds of milliseconds**.

```bash
# Full suite
cargo bench

# Head-to-head find (jshift / gjson / sonic-rs / serde_json)
cargo bench --bench json_benchmark -- "Compete Find"

# Parallel groups (key-last + key-first)
cargo bench --bench json_benchmark -- "JSON Concurrent"
```

These numbers are not a claim that jshift is always faster for every JSON task. For full typed deserialization and schema validation of entire documents, use serde.

---

## Installation

```toml
[dependencies]
jshift = "0.2"
```

Requires a recent stable Rust (see `rust-version` in `Cargo.toml`; edition 2024).

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

The derive emits **`'static` path constants** (no re-parsing the path string on every `set_*`).

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
use jshift::{find_value, mutate_value, mutate_value_checked, parse_path, Path, ToJsonBytes, upsert_at_path};

let mut json = b"{\"user\": \"farmer\", \"score\": 9.5}".to_vec();
let path = parse_path("score");
// Or reuse an owned path:
// let path = Path::parse("score");

assert_eq!(find_value(&json, &path).unwrap(), b"9.5");
mutate_value(&mut json, &path, b"10.0").unwrap();

// Prefer checked mutate when the payload might be garbage:
mutate_value_checked(&mut json, &path, b"11.0").unwrap();

// Strings are escaped for you:
mutate_value(&mut json, &parse_path("user"), &r#"o'reilly "x""#.to_json_bytes()).unwrap();

// Create nested parents if missing:
let mut empty = b"{}".to_vec();
upsert_at_path(&mut empty, &parse_path("a.b.c"), b"1").unwrap();
```

### Optional fields (`null` / missing)

```rust
#[derive(JsonMutatorSchema)]
struct Row {
    #[json(path = "label")]
    label: Option<String>, // JSON null or missing path → None
}
```

---

## How mutations work (detail)

When a replacement changes size, jshift resizes the `Vec<u8>` and rotates the tail:

```text
Upsize:   [prefix | OLD | tail....]
          resize
          [prefix | OLD | ....tail | pad]
          rotate_right(delta)
          [prefix | gap | tail....]
          write NEW
          [prefix | NEW | tail....]

Downsize: [prefix | OLDVALUE | tail]
          rotate_left(delta)
          [prefix | NEW | tail | garbage]
          truncate
          [prefix | NEW | tail]
```

Deletes:

1. Locate the member (keys use **logical** names; matching uses escaped on-wire form).
2. Expand the delete span to include the correct comma and adjacent whitespace (pretty delete → `{}` / `[]` when empty).
3. Shift the tail left.

---

## Capabilities

### Object operations

* `mutate_value` / `mutate_value_checked` — overwrite at a path  
* `upsert_object_key` — insert or update a key (logical keys, JSON-escaped on write)  
* `upsert_at_path` — upsert a leaf and create missing **object** parents as `{}`  
* `delete_key` — remove a key/value, fix commas, trim whitespace  

### Array operations

* `append_to_array` — append with comma injection  
* `array_len` — count elements without allocating  
* `delete_index` — remove an element, fix commas, pretty-trim  

### Paths

* Dot / bracket: `metadata.tags[0].name` via `parse_path` / `try_parse_path` / `Path`  
* JSON Pointer: `Path::from_json_pointer("/a~1b/0")`  
* Derive validates paths at compile time and caches segment tables  

### Convert

* `FromJsonSlice` / `ToJsonBytes` for numbers, bool, `String`, `Vec`, `Option`  
* `from_json_string` / `escape_json_string` / `escape_json_key`  

---

## Related crates

| Crate | Role | Prefer when… |
| :--- | :--- | :--- |
| **serde_json** | Full AST / typed serde | Complete models, validation, general transforms |
| **gjson** | Fast path **reads** (may use unsafe in hot loops) | Read-only queries; absolute max skip speed |
| **sonic-rs** / simd-json | High-performance parse | Full document parse with speed focus |
| **jshift** | Path scan + **in-place mutate** + schema derive + **safe** | Selective R/W, JSONL, gateways, safe-only codebases |

---

## Development

```bash
cargo test
cargo clippy --workspace --all-targets -- -D warnings
cargo bench --bench json_benchmark -- "Compete Find"
```

See [CONTRIBUTING.md](CONTRIBUTING.md) and [CHANGELOG.md](CHANGELOG.md).

---

## License

Licensed under either of:

* Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
* MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option.
