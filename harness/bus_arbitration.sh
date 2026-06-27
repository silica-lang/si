#!/usr/bin/env bash
# Multi-consumer bus arbitration gate (§5.2, audit P6-9): prove two reactions
# sharing one I²C controller are correctly serialized — BOTH reads complete and
# the bus is granted priority-first.  Under the old single-owner model the second
# kick clobbers the in-flight owner and the first read is LOST.
#
#   RENODE=… ./harness/bus_arbitration.sh            # C backend
#   BUILD=llvm RENODE=… ./harness/bus_arbitration.sh  # LLVM backend
#
# A low-priority `every` (env_a → cell `sa`) owns the bus first; a high-priority
# button (env_b → cell `sb`) contends mid-transfer, waits, and is granted the bus
# the instant the first transfer completes.  Both `sa` and `sb` reach 1.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/bus_contend_nrf52840.si}"
RENODE="${RENODE:-renode}"
WORK="$(mktemp -d)"; RESC="$(mktemp).resc"; ELF="$WORK/bc.elf"
trap 'rm -rf "$WORK" "$RESC"' EXIT
if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
if [[ -d "$HOME/arm-gnu-toolchain-15.2/Payload/bin" ]]; then PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"; fi
command -v "$RENODE" >/dev/null 2>&1 || [[ -x "$RENODE" ]] || { echo "SKIP: renode not found"; exit 0; }

cd "$REPO"
echo "== reference: host simulator (both reads complete, button granted on free) =="
SIM="$(cargo run -q --bin silicac -- --sim "$EX")"
echo "$SIM" | sed -n 's/.*\(BLOCKED.*\|GRANTED.*\|cell sa = .*\|cell sb = .*\)/  sim| \1/p' | head
S_SA="$(echo "$SIM" | sed -n 's/.*cell sa = \([0-9]*\).*/\1/p' | tail -1)"
S_SB="$(echo "$SIM" | sed -n 's/.*cell sb = \([0-9]*\).*/\1/p' | tail -1)"
[[ "$S_SA" == "1" && "$S_SB" == "1" ]] || { echo "FAIL: sim did not complete both reads (sa=$S_SA sb=$S_SB)"; exit 1; }

echo "== build metal firmware (${BUILD:-c} backend) =="
if [[ "${BUILD:-c}" == "llvm" ]]; then
  for t in llc arm-none-eabi-gcc; do command -v "$t" >/dev/null 2>&1 || { echo "SKIP: '$t' not found"; exit 0; }; done
  cargo run -q --bin silicac -- --target metal-nrf52840 --emit-llvm "$EX" -o "$WORK/bc" 2>"$WORK/emit.log" || { echo "FAIL: --emit-llvm"; cat "$WORK/emit.log"; exit 1; }
  llc "$WORK/bc.ll" -filetype=obj -o "$WORK/bc.o" 2>"$WORK/llc.log" || { echo "FAIL: llc"; cat "$WORK/llc.log"; exit 1; }
  arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -nostdlib -nostartfiles -T "$WORK/bc.ld" "$WORK/bc.o" -o "$ELF" 2>"$WORK/link.log" || { echo "FAIL: link"; cat "$WORK/link.log"; exit 1; }
else
  command -v arm-none-eabi-gcc >/dev/null 2>&1 || { echo "SKIP: arm-none-eabi-gcc not found"; exit 0; }
  cargo run -q -- --target metal-nrf52840 "$EX" -o "$ELF"
fi

addr() { arm-none-eabi-nm "$ELF" | awk -v s="$1" '$3==s{print "0x"$1}'; }
SA="$(addr sa)"; SB="$(addr sb)"
[[ -n "$SA" && -n "$SB" ]] || { echo "FAIL: missing sa/sb symbols"; exit 1; }

echo "== run on metal (Renode, mock bus @ 0x4000_3000, IRQ→NVIC#8) =="
# env_a fires at 1000ms, claims the bus, suspends (mock 5ms latency → done 1005ms).
# Inject the button at ~1001ms (contends → waits).  mid (~1002ms): both pending
# (sa=0, sb=0).  After env_a completes (1005ms) the bus is granted to the button,
# which completes ~1010ms.  post (~1011ms): both done (sa=1, sb=1).
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
sysbus ReadDoubleWord $SA
sysbus ReadDoubleWord $SB
emulation RunFor "0.0100"
sysbus ReadDoubleWord $SA
sysbus ReadDoubleWord $SB
quit
RESC
mapfile -t RAW < <("$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}')
(( ${#RAW[@]} == 4 )) || { echo "FAIL: expected 4 samples, got ${#RAW[@]}: ${RAW[*]:-<none>}"; exit 1; }
SA_MID=$(( RAW[0] )); SB_MID=$(( RAW[1] )); SA_END=$(( RAW[2] )); SB_END=$(( RAW[3] ))
echo "mid-window:  sa=$SA_MID sb=$SB_MID  (both pending: 0 0)"
echo "post-window: sa=$SA_END sb=$SB_END  (both complete: 1 1)"

echo "== compare =="
# Decisive: BOTH reads complete (sa=1 AND sb=1).  The old single-owner model would
# clobber env_a's in-flight transfer when the button kicks → sa stuck at 0.
if (( SA_MID == 0 && SB_MID == 0 && SA_END == 1 && SB_END == 1 )); then
  echo "PASS: two reactions sharing one bus are serialized — both reads complete, button granted on free (sim ≡ metal, P6-9)."
  exit 0
else
  echo "FAIL: expected mid(0,0) end(1,1); got mid($SA_MID,$SB_MID) end($SA_END,$SB_END)"
  exit 1
fi
