#!/usr/bin/env bash
# Metal LLVM `now()`/SysTick gate (§4.5, audit P5-1): prove the LLVM backend's
# SysTick time base actually advances `now()`.  Builds an uptime program ENTIRELY
# through the LLVM backend (no C), boots it on Renode, and checks the `stamp`
# cell — set to `now()` on each 100ms tick — reads back ≈ the elapsed wall time
# (the sim oracle), NOT a raw cycle counter or zero.  Proves SysTick + the
# SysTick_Handler + `@__uptime_ns` + the metal `now()` load are all wired.
#
#   RENODE=/path/to/renode ./harness/now_uptime.sh
#
# Requires: cargo, LLVM (`llc`), arm-none-eabi-gcc/nm, Renode.  No mock needed.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/uptime_nrf52840.si}"
RENODE="${RENODE:-renode}"
WORK="$(mktemp -d)"; RESC="$(mktemp).resc"
trap 'rm -rf "$WORK" "$RESC"' EXIT
if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
if [[ -d "$HOME/arm-gnu-toolchain-15.2/Payload/bin" ]]; then PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"; fi
for t in llc arm-none-eabi-gcc; do command -v "$t" >/dev/null 2>&1 || { echo "SKIP: '$t' not found (need LLVM + ARM toolchain)"; exit 0; }; done
command -v "$RENODE" >/dev/null 2>&1 || [[ -x "$RENODE" ]] || { echo "SKIP: renode not found"; exit 0; }

echo "== reference: host simulator final stamp =="
cd "$REPO"
SIM="$(cargo run -q --bin silicac -- --sim "$EX")"
SIM_STAMP="$(echo "$SIM" | sed -n 's/.*cell stamp = \([0-9][0-9]*\).*/\1/p' | tail -1)"
echo "sim final stamp (ns): ${SIM_STAMP:-<none>}"
if [[ -z "$SIM_STAMP" ]]; then echo "FAIL: sim produced no stamp"; exit 1; fi

echo "== build uptime via the LLVM backend (no C) =="
cargo run -q --bin silicac -- --target metal-nrf52840 --emit-llvm "$EX" -o "$WORK/up" 2>"$WORK/emit.log" \
  || { echo "FAIL: --emit-llvm errored"; cat "$WORK/emit.log"; exit 1; }
grep -q 'define void @SysTick_Handler()' "$WORK/up.ll" || { echo "FAIL: no SysTick handler in IR"; exit 1; }
grep -q '@llvm.readcyclecounter' "$WORK/up.ll" && { echo "FAIL: metal now() still uses readcyclecounter"; exit 1; }
llc "$WORK/up.ll" -filetype=obj -o "$WORK/up.o" 2>"$WORK/llc.log" || { echo "FAIL: llc"; cat "$WORK/llc.log"; exit 1; }
arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -nostdlib -nostartfiles -T "$WORK/up.ld" "$WORK/up.o" -o "$WORK/up.elf" 2>"$WORK/link.log" \
  || { echo "FAIL: link"; cat "$WORK/link.log"; exit 1; }

STAMP_ADDR="$(arm-none-eabi-nm "$WORK/up.elf" | awk '$3=="stamp"{print "0x"$1}')"
[[ -n "$STAMP_ADDR" ]] || { echo "FAIL: no stamp symbol in ELF"; exit 1; }

echo "== run on metal (Renode, SysTick/NVIC pinned to 64MHz) =="
# Run to 350ms (ticks at 100/200/300ms); read the low word of the i64 stamp cell.
cat > "$RESC" <<RESC
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
nvic Frequency 64000000
sysbus LoadELF @$WORK/up.elf
mach set "dk"
emulation RunFor "0.35"
sysbus ReadDoubleWord $STAMP_ADDR
quit
RESC
RAW="$(timeout 150 "$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}' | tail -1)"
[[ -n "$RAW" ]] || { echo "FAIL: no readback from Renode"; exit 1; }
METAL_STAMP=$(( RAW ))
echo "metal stamp @350ms (ns): $METAL_STAMP  (expect ≈ $SIM_STAMP ± 3ms)"

echo "== compare =="
# A SysTick-driven uptime should match the sim's 300ms stamp within a few ticks.
# (A broken base reads ~0; a cycle counter would read a wildly different magnitude.)
LO=$(( SIM_STAMP - 3000000 )); HI=$(( SIM_STAMP + 3000000 ))
if (( METAL_STAMP >= LO && METAL_STAMP <= HI )); then
  echo "PASS: LLVM-built now() tracks SysTick uptime — metal stamp ≈ sim (sim ≡ metal(LLVM), P5-1)."
  exit 0
else
  echo "FAIL: metal stamp $METAL_STAMP outside [$LO, $HI] (sim $SIM_STAMP)"
  exit 1
fi
