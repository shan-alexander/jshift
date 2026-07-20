# jshift benchmarks

## Hero matrix (README “task × size × competitor” table)

```bash
cargo run --release --example hero_matrix
# optional: large catalog column uses benches/data/large.json
```

Produces multi-size medians for find (key-first/last), mutate, sparse product access,
indexed mid hops, first-card project, and full thin cards vs serde / gjson / sonic-rs.
Numbers are pasted into the root README hero matrix; re-run on your machine before citing.

## JSONL (AI / training dumps)

```bash
# generate ~30k-line / ~20 MiB multi-topic fixture (gitignored)
cargo run --release --example gen_jsonl_fixture -- benches/data/jsonl_20mb.jsonl
# JSONL_LINES=50000 JSONL_TARGET_MIB=40 cargo run --release --example gen_jsonl_fixture -- …

cargo run --release --example jsonl_bench
# prefers benches/data/jsonl_20mb.jsonl, else 3k_lines_4mb.jsonl
# or: JSHIFT_JSONL=/path/to/file.jsonl cargo run --release --example jsonl_bench
```

Walks every line with `json_lines`; compares jshift find / derive meta / project / upsert /
`array_len` vs serde_json full parse per line. See README Quick Start B for numbers and
when JSONL work is (and is not) a multi-× win.

## Synthetic suite (in-repo)

```bash
cargo bench --bench json_benchmark
cargo bench --bench json_benchmark -- "Compete Find"
cargo bench --bench json_benchmark -- "JSON Concurrent"

# Projection (10 / 50 / 100 MiB synthetic catalogs + optional giant file)
cargo bench --bench project_benchmark
JSHIFT_LARGE_JSON=benches/data/catalog_300mb.json cargo bench --bench project_benchmark -- large

# Hygiene groups on large.json (index build, first P0, filter, flatten)
cargo bench --bench project_benchmark -- "large hygiene"

# CPU-heavy list project: sequential vs parallel (feature parallel)
cargo run --example gen_heavy_parallel_fixture --release -- benches/data/heavy_parallel.json
cargo bench --features parallel --bench project_benchmark -- "heavy parallel"
```

Criterion builds multi-MB synthetic documents in memory. No huge files are required
for the default groups. Drop a 100–300 MiB JSON at `benches/data/large.json` or set
`JSHIFT_LARGE_JSON` for the large-file project group.

### Heavy parallel fixture (when `project_indexed_parallel` shines)

Thin-card rewrites (`id`/`title` only) are often **memory-bound**; parallel may not win.
The **heavy** fixture is a domain-agnostic `records[]` array where each element forces
nested filter/length work on a large `scores` array — **CPU-bound per element**.

| Env | Default | Meaning |
| --- | ---: | :--- |
| `HEAVY_RECORDS` | 60000 | outer array length |
| `HEAVY_SCORES` | 350 | floats filtered **twice** per record |
| `HEAVY_EVENTS` | 35 | nested event objects |
| `JSHIFT_HEAVY_PARALLEL_JSON` | `benches/data/heavy_parallel.json` | override path |

Suggested expr (also printed by the generator):

```text
records[*].{id: id, hi: length(scores[?@ > `0.7`]), lo: length(scores[?@ > `0.3`]), n_ev: length(events), w0: events[0].w, a0: attrs.k0}
```

## Building `benches/data/large.json` (reproducible, gitignored)

The Criterion “large compete / large hygiene” groups expect a single catalog document:

```json
{ "products": [ /* many product objects */ ] }
```

### Method A — TeeFury public Shopify `products.json` pages (default story)

```bash
# Download N pages (default 4). Output: benches/data/teefury_products_p{1..N}.json
./scripts/fetch_teefury.sh 4

# Merge all products arrays into one root object (compact JSON)
./scripts/build_large_catalog.sh       # uses pages 1..4
./scripts/build_large_catalog.sh 12    # if you fetched 12 pages

# Override output path
JSHIFT_LARGE_JSON=/tmp/my_large.json ./scripts/build_large_catalog.sh
```

`build_large_catalog.sh` prefers **python3**, else **jq**. It fails clearly if page
files are missing.

**Important:** live catalog contents and sizes change over time. Re-fetch + rebuild
before quoting absolute wall times. Ratios across engines on **one snapshot** are the
durable product story. Historical README numbers used ~338 MiB / ~25 000 products on
one quiet machine after a 4-page merge (exact byte count will drift).

### Method B — your own Shopify export

1. Export or download `products.json` (single page or multi-page).
2. If multi-page, place files as `benches/data/teefury_products_pN.json` with the same
   `{ "products": [ ... ] }` shape (any host; the merge script only reads that key),
   **or** concatenate with your own tool into one `{ "products": [...] }` file.
3. Copy/move to `benches/data/large.json` or set `JSHIFT_LARGE_JSON`.

### Method C — synthetic only

Use Criterion’s in-memory `generate_catalog_mb` groups (always available; no fixture file).

## Peak RSS and allocator profiles

```bash
# Max RSS across hold / project / project_write / project_jsonl (Linux GNU time)
./scripts/measure_rss.sh

# dhat: precise heap totals + peak live (writes dhat-heap.json, gitignored)
cargo run --release --example alloc_profile --features dhat-heap -- project
# modes: hold | project | project_write | project_jsonl | project_each

# heaptrack (optional system package): call-tree of allocations
cargo build --release --example alloc_profile
heaptrack ./target/release/examples/alloc_profile project
```

See the root [README.md](../README.md) Performance section for measured tables and
how Criterion + RSS + dhat + heaptrack fit together.

## Optional real-world fixtures (local only)

### TeeFury Shopify catalog (projection integration tests)

```bash
./scripts/fetch_teefury.sh 4   # pages 1..4 → benches/data/teefury_products_p*.json
cargo test --test teefury_project -- --nocapture
```

Fixtures are gitignored. Without them, `teefury_project` tests **skip** (CI stays green offline).

Example measured on a quiet machine (live API, sizes vary):

| Workload | Input | Output | Notes |
| :--- | ---: | ---: | :--- |
| Path keep-list (variants/images/ids) | ~386 KiB p1 | ~73 KiB (~19%) | nested `products[]` wrapper kept |
| JMESPath listing cards | ~386 KiB p1 | ~7.6 KiB | `products[*].{id,title,handle,price,image}` |
| JMESPath cards ×4 pages | ~1.9 MiB | ~15 KiB | 120 product cards |

### Other large dumps

Drop large files under:

```text
benches/datasets/   # gitignored
# e.g. benches/datasets/products_300mb.json
```

They are:

* listed in **`.gitignore`** (not committed),
* listed in **`Cargo.toml` `exclude`** (not published to crates.io),

so `cargo publish` stays small even if you have fixtures on disk.

### Recommended layout for heavy data

| Approach | Use when |
| :--- | :--- |
| **gitignored local files** + short path in a private bench binary | You alone re-run real-world benches |
| **Separate repo** `jshift-benchmarks` with scripts + download URLs / LFS | You want shareable methodology without bloating the library clone |
| **Git LFS in the library repo** | Usually **avoid** for 100MB+ fixtures; clones still hurt and CI gets heavy |

Prefer **not** committing multi-hundred-MiB JSON into `jshift` itself: history never shrinks, and every `git clone` pays for it forever.

A clean pattern:

1. Keep Criterion synthetic benches here (always runnable).
2. Optional: `jshift-benchmarks` repo clones `jshift` as a path/git dep and holds fixtures + report generation.
3. Document fixture source (URL, license, generation script) so results are reproducible without the blob in git.
