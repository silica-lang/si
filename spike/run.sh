#!/usr/bin/env bash
# Build the spike firmware and run both Renode checks.
#
#   RENODE=/path/to/renode ./spike/run.sh
#
# Requires arm-none-eabi-gcc on PATH and a Renode binary (set $RENODE, or have
# `renode` on PATH).  Renode portable: https://github.com/renode/renode/releases
set -euo pipefail

DIR="$(cd "$(dirname "$0")" && pwd)"
CC="${CC:-arm-none-eabi-gcc}"
RENODE="${RENODE:-renode}"
CFLAGS=(-mcpu=cortex-m4 -mthumb -ffreestanding -nostdlib -nostartfiles -O1 -Wall -T "$DIR/nrf52840.ld")

echo "== building =="
"$CC" "${CFLAGS[@]}" "$DIR/blink.c"      -o "$DIR/blink.elf"
"$CC" "${CFLAGS[@]}" "$DIR/button_irq.c" -o "$DIR/button_irq.elf"

echo "== blink: expect OUT bit13 (0x2000) to alternate =="
"$RENODE" --console --disable-xwt --plain \
  -e "\$bin=@$DIR/blink.elf; include @$DIR/blink.resc" 2>&1 | grep -E '^0x[0-9A-Fa-f]{8}'

echo "== button IRQ: expect bit13 to toggle once per press+release =="
"$RENODE" --console --disable-xwt --plain \
  -e "\$bin=@$DIR/button_irq.elf; include @$DIR/button_irq.resc" 2>&1 | grep -E '^0x[0-9A-Fa-f]{8}'
