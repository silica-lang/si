#!/usr/bin/env bash
# LLVM-IR canary gate (§6.3/§12 / audit #35, P2-1): prove that SIR lowers to a
# *second*, structurally independent backend — textual LLVM IR — that
# (a) is well-formed (`llvm-as` + `opt -verify` accept it),
# (b) actually computes (the arithmetic canary compiles and exits with the value
#     it computed, 42), and
# (c) contains no C-ism: the overflow trap is `llvm.*.with.overflow` + `llvm.trap`
#     (not `__builtin_*`), and the module references no libc / C-runtime symbol.
#
#   ./harness/llvm_canary.sh
#
# This is a build-level gate (no Renode). Requires: cargo + an LLVM toolchain
# (`llvm-as`, `opt`, `clang`). On macOS install with `brew install llvm`; the
# Homebrew keg is auto-detected below. Set LLVM_BIN to override.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$REPO/examples/llvm_canary.si"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$REPO"

# ── Locate the LLVM toolchain ────────────────────────────────────────────────
if [[ -n "${LLVM_BIN:-}" ]]; then
  PATH="$LLVM_BIN:$PATH"
elif [[ -d /opt/homebrew/opt/llvm/bin ]]; then
  PATH="/opt/homebrew/opt/llvm/bin:$PATH"        # Apple-silicon Homebrew
elif [[ -d /usr/local/opt/llvm/bin ]]; then
  PATH="/usr/local/opt/llvm/bin:$PATH"           # Intel Homebrew
fi
for tool in llvm-as opt clang; do
  if ! command -v "$tool" >/dev/null 2>&1; then
    echo "SKIP: '$tool' not found (install LLVM, e.g. 'brew install llvm', or set LLVM_BIN)"
    exit 0
  fi
done
echo "== using $(llvm-as --version | sed -n '2p' | tr -s ' ') =="

# ── 1. Emit the IR ───────────────────────────────────────────────────────────
echo "== emit LLVM IR from SIR =="
if ! cargo run -q -- --emit-llvm "$SRC" -o "$WORK/canary" 2>"$WORK/emit.log"; then
  echo "FAIL: --emit-llvm errored"; cat "$WORK/emit.log"; exit 1
fi
LL="$WORK/canary.ll"
[[ -f "$LL" ]] || { echo "FAIL: no $LL produced"; exit 1; }
cat "$LL"

# ── 2. Well-formedness: llvm-as + opt -verify ────────────────────────────────
echo "== llvm-as (assemble) =="
llvm-as "$LL" -o "$WORK/canary.bc" || { echo "FAIL: llvm-as rejected the IR"; exit 1; }
echo "== opt -verify =="
opt -passes=verify "$WORK/canary.bc" -o /dev/null || { echo "FAIL: opt -verify rejected the IR"; exit 1; }
echo "PASS: IR is well-formed."

# ── 3. No C-ism: the trap is an LLVM intrinsic; no libc symbol ────────────────
echo "== structural purity (no C-ism) =="
grep -q "@llvm.uadd.with.overflow.i32" "$LL" || { echo "FAIL: trap is not an LLVM overflow intrinsic"; exit 1; }
grep -q "call void @llvm.trap()"        "$LL" || { echo "FAIL: no llvm.trap"; exit 1; }
if grep -Eq "__builtin|@printf|@puts|@putchar|@malloc|@memcpy|@fwrite|@fflush" "$LL"; then
  echo "FAIL: IR leaks a libc / C-runtime symbol"; exit 1
fi
echo "PASS: no C-ism — trap is llvm.*, module references no libc."

# ── 4. It computes: compile + run, expect exit code 42 ───────────────────────
echo "== compile + run (expect exit code 42) =="
clang "$LL" -o "$WORK/canary_exe" 2>"$WORK/clang.log" || { echo "FAIL: clang could not build the IR"; cat "$WORK/clang.log"; exit 1; }
set +e
"$WORK/canary_exe"; code=$?
set -e
if [[ "$code" -ne 42 ]]; then
  echo "FAIL: canary exited $code, expected 42 (20 + 22)"; exit 1
fi
echo "PASS: canary computed and exited 42."

echo "PASS: LLVM-IR canary gate (P2-1) — SIR is target-neutral; the trap is not a C-ism."
