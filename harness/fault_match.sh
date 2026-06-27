#!/usr/bin/env bash
# `match` over an op's fault codes — sim + metal parity gate (§4.4/D14, audit
# #35 P2-4).  A bus op declares `-> u32 or fault{nak, timeout, arblost}`; a
# `match` over its *result* dispatches each outcome to its own arm.  This proves
# the SAME SIR drives both consumers to the SAME arm for EVERY declared code:
#   - sim: an injected `bus_fault <code>` maps to the matching arm;
#   - metal: the resumed transaction decodes the controller's SR error bits
#     (nak=0x2, arblost=0x4, timeout=0x8) into the same arm.
#
#   RENODE=/path/to/renode ./harness/fault_match.sh
#
# Requires: cargo, arm-none-eabi-gcc/nm, and a Renode binary (set $RENODE or have
# `renode` on PATH).  Loads harness/MockBusController.cs @ 0x4000_3000, IRQ→NVIC#8.
#
# Each metal outcome is exercised on a FRESH boot (the reaction's first fire),
# driving one outcome via the mock's FaultBits and reading the cells back.  This
# isolates exactly what P2-4 adds — the SR→fault-code decode + match dispatch —
# from the (orthogonal, pre-existing) re-fire path of a yielding `every`
# reaction, which is not what this gate measures.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="$REPO/examples/fault_match.si"
RENODE="${RENODE:-renode}"
ELFDIR="$(mktemp -d)"
ELF="$ELFDIR/metal.elf"
RESC="$(mktemp).resc"
trap 'rm -rf "$ELFDIR" "$RESC"' EXIT

echo "== reference: host simulator (inject nak, timeout; then ok) =="
SIM_TRACE="$(cd "$REPO" && cargo run -q -- --sim "$EX")"
echo "$SIM_TRACE" | sed -n 's/^/  sim| /p'
sim_ok=1
echo "$SIM_TRACE" | grep -q "cell naks = 1"     || { echo "FAIL(sim): nak arm did not fire"; sim_ok=0; }
echo "$SIM_TRACE" | grep -q "cell timeouts = 1" || { echo "FAIL(sim): timeout arm did not fire"; sim_ok=0; }
echo "$SIM_TRACE" | grep -q "cell reads = 1"    || { echo "FAIL(sim): ok arm did not fire"; sim_ok=0; }
(( sim_ok == 1 )) || exit 1
echo "sim: ok / nak / timeout each dispatched to its own arm ✓"

echo "== build metal firmware =="
(cd "$REPO" && cargo run -q -- --target metal-nrf52840 "$EX" -o "$ELF")

addr() { arm-none-eabi-nm "$ELF" | awk -v s="$1" '$3==s{print "0x"$1}'; }
R="$(addr reads)"; N="$(addr naks)"; T="$(addr timeouts)"; A="$(addr arblosts)"
if [[ -z "$R" || -z "$N" || -z "$T" || -z "$A" ]]; then
  echo "FAIL: could not find reads/naks/timeouts/arblosts symbols in $ELF"; exit 1
fi
echo "cells: reads=$R naks=$N timeouts=$T arblosts=$A"

echo "== run on metal (Renode, mock bus @ 0x4000_3000, IRQ→NVIC#8) =="
# Each block: machine Reset → set the mock's FaultBits → run past the first fire
# (1000ms) + bus latency (5ms) → dump all four cells.  The expected arm must read
# 1 and the other three 0 (no cross-talk).
cat > "$RESC" <<RESC
i @$REPO/harness/MockBusController.cs
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
sysbus Unregister twi0
machine LoadPlatformDescriptionFromString "mockBus: Mocks.MockBusController @ sysbus 0x40003000 { IRQ -> nvic@8 }"
nvic Frequency 64000000
sysbus LoadELF @$ELF
mach set "dk"
mockBus FaultBits 0
emulation RunFor "1.0100"
sysbus ReadDoubleWord $R
sysbus ReadDoubleWord $N
sysbus ReadDoubleWord $T
sysbus ReadDoubleWord $A
machine Reset
mockBus FaultBits 2
emulation RunFor "1.0100"
sysbus ReadDoubleWord $R
sysbus ReadDoubleWord $N
sysbus ReadDoubleWord $T
sysbus ReadDoubleWord $A
machine Reset
mockBus FaultBits 8
emulation RunFor "1.0100"
sysbus ReadDoubleWord $R
sysbus ReadDoubleWord $N
sysbus ReadDoubleWord $T
sysbus ReadDoubleWord $A
machine Reset
mockBus FaultBits 4
emulation RunFor "1.0100"
sysbus ReadDoubleWord $R
sysbus ReadDoubleWord $N
sysbus ReadDoubleWord $T
sysbus ReadDoubleWord $A
quit
RESC

mapfile -t RAW < <(
  "$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 \
    | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}'
)
if [[ ${#RAW[@]} -ne 16 ]]; then
  echo "FAIL: expected 16 memory samples from Renode (4 cells × 4 outcomes), got ${#RAW[@]}: ${RAW[*]:-<none>}"; exit 1
fi

# Each row is (reads, naks, timeouts, arblosts) for one driven outcome.
# Expected identity matrix: ok→reads, nak→naks, timeout→timeouts, arblost→arblosts.
declare -a NAMES=(ok nak timeout arblost)
declare -a EXPECT=("1 0 0 0" "0 1 0 0" "0 0 1 0" "0 0 0 1")
fail=0
for i in 0 1 2 3; do
  base=$(( i * 4 ))
  got="$(( RAW[base] )) $(( RAW[base+1] )) $(( RAW[base+2] )) $(( RAW[base+3] ))"
  echo "metal ${NAMES[$i]}-outcome: reads/naks/timeouts/arblosts = $got"
  if [[ "$got" != "${EXPECT[$i]}" ]]; then
    echo "  FAIL: expected ${EXPECT[$i]} (only the ${NAMES[$i]} arm should fire)"; fail=1
  fi
done

echo "== compare =="
if (( fail == 0 )); then
  echo "PASS: metal decoded ok + every declared fault code to the same arm the sim did (match-over-fault-codes parity)."
  exit 0
else
  echo "FAIL: metal fault-code dispatch did not match the expected per-outcome arms."; exit 1
fi
