# jshift-derive

Procedural macros for [**jshift**](https://crates.io/crates/jshift): schema-guided, **100% safe Rust** JSON path reading, **in-place** mutation, and field projection on raw byte buffers.

**`jshift` has blazing fast speeds when working with JSON data, +2x to +20000x speeds compared to `serde`** 🚀

> [!IMPORTANT]
> You almost never depend on this crate directly. Depend on **`jshift`** (feature `derive` is **on by default**); the macros are re-exported as `jshift::JsonView` / `jshift::JsonMutatorSchema`.

```toml
[dependencies]
jshift = "0.4"
```

## What it generates

On a struct with `#[json(path = "...")]` (and optional `#[json(jmes = "...")]`):

* **`JsonView`** — `read_from` / `write_into` / `project_bytes` for open partial documents (unmentioned keys stay unread and byte-preserved on write)
* **`read_from_json`**, **`mutator()`** + `set_*` / `append_*` helpers
* **`FIELD_PATHS`**, **`FIELD_JMES`**, **`INDEXED_ARRAY_PATHS`**
* **`project_json`**, **`schema_project_plan`**, index-aware project helpers

```rust
use jshift::{JsonMutatorSchema, JsonView};

#[derive(JsonMutatorSchema)] // or #[derive(JsonView)]
struct ListingCard {
    #[json(path = "id")]
    id: u64,
    #[json(path = "title")]
    title: String,
}

let json = br#"{"id":7,"title":"Hat","images":[1,2,3]}"#;
let card = ListingCard::read_from_json(json).unwrap();
assert_eq!(card.id, 7);
// `images` was never allocated into the struct (open projection)
```

## Why jshift

When you only need a few paths on large or high-volume JSON (gateways, JSONL cleaners, bronze→silver cards), jshift path-scans and mutates the **same** `Vec<u8>` — no full AST allocate / re-serialize cycle. Safe Rust (`forbid(unsafe_code)`). See the main crate for the full story, benchmarks, and JMESPath subset projector:

**[https://crates.io/crates/jshift](https://crates.io/crates/jshift)** · [GitHub](https://github.com/shan-alexander/jshift)

## License

Licensed under either of Apache-2.0 or MIT, at your option (same as **jshift**).
