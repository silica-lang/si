# Phase 0 — on-metal backend scope (nRF52840 / Renode)

> Scope document for the next increment. Status: planned, not yet implemented.
> Companion to [`DESIGN.md`](DESIGN.md) §6.2/§6.4 (backend), §4.2 (registers),
> §5.5 (atomicity), §11 (roadmap). The sim-side reactive-core slice is already
> built; this closes the on-metal half of Phase 0.

## Context

The compiler today runs the canonical blink+button program (`examples/blink_button.si`)
end-to-end in a deterministic host simulator (`silicac --sim`). SIR is already the
target-neutral contract (§6.1): register access lowers to `SirPlace::Reg{device, offset,
mask, shift, access}` and the priority-ceiling critical section lowers to a `SirStmt::Critical`
node — both deliberately emitted even though the single-threaded sim treats them as no-ops,
precisely so the metal backend has something to consume.

This increment makes the **same** `.si` run as real firmware on an **nRF52840 (Cortex-M4F)**,
observed in **Renode**. It closes the outstanding Phase-0 items:

- validation gate **#1** (no dynamic allocation / exact RAM budget),
- validation gate **#3** (barrier insertion in emitted C),
- success criteria "identical program runs in sim **and** on metal" + "LED blinks".

It takes Phase 0 from ~60% to ~95%; only the Layer-3 forced-fault decoder (§5.4) remains
after this.

## Target facts (nRF52840 / PCA10056 DK)

- Cortex-M4F @ 64 MHz; **flash 1 MB @ `0x0000_0000`**, **RAM 256 KB @ `0x2000_0000`**;
  vector table at flash base — no second-stage bootloader (simpler than RP2040).
  **Has BASEPRI** → faithful priority-ceiling masking.
- **GPIO P0 @ `0x5000_0000`** (P1 @ `0x5000_0300`): `OUT 0x504`, `OUTSET 0x508`,
  `OUTCLR 0x50C`, `IN 0x510`, `DIR 0x514`, `DIRSET 0x518`, `PIN_CNF[n] 0x700+4*n`.
  DK: **LED1 = P0.13** (active-low), **Button1 = P0.11** (active-low, pull-up).
- **GPIOTE @ `0x4000_6000`**, `GPIOTE_IRQn = 6` → vector index 22 (offset `0x58`).
- **SysTick** (SCS): `CSR 0xE000_E010`, `RVR 0xE000_E014`, `CVR 0xE000_E018`; exception #15
  (offset `0x3C`). **NVIC** `ISER0 @ 0xE000_E100`.

The nRF GPIO layout differs from the STM32-shaped `std/gpio.si` (it uses `OUT`/`DIR`/
`OUTSET`/`OUTCLR`, not `IDR`/`ODR`) — which is exactly why register layout lives in std-lib
`.si` data, not the compiler core.

## What gets built

### 1. std-lib devices (ordinary `.si`, no core privilege — §2)

- `std/nrf_gpio.si` — `OUT/OUTSET/OUTCLR/IN/DIR/DIRSET/PIN_CNF` with correct access
  qualifiers. `OUTSET`/`OUTCLR` are write-1-to-set/clear → the `w1c`-family lowering already
  modelled. `set` lowers to OUTSET/OUTCLR (atomic, no RMW race); `emits falling` is wired via
  GPIOTE.
- `std/systick.si` — how `every` becomes real (folds in the "timer-device wiring" chunk).
- `std/gpiote.si`, `std/nvic.si` — how `on <pin>.falling` resolves to an IRQ + vector entry
  (§4.1: the compiler follows `needs irq` into the interrupt-controller device and generates
  the vector-table entry).

### 2. SIR → MMIO lowering (gate #3)

`SirPlace::Reg` / `SirExpr::RegLoad` (currently TODO comments in `backend/c.rs`) lower to
volatile masked load/store on fixed-width pointers, with `__DSB()`/`__DMB()` fences around
register-write blocks and before IRQ enable (§4.2/§6.2). No C bitfields; fixed-width types
only.

### 3. A metal target in the C backend

Add `--target {host|metal-nrf52840}` (default `host`, so existing host/sim paths are
untouched). Metal mode emits the **freestanding subset** — no libc, no `nanosleep`:

- MMIO register access (item 2);
- a generated **vector table** (`__attribute__((section(".vectors")))`) from the resolved
  `on`/`every` bindings;
- **`SysTick_Handler` / `GPIOTE_IRQHandler`** that dispatch the corresponding reactions;
- **`SirStmt::Critical` → BASEPRI** raise/restore (the metal lowering of the §5.5 node).

### 4. Generated startup + linker (§6.4), comptime-derived from the board type

- **Linker script** from `board.soc.memory` (flash/RAM origin+size): `.vectors/.text/.rodata/
  .data/.bss` + stack at top of RAM.
- **Reset handler**: copy `.data` flash→RAM, zero `.bss`, run device init in dependency order
  (GPIO `DIR`/`PIN_CNF` for the LED, GPIOTE channel for the button, SysTick reload for the
  period), enable IRQs, enter the WFI scheduler loop.
- **RAM-budget computation (gate #1)**: sum statics + `.bss` + `.data` + worst-case stack and
  emit it; exceeding the region is a build error.

### 5. Renode harness (CI gate)

A generated `.resc` loading the nRF52840 platform + an `LED` on P0.13 and a `Button` on P0.11;
a Robot/script test that runs virtual time, **injects button presses at 1.2 s / 1.8 s**, and
asserts the **same LED toggle sequence** the `--sim` integration test asserts.
Toolchain: `arm-none-eabi-gcc -mcpu=cortex-m4 -mthumb -nostartfiles -T <generated>.ld`.

## Staging (each stage independently testable)

| Stage | Deliverable | Renode checkpoint |
| --- | --- | --- |
| **A** ✅ | `--target` flag; linker-script + vector-table + reset generation; RAM-budget gate; metal `main` runs `sys.start` then idles | firmware links & boots in Renode |
| **B** ✅ | `SirPlace::Reg`→MMIO with barriers; `std/nrf_gpio.si`; init sets `DIR` | LED driven once at boot — observed on the pin |
| **C** | `every`→SysTick (std device + handler + vector entry) | LED **blinks** periodically |
| **D** | `on falling`→GPIOTE+NVIC vector; `Critical`→BASEPRI | full **blink+button**, shared cell, injected presses |
| **E** | Renode `.resc` + Robot test asserting the sim-identical sequence; README docs | **automated metal gate** in CI |

## Progress

**Stage A — done** (verified in Renode). `silicac --target metal-nrf52840
examples/boot_nrf52840.si -o boot.elf` generates the linker script from
`board.soc.memory`, a vector table + reset/startup (`.data` copy, `.bss` zero,
`sys.start` dispatch, WFI idle), and the freestanding C (no libc). Booting the
ELF in Renode: `value` reads back `0x2A` after `sys.start` (proving startup +
`.data` init + reaction dispatch), CPU idles in WFI. RAM-budget gate (#1) reports
`2052 B of 262144 B`. Covered by `tests/metal_codegen.rs` (hermetic) and
end-to-end with `arm-none-eabi-gcc` + Renode 1.16.1.

**Stage B — done** (verified in Renode). `SirPlace::Reg` lowers to ordered
volatile MMIO with `__DMB()` barriers (gate #3); `std/nrf_gpio.si` is the nRF
GPIO device; the generated startup configures output-pin direction (`DIR`)
before `sys.start`. On `examples/blink_button_nrf52840.si`, `sys.start`'s
`led.set(true)` drives P0.13: Renode's `OUT` (`0x5000_0504`) reads back `0x2000`
after boot (was `0x0`). The *same* program runs under `--sim` (mock registers).
Direction-register selection is a documented heuristic (a writable register
distinct from the data register) to be replaced by per-pin config ops.

## How the Phase-0 gates close

- **Gate #1 (RAM budget):** Stage A emits the summed static footprint and fails the build if
  it exceeds the RAM region.
- **Gate #2 (deterministic pin muxing):** already closed (duplicate-pad compile error).
- **Gate #3 (barriers):** Stage B emits `__DSB`/`__DMB` around register-write blocks and
  before IRQ enable; an assertion checks their presence in the generated C.
- **Success — sim ≡ metal:** Stage E asserts byte-for-byte the same LED toggle sequence the
  sim test asserts.

## Risks / open calls

1. **Renode GPIO/GPIOTE fidelity** — the highest-leverage unknown. Mitigation: Stage B proves a
   single observed pin write before the IRQ path is built on top.
2. **`every`→SysTick vs a nRF `TIMER`/`RTC`** — SysTick is simplest and architectural; modelled
   as a std device (not a core built-in) so §2 holds. A `TIMER`/`RTC` swap is a later increment.
3. **Toolchain availability** — `arm-none-eabi-gcc` and Renode (with the nRF52840 model) must be
   present in dev/CI; to be confirmed and documented.

## Explicitly out of scope (remaining ~5% of Phase 0)

The **Layer-3 forced-fault → decoded-trace** success criterion (§5.4). A minimal
`HardFault_Handler` emitting a trace marker is cheap to add here, but the full graph-aware
fault decoder (address-ownership + site maps) is a separate increment.
