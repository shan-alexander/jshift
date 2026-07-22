# jshift-derive

Procedural macros for [**jshift**](https://crates.io/crates/jshift): schema-guided, **100% safe Rust** JSON path reading, **in-place** mutation, and field projection on raw byte buffers.

**`jshift` has blazing fast speeds when working with JSON data, +2x to +20000x speeds compared to `serde`** 🚀

> [!IMPORTANT]
> You almost never depend on this crate directly. Depend on **`jshift`** (feature `derive` is **on by default**); the macros are re-exported as `jshift::JsonView` / `jshift::JsonMutatorSchema`.

```toml
[dependencies]
jshift = "0.6"
```

## Default: field name = JSON path

**You do not need `#[json(path = "...")]` on every field.**  
If you omit it, the path is the **Rust field name** (top-level key) in the struct. Only write the fields you care about in the struct, everything else in the JSON is left unread and byte-preserved on write (**open projection**).

### Attributes

| Attribute | Role |
| :--- | :--- |
| `#[json(path = "a.b[0].c")]` | Explicit path (default: field name) |
| `#[json(jmes = "...")]` | JMES multi-select for schema project |
| `#[json(default)]` | On `PathNotFound`, use `Default::default()` (0.5; type must implement `Default`). Prefer `Option<T>` when absent → `None`. |

```rust
use jshift::JsonView;

#[derive(JsonView)]
struct ListingCard {
    id: u64,       // → path "id"
    title: String, // → path "title"
}

let json = br#"{"id":7,"title":"Hat","images":[1,2,3],"variants":[]}"#;
let card = ListingCard::read_from_json(json).unwrap();
assert_eq!(card.id, 7);
assert_eq!(card.title, "Hat");
// `images` / `variants` were never allocated into the struct
```

`JsonMutatorSchema` is the same macro (historical name); prefer **`JsonView`** when thinking in partial records / views.

---

## Mutator surface: `set_*`, `append_*`, `prepend_*`, `insert_*`, `delete_*`

Besides shared view helpers (`read_from_json`, `project_json`, …), the derive’s **unique
field-level mutator API** is intentionally small.

### What you get

| Generated | When | What it does |
| :--- | :--- | :--- |
| **`Type::mutator(&mut buf)`** | always | Returns `{Type}Mutator { json: &mut Vec<u8> }` |
| **`m.set_<field>(val)`** | **every** field | `ToJsonBytes(val)` → `mutate_value` (replace whole value) |
| **`m.delete_<field>()`** | path ends in a **key** | `delete_key` (parent path + last key; removes object member) |
| **`m.append_<field>(val)`** | field type is **`Vec<_>`** | `append_to_array` (push at **end**) |
| **`m.prepend_<field>(val)`** | field type is **`Vec<_>`** | `prepend_to_array` (push at **start**) |
| **`m.insert_<field>(index, val)`** | field type is **`Vec<_>`** | `insert_array_element` (`0` = prepend, `len` = append) |
| **`m.delete_<field>_at(i)`** | field type is **`Vec<_>`** | `delete_index` at `i` |

Naming uses the **Rust field identifier**, not a mangled path:

```text
status: String     →  set_status + delete_status
tags: Vec<String>  →  set/append/prepend/insert_tags + delete_tags + delete_tags_at
tokens: usize      →  set_tokens + delete_tokens  (no array helpers)
```

Path override does **not** rename the method:

```rust
#[json(path = "meta.status")]
status: String,
// still set_status / delete_status; mutates path "meta.status"
```

There are no `get_*` methods on the mutator (read via `read_from_json` / `JsonView`).

### Bench (~50 MiB catalog, ~900k `items`, vs serde)

Fixture shape: `{"status":"ok","tags":["seed"],"items":[…huge…]}` (`items[i].n` alternates
so filter `items[?n]` keeps ~half).  
Open mutator view only names `status` + `tags` (never loads `items`).  
Free splice/delete-mid target the large `items` array.  
Serde = full `from_slice` + edit + `to_vec` (JMES rows: parse + index only;
`project_each` rows: parse + rebuild a thin projected array).  
`project_each` holds **one card** at a time.

Reproduce:

```bash
cargo run --release --example array_insert_bench
# ARRAY_BENCH_MIB=50 ARRAY_BENCH_ITERS=12
```

| Workload | jshift | serde | **jshift vs serde** |
| :--- | ---: | ---: | ---: |
| Free **prepend** on `items` | ~247 ms | ~736 ms | **~3.0×** |
| Free **insert mid** on `items` | ~239 ms | ~728 ms | **~3.0×** |
| Free **append** on `items` | ~274 ms | ~759 ms | **~2.8×** |
| Free **delete mid** on `items` | ~167 ms | ~731 ms | **~4.4×** |
| Mutator **`set_status`** | **~21 ms** | ~886 ms | **~42×** |
| Mutator **`append_tags`** | **~23 ms** | ~727 ms | **~32×** |
| Mutator **`prepend_tags`** | **~19 ms** | ~727 ms | **~38×** |
| Mutator **`insert_tags(1, …)`** | **~19 ms** | ~724 ms | **~39×** |
| Mutator **`delete_status`** | ~72 ms | ~814 ms | **~11×** |
| Mutator **`delete_tags_at(0)`** | **~8.0 ms** | ~722 ms | **~91×** |
| **`project_each`** `items[*].{id,t}` | ~524 ms | ~1.7 s | **~3.2×** |
| **`project_each`** `items[?n].{id,t}` | ~563 ms | ~1.2 s | **~2.1×** |
| **`project_each`** `items[0:1000].{id,t}` | ~132 ms | ~587 ms | **~4.4×** |
| Derive **JMES** read `items[0].id` / `t` | **~3.7 µs** | ~581 ms | **~160 000×** |
| Free **JMES** project `items[0].{id,t}` | **~2.3 µs** | ~581 ms | **~260 000×** |

**How to read it:** splicing / mid-deleting a **huge** `items` array is still multi-hundred-ms
(clone + memmove) but beats a full DOM rebuild by a few×. The derive mutator shines when the
schema is **open**: stamp tags without materializing ~900k products (**~30–40×**);
`delete_tags_at` is especially cheap (**~91×**). `delete_status` is slower (~72 ms) because
`status` sits at the front of the document — removing it still memmoves the rest of the
~50 MiB buffer (no full parse, but a big shift). Streaming **`project_each`** over all /
filtered / sliced cards stays **one-card** peak RAM and is a few× faster than serde’s parse
+ rebuild of a full thin array; a short head slice (`[0:1000]`) is **~4.4×**. Sparse JMES /
first-item project stays **microseconds** vs serde’s full parse.

```rust
#[derive(JsonMutatorSchema)]
struct CatalogMeta {
    status: String,
    tags: Vec<String>,
    // no `items` field → never read the giant array
}

let mut m = CatalogMeta::mutator(&mut buf);
m.set_status("skipped")?;
m.prepend_tags("hot")?;
m.insert_tags(1, "mid")?;
m.append_tags("tail")?;
m.delete_tags_at(0)?; // drop first tag
// m.delete_status()?;  // remove the key entirely
```

---

## JsonView — typed open projection

Use when several call sites share one partial schema, or you want `T: JsonView` in generic code.

```rust
use jshift::{read_view, JsonView};

#[derive(JsonView)]
struct RouteHeader {
    status: String,
    tenant: String,
}

fn ingest<T: JsonView>(buf: &[u8]) -> Result<T, jshift::Error> {
    T::read_from(buf)
}

let body = br#"{"status":"ok","tenant":"acme","products":[{"id":1}]}"#;
let h: RouteHeader = ingest(body).unwrap();
// or: let h = read_view::<RouteHeader>(body).unwrap();
assert_eq!(h.status, "ok");

// Patch only named fields; unmentioned keys stay on the wire
let mut buf = body.to_vec();
let next = RouteHeader {
    status: "accepted".into(),
    tenant: h.tenant,
};
next.write_into(&mut buf).unwrap();
// products / rest of document still present
```

---

## JsonMutatorSchema — read + in-place mutators

Same derive as `JsonView`, with the ergonomic **mutator** surface for gateways and JSONL cleaners.

```rust
use jshift::JsonMutatorSchema;

#[derive(JsonMutatorSchema)]
struct TrainingRecord {
    tokens: usize,
    status: String,
    tags: Vec<String>,
}

let mut line = br#"{"instruction":"Translate…","tokens":1024,"status":"pending","tags":["llm"]}"#.to_vec();

let rec = TrainingRecord::read_from_json(&line).unwrap();
if rec.tokens > 512 {
    let mut m = TrainingRecord::mutator(&mut line);
    m.set_status("skipped").unwrap();
    m.append_tags("oversized").unwrap();
}

let updated = TrainingRecord::read_from_json(&line).unwrap();
assert_eq!(updated.status, "skipped");
assert_eq!(updated.tags, vec!["llm".to_string(), "oversized".to_string()]);
// `instruction` was never re-serialized from a tree — only status/tags were spliced
```

---

## Variations (when attributes help)

### Nested paths and renames

Field name can differ from the on-wire path:

```rust
use jshift::JsonView;

#[derive(JsonView)]
struct Nested {
    #[json(path = "meta.ver")]
    ver: u32,
    #[json(path = "products[0].title")]
    first_title: String,
}

let json = br#"{"meta":{"ver":2},"products":[{"title":"Hat","blob":true}]}"#;
let n = Nested::read_from_json(json).unwrap();
assert_eq!(n.ver, 2);
assert_eq!(n.first_title, "Hat");
```

### Optional fields

`Option<T>`: JSON `null` or missing path → `None` (no error under soft missing).

```rust
use jshift::JsonView;

#[derive(JsonView)]
struct Row {
    id: u64,
    label: Option<String>,
}
```

### JMESPath on a field (`jmes`)

Use when **read/project** should evaluate a JMES expression (e.g. first variant price) while you still keep a path for writes/indexing when needed:

```rust
use jshift::JsonMutatorSchema;

#[derive(JsonMutatorSchema)]
struct Card {
    id: u64,
    #[json(path = "title")]
    title: String,
    /// Read/project from nested JMES; path used for schema/index bookkeeping
    #[json(path = "price", jmes = "variants[0].price")]
    price: String,
}

let json = br#"{
  "id": 1,
  "title": "Hat",
  "variants": [{"price": "9.99"}, {"price": "12.00"}]
}"#;

let c = Card::read_from_json(json).unwrap();
assert_eq!(c.price, "9.99");

// Keep-list / multi-select project driven by FIELD_PATHS + FIELD_JMES
let slim = Card::project_json(json).unwrap();
// slim is a smaller document shaped by the schema (not a full DOM round-trip)
assert!(slim.windows(4).any(|w| w == b"9.99"));
```

### Schema project (thin JSON for the next hop)

```rust
use jshift::JsonView;

#[derive(JsonView)]
struct SilverMeta {
    topic: String,
    record_id: String,
}

let line = br#"{"topic":"egui","messages":[{"role":"user","content":"…huge…"}],"record_id":"abc"}"#;
let out = SilverMeta::project_json(line).unwrap();
// drops fat `messages` — open keep-list style projection
```

### Indexed reads (large arrays)

When field paths cross big arrays, prefer schema-guided indexing:

```rust
use jshift::JsonView;

#[derive(JsonView)]
struct Hit {
    #[json(path = "products[0].title")]
    title: String,
}

// build side-tables from INDEXED_ARRAY_PATHS, then find
let card = Hit::read_from_json_indexed(json).unwrap();
// or: let doc = Hit::prepare(json)?; Hit::from_indexed_document(&doc)?
```

---

## Quick rules

| Goal | Pattern |
| :--- | :--- |
| Top-level key = field name | bare field, no attribute |
| Nested / renamed key | `#[json(path = "a.b[0].c")]` |
| Soft missing | `Option<T>` |
| Nested extract for read/project | `#[json(path = "…", jmes = "…")]` |
| Shared partial type in generics | `T: JsonView` / `read_view` |
| Splice fields in place | `Type::mutator(&mut buf)` → `set_*` |

---

## Why jshift

When you only need a few paths on large or high-volume JSON (gateways, JSONL cleaners, bronze→silver cards), jshift path-scans and mutates the **same** `Vec<u8>` — no full AST allocate / re-serialize cycle. Safe Rust (`forbid(unsafe_code)`). Full story, benches, and JMESPath subset projector:

**[https://crates.io/crates/jshift](https://crates.io/crates/jshift)** · [GitHub](https://github.com/shan-alexander/jshift)

## License

Licensed under either of Apache-2.0 or MIT, at your option (same as **jshift**).
