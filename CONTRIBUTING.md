# Contributing to jshift

Thanks for helping improve a small, focused crate. This document covers the
workflow expected for patches and releases.

## Development setup

```bash
git clone https://github.com/shan-alexander/jshift.git
cd jshift
cargo test
cargo clippy --all-targets -- -D warnings
cargo doc --no-deps --document-private-items
```

Optional fuzzing (requires [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz)
and a nightly toolchain for libFuzzer):

```bash
cargo install cargo-fuzz
cargo +nightly fuzz run parse_path -- -max_total_time=30
cargo +nightly fuzz run mutate_ops -- -max_total_time=30
```

## Project layout

| Path | Role |
| --- | --- |
| `src/lib.rs` | Crate docs and public re-exports |
| `src/error.rs` | `Error` type |
| `src/path.rs` | `PathSegment`, `parse_path` |
| `src/scan.rs` | Path scans, skip/find helpers |
| `src/mutate.rs` | In-place mutate / upsert / delete / append |
| `src/convert.rs` | `FromJsonSlice`, `ToJsonBytes`, escape helpers |
| `jshift-derive/` | `#[derive(JsonMutatorSchema)]` |
| `fuzz/` | libFuzzer targets |
| `benches/` | Criterion benchmarks |

Keep the **public API re-exported from `lib.rs`**. Prefer pure refactors that
do not change call sites for downstream users unless the PR is intentionally
breaking (and versioned accordingly).

## Coding guidelines

- `#![forbid(unsafe_code)]` stays on. Do not introduce `unsafe`.
- Prefer nested `if` / early returns over `let` chains so the style stays simple.
- Prefer small modules and `pub(crate)` helpers over growing `lib.rs` again.
- Mutation helpers must use checked spans/growth (`validate_span`, `grow_and_shift_right`)
  rather than bare `json[start]` on untrusted offsets.
- Mutations must keep surrounding JSON structurally intact (commas, braces).
- Keys and string values that go on the wire must be escaped (`write_json_string`
  / `escape_json_key`). Logical key APIs (`upsert_object_key`, `delete_key`)
  accept unescaped Rust strings.
- Path segments from `parse_path` match **on-wire** key bytes (escaped form).
  Document any path semantics changes carefully.
- No drive-by reformatting of unrelated code.

## Tests

Every behavior change needs tests:

- Unit tests live in `src/lib.rs` (`#[cfg(test)] mod tests`) or beside the module
  under test when they are tightly scoped.
- Cover edge cases: nested paths, empty arrays/objects, resize larger/smaller,
  escaped keys/values, type mismatches, and invalid paths.
- Doctests in public item docs should compile and assert something meaningful.

```bash
cargo test
```

## Pull requests

1. Open a PR against `main` with a clear description of *what* and *why*.
2. Update `CHANGELOG.md` under `[Unreleased]` (or the target version section).
3. Ensure CI is green: tests, Clippy (`-D warnings`), and `cargo doc`.
4. Keep PRs focused; large features can be split (refactor → behavior → docs).

## Release checklist (maintainers)

1. Move `[Unreleased]` notes into a dated version section in `CHANGELOG.md`.
2. Bump versions in root and `jshift-derive` `Cargo.toml` (and path/version deps).
3. `cargo test && cargo clippy --all-targets -- -D warnings && cargo doc --no-deps`
4. Tag `vX.Y.Z` and publish derive first, then the main crate:

```bash
cargo publish -p jshift-derive
cargo publish -p jshift
```

## License

By contributing, you agree that your contributions are dual-licensed under the
same terms as the project: MIT OR Apache-2.0.
