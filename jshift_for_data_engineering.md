# jshift for Data Engineers

## The Problem Statement

Modern data pipelines frequently ingest large vendor JSON payloads — financial backtesting results, enterprise invoice batches, high-volume event logs, e‑commerce catalog dumps (for example Shopify-style `products.json` pages), and similar documents that routinely reach tens to hundreds of megabytes (sometimes larger). These payloads are almost always noisy: they contain dozens or hundreds of fields that will never be used downstream. In a typical medallion architecture those fields are dropped during bronze → silver or silver → gold transforms.

When these payloads are ingested with the standard Rust stack (`serde` + `serde_json`), two expensive things happen:

**Full materialization** — the entire JSON document is scanned, tokenized, and turned into owned Rust values (`String`, `Vec`, maps, nested structs, or a full `serde_json::Value` tree). Even if you only need one or two fields, a full deserialize still allocates heap memory for every key and every value that the chosen type (or `Value`) materializes.

**Peak memory amplification** — when several such payloads are held in memory at once (common when batching API responses before writing to S3, a Hive-style partition, or an Iceberg table), resident set size (RSS) can climb into the multi-gigabyte range. This forces higher cloud instance sizes, triggers autoscaling, or leads teams into complex and error-prone chunking strategies.

A concrete illustration of the *selective find* contrast (not a claim that every workload looks like this): on synthetic multi‑megabyte JSON, a full `serde_json` parse can sit in the hundreds of milliseconds while a path-only find that stops after the hot key finishes in tens of nanoseconds to low milliseconds depending on key position. The CPU and memory cost of full parse is paid even when almost all fields will be discarded. Absolute numbers always depend on CPU, document shape, and Criterion noise; re-run benches on your hardware before putting figures on a slide deck.

## What jshift Changes

jshift attacks the problem at the **byte level** rather than the **value tree** level. It is a schema-guided, **100% safe Rust** (`#![forbid(unsafe_code)]`) path engine for raw `&[u8]` / `Vec<u8>` buffers. In practice data engineers use it in three complementary ways:

1. **Zero-copy path reads** — locate only the keys/paths that are required and return slices into the existing buffer (no full AST).
2. **In-place mutations** — surgically update, upsert, or delete values by shifting bytes inside the existing `Vec<u8>` so the document stays valid JSON without a second full serialize of a tree.
3. **Field projection** — build a **new, smaller** JSON document that keeps only a path keep-list or a JMESPath-style selection (`project` / `project_paths` / `project_jmespath`), copying raw on-wire leaf spans for kept values.

Because the heavy lifting is done by scanning and moving bytes (or writing a compact projected buffer) instead of allocating and later freeing thousands of heap objects for unused fields, peak memory stays close to the size of the working buffers you actually need. The cleaned or projected document can then be handed to a conventional deserializer, written directly to object storage, or converted into a more efficient binary format.

**What jshift is not:** it is not a full JSON DOM, not a replacement for `serde` when you need a complete typed domain model, and not a full RFC validator unless you add validation yourself. It assumes mostly well-formed JSON along the paths you traverse (a non-validating path engine by design). Unmentioned fields are left unread on projection/read and can be preserved byte-for-byte on in-place `write_into` / mutate workflows (“open document” semantics).

The net effect for data engineers is straightforward:

- Dramatically lower RSS when processing large vendor dumps *if* you only need a small schema of fields.
- Reduced risk of OOM kills and the associated operational noise on hot ingestion paths.
- Lower cloud spend because instances no longer need to be oversized for transient full-tree allocation spikes.
- Simpler pipeline code — the “drop the noise” step can be a path keep-list or a JMESPath projection instead of careful multi-stage “parse everything then drop.”
- Optional structural indexes (`IndexedDocument`) when you must hop many times into large arrays (`products[i].…`) after a one-time opt-in build — default finds never pay that tax.

## Projection: keep-lists, `project()`, and JMESPath

Ingestion teams often need more than “mutate one status field.” They need to **reduce** a catalog page or event blob to a slim card before silver. jshift’s projector is that step.

### Path keep-lists (`project_paths` / `ProjectPlan::from_paths`)

You list jshift paths (including array wildcards and slices). Ancestor keys are **preserved** in the output shape:

```text
// input:  {"products":[{"id":1,"body_html":"…huge…","title":"A"}, …]}
// paths:  products[].id, products[].title
// output: {"products":[{"id":1,"title":"A"}, …]}
```

This matches the mental model “still a products document, just thinner.”

### JMESPath-style selection (`project_jmespath` / `ProjectPlan::from_jmespath`)

For listing cards, renames, filters, and function transforms, jshift compiles a **JMESPath subset** (growing toward fuller coverage) into the same selection AST (`SelectExpr`) used by the projector:

```text
// JMESPath result is the projected value (often a bare array of cards):
products[*].{id: id, title: title, handle: handle, price: variants[0].price}

// filters (per-element predicate), negative indices, functions, pipes, flatten, …
products[?variants[0].available == `true`].{id: id, title: title}
products[-1].title
length(products)
products[*].variants[*].price | []
```

Important semantic distinction for pipeline authors:

| API | Output shape intuition |
| --- | --- |
| `project_paths` / keep-list | Keeps **ancestor wrappers** (`products` still present) |
| `project_jmespath` | Emits the **JMESPath result value** (e.g. an array of cards, not re-wrapped) |

Both copy kept leaf values as raw on-wire spans when possible (numbers and escaped strings are not re-encoded), and both support formatting styles: **Compact**, **PreserveSource** (spacing fidelity around kept structure), and **Pretty**.

### Measured illustration (live Shopify-style catalog)

Against TeeFury’s public `products.json` pages (sizes vary over time; re-fetch with `./scripts/fetch_teefury.sh`):

| Workload | Approx. input | Approx. output | Notes |
| --- | ---: | ---: | --- |
| Path keep-list (ids, titles, variants, images meta) | ~386 KiB page | ~73 KiB (~19%) | Nested `products[]` wrapper kept |
| JMESPath listing cards | ~386 KiB page | ~7.6 KiB | Bare array of slim cards |
| JMESPath cards × 4 pages | ~1.9 MiB | ~15 KiB | ~120 product cards |

That is the data-engineering “noise tax” in one picture: most of the catalog bytes were never needed for a listing or silver card.

Integration tests live in `tests/teefury_project.rs` and **skip** when fixtures are absent so CI stays offline-friendly; fixtures under `benches/data/` are gitignored and never published to crates.io.

### Typed views (`JsonView` / derive)

For stable pipelines, define a partial Rust struct:

```rust
#[derive(JsonMutatorSchema)] // or JsonView
struct ListingCard {
    #[json(path = "id")]
    id: u64,
    #[json(path = "title")]
    title: String,
}
```

You get compile-time path constants, `read_from` / `write_into`, schema-guided index plans for large arrays, and `project_json` / `project_bytes` driven by `FIELD_PATHS`. Fields you never name are never read — intentional open projections for API evolution.

## The Powerful Combination: jshift + prost

Once the JSON has been reduced to only the fields that matter, the next high-leverage step is often to leave the text format behind. This is where **prost** (the idiomatic Protocol Buffers implementation for Rust) becomes extremely effective — not because jshift and prost share a wire format (they do not: prost is a binary Protobuf codec; jshift is a JSON path / projection engine on text bytes), but because they compose cleanly in a pipeline:

1. Receive the large, noisy vendor JSON (HTTP body, object store blob, or JSONL line).
2. Run jshift to **project** only the required paths (or mutate a few stamps in place on the original buffer when you must forward almost everything).
3. Deserialize the now-small cleaned JSON into a prost-generated message (or map kept fields into the protobuf struct with a thin conversion layer).
4. Serialize the protobuf message and write the compact binary form to the data lake (S3 + Iceberg, Parquet via Arrow, etc.).

The advantages compound:

- **Size** — Protocol Buffers are far more compact than even a cleaned JSON document. Storage and network costs drop.
- **Schema enforcement** — the `.proto` definition becomes the contract. Downstream consumers receive strongly typed, versioned data instead of free-form JSON.
- **Performance** — subsequent processing (Spark, DataFusion, Polars, custom Rust services) works on a dense binary format instead of repeatedly parsing text.
- **Evolution** — protobuf’s field presence and compatibility rules make schema changes safer than ad-hoc JSON field additions or removals.
- **Shared design instincts** — both ecosystems favor boring generated types, trait-shaped codecs (`Message` / `JsonView`), optional derive, and deliberate non-goals (no full reflection DOM). jshift absorbs that *product* architecture for JSON views and projections; it does not reimplement Protobuf.

In short, jshift solves the ingestion memory and CPU tax of noisy JSON; prost solves the long-term storage, transmission, and computational tax of staying in JSON. Together they form a clean boundary between the messy external world of vendor APIs and the efficient, typed internal world of a modern data platform.

### Related patterns (same idea, different second stage)

- **jshift → Arrow / Parquet** — project cards, then build Arrow batches without ever holding a fat `serde_json::Value` tree of the raw vendor dump.
- **jshift → JSONL silver** — project each TeeFury (or similar) page line-by-line (`json_lines` + per-line project) into a slim JSONL for training or analytics.
- **jshift in-place stamp** — when the contract is “forward the body but set `ingested_at` / `status`,” mutate the `Vec<u8>` and publish without a full re-serialize.

## When This Pattern Shines

- High-volume API polling or webhook ingestion where payloads are large and field-heavy.
- Cost-sensitive environments (spot instances, constrained containers, serverless with tight memory limits).
- Pipelines that already use (or want to adopt) Protocol Buffers / Apache Arrow / Iceberg as the internal interchange format.
- Teams that have already measured that JSON deserialization is a top contributor to memory pressure or job runtime.
- Catalog and multipage merge jobs where **message-at-a-time** projection (one index or one project per Shopify page / JSONL line) beats one giant DOM over a merged multi-hundred-megabyte blob.

The combination does not eliminate the need for good data modeling, but it removes one of the most common and expensive accidental complexities in Rust-based data engineering: paying full deserialization cost for data you intend to throw away.

## Operational notes (so the writeup stays honest)

- **Indexes go stale after in-place mutate.** Rebuild or drop `IndexedDocument` after edits; prefer index → many reads / project → new buffer.
- **Default path finds never auto-index.** Structural indexing is opt-in at the call site.
- **Core vs derive.** `default-features = false` builds the path/project engine without proc-macros; CI covers both matrices.
- **Projection is not always “edit in place.”** Keep-list/`project` writes a new document; in-place mutate/delete is a separate API family for surgical edits on the original buffer.
- **JMESPath coverage grows over time.** Prefer keep-lists and multi-select hashes for stable production jobs; adopt filters/functions as they land and pin versions under `0.y` carefully.
- **Fixtures and benches.** Large real JSON must stay out of the published crate history (`benches/data/` gitignored; crates.io exclude). Synthetic Criterion benches always run; real catalog checks are optional local/integration.

## Bottom line for data engineers

If your bronze layer’s job is “accept messy vendor JSON and emit only what silver needs,” jshift is the selective path-mutate and **field-projection** tool that keeps that job on the byte buffer: safe Rust, open partial schemas, optional indexes for large arrays, and a projector (including a growing JMESPath surface) that turns multi-megabyte pages into kilobyte cards before prost, Arrow, or the next hop ever sees them.
