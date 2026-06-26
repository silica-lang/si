# Reactive Scheduling

Silica's whole concurrency model is `on <event>` and `every <duration>`. There are no
threads, tasks, semaphores, or mutexes in the surface language — just
[reactions](../language/reactive.md). A reaction is bound to an event source (a device
`emits`, or a timer for `every`), and the runtime is an event-driven scheduler: when an event
fires, its reaction(s) run.

Priorities derive from the event source — an IRQ priority for hardware events, a timer tick
for `every`. The compiler knows the full static set of reactions and the cells each one
touches, which is what makes the scheduler analyzable (and what powers the automatic critical
sections described under [atomicity](atomicity.md)).

## The scheduler contract

"Event-driven" on its own is not a specification, so the model fixes the following. All of it
is statically sized so that RAM stays a compile-time constant (see
[memory & allocation](memory.md)).

### Bounded queues

Each event source has a statically-sized *pending capacity* — often exactly 1. That capacity is
part of the program's RAM budget, not a dynamic queue. There is no unbounded growth waiting to
surprise you at runtime.

### Overflow is explicit

When an event arrives and its pending slot is full, the policy is declared per source, so there
is never silent loss and never silent growth:

- **coalesce** — collapse to one pending (the default for level/periodic sources);
- **drop-newest** — discard the new event;
- **fault** — raise it to the overflow/fault handler.

A reaction declares its policy with an `on overflow <coalesce | drop_newest | fault>` clause
(the default is `coalesce`). When an event re-fires while an activation is in flight, the
simulator's dispatch applies it: coalesce collapses the re-fire, drop-newest discards it (with a
distinct trace entry), and `fault` drives the device to its [safe state](safe-state.md) and
stops — a system-integrity fault. On metal the trigger entry of a yielding reaction branches the
same way: coalesce/drop return; `fault` calls the safe-state path and halts.

> **Status (partly deferred).** Today the pending slot is exactly one (the common case); a
> capacity greater than 1 and a per-event-*source* declaration (rather than per-reaction) are
> follow-ups. Multi-consumer bus arbitration and a bounded per-bus wait queue beyond the
> single-yield [suspension](suspension.md) model are not yet built.

### Single live activation per reaction

A reaction has at most one in-flight activation. If its event re-fires while it is running *or
yielded*, the re-fire follows the overflow policy above (default: coalesce). Reactions are **not**
re-entrant — there is no stack of suspended activations of the same handler.

### Deterministic order

When several reactions are runnable at once, they run in a deterministic order: by source
priority, with ties broken by a stable compile-time order. Same inputs produce the same order, on
metal and in the simulator alike.

### Missed timer ticks

Missed `every` ticks follow the overrun rule: coalesced by default, and observable via the
execution trace. A periodic reaction that occasionally runs long does not silently pile up work.

## The run loop

Putting it together, the generated event loop is small and predictable: it waits for an event,
dispatches the highest-priority runnable reaction, lets it run to completion or yield, and
returns to idle. Because the reaction-to-cell access graph and the priority of every reaction are
known statically, the loop can insert exactly the critical sections it needs (see
[atomicity & interrupt-safety](atomicity.md)) and nothing more.
