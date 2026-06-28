# Targets & Codegen

Silica's compiler does not lower source straight to a target. It first emits a narrow
intermediate form — **Silica IR (SIR)** — and a *backend consumes it*. The host
[simulator](./simulator.md), the C backend, and the LLVM backend are all consumers of the
same SIR: swapping backends is "swap one consumer," not "rewrite the compiler." Two of those
consumers (C and LLVM) reach the same metal target independently and are held to identical
behaviour, so the IR boundary is not a hypothesis — it is continuously tested.

## The SIR boundary

SIR sits deliberately **below** any source-level sugar and **above** any target detail.
By the time a backend sees it:

- handlers are lowered to **explicit state machines** — suspension points resolved, frames
  sized;
- register accesses are **typed volatile loads/stores with explicit ordering** (there is no
  `volatile` keyword — ordering is a property of the op, not a type qualifier);
- the **device graph is resolved** to concrete addresses, and the schedule and vector table
  are computed;
- the **memory layout** (pools, arenas, statics, frames) is fixed as concrete sizes and
  sections;
- faults are explicit control edges, and `comptime` values are already folded.

SIR is the contract. Everything below is "just" a printer/lowering from SIR. SIR uses only
fixed-width types, explicit memory ops, and explicit control flow — nothing that *needs* a
C semantic to be meaningful — so it stays expressible in LLVM with no libc and no C runtime.

## The C backend

The first backend emits C — the fast path to real hardware and to design feedback. Each
reaction becomes a C function; the state-machine transform becomes a `switch` on an explicit
state variable; register accesses become volatile pointer writes with the required barriers;
pools become static arrays; the scheduler becomes a generated event loop plus interrupt
controller configuration. The generated C is an *implementation detail of a backend*, not a
HAL the language sits on — there is no hand-written C driver layer underneath Silica devices.

Because "C is just a printer" only holds if the printer dodges C's undefined behaviour, the
backend emits a strict **freestanding subset**:

- **Fixed-width types only** (`uint32_t` and friends) — never `int`/`long`, and no reliance
  on host word size or integer-promotion rules.
- **Explicit checked arithmetic** — overflow checks are emitted as explicit compares. The
  backend never relies on signed-overflow wraparound (which is UB) and never leaves a
  trap-on-overflow op as a bare `+`.
- **No C bitfields** — register and bit-field access lowers to explicit mask/shift on
  volatile fixed-width pointers, because C bitfield layout and access width are
  implementation-defined.
- **Explicit barriers** — compiler barriers and hardware fences are emitted where the
  ordering model requires them, rather than trusting `volatile` alone (which orders
  volatile-vs-volatile but not volatile-vs-ordinary, and implies no hardware fence).
- **No libc, no dynamic initialization, no hidden runtime** — startup is generated (below);
  there is no `__attribute__((constructor))`, no `malloc`, and no static initializers that
  run before the generated reset path.

Each of these is also exactly what the LLVM backend does, which is why holding the C backend
to them keeps SIR honest instead of letting C-isms calcify — and the LLVM backend (below) now
*proves* it, emitting the same behaviour with no C in the loop at all.

You can inspect the emitted C, or build a host binary through it:

```sh
# Compile a host program to a native binary via the C backend
cargo run -- examples/hello.si -o /tmp/hello && /tmp/hello
```

## The LLVM backend

The second backend (`--emit-llvm`) emits **textual LLVM IR** straight from SIR — a
structurally independent consumer that shares no code with the C printer. It exists to keep
the IR boundary honest: anything the LLVM backend cannot express from SIR alone is a C-ism
that leaked into the contract. In practice nothing did — the LLVM backend now reaches **full
parity with the C backend on metal**.

```sh
# Emit LLVM IR for inspection
cargo run -- --emit-llvm examples/hello.si

# Build the same bare-metal nRF52840 image through LLVM instead of C (needs llc)
cargo run -- --target metal-nrf52840 --emit-llvm examples/blink_button_nrf52840.si -o blink.elf
```

On metal it emits its own freestanding module — the `thumbv7em-none-eabi` triple, the
`.vectors` table, reset/startup, and every runtime feature the C path has: the
TIMER1/TIMER2/GPIOTE scheduler, BASEPRI critical sections, the bus yields state machine and
arbitration, `ring<T,N>`, `fixed<I,F>`, hardware-FPU float, the Layer-3 `HardFault_Handler`,
drive-safe, poll/await, deadline + watchdog, and `host_io.print` via ARM semihosting — with
**no libc and no `__builtin`** (overflow traps lower to `llvm.*.with.overflow` intrinsics, not
C helpers). Every `harness/*.sh` Renode gate takes a `BUILD=llvm` switch that runs the whole
check through this backend; the two backends are required to produce identical observable
behaviour.

## The metal target

The metal target lowers the same program to a freestanding bare-metal image. For the
nRF52840:

```sh
# Build a bare-metal nRF52840 image (needs arm-none-eabi-gcc)
cargo run -- --target metal-nrf52840 examples/blink_button_nrf52840.si -o blink.elf
```

The typed hardware model already knows the memory map (flash/RAM origin and size), the IRQ
table (from `needs irq` relations and the interrupt-controller device), and the full static
memory budget. So the artifacts that are usually hand-written are instead **generated** as
`comptime` computations over the board type:

- **Linker script** — from `board.soc.memory` regions plus the computed section sizes.
- **Vector table** — from the reset vector plus the resolved `on <irq>` bindings.
- **Reset / startup** — set the stack pointer, copy `.data` from flash to RAM, zero `.bss`,
  run device init ops in dependency order, then enter the scheduler.
- **`.data`/`.bss` init** — from the typed statics and pool declarations.

Hand-editing a linker script is not a supported workflow. Changing the memory map means
changing the board type, which re-derives everything consistently. The result is a
freestanding image with no libc.

On metal, reactions also lower to real hardware wiring: `every <duration>` becomes a
**TIMER1** compare channel (1 MHz, one channel per periodic reaction, re-armed in its IRQ),
and `on <pin>.falling` becomes a GPIOTE event routed through the NVIC. `now()`, the
`within`-deadline countdowns, and the watchdog wake cadence all run off a dedicated
**TIMER2** (1 µs resolution; the old 1 ms SysTick grid is fully retired). MMIO writes are
emitted in order with the required barriers, and the auto-computed critical sections lower to
real **BASEPRI** masking — see [atomicity](../execution/atomicity.md).

## Register access → MMIO lowering

A leaf device's `regs` lower to MMIO. A field write becomes a read-modify-write of the
owning register at `base + offset`, as a volatile access with the register type's declared
ordering. For example a multi-field write like `CR1{ enable = 1, rxneie = 1 }` lowers to a
single load, mask/set, store — multi-field writes coalesce, and a single-field write to a
write-1-to-clear register lowers correctly because the [register](../types/registers.md)
type itself declares its access semantics.

The base address comes from the instance (`usart2 : uart at 0x4000_4400`), so the same
`uart` device type is reused at every instance address with no per-instance code.

## Composed-device lowering

A composed op lowers to a **sequence of calls to the substrate device's ops**, which
themselves lower (recursively) until a leaf reaches MMIO. So `bme280.read_temp()` lowers to
`i2c.read_reg24(...)`, which lowers to a controller-leaf register sequence, which lowers to
MMIO — with any `yields` points becoming state-machine suspension/resume edges in SIR.

Composition is real and zero-HAL, but it is *transactions lowered to MMIO* — not direct MMIO
from the sensor. The sensor talks to its bus; the bus controller talks to the registers.

## The "sim ≡ metal" gate

The same program must produce the same observable behaviour in the simulator and on metal.
The `harness/metal_vs_sim.sh` gate asserts exactly that: it builds the metal image, runs it
on the nRF52840 in Renode, and checks that the LED sequence matches the
[simulator](./simulator.md) trace from the same source.

```sh
# End-to-end "sim ≡ metal" gate (needs arm-none-eabi-gcc + Renode)
RENODE=/path/to/renode ./harness/metal_vs_sim.sh

# ...and the same gate through the LLVM backend
RENODE=/path/to/renode BUILD=llvm ./harness/metal_vs_sim.sh
```

The simulator, the C backend, and the LLVM backend are all consumers of the same SIR, so this
gate — run for each backend, across the whole [harness suite](https://github.com/silica-lang/si/tree/main/harness) —
is what keeps all three, and the IR boundary between them, honest.
