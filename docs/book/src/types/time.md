# Time as a Type

A point in time and a length of time are different things, and confusing them is
a classic embedded bug. Silica makes them *different types*, so a unit error
becomes a type error the compiler catches.

## Instants vs durations

`duration` and `instant` are **distinct types**:

```si
let deadline : instant  = now() + 500ms   // ok: instant + duration -> instant
let bad                 = now() + 5       // compile error: instant + (untyped) int
let elapsed : duration  = now() - start   // ok: instant - instant -> duration
```

- An **`instant`** is a reading of the monotonic clock — a point in time.
- A **`duration`** is a length of time: a count of ticks in a *known monotonic
  tick domain* whose rate is derived from the board's clock topology. Because the
  typed hardware model already knows the timer clock, `500ms` lowers to an exact
  tick count for *this* board.

`now()` returns the current time as an `instant`. Arithmetic is defined so that
only sensible combinations type-check:

| Expression | Result |
| --- | --- |
| `instant - instant` | `duration` (elapsed time) |
| `instant ± duration` | `instant` (shift a point in time) |
| `instant + instant` | rejected — adding two absolute times is meaningless |
| `now() + <bare int>` | rejected — a raw integer is not a duration |

### Why mixing is a type error

Adding two absolute times, or adding a bare integer to a time, has no physical
meaning — so the compiler rejects it. A bare `5` is not a `duration`; only a
duration literal such as `500ms` (or a value typed `duration`) can shift an
`instant`. This is what makes `now() + 500ms` valid while `now() + 5` is a
compile error.

In `examples/instant.si`, each tick records the moment it ran and reports the
period since the previous tick:

```si
cell last   : instant  = 0   // when the previous tick ran
cell period : duration = 0   // measured gap between ticks
cell ticks  : u32      = 0

every 100ms {
  ticks  = ticks + 1
  period = now() - last   // instant - instant → duration
  last   = now()          // instant ← instant
}
```

`now() - last` is an `instant - instant`, so it yields a `duration` — exactly the
right type for the `period` cell.

> **Status.** `instant` and `duration` are distinct types (both 64-bit
> nanoseconds at runtime), and `now()` reads the clock — the sim's virtual time, a
> host monotonic read, or, on metal, a **TIMER2**-driven uptime counter at **1 µs**
> resolution (a hardware CAPTURE of the live counter combined with a software wrap
> high word; SysTick is retired). The resolver enforces the arithmetic above and
> rejects `instant + instant`, `now() + <bare int>`, scaling an instant, comparing
> an instant to a non-instant, and assigning an instant to a non-instant cell. A
> duration literal (`500ms`) is kept type-distinct from a bare integer (`5`). Not
> yet enforced: the exact-or-error tick-rate conversion and `rounded` modes below.

## Deadlines build on the same model

The representation is chosen so that deadline / WCET annotations attach later
without redesign. A reaction may already declare a `within <d>` budget — the
wall-clock it is allowed between firing and returning to idle. In
`examples/deadline.si`, a yielding sensor read is given a budget, and overrunning
it starves the watchdog and resets the system:

```si
// The read must complete within 5ms of the tick, else the watchdog resets.
every 1000ms within 5ms on fault retry(max = 3) {
  let t = sensor.read_temp()?
  samples = samples + 1
}
```

Today `within` is a runtime-checked bound; the same annotation can later feed a
static schedulability analysis. (The `on fault` disposition is covered in
[Faults & Fallibility](faults.md).)

## Conversion and jitter

Lowering `500ms` to ticks has rules. The value is converted at compile time
against the board's tick rate, and **conversion must be exact, or it is a compile
error** — unless an explicit rounding mode is given (`500ms rounded nearest`) — so
a period the hardware cannot represent fails loudly rather than drifting
silently.

A periodic `every` is **fixed-rate**: the next deadline is computed from the
*scheduled* time, not from handler completion, so handler execution time does not
accumulate as drift. If a periodic handler **overruns** its own period, the
default is to **coalesce** — the missed tick is dropped, not queued, because a
backlog of stale ticks is worse than skipping — and the overrun is observable via
the trace ring. Clock *drift* (crystal tolerance, temperature) is a physical fact
the type system does not pretend to remove; it surfaces as the bound on `instant`
accuracy, not as a guarantee.
