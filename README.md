# Silica

[![docs](https://img.shields.io/badge/docs-mdBook-blue)](https://silica-lang.github.io/si/)
[![llms.txt](https://img.shields.io/badge/llms.txt-ai--ready-brightgreen)](https://silica-lang.github.io/si/llms.txt)

> An experimental, **embedded-native** and **agentic-native** programming language.
> File extension: `.si`. Status: early — a working Phase-0 compiler slice.

Silica is built around a wager: that the language an **AI agent** most wants to author,
edit, and debug is the same language a **compiler** most wants to analyze and a **hardware
engineer** most wants to read — and that *embedded systems* are where that alignment is
sharpest. Semantics are built from hardware concepts (devices, registers, interrupts, time,
resources, capabilities) rather than the C-and-UNIX vocabulary most embedded toolchains
smuggle in, and the language deliberately removes the things that make code hard for a
machine to reason about: hidden state, ambiguous grammar, and untyped text.

The full rationale, type system, execution model, and roadmap live in
**[`docs/DESIGN.md`](docs/DESIGN.md)** — start there. A friendlier, user-facing version is
published at **[silica-lang.github.io/si](https://silica-lang.github.io/si/)** (built from
[`docs/book`](docs/book) with [mdBook](https://rust-lang.github.io/mdBook/)). The docs are
also published in the [llms.txt](https://llmstxt.org/) format for AI agents:
[`llms.txt`](https://silica-lang.github.io/si/llms.txt) (index) and
[`llms-full.txt`](https://silica-lang.github.io/si/llms-full.txt) (the whole site in one file).

## Repository layout

| Path | What it is |
| --- | --- |
| [`docs/DESIGN.md`](docs/DESIGN.md) | The design document / spec. The source of truth. |
| [`crates/silicac`](crates/silicac) | `silicac`, the Rust compiler (lexer → parser → resolver → SIR → consumers). |
| [`crates/silicac/std`](crates/silicac/std) | The standard library: peripheral `device`s authored in `.si` (e.g. `gpio`, `timer`). |
| [`examples`](examples) | Example `.si` programs (host, sim, and nRF52840 metal). |
| [`harness`](harness) | `metal_vs_sim.sh` — the on-metal (Renode) ≡ simulator validation gate. |

## What works today

The compiler implements the **reactive-core vertical slice** (DESIGN.md §9.6): the canonical
blink + button program runs **both** in a deterministic host simulator **and on real
hardware** (nRF52840), from the same source.

- **Language:** `program` / `board` / `soc` / `device` (`regs`/`config`/`needs`/`ops`/`emits`),
  typed pin bindings, `cell`, and the `on <event>` / `every <duration>` reactive model.
- **A real device model.** `gpio` is an ordinary std-lib `device`, not a compiler built-in
  ("no privileged built-ins" — DESIGN.md §2); pin ops lower to **target-neutral register
  accesses**.
- **Compiler-computed concurrency.** Two reactions sharing one `cell` get a
  priority-ceiling **critical section computed automatically** — no `disable_irq` in source
  (§5.5); on metal it lowers to real BASEPRI masking. Single-owner cells are *proven*
  section-free.
- **Static safety checks.** Binding two things to one physical pad is a **compile error**
  (§3.3); the static RAM budget is checked against the chip's memory (no dynamic allocation).
- **A deterministic simulator** (§7.1): a virtual clock, scripted event injection, mock
  register side effects, and a structured trace — reproducible, no wall-clock dependence.
- **On-metal codegen** (`--target metal-nrf52840`): generated linker script, vector table,
  reset/startup, ordered MMIO with barriers, `every`→SysTick, and `on <pin>.falling`→GPIOTE/NVIC
  — a freestanding image with no libc (§6.2/§6.4). The `harness/metal_vs_sim.sh` gate asserts
  the metal LED sequence matches the simulator's (validated in Renode).

- **Layer-3 fault decoding** (§5.4): an address-ownership map (from the board) turns a
  faulting address into a language-level diagnosis (*"no device claims this address"* /
  *"within device `gpio0`"*). A `sim` block can `inject fault`, and the metal backend emits a
  `HardFault_Handler` that decodes against the same map.

SIR (Silica IR) is the contract (§6.1): the host simulator and the C/metal backend are
*consumers* of the same IR, keeping a future LLVM backend reachable.

This completes the Phase-0 reactive core (DESIGN.md §11). Phase 1 adds composed devices
over buses — both `i2c` and `spi` controllers, with composed sensors (BME280 over I²C,
a BMP280-style sensor over SPI). Not yet built (deferred, not foreclosed — see §10/§11):
the Layer-3 *site map* (when-state-violation decode), typed overlays, and the DTS→Silica
fact importer.

## Build & run

Requires a Rust toolchain (`cargo`).

```sh
# Build the compiler
cargo build

# Run the blink + button program in the deterministic simulator
cargo run -- --sim examples/blink_button.si

# Compile a host program to a native binary via the C backend
cargo run -- examples/hello.si -o /tmp/hello && /tmp/hello

# Build a bare-metal nRF52840 image (needs arm-none-eabi-gcc)
cargo run -- --target metal-nrf52840 examples/blink_button_nrf52840.si -o blink.elf

# Run the test suite
cargo test

# End-to-end "sim ≡ metal" gate (needs arm-none-eabi-gcc + Renode)
RENODE=/path/to/renode ./harness/metal_vs_sim.sh
```

`silicac` usage:

```
silicac <input.si> [-o <output>] [--emit-c] [--sim] [--target host|metal-nrf52840] [--cc <compiler>] [--std <dir>]
```

The standard-library devices are loaded from `crates/silicac/std` by default; override with
`--std <dir>`.

### Example

```si
program blink {
  use board nucleo_f401re as nucleo

  let led    = nucleo.led_user
  let button = nucleo.btn_user

  cell lit : bool = false

  every 500ms       { lit = not lit; led.set(lit) }
  on button.falling { lit = not lit; led.set(lit) }   // shares `lit` — critical section auto-computed
}
```

See [`examples/blink_button.si`](examples/blink_button.si) for the full program, board, and
simulation script.

## Status & scope

Silica is deliberately a "toy" — an intellectual exercise — with an aspirational long-term
ceiling (potentially replacing an RTOS like Zephyr for personal projects) used as a
*foreclosure constraint*, not a v1 deliverable. See DESIGN.md §1 (scope) and §11 (roadmap).
