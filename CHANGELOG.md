# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- `try_parse_path` — strict path parser (`Error::InvalidPath` on bad brackets/indexes).
- `mutate_value_checked` — structural sniff that `new_value` is one complete JSON value.
- `from_json_string` — unescape a quoted JSON string literal only.
- Fuzz target `valid_json_ops` (random bytes + serde_json-valid property).
- Unit coverage for Wave B correctness items and prior hardenings.

### Changed
- Avoid `let` chains for broader readability / simpler MSRV story within edition 2024.
- Document `rust-version = "1.85"` (edition 2024 floor).
- `mutate_value` docs state raw splice contract (no value validation).
- `FromJsonSlice for String` docs: unescapes JSON string literals.
- `JsonMutatorSchema` derive emits `syn::Error` / `compile_error!` for bad shapes and
  invalid `#[json(path = ...)]` syntax (no `panic!` in the macro).

### Fixed
- Mutation helpers no longer panic on empty buffers or empty replacement payloads.
- Checked buffer growth / delete spans; container delimiter validation on array/object ops.
- String unescape rejects raw controls and lone surrogates; accepts UTF-16 surrogate pairs.
- `delete_key` uses forward-tracked key spans (safe with escaped quotes in keys).

## [0.1.1] - 2026-07-19

### Changed
- Split the monolithic `src/lib.rs` into focused modules without changing the public API:
  `error`, `path`, `scan`, `mutate`, and `convert` (re-exported from `lib.rs`).
- `parse_path` skips empty key segments, stops on unclosed `[`, and ignores non-numeric
  bracket contents instead of emitting confusing partial segments.
- `upsert_object_key` and `delete_key` treat the key argument as a **logical** key and
  match the escaped on-wire form, so keys containing `"`, `\`, or control characters
  update/delete correctly instead of duplicating or reverse-scan-failing.
- `String::from_json_slice` unescapes JSON string literals (`\"`, `\\`, `\n`, `\u00XX`, …).

### Added
- `escape_json_key` / `write_json_string_content` helpers for key/content escaping.
- Expanded unit coverage for path edge cases, escaped keys, nested CRUD, and error paths.
- CI workflow (tests, Clippy, rustdoc) and fuzz targets under `fuzz/`.
- `CHANGELOG.md` and `CONTRIBUTING.md`.

### Fixed
- Delete/upsert footguns when object keys contain escapes (no more reverse-scan that
  stopped on escaped quotes; no more duplicate keys on repeated upsert of special keys).

## [0.1.0] - 2026-07-18

### Added
- Initial release: path-selective find/mutate on raw JSON bytes, object/array CRUD,
  `ToJsonBytes` / `FromJsonSlice`, and `#[derive(JsonMutatorSchema)]`.

[Unreleased]: https://github.com/shan-alexander/jshift/compare/v0.1.1...HEAD
[0.1.1]: https://github.com/shan-alexander/jshift/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/shan-alexander/jshift/releases/tag/v0.1.0
