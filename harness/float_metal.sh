#!/usr/bin/env bash
# Metal LLVM float gate (§4.3, audit P6-8): prove runtime float arithmetic runs
# on the nRF52840's Cortex-M4F hardware FPU, built ENTIRELY through the LLVM
# backend (no C).  The reset handler enables CPACR (CP10/CP11); float add/mul
# lower to hardware vadd.f32/vmul.f32 (no soft-float libcalls).  The result cells
# (IEEE bit patterns) are read back and must equal the simulator's.
#
#   RENODE=/path/to/renode ./harness/float_metal.sh
#
# Requires: cargo, LLVM (`llc`), arm-none-eabi-gcc/nm, Renode.  No mock needed.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/float_nrf52840.si}"
RENODE="${RENODE:-renode}"
WORK="$(mktemp -d)"; RESC="$(mktemp).resc"
trap 'rm -rf "$WORK" "$RESC"' EXIT
if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
if [[ -d "$HOME/arm-gnu-toolchain-15.2/Payload/bin" ]]; then PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"; fi
for t in llc arm-none-eabi-gcc; do command -v "$t" >/dev/null 2>&1 || { echo "SKIP: '$t' not found (need LLVM + ARM toolchain)"; exit 0; }; done
command -v "$RENODE" >/dev/null 2>&1 || [[ -x "$RENODE" ]] || { echo "SKIP: renode not found"; exit 0; }

cd "$REPO"
echo "== reference: host simulator (IEEE bit patterns) =="
SIM="$(cargo run -q --bin silicac -- --sim "$EX")"
sc() { echo "$SIM" | sed -n "s/.*cell $1 = \([0-9][0-9]*\).*/\1/p" | tail -1; }
S_ACC="$(sc acc)"; S_OUT="$(sc out)"
echo "sim: acc=$S_ACC out=$S_OUT"
[[ -n "$S_ACC" && -n "$S_OUT" ]] || { echo "FAIL: sim produced no cells"; exit 1; }

echo "== build float via the LLVM backend (no C), hardware FPU =="
cargo run -q --bin silicac -- --target metal-nrf52840 --emit-llvm "$EX" -o "$WORK/flt" 2>"$WORK/emit.log" \
  || { echo "FAIL: --emit-llvm errored"; cat "$WORK/emit.log"; exit 1; }
grep -q 'CPACR' "$WORK/flt.ll" || { echo "FAIL: no FPU (CPACR) enable in IR"; exit 1; }
grep -qE 'fadd float|fmul float' "$WORK/flt.ll" || { echo "FAIL: no float arithmetic in IR"; exit 1; }
# Hard-float codegen: -mcpu=cortex-m4 enables the FPU, -float-abi=hard avoids the
# soft-float libcalls (__addsf3, …) that -nostdlib couldn't satisfy.
llc -mcpu=cortex-m4 -float-abi=hard "$WORK/flt.ll" -filetype=obj -o "$WORK/flt.o" 2>"$WORK/llc.log" || { echo "FAIL: llc"; cat "$WORK/llc.log"; exit 1; }
arm-none-eabi-nm "$WORK/flt.o" | grep -iqE "addsf|mulsf|aeabi_f" && { echo "FAIL: soft-float libcall emitted (FPU not targeted)"; exit 1; }
arm-none-eabi-gcc -mcpu=cortex-m4 -mfpu=fpv4-sp-d16 -mfloat-abi=hard -mthumb -nostdlib -nostartfiles -T "$WORK/flt.ld" "$WORK/flt.o" -o "$WORK/flt.elf" 2>"$WORK/link.log" \
  || { echo "FAIL: link"; cat "$WORK/link.log"; exit 1; }

addr() { arm-none-eabi-nm "$WORK/flt.elf" | awk -v s="$1" '$3==s{print "0x"$1}'; }
ACC_A="$(addr acc)"; OUT_A="$(addr out)"
[[ -n "$ACC_A" && -n "$OUT_A" ]] || { echo "FAIL: missing cell symbols"; exit 1; }

echo "== run on metal (Renode, NVIC/TIMER pinned to 64MHz) =="
cat > "$RESC" <<RESC
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
nvic Frequency 64000000
sysbus LoadELF @$WORK/flt.elf
mach set "dk"
emulation RunFor "0.35"
sysbus ReadDoubleWord $ACC_A
sysbus ReadDoubleWord $OUT_A
quit
RESC
mapfile -t RAW < <(timeout 150 "$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}')
(( ${#RAW[@]} == 2 )) || { echo "FAIL: expected 2 readbacks, got ${#RAW[@]}: ${RAW[*]:-<none>}"; exit 1; }
M_ACC=$(( RAW[0] )); M_OUT=$(( RAW[1] ))
echo "metal: acc=$M_ACC out=$M_OUT  (IEEE bits; expect acc=$S_ACC out=$S_OUT)"

echo "== compare =="
if (( M_ACC == S_ACC && M_OUT == S_OUT )); then
  echo "PASS: LLVM-built hardware-FPU float (add/mul) matches the simulator — acc=4.5 out=9.0 bit-exact, sim ≡ metal(LLVM), P6-8."
  exit 0
else
  echo "FAIL: sim(acc=$S_ACC out=$S_OUT) != metal(acc=$M_ACC out=$M_OUT)"
  exit 1
fi
