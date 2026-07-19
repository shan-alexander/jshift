# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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

[Unreleased]: https://github.com/shan-alexander/jshift/compare/v0.2.2...HEAD
[0.2.2]: https://github.com/shan-alexander/jshift/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/shan-alexander/jshift/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/shan-alexander/jshift/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/shan-alexander/jshift/releases/tag/v0.1.0
