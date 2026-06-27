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

# ── 5. Extended subset (P3-4a): match→if, now(), non-sys.start reactions ──────
FEAT="$REPO/examples/llvm_features.si"
if [[ -f "$FEAT" ]]; then
  echo "== extended subset (P3-4a): emit + opt -verify examples/llvm_features.si =="
  cargo run -q --bin silicac -- --emit-llvm "$FEAT" -o "$WORK/features" 2>"$WORK/feat.log" \
    || { echo "FAIL: --emit-llvm errored on features"; cat "$WORK/feat.log"; exit 1; }
  FLL="$WORK/features.ll"
  llvm-as "$FLL" -o "$WORK/features.bc" || { echo "FAIL: llvm-as rejected features IR"; exit 1; }
  opt -passes=verify "$WORK/features.bc" -o /dev/null || { echo "FAIL: opt -verify rejected features IR"; exit 1; }
  grep -q "@llvm.readcyclecounter()" "$FLL" || { echo "FAIL: now() is not the LLVM cycle counter"; exit 1; }
  grep -q "define void @__reaction_" "$FLL" || { echo "FAIL: no non-sys.start reaction function"; exit 1; }
  grep -q "br i1" "$FLL" || { echo "FAIL: match did not lower to branches"; exit 1; }
  if grep -Eq "__builtin|@printf|@malloc|clock_gettime" "$FLL"; then
    echo "FAIL: features IR leaks a C-ism"; exit 1
  fi
  echo "PASS: extended subset is well-formed (match→if, now()→llvm.readcyclecounter, reaction functions; no C-ism)."
fi

# ── 6. MMIO register access (P3-4b): volatile load/store + llc to object ──────
MMIO="$REPO/examples/llvm_mmio.si"
if [[ -f "$MMIO" ]]; then
  echo "== MMIO (P3-4b): emit + opt -verify + llc to object =="
  cargo run -q --bin silicac -- --emit-llvm "$MMIO" -o "$WORK/mmio" 2>"$WORK/mmio.log" \
    || { echo "FAIL: --emit-llvm errored on mmio"; cat "$WORK/mmio.log"; exit 1; }
  MLL="$WORK/mmio.ll"
  llvm-as "$MLL" -o "$WORK/mmio.bc" || { echo "FAIL: llvm-as rejected mmio IR"; exit 1; }
  opt -passes=verify "$WORK/mmio.bc" -o /dev/null || { echo "FAIL: opt -verify rejected mmio IR"; exit 1; }
  grep -q "load volatile i32"  "$MLL" || { echo "FAIL: no volatile MMIO load"; exit 1; }
  grep -q "store volatile i32" "$MLL" || { echo "FAIL: no volatile MMIO store"; exit 1; }
  grep -q "inttoptr i64"       "$MLL" || { echo "FAIL: no absolute-address pointer"; exit 1; }
  llc "$MLL" -o "$WORK/mmio.o" -filetype=obj 2>"$WORK/llc.log" \
    || { echo "FAIL: llc could not codegen the MMIO IR to an object"; cat "$WORK/llc.log"; exit 1; }
  echo "PASS: MMIO lowers to volatile load/store at absolute addresses; llc → object OK."
fi

echo "PASS: LLVM-IR backend gate (P2-1 + P3-4a + P3-4b) — SIR is target-neutral; the trap is not a C-ism."
