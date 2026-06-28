# Metal validation harness

These scripts are the **sim ≡ metal** gates: each proves the *same* `.si` program
produces the *same* observable behaviour in the host simulator (`silicac --sim`)
and on real hardware (nRF52840, run in Renode). Together they cover the whole
runtime — the [full catalog](#full-gate-catalog) is at the bottom; the foundational
gates are written up in detail below.

Requires `cargo`, `arm-none-eabi-gcc`, and a Renode binary (set `$RENODE` or put
`renode` on `PATH`; portable builds: <https://github.com/renode/renode/releases>).
LLVM gates additionally need `llc` (Homebrew LLVM).

> **Both backends.** Silica emits the metal image through two independent backends
> (C and LLVM). Every shared gate below takes a **`BUILD=llvm`** switch that re-runs
> the entire check through the LLVM backend, so the two backends are continuously
> required to produce identical behaviour:
> ```sh
> RENODE=/path/to/renode            ./harness/metal_vs_sim.sh   # C backend (default)
> RENODE=/path/to/renode BUILD=llvm ./harness/metal_vs_sim.sh   # LLVM backend
> ```

## `metal_vs_sim.sh` — the baseline sim ≡ metal gate (§9.6)

`metal_vs_sim.sh` is the Phase-0 **sim ≡ metal** gate (DESIGN.md §9.6): it proves
the *same* `.si` program produces the *same* LED behaviour in the simulator and on
the nRF52840 in Renode.

```sh
RENODE=/path/to/renode ./harness/metal_vs_sim.sh [path/to/program.si]
```

Default program: `examples/blink_button_nrf52840.si`.

## What it does

1. Runs `--sim` and extracts the reference LED toggle sequence (writes to
   `gpio0` bit 13).
2. Compiles the same program with `--target metal-nrf52840` to an `.elf`.
3. Runs the `.elf` in Renode on the nRF52840-DK platform, with **`nvic Frequency
   64000000`** so the on-chip timers run at the 64 MHz core clock and the blink
   period is real-time (500 ms; `every` is driven by TIMER1). It injects the button
   (`gpio0.sw0`) at the same virtual times as the
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

## `watchdog_reset.sh` — Phase-1 watchdog reset gate (§5.6)

`watchdog_reset.sh` proves the generated metal idle loop feeds the hardware
watchdog **only on a clean return to idle**, so a wedged reaction stops the feeds
and the watchdog fires — the scheduler-fed behaviour the simulator models, on
nRF52840 in Renode.

```sh
RENODE=/path/to/renode ./harness/watchdog_reset.sh
```

Default program: `examples/bus_watchdog_nrf52840.si` (a sensor read over I²C every
100ms + a `wdt` with a 50ms timeout). Renode models a *real* nRF WDT (different
registers) and no abstract bus controller, so the harness unregisters `wdt` +
`twi0` and loads `harness/MockWatchdog.cs` (@`0x4001_0000`, latches `SR` when it
expires unfed) and `harness/MockBusController.cs` (@`0x4000_3000`). It runs twice:
a **wedged** bus (mock latency 100s → the transfer never completes, the sensor
stays suspended, the idle loop stops feeding) must fire the watchdog (`SR=1`); a
**healthy** bus (latency 1ms → completes each tick) must not (`SR=0`). Expected
output ends with:

```
PASS: a wedged reaction starves the watchdog → reset; a healthy one is fed (§5.6).
```

## `overflow_trap.sh` — overflow-trap-by-default gate (§4.3 / SIL-004)

`overflow_trap.sh` proves a plain `+` whose result does not fit its target type
**traps** on metal — the generated helper drives the system to its safe state and
halts — rather than silently wrapping. This is the silent-wraparound footgun the
number model (§4.3) exists to kill, now machine-checked on nRF52840.

```sh
RENODE=/path/to/renode ./harness/overflow_trap.sh
```

Default program: `examples/overflow_nrf52840.si` (a `u8` accumulator advanced by
85 every 100ms, plus a `u32` `ticks` counter). The harness builds it twice: the
trapping original (`acc + 85`) and a wrapping control derived by `sed`
(`acc +% 85`). The `u8` overflows on the 4th tick: the trap build halts there
(`ticks` frozen ≈4), the wrapping build runs every tick (`ticks` ≈10 over ~1.05s).
The gap is the proof the default `+` trapped. `ticks` is read from its `.bss`
symbol via `arm-none-eabi-nm`. Expected output ends with:

```
PASS: the default `+` trapped on overflow and halted (safe-state); `+%` wrapped and kept running (§4.3/SIL-004).
```

## `deadline_reset.sh` — on-metal `within <d>` deadline gate (§4.5/§5.6)

`deadline_reset.sh` proves the generated firmware **detects a reaction that
overruns its declared `within` deadline** — a tighter bound than the bare
watchdog (which only catches a handler that *never* returns to idle). The
backend arms a per-reaction `__deadline_N` countdown at the trigger entry, ticks
it down on the TIMER2 1 ms tick, and latches `__deadline_missed` on an overrun;
that flag gates off the watchdog feed, so the system resets.

```sh
RENODE=/path/to/renode ./harness/deadline_reset.sh
```

Default program: `examples/deadline_nrf52840.si` (a sensor read `every 100ms
within 30ms`). The harness loads `MockBusController.cs` at a ~50ms latency and
builds the program twice — the original (`within 30ms`, tighter than the bus) and
a control derived by `sed` (`within 80ms`, looser). It reads `__deadline_missed`
straight from RAM via its symbol (`arm-none-eabi-nm`): the tight budget overruns
(`= 1`), the loose one completes in time (`= 0`). The feed-stop → reset half is
already covered by the §5.6 watchdog harnesses, so this isolates the *detection*.
Expected output ends with:

```
PASS: the metal firmware detects a within-deadline overrun (latches __deadline_missed → stops the watchdog feed, §4.5/§5.6).
```

## Full gate catalog

The five gates above are written up in detail; the rest follow the same pattern
(build the program, run it in Renode, assert an observable equals the simulator's).
Unless noted, each takes the `BUILD=llvm` switch to re-run through the LLVM backend.

| Script | Gates | Default program |
| --- | --- | --- |
| `metal_vs_sim.sh` | baseline blink+button sim ≡ metal (§9.6) | `blink_button_nrf52840.si` |
| `bus_parity.sh` | a yielding bus xfer suspends so a peer runs during the window (§5.2) | `bus_interleave_nrf52840.si` |
| `bus_refire.sh` | a yielding `every` reaction re-arms and fires repeatedly (clears stale bus pending) | `fault_match.si` |
| `bus_arbitration.sh` | two reactions on one bus are priority-arbitrated; both reads complete (P6-9) | `bus_contend_nrf52840.si` |
| `watchdog_reset.sh` | the idle loop feeds the WDT only on clean idle; a wedged reaction → reset (§5.6) | `bus_watchdog_nrf52840.si` |
| `deadline_reset.sh` | a `within <d>` overrun latches `__deadline_missed` → stops the feed (§4.5/§5.6) | `deadline_nrf52840.si` |
| `overflow_trap.sh` | a default `+` traps to safe-state on overflow; `+%` wraps (§4.3/SIL-004) | `overflow_nrf52840.si` |
| `fault_decode_metal.sh` | the `HardFault_Handler` Layer-3 region decoder + ownership table link & run (§5.4) | `fault_nrf52840.si` |
| `fault_match.sh` | `match` over an op's `ok`/`fault <code>` decodes the runtime outcome (P2-4/P3-2) | `fault_match{,_composed}.si` |
| `poll_await.sh` | `poll`/`await … within … else fault` resume when satisfied, fault on timeout (§3.2) | `poll_nrf52840.si`, `await_nrf52840.si` |
| `await_interleave.sh` | an `await` frame-suspends so a peer runs during the wait (P6-5) | `await_interleave_nrf52840.si` |
| `now_uptime.sh` | `now()` reads the TIMER2 µs uptime, matching sim virtual time (P5-1/P6-6) | `uptime_nrf52840.si` |
| `ring_metal.sh` | `ring<T,N>` push/pop/len on metal (P6-1) | `ring_nrf52840.si` |
| `fixed_metal.sh` | `fixed<I,F>` mul/div with rescale on metal (P6-2) | `fixed_nrf52840.si` |
| `float_metal.sh` | runtime float on the hardware FPU, bit-exact with sim (P6-8) | `float_nrf52840.si` |
| `semihosting.sh` | `host_io.print` → ARM semihosting, captured stream = sim stdout (P6-7) | `semihosting_nrf52840.si` |
| `typestate_runtime.sh` | a runtime `when`-precondition guard drives safe-state on mismatch (P3-3) | `typestate_runtime.si` |
| `stack_budget.sh` | the measured worst-case stack budget is enforced; oversized → no ELF (P0-1b) | `stack_over_budget_nrf52840.si` |
| `flash_budget.sh` | the flash/code-size budget is enforced; oversized → no ELF (P1-3) | `flash_over_budget_nrf52840.si` |
| `llvm_metal.sh` | the LLVM backend boots `sys.start` on Renode, end-to-end, no C (P3-4c) | `boot_nrf52840.si` |
| `llvm_metal_sched.sh` | the LLVM backend's TIMER1 scheduler toggles the LED on period (P4-1) | `blink_nrf52840.si` |
| `escape_hatch_audit.sh` | reports the cast / wrap-sat escape-hatch count per file (host, no Renode) (P2-2) | (the corpus) |

The C# files `MockBusController.cs` (the abstract bus controller at `0x4000_3000`,
IRQ 8) and `MockWatchdog.cs` (a latching WDT at `0x4001_0000`) are Renode peripheral
models loaded by the bus/watchdog gates, since Renode does not model Silica's
abstract controllers.

## Notes

- The hermetic codegen tests in `crates/silicac/tests/metal_codegen.rs` and
  `crates/silicac/tests/llvm_canary.rs` run in CI without Renode/arm-gcc/llc; these
  harnesses are the end-to-end on-metal complement and need the toolchain, so they
  run on demand rather than in `cargo test`. The interleave example's sim oracle is
  additionally guarded hermetically by
  `sim_composition::bus_interleave_example_runs_button_during_the_suspension`.
- Renode clocks the on-chip timers from the NVIC `Frequency` property; pinning it to
  the board's 64 MHz core clock is what makes the metal *timing* (not just the toggle
  sequence) match the simulator.
