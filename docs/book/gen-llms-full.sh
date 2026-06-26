#!/usr/bin/env bash
# Generate docs/book/src/llms-full.txt: the entire book concatenated into one
# Markdown file for single-fetch LLM ingestion (https://llmstxt.org/).
# Chapters are emitted in SUMMARY.md order. Run before `mdbook build`.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")" && pwd)"
SRC="$ROOT/src"
OUT="$SRC/llms-full.txt"
BASE="https://silica-lang.github.io/si"

{
  echo "# Silica — Full Documentation"
  echo
  echo "> The entire Silica documentation concatenated into one file for LLM ingestion."
  echo "> Generated from docs/book/src by gen-llms-full.sh. See $BASE/llms.txt for the index."
  echo
} > "$OUT"

# Pull every chapter path out of SUMMARY.md, in order, from its Markdown links.
grep -oE '\]\(([^)]+\.md)\)' "$SRC/SUMMARY.md" \
  | sed -E 's/^\]\(//; s/\)$//' \
  | while read -r path; do
      file="$SRC/$path"
      if [ ! -f "$file" ]; then
        echo "gen-llms-full: warning: missing $path" >&2
        continue
      fi
      url="$BASE/${path%.md}.html"
      {
        echo
        echo "---"
        echo
        echo "<!-- source: docs/book/src/$path | canonical: $url -->"
        echo
        cat "$file"
        echo
      } >> "$OUT"
    done

echo "gen-llms-full: wrote $OUT"
