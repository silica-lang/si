#!/usr/bin/env bash
# Metal semihosting gate (§7.1, audit P6-7): prove `host_io.print` on metal emits
# ARM semihosting (BKPT 0xAB / SYS_WRITE0) that Renode captures, and that the
# captured stream equals the simulator's stdout.  Renode has no `EnableSemihosting`
# toggle — the mechanism is attaching `UART.SemihostingUart @ cpu` and a file
# backend (verified).  Built through the LLVM backend by default; `BUILD=c` mirrors.
#
#   RENODE=/path/to/renode ./harness/semihosting.sh
#   BUILD=c RENODE=… ./harness/semihosting.sh
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/semihosting_nrf52840.si}"
RENODE="${RENODE:-renode}"
WORK="$(mktemp -d)"; RESC="$(mktemp).resc"; OUT="$WORK/semi.txt"
ELF="$WORK/sh.elf"
trap 'rm -rf "$WORK" "$RESC"' EXIT
if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
if [[ -d "$HOME/arm-gnu-toolchain-15.2/Payload/bin" ]]; then PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"; fi
command -v "$RENODE" >/dev/null 2>&1 || [[ -x "$RENODE" ]] || { echo "SKIP: renode not found"; exit 0; }

cd "$REPO"
echo "== reference: simulator stdout =="
SIM="$(cargo run -q --bin silicac -- --sim "$EX" 2>&1 | sed -n 's/.*print "\(.*\)"/\1/p' | tr -d '\n' | sed 's/\\n/\n/g')"
printf 'sim stdout:\n%s\n' "$SIM" | sed 's/^/  | /'

echo "== build metal firmware (${BUILD:-llvm} backend) =="
if [[ "${BUILD:-llvm}" == "c" ]]; then
  command -v arm-none-eabi-gcc >/dev/null 2>&1 || { echo "SKIP: arm-none-eabi-gcc not found"; exit 0; }
  cargo run -q -- --target metal-nrf52840 "$EX" -o "$ELF"
else
  for t in llc arm-none-eabi-gcc; do command -v "$t" >/dev/null 2>&1 || { echo "SKIP: '$t' not found (need LLVM + ARM toolchain)"; exit 0; }; done
  cargo run -q --bin silicac -- --target metal-nrf52840 --emit-llvm "$EX" -o "$WORK/sh" 2>"$WORK/emit.log" || { echo "FAIL: --emit-llvm"; cat "$WORK/emit.log"; exit 1; }
  grep -q 'bkpt #0xab' "$WORK/sh.ll" || { echo "FAIL: no semihosting BKPT in IR"; exit 1; }
  llc "$WORK/sh.ll" -filetype=obj -o "$WORK/sh.o" 2>"$WORK/llc.log" || { echo "FAIL: llc"; cat "$WORK/llc.log"; exit 1; }
  arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -nostdlib -nostartfiles -T "$WORK/sh.ld" "$WORK/sh.o" -o "$ELF" 2>"$WORK/link.log" || { echo "FAIL: link"; cat "$WORK/link.log"; exit 1; }
fi

echo "== run on metal (Renode, SemihostingUart → file) =="
# Attach a SemihostingUart to the CPU (the nRF repl declares none) and capture it
# to a file with immediate flush, so the output is on disk before quit.
cat > "$RESC" <<RESC
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
machine LoadPlatformDescriptionFromString "uartSemihosting: UART.SemihostingUart @ cpu"
cpu.uartSemihosting CreateFileBackend @$OUT true
nvic Frequency 64000000
sysbus LoadELF @$ELF
mach set "dk"
emulation RunFor "0.35"
quit
RESC
timeout 150 "$RENODE" --console --disable-xwt --plain -e "include @$RESC" >/dev/null 2>&1 || true
[[ -f "$OUT" ]] || { echo "FAIL: no semihosting output file (capture failed)"; exit 1; }
CAP="$(tr -d '\r' < "$OUT")"
printf 'metal semihosting:\n%s\n' "$CAP" | sed 's/^/  | /'

echo "== compare =="
ok=1
for tok in "n=1" "n=2" "n=3"; do
  echo "$CAP" | grep -qx "$tok" || ok=0
done
# The captured stream must equal the sim stdout (modulo a trailing newline).
if (( ok )) && [[ "$(printf '%s' "$CAP")" == "$(printf '%s' "$SIM")" ]]; then
  echo "PASS: metal semihosting output captured by Renode equals the sim stdout (sim ≡ metal, P6-7)."
  exit 0
else
  echo "FAIL: sim=[$(printf '%s' "$SIM" | tr '\n' '/')] metal=[$(printf '%s' "$CAP" | tr '\n' '/')]"
  exit 1
fi
