# The Three-Layer Fault Model

Silica's [fault types](../types/faults.md) describe three layers in type terms; this page is the
execution view — how each layer actually runs.

## The three layers

- **Layer 1** discharges or propagates within a handler, via `?` or `match`. This is the ordinary
  fallible-op flow: a faulting op's error is handled on the spot or passed up.
- **Layer 2** catches at the reaction boundary, via the reaction's declared **disposition** —
  `retry`, `skip`, `safe`, or `escalate`. A fault that reaches the edge of a reaction is resolved
  according to the disposition you declared on it.
- **Layer 3** is the hardware trap path: the HardFault / bus-fault / mem-fault handler. This is
  where a fault that hardware caught — not the language — gets turned back into a language-level
  diagnosis.

## Layer 3: address-ownership decoding

Alongside debug info, the compiler emits two tables:

- an **address-ownership map** — which device claims each MMIO range, and where flash and RAM
  regions live;
- a **site map** — each code site mapped to its enclosing handler, the device/op it touches, and
  the `when` state expected there.

On a HardFault or bus/mem fault, the decoder reads the faulting address and PC and produces a
language-level diagnosis instead of a raw register dump. For example:

> handler `pump_ctrl` wrote `valve.regs.CR` while valve was in state `closed`, which forbids it

or, for an address no device claims:

> store to 0x4002_0000 — no device claims this address

This is the same graph-aware information the agent uses to debug, reused at fault time.

## Trying it: `inject fault`

In the [simulator](../tooling/simulator.md), a `sim` block can inject hardware faults at chosen
times, and the simulator decodes each faulting address against the board's address-ownership map:

```si
sim fault_demo_sim for fault_demo {
  inject fault 0x4001_0000 at 800ms   // unclaimed peripheral address
  inject fault 0x5000_0504 at 1200ms  // inside gpio0's MMIO (OUT register)
  run until 1500ms
}
```

The first address belongs to no device, so the decoder reports an unclaimed-address store; the
second lands inside `gpio0`'s MMIO region (its OUT register), so the decoder names the device and
register. (Full example: `examples/fault_nrf52840.si`.)

On metal the same tables back the `HardFault_Handler`: the trap path reads the faulting address
and PC and runs the same decode, so the diagnosis you see in the simulator is the diagnosis you
get on hardware. Both the C and the LLVM backend emit this decoder against the identical
ownership table.
