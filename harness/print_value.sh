#!/usr/bin/env bash
# Host dynamic-print gate (§7.1, audit P6-4): prove `host_io.print(<value>)`
# prints a runtime value as unsigned decimal, identically in the simulator and in
# the LLVM-built host binary (no libc — a raw write syscall + an inline itoa).
#
#   ./harness/print_value.sh
#
# Host-only (host-io has no metal target — semihosting is P6-7).  Requires clang.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/print_value.si}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
command -v clang >/dev/null 2>&1 || { echo "SKIP: clang not found"; exit 0; }

cd "$REPO"
echo "== reference: simulator stdout =="
SIM="$(cargo run -q --bin silicac -- --sim "$EX" 2>&1 | sed -n 's/.*print "\(.*\)"/\1/p')"
# Reassemble the printed pieces, decoding the \n escape the trace shows.
SIM_OUT="$(printf '%b' "$(echo "$SIM" | tr -d '\n')")"
echo "sim printed: [$(echo "$SIM" | tr '\n' '|')]"

echo "== build + run the LLVM host binary (no libc) =="
cargo run -q --bin silicac -- --emit-llvm "$EX" -o "$WORK/pv" 2>"$WORK/emit.log" \
  || { echo "FAIL: --emit-llvm errored"; cat "$WORK/emit.log"; exit 1; }
grep -q '; unsupported in llvm canary: dynamic host_io.print' "$WORK/pv.ll" && { echo "FAIL: dynamic print still unsupported"; exit 1; }
clang "$WORK/pv.ll" -o "$WORK/pv.bin" 2>"$WORK/clang.log" || { echo "FAIL: clang"; cat "$WORK/clang.log"; exit 1; }
METAL_OUT="$("$WORK/pv.bin")"
echo "binary printed: [$METAL_OUT]"

echo "== compare =="
# The decimal value the program computes (40 + 2 = 42) must appear in both.
if echo "$METAL_OUT" | grep -qx "42" && echo "$SIM" | grep -qx "42"; then
  echo "PASS: host_io.print(<value>) prints the runtime decimal (42) identically in sim and the LLVM host binary (P6-4)."
  exit 0
else
  echo "FAIL: expected '42' in both; sim=[$SIM] binary=[$METAL_OUT]"
  exit 1
fi
