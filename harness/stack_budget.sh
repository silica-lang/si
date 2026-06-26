#!/usr/bin/env bash
# Measured RAM-budget gate (§5.3 / audit #35, P0-1b): prove that the metal build
# (a) reports a *measured* worst-case stack budget folded from the toolchain's
# own frame accounting (-fcallgraph-info/-fstack-usage), and (b) HARD-ERRORS
# when statics + worst-case stack exceed the chip's RAM region — rather than
# emitting firmware that would smash the stack at runtime.
#
#   ./harness/stack_budget.sh
#
# This is a build-level gate (no Renode): the guarantee is compile-time. The
# on-metal *behaviour* is unchanged and is covered by harness/metal_vs_sim.sh.
# Requires: cargo + arm-none-eabi-gcc.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
OK="$REPO/examples/blink_button_nrf52840.si"
OVER="$REPO/examples/stack_over_budget_nrf52840.si"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$REPO"

echo "== healthy program: build must succeed and report a measured budget =="
ok_log="$WORK/ok.log"
if ! cargo run -q -- --target metal-nrf52840 "$OK" -o "$WORK/ok.elf" 2>"$ok_log"; then
  echo "FAIL: healthy program did not build"; cat "$ok_log"; exit 1
fi
cat "$ok_log"
if ! grep -q "RAM budget (measured)" "$ok_log"; then
  echo "FAIL: no measured RAM budget reported for the healthy build"; exit 1
fi
echo "PASS: measured budget reported."

echo "== oversized program: build must FAIL with a RAM-budget error =="
over_log="$WORK/over.log"
if cargo run -q -- --target metal-nrf52840 "$OVER" -o "$WORK/over.elf" 2>"$over_log"; then
  echo "FAIL: oversized program built (expected a RAM-budget rejection)"; cat "$over_log"; exit 1
fi
cat "$over_log"
if ! grep -q "RAM budget exceeded" "$over_log"; then
  echo "FAIL: build failed but not with the expected RAM-budget error"; exit 1
fi
# And it must not leave a stale ELF behind.
if [[ -e "$WORK/over.elf" ]]; then
  echo "FAIL: over-budget build left an output ELF behind"; exit 1
fi
echo "PASS: oversized program rejected at compile time (no firmware emitted)."

echo "PASS: measured RAM-budget gate (P0-1b)."
