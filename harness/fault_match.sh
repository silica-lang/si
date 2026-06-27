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
# Since the P3-1 multi-fire re-arm fix, the metal side drives all FOUR outcomes
# across four consecutive fires in ONE boot (nak → timeout → arblost → ok),
# mirroring the sim's injected sequence — the stronger parity check P2-4 had to
# sidestep (fresh boot per outcome) when a yielding `every` reaction could fire
# only once on metal.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
# Default: the leaf-bus-op example (P2-4); pass a path to validate another (e.g.
# examples/fault_match_composed.si — the composed-op form, P3-2).  Both expose
# the same reads/naks/timeouts/arblosts cells.
EX="${1:-$REPO/examples/fault_match.si}"
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
# ONE boot, FOUR consecutive fires (the reaction re-arms each time since P3-1):
# set the mock's FaultBits between fires so fire 1 NAKs (0x2), fire 2 times out
# (0x8), fire 3 arbitration-losts (0x4), fire 4 succeeds (0x0).  Each fires at
# 1000/2000/3000/4000ms and completes 5ms later.  Read all four cells at the end:
# every arm must have fired exactly once.
cat > "$RESC" <<RESC
i @$REPO/harness/MockBusController.cs
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
sysbus Unregister twi0
machine LoadPlatformDescriptionFromString "mockBus: Mocks.MockBusController @ sysbus 0x40003000 { IRQ -> nvic@8 }"
nvic Frequency 64000000
sysbus LoadELF @$ELF
mach set "dk"
mockBus FaultBits 2
emulation RunFor "1.0100"
mockBus FaultBits 8
emulation RunFor "1.0000"
mockBus FaultBits 4
emulation RunFor "1.0000"
mockBus FaultBits 0
emulation RunFor "1.0000"
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
if [[ ${#RAW[@]} -ne 4 ]]; then
  echo "FAIL: expected 4 memory samples from Renode, got ${#RAW[@]}: ${RAW[*]:-<none>}"; exit 1
fi
R_N=$(( RAW[0] )); N_N=$(( RAW[1] )); T_N=$(( RAW[2] )); A_N=$(( RAW[3] ))
echo "metal (4 fires, one boot): reads=$R_N naks=$N_N timeouts=$T_N arblosts=$A_N"

echo "== compare =="
# Every declared fault code + ok dispatched to its own arm exactly once, across
# four re-fires in a single boot.
if (( R_N == 1 && N_N == 1 && T_N == 1 && A_N == 1 )); then
  echo "PASS: metal decoded ok + every declared fault code to the same arm the sim did, across re-fires (match-over-fault-codes parity)."
  exit 0
else
  echo "FAIL: expected reads=1 naks=1 timeouts=1 arblosts=1; got reads=$R_N naks=$N_N timeouts=$T_N arblosts=$A_N"; exit 1
fi
