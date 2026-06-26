#!/usr/bin/env bash
# Phase-1 bus trace-order parity gate: prove a yielding I²C transaction really
# SUSPENDS the handler on metal, so a higher-priority reaction runs DURING the
# bus window — the §5.2 interleaving the simulator models, now observable on
# nRF52840 hardware (Renode).  This is what an IRQ-driven yields lowering (D2)
# buys over the old busy-poll, which could not interleave.
#
#   RENODE=/path/to/renode ./harness/bus_parity.sh
#
# Requires: cargo, arm-none-eabi-gcc/nm, and a Renode binary (set $RENODE or have
# `renode` on PATH).  Loads harness/MockBusController.cs as the bus controller at
# 0x4000_3000 (Renode does not model the abstract controller) and wires its
# completion IRQ to NVIC IRQ 8 (= __BUS_IRQN).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/bus_interleave_nrf52840.si}"
RENODE="${RENODE:-renode}"
ELFDIR="$(mktemp -d)"
ELF="$ELFDIR/metal.elf"
RESC="$(mktemp).resc"
trap 'rm -rf "$ELFDIR" "$RESC"' EXIT

echo "== reference: host simulator =="
# The sim trace shows the button (hits) firing DURING the sensor's bus suspension,
# before the sensor resumes (samples).  Confirm that ordering as the oracle.
SIM_TRACE="$(cd "$REPO" && cargo run -q -- --sim "$EX")"
echo "$SIM_TRACE" | sed -n 's/^/  sim| /p'
if ! echo "$SIM_TRACE" | awk '/cell hits = 1/{h=NR} /cell samples = 1/{s=NR} END{exit !(h && s && h < s)}'; then
  echo "FAIL: sim did not show the button (hits) running before the sensor resume (samples)"
  exit 1
fi
echo "sim: button interleaves during the bus suspension (hits before samples) ✓"

echo "== build metal firmware =="
(cd "$REPO" && cargo run -q -- --target metal-nrf52840 "$EX" -o "$ELF")

# Cell addresses (in .bss) — read them from the ELF so the check is layout-robust.
addr() { arm-none-eabi-nm "$ELF" | awk -v s="$1" '$3==s{print "0x"$1}'; }
HITS="$(addr hits)"
SAMPLES="$(addr samples)"
if [[ -z "$HITS" || -z "$SAMPLES" ]]; then
  echo "FAIL: could not find hits/samples symbols in $ELF"
  exit 1
fi
echo "cells: hits=$HITS samples=$SAMPLES"

echo "== run on metal (Renode, mock bus controller @ 0x4000_3000, IRQ→NVIC#8) =="
# Timeline (SysTick/NVIC pinned to 64MHz so 1ms ticks are real; bus latency 5ms):
#  - run to ~1001ms: the sensor fires at 1000ms, kicks the bus, and SUSPENDS
#    (the mock completes 5ms later, at ~1005ms);
#  - inside the window (~1001ms) inject the button and let it run; sample at
#    ~1002.5ms — a higher-priority reaction ran while the sensor is suspended,
#    so expect hits=1, samples=0;
#  - past the window (~1008ms) the bus completes and the sensor resumes; sample:
#    expect hits=1, samples=1.
cat > "$RESC" <<RESC
i @$REPO/harness/MockBusController.cs
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
sysbus Unregister twi0
machine LoadPlatformDescriptionFromString "mockBus: Mocks.MockBusController @ sysbus 0x40003000 { IRQ -> nvic@8 }"
nvic Frequency 64000000
sysbus LoadELF @$ELF
mach set "dk"
emulation RunFor "1.0010"
gpio0.sw0 Press
emulation RunFor "0.0005"
gpio0.sw0 Release
emulation RunFor "0.0010"
sysbus ReadDoubleWord $HITS
sysbus ReadDoubleWord $SAMPLES
emulation RunFor "0.0060"
sysbus ReadDoubleWord $HITS
sysbus ReadDoubleWord $SAMPLES
quit
RESC

mapfile -t RAW < <(
  "$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 \
    | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}'
)
if [[ ${#RAW[@]} -ne 4 ]]; then
  echo "FAIL: expected 4 memory samples from Renode, got ${#RAW[@]}: ${RAW[*]:-<none>}"
  exit 1
fi
HITS_MID=$(( RAW[0] )); SAMP_MID=$(( RAW[1] ))
HITS_END=$(( RAW[2] )); SAMP_END=$(( RAW[3] ))
echo "mid-window:  hits=$HITS_MID samples=$SAMP_MID"
echo "post-window: hits=$HITS_END samples=$SAMP_END"

echo "== compare =="
# The decisive condition: mid-window the button has run (hits=1) while the sensor
# is still suspended (samples=0).  That ordering is impossible under a busy-poll.
if (( HITS_MID == 1 && SAMP_MID == 0 && HITS_END == 1 && SAMP_END == 1 )); then
  echo "PASS: the button reaction ran DURING the sensor's bus suspension (trace-order parity with sim)."
  exit 0
else
  echo "FAIL: expected mid(hits=1,samples=0) end(hits=1,samples=1); got mid($HITS_MID,$SAMP_MID) end($HITS_END,$SAMP_END)"
  exit 1
fi
