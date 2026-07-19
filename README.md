# jshift

**jshift** is a schema-guided, **100% safe Rust** JSON path reader and **in-place** mutator.

It is built for high-performance middleboxes, API gateways, webhook routers, edge filters, and data pipelines that need to **selectively query and modify** JSON on raw byte buffers without the full AST allocate / serialize cycle of traditional parsers.

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

### Real-world example: API JSON ingestion (what IT teams do today)

A common pattern in platform / integration teams looks like this:

1. **Pull** a large JSON payload from a partner or SaaS API (catalog, events, orders, transaction dumps... often multi‑MB or hundreds of MB).
2. **Ingest** into an internal service: validate a couple of fields, stamp metadata (`ingested_at`, `source`, `tenant_id`), maybe drop or rewrite a status flag, then forward the body to a queue, object store, or downstream microservice.
3. **Today’s default stack** is almost always: `HTTP body → serde_json::from_slice` (or equivalent) → walk a full in-memory tree → change one or two fields → `to_vec` / re-serialize → publish. That is simple to write and easy to reason about but on a **300 MB json catalog** you still allocate and walk the entire tree, then allocate and write another ~300 MB of output, even if you only needed `products[0].title` and a top-level `status`. Under bursty multi-worker ingestion, that becomes CPU, memory, and GC (or allocator) pressure, not “JSON is slow because HTTP is slow.”
4. **With jshift**, the same job is: keep the body as `Vec<u8>` → path-scan only the fields you need → **splice** stamps or flags in place with safe byte rotations → hand the same buffer downstream. Peers that only need a header field never pay for the giant `products` array. Teams that must stay on **safe Rust** (no `unsafe` hot loops) get selective R/W without becoming a second full parser.

Impact in practice: lower p99 on hot ingestion paths, less memory headroom for concurrent workers, and fewer “we only touch three fields but clone the whole document” incidents while serde remains the right tool when you truly need a full typed domain model.

---

## Key Features

* **Path-selective scans:** Walk only the path you need on raw `&[u8]` / `Vec<u8>`.
* **In-place mutations:** Upsize and downsize with safe slice rotations (compiles to `memmove`-class moves).
* **100% Safe Rust:** `#![forbid(unsafe_code)]`, no `get_unchecked` in the hot path.
* **Zero-copy reads:** `find_value` returns a subslice of the input buffer.
* **`JsonView` trait:** one protocol surface for “this Rust type is a projection of JSON bytes” (`read_from` / `read_from_indexed` / `write_into`), generic pipelines without ad-hoc methods.
* **Schema derive:** `#[derive(JsonView)]` or `JsonMutatorSchema`, typed readers/mutators, `FIELD_PATHS`, schema-guided `INDEXED_ARRAY_PATHS` / `prepare()`.
* **Open projections:** fields you don’t name are **unread** and **byte-preserved** on write (API evolution as a feature).
* **Shared documents:** `SharedDocument` (`Arc<[u8]>`) for cheap clone + many concurrent readers.
* **JSONL helpers:** `json_lines` / `read_jsonl` -- index **per line**, not one giant merge.
* **Projection estimates:** `estimate_projected_len` (size planning before big jobs).
* **Object & array CRUD:** Update, upsert, delete keys; append, index, delete elements; nested `upsert_at_path`.
* **Correct string encoding:** `ToJsonBytes` and key upserts escape `"`, `\`, and control characters.
* **Owned + pointer paths:** `Path`, `try_parse_path`, JSON Pointer (`Path::from_json_pointer`).
* **Option / null:** first-class for training JSONL and partial records.
* **Structural indexes (opt-in):** [`IndexedDocument`] side-tables so mid/last `products[i].field` jumps instead of scanning every sibling.

> **Contract:** jshift is a **non-validating** path engine. It assumes mostly well-formed JSON along the path you traverse. Callers must supply complete JSON value bytes for raw mutations (or use `ToJsonBytes` / `mutate_value_checked`).

### Cargo features

```toml
jshift = "0.4"                              # default: derive on
jshift = { version = "0.4", default-features = false }  # core only
# index-simd is reserved (no-op); indexing APIs are always available, opt-in at call site
```

### Non-goals (deliberate)

| Do | Don’t |
| :--- | :--- |
| Path index + mutate | Full JSON DOM |
| Schema-guided projection | Replace serde for fully typed apps |
| Safe structural tables | Promise simdjson Stage-1 crowns |
| Preserve unmentioned fields | Full RFC validator (unless optional later) |

Clear non-goals keep jshift the **safe path-mutate / field-projection** crate, not a second full parser.

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
* “Partial view struct (`ListingCard { id, title }`) over a fat catalog object” → **`JsonView`**.

---

## How it works under the hood (byte shifts, not magic)

You do not need a systems-programming background to use jshift, but understanding the mechanism explains the speed.

### 1. Find = locate byte offsets, not build a tree

`find_value(json, path)` walks the raw bytes along a path like `user.score` or `tags[0]`:

1. Skip whitespace and nested structures you do not need (including large arrays/objects on the way).
2. Match object keys (as on-wire bytes between quotes) or array indexes.
3. Return `(start, end)` into the original buffer or just the slice `json[start..end]`.

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

**gjson** is an excellent, battle-tested **read** engine. Its hottest skip loops use techniques like continuous bulk scans, and in the Rust port, **unchecked indexing** (`get_unchecked`) in the inner loop. That can buy speed on “skip a huge array to a trailing key.”

**jshift deliberately does not.**

| Choice | jshift | Typical max-speed finder (e.g. gjson hot path) |
| :--- | :--- | :--- |
| `unsafe` | **Forbidden** (`forbid(unsafe_code)`) | Used in hot loops for fewer bounds checks |
| Primary goal | Find **and mutate** with byte offsets | Find / query |
| Failure mode on bad input | `Result` / syntax errors along the path | Often best-effort on malformed JSON |
| Dependency / trust surface | One crate, auditable safe code | Speed via unsafe assumptions |

We **do** absorb a few *ideas* that transfer cleanly to safe Rust:

* bulk “squash” of nested containers,
* tight string skipping,
* unrolled scans over non-structural bytes,

…and left the remaining gap on pure “key-last + 10MB array” finds as a conscious trade for **memory safety and mutator correctness**. If your only requirement is the absolute fastest read and you accept unsafe, evaluate gjson. If you need **safe in-place mutation**, jshift is the fit.

---

## Performance

**Fresh Criterion run** on this repo’s `benches/json_benchmark.rs` (quiet machine, release, ~6 s measurement windows). Means below are the criterion mid estimate. Re-run before putting numbers on a slide deck, the absolute times vary by CPU, but **ratios** are the story.

**Legend**

* **jshift** — path scan / in-place mutate, **`#![forbid(unsafe_code)]`**
* **gjson** — path **read** (hot skip path may use **unchecked** loads)
* **sonic-rs** — high-performance JSON library (pointer get)
* **serde_json** — full `Value` parse (+ re-serialize on mutate)

### Find: path engines + full parse

| Workload | jshift | gjson | sonic-rs | serde_json | **jshift vs serde** | **jshift vs gjson** | **jshift vs sonic** |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **Key-last ~10MB** (target after huge array) | ~11.0 ms | ~4.9 ms | ~27.4 ms | ~223 ms | **~20× faster** | ~2.3× slower | **~2.5× faster** |
| **Key-first ~10MB** (target is first field) | ~39 ns | ~87 ns | ~97 ns | ~222 ms | **~5,700,000× faster** | **~2.2× faster** | **~2.5× faster** |
| **Small ~1KB** top-level key | ~39 ns | ~89 ns | ~96 ns | ~6.1 µs | **~160× faster** | **~2.3× faster** | **~2.5× faster** |
| **Small ~1KB** nested `meta.ver` | ~80 ns | ~143 ns | ~153 ns | ~6.4 µs | **~80× faster** | **~1.8× faster** | **~1.9× faster** |

**Interpretation:** *On selective finds, jshift is typically **~20×–millions×** faster than full `serde_json` parse, and on most shapes here **~2×** faster than gjson/sonic while remaining 100% safe Rust; gjson can still win pure “skip a giant trailing array” finds (~2×).*

The key-first row might seem absurd and that is the point. Serde still parses the entire multi‑megabyte document. jshift matches the first key and stops.

**Path-engine honesty**

* On **key-last + giant array**, **gjson** can win pure find (~2× here). We optimize that path in **safe** code, still beat **sonic-rs (~2.5×)**, and still crush full parse (**~20×** vs serde).
* On **key-first** and **small / nested** finds in this run, **jshift leads** gjson and sonic-rs while remaining safe.
* jshift’s killer feature is not “always #1 find”; it is **find → mutate same buffer**.

### Mutate: the product story

| Workload | jshift | serde_json (parse + set + `to_vec`) | **jshift vs serde** |
| :--- | ---: | ---: | ---: |
| **Key-last ~10MB** (same-length overwrite) | ~11.3 ms | ~198 ms | **~18× faster** |
| **Small ~1KB** | ~76 ns | ~7.3 µs | **~95× faster** |

**Interpretation:** *Selective in-place mutate is where jshift shines, about **~18×** (large) to **~100×** (small) versus “parse whole tree, change one field, re-serialize.”*

This is the “gateway / JSONL cleaner / feature-flag rewrite” workload: change a field, keep shipping the rest of the bytes.

### Concurrent find with 8 independent workers

Same model for every engine: **eight concurrent workers**, each extracts `target` from a **shared** buffer. No shared parse tree. That means **serde re-parses eight times**.

#### Key-last ~10MB (must skip the array)

| Engine | Mean (8-way wall) | **vs serde** | **vs gjson** |
| :--- | ---: | ---: | ---: |
| gjson ×8 | ~10.3 ms | **~43× faster** | 1× |
| jshift ×8 | ~20.1 ms | **~22× faster** | ~2.0× slower |
| serde_json ×8 | ~442 ms | 1× | — |

**Interpretation:** *Under 8-way key-last load, jshift is still **~22×** faster than parallel full parses; gjson leads pure read (~2×) on this shape.*

#### Key-first ~10MB (early exit)

| Engine | Mean (8-way wall) | **vs serde** | **vs gjson** |
| :--- | ---: | ---: | ---: |
| **jshift ×8** | **~18.7 µs** | **~22,000× faster** | **~1.02× (≈tie / slight win)** |
| gjson ×8 | ~19.1 µs | **~21,000× faster** | 1× |
| serde_json ×8 | ~404 ms | 1× | — |

**Interpretation:** *When the hot field is near the front (the common API case) eight workers finish in **~19 µs** with jshift vs serde's **~400 ms** of full re-parses (ie jshift is **~22,000×** faster than serde).*

```bash
# Full suite
cargo bench

# Head-to-head find (jshift / gjson / sonic-rs / serde_json)
cargo bench --bench json_benchmark -- "Compete Find"

# Parallel groups (key-last + key-first)
cargo bench --bench json_benchmark -- "JSON Concurrent"
```

These numbers are not a claim that jshift is always fastest for every JSON task. Serde, gjson, and other crates still have purpose.

---


### Structural indexing, opt-in mid-array / wide-object access

**Indexing is never forced.** Default APIs (`find_value`, `mutate_value`, `read_from_json`, …) do **not** build indexes and pay **no** index tax. You only build metadata when you call `IndexedDocument::build`, `index_array`, `index_object`, `index_structural`, `indexed_document()`, or `read_from_json_indexed()`.

Linear path scans must `skip_value` every sibling before `products[12500]`. That is correct and fine for streaming “touch once” work; it is wrong for **random / multi-query** access into huge arrays. Structural indexing is the lever for the second case.

**jshift 0.3+** adds **safe** structural indexing (not a full simdjson DOM): array side-tables, optional Stage-1 structural lists, object key maps, and derive helpers that *offer* auto-index without changing the default path.

```rust
use jshift::{IndexedDocument, parse_path, find_value};

// Default path.  no index, no build cost:
let _ = find_value(&json, &parse_path("status"));

// Opt-in: pay build once, then many random hops:
let doc = IndexedDocument::build(&json, &["products"])?;
let title = doc.find(&parse_path("products[12500].title"))?;
```

| Phase | Cost class | When you pay |
| :--- | :--- | :--- |
| Default `find_value` / `read_from_json` | Same as always | **Never** builds an index |
| `IndexedDocument::build` / `index_*` | One linear pass over chosen arrays/objects | **Only if you call it** |
| `doc.find(products[i].…)` after index | **O(1)** jump + small local scan | After opt-in build |
| Unindexed `find_value(products[i].…)` | **O(i)** sibling skips | Default |

#### Compete: mid/last array element (50k products, quiet Criterion)

Prebuilt jshift index vs peers on the **same** buffer (index build timed separately).

| Access | jshift **indexed** | jshift linear | gjson | sonic-rs | serde_json | **indexed vs serde** | **indexed vs gjson** |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| **Mid** `products[25000].title` | **~76 ns** | ~1.02 ms | ~974 µs | ~2.42 ms | ~52.9 ms | **~700,000×** | **~13,000×** |
| **Last** `products[49999].title` | **~76 ns** | ~2.06 ms | ~1.98 ms | ~4.81 ms | ~52.7 ms | **~690,000×** | **~26,000×** |
| **First** `products[0].title` | **~68 ns** | ~83 ns | ~202 ns | ~160 ns | ~53.1 ms | **~780,000×** | **~3×** |

**One-liner:** *With an opt-in array index, mid/last element hops are **~10⁴–10⁶×** faster than full parse and **~10³–10⁴×** faster than linear path engines that must walk siblings while default jshift paths stay zero-overhead if you never build an index.*

| Index build (opt-in, once) | Mean |
| :--- | ---: |
| Array side-table `products` only | ~4.3 ms |
| Stage-1 structural + array | ~8.5 ms |

#### Compete: wide object last key (2 000 keys at root)

| Access | jshift **key map** | jshift linear | gjson | sonic-rs | serde_json | **map vs serde** | **map vs gjson** |
| :--- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Last key `k1999` | **~47 ns** | ~52.6 µs | ~49.4 µs | ~98.7 µs | ~554 µs | **~12,000×** | **~1,050×** |
| Build root object map (once) | ~328 µs | — | — | — | — | — | — |

**One-liner:** *Object key maps are also opt-in: pay ~0.3 ms once on a 2k-key object, then last-key lookup at **~47 ns** instead of tens of microseconds of linear scan.*

Indexes bind to a fixed byte snapshot. After in-place mutate/delete, **rebuild** (or drop) the index. Best ETL pattern: **index → many reads / project → write a new buffer → optional reindex**.

This stays `forbid(unsafe_code)`: `Vec<u32>` offsets, `HashMap` key tables, Stage-1 structural lists, and existing safe cursors.

| Layer | API | Helps | Forced? |
| :--- | :--- | :--- | :--- |
| Array side-table | `index_array` / `build` | `products[i].field` mid/last | **No** |
| Object key map | `index_object` | wide roots / hot config | **No** |
| Stage-1 structurals | `index_structural` | faster container skip while building tables | **No** |
| Derive | `indexed_document` / `read_from_json_indexed` | auto-index array prefixes when *you* call it | **No** (`read_from_json` unchanged) |

```bash
cargo bench --bench json_benchmark -- "Indexed"
```


## Installation

```toml
[dependencies]
jshift = "0.3"
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
* **`IndexedDocument`**: array side-tables for O(1) element jumps (`build`, `find`, `for_each_element`)  

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
