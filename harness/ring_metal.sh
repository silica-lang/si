#!/usr/bin/env bash
# Metal LLVM ring gate (§5.3, audit P6-1): prove the LLVM backend's bounded-ring
# lowering (push/pop/len, overwrite-oldest on full) runs on metal.  Builds the
# ring program ENTIRELY through the LLVM backend (no C), boots it on Renode, and
# checks the observable cells (len, sum, n) match the simulator.
#
#   RENODE=/path/to/renode ./harness/ring_metal.sh
#
# Requires: cargo, LLVM (`llc`), arm-none-eabi-gcc/nm, Renode.  No mock needed.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/ring_nrf52840.si}"
RENODE="${RENODE:-renode}"
WORK="$(mktemp -d)"; RESC="$(mktemp).resc"
trap 'rm -rf "$WORK" "$RESC"' EXIT
if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
if [[ -d "$HOME/arm-gnu-toolchain-15.2/Payload/bin" ]]; then PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"; fi
for t in llc arm-none-eabi-gcc; do command -v "$t" >/dev/null 2>&1 || { echo "SKIP: '$t' not found (need LLVM + ARM toolchain)"; exit 0; }; done
command -v "$RENODE" >/dev/null 2>&1 || [[ -x "$RENODE" ]] || { echo "SKIP: renode not found"; exit 0; }

cd "$REPO"
echo "== reference: host simulator final cells =="
SIM="$(cargo run -q --bin silicac -- --sim "$EX")"
sim_cell() { echo "$SIM" | sed -n "s/.*cell $1 = \([0-9][0-9]*\).*/\1/p" | tail -1; }
S_LEN="$(sim_cell len)"; S_SUM="$(sim_cell sum)"; S_N="$(sim_cell n)"
echo "sim: len=$S_LEN sum=$S_SUM n=$S_N"
[[ -n "$S_LEN" && -n "$S_SUM" && -n "$S_N" ]] || { echo "FAIL: sim produced no cells"; exit 1; }

echo "== build ring via the LLVM backend (no C) =="
cargo run -q --bin silicac -- --target metal-nrf52840 --emit-llvm "$EX" -o "$WORK/ring" 2>"$WORK/emit.log" \
  || { echo "FAIL: --emit-llvm errored"; cat "$WORK/emit.log"; exit 1; }
grep -q '@__ring_q_buf' "$WORK/ring.ll" || { echo "FAIL: no ring backing store in IR"; exit 1; }
llc "$WORK/ring.ll" -filetype=obj -o "$WORK/ring.o" 2>"$WORK/llc.log" || { echo "FAIL: llc"; cat "$WORK/llc.log"; exit 1; }
arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -nostdlib -nostartfiles -T "$WORK/ring.ld" "$WORK/ring.o" -o "$WORK/ring.elf" 2>"$WORK/link.log" \
  || { echo "FAIL: link"; cat "$WORK/link.log"; exit 1; }

addr() { arm-none-eabi-nm "$WORK/ring.elf" | awk -v s="$1" '$3==s{print "0x"$1}'; }
LEN_A="$(addr len)"; SUM_A="$(addr sum)"; N_A="$(addr n)"
[[ -n "$LEN_A" && -n "$SUM_A" && -n "$N_A" ]] || { echo "FAIL: missing cell symbols"; exit 1; }

echo "== run on metal (Renode, NVIC/TIMER pinned to 64MHz) =="
cat > "$RESC" <<RESC
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
nvic Frequency 64000000
sysbus LoadELF @$WORK/ring.elf
mach set "dk"
emulation RunFor "0.65"
sysbus ReadDoubleWord $LEN_A
sysbus ReadDoubleWord $SUM_A
sysbus ReadDoubleWord $N_A
quit
RESC
mapfile -t RAW < <(timeout 150 "$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}')
(( ${#RAW[@]} == 3 )) || { echo "FAIL: expected 3 readbacks, got ${#RAW[@]}: ${RAW[*]:-<none>}"; exit 1; }
M_LEN=$(( RAW[0] )); M_SUM=$(( RAW[1] )); M_N=$(( RAW[2] ))
echo "metal: len=$M_LEN sum=$M_SUM n=$M_N"

echo "== compare =="
if (( M_LEN == S_LEN && M_SUM == S_SUM && M_N == S_N )); then
  echo "PASS: LLVM-built ring (push/saturate/pop/len) matches the simulator — sim ≡ metal(LLVM), P6-1."
  exit 0
else
  echo "FAIL: sim(len=$S_LEN sum=$S_SUM n=$S_N) != metal(len=$M_LEN sum=$M_SUM n=$M_N)"
  exit 1
fi
