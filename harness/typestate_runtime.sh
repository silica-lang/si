#!/usr/bin/env bash
# Runtime typestate precondition gate (§4.1/D07, audit P3-3).  A `when ready` op
# whose state is established in *another* reaction lowers a RUNTIME guard on the
# device's live state cell — a mismatch drives the safe state (§5.6) — instead of
# a conservative compile error.  This proves the same SIR drives both consumers
# to the same behaviour:
#   - NOT armed  → the periodic read's guard fires → safe state → ticks frozen;
#   - armed first (button press → power_on) → the read passes → ticks climb.
#
#   RENODE=/path/to/renode ./harness/typestate_runtime.sh
#
# Requires: cargo, arm-none-eabi-gcc/nm, Renode.  The custom `thermostat` device
# at 0x4000_5000 is backed by a plain RAM region in Renode (its MMIO is incidental
# — the guard is on the device's state cell in RAM).
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="$REPO/examples/typestate_runtime.si"
RENODE="${RENODE:-renode}"
ELFDIR="$(mktemp -d)"
ELF="$ELFDIR/metal.elf"
RESC="$(mktemp).resc"
trap 'rm -rf "$ELFDIR" "$RESC"' EXIT

echo "== reference: host simulator (button @50ms arms the device → reads pass) =="
SIM_TRACE="$(cd "$REPO" && cargo run -q --bin silicac -- --sim "$EX")"
echo "$SIM_TRACE" | sed -n 's/^/  sim| /p' | tail -6
if ! echo "$SIM_TRACE" | grep -q "cell ticks = 3"; then
  echo "FAIL(sim): expected the armed read to reach ticks=3"; exit 1
fi
echo "sim: armed read passes (ticks=3) ✓"

echo "== build metal firmware =="
(cd "$REPO" && cargo run -q --bin silicac -- --target metal-nrf52840 "$EX" -o "$ELF")
addr() { arm-none-eabi-nm "$ELF" | awk -v s="$1" '$3==s{print "0x"$1}'; }
TICKS="$(addr ticks)"
[[ -n "$TICKS" ]] || { echo "FAIL: no 'ticks' symbol in $ELF"; exit 1; }
echo "cell: ticks=$TICKS"

echo "== run on metal (Renode) — NOT armed, then armed (fresh boots) =="
# The thermostat's MMIO is backed by plain RAM (the guard reads a state cell in
# RAM, not the device).  Each case is a fresh boot — the unarmed case halts in
# the safe state (interrupts off), so we don't reuse the machine across cases.
run_case() {  # $1 = extra monitor lines (button press), echoes ticks
  local extra="$1"
  cat > "$RESC" <<RESC
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
machine LoadPlatformDescriptionFromString "therm: Memory.MappedMemory @ sysbus 0x40005000 { size: 0x1000 }"
nvic Frequency 64000000
sysbus LoadELF @$ELF
mach set "dk"
$extra
sysbus ReadDoubleWord $TICKS
quit
RESC
  "$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 \
    | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}' | head -1
}

NOT_ARMED=$(( $(run_case 'emulation RunFor "0.3500"') ))
ARMED=$(( $(run_case 'emulation RunFor "0.0400"
gpio0.sw0 Press
emulation RunFor "0.0050"
gpio0.sw0 Release
emulation RunFor "0.3100"') ))
echo "metal: not-armed ticks=$NOT_ARMED ; armed ticks=$ARMED"

echo "== compare =="
# Not armed: the guard fires on the first read → safe state → ticks frozen at 0.
# Armed (button pressed first): the read passes each period → ticks climb.
if (( NOT_ARMED == 0 && ARMED >= 1 )); then
  echo "PASS: the runtime typestate guard drove safe when unarmed (ticks=0) and the op ran once armed (ticks=$ARMED) — sim/metal parity (P3-3)."
  exit 0
else
  echo "FAIL: expected not-armed=0 and armed>=1; got not-armed=$NOT_ARMED armed=$ARMED"; exit 1
fi
