# Status & Roadmap

Silica is deliberately a **"toy"** — an intellectual exercise — with an aspirational
long-term ceiling: *potentially replacing an RTOS like Zephyr for personal projects.* That
ceiling is **not a v1 deliverable**; it is a *foreclosure constraint*. Every design
decision is checked against it, but the deferred feature list stays short on purpose.
Foreclosure lives in only two places — the type system and the memory model — so those are
designed conservatively and reviewed hardest.

The phases below describe the path. Phase 0 is done; everything after it is *deferred, not
foreclosed*.

## Phase 0 — the reactive core (done)

Enough to run **blink + button on one open board, in simulator then on metal, via the C
backend**. Delivered:

- Minimal grammar, parser, and resolver; `device` / `board` / `program` / `on` / `every` /
  `cell`; the leaf `gpio` and `timer` device types.
- The automatic [atomicity](execution/atomicity.md) construct — two reactions sharing a
  cell get a priority-ceiling critical section, no manual `disable_irq`.
- SIR (Silica IR) plus the C backend, generated startup/linker, and the deterministic
  [simulator](tooling/simulator.md).
- A **minimal pin/pad model** (mux, pull, direction) where binding two things to one pad is
  a static error, and a **structured trace ring buffer** so first bring-up is debuggable.

The machine-checkable acceptance gates are CI assertions, not prose: **no dynamic
allocation** (`.bss` + `.data` + computed pool/frame/stack sizes equal total RAM exactly),
**deterministic pin muxing** (two bindings to one pad is a static error), and **barrier
insertion** (the emitted C contains the required fences around register writes and before
IRQ enable).

## Phase 1 — composition + faults

Interfaces, the `i2c` / `spi` controller leaf devices, composed sensors (e.g. a BME280 over
I²C, a BMP280-style sensor over SPI), `yields` / suspension lowering, and the three-layer
fault model including [safe-state](execution/safe-state.md) and the scheduler-fed hardware
watchdog. This is where the device-composition keystone is proven against real silicon —
deliberately against *awkward* parts (clock-stretching, `w1c` / `pop-on-read` registers),
not just a clean I²C temperature sensor.

## Phase 2 — agent edit surface + facts

Typed [overlays](language/overlays.md) as the agent edit API; the DTS→Silica transpiler to
harvest board facts from the Devicetree corpus; graph-aware debug info v1 from the
simulator; and self-versioning.

## Phase 3 — LLVM backend

A second consumer of SIR, validating the C-purity guard and making the "replace Zephyr"
path structurally real. No language changes are expected — this is the proof that none were
needed.

## Phase 4+ — deferred, demand-ordered

Pulled from the deferred register as real projects need them (protocol state machine →
flash/DFU → filesystem → richer observability), each as an *instance* of an existing
mechanism rather than a new one.
