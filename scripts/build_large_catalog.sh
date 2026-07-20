#!/usr/bin/env bash
# Merge TeeFury/Shopify-style products.json pages into one large catalog document
# for Criterion large-file benches (gitignored — never commit the blob).
#
# Usage:
#   ./scripts/fetch_teefury.sh 4          # optional: download pages first
#   ./scripts/build_large_catalog.sh      # → benches/data/large.json
#   ./scripts/build_large_catalog.sh 12   # more pages if you fetched them
#
# Output shape (same as Shopify products.json root):
#   { "products": [ ...all products from every page, in page order... ] }
#
# Live catalog sizes change over time; re-fetch + rebuild before claiming absolute
# wall times. Ratios across engines on the same snapshot are the product story.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT_DIR="$ROOT/benches/data"
PAGES="${1:-4}"
OUT="${JSHIFT_LARGE_JSON:-$OUT_DIR/large.json}"

mkdir -p "$OUT_DIR"

# Prefer Python for a correct JSON merge (jq is fine too if present).
if command -v python3 >/dev/null 2>&1; then
  python3 - "$OUT_DIR" "$PAGES" "$OUT" <<'PY'
import json, sys, os
out_dir, pages_s, dest = sys.argv[1], int(sys.argv[2]), sys.argv[3]
products = []
missing = []
for p in range(1, pages_s + 1):
    path = os.path.join(out_dir, f"teefury_products_p{p}.json")
    if not os.path.isfile(path):
        missing.append(path)
        continue
    with open(path, "rb") as f:
        doc = json.load(f)
    arr = doc.get("products")
    if not isinstance(arr, list):
        sys.exit(f"error: {path} has no products array")
    products.extend(arr)
    print(f"  page {p}: +{len(arr)} products ({os.path.getsize(path)} bytes)", flush=True)
if missing:
    print("missing page files:", file=sys.stderr)
    for m in missing:
        print(f"  {m}", file=sys.stderr)
    print("run: ./scripts/fetch_teefury.sh", pages_s, file=sys.stderr)
    sys.exit(1)
if not products:
    sys.exit("error: no products collected")
# Compact separators keep the fixture smaller on disk (still valid JSON).
payload = json.dumps({"products": products}, separators=(",", ":"), ensure_ascii=False)
with open(dest, "w", encoding="utf-8") as f:
    f.write(payload)
print(f"wrote {dest}: {len(products)} products, {os.path.getsize(dest)} bytes "
      f"({os.path.getsize(dest)/1024/1024:.2f} MiB)")
PY
  exit 0
fi

if command -v jq >/dev/null 2>&1; then
  files=()
  for p in $(seq 1 "$PAGES"); do
    f="$OUT_DIR/teefury_products_p${p}.json"
    if [[ ! -f "$f" ]]; then
      echo "missing $f — run: ./scripts/fetch_teefury.sh $PAGES" >&2
      exit 1
    fi
    files+=("$f")
  done
  # shellcheck disable=SC2016
  jq -cs '{products: (map(.products) | add)}' "${files[@]}" >"$OUT"
  echo "wrote $OUT ($(wc -c <"$OUT") bytes)"
  exit 0
fi

echo "error: need python3 or jq to merge product pages" >&2
exit 1
