# The Deterministic Simulator

In Silica a device is just `regs` + `ops`. On the host, that same device becomes a
**mock object implementing the same `ops`** — no MMIO, no operating-system dependency.
The simulator runs the *same* program your hardware will run, with device ops dispatched
to host models instead of memory-mapped registers. Sim is where you develop blink + button
before touching metal, and where CI runs.

Because the runtime deliberately contains no UNIX-isms, the simulator is **portable to
macOS, Windows, and Linux** — running in the simulator is not a privileged "develop on
Linux only" mode. That portability is a first-class constraint on the runtime, the same
constraint that keeps the language embedded-native.

## Running a program in the simulator

Pass `--sim` to run a program in the host simulator:

```sh
cargo run -- --sim examples/blink_button.si
```

You will get a structured trace of what the program did — which reactions fired, the
virtual time at each step, and the values written to each device. See
[Installation](../getting-started/install.md) for toolchain setup.

## Deterministic by default

"Same program, sim and metal" only helps if a run is reproducible. The simulator is
**deterministic by default**, with no dependence on wall-clock time:

- **A virtual clock** that advances only when the program would *wait*. There is no real
  sleeping; time jumps forward to the next scheduled event. Two runs of the same program
  produce the same trace.
- **Explicit event injection.** Inputs, IRQs, and bus responses arrive at scripted
  virtual-time points — not whenever the host happens to schedule a thread.
- **A fixed, documented order for simultaneous IRQs**, matching the scheduler's
  deterministic ordering. Events that land at the same virtual instant resolve the same
  way every time.
- **Modelled register side-effects** — write-1-to-clear, read-clears, and reset values
  behave like real [registers](../types/registers.md), so a mock device is not a polite
  fiction that ignores hardware semantics.
- **A first-class fault-injection API**, keyed to each op's declared fault codes.

Nondeterminism — jitter, random inputs, race exploration — is available, but it is
**opt-in and seeded**, so a failing run is always replayable. That is what lets the
simulator serve as CI rather than a flaky approximation.

## `sim` blocks

A scenario lives in a `sim` block alongside the program it drives. It scripts events at
virtual-time points and bounds the run. From `examples/blink_button.si`:

```si
// Deterministic scenario: press the button at 1.2s and 1.8s, run 3s of
// virtual time.
sim blink_demo for blink {
  inject btn_user.falling at 1200ms
  inject btn_user.falling at 1800ms
  run until 3000ms
}
```

- `inject <event> at <time>` schedules an event — here a falling edge on the
  `btn_user` pin — at a precise point on the virtual clock.
- `run until <time>` advances the virtual clock to that bound and stops.

The simplest scenario injects nothing and just runs for a fixed window, for example
`sim app_sim for app { run until 2100ms }`.

## Injecting faults

The same scripting mechanism can inject hardware faults at a faulting address. The
simulator decodes each address against the board's address-ownership map into a
language-level diagnosis — the same decode the on-metal `HardFault_Handler` performs.
From `examples/fault_nrf52840.si`:

```si
sim fault_demo_sim for fault_demo {
  inject fault 0x4001_0000 at 800ms   // unclaimed peripheral address
  inject fault 0x5000_0504 at 1200ms  // inside gpio0's MMIO (OUT register)
  run until 1500ms
}
```

The first address belongs to no device ("no device claims this address"); the second falls
inside a device's MMIO region ("within device `gpio0`"). For how this decoding fits into
the broader model, see [the fault model](../execution/faults.md).

Because the simulator *is* a runtime that knows full language-level state — which reactions
exist, each device's `when` state, every cell's value — it is also the natural place to
prototype the graph-aware debug model that the on-metal decoder and tooling consume.
