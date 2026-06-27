#!/usr/bin/env bash
# Metal await TRUE-suspend gate (§5.2, audit P6-5): prove a peer reaction runs
# DURING an `await` suspension (the await analogue of bus_parity.sh).  The worker
# awaits `ready` (which starts 0); only the setter — a separate TIMER1 reaction —
# sets it.  Under the old bounded-`wfi` lowering the worker would block its own
# IRQ context and the setter could never run → timeout → done=0.  A true frame
# suspend RETURNS to the scheduler, the setter runs during the gap, and SysTick
# resumes the worker → done>0.  The done count matches the simulator (sim≡metal).
#
#   RENODE=… ./harness/await_interleave.sh           # C backend
#   BUILD=llvm RENODE=… ./harness/await_interleave.sh # LLVM backend
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/await_interleave_nrf52840.si}"
RENODE="${RENODE:-renode}"
WORK="$(mktemp -d)"; RESC="$(mktemp).resc"
ELF="$WORK/awi.elf"
trap 'rm -rf "$WORK" "$RESC"' EXIT
if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
if [[ -d "$HOME/arm-gnu-toolchain-15.2/Payload/bin" ]]; then PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"; fi
command -v "$RENODE" >/dev/null 2>&1 || [[ -x "$RENODE" ]] || { echo "SKIP: renode not found"; exit 0; }

cd "$REPO"
echo "== reference: host simulator (true suspend — setter runs during the await) =="
SIM="$(cargo run -q --bin silicac -- --sim "$EX")"
SIM_DONE="$(echo "$SIM" | sed -n 's/.*cell done = \([0-9][0-9]*\).*/\1/p' | tail -1)"
echo "sim final done = ${SIM_DONE:-0}"
[[ -n "$SIM_DONE" && "$SIM_DONE" -gt 0 ]] || { echo "FAIL: sim did not resume the await (done=0)"; exit 1; }

echo "== build metal firmware (${BUILD:-c} backend) =="
if [[ "${BUILD:-c}" == "llvm" ]]; then
  for t in llc arm-none-eabi-gcc; do command -v "$t" >/dev/null 2>&1 || { echo "SKIP: '$t' not found (need LLVM + ARM toolchain for BUILD=llvm)"; exit 0; }; done
  cargo run -q --bin silicac -- --target metal-nrf52840 --emit-llvm "$EX" -o "$WORK/awi" 2>"$WORK/emit.log" || { echo "FAIL: --emit-llvm"; cat "$WORK/emit.log"; exit 1; }
  grep -q '@__rf_0_await' "$WORK/awi.ll" || { echo "FAIL: no await suspend state in IR"; exit 1; }
  llc "$WORK/awi.ll" -filetype=obj -o "$WORK/awi.o" 2>"$WORK/llc.log" || { echo "FAIL: llc"; cat "$WORK/llc.log"; exit 1; }
  arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -nostdlib -nostartfiles -T "$WORK/awi.ld" "$WORK/awi.o" -o "$ELF" 2>"$WORK/link.log" || { echo "FAIL: link"; cat "$WORK/link.log"; exit 1; }
else
  command -v arm-none-eabi-gcc >/dev/null 2>&1 || { echo "SKIP: arm-none-eabi-gcc not found"; exit 0; }
  cargo run -q -- --target metal-nrf52840 "$EX" -o "$ELF"
fi

DONE_A="$(arm-none-eabi-nm "$ELF" | awk '$3=="done"{print "0x"$1}')"
[[ -n "$DONE_A" ]] || { echo "FAIL: no done symbol in ELF"; exit 1; }

echo "== run on metal (Renode, NVIC/TIMER/SysTick pinned to 64MHz) =="
cat > "$RESC" <<RESC
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
nvic Frequency 64000000
sysbus LoadELF @$ELF
mach set "dk"
emulation RunFor "0.40"
sysbus ReadDoubleWord $DONE_A
quit
RESC
RAW="$(timeout 150 "$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}' | tail -1)"
[[ -n "$RAW" ]] || { echo "FAIL: no readback from Renode"; exit 1; }
M_DONE=$(( RAW ))
echo "metal final done = $M_DONE  (expect > 0, and = sim $SIM_DONE)"

echo "== compare =="
if (( M_DONE > 0 && M_DONE == SIM_DONE )); then
  echo "PASS: the setter reaction ran DURING the worker's await suspension — true frame suspend, done=$M_DONE = sim (sim ≡ metal, P6-5)."
  exit 0
else
  echo "FAIL: expected done>0 and = sim ($SIM_DONE); got metal done=$M_DONE"
  exit 1
fi
