#!/usr/bin/env bash
# Yielding-reaction multi-fire re-arm gate (§5.2, audit P3-1).  A yielding `every`
# reaction must fire REPEATEDLY on metal: each periodic activation kicks a bus
# transaction, suspends, resumes on the completion IRQ, and is ready to fire
# again.  Before P3-1 this fired only ONCE — the completion IRQ line is level, so
# a stale NVIC pending from the previous transaction was re-taken spuriously at
# the next kick (resuming before the new transfer completed); the fix clears the
# bus IRQ pending at each kick.  This gate proves N consecutive fires all land.
#
#   RENODE=/path/to/renode ./harness/bus_refire.sh
#
# Requires: cargo, arm-none-eabi-gcc/nm, Renode. Loads harness/MockBusController.cs
# @ 0x4000_3000, IRQ→NVIC#8.  Uses examples/fault_match.si (every 1000ms bus read).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="$REPO/examples/fault_match.si"
RENODE="${RENODE:-renode}"
ELFDIR="$(mktemp -d)"
ELF="$ELFDIR/metal.elf"
RESC="$(mktemp).resc"
trap 'rm -rf "$ELFDIR" "$RESC"' EXIT

echo "== reference: host simulator fires the reaction repeatedly =="
SIM_TRACE="$(cd "$REPO" && cargo run -q -- --sim "$EX")"
FIRES="$(echo "$SIM_TRACE" | grep -c 'fire reaction#0')"
echo "sim: reaction fired $FIRES times"
if (( FIRES < 3 )); then
  echo "FAIL: sim fired the reaction <3 times (expected the periodic re-fire oracle)"; exit 1
fi
echo "sim: periodic reaction re-fires ✓"

echo "== build metal firmware =="
(cd "$REPO" && cargo run -q -- --target metal-nrf52840 "$EX" -o "$ELF")
addr() { arm-none-eabi-nm "$ELF" | awk -v s="$1" '$3==s{print "0x"$1}'; }
READS="$(addr reads)"
[[ -n "$READS" ]] || { echo "FAIL: no 'reads' symbol in $ELF"; exit 1; }
echo "cell: reads=$READS"

echo "== run on metal (3 ok fires in ONE boot; expect reads == 3) =="
# All-ok bus completions; the reaction fires at 1000/2000/3000ms and must re-arm
# each time.  Run to 3.5s and read the success counter back.
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
emulation RunFor "3.5000"
sysbus ReadDoubleWord $READS
quit
RESC

mapfile -t RAW < <(
  "$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 \
    | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}'
)
if [[ ${#RAW[@]} -ne 1 ]]; then
  echo "FAIL: expected 1 memory sample from Renode, got ${#RAW[@]}: ${RAW[*]:-<none>}"; exit 1
fi
READS_N=$(( RAW[0] ))
echo "metal: reads=$READS_N after 3 fires"

echo "== compare =="
if (( READS_N == 3 )); then
  echo "PASS: the yielding reaction re-fired 3× on metal (multi-fire re-arm fixed, P3-1)."
  exit 0
else
  echo "FAIL: expected reads=3 (one per fire); got $READS_N — the reaction did not re-fire"; exit 1
fi
