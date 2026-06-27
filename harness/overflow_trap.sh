#!/usr/bin/env bash
# Overflow-trap-by-default gate (§4.3 / SIL-004): prove that a plain `+` whose
# result does not fit the target type TRAPS on metal — the generated helper
# drives the system to its safe state and halts — rather than silently wrapping.
#
#   RENODE=/path/to/renode ./harness/overflow_trap.sh
#
# Method: build two firmwares from the same program — the trapping original
# (`acc + 85`, u8) and a wrapping control (`acc +% 85`).  Both advance a `ticks`
# counter every 100ms.  The u8 accumulator overflows on the 4th tick: the trap
# build halts there (ticks frozen ≈4); the wrapping build runs every tick
# (ticks keeps climbing).  The gap is the proof the default `+` trapped.
#
# Requires: cargo, arm-none-eabi-gcc/nm, and a Renode binary (set $RENODE or have
# `renode` on PATH).  The program declares no peripherals, so nothing is mocked.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="${1:-$REPO/examples/overflow_nrf52840.si}"
RENODE="${RENODE:-renode}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT

# BUILD=llvm (audit P5-2): build the firmware ENTIRELY through the LLVM backend
# (--emit-llvm → llc → link) instead of the C backend, to prove the overflow trap
# drives safe + halts on the LLVM path too.  Requires LLVM + the ARM toolchain.
if [[ "${BUILD:-c}" == "llvm" ]]; then
  if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
  if [[ -d "$HOME/arm-gnu-toolchain-15.2/Payload/bin" ]]; then PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"; fi
  for t in llc arm-none-eabi-gcc; do command -v "$t" >/dev/null 2>&1 || { echo "SKIP: '$t' not found (need LLVM + ARM toolchain for BUILD=llvm)"; exit 0; }; done
fi

# Wrapping control: same program with `+` → `+%` on the accumulator.
CTRL="$WORK/overflow_wrap.si"
sed 's/acc   = acc + 85/acc   = acc +% 85/' "$EX" > "$CTRL"
if ! grep -q 'acc +% 85' "$CTRL"; then
  echo "FAIL: could not derive the wrapping control from $EX"
  exit 1
fi

addr() { arm-none-eabi-nm "$1" | awk -v s="$2" '$3==s{print "0x"$1}'; }

# Build $1 → ELF, run it for ~1.05s on metal, and echo the final `ticks` value.
run_ticks() {
  local src="$1"
  local elf="$WORK/$(basename "$src" .si).elf"
  local resc="$WORK/run.resc"
  if [[ "${BUILD:-c}" == "llvm" ]]; then
    (cd "$REPO" && cargo run -q --bin silicac -- --target metal-nrf52840 --emit-llvm "$src" -o "$WORK/$(basename "$src" .si)")
    llc "$WORK/$(basename "$src" .si).ll" -filetype=obj -o "$WORK/$(basename "$src" .si).o" || { echo "FAIL: llc" >&2; exit 1; }
    arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -nostdlib -nostartfiles -T "$WORK/$(basename "$src" .si).ld" "$WORK/$(basename "$src" .si).o" -o "$elf" || { echo "FAIL: link" >&2; exit 1; }
  else
    (cd "$REPO" && cargo run -q -- --target metal-nrf52840 "$src" -o "$elf")
  fi
  local ticks_addr; ticks_addr="$(addr "$elf" ticks)"
  if [[ -z "$ticks_addr" ]]; then
    echo "FAIL: no ticks symbol in $elf" >&2
    exit 1
  fi
  cat > "$resc" <<RESC
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
nvic Frequency 64000000
sysbus LoadELF @$elf
mach set "dk"
emulation RunFor "1.05"
sysbus ReadDoubleWord $ticks_addr
quit
RESC
  local out
  out="$("$RENODE" --console --disable-xwt --plain -e "include @$resc" 2>&1 \
        | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}' | tail -1)"
  echo $(( out ))
}

echo "== build + run: trap (plain +, u8 overflow on tick 4) =="
TRAP_TICKS="$(run_ticks "$EX")"
echo "trap build:    ticks = $TRAP_TICKS  (expect ≈4: halted at the overflow)"

echo "== build + run: wrapping control (+%, never traps) =="
WRAP_TICKS="$(run_ticks "$CTRL")"
echo "wrap build:    ticks = $WRAP_TICKS  (expect ≈10: ran every tick)"

echo "== compare =="
if (( TRAP_TICKS >= 3 && TRAP_TICKS <= 5 && WRAP_TICKS >= 9 && WRAP_TICKS > TRAP_TICKS )); then
  echo "PASS: the default \`+\` trapped on overflow and halted (safe-state); \`+%\` wrapped and kept running (§4.3/SIL-004)."
  exit 0
else
  echo "FAIL: expected trap≈4 (3..5) and wrap≈10 (≥9, > trap); got trap=$TRAP_TICKS wrap=$WRAP_TICKS"
  exit 1
fi
