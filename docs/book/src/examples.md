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

## Numbers & data

- [`casts.si`](https://github.com/silica-lang/si/blob/main/examples/casts.si) — explicit
  numeric [casts](types/numbers.md) with implicit narrowing rejected.
- [`overflow.si`](https://github.com/silica-lang/si/blob/main/examples/overflow.si) —
  checked arithmetic [overflow](types/numbers.md) and the wrapping/saturating operators.
- [`fixed.si`](https://github.com/silica-lang/si/blob/main/examples/fixed.si) —
  `fixed<I,F>` fixed-point math (mul/div, decimal + voltage literals).
- [`fpu.si`](https://github.com/silica-lang/si/blob/main/examples/fpu.si) — `float` values
  and FPU capability gating.
- [`ring.si`](https://github.com/silica-lang/si/blob/main/examples/ring.si) — the bounded
  `ring<T,N>` queue.
- [`reg_multifield.si`](https://github.com/silica-lang/si/blob/main/examples/reg_multifield.si)
  — a multi-field single-write to a [register](types/registers.md).

## Faults & matching

- [`fault_match.si`](https://github.com/silica-lang/si/blob/main/examples/fault_match.si) —
  `match` over an op's `ok` / `fault <code>` result.
- [`fault_match_composed.si`](https://github.com/silica-lang/si/blob/main/examples/fault_match_composed.si)
  — the same, one composition hop up from the bus transaction.
- [`safe_state.si`](https://github.com/silica-lang/si/blob/main/examples/safe_state.si) —
  driving outputs to a [safe state](execution/safe-state.md) on fault.

## Concurrency & timing

- [`atomic.si`](https://github.com/silica-lang/si/blob/main/examples/atomic.si) —
  shared-cell [atomicity](execution/atomicity.md) and auto-computed critical sections.
- [`await.si`](https://github.com/silica-lang/si/blob/main/examples/await.si) —
  [suspension](execution/suspension.md) with `await`.
- [`instant.si`](https://github.com/silica-lang/si/blob/main/examples/instant.si) —
  reading instants from the [time](types/time.md) model with `now()`.
- [`deadline.si`](https://github.com/silica-lang/si/blob/main/examples/deadline.si) —
  bounded waits against a deadline.
- [`overflow_policy.si`](https://github.com/silica-lang/si/blob/main/examples/overflow_policy.si)
  — the scheduler `on overflow coalesce|drop_newest|fault` policy.
- [`watchdog.si`](https://github.com/silica-lang/si/blob/main/examples/watchdog.si) — the
  scheduler-fed hardware watchdog.

## Buses & peripherals

- [`bus_speed.si`](https://github.com/silica-lang/si/blob/main/examples/bus_speed.si) —
  configuring bus clock speed via interface properties.
- [`poll_usart.si`](https://github.com/silica-lang/si/blob/main/examples/poll_usart.si) —
  polling a USART for data.
- [`sensor_i2c.si`](https://github.com/silica-lang/si/blob/main/examples/sensor_i2c.si) —
  a composed sensor over I²C.
- [`sensor_spi.si`](https://github.com/silica-lang/si/blob/main/examples/sensor_spi.si) —
  a composed sensor over SPI.
- [`sensor_temp_c.si`](https://github.com/silica-lang/si/blob/main/examples/sensor_temp_c.si)
  — a BME280 with datasheet fixed-point temperature compensation.

## Devices & typestate

- [`overlay.si`](https://github.com/silica-lang/si/blob/main/examples/overlay.si) — typed
  compile-time [overlays](language/overlays.md) (`set` / `remove`).
- [`typestate.si`](https://github.com/silica-lang/si/blob/main/examples/typestate.si) —
  `when`-typestate tracking a [device](types/devices.md)'s lifecycle (static half).
- [`typestate_persist.si`](https://github.com/silica-lang/si/blob/main/examples/typestate_persist.si)
  — state configured at boot that persists into later reactions.
- [`typestate_runtime.si`](https://github.com/silica-lang/si/blob/main/examples/typestate_runtime.si)
  — runtime-precondition lowering to a safe-state guard.

## Budgets

- [`stack_budget.si`](https://github.com/silica-lang/si/blob/main/examples/stack_budget.si)
  — static stack accounting against the chip's [memory](execution/memory.md).

## nRF52840 metal targets

These build freestanding images for the nRF52840 with `--target metal-nrf52840` — through
the C backend by default, or through the LLVM backend by adding `--emit-llvm`:

- [`blink_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/blink_nrf52840.si)
  — blink on metal (`every`→TIMER1).
- [`blink_button_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/blink_button_nrf52840.si)
  — blink + button on real metal.
- [`boot_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/boot_nrf52840.si)
  — minimal boot / startup.
- [`button_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/button_nrf52840.si)
  — a button reaction via GPIOTE/NVIC.
- [`every_timer_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/every_timer_nrf52840.si)
  — `every` on a hardware TIMER1 compare channel.
- [`uptime_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/uptime_nrf52840.si)
  — `now()` uptime from the TIMER2 counter.
- [`deadline_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/deadline_nrf52840.si)
  — `within`-deadline enforcement gating the watchdog.
- [`poll_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/poll_nrf52840.si)
  — a bounded `poll … within … else fault` on metal.
- [`await_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/await_nrf52840.si)
  — a suspending `await` on metal.
- [`await_interleave_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/await_interleave_nrf52840.si)
  — a peer reaction running during an `await` suspension.
- [`fault_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/fault_nrf52840.si)
  — a forced fault decoded by the `HardFault_Handler`.
- [`overflow_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/overflow_nrf52840.si)
  — overflow-trap safe-state on metal.
- [`fixed_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/fixed_nrf52840.si)
  — fixed-point math on metal.
- [`float_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/float_nrf52840.si)
  — runtime float on the hardware FPU.
- [`ring_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/ring_nrf52840.si)
  — a `ring<T,N>` on metal.
- [`semihosting_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/semihosting_nrf52840.si)
  — `host_io.print` via ARM semihosting.
- [`bus_interleave_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/bus_interleave_nrf52840.si)
  — a button running during a sensor's bus suspension.
- [`bus_contend_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/bus_contend_nrf52840.si)
  — two reactions arbitrated on one shared bus.
- [`bus_watchdog_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/bus_watchdog_nrf52840.si)
  — the hardware watchdog guarding a wedged bus.
- [`flash_over_budget_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/flash_over_budget_nrf52840.si)
  — an image rejected by the flash-budget gate.
- [`stack_over_budget_nrf52840.si`](https://github.com/silica-lang/si/blob/main/examples/stack_over_budget_nrf52840.si)
  — an image rejected by the measured-stack budget gate.
