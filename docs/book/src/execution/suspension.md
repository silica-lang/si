# Run-to-completion & Suspension

How a reaction runs once it is dispatched is the central execution decision in Silica. The short
version: a reaction runs **to completion between explicit yield points**. It never blocks the
scheduler — it either finishes or *yields*.

## Why not strict run-to-completion?

The simplest model is strict run-to-completion (RTC): each handler runs to its end before any
other runs. It needs no per-handler stack and is trivially analyzable. But a handler that needs a
slow bus transaction would have to **busy-block**, starving everything else. With device
composition layered over I²C or SPI (see [composition](../language/composition.md)), that is
untenable — one sensor read would freeze the whole system.

The alternative is suspendable handlers: a handler may suspend at explicit points and resume
later. That requires the compiler to transform each handler into a state machine whose locals
across a suspension point are captured in a statically-sized frame. It is more compiler work, and
it introduces the reentrancy that [atomicity](atomicity.md) exists to manage.

## Run-to-completion between yields

Silica takes the middle path. A handler never blocks the scheduler; it either completes or
**yields**, and yields are explicit and typed. An op that may suspend is marked `yields` in its
signature (`op read_temp() ... yields`), so suspension is visible in the type — exactly like
fallibility is. Between two yields, a handler is strict RTC: straight-line code plus bounded
loops.

This buys three things:

- device composition over slow buses is natural — the bus op yields, and other reactions run
  meanwhile;
- each *segment* between yields stays analyzable for worst-case execution time;
- the "no hidden state" promise holds — every suspension is spelled `yields` or `await`, never
  implicit.

The cost is the state-machine lowering and the reentrancy it creates: while one handler is
yielded, another can run and touch shared cells. That cost is paid once in the compiler and the
[atomicity construct](atomicity.md), and it buys the entire composed-device story.

## Suspending: `await`

`await <cond> within <d> else fault <code>` is a bounded *suspending* wait. On reaching the
`await`, the handler yields to the scheduler; its condition is re-checked on a cadence until it
holds — then the handler resumes — or the `within` budget elapses, raising `fault <code>` into
the reaction's Layer-2 disposition. Because the handler is suspended rather than spinning, other
reactions run while it waits.

```si
cell ready : u32 = 0
cell done  : u32 = 0

// The worker waits (suspended) for `ready`, then records progress.
every 100ms on fault skip {
  await ready == 1 within 500ms else fault not_ready
  done = done + 1
}

// A separate reaction makes the awaited condition true.
every 130ms {
  ready = 1
}
```

Here the worker fires, awaits `ready == 1`, and suspends. While it is suspended the second
reaction sets `ready = 1`; the worker's next re-check sees it and resumes. With no second
reaction, the `await` would time out and the fault disposition would fire. (Full example:
`examples/await.si`.)

## Not suspending: `poll`

`await` has a non-suspending sibling, `poll <cond> within <d> else fault <code>`, which spins
until the condition holds and does **not** yield the scheduler. It is for sub-microsecond
hardware waits — a UART transmit-empty flag, for instance — where yielding would cost more than
the wait itself. If the bound elapses it raises the fault, which flows to the reaction's Layer-2
disposition.

```si
op send(b: u32) -> () or fault{timeout} {
  poll SR.txe == 1 within 2ms else fault timeout
  DR = b
}
```

Reach for `poll` for tight hardware flags and `await` for anything that takes long enough that
other reactions should run meanwhile. (Full example: `examples/poll_usart.si`.)
