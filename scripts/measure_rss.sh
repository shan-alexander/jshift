#!/usr/bin/env bash
# Measure max RSS for large-file project shapes (Linux: /usr/bin/time -v).
#
# Prerequisites:
#   ./scripts/fetch_teefury.sh 4 && ./scripts/build_large_catalog.sh
#   (or set JSHIFT_LARGE_JSON to an existing catalog)
#
# Usage:
#   ./scripts/measure_rss.sh
#   JSHIFT_LARGE_JSON=/path/to/large.json ./scripts/measure_rss.sh
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

JSON="${JSHIFT_LARGE_JSON:-$ROOT/benches/data/large.json}"
if [[ ! -f "$JSON" ]]; then
  echo "missing $JSON" >&2
  echo "  ./scripts/fetch_teefury.sh 4 && ./scripts/build_large_catalog.sh" >&2
  exit 1
fi

echo "=== RSS profile (large catalog) ==="
echo "fixture: $JSON ($(du -h "$JSON" | awk '{print $1}'))"
echo

# Build once so timings are not compile-dominated.
cargo build --release --example rss_project_profile -q

TIME_BIN=""
if [[ -x /usr/bin/time ]]; then
  TIME_BIN=/usr/bin/time
elif command -v time >/dev/null 2>&1; then
  TIME_BIN=time
else
  echo "error: no time binary found" >&2
  exit 1
fi

run_mode() {
  local mode="$1"
  echo "── mode=$mode ──"
  # GNU time: Maximum resident set size (kbytes)
  # Fall back to plain run if -v unsupported (busybox).
  if "$TIME_BIN" -v true >/dev/null 2>&1; then
    "$TIME_BIN" -v env JSHIFT_LARGE_JSON="$JSON" \
      ./target/release/examples/rss_project_profile "$mode" 2>&1 \
      | rg -e "Maximum resident set size" -e "mode=" -e "input_mib=" -e "out_bytes=" -e "elapsed=" \
          -e "User time" -e "System time" || true
  else
    env JSHIFT_LARGE_JSON="$JSON" ./target/release/examples/rss_project_profile "$mode"
  fi
  echo
}

for mode in hold_input project project_write project_jsonl; do
  run_mode "$mode"
done

echo "done. Compare Maximum resident set size across modes (kbytes on Linux)."
echo "Typical story: hold_input ≈ file size; project holds full card array;"
echo "project_write / project_jsonl keep output streaming (lower peak for large cards)."
