# Introduction

Silica is an experimental, **embedded-native** and **agentic-native** programming
language. File extension: `.si`. Status: early — a working Phase-0 compiler slice.

Silica is built around a wager: that the language an **AI agent** most wants to author,
edit, and debug is the same language a **compiler** most wants to analyze and a **hardware
engineer** most wants to read — and that *embedded systems* are where that alignment is
sharpest, because embedded work is already about explicit resources, explicit time, and a
hardware truth that does not lie.

Two design goals drive everything, and they reinforce rather than compete:

- **Embedded-native.** Semantics are built from hardware concepts — *devices, registers,
  interrupts, time, resources, capabilities* — rather than from the C-and-UNIX vocabulary
  (files, heap, stdio, errno, flat untyped memory) that most embedded toolchains smuggle
  in. A program's mental model should be the *board*, not a stripped-down PC.
- **Agentic-native.** The language is engineered to be an excellent target for AI
  authoring, editing, and debugging — not by adding "AI features," but by removing the
  things that make code hard for a machine to reason about: hidden state, ambiguous
  grammar, and untyped text that only becomes meaningful after a build.

These two goals converge: the thesis is that they are the *same* language, and where they
seem to pull apart, that is a signal the design is wrong, not a tradeoff to split.

Silica is deliberately a "toy" — an intellectual exercise — with an aspirational
long-term ceiling (potentially replacing an RTOS like Zephyr for personal projects) used
as a *foreclosure constraint*, not a v1 deliverable. See the [roadmap](roadmap.md) for
where it stands.

## What works today

The compiler implements the **reactive-core vertical slice**: the canonical blink + button
program runs **both** in a deterministic host simulator **and on real hardware**
(nRF52840), from the *same source*.

- **Language:** `program` / `board` / `soc` / `device`, typed pin bindings, `cell`, and
  the `on <event>` / `every <duration>` reactive model.
- **A real device model.** `gpio` is an ordinary std-lib `device`, not a compiler
  built-in ("no privileged built-ins"); pin ops lower to target-neutral register accesses.
- **Compiler-computed concurrency.** Two reactions sharing one `cell` get a
  priority-ceiling **critical section computed automatically** — no `disable_irq` in
  source; on metal it lowers to real BASEPRI masking. Single-owner cells are *proven*
  section-free.
- **Static safety checks.** Binding two things to one physical pad is a **compile error**;
  the static RAM budget is checked against the chip's memory (no dynamic allocation).
- **A deterministic simulator:** a virtual clock, scripted event injection, mock register
  side effects, and a structured trace — reproducible, no wall-clock dependence.
- **On-metal codegen** (`--target metal-nrf52840`): generated linker script, vector
  table, reset/startup, ordered MMIO with barriers, `every`→SysTick, and
  `on <pin>.falling`→GPIOTE/NVIC — a freestanding image with no libc.
- **Fault decoding:** an address-ownership map turns a faulting address into a
  language-level diagnosis (*"no device claims this address"* / *"within device
  `gpio0`"*), in both the simulator and the metal `HardFault_Handler`.

This completes the Phase-0 reactive core. [Phase 1](roadmap.md) adds composed devices over
buses — both `i2c` and `spi` controllers, with composed sensors.

## For AI agents

This documentation is published in the [llms.txt](https://llmstxt.org/) format, so an
agent can ingest it directly:

- [`llms.txt`](https://silica-lang.github.io/si/llms.txt) — a curated index of the docs.
- [`llms-full.txt`](https://silica-lang.github.io/si/llms-full.txt) — the entire docs
  concatenated into a single file for one-fetch ingestion.

## Where to go next

- [Installation & Build](getting-started/install.md) — get the compiler building.
- [Your First Program](getting-started/first-program.md) — hello world, then blink + button.
