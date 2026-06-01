#!/usr/bin/env bash
# Phase-0 metal gate: prove the SAME program produces the SAME LED behaviour in
# the host simulator (`--sim`) and on metal (nRF52840 in Renode) — the §9.6
# "identical program runs in sim and on metal" criterion, as an automated check.
#
#   RENODE=/path/to/renode ./harness/metal_vs_sim.sh
#
# Requires: cargo, arm-none-eabi-gcc, and a Renode binary (set $RENODE or have
# `renode` on PATH).  Pins Renode's NVIC/SysTick clock to 64 MHz so the blink
# period matches real time; injects the button at the same virtual times as the
# program's `sim` block (1200ms, 1800ms).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/blink_button_nrf52840.si}"
RENODE="${RENODE:-renode}"
ELF="$(mktemp -d)/metal.elf"
RESC="$(mktemp).resc"

echo "== reference: host simulator =="
# LED writes to gpio0 bit13, in order; drop the t=0 sys.start write so the 7
# remaining values line up with the metal checkpoints (one per toggle).
mapfile -t SIM < <(
  (cd "$REPO" && cargo run -q -- --sim "$EX") \
    | sed -n 's/.*bit(13) = \([01]\).*/\1/p' | tail -n +2
)
echo "sim LED sequence: ${SIM[*]}"

echo "== build metal firmware =="
(cd "$REPO" && cargo run -q -- --target metal-nrf52840 "$EX" -o "$ELF")

echo "== run on metal (Renode, SysTick pinned to 64MHz) =="
# Checkpoints are sampled just after each expected toggle (500/1000/1200/1500/
# 1800/2000/2500ms).  The button's falling edge lands on Release, so each
# injection is a Press shortly before the target and a Release at it.
cat > "$RESC" <<RESC
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
nvic Frequency 64000000
sysbus LoadELF @$ELF
mach set "dk"
emulation RunFor "0.51"
sysbus ReadDoubleWord 0x50000504
emulation RunFor "0.50"
sysbus ReadDoubleWord 0x50000504
emulation RunFor "0.18"
gpio0.sw0 Press
emulation RunFor "0.01"
gpio0.sw0 Release
emulation RunFor "0.02"
sysbus ReadDoubleWord 0x50000504
emulation RunFor "0.30"
sysbus ReadDoubleWord 0x50000504
emulation RunFor "0.28"
gpio0.sw0 Press
emulation RunFor "0.01"
gpio0.sw0 Release
emulation RunFor "0.02"
sysbus ReadDoubleWord 0x50000504
emulation RunFor "0.20"
sysbus ReadDoubleWord 0x50000504
emulation RunFor "0.50"
sysbus ReadDoubleWord 0x50000504
quit
RESC

mapfile -t RAW < <(
  "$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 \
    | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}'
)
METAL=()
for v in "${RAW[@]}"; do
  if (( (v & 0x2000) != 0 )); then METAL+=("1"); else METAL+=("0"); fi
done
echo "metal LED sequence: ${METAL[*]}"

echo "== compare =="
if [[ "${SIM[*]}" == "${METAL[*]}" && ${#METAL[@]} -eq 7 ]]; then
  echo "PASS: metal LED sequence matches the simulator (sim ≡ metal)."
  exit 0
else
  echo "FAIL: sim=${SIM[*]} metal=${METAL[*]}"
  exit 1
fi
