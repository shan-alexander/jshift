# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.4.0] - 2026-07-20

API maturity pass: open document views, field projection / JMESPath subset, streaming
cards, parallel list project, and measurement tooling. Still `#![forbid(unsafe_code)]`.

### Added

#### Views, documents, JSONL
- **`JsonView` trait**: `read_from` / `read_from_indexed` / `read_from_doc` / `write_into`
  for typed projections of JSON bytes; free helpers `read_view` / `write_view`.
- **Derive implements `JsonView`:** `#[derive(JsonView)]` alias of `JsonMutatorSchema`;
  `FIELD_PATHS`, `FIELD_JMES`, `INDEXED_ARRAY_PATHS`, `prepare()`, mutators,
  `project_json` / `schema_project_plan`, index-aware project helpers.
- **`SharedDocument`**: cheaply cloneable `Arc<[u8]>` for read-heavy fan-out.
- **JSONL helpers:** `json_lines`, `JsonLines`, `read_jsonl`, `read_jsonl_indexed`,
  `read_line_indexed` (message-at-a-time indexing).
- **Cargo feature `derive`** (default). Core path + index APIs always compile;
  indexing remains **opt-in at call site**.

#### Field projection / JMESPath subset
- **`project` / `project_paths` / `project_jmespath` / `project_into` / `project_write`**,
  `ProjectPlan`, `SelectExpr` AST (fields, multi-select, pipe, flatten, slices,
  filters, comparisons, `&&`/`||`/`!`, functions, literals), styles
  (`Compact` / `PreserveSource` / `Pretty`), `MissingPolicy`, `projected_len` /
  `estimate_projected_len` / `estimate_values_len`.
- **JMESPath surface:** filters, slices, object projection (`*` / `foo.*` / `*.bar`),
  multi-select list/hash, flatten `[]` vs list `[*]`, expression refs, higher-order
  `map` / `sort_by` / `max_by` / `min_by` / `group_by` plus standard functions;
  soft null semantics; `Error::Jmespath`.
- **`project_indexed` / `project_auto_indexed` / `project_indexed_prepare`**,
  `ProjectPlan::array_paths_for_index`, `IndexedDocument::index_for_plan`.
- **P0 sparse project:** open-ended descent, no root full-document `skip_value` for
  non-Identity plans; keep-list last-field skip avoidance.
- **Streaming list emit** for `[*]` / filter / slice; **one-pass pure-field multi-select**.
- **`project_object_fields` / `plan_object_fields`:** domain-agnostic thin cards.
- **Streaming cards / JSONL emit:** `project_each` / `project_object_fields_each`,
  `project_jsonl_write` / `project_object_fields_jsonl_write` (+ indexed variants).
- **Feature `parallel`:** Rayon `project_parallel` / `project_indexed_parallel` /
  `project_object_fields_parallel`; **`plan_prefers_parallel`**, `project_indexed_auto`,
  `project_parallel_auto`.
- **`Transform` / `TransformPipeline`** (KeepPaths, Jmes, Rename, Drop, Inject, Style).
- **Derive:** `#[json(jmes = "...")]` multi-select project plan.

#### Tests, benches, docs tooling
- JMESPath compliance suite (tier A + full suite gate, zero residuals on vendored fixtures).
- Fuzz targets for project paths / jmespath.
- Criterion project benches; optional large-file / heavy-parallel fixtures (gitignored).
- Examples: `hero_matrix`, `jsonl_bench`, `gen_jsonl_fixture`, `gen_heavy_parallel_fixture`,
  `rss_project_profile`, `alloc_profile` (optional feature `dhat-heap`).
- Scripts: `fetch_teefury.sh`, `build_large_catalog.sh`, `measure_rss.sh`.
- README hero matrix, large-file tables, JSONL Quick Start; CI for core-only + compliance.

### Notes
- Unmentioned JSON paths stay unread / byte-preserved on `write_into`.
- Indexes bind to a byte snapshot; rebuild or drop after in-place mutate.
- Derive-dependent tests are `cfg(feature = "derive")` so core-only builds are real.
- Large fixtures under `benches/data/` are gitignored and not published to crates.io.

## [0.3.1] - 2026-07-19

### Added
- Indexed benches vs **gjson / sonic-rs / serde_json** (array mid/last + wide object);
  README documents **opt-in** indexing (no tax on default paths).

### Added
- **Stage-1 structural index** ([`StructuralIndex`] / `build_structural_index`): safe list of
  `{ } [ ] : ,` outside strings; container skip via structural walk; optional on
  [`IndexedDocument`] (`index_structural`, `build_structural`, `build_full`).
- **Object key maps** ([`ObjectKeyIndex`]): `index_object` / `index_object_str` for O(1)
  key → value span on wide/hot objects.
- **Derive auto-index:** `INDEXED_ARRAY_PATHS`, `indexed_document()`,
  `read_from_json_indexed()` — infers static array prefixes (`products[0].x` → `products`).
- `static_array_prefixes_from_path` helper for tooling.

## [0.3.0] - 2026-07-19

### Added
- **Structural array indexing (safe Rust):** [`IndexedDocument`] / [`ArrayIndex`] build
  per-array element start side-tables so `products[i].field` jumps in O(1) instead of
  linearly skipping siblings. `find`, `for_each_element`, multi-array `build`.
- Bench group **Indexed array mid/last find** (linear vs indexed on 50k elements).
- `find_from_value` path continuation helper for index jumps.

### Notes
- Indexes bind to a `&[u8]` snapshot; rebuild after in-place mutations (documented).
- Not a full simdjson DOM — metadata in service of path navigation / projection.

## [0.2.2] - 2026-07-19

### Fixed
- Path descent no longer fully `skip_value`s a matching container before walking into
  it. Looking up `products[0].title` on a multi-hundred-MiB array no longer scans the
  entire `products` value first (was ~500 ms; now microseconds for early keys).

### Changed
- README: real-world API ingestion story, refreshed Criterion numbers, vs-engine
  speedup columns, concurrent key-first ×8; large fixtures documented as gitignored /
  crates.io-excluded (`benches/README.md`).

## [0.2.1] - 2026-07-19

### Added
- `Path` / `OwnedPathSegment` — owned reusable paths (`Path::parse`, `try_parse`, `find`,
  `mutate`, `borrowed`).
- `Path::from_json_pointer` — RFC 6901 JSON Pointer (`~0` / `~1`, numeric tokens as indexes).
- `upsert_at_path` — upsert a leaf while creating missing object parents as `{}`.
- `Option<T>` for `FromJsonSlice` / `ToJsonBytes` (`null` ↔ `None`); derive maps
  missing paths to `None` for `Option` fields.
- Derive emits `'static` path segment constants (no `parse_path` on every `set_*` / read).
- Fair criterion groups: key-first 10MB + ~1KB hot path vs **gjson** / **sonic-rs** /
  serde_json (legacy key-last 10MB groups retained).

### Changed
- `delete_key` / `delete_index` pretty-delete: expand the removed span over adjacent
  whitespace so empties become `{}` / `[]` and first-member deletes do not leave a
  leading space after `{` / `[`.
- Faster `skip_value` using gjson techniques in **safe** Rust: unified brace squash,
  16-byte unrolled bulk scan, tight string skip (`\` + next byte). Still
  `forbid(unsafe_code)`.
- Concurrent bench: jshift / gjson / serde_json ×8 independent workers.
- New **Compete Find** criterion groups (key-last 10MB, key-first 10MB, small+nested)
  vs gjson, sonic-rs, and **serde_json**; README performance tables updated.

## [0.2.0] - 2026-07-19

Minor bump under Cargo’s `0.y` rules: several **behavior and type** changes vs `0.1.0`
are intentionally not patch-compatible.

### Breaking
- New `Error::InvalidPath` variant; `Error` is now `#[non_exhaustive]` (exhaustive
  `match`es on `Error` need a wildcard arm).
- `String::from_json_slice` **unescapes** JSON string literals (was raw content between
  quotes, including escape backslashes).
- `upsert_object_key` / `delete_key` take **logical** keys and match the escaped on-wire
  form (callers that passed already-escaped key strings must pass the logical form).
- `parse_path` edge cases tightened (empty keys skipped, unclosed `[` stops, non-numeric
  indexes dropped); prefer `try_parse_path` when invalid paths must error.

### Added
- `try_parse_path` — strict path parser (`Error::InvalidPath` on bad brackets/indexes).
- `mutate_value_checked` — structural sniff that `new_value` is one complete JSON value.
- `from_json_string` — unescape a quoted JSON string literal only.
- `escape_json_key` / `write_json_string_content` helpers for key/content escaping.
- Expanded unit coverage for path edge cases, escaped keys, nested CRUD, hardenings,
  Wave B correctness, and serde_json property-style checks.
- CI workflow (tests, Clippy, rustdoc) and fuzz targets under `fuzz/`
  (`parse_path`, `find_value`, `mutate_ops`, `valid_json_ops`).
- `CHANGELOG.md` and `CONTRIBUTING.md`.
- Document `rust-version = "1.85"` (edition 2024 floor).

### Changed
- Split the monolithic `src/lib.rs` into focused modules without changing the public API
  surface of re-exports: `error`, `path`, `scan`, `mutate`, and `convert`.
- `mutate_value` docs state the raw splice contract (no value validation).
- `FromJsonSlice for String` docs clarify unescaping of quoted literals.
- `JsonMutatorSchema` derive emits `syn::Error` / `compile_error!` for bad shapes and
  invalid `#[json(path = ...)]` syntax (no `panic!` in the macro).
- Avoid `let` chains for broader readability / simpler MSRV story within edition 2024.

### Fixed
- Delete/upsert footguns when object keys contain escapes (forward-tracked key spans;
  no reverse-scan that stops on escaped quotes; no duplicate keys on repeated upsert
  of special keys).
- Mutation helpers no longer panic on empty buffers or empty replacement payloads.
- Checked buffer growth / delete spans; container delimiter validation on array/object ops.
- String unescape rejects raw controls and lone surrogates; accepts UTF-16 surrogate pairs.

## [0.1.0] - 2026-07-18

### Added
- Initial release: path-selective find/mutate on raw JSON bytes, object/array CRUD,
  `ToJsonBytes` / `FromJsonSlice`, and `#[derive(JsonMutatorSchema)]`.

[Unreleased]: https://github.com/shan-alexander/jshift/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/shan-alexander/jshift/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/shan-alexander/jshift/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/shan-alexander/jshift/compare/v0.2.2...v0.3.0
[0.2.2]: https://github.com/shan-alexander/jshift/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/shan-alexander/jshift/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/shan-alexander/jshift/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/shan-alexander/jshift/releases/tag/v0.1.0
