#!/usr/bin/env bash
# Metal LLVM poll/await gate (§3.2/§5.2, audit P5-3): prove the bounded-wait +
# fault-disposition lowering works end-to-end on metal, built ENTIRELY through the
# LLVM backend (no C).  For BOTH `poll` (busy-wait) and `await` (wfi re-check):
#  - success: the condition is preset true → the wait passes → `done` advances;
#  - timeout: flip the preset to 0 → the wait elapses, faults, and `skip` drops
#    the activation → `done` stays 0.
# The gap (done>0 vs done=0) is the proof the bounded wait + __faulted + the
# Layer-2 disposition all work on the LLVM-built firmware.
#
#   RENODE=/path/to/renode ./harness/poll_await.sh
#
# Requires: cargo, LLVM (`llc`), arm-none-eabi-gcc/nm, Renode.  No mock needed.
# (Neither poll nor await has a C-path Renode harness — this is the first.)
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
RENODE="${RENODE:-renode}"
WORK="$(mktemp -d)"
trap 'rm -rf "$WORK"' EXIT
if [[ -d /opt/homebrew/opt/llvm/bin ]]; then PATH="/opt/homebrew/opt/llvm/bin:$PATH"; fi
if [[ -d "$HOME/arm-gnu-toolchain-15.2/Payload/bin" ]]; then PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"; fi
for t in llc arm-none-eabi-gcc; do command -v "$t" >/dev/null 2>&1 || { echo "SKIP: '$t' not found (need LLVM + ARM toolchain)"; exit 0; }; done
command -v "$RENODE" >/dev/null 2>&1 || [[ -x "$RENODE" ]] || { echo "SKIP: renode not found"; exit 0; }

cd "$REPO"

# Build $1 (.si) → ELF via the LLVM backend; echo the `done` cell after a 0.35s run.
run_done() {
  local src="$1" name; name="$(basename "$src" .si)"
  cargo run -q --bin silicac -- --target metal-nrf52840 --emit-llvm "$src" -o "$WORK/$name" 2>"$WORK/$name.emit" \
    || { echo "FAIL: --emit-llvm $src" >&2; cat "$WORK/$name.emit" >&2; exit 1; }
  llc "$WORK/$name.ll" -filetype=obj -o "$WORK/$name.o" 2>"$WORK/$name.llc" || { echo "FAIL: llc $name" >&2; cat "$WORK/$name.llc" >&2; exit 1; }
  arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -nostdlib -nostartfiles -T "$WORK/$name.ld" "$WORK/$name.o" -o "$WORK/$name.elf" 2>"$WORK/$name.link" \
    || { echo "FAIL: link $name" >&2; cat "$WORK/$name.link" >&2; exit 1; }
  local addr; addr="$(arm-none-eabi-nm "$WORK/$name.elf" | awk '$3=="done"{print "0x"$1}')"
  [[ -n "$addr" ]] || { echo "FAIL: no done symbol in $name.elf" >&2; exit 1; }
  local resc="$WORK/$name.resc"
  cat > "$resc" <<RESC
mach create "dk"
machine LoadPlatformDescription @platforms/boards/nrf52840dk_nrf52840.repl
nvic Frequency 64000000
sysbus LoadELF @$WORK/$name.elf
mach set "dk"
emulation RunFor "0.35"
sysbus ReadDoubleWord $addr
quit
RESC
  local out; out="$(timeout 150 "$RENODE" --console --disable-xwt --plain -e "include @$resc" 2>&1 | tr -d '\r' | grep -E '^0x[0-9A-Fa-f]{8}' | tail -1)"
  [[ -n "$out" ]] || { echo "FAIL: no readback for $name" >&2; exit 1; }
  echo $(( out ))
}

# Run one construct: success (as-is) vs timeout (preset flipped 1→0).
check() {
  local label="$1" src="$2"
  echo "== $label: success (ready preset true) =="
  local ok; ok="$(run_done "$src")"
  echo "  $label success: done = $ok  (expect > 0)"
  echo "== $label: timeout (ready forced 0) =="
  local to_src="$WORK/$(basename "$src" .si)_timeout.si"
  sed 's/cell ready : u32 = 1/cell ready : u32 = 0/' "$src" > "$to_src"
  grep -q 'cell ready : u32 = 0' "$to_src" || { echo "FAIL: could not derive the timeout control"; exit 1; }
  local to; to="$(run_done "$to_src")"
  echo "  $label timeout: done = $to  (expect 0)"
  if (( ok > 0 && to == 0 )); then
    echo "  $label OK ✓"
  else
    echo "FAIL: $label expected success>0 timeout=0; got success=$ok timeout=$to"; exit 1
  fi
}

check poll  "$REPO/examples/poll_nrf52840.si"
check await "$REPO/examples/await_nrf52840.si"

echo "== compare =="
echo "PASS: LLVM-built poll & await pass when satisfied (done>0) and fault→skip on timeout (done=0) — sim ≡ metal(LLVM), P5-3."
