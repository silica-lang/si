#!/usr/bin/env bash
# Metal LLVM scheduler gate (§4.5/§6.3, audit P4-1): the LLVM backend's `every`
# → hardware-TIMER path actually fires.  Builds blink ENTIRELY through the LLVM
# backend (no C), boots it on Renode, and checks the LED (P0.13) toggles on the
# 500 ms period — matching the simulator.  Proves Reset_Handler startup + TIMER1
# + TIMER1_IRQHandler + the periodic reaction function are all wired.
#
#   RENODE=/path/to/renode ./harness/llvm_metal_sched.sh
#
# Requires: cargo, LLVM (`llc`), arm-none-eabi-gcc, Renode. Uses examples/blink_nrf52840.si.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="$REPO/examples/blink_nrf52840.si"
RENODE="${RENODE:-renode}"
WORK="$(mktemp -d)"; RESC="$(mktemp).resc"
trap 'rm -rf "$WORK" "$RESC"' EXIT
if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
if [[ -d "$HOME/arm-gnu-toolchain-15.2/Payload/bin" ]]; then PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"; fi
for t in llc arm-none-eabi-gcc renode; do command -v "$t" >/dev/null 2>&1 || { [[ "$t" == renode && -x "$RENODE" ]] || { echo "SKIP: '$t' not found"; exit 0; }; }; done

echo "== reference: host simulator LED (P0.13) sequence =="
cd "$REPO"
SIM="$(cargo run -q --bin silicac -- --sim "$EX")"
# bit(13) writes after the t=0 sys.start write = the toggle sequence (1,0,1,0,…).
mapfile -t SIM_SEQ < <(echo "$SIM" | grep -oE 'bit\(13\) = [01]' | grep -oE '[01]$' | tail -n +2)
echo "sim toggles: ${SIM_SEQ[*]:-<none>}"
if (( ${#SIM_SEQ[@]} < 4 )); then echo "FAIL: sim did not toggle the LED"; exit 1; fi

echo "== build blink via the LLVM backend (no C) =="
cargo run -q --bin silicac -- --target metal-nrf52840 --emit-llvm "$EX" -o "$WORK/blink" 2>"$WORK/emit.log" \
  || { echo "FAIL: --emit-llvm errored"; cat "$WORK/emit.log"; exit 1; }
grep -q 'define void @TIMER1_IRQHandler()' "$WORK/blink.ll" || { echo "FAIL: no TIMER1 handler in IR"; exit 1; }
llc "$WORK/blink.ll" -filetype=obj -o "$WORK/blink.o" 2>"$WORK/llc.log" || { echo "FAIL: llc"; cat "$WORK/llc.log"; exit 1; }
arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -nostdlib -nostartfiles -T "$WORK/blink.ld" "$WORK/blink.o" -o "$WORK/blink.elf" 2>"$WORK/link.log" \
  || { echo "FAIL: link"; cat "$WORK/link.log"; exit 1; }

echo "== run on metal (Renode, SysTick/TIMER pinned to 64MHz) =="
# LED off at boot; toggles at 500/1000/1500/2000ms. Sample P0.13 at half-period
# offsets (750/1250/1750/2250ms) → expect on/off/on/off = 1,0,1,0.
cat > "$RESC" <<RESC
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
nvic Frequency 64000000
sysbus LoadELF @$WORK/blink.elf
mach set "dk"
emulation RunFor "0.7500"
sysbus ReadDoubleWord 0x50000504
emulation RunFor "0.5000"
sysbus ReadDoubleWord 0x50000504
emulation RunFor "0.5000"
sysbus ReadDoubleWord 0x50000504
emulation RunFor "0.5000"
sysbus ReadDoubleWord 0x50000504
quit
RESC
mapfile -t RAW < <(timeout 150 "$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}')
if (( ${#RAW[@]} != 4 )); then echo "FAIL: expected 4 samples, got ${#RAW[@]}: ${RAW[*]:-<none>}"; exit 1; fi
METAL_SEQ=()
for v in "${RAW[@]}"; do METAL_SEQ+=( $(( ( $(( v )) & 0x2000 ) != 0 ? 1 : 0 )) ); done
echo "metal LED @750/1250/1750/2250ms: ${METAL_SEQ[*]}"

echo "== compare =="
EXPECT="${SIM_SEQ[0]} ${SIM_SEQ[1]} ${SIM_SEQ[2]} ${SIM_SEQ[3]}"
GOT="${METAL_SEQ[*]}"
if [[ "$GOT" == "$EXPECT" && "$GOT" == "1 0 1 0" ]]; then
  echo "PASS: LLVM-built blink toggles the LED on its 500ms TIMER period — sim ≡ metal(LLVM), scheduler works (P4-1)."
  exit 0
else
  echo "FAIL: expected '$EXPECT' (sim), got '$GOT' (metal)"; exit 1
fi
