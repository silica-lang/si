# Status & Roadmap

Silica is deliberately a **"toy"** — an intellectual exercise — with an aspirational
long-term ceiling: *potentially replacing an RTOS like Zephyr for personal projects.* That
ceiling is **not a v1 deliverable**; it is a *foreclosure constraint*. Every design
decision is checked against it, but the deferred feature list stays short on purpose.
Foreclosure lives in only two places — the type system and the memory model — so those are
designed conservatively and reviewed hardest.

The phases below describe the path. Phases 0–3 are substantially **done** — the reactive
core, composition + faults, the agent edit surface, and a second (LLVM) backend at metal
parity all run today; everything after them is *deferred, not foreclosed*.

## Phase 0 — the reactive core (done)

Enough to run **blink + button on one open board, in simulator then on metal**. Delivered:

- Minimal grammar, parser, and resolver; `device` / `board` / `program` / `on` / `every` /
  `cell`; the leaf `gpio` and `timer` device types.
- The automatic [atomicity](execution/atomicity.md) construct — two reactions sharing a
  cell get a priority-ceiling critical section, no manual `disable_irq`.
- SIR (Silica IR) plus the C backend, generated startup/linker, and the deterministic
  [simulator](tooling/simulator.md).
- A **minimal pin/pad model** (mux, pull, direction) where binding two things to one pad is
  a static error, and a **structured trace ring buffer** so first bring-up is debuggable.

The machine-checkable acceptance gates are CI assertions, not prose: **no dynamic
allocation** (`.bss` + `.data` + computed pool/frame/stack sizes fit total RAM, with the
stack bound *measured* from the toolchain call-graph), **deterministic pin muxing** (two
bindings to one pad is a static error), and **barrier insertion** (the emitted code contains
the required fences around register writes and before IRQ enable).

## Phase 1 — composition + faults (done)

Interfaces, the `i2c` / `spi` controller leaf devices, composed sensors (a BME280 over I²C
with full datasheet fixed-point compensation, a BMP280-style sensor over SPI), `yields` /
suspension lowering (a real IRQ-driven state machine on metal), and the three-layer fault
model including [safe-state](execution/safe-state.md) and the scheduler-fed hardware
watchdog. The device-composition keystone is proven against real silicon (Renode) —
deliberately against *awkward* parts (clock-stretching, `w1c` / `pop-on-read` registers),
not just a clean I²C temperature sensor. Multiple consumers sharing one bus are
priority-arbitrated with a bounded wait queue.

## Phase 2 — agent edit surface + facts (partially done)

Typed [overlays](language/overlays.md) as the compile-time edit API (`set` / `remove`) are
**done**; `when`-typestate (`states` / `become`, with runtime-precondition lowering) is
**done**. Still ahead: the DTS→Silica transpiler to harvest board facts from the Devicetree
corpus, graph-aware debug info v1 from the simulator, the agent overlay-edit *API* surface,
and self-versioning.

## Phase 3 — LLVM backend (done)

A second, structurally independent consumer of SIR (`--emit-llvm`), now at **full parity
with the C backend on metal** — it boots on the nRF52840 and runs the scheduler, GPIOTE
events, the yields state machine + bus arbitration, rings, fixed-point, runtime float,
the Layer-3 fault decoder, drive-safe, poll/await, deadline + watchdog, and semihosting,
all held to `sim ≡ metal` in Renode. No language changes were needed — exactly the proof the
purity guard was meant to give. See [Targets & Codegen](tooling/targets.md).

## Phase 4+ — deferred, demand-ordered

Pulled from the deferred register as real projects need them (protocol state machine →
flash/DFU → filesystem → richer observability), each as an *instance* of an existing
mechanism rather than a new one.
