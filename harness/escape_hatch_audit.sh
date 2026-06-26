#!/usr/bin/env bash
# Escape-hatch / idiom-corpus audit (§7.4 / audit #35 P2-2): report how often the
# language's strictness escape hatches (casts, +%/+| wrap-sat ops, .raw, .le/.be)
# appear across the std lib + examples — the measurable proxy for the
# agentic-native thesis (risk #4: "if the corpus is full of .raw, the defaults
# are wrong").  The std-lib CI gate lives in tests/escape_hatch.rs.
#
#   ./harness/escape_hatch_audit.sh
#
# Reporting gate (no Renode/LLVM). Requires cargo.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
cd "$REPO"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

log="$WORK/audit.log"
cargo run -q --bin escape_audit 2>"$WORK/err" | tee "$log" || { echo "FAIL: escape_audit did not run"; cat "$WORK/err"; exit 1; }

if ! grep -q "escape-hatch audit: corpus total" "$log"; then
  echo "FAIL: no corpus total reported"; exit 1
fi
# The std lib must stay nearly escape-hatch-free (the agent's idiom corpus).
std_total=$(awk '/-- std total:/ {for (i=1;i<=NF;i++) if ($i ~ /^\(total$/) {print $(i+1); exit}}' "$log" | tr -d ')')
echo "std-lib escape-hatch total: ${std_total:-?}"
if [[ -z "${std_total:-}" || "$std_total" -gt 3 ]]; then
  echo "FAIL: std-lib escape-hatch count ${std_total:-?} exceeds the ceiling (3)"; exit 1
fi

echo "PASS: escape-hatch corpus audit (std lib within the idiom-corpus ceiling)."
