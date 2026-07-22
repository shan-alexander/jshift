# jshift

**jshift** is a schema-guided, **100% safe Rust** JSON path reader and **in-place** mutator.

![safe Rust](https://img.shields.io/badge/safe-Rust-brightgreen)
![zero-copy](https://img.shields.io/badge/zero-copy-blue)
![raw-bytes](https://img.shields.io/badge/raw-bytes-red)
![in-place](https://img.shields.io/badge/in--place-mutation-blueviolet)
![fast-json](https://img.shields.io/badge/fast-json-orange)

It's built for high-performance; selectively **query** and **modify** JSON on **raw byte buffers** without the full AST allocate / serialize cycle of traditional parsers. In other words:

> `jshift` can be used in place of `serde` for 

> 🚀 **+2x to +20000x speed gains** 🚀

> on common JSON tasks.

`jshift` is ideal for optimizing **data engineering** pipelines, API gateways and request/response transformers, webhook routers, event processors, telemetry, edge filters, and any system that handles JSON.

- If you only need “read a field from a JSON message” other path engines exist, like `gjson`.  
- If you need **read + rewrite the same buffer** under real production constraints (safe Rust, no second serialize pass), **jshift is the product**. 


**A simple analogy:** most JSON libraries ask you to *unpack the whole filing cabinet* to change 1 paper document. jshift is built for the scenario where you already know which cabinet drawer and folder you need, and you want to make your find/edit/extract without unpacking and repacking every paper in every drawer.

Simple mental model:

- **Find**: Extract the value(s) you're looking for, via a slice of the original bytes 
- **Mutate**: Edit the same Vec<u8> of raw bytes without making a copy or reading into memory 
- **Project** (as in field projection): Output new smaller JSON document (or NDJSON lines) efficiently
- **TypedDoc** (0.5+): hold the buffer, `get::<T>(path)`, stream arrays with `each_*`, exclusive `mutate()` — typed fields without `serde_json::Value`

That’s the whole product: peek, patch same buffer, and/or project thinner JSON, without unpacking and reading the whole filing cabinet.

### TypedDoc (typed without Value)

Bring the [`JsonDoc`](https://docs.rs/jshift/latest/jshift/trait.JsonDoc.html) trait into scope for path `get` / `each_*` methods (implemented for `TypedDoc`, `TypedDocRef`, `SharedDocument`, and `Vec<u8>`).

```rust
use jshift::{JsonDoc, TypedDoc};

let mut doc = TypedDoc::from_slice(
    br#"{"status":"ok","items":[{"id":1},{"id":2}],"extra":true}"#,
);

assert_eq!(doc.get::<u64>("items[0].id").unwrap(), 1);
assert_eq!(doc.get_str("status").unwrap(), "ok");

// sparse array field stream — relative path parsed once (beats full-card views)
let ids: Vec<u64> = doc.collect_each_get("items", "id").unwrap();
assert_eq!(ids, vec![1, 2]);

// exclusive mutate: zero-copy borrows cannot coexist (Rust borrow law)
doc.mutate().set("status", "accepted").unwrap();
assert!(doc.contains("extra").unwrap()); // unknowns preserved
```

**Also in 0.5:** `ViewList` / `ValueList` (+ `index()` for O(1) gets), `RootKind` +
`get_nullable`, `RawJson`, **`ObjectBuilder` / `JsonWriter`** (in-place nest, no
`Value`), `TypedDoc::from_view`, and typed JSONL (`jsonl_docs`, `write_jsonl_views`).

---

## Benchmarks on JSON tasks

The bench is designed to provide a fair comparison between jshift, serde, gjson, and sonic-rs.

**JSON files used on each bench task:**

| Scale |  Size |
| :--- |  :--- |
| **Small** | **~500 KiB** |
| **Medium** | **~10 MiB** |
| **Large (find/mutate)** |  **~50 MiB** |
| **Large (catalog)** | **~338 MiB**, ~25k products from a Shopify public API |

---

### Benchmark Task 1: Key-first find

Hot field is the **first** key; a bulk array trails behind.

- **APIs**
  - **jshift:** `find_value(json, &parse_path("target"))`
  - **serde:** `serde_json::from_slice` → `v["target"]`
  - **gjson:** `gjson::get(s, "target")`
  - **sonic-rs:** `sonic_rs::get(json, &pointer!["target"])`
- **Why jshift is fast:** stops at the first key; never materializes the trailing bulk. Serde always builds a full tree of the whole buffer.
- **Ratios (medium):** vs serde **~3 000 000×**; vs gjson **~1.5×**; vs sonic **~2.1×**
- **Timings**

  | Engine | ~500 KiB | ~10 MiB | Large (~50 MiB) |
  | :--- | ---: | ---: | ---: |
  | **jshift** | **~80 ns** | **~80 ns** | **~80 ns** |
  | serde | ~12 ms | ~253 ms | ~856 ms |
  | gjson | ~140 ns | ~120 ns | ~140 ns |
  | sonic | ~190 ns | ~171 ns | ~241 ns |

- **When to use:** default API / gateway shape — “Is `status` ok?” near the root. Common production case; **not** the adversarial key-last row.

#### Code Example of Key-first find

```rust
use jshift::{find_value, parse_path};

let json = br#"{"status":"ok","items":[1,2,3,4,5]}"#;

// jshift makes `v` a zero-copy slice into `json`:
let v = find_value(json, &parse_path("status")).unwrap();

assert_eq!(v, br#""ok""#);
```
> The jshift concept: stop at status; never build a tree of items (ie what `serde` does).

---

### Benchmark Task 2: Key-last find

Hot field sits **after** a giant array (adversarial skip).

- **APIs**
  - **jshift:** `find_value(…, "target")` after bulk `data`
  - **serde:** full `from_slice` + index
  - **gjson:** `gjson::get(s, "target")`
  - **sonic-rs:** pointer get `"target"`
- **Why:** everyone must skip the bulk. gjson’s unsafe-hot skip can win pure find; jshift stays safe and multi-× vs full parse.
- **Ratios (medium):** vs serde **~10×**; vs gjson **~0.38×** *(gjson faster)*; vs sonic **~2.4×**
- **Timings**

  | Engine | ~500 KiB | ~10 MiB | Large (~50 MiB) |
  | :--- | ---: | ---: | ---: |
  | **jshift** | ~655 µs | ~13.5 ms | ~66 ms |
  | serde | ~6.3 ms | ~140 ms | ~717 ms |
  | **gjson** | ~259 µs | ~5.2 ms | ~26 ms |
  | sonic | ~1.6 ms | ~32 ms | ~152 ms |

- **When to use:** the key you're looking up is structured to come after other large fields, especially when you still need **safe mutate / project** on the same buffer (jshift's forte), not only a get (gjson's forte).

#### Code Example of Key-last find

```rust
use jshift::{find_value, parse_path};

let json = br#"{"items":[0,1,2,3,4,5,6,7,8,9],"target":123456}"#;

let v = find_value(json, &parse_path("target")).unwrap();
assert_eq!(v, b"123456");
```
> The jshift concept: this is still a path scan, ie skip the array as one value, then read target. Cost is higher when the bulk is huge. This is one of the few scenario's in which `gjson` is more performant, using unsafe rust to achieve the speed, but `jshift` is still extremely performant.

---

### Benchmark Task 3: 🏆 In-place mutate 🏆

Same-length overwrite of one field.

This is the mutator feature that jshift provides, which no other rust crate offers.

- **APIs**
  - **jshift:** `mutate_value(&mut buf, &path, b"654321")`
  - **serde:** `from_slice` → set → `to_vec` (new document)
  - **gjson:** — *no in-place mutate*
  - **sonic-rs:** — *no jshift-style splice mutate*
- **Why:** jshift splices bytes in the same `Vec<u8>`. Serde rebuilds a full document. Peers are not mutators.
- **Ratios (medium):** vs serde **~9.3×**; gjson/sonic **N/A**
- **Timings**

  | Engine | ~500 KiB | ~10 MiB | Large (~50 MiB) |
  | :--- | ---: | ---: | ---: |
  | **jshift** | **~1.3 ms** | **~28 ms** | **~139 ms** |
  | serde | ~7.4 ms | ~257 ms | ~1.0 s |
  | gjson | — | — | — |
  | sonic | — | — | — |

- **When to use:** gateways, JSONL cleaners, feature flags — change `status`, keep shipping the rest of the bytes. Prefer `mutate_value_checked` for untrusted payloads.

#### Code Example of In-place Mutate

```rust
use jshift::{find_value, mutate_value, parse_path};

let mut json = br#"{"status":"new","id":7}"#.to_vec();

mutate_value(&mut json, &parse_path("status"), br#""accepted""#).unwrap();

assert_eq!(&json[..], br#"{"status":"accepted","id":7}"#);
// output document (same Vec): {"status":"accepted","id":7}

assert_eq!(find_value(&json, &parse_path("id")).unwrap(), b"7");
```

> The jshift concept: splice bytes in place. (Same-length edits are the cheapest case; longer/shorter values still work via tail rotate.)

---

### Benchmark Task 4: Sparse find first array element

`products[0].title` without walking the catalog.

- **APIs**
  - **jshift:** `find_value(…, "products[0].title")`
  - **serde:** parse → `v["products"][0]["title"]`
  - **gjson:** `products.0.title`
  - **sonic-rs:** `pointer!["products", 0, "title"]`
- **Why:** early exit into the first element; no catalog DOM. Path engines stay ns–µs; full parse pays for every product.
- **Ratios (medium):** vs serde **~1 300 000×**; vs gjson **~2.5×**; vs sonic **~1.9×**
- **Timings**

  | Engine | ~500 KiB | ~10 MiB | Large (338 MiB catalog) |
  | :--- | ---: | ---: | ---: |
  | **jshift** | **~120 ns** | **~120 ns** | **~150 ns** |
  | serde | ~6.3 ms | ~153 ms | ~3.4 s |
  | gjson | ~301 ns | ~301 ns | ~3.8 µs |
  | sonic | ~231 ns | ~231 ns | ~270 ns |

- **When to use:** sample one record from a huge export, health checks, “show me the first listing.” On huge real dumps serde is multi-second; jshift stays in nanoseconds.


#### Code Example of Sparse find first array element

```rust
use jshift::{find_value, parse_path};

let json = br#"{
  "products": [
    {"id":1,"title":"Hat","noise":true},
    {"id":2,"title":"Mug","noise":false}
  ]
}"#;

// find products[0].title without caring about the rest of the catalog.
let v = find_value(json, &parse_path("products[0].title")).unwrap();
assert_eq!(v, br#""Hat""#);
```

> The jshift concept: walk only to the first product’s title; leave later products unread.

---

### Benchmark Task 5: Sparse find mid element (linear)

`products[mid].title` **without** an index (sibling skip).

- **APIs**
  - **jshift:** `find_value(…, "products[N].title")` scan
  - **serde:** full parse + index
  - **gjson:** `products.N.title`
  - **sonic-rs:** parse or pointer walk
- **Why:** linear sibling skip is O(N). Correct for one-shot streaming; painful for random access. gjson is a strong pure scanner.
- **Ratios (medium):** vs serde **~26×**; vs gjson **~0.59×** *(gjson faster)*; vs sonic **~3.5×**
- **Timings**

  | Engine | ~500 KiB | ~10 MiB | Large (338 MiB) |
  | :--- | ---: | ---: | ---: |
  | jshift | ~292 µs | ~6.0 ms | ~178 ms |
  | serde | ~7.0 ms | ~154 ms | ~3.4 s |
  | **gjson** | **~173 µs** | **~3.6 ms** | **~109 ms** |
  | sonic | ~986 µs | ~21 ms | ~469 ms |

- **When to use:** rare one-off mid hits only. If you query mid/last often, pay the upfront cost for the **indexed** entry which provides blazing fast speeds after the initial index creation (examples provided lower in the doc).

#### Code Example of Sparse find mid element (linear)

```rust
use jshift::{find_value, parse_path};

let json = br#"{
  "products": [
    {"id":0,"title":"A"},
    {"id":1,"title":"B"},
    {"id":2,"title":"C"}
  ]
}"#;

let v = find_value(json, &parse_path("products[2].title")).unwrap();
assert_eq!(v, br#""C""#);
```

> The jshift concept: fine for a one-off, however each mid/last hit re-skips earlier siblings, therefore use the index approach instead of this approach if you'll make several calls to find a mid-placed element. 

---

### Benchmark Task 6: Sparse find mid element (🏆 indexed 🏆)

Opt-in array side-table (`IndexedDocument`).

- **APIs**
  - **jshift:** `doc.index_array_str("products")?; doc.find(&parse_path("products[N].title"))?`
  - **serde / gjson / sonic:** no jshift-style side-table — still full parse or full scan each time
- **Why:** side-table bookmarks every element start (table of contents). Lookup is O(1) jump + tiny local scan. Index build is opt-in **(~0.7 to 0.9 s once on 338 MiB)**.
- **Ratios (medium):** vs serde **~1 500 000×**; vs gjson **~36 000×**; vs sonic **~210 000×** *(peers re-scan/parse)*
- **Timings** (peers = same as linear row above)

  | Engine | ~500 KiB | ~10 MiB | Large (338 MiB) |
  | :--- | ---: | ---: | ---: |
  | **jshift indexed** | **~100 ns** | **~100 ns** | **~110 ns** |
  | serde | ~7.0 ms | ~154 ms | ~3.4 s |
  | gjson | ~173 µs | ~3.6 ms | ~109 ms |
  | sonic | ~986 µs | ~21 ms | ~469 ms |

- **When to use:** random access / multi-query on one snapshot. Build once (`IndexedDocument` / `index_for_plan`), then many finds/projects. Indexes go stale after in-place mutate.

#### Code Example of Sparse find mid element (indexed)

```rust
use jshift::{IndexedDocument, parse_path};

let json = br#"{
  "products": [
    {"id":0,"title":"A"},
    {"id":1,"title":"B"},
    {"id":2,"title":"C"}
  ]
}"#;

let mut doc = IndexedDocument::empty(json);
doc.index_array_str("products").unwrap(); // pay once

let v = doc.find(&parse_path("products[2].title")).unwrap();
assert_eq!(v, br#""C""#);
```

> The jshift concept: index = “table of contents” for array starts; then mid/last is a jump, not a marathon to skip each sibling.

---

### Benchmark Task 7: Sparse project first card

Keep-list / schema card for `products[0]` (`id` / `title` / `handle`).

- **APIs**
  - **jshift:** `project(json, &ProjectPlan::from_paths(&["products[0].id", "products[0].title", "products[0].handle"])?)?`
  - **serde:** parse → build map → `to_vec`
  - **gjson:** 3× get + string rebuild
  - **sonic-rs:** 3× pointer + string rebuild
- **Why:** jshift emits a **schema-shaped** mini document without parsing the catalog (P0 open-ended descent). sonic/gjson win “hand me three fragments”; jshift wins “hand me JSON I can forward.”
- **Ratios (medium):** vs serde **~140 000×**; vs gjson **~0.85×** *(gjson slightly faster on synthetic medium)*; vs sonic **~0.59×** *(sonic faster on pure fragments)*
- **Timings**

  | Engine | ~500 KiB | ~10 MiB | Large (338 MiB) |
  | :--- | ---: | ---: | ---: |
  | jshift | ~1.1 µs | ~1.1 µs | **~7.7 µs** |
  | serde | ~6.4 ms | ~156 ms | ~3.2 s |
  | gjson | ~962 ns | ~962 ns | ~11.7 µs |
  | **sonic** | **~691 ns** | **~672 ns** | **~991 ns** |

- **When to use:** admin sample cards, support tools. Prefer jshift when output must be valid nested JSON / keep-list schema; prefer sonic when you only need raw field strings.

#### Code Example of Sparse project first card

```rust
use jshift::{project, ProjectPlan};

let json = br#"{
  "products": [
    {"id":1,"title":"Hat","handle":"hat","blob":[1,2,3]},
    {"id":2,"title":"Mug","handle":"mug","blob":[]}
  ]
}"#;

let plan = ProjectPlan::from_paths(&[
    "products[0].id",
    "products[0].title",
    "products[0].handle",
]).unwrap();

let out = project(json, &plan).unwrap();
assert_eq!(
    out,
    br#"{"products":{"id":1,"title":"Hat","handle":"hat"}}"#
);
// output:
// {"products":{"id":1,"title":"Hat","handle":"hat"}}
```

> The jshift concept: schema-shaped card you can forward; blob never enters the output. (JMES `products[0].{id: id, title: title, handle: handle}` yields a flat object instead of nesting under products—different shape, same “first card” job.)

---

### Benchmark Task 8: Full-catalog thin cards

When you want a thinned full-catalog of any array of objects, for example removing all but 2 keys from the full catalog → `{id, title}`.

- **APIs**
  - **jshift:** `index_for_plan` + `project_indexed` / `project_object_fields("products", &["id","title"])`
  - **serde:** parse all → map each → `to_vec`
  - **gjson:** each product + rebuild array string
  - **sonic-rs:** `Value` map + serialize
- **Why:** one-pass multi-select + streaming list emit + array side-table. Avoids a full DOM of every product. Medium synthetic is dense/small (scanner-friendly); **large real catalog** is where jshift’s stack pulls ahead clearly.
- **Ratios:** medium synthetic vs serde **~6.3×**, vs gjson **~0.73×**, vs sonic **~3.0×**; **338 MiB real** vs serde **~63×**, vs gjson **~8.8×**, vs sonic **~10×**
- **Timings**

  | Engine | ~500 KiB | ~10 MiB | Large (338 MiB) |
  | :--- | ---: | ---: | ---: |
  | jshift | ~1.6 ms | ~32 ms | **~50 ms** |
  | serde | ~8.3 ms | ~203 ms | ~3.2 s |
  | gjson | **~1.2 ms** | **~24 ms** | ~444 ms |
  | sonic | ~2.9 ms | ~95 ms | ~503 ms |

- **When to use:** domain-agnostic thin cards (any array path). For NDJSON / one-card-at-a-time use `project_jsonl_write` / `project_each`. Use `project_indexed_auto` when plans may be CPU-heavy.


#### Code Example of Full-catalog thin cards

The standard example:
```rust
use jshift::project_object_fields;

let json = br#"{
  "products": [
    {"id":1,"title":"Hat","price":9.99,"images":[1,2]},
    {"id":2,"title":"Mug","price":5.00,"images":[]}
  ]
}"#;

let out = project_object_fields(json, "products", &["id", "title"]).unwrap();
assert_eq!(
    out,
    br#"[{"id":1,"title":"Hat"},{"id":2,"title":"Mug"}]"#
);
// output:
// [{"id":1,"title":"Hat"},{"id":2,"title":"Mug"}]
```

JMES example with optional index:
```rust
use jshift::{project_indexed, IndexedDocument, ProjectPlan};

let plan = ProjectPlan::from_jmespath("products[*].{id: id, title: title}").unwrap();
let mut doc = IndexedDocument::empty(json);
doc.index_for_plan(&plan).unwrap();

let out = project_indexed(&doc, &plan).unwrap();
// same: [{"id":1,"title":"Hat"},{"id":2,"title":"Mug"}]
```

> The jshift concept: one small card per element; fat fields stay in the source and never get a DOM.

---

### A few more shapes

**JSONL lines**
```rust
use jshift::{find_value, json_lines, parse_path, project_paths};

let buf = br#"{"topic":"rust neural networks","messages":[{"role":"user","content":"…huge…"}],"record_id":"abc"}
"#;

for line in json_lines(buf) {
    let topic = find_value(line, &parse_path("topic")).unwrap();
    // topic == "rust neural networks" (with quotes in the slice)

    let all_topic_records = project_paths(line, &["topic", "record_id"]).unwrap();
    // all_topic_records: {"topic":"rust neural networks","record_id":"abc"}
}
```

> The jshift concept: You've got gigabytes, maybe terabytes, of JSONL training data for LoRA tuning your favorite open-source LLM. You don’t want to fully deserialize every record just to keep a couple of fields, filter by topic, or rewrite a system prompt. jshift lets you do path-based reads and mutations directly on the raw bytes at blazing speeds and with minimal allocation, so your data prep finishes faster and your time goes toward actual training instead of waiting on the data pipeline.

**NDJSON stream of cards (without a giant [...] in memory)**

```rust
use jshift::{project_object_fields_jsonl_write, ProjectPlan};
// or project_jsonl_write with a list-projection plan

let json = br#"{
  "messages": [
    {"id":1,"title":"How to debug borrow checker","difficulty":9,"related_topics":["rust","rust lifetimes",...], ...},
    {"id":2,"title":"Best crates for JSON","difficulty":3,"related_topics":["jshift","serde",...], ...}
  ]
}"#;

let mut out = Vec::new();
project_object_fields_jsonl_write(json, "messages", &["id", "title"], &mut out).unwrap();
// out as text:
// [{"id":1,"title":"How to debug borrow checker"}, {"id":2,"title":"Best crates for JSON"}, ...]
```

> The jshift concept: Efficiently output a new smaller JSON document or NDJSON lines. 

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
* **JSONL helpers:** `json_lines` / `read_jsonl` -- index **per line**, not one giant merge; array→NDJSON cards via `project_jsonl_write` / `project_object_fields_jsonl_write`.
* **Field projection:** `project` / `project_paths` / `project_jmespath` / `project_write` / `project_indexed` / `ProjectPlan` (keep-list + **byte-oriented** JMESPath subset on raw spans, not a DOM port of jmespath.rs; Compact / PreserveSource / Pretty).
* **Streaming cards:** `project_each` / `project_object_fields_each` — one callback per list element (no giant output array); peak RAM ≈ one card.
* **Parallel auto-pick:** `plan_prefers_parallel` + `project_indexed_auto` / `project_parallel_auto` (Rayon only when the plan is likely CPU-bound; thin cards stay sequential).
* **Projection estimates:** `estimate_projected_len` / `projected_len` (planning vs exact).
* **Transforms:** `Transform` / `TransformPipeline` (KeepPaths, Jmes, Rename, Drop, Inject, Style).
* **Derive JMES:** `#[json(jmes = "...")]` + `FIELD_JMES` multi-select project plan.
* **Object & array CRUD:** Update, upsert, delete keys; append, index, delete elements; nested `upsert_at_path`.
* **Correct string encoding:** `ToJsonBytes` and key upserts escape `"`, `\`, and control characters.
* **Owned + pointer paths:** `Path`, `try_parse_path`, JSON Pointer (`Path::from_json_pointer`).
* **Option / null:** first-class for training JSONL and partial records.
* **Structural indexes (opt-in):** [`IndexedDocument`] side-tables so mid/last `products[i].field` jumps instead of scanning every sibling.

**In other words, the feature set is three jobs that share one engine:**

| Job | Everyday phrase | Typical API |
| :--- | :--- | :--- |
| **Peek** | “What’s at this path?” | `find_value`, `JsonView::read_from` |
| **Patch** | “Change this field, keep the rest of the bytes.” | `mutate_value`, derive mutator |
| **Projection** | “Emit a smaller JSON with only the fields I need.” | `project`, `project_object_fields`, JMES |

> jshift is a **non-validating** path engine. It assumes mostly well-formed JSON along the path you traverse. Callers must supply complete JSON value bytes for raw mutations (or use `ToJsonBytes` / `mutate_value_checked`).

---

## Why jshift exists

Typical stack today:

> bytes → serde_json::from_slice → Value tree → change a field → to_vec → bytes

That is correct and ergonomic. It is also expensive when:

* documents are large (MBs of payload, one hot field),
* you process millions of JSONL lines,
* you run many concurrent workers on the same shape of traffic,
* you only care about **one path** (or a small schema of paths).

**In other words:** the cost is not “JSON is text.” The cost is *materializing a second representation of everything* so you can touch a tiny slice of it. That is the right default for application models; it is the wrong default for gateways, cleaners, and data pipeline projections.

jshift flips the model:

> bytes → path scan to byte offsets → splice / shift bytes in place → same Vec<u8>

No tree. No second full document serialize. Reads return **zero-copy** slices into the original buffer.

**Analogy:** think of a paper book versus photocopying the entire library to highlight one sentence. Serde often photocopies (parse → tree → re-print). jshift walks the aisle, opens the right volume, and edits the margin... still carefully, still with bounds and `Result`s, but without reprinting the shelves.

### Real-world example: API JSON ingestion (what IT teams do today)

A common pattern in platform / integration teams looks like this:

1. **Pull** a large JSON payload from a partner or SaaS API (catalog, events, orders, transaction dumps... often multi‑MB or hundreds of MB).
2. **Ingest** into an internal service: validate a couple of fields, stamp metadata (`ingested_at`, `source`, `status`), maybe drop or rewrite a status flag, then forward the body to a queue, object store, or downstream microservice.
3. **Today’s default stack** is almost always: `HTTP body → serde_json::from_slice` (or equivalent) → walk a full in-memory tree → change one or two fields → `to_vec` / re-serialize → publish. That is simple to write and easy to reason about but on a **300 MB json catalog** you still allocate and walk the entire tree, then allocate and write another ~300 MB of output, even if you only needed `products[0].title` and a top-level `status`. Under bursty multi-worker ingestion, that becomes CPU, memory, and GC (or allocator) pressure, not “JSON is slow because HTTP is slow.”
4. **With jshift**, the same job is: keep the body as `Vec<u8>` → path-scan only the fields you need → **splice** stamps or flags in place with safe byte rotations → hand the same buffer downstream. Peers that only need a header field never pay for the giant `products` array. Teams that must stay on **safe Rust** (no `unsafe` hot loops) get selective R/W without becoming a second full parser.

**When this is impactful:** multi-worker ingestion of multi‑MB bodies, JSONL cleaners at millions of lines/day, feature-flag or status rewrites in a gateway, bronze-to-silver “keep ten fields, drop the rest.” **When it is not:** a request handler that already deserializes into a rich domain struct and never ships the raw bytes again --> this ought to stay on serde.

---

## When to use jshift vs serde_json

| You should use… | When… |
| :--- | :--- |
| **jshift** | You touch **few fields** on large or high-volume JSON; you want **in-place** updates; you control or trust path shape; latency / throughput matter. |
| **serde_json** | You need a **full typed model**, validation of the whole document, arbitrary transforms, or you already pay for a complete parse. |
| **Both** | Parse selectively with jshift for hot paths; use serde when a request actually needs the full document. |

**Rule of thumb**

* “Filter, add tags, or rewrite `status` on every JSONL line” → **jshift**.
* “Deserialize into `struct Request { … }` with dozens of fields and nested enums” → **serde**.
* “Gateway: inspect `headers.x-request-id`, maybe set `status`, forward body” → **jshift**.
* “Partial view struct (`ListingCard { id, title }`) over a fat catalog object” → **`JsonView`**.

**In other words:** choose by *how much of the document becomes your domain model*. If the answer is “almost all of it, with types and invariants,” serde wins. If the answer is “a few paths, then forward or slim the bytes,” jshift wins. Many production systems can benefit from both: jshift on the wire edge, serde inside the application core.

---

## How it works under the hood (byte shifts, not magic)

You do not need a systems-programming background to use jshift, but understanding the mechanism explains the speed — and when *not* to expect miracles.

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

**In other words:** skipping is not “ignoring data forever”; it is *refusing to build furniture for rooms you will never enter*. The bytes stay in the buffer; they just never become `Value::Object` nodes.

### 2. Mutate = splice into the same `Vec<u8>`

When you replace a value:

| Case | What jshift does |
| :--- | :--- |
| **Same length** (e.g. `123456` → `999999`) | Overwrite bytes in place. |
| **Longer** | `Vec::resize`, then `tail.rotate_right(delta)` to open a gap, write the new value. |
| **Shorter** | `tail.rotate_left(delta)` to close the gap, then `truncate`. |

`rotate_left` / `rotate_right` on a slice are **safe** APIs. LLVM typically lowers them to the same class of bulk memory moves as `memmove`. You get high performance **without** writing `unsafe` pointer arithmetic.

Deletes work the same way: compute a span (including commas), expand over adjacent whitespace for tidy output (“pretty delete”), then shift the tail left.

**In other words:** the “hard” part of editing a JSON string in place is not finding the field — it is *keeping commas and braces honest* when the replacement changes length. jshift’s job is that bookkeeping, with the tail of the document treated like a sliding window of bytes.

### 3. Why that is faster than serde for selective work

serde’s mutation happy path is:

1. Parse **every** token into a `Value` (or a typed struct).
2. Mutate the tree.
3. Walk the tree and **write a new document**.

jshift’s mutation happy path is:

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

**In other words:** speed without safety is a different product. jshift optimizes *under the constraint* `forbid(unsafe_code)` because many teams cannot take “we `get_unchecked` in a skip loop” into a security review — even when that loop is carefully written.

---

## Capabilities (full API map)

Public surface as of **0.5.0** (`#![forbid(unsafe_code)]`). Convenience map for agentic workflows and humans reading crates.io / GitHub.

### Typed documents (0.5 — center of gravity)

| API | Role |
| :--- | :--- |
| `TypedDoc` / `TypedDocRef` | Owned / borrowed buffer; exclusive `mutate()` |
| `JsonDoc` | Shared read trait: `get` / `get_opt` / `get_nullable` / `get_str` / `contains` / `presence` / `view_at` / `each_*` / `each_get` / … |
| `TypedMutator` | Exclusive splice: `set` / `upsert` / `delete` / `rename_key` / `merge_shallow` / `apply_ops` |
| `ViewList` / `ValueList` | Array cursors (`len` / `get` / `iter` / `collect_owned`) |
| `IndexedViewList` / `IndexedValueList` | `list.index()` → O(1) `get(i)` after one walk |
| `ArrayElems` / `ObjectEntries` / `ObjectEntry` | Raw element / DynObject key-value cursors |
| `RootKind` / `Presence` | Root type + missing vs null vs value |
| `RawJson` | Dynamic pocket (owned subtree bytes) |
| `from_jshift_bytes` / `project_as_view` | View decode; project keep-list then decode as `T` |
| `TypedDoc::from_view` | Emit from `JsonView` onto `{}` |

### Build without `Value` (0.5)

| API | Role |
| :--- | :--- |
| `ObjectBuilder` / `ArrayBuilder` | Fluent emit; **in-place** nesting (single buffer) |
| `JsonWriter` | Imperative encoder (`key` / `value` / `begin_*` / auto-close `finish`) |
| `object_from_iter` / `array_from_iter` / `build_object` / `build_array` | Helpers |

### Batch mutate & validate (0.5)

| API | Role |
| :--- | :--- |
| `MutateOp` / `BatchPlan` / `apply_ops` | Ordered multi-op plans (set/upsert/delete/rename/merge) |
| `rename_key` / `merge_object_shallow` | Key rename; shallow object merge |
| `require_paths` / `require_paths_non_null` | Required fields present |
| `deny_unknown_keys` / `validate_open` / `validate_closed` | Open vs closed (no DOM) |
| `type_at` / `require_type` / `JsonType` | Span type sniff |

### Find (zero-copy path scan)

| API | Role |
| :--- | :--- |
| `find_value` | Locate a path → `&[u8]` subslice into the buffer |
| `parse_path` / `try_parse_path` | Dot/bracket paths → `PathSegment`s |
| `Path` / `Path::parse` / `Path::from_json_pointer` | Owned paths; JSON Pointer support |
| `OwnedPathSegment` / `PathSegment` | Path AST types |

### Mutate (in-place on `Vec<u8>`)

| API | Role |
| :--- | :--- |
| `mutate_value` | Overwrite value at path (caller supplies JSON bytes) |
| `mutate_value_checked` | Same + validates the replacement span |
| `upsert_object_key` | Insert or update a key under an object path |
| `upsert_at_path` | Upsert leaf; create missing **object** parents as `{}` |
| `delete_key` | Remove key/value; fix commas / whitespace. Early keys **memmove the buffer tail** — prefer trailing keys / sparse open-view edits on large docs |
| `append_to_array` | Append element with comma injection |
| `prepend_to_array` | Insert at index `0` |
| `insert_array_element` | Insert at `index` (`0` = prepend, `len` = append) |
| `delete_index` | Remove array element; fix commas. Mid/front deletes shift the **array tail** |
| `array_len` | Count array elements without allocating a `Vec` of them |
| `rename_key` / `merge_object_shallow` | See batch/validate section |

### Project (emit smaller JSON)

| API | Role |
| :--- | :--- |
| `project` / `project_into` | Run a `ProjectPlan` → new buffer |
| `project_paths` | Keep-list convenience |
| `project_jmespath` | JMESPath **subset** on raw spans (not a DOM port) |
| `project_write` | Stream projected document into any `Write` |
| `projected_len` / `estimate_projected_len` / `estimate_values_len` | Exact / ballpark sizes |
| `ProjectPlan` / `ProjectStyle` / `MissingPolicy` | Compact / Pretty / PreserveSource; missing policy |
| `SelectExpr` / `ArraySelect` / `ObjectSelect` / `HashField` / `CmpOp` | Selection AST |
| `parse_jmespath_expr` / `parse_project_path` / `select_from_project_path` | Parsers |
| `WriteSink` / `CountingSink` | Emit sinks |
| `Transform` / `TransformPipeline` | KeepPaths, Jmes, Rename, Drop, Inject, Style |

### Thin cards & streaming (no giant output array)

| API | Role |
| :--- | :--- |
| `project_object_fields` / `plan_object_fields` | Array path + field list → card array (any schema) |
| `project_each` / `project_each_indexed` | Callback per list-projection element: `[*]`, **filter** `[?…]`, and **slice**; one-card peak RAM |
| `project_object_fields_each` / `_indexed` | Thin-field each-callback |
| `project_jsonl_write` / `project_jsonl_write_indexed` | NDJSON lines to `Write` (inherits filter/slice peel from `project_each`) |
| `project_object_fields_jsonl_write` | Thin-field NDJSON |

### Index-wired project & parallel

| API | Role |
| :--- | :--- |
| `project_indexed` | Project with prebuilt `IndexedDocument` |
| `project_indexed_prepare` / `project_auto_indexed` | Index plan paths then project |
| `plan_prefers_parallel` | Heuristic: heavy filters/calls vs thin pure fields |
| `project_indexed_auto` / `project_parallel_auto` | Seq vs Rayon by heuristic |
| `project_parallel` / `project_indexed_parallel` / `project_object_fields_parallel` | Feature **`parallel`** (Rayon) |

### Structural indexes (opt-in)

| API | Role |
| :--- | :--- |
| `IndexedDocument` | Side-tables for arrays/objects; `find`, `for_each_element`, `index_for_plan` |
| `index_array` / `index_array_str` / `index_object` / `index_structural` | Build tables (via `IndexedDocument` methods) |
| `build_array_index` / `build_object_key_index` / `build_structural_index` | Free functions |
| `ArrayIndex` / `ObjectKeyIndex` / `StructuralIndex` | Table types |
| `static_array_prefixes_from_path` | Derive helper for schema array prefixes |

**Never forced** on default `find_value` / `project` / `read_from_json`.

### JSONL / multi-document

| API | Role |
| :--- | :--- |
| `json_lines` / `JsonLines` | Zero-copy iterator over non-empty lines |
| `jsonl_docs` / `jsonl_docs_owned` | `TypedDocRef` / `TypedDoc` per line |
| `read_jsonl` | Map each line through `JsonView` |
| `read_jsonl_indexed` / `read_line_indexed` | Per-line index + view |
| `write_jsonl_line` / `write_jsonl_views` / `write_jsonl_docs` | NDJSON out without `Value` |
| `for_each_jsonl_line` / `for_each_jsonl_doc` | Stream callbacks |

### Views, documents, derive

| API | Role |
| :--- | :--- |
| `JsonView` | Trait: `read_from` / `read_from_indexed` / `write_into` / project helpers |
| `read_view` / `write_view` / `from_jshift_bytes` / `project_as_view` | Free helpers |
| `SharedDocument` | `Arc<[u8]>` shared buffer; `typed_ref` / `to_typed_doc` |
| `#[derive(JsonView)]` / `JsonMutatorSchema` | Feature **`derive`** (default): `FIELD_PATHS`, mutators (`set_*`, array insert/delete), `project_json`, `#[json(jmes)]`, **`#[json(default)]`** (0.5) |
| `Error` | Path / syntax / JMES / type / `Decode` / `MissingField` / `UnknownField` (`non_exhaustive`) |

### Convert / escape

| API | Role |
| :--- | :--- |
| `FromJsonSlice` / `ToJsonBytes` | Numbers, bool, `String`, `Vec`, `Option`, …; `write_json_bytes` (stack itoa for integers) |
| `from_json_string` / `escape_json_string` / `escape_json_key` | String helpers |
| `write_json_string` / `write_json_string_content` | Append escaped strings into a buffer |

### Cargo features

| Feature | Default | Enables |
| :--- | :---: | :--- |
| `derive` | yes | Proc-macro schemas |
| `parallel` | no | Rayon list project APIs |
| `dhat-heap` | no | Allocator profiling example only |

---

## Related crates

| Crate | Role | Prefer when… |
| :--- | :--- | :--- |
| **serde_json** | Full AST / typed serde | Complete models, validation, general transforms |
| **gjson** | Fast path **reads** (may use unsafe in hot loops) | Read-only queries; absolute max skip speed |
| **sonic-rs** / simd-json | High-performance parse | Full document parse with speed focus |
| **jshift** | Path scan + **in-place mutate** + schema derive + **safe** | Selective R/W, JSONL, gateways, safe-only codebases |

**In other words:** these crates are not enemies on a single leaderboard — they are tools for different jobs. Serde owns the typed application core. gjson/sonic own pure read races (sometimes with different safety trade-offs). jshift owns **safe selective read+write and projection** on raw buffers. Mature systems often use more than one.

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
