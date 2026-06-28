# Silica

[![docs](https://img.shields.io/badge/docs-mdBook-blue)](https://silica-lang.github.io/si/)
[![llms.txt](https://img.shields.io/badge/llms.txt-ai--ready-brightgreen)](https://silica-lang.github.io/si/llms.txt)

> An experimental, **embedded-native** and **agentic-native** programming language.
> File extension: `.si`. Status: early but substantial — the reactive core, composed
> devices on buses, and the full numeric/fault/typestate surface run in a deterministic
> simulator **and** on real hardware (nRF52840), through **two independent backends** (C
> and LLVM) that are held to byte-for-byte `sim ≡ metal` parity in Renode.

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
| [`harness`](harness) | The Renode gate suite — each script asserts a feature matches `sim ≡ metal` on real hardware, for **both** the C and the LLVM backend (`BUILD=llvm`). See [`harness/README.md`](harness/README.md). |

## What works today

The compiler runs the **reactive core, composed devices on buses, and the full
numeric/fault/typestate surface** — every feature in a deterministic host simulator and on
real hardware (nRF52840), from one source, with the metal image emitted by **either** of two
independent backends.

**The language**
- `program` / `board` / `soc` / `device` (`regs`/`config`/`needs`/`ops`/`emits`), typed pin
  bindings, `cell`, and the `on <event>` / `every <duration>` reactive model.
- **Control & data:** `match` (over literals *and* over an op's `ok`/`fault <code>` result),
  `atomic { … }`, `poll`/`await <cond> within <d> else fault <code>`, scheduler `on overflow
  coalesce|drop_newest|fault`, and `reaction … within <d>` deadlines.
- **Typed numbers:** explicit `as` casts with implicit-narrowing rejected, saturating/wrapping
  operators (`+| -% …`) with **overflow-trap-by-default**, `fixed<I,F>` fixed-point (mul/div,
  decimal + `3v3` voltage literals), runtime `float`/FPU arithmetic, `instant`/`duration` time
  types with `now()`, and the bounded `ring<T,N>` queue.
- **Devices & state:** `when`-typestate (`states`/`become`, with runtime-precondition lowering
  to a safe-state guard), per-field register access semantics (`rc`/`pop_on_read`, `w1c`,
  multi-field single writes), and compile-time **typed overlays** (`overlay … for board`,
  `set`/`remove`).

**The model**
- **A real device model.** `gpio` is an ordinary std-lib `device`, not a compiler built-in
  ("no privileged built-ins" — DESIGN.md §2); pin ops lower to **target-neutral register
  accesses**.
- **Composed devices on a bus** (§3.5): `i2c` and `spi` controllers carry composed sensors
  (BME280 over I²C with datasheet fixed-point compensation, a BMP280-style sensor over SPI).
  A bus transaction **suspends** the reaction (a real IRQ-driven yields state machine on metal)
  so others run during the wait; **multiple consumers on one bus are priority-arbitrated**
  with a bounded wait queue.
- **Compiler-computed concurrency.** Two reactions sharing one `cell` get a priority-ceiling
  **critical section computed automatically** — no `disable_irq` in source (§5.5); on metal it
  lowers to real BASEPRI masking. Single-owner cells are *proven* section-free.
- **Static safety checks.** Binding two things to one physical pad is a **compile error**
  (§3.3); a **measured** worst-case stack bound (from the toolchain call-graph) and a **flash
  budget** are enforced against the chip's memory — no dynamic allocation, no over-budget image.

**Two backends, held to parity**
- SIR (Silica IR) is the contract (§6.1): the simulator, the **C backend**, and the **LLVM
  backend** are all *consumers* of the same IR.
- **On-metal codegen** (`--target metal-nrf52840`, via C or via `--emit-llvm` → `llc`):
  generated linker script, vector table, reset/startup, ordered MMIO with ARM-conformant
  barriers, `every`→**TIMER1**, `now()`/deadlines→**TIMER2** (1 µs; SysTick retired),
  `on <pin>.falling`→GPIOTE/NVIC, the bus yields state machine, hardware watchdog,
  runtime float on the FPU, and `host_io.print`→ARM semihosting — a freestanding image with
  no libc (§6.2/§6.3/§6.4). Each `harness/*.sh` gate asserts the metal behaviour matches the
  simulator's in Renode, for **both** backends.
- **Layer-3 fault decoding** (§5.4): an address-ownership map (from the board) turns a faulting
  address into a language-level diagnosis (*"no device claims this address"* / *"within device
  `gpio0`"*). A `sim` block can `inject fault`, and both backends emit a `HardFault_Handler`
  that decodes against the same map.

Deferred (not foreclosed — see §10/§11): the Layer-3 *site map* (per-call-site
when-state-violation decode), `pool`/`arena`/`buffer` containers, `f64`/float comparisons on
the M4F, and the DTS→Silica fact importer.

## Build & run

Requires a Rust toolchain (`cargo`).

```sh
# Build the compiler
cargo build

# Run the blink + button program in the deterministic simulator
cargo run -- --sim examples/blink_button.si

# Compile a host program to a native binary via the C backend
cargo run -- examples/hello.si -o /tmp/hello && /tmp/hello

# Build a bare-metal nRF52840 image via the C backend (needs arm-none-eabi-gcc)
cargo run -- --target metal-nrf52840 examples/blink_button_nrf52840.si -o blink.elf

# ...or via the LLVM backend — same metal target, a structurally independent path (needs llc)
cargo run -- --target metal-nrf52840 --emit-llvm examples/blink_button_nrf52840.si -o blink.elf

# Run the test suite
cargo test

# End-to-end "sim ≡ metal" gate (needs arm-none-eabi-gcc + Renode); set BUILD=llvm for the LLVM path
RENODE=/path/to/renode ./harness/metal_vs_sim.sh
RENODE=/path/to/renode BUILD=llvm ./harness/metal_vs_sim.sh
```

`silicac` usage:

```
silicac <input.si> [-o <output>] [--emit-c] [--emit-llvm] [--sim] [--target host|metal-nrf52840] [--cc <compiler>] [--opt <level>] [--std <dir>]
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
