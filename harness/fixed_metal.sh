#!/usr/bin/env bash
# Metal LLVM fixed-point gate (§4.3, audit P6-2): prove the LLVM backend's
# fixed-point lowering (FixedArith mul/div via a 64-bit intermediate + rescale,
# FixedCast binary-point shift) runs on metal.  Builds ENTIRELY through the LLVM
# backend (no C), boots on Renode, checks the result cells match the simulator.
#
#   RENODE=/path/to/renode ./harness/fixed_metal.sh
#
# Requires: cargo, LLVM (`llc`), arm-none-eabi-gcc/nm, Renode.  No mock needed.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/fixed_nrf52840.si}"
RENODE="${RENODE:-renode}"
WORK="$(mktemp -d)"; RESC="$(mktemp).resc"
trap 'rm -rf "$WORK" "$RESC"' EXIT
if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
if [[ -d "$HOME/arm-gnu-toolchain-15.2/Payload/bin" ]]; then PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"; fi
for t in llc arm-none-eabi-gcc; do command -v "$t" >/dev/null 2>&1 || { echo "SKIP: '$t' not found (need LLVM + ARM toolchain)"; exit 0; }; done
command -v "$RENODE" >/dev/null 2>&1 || [[ -x "$RENODE" ]] || { echo "SKIP: renode not found"; exit 0; }

cd "$REPO"
echo "== reference: host simulator =="
SIM="$(cargo run -q --bin silicac -- --sim "$EX")"
sc() { echo "$SIM" | sed -n "s/.*cell $1 = \([0-9][0-9]*\).*/\1/p" | tail -1; }
S_TRIPLE="$(sc triple)"; S_QUARTER="$(sc quarter)"; S_N="$(sc n)"
echo "sim: triple=$S_TRIPLE quarter=$S_QUARTER n=$S_N"
[[ -n "$S_TRIPLE" && -n "$S_QUARTER" && -n "$S_N" ]] || { echo "FAIL: sim produced no cells"; exit 1; }

echo "== build fixed via the LLVM backend (no C) =="
cargo run -q --bin silicac -- --target metal-nrf52840 --emit-llvm "$EX" -o "$WORK/fx" 2>"$WORK/emit.log" \
  || { echo "FAIL: --emit-llvm errored"; cat "$WORK/emit.log"; exit 1; }
grep -qE 'mul i64|sdiv i64|udiv i64' "$WORK/fx.ll" || { echo "FAIL: no fixed-point 64-bit math in IR"; exit 1; }
grep -q '; unsupported expr' "$WORK/fx.ll" && { echo "FAIL: an unsupported expr remained"; exit 1; }
# Run `opt -O2` (a normal LLVM pipeline stage) before llc: it constant-folds the
# fixed-point scale constants so the 64-bit divide-by-constant lowers to shifts —
# the same way the C path's -Os folds them.  (A genuinely *runtime* divisor would
# pull libgcc's __divdi3, exactly as it would for the C backend.)
opt -O2 "$WORK/fx.ll" -o "$WORK/fx.bc" 2>"$WORK/opt.log" || { echo "FAIL: opt"; cat "$WORK/opt.log"; exit 1; }
llc "$WORK/fx.bc" -filetype=obj -o "$WORK/fx.o" 2>"$WORK/llc.log" || { echo "FAIL: llc"; cat "$WORK/llc.log"; exit 1; }
arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -nostdlib -nostartfiles -T "$WORK/fx.ld" "$WORK/fx.o" -o "$WORK/fx.elf" 2>"$WORK/link.log" \
  || { echo "FAIL: link"; cat "$WORK/link.log"; exit 1; }

addr() { arm-none-eabi-nm "$WORK/fx.elf" | awk -v s="$1" '$3==s{print "0x"$1}'; }
TRIPLE_A="$(addr triple)"; QUARTER_A="$(addr quarter)"; N_A="$(addr n)"
[[ -n "$TRIPLE_A" && -n "$QUARTER_A" && -n "$N_A" ]] || { echo "FAIL: missing cell symbols"; exit 1; }

echo "== run on metal (Renode) — sys.start computes at boot =="
cat > "$RESC" <<RESC
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
nvic Frequency 64000000
sysbus LoadELF @$WORK/fx.elf
mach set "dk"
emulation RunFor "0.65"
sysbus ReadDoubleWord $TRIPLE_A
sysbus ReadDoubleWord $QUARTER_A
sysbus ReadDoubleWord $N_A
quit
RESC
mapfile -t RAW < <(timeout 150 "$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}')
(( ${#RAW[@]} == 3 )) || { echo "FAIL: expected 3 readbacks, got ${#RAW[@]}: ${RAW[*]:-<none>}"; exit 1; }
M_TRIPLE=$(( RAW[0] )); M_QUARTER=$(( RAW[1] )); M_N=$(( RAW[2] ))
echo "metal: triple=$M_TRIPLE quarter=$M_QUARTER n=$M_N"

echo "== compare =="
if (( M_TRIPLE == S_TRIPLE && M_QUARTER == S_QUARTER && M_N == S_N )); then
  echo "PASS: LLVM-built fixed-point (mul/div rescale + cast) matches the simulator — sim ≡ metal(LLVM), P6-2."
  exit 0
else
  echo "FAIL: sim(triple=$S_TRIPLE quarter=$S_QUARTER n=$S_N) != metal(triple=$M_TRIPLE quarter=$M_QUARTER n=$M_N)"
  exit 1
fi
