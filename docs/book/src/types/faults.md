# Faults & Fallibility

Things go wrong on hardware: a sensor NAKs, a transfer times out, a reading lands
out of range. Silica makes *fallibility* part of the type, so you cannot
accidentally ignore a failure — but it deliberately keeps the error shape simple,
so you only ever handle one kind of thing.

This page covers how fallibility is expressed and propagated in your code. The
execution-side view — how faults flow through the reactive model and how hardware
traps are decoded — lives in [Execution-level faults](../execution/faults.md).

## Three layers, kept distinct

Silica separates faults into three layers. The first is what you reach for in
everyday code; the second and third are how faults travel once a handler can't
deal with them locally.

## Layer 1 — expected operational failures: fallibility in the type

A fallible op returns `T or fault`. You cannot obtain the `T` without discharging
the fault path — ignoring it is a **compile error**. This kills the
errno / unchecked-return-code footgun at the type level. You discharge a fault by
pattern match, or with the propagation operator `?`, which forwards the fault
outward:

```si
op read_temp() when ready -> fixed<16,16> or fault yields {
  let raw = bus.read_reg24(addr, REG_TEMP)?   // on fault, return it from read_temp
  return compensate(raw)
}
```

### Faults are opaque

There is **one** `fault` type, with a queryable `code` inside — not a typed error
zoo:

```si
match usart2.write(b) {
  ok                -> { }
  fault f if f.code == timeout -> retry_later()
  fault f           -> escalate(f)
}
```

This is chosen for *learnability* and *agentic regularity* over fine-grained
expressiveness: a human and an agent only ever handle one error shape. The cost —
you cannot get the compiler to prove you handled every distinct error *variant* —
is accepted deliberately.

### Codes are still declared

Even though the runtime value is the single opaque `fault`, each op *declares* the
fault codes it can raise:

```si
op read_reg(...) -> u8 or fault{nak, timeout, arblost}
```

So the set is *known statically* even though the value is opaque at runtime. This
buys back what matters without a typed-error zoo: tooling and the agent see
exactly which failures to expect; the simulator can fault-inject the right set;
and a `match` or fault disposition can be checked for **completeness against the
declared codes**, flagged when it ignores one a device documents (for example,
silently dropping `crc` on a sensor read). Recovery in embedded systems routinely
depends on *which* failure occurred — `timeout` vs `nak` vs `exhausted` vs
`overflow` vs a `when`-precondition violation — so the codes are first-class to
docs, tools, and validation, while the value stays one regular shape to handle.

> **Status.** The `match` statement is the surface conditional
> (`match <expr> { <lit> => …, _ => … }`). Matching is enforced **total**: a `_`
> wildcard arm is required (else a compile error) and duplicate literal arms are
> rejected. Today patterns are integer/bool literals only; the `ok` / `fault f`
> op-result patterns and exhaustiveness against an op's *declared* fault codes
> build on this and are not yet wired.

## Layer 2 — propagation through the reactive model: fault disposition

Fallibility composes *within* a handler via `?`, but a handler has **no caller to
unwind to** — it was invoked by an event, not a function call. So each reaction
declares a **fault disposition**: the reactive-model equivalent of a catch block,
attached to the *event source* rather than a stack frame.

```si
on sensor.sample_ready on fault retry(max = 3) {   // retry up to 3×
  let t = bme280.read_temp()?
  log_sample(t)
}

every 1s on fault skip {                            // drop this tick, keep scheduling
  housekeeping()?
}
```

Dispositions are a small, fixed set with sane defaults:

- **`retry`** — re-run the reaction.
- **`skip`** — drop this event, keep running.
- **`safe`** — drive devices to their [safe state](../execution/safe-state.md).
- **`escalate`** — raise to the Layer-3 handler.

The default, if unstated, is conservative (`escalate`) so that an unhandled fault
is never silently dropped.

## Layer 3 — hardware faults

Layer 3 covers hardware faults — HardFault, bus fault, mem fault — and
`when`-precondition violations. A **language-level fault decoder** maps a hardware
trap back to language-level truth: "handler *X* touched device *Y* outside its
valid `when` state," or "MMIO to an address no device claims." The decoder is
possible *because* states are explicit and every address range has a declared
owning device.

The mechanics of the decoder, and how it ties into safe-state, are described in
[Execution-level faults](../execution/faults.md).
