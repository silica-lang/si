#!/usr/bin/env bash
# On-metal `within <d>` deadline gate (§4.5/§5.6): prove the generated firmware
# detects a reaction that overruns its declared deadline and latches the
# `__deadline_missed` flag that stops the watchdog feed (→ reset).  This is the
# DETECTION the deadline adds over the bare watchdog: it fires for a handler that
# is merely *too slow*, even though it would eventually complete.
#
#   RENODE=/path/to/renode ./harness/deadline_reset.sh
#
# Method: a sensor read yields on the bus; the mock bus is set to ~50ms latency.
# With `within 30ms` (< 50ms) the read overruns → `__deadline_missed` latches;
# the control, `within 80ms` (> 50ms), completes in time → it stays 0.  The flag
# is read straight from RAM via its symbol (no watchdog mock needed — the
# feed-stop → reset half is already covered by the §5.6 watchdog harnesses).
#
# Requires: cargo, arm-none-eabi-gcc/nm, and a Renode binary (set $RENODE).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/deadline_nrf52840.si}"
RENODE="${RENODE:-renode}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# Control: same program with a budget LARGER than the bus latency.
CTRL="$WORK/deadline_loose.si"
sed 's/within 30ms/within 80ms/' "$EX" > "$CTRL"
if ! grep -q 'within 80ms' "$CTRL"; then
  echo "FAIL: could not derive the loose-budget control from $EX"
  exit 1
fi

addr() { arm-none-eabi-nm "$1" | awk -v s="$2" '$3==s{print "0x"$1}'; }

# Build $1 → ELF, run ~0.4s on metal with a ~50ms mock bus, echo __deadline_missed.
run_missed() {
  local src="$1"
  local elf="$WORK/$(basename "$src" .si).elf"
  local resc="$WORK/run.resc"
  (cd "$REPO" && cargo run -q -- --target metal-nrf52840 "$src" -o "$elf")
  local flag; flag="$(addr "$elf" __deadline_missed)"
  if [[ -z "$flag" ]]; then
    echo "FAIL: no __deadline_missed symbol in $elf" >&2
    exit 1
  fi
  cat > "$resc" <<RESC
i @$REPO/harness/MockBusController.cs
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
sysbus Unregister twi0
sysbus Unregister wdt
machine LoadPlatformDescriptionFromString "mockBus: Mocks.MockBusController @ sysbus 0x40003000 { IRQ -> nvic@8; LatencyMicroseconds: 50000 }"
nvic Frequency 64000000
sysbus LoadELF @$elf
mach set "dk"
emulation RunFor "0.4"
sysbus ReadDoubleWord $flag
quit
RESC
  local out
  out="$("$RENODE" --console --disable-xwt --plain -e "include @$resc" 2>&1 \
        | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}' | tail -1)"
  echo $(( out ))
}

echo "== build + run: within 30ms (< 50ms bus → overruns) =="
TIGHT="$(run_missed "$EX")"
echo "tight budget:  __deadline_missed = $TIGHT  (expect 1: the read overran its 30ms budget)"

echo "== build + run: within 80ms (> 50ms bus → completes in time) =="
LOOSE="$(run_missed "$CTRL")"
echo "loose budget:  __deadline_missed = $LOOSE  (expect 0: completed within budget)"

echo "== compare =="
if (( TIGHT == 1 && LOOSE == 0 )); then
  echo 'PASS: the metal firmware detects a within-deadline overrun (latches __deadline_missed → stops the watchdog feed, §4.5/§5.6).'
  exit 0
else
  echo "FAIL: expected tight=1 loose=0; got tight=$TIGHT loose=$LOOSE"
  exit 1
fi
