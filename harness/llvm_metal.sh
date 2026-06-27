#!/usr/bin/env bash
# Metal LLVM end-to-end gate (§6.3, audit P3-4c): the LLVM backend's metal
# direction boots real firmware.  Proves SIR → LLVM IR → `llc` → object → linked
# with the *generated* linker script → ELF → **runs on Renode** and produces the
# right result — the genuine "second backend, no C anywhere" proof for the metal
# direction (the C backend is not involved in this pipeline at all).
#
#   RENODE=/path/to/renode ./harness/llvm_metal.sh
#
# Requires: cargo, LLVM (`llc`), arm-none-eabi-gcc/nm, and Renode.
# Uses examples/boot_nrf52840.si: `on sys.start { value = 42 }`.
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
EX="$REPO/examples/boot_nrf52840.si"
RENODE="${RENODE:-renode}"
WORK="$(mktemp -d)"
RESC="$(mktemp).resc"
trap 'rm -rf "$WORK" "$RESC"' EXIT

# Locate LLVM (llc) + ARM toolchain.
if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
if [[ -d "$HOME/arm-gnu-toolchain-15.2/Payload/bin" ]]; then PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"; fi
for t in llc arm-none-eabi-gcc arm-none-eabi-nm; do
  command -v "$t" >/dev/null 2>&1 || { echo "SKIP: '$t' not found (need LLVM + ARM toolchain)"; exit 0; }
done

echo "== emit metal LLVM IR + linker script (no C backend involved) =="
cd "$REPO"
cargo run -q --bin silicac -- --target metal-nrf52840 --emit-llvm "$EX" -o "$WORK/boot" 2>"$WORK/emit.log" \
  || { echo "FAIL: --emit-llvm --target metal-nrf52840 errored"; cat "$WORK/emit.log"; exit 1; }
LL="$WORK/boot.ll"; LD="$WORK/boot.ld"
cat "$LL"

echo "== IR shape: freestanding, no @main, no host syscall =="
grep -q 'target triple = "thumbv7em-none-eabi"' "$LL" || { echo "FAIL: no metal triple"; exit 1; }
grep -q '@__vectors' "$LL"           || { echo "FAIL: no vector table"; exit 1; }
grep -q 'section ".vectors"' "$LL"   || { echo "FAIL: vectors not in .vectors section"; exit 1; }
grep -q 'define void @Reset_Handler()' "$LL" || { echo "FAIL: no Reset_Handler"; exit 1; }
if grep -q 'define i32 @main()' "$LL"; then echo "FAIL: metal must not emit @main"; exit 1; fi
if grep -q 'svc #' "$LL"; then echo "FAIL: metal must not emit a host syscall"; exit 1; fi
echo "PASS: freestanding metal IR (vector table + Reset_Handler, no @main/syscall)."

echo "== llc → object, link with the generated script → ELF =="
llc "$LL" -filetype=obj -o "$WORK/boot.o" 2>"$WORK/llc.log" || { echo "FAIL: llc"; cat "$WORK/llc.log"; exit 1; }
arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -nostdlib -nostartfiles -T "$LD" "$WORK/boot.o" -o "$WORK/boot.elf" 2>"$WORK/link.log" \
  || { echo "FAIL: link"; cat "$WORK/link.log"; exit 1; }
VAL="$(arm-none-eabi-nm "$WORK/boot.elf" | awk '$3=="value"{print "0x"$1}')"
[[ -n "$VAL" ]] || { echo "FAIL: no 'value' symbol in ELF"; exit 1; }
echo "PASS: linked ELF; value @ $VAL."

echo "== boot on Renode, read the cell back (expect 42) =="
cat > "$RESC" <<RESC
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
sysbus LoadELF @$WORK/boot.elf
mach set "dk"
emulation RunFor "0.0100"
sysbus ReadDoubleWord $VAL
quit
RESC
GOT="$(timeout 120 "$RENODE" --console --disable-xwt --plain -e "include @$RESC" 2>&1 | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}' | head -1)"
echo "metal: value = $GOT"

echo "== compare =="
if [[ "$(( GOT ))" -eq 42 ]]; then
  echo "PASS: LLVM-built firmware booted on Renode and ran sys.start (value=42) — metal LLVM end-to-end, no C (P3-4c)."
  exit 0
else
  echo "FAIL: expected value=42, got $GOT"; exit 1
fi
