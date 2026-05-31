/* Spike linker script for nRF52840 — the shape the metal backend will generate
 * from `board.soc.memory` (§6.4).  Flash 1 MB @ 0x0, RAM 256 KB @ 0x2000_0000. */
MEMORY
{
  FLASH (rx) : ORIGIN = 0x00000000, LENGTH = 1024K
  RAM   (rwx): ORIGIN = 0x20000000, LENGTH = 256K
}

ENTRY(Reset_Handler)

SECTIONS
{
  .vectors : { KEEP(*(.vectors)) } > FLASH
  .text    : { *(.text*) *(.rodata*) } > FLASH
  .data    : { *(.data*) } > RAM AT > FLASH
  .bss     : { *(.bss*) *(COMMON) } > RAM

  _estack = ORIGIN(RAM) + LENGTH(RAM);
}
