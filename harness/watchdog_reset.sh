#!/usr/bin/env bash
# Phase-1 watchdog reset gate (§5.6): prove the generated metal idle loop feeds
# the hardware watchdog ONLY on a clean return to idle, so a wedged bus (a
# reaction stuck suspended) stops the feeds and the watchdog fires — exactly the
# scheduler-fed behaviour the simulator models, now on nRF52840 in Renode.
#
#   RENODE=/path/to/renode ./harness/watchdog_reset.sh
#
# Renode models a real nRF WDT (different registers) and no abstract bus
# controller, so the harness unregisters `wdt` + `twi0` and loads
# harness/MockWatchdog.cs (@0x4001_0000) and harness/MockBusController.cs
# (@0x4000_3000).  Two runs: a wedged bus (huge latency → never completes) must
# fire the watchdog; a healthy bus (completes each tick) must not.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/bus_watchdog_nrf52840.si}"
RENODE="${RENODE:-renode}"
ELFDIR="$(mktemp -d)"
ELF="$ELFDIR/metal.elf"
trap 'rm -rf "$ELFDIR"' EXIT

echo "== build metal firmware (${BUILD:-c} backend) =="
if [[ "${BUILD:-c}" == "llvm" ]]; then
  # Build ENTIRELY through the LLVM backend (audit P5-4).
  if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
  if [[ -d "$HOME/arm-gnu-toolchain-15.2/Payload/bin" ]]; then PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"; fi
  for t in llc arm-none-eabi-gcc; do command -v "$t" >/dev/null 2>&1 || { echo "SKIP: '$t' not found (need LLVM + ARM toolchain for BUILD=llvm)"; exit 0; }; done
  (cd "$REPO" && cargo run -q --bin silicac -- --target metal-nrf52840 --emit-llvm "$EX" -o "$ELFDIR/m")
  llc "$ELFDIR/m.ll" -filetype=obj -o "$ELFDIR/m.o" || { echo "FAIL: llc"; exit 1; }
  arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -nostdlib -nostartfiles -T "$ELFDIR/m.ld" "$ELFDIR/m.o" -o "$ELF" || { echo "FAIL: link"; exit 1; }
else
  (cd "$REPO" && cargo run -q -- --target metal-nrf52840 "$EX" -o "$ELF")
fi

# Run the firmware with the mock bus at a given latency (µs); return the mock
# watchdog's SR (0x4001_000C) = 1 iff it expired unfed.
run_scenario() {
  local latency_us="$1"
  local resc; resc="$(mktemp).resc"
  cat > "$resc" <<RESC
i @$REPO/harness/MockBusController.cs
i @$REPO/harness/MockWatchdog.cs
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
sysbus Unregister twi0
sysbus Unregister wdt
machine LoadPlatformDescriptionFromString "mockBus: Mocks.MockBusController @ sysbus 0x40003000 { IRQ -> nvic@8; LatencyMicroseconds: $latency_us }"
machine LoadPlatformDescriptionFromString "mockWdt: Mocks.MockWatchdog @ sysbus 0x40010000"
nvic Frequency 64000000
sysbus LoadELF @$ELF
mach set "dk"
emulation RunFor "0.3"
sysbus ReadDoubleWord 0x4001000C
quit
RESC
  local out
  out="$("$RENODE" --console --disable-xwt --plain -e "include @$resc" 2>&1 | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}' | tail -1)"
  rm -f "$resc"
  echo $(( out ))
}

echo "== wedged bus (latency 100s → transfer never completes) =="
WEDGED="$(run_scenario 100000000)"
echo "mock watchdog fired = $WEDGED  (expect 1: idle loop stopped feeding → reset)"

echo "== healthy bus (latency 1ms → completes each tick) =="
HEALTHY="$(run_scenario 1000)"
echo "mock watchdog fired = $HEALTHY  (expect 0: clean idle keeps feeding)"

echo "== compare =="
if [[ "$WEDGED" == "1" && "$HEALTHY" == "0" ]]; then
  echo "PASS: a wedged reaction starves the watchdog → reset; a healthy one is fed (§5.6)."
  exit 0
else
  echo "FAIL: expected wedged=1 healthy=0; got wedged=$WEDGED healthy=$HEALTHY"
  exit 1
fi
