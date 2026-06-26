# Examples Tour

The [`examples/`](https://github.com/silica-lang/si/tree/main/examples) directory holds the
working `.si` programs that exercise the compiler — host, simulator, and nRF52840 metal.
Most run in the [simulator](tooling/simulator.md) with `silicac --sim <file>`; the
`*_nrf52840.si` programs target real metal (`--target metal-nrf52840`).

## Basics

- [`hello.si`](https://github.com/silica-lang/si/blob/main/examples/hello.si) — the
  smallest program: print a line on `sys.start`.
- [`blink.si`](https://github.com/silica-lang/si/blob/main/examples/blink.si) — toggle an
  LED on a periodic `every` reaction.
- [`blink_button.si`](https://github.com/silica-lang/si/blob/main/examples/blink_button.si)
  — the canonical reactive-core slice: a timer and a button share one `cell`.
- [`match.si`](https://github.com/silica-lang/si/blob/main/examples/match.si) — pattern
  matching with `match`.
- [`casts.si`](https://github.com/silica-lang/si/blob/main/examples/casts.si) — explicit
  numeric [casts](types/numbers.md).

## Concurrency & timing

- [`atomic.si`](https://github.com/silica-lang/si/blob/main/examples/atomic.si) —
  shared-cell [atomicity](execution/atomicity.md) and auto-computed critical sections.
- [`await.si`](https://github.com/silica-lang/si/blob/main/examples/await.si) —
  [suspension](execution/suspension.md) with `await`.
- [`instant.si`](https://github.com/silica-lang/si/blob/main/examples/instant.si) —
  reading instants from the [time](types/time.md) model.
- [`deadline.si`](https://github.com/silica-lang/si/blob/main/examples/deadline.si) —
  bounded waits against a deadline.
- [`safe_state.si`](https://github.com/silica-lang/si/blob/main/examples/safe_state.si) —
  driving outputs to a [safe state](execution/safe-state.md) on fault.
- [`watchdog.si`](https://github.com/silica-lang/si/blob/main/examples/watchdog.si) — the
  scheduler-fed hardware watchdog.

## Buses & peripherals

- [`bus_speed.si`](https://github.com/silica-lang/si/blob/main/examples/bus_speed.si) —
  configuring bus clock speed.
- [`poll_usart.si`](https://github.com/silica-lang/si/blob/main/examples/poll_usart.si) —
  polling a USART for data.
- [`ring.si`](https://github.com/silica-lang/si/blob/main/examples/ring.si) — a bounded
  ring buffer.
- [`sensor_i2c.si`](https://github.com/silica-lang/si/blob/main/examples/sensor_i2c.si) —
  a composed sensor over I²C.
- [`sensor_spi.si`](https://github.com/silica-lang/si/blob/main/examples/sensor_spi.si) —
  a composed sensor over SPI.

## Type-system features

- [`overflow.si`](https://github.com/silica-lang/si/blob/main/examples/overflow.si) —
  checked arithmetic [overflow](types/numbers.md).
- [`overflow_policy.si`](https://github.com/silica-lang/si/blob/main/examples/overflow_policy.si)
  — selecting an overflow policy.
- [`overlay.si`](https://github.com/silica-lang/si/blob/main/examples/overlay.si) — typed
  [overlays](language/overlays.md).
- [`typestate.si`](https://github.com/silica-lang/si/blob/main/examples/typestate.si) —
  typestate to track a device's lifecycle.
- [`fpu.si`](https://github.com/silica-lang/si/blob/main/examples/fpu.si) —
  floating-point usage and FPU gating.
- [`stack_budget.si`](https://github.com/silica-lang/si/blob/main/examples/stack_budget.si)
  — static stack accounting against the chip's [memory](execution/memory.md).

## nRF52840 metal targets

These build freestanding images for the nRF52840 with `--target metal-nrf52840`:

- [`blink_button_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/blink_button_nrf52840.si)
  — blink + button on real metal.
- [`boot_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/boot_nrf52840.si)
  — minimal boot / startup.
- [`button_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/button_nrf52840.si)
  — a button reaction via GPIOTE/NVIC.
- [`deadline_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/deadline_nrf52840.si)
  — deadlines on metal.
- [`fault_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/fault_nrf52840.si)
  — a forced fault decoded by the `HardFault_Handler`.
- [`overflow_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/overflow_nrf52840.si)
  — overflow handling on metal.
- [`bus_interleave_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/bus_interleave_nrf52840.si)
  — interleaved bus transactions.
- [`bus_watchdog_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/bus_watchdog_nrf52840.si)
  — the hardware watchdog guarding a bus.
