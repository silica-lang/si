# Metal validation harness

`metal_vs_sim.sh` is the Phase-0 **sim ≡ metal** gate (DESIGN.md §9.6): it proves
the *same* `.si` program produces the *same* LED behaviour in the host simulator
(`silicac --sim`) and on real hardware (nRF52840, run in Renode).

```sh
RENODE=/path/to/renode ./harness/metal_vs_sim.sh [path/to/program.si]
```

Default program: `examples/blink_button_nrf52840.si`.

Requires `cargo`, `arm-none-eabi-gcc`, and a Renode binary (set `$RENODE` or put
`renode` on `PATH`; portable builds: <https://github.com/renode/renode/releases>).

## What it does

1. Runs `--sim` and extracts the reference LED toggle sequence (writes to
   `gpio0` bit 13).
2. Compiles the same program with `--target metal-nrf52840` to an `.elf`.
3. Runs the `.elf` in Renode on the nRF52840-DK platform, with **`nvic Frequency
   64000000`** so SysTick ticks at 64 MHz and the blink period is real-time
   (500 ms). It injects the button (`gpio0.sw0`) at the same virtual times as the
   program's `sim` block (1200 ms, 1800 ms — the GPIOTE falling edge lands on
   Release), and samples the LED just after each expected toggle.
4. Asserts the metal LED sequence equals the simulator's.

Expected output ends with:

```
sim LED sequence:   1 0 1 0 1 0 1
metal LED sequence: 1 0 1 0 1 0 1
PASS: metal LED sequence matches the simulator (sim ≡ metal).
```

## `bus_parity.sh` — Phase-1 bus trace-order parity gate (§5.2)

`bus_parity.sh` proves a yielding I²C transaction really **suspends** the handler
on metal, so a higher-priority reaction runs **during** the bus window — the §5.2
interleaving the simulator models, now observable on nRF52840 hardware. This is
what the IRQ-driven yields lowering (D2) buys over the old busy-poll, which could
not interleave.

```sh
RENODE=/path/to/renode ./harness/bus_parity.sh
```

Default program: `examples/bus_interleave_nrf52840.si` (a sensor read over I²C +
a higher-priority button reaction). Because Renode does not model Silica's
abstract bus controller, the harness loads `harness/MockBusController.cs` as the
controller at `0x4000_3000` (the `CR/SR/SA/RA/DR` protocol of
`std/*_controller.si`) and wires its completion IRQ to NVIC IRQ 8 (`__BUS_IRQN`).
A `CR` write (kick) completes asynchronously after a latency and raises the IRQ,
modelling a real in-flight transfer.

It then: (1) confirms the **sim** oracle shows the button (`hits`) running before
the sensor resume (`samples`); (2) builds the metal firmware; (3) in Renode, fires
the sensor, injects the button mid-window, and samples the `hits`/`samples` cells.
Mid-window it asserts `hits=1, samples=0` (button ran while the sensor is
suspended) and post-window `hits=1, samples=1` — an ordering impossible under a
busy-poll. Expected output ends with:

```
PASS: the button reaction ran DURING the sensor's bus suspension (trace-order parity with sim).
```

## Notes

- The hermetic codegen tests in `crates/silicac/tests/metal_codegen.rs` run in CI
  without Renode/arm-gcc; these harnesses are the end-to-end on-metal complement
  and need both tools, so they run on demand rather than in `cargo test`. The
  interleave example's sim oracle is additionally guarded hermetically by
  `sim_composition::bus_interleave_example_runs_button_during_the_suspension`.
- Renode clocks SysTick from the NVIC `Frequency` property; pinning it to the
  board's 64 MHz core clock is what makes the metal *timing* (not just the toggle
  sequence) match the simulator.
