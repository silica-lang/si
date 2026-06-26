#!/usr/bin/env bash
# Flash / code-size budget gate (§5.3 / audit #35, P1-3): prove that a metal
# build (a) REPORTS its flash usage (.text+.rodata+.data vs the flash region) —
# the cost-visibility the RAM gate has long had — and (b) is REJECTED when the
# code does not fit the flash region, rather than producing unfittable firmware.
#
#   ./harness/flash_budget.sh
#
# Build-level gate (no Renode). Requires cargo + arm-none-eabi-gcc/size.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
OK="$REPO/examples/blink_button_nrf52840.si"
OVER="$REPO/examples/flash_over_budget_nrf52840.si"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
cd "$REPO"

echo "== healthy program: build succeeds and reports a flash budget =="
ok_log="$WORK/ok.log"
if ! cargo run -q -- --target metal-nrf52840 "$OK" -o "$WORK/ok.elf" 2>"$ok_log"; then
  echo "FAIL: healthy program did not build"; cat "$ok_log"; exit 1
fi
cat "$ok_log"
if ! grep -q "flash budget" "$ok_log"; then
  echo "FAIL: no flash budget reported for the healthy build"; exit 1
fi
echo "PASS: flash budget reported."

echo "== oversized program: build must FAIL (code does not fit flash) =="
over_log="$WORK/over.log"
if cargo run -q -- --target metal-nrf52840 "$OVER" -o "$WORK/over.elf" 2>"$over_log"; then
  echo "FAIL: oversized program built (expected a flash rejection)"; cat "$over_log"; exit 1
fi
cat "$over_log"
# The linker rejects a region overflow first; the silicac flash gate is the
# clean-message backstop.  Either way the build must fail and leave no ELF.
if grep -qiE "overflow|flash budget exceeded|region .* overflowed" "$over_log"; then
  echo "PASS: oversized program rejected (flash region overflow)."
else
  echo "FAIL: build failed but not with a flash-overflow error"; exit 1
fi
if [[ -e "$WORK/over.elf" ]]; then
  echo "FAIL: over-budget build left an output ELF behind"; exit 1
fi
echo "PASS: no firmware emitted for the oversized program."

echo "PASS: flash budget gate (P1-3)."
