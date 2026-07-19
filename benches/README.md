# jshift benchmarks

## Synthetic suite (in-repo)

```bash
cargo bench --bench json_benchmark
cargo bench --bench json_benchmark -- "Compete Find"
cargo bench --bench json_benchmark -- "JSON Concurrent"
```

Criterion builds multi-MB synthetic documents in memory. No huge files are required.

## Optional real-world fixtures (local only)

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
