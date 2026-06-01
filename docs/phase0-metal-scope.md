# Phase 0 — on-metal backend scope (nRF52840 / Renode)

> Scope + status document for the on-metal backend. **Status: implemented** — all
> five stages (A–E) and the Layer-3 fault decoder are done; per-stage progress and
> the final status are recorded inline below.
> Companion to [`DESIGN.md`](DESIGN.md) §6.2/§6.4 (backend), §4.2 (registers),
> §5.5 (atomicity), §11 (roadmap). The sim-side reactive-core slice was built
> first; this closed the on-metal half of Phase 0.

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

`SirPlace::Reg` (Stage B) and `SirExpr::RegLoad` (Phase-1 metal lowering) lower to
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
| **C** ✅ | `every`→SysTick (handler + vector entry + startup config) | LED **blinks** periodically |
| **D** ✅ | `on falling`→GPIOTE+NVIC vector; `Critical`→BASEPRI | full **blink+button**, shared cell, injected presses |
| **E** ✅ | `harness/metal_vs_sim.sh` asserting the sim-identical sequence; README docs | **automated metal gate** |

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

**Stage C — done** (verified in Renode). `every` lowers onto the Cortex-M
SysTick (§4.5): `systick_plan` computes a 1 ms base reload from
`board.soc.clocks` (RVR = `core_hz/1000 - 1`; a 24-bit-overflow or non-whole-ms
period is a compile error), and the generated `SysTick_Handler` software-counts
base ticks per `every` reaction. Startup programs SYST_RVR/CVR/CSR and enables
interrupts; the vector table gains the SysTick entry (#15). On
`blink_button_nrf52840.si` the LED toggles in Renode: `OUT` bit 13 tracks the
`lit` cell 1↔0. Timing note: Renode's SysTick rate follows its CPU clock, which
must be pinned to 64 MHz in the Stage-E harness for sim≡metal *timing* (the
*sequence* already matches). SysTick is programmed at its architectural SCS
address (it is part of the Cortex-M core, not a board peripheral).

**Stage D — done** (verified in Renode). `on <pin>.falling` lowers to a GPIOTE
channel (event mode, HiToLo) + an NVIC vector entry (#22 = IRQ 6); the generated
`GPIOTE_IRQHandler` clears the event and dispatches the bound reactions. Input
pins get `PIN_CNF` (connect + pull-up). The `Critical` node lowers to a real
**BASEPRI** raise/restore at the priority ceiling (§5.5): the timer (SysTick) and
button (GPIOTE) are given *distinct* NVIC priorities (button more urgent), so the
shared-`lit` access genuinely masks the racing interrupt. Verified: an isolated
button example toggles the LED per press (`OUT` 0↔0x2000); the full blink+button
runs with SysTick + GPIOTE + BASEPRI coexisting, blinks, responds to injected
presses, and never faults. GPIOTE register details are nRF-specific to this
target (SIR stays neutral); a GPIOTE std-device with full event routing is a
documented refinement.

**Stage E — done.** `harness/metal_vs_sim.sh` is the automated **sim ≡ metal**
gate: it runs `--sim` for the reference LED sequence, compiles the same program
with `--target metal-nrf52840`, runs it in Renode with the NVIC/SysTick clock
pinned to 64 MHz (so the 500 ms period is real-time) while injecting the button
at the `sim` block's times, and asserts the metal LED sequence equals the
simulator's. Result: `sim 1 0 1 0 1 0 1` ≡ `metal 1 0 1 0 1 0 1` — **PASS**. The
hermetic `tests/metal_codegen.rs` covers codegen in CI; this harness is the
end-to-end on-metal complement (needs arm-gcc + Renode, run on demand).

## Status: metal scope complete

All five stages done; the §9.6 "identical program in sim and on metal" criterion
is met and automated. This takes Phase 0 to ~95%; the remaining item is the
Layer-3 forced-fault → decoded-trace decoder (§5.4), tracked as the out-of-scope
note below.

### Phase-1 metal lowering (in progress)

Closing the sim≡metal gap for the Phase-1 keystone (composition + yields). Done:

- **`SirExpr::RegLoad` → volatile masked MMIO read** — the read counterpart of the
  Stage-B store lowering. A register read (e.g. `button.get()`) now emits
  `((((uint32_t)(*(volatile uint32_t *)<addr>UL)) & <mask>UL) >> <shift>)`, matching
  the simulator's `(reg & field_mask) >> field_shift` exactly (no RMW, so a `w1c`
  status bit is undisturbed). Covered by `reg_load_lowers_to_volatile_masked_read`
  in `tests/metal_codegen.rs`. `SirStmt::DeviceOp` now emits `#error` on metal
  rather than a silent no-op.

Remaining: **`SirStmt::BusXfer`** (yielding I²C transaction) on metal, which needs
the nRF52840 **TWIM** sequence (the abstract `CR/SR/DR` controller model is the
sim's view) plus disposition lowering — tracked as the next increment.

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

## Layer-3 fault decoder (§4.4/§5.4) — done

The final Phase-0 item. A shared **address-ownership map** (`crate::layer3`)
computed from the board — each device's MMIO span + the flash/RAM regions — turns
a faulting address into a language-level diagnosis (*"no device claims this
address"* / *"within device `gpio0` …"*).

- **Simulator (primary):** a `sim` block can `inject fault <addr> at <dur>`; the
  interpreter decodes each against the ownership map and emits a structured
  `Fault` trace record. This satisfies the §11 "forced fault → decoded trace
  record" criterion and is where §7.2 says the decoder lives. Covered by
  `tests/sim_fault_decode.rs` and `examples/fault_nrf52840.si`.
- **Metal:** the backend emits the ownership table (indices, no on-device
  strings — §4.3) + a `HardFault_Handler` (vector #3) that reads SCB BFAR, scans
  the table, and records `{addr, owner, pending}` to fixed RAM for the host to
  render. Covered by a hermetic codegen test; the handler was verified to execute
  and scan the table on the Renode CPU. **Live forced-fault decode is limited by
  Renode**, which tolerates unmapped accesses (no bus fault raised) and exposes
  BFAR read-only — an emulator constraint, not a compiler one; on real silicon the
  precise bus fault populates BFAR.

The **site map** (PC→handler/op/when-state) half of §5.4 — for *"handler X touched
device Y outside its valid when state"* — remains a later increment (it needs
per-site debug info and enforced typestate).

## Phase 0 status

All §11 deliverables and the three validation-matrix gates are met; the §9.6
success criteria — identical program in sim and on metal, LED blinks, button
shares a cell without a manual critical section, and a forced fault shows up as a
decoded trace record — are demonstrated. Remaining refinements (timer-as-device
wiring, GPIOTE/NVIC as std devices, the Layer-3 site map, multi-port GPIO) are
documented inline and are additive, not foreclosed.
