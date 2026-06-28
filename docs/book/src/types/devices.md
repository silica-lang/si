# Devices, Interfaces & Capabilities

In Silica, hardware is described with *types*. A device isn't a node in a
schema you fill out — it's a type whose structure the compiler understands and
checks. Three related ideas work together here: **devices** (a concrete piece
of hardware and its behaviour), **interfaces** (the abstract contract a device
provides or requires), and **capabilities** (unforgeable grants that gate
access).

> Silica is early and experimental. The notes below call out which parts are
> implemented today and which are still on the way.

## Devices as types

A `device` type is an *interface-with-behaviour over a register-backed (or
device-backed) resource* — not a Devicetree node with a schema. A device
declaration contributes to type-checking through four sections:

- **`regs`** — the memory-mapped layout (leaf devices only). The types here are
  register and bit-field types; see [Registers & Bit-fields](registers.md).
- **`config`** — typed fields with `where` constraints, checked when the device
  is instantiated.
- **`needs`** — typed *relations* to other devices: a `clock`, a `reset`, a
  `power_domain`, an `irq`, a `bus`, or a `dma` channel. These replace
  phandles. A `needs` is satisfied by a named reference whose type matches.

  Clock, reset, and power are **first-class relations**, not a flat
  `clock_source` scalar. A peripheral commonly needs its clock *enabled*, its
  reset *deasserted*, and its power domain *up* before any operation is legal —
  so these are devices and relations the compiler can order in generated
  startup, rather than assumed-on globals. (v1 *freezes the clock tree after
  init*; typed dynamic frequency changes are deferred, not foreclosed.)
- **`ops`** — verbs, each optionally guarded by `when <state>` and typed for
  fallibility (see [Faults & Fallibility](faults.md)), latency, and capability.

### Typestate: `when` and `become`

An op may be guarded `when <state>`, and an op may transition the device with
`become <state>`. The compiler tracks each device's provable state through a
reaction's straight-line control flow. Where it can prove from the flow that the
device is in the required state at a call site — for example, a `become ready`
dominates the call — the `when` guard is a **static typestate** check with zero
runtime cost. Where the state cannot be statically established — across an event
boundary, after a yield, or through a dynamic reference — the guard is intended
to lower to a **runtime precondition** whose violation is a hardware-level
(Layer-3) fault, never undefined behaviour.

> **Status.** The static half is implemented: devices declare `states { … }`;
> an op may be guarded `when <state>` and transition with `become <state>`. The
> resolver tracks each device's provable state through a reaction's straight-line
> flow (reset at every event boundary, since typestate is not carried across
> one). A `when S` op call where a dominating `become S` has not run is a compile
> error; naming an undeclared state is rejected. State established at boot (in
> `on sys.start`) **persists** into later reactions. The **runtime-precondition
> lowering** is also implemented: a `when <state>` call that no dominating
> `become` proves — because the state is configured in another reaction — lowers a
> runtime guard that drives the device to its safe state on a mismatch (rather than
> a conservative compile error); only a state that *no* op can establish stays a
> compile error. See `examples/typestate_runtime.si` and
> `examples/typestate_persist.si`. Across-yield preemption of a shared device's
> proven state and the rich Layer-3 site map remain follow-ups.

For more on how device types are declared and instantiated, see
[Program & board structure](../language/structure.md). For an end-to-end
typestate example, see `examples/typestate.si`.

## Interfaces: the abstract contract

An **interface** is the contract a device *provides* (`implements i2c`) or
*requires* (`needs bus: i2c`). Interfaces are how composition is typed: any
device providing `i2c` can satisfy any `needs bus: i2c`. A controller does not
need to know about the sensors that will eventually use it — they meet through
the interface.

Interfaces are **nominal with structural conformance**. Pure structural matching
would let a bus with the same op shapes but different semantics be silently
accepted, so an interface is *named* — `implements i2c` is a declared claim, not
an accident of matching signatures — *and* it carries *semantic properties* the
compiler and tools can check and version. For `i2c` those properties include
addressing mode (7- vs 10-bit), maximum bus speed, transaction atomicity
(start→stop is one indivisible unit), clock-stretching support, and bus-recovery
behaviour. A device declares the properties it requires; the controller declares
what it provides; a mismatch — a 400 kHz-only sensor on a 100 kHz-capped
controller — is a **compile error**, not a runtime surprise.

> **Status.** An interface declares `property <name> [= default]`; a controller
> adds a `provides <iface> { <name> = <value> }` block; a device constrains a
> need with `needs { bus : i2c where <expr> }`. At board-bind, the resolver
> const-evaluates the requirement against the provider's values (overlaid on the
> interface defaults). A false result — or a reference to a property the provider
> doesn't declare — is a compile error. The richer property set (atomicity,
> clock-stretch, bus-recovery) is expressible but not yet declared on the std
> interface; property values are integer/bool constants only.

How interfaces let independent devices be wired together is covered in
[Composition](../language/composition.md).

## Capabilities: unforgeable grants

**Capabilities** are unforgeable typed values that gate access. A handler can
only touch a device it has been *granted* — passed a typed reference to.
Floating-point, for instance, requires an `fpu` capability that the board only
provides if the SoC declares an FPU (see
[The number / data model](numbers.md)). A secure-enclave boundary is, in this
model, "a core with a capability boundary."

Capabilities are the through-line that keeps the confidential-computing and
coprocessor deferrals open: because they already live in the type model, adding
a boundary later is "introduce a new capability," not "retrofit the type
system."

> **What capabilities do and do not buy.** Capabilities are a *source-level
> discipline*: inside safe Silica they prevent a handler from touching a device
> it was not granted, the way a borrow checker prevents aliasing. On bare metal
> with no MPU/TrustZone/MMU, a compiled capability is **not** a hardware
> isolation boundary — a `raw` escape hatch, an FFI edge, or a hardware fault can
> step outside it. The honest claim is: capabilities prevent *accidental* misuse
> within well-typed Silica; real security isolation requires hardware support
> (MPU regions, TrustZone, an enclave) that a capability can be made to *drive*
> but does not itself provide. Keeping capabilities clean in the type model is
> what lets such a hardware boundary later be *attached* to a capability rather
> than retrofitted.

> **Status.** The FPU capability is the first concrete instance and is
> implemented (see [The number / data model](numbers.md)). The broader capability
> system — general unforgeable typed device grants and the "a handler touches
> only granted devices" check — is not yet built.

## Why `on`/`every` stay primitive while devices stay un-privileged

The compiler core knows the *binding/trigger* concepts `on` and `every`. It does
**not** know what a UART or an NVIC is. A device declares `emits <name> : event`;
`on usart2.rx_ready { ... }` binds a handler to that event source. The compiler
resolves the binding to a concrete IRQ by following the device's `needs irq`
relation into the (ordinary, std-lib) interrupt-controller device, and generates
the vector-table entry. `every` is implemented over an ordinary timer device the
same way. The primitives are control-flow constructs; the devices remain equal
citizens. Nothing about `gpio`, `uart`, or the NVIC is special to the compiler.
