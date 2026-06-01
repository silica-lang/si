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

## Notes

- The hermetic codegen tests in `crates/silicac/tests/metal_codegen.rs` run in CI
  without Renode/arm-gcc; this harness is the end-to-end on-metal complement and
  needs both tools, so it runs on demand rather than in `cargo test`.
- Renode clocks SysTick from the NVIC `Frequency` property; pinning it to the
  board's 64 MHz core clock is what makes the metal *timing* (not just the toggle
  sequence) match the simulator.
