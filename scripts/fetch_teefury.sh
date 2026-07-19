#!/usr/bin/env bash
# Download TeeFury Shopify products.json pages for local projection / bench work.
# Fixtures land under benches/data/ (gitignored — never commit).
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/benches/data"
mkdir -p "$OUT"
PAGES="${1:-4}"
UA="jshift-fetch/0.4 (+https://github.com/shan-alexander/jshift)"
for p in $(seq 1 "$PAGES"); do
  dest="$OUT/teefury_products_p${p}.json"
  echo "GET https://teefury.com/products.json?page=$p -> $dest"
  curl -fsSL -A "$UA" -o "$dest" "https://teefury.com/products.json?page=$p"
  wc -c "$dest"
  sleep 0.4
done
echo "done: $PAGES page(s) in $OUT"
