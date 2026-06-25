# Silica completion backlog

Tracking file for the `/loop` that drives Silica to "complete" (surface-language scope).
Plan: `~/.claude/plans/serene-crafting-prism.md`. One item per loop iteration; each item
= its own branch + PR behind the hard gate below.

## Hard gate (every item)
- `cargo test` 100% green, no warning regressions.
- A new test covers the feature (inline-program style in `crates/silicac/tests/`).
- The relevant example compiles/runs on its target; new constructs add an `examples/*.si`.
- DESIGN.md / phase docs status line updated.
- `[metal]` items must additionally pass a **Renode run** before being checked off.

## Toolchain (Iteration 0) — verified
- Rust: 1.96.0 — `. "$HOME/.cargo/env"`.
- ARM GCC: 15.2.1 (Arm GNU Toolchain 15.2.Rel1). The Homebrew `.pkg` needs sudo (no TTY
  here), so the payload was expanded in place — binaries at
  `$HOME/arm-gnu-toolchain-15.2/Payload/bin` (not on the default PATH).
- Renode: 1.16.1 portable at `$HOME/Renode.app/Contents/MacOS/renode`.
- **Every metal iteration must export this env before running the harness:**
  ```sh
  . "$HOME/.cargo/env"
  export PATH="$HOME/arm-gnu-toolchain-15.2/Payload/bin:$PATH"
  export RENODE="$HOME/Renode.app/Contents/MacOS/renode"
  ./harness/metal_vs_sim.sh            # blink/button gate
  ```
- Baseline gate (blink/button `sim ≡ metal`): **PASS** — `1 0 1 0 1 0 1` both sides.

## Sequencing (revised after A1)
Investigation found cluster A is **not** mostly "enforce already-parsed syntax": only A1
was parse-complete. A2 needs a numeric **cast** spelling; A3 needs `instant`/`now()`; A4
needs the **`match`** construct (lexer-token only today); A5 needs interface **properties**.
So A2–A5 are full feature builds. Per user decision: **keystone-first** — do D (spi →
yields state machine) and E (Renode I²C parity) first (the concrete path to "device on
Renode with trace-order parity"), then return to A–C/B/F. New surface-syntax decisions:
pick the spec-consistent default and note it in the PR.

## Backlog (check off as completed; record PR #)

### Cluster A — enforcement on already-parsed syntax
- [x] A1 `where`-constraint enforcement (§3.2/§4.1) — PR #12. Also fixed a parser
      greediness bug where `where <expr> = <default>` swallowed the default.
- [ ] A2 Number model: casts / mixed-sign / odd-width / endianness (§4.3)
- [ ] A3 instant/duration type rules + `now()` (§4.5)
- [ ] A4 Disposition completeness vs declared codes (§4.4/D14)
- [ ] A5 Interface semantic-property checks (§4.1/D18)

### Cluster B — arithmetic safety
- [ ] B1 Saturating/wrapping ops + `@overflow` directive + overflow-trap-by-default (§4.3/SIL-004) `[metal]`

### Cluster C — bounded-memory & atomicity
- [ ] C1 `atomic { … }` multi-cell construct (§5.5/D03)
- [ ] C2 Bounded types `pool`/`arena`/`ring`/`buffer`/`bytes` (§5.3/§4.3)
- [ ] C3 Typed overlays — language construct only (§3.6)

### Cluster D — Phase-1 yields keystone
- [x] D1 spi controller leaf + composed example (§3.5) `[metal]` — PR #13. std/spi.si +
      std/spi_controller.si + examples/sensor_spi.si (bmp280-over-spi). Reuses the generic
      BusXfer path with **zero backend change** (metal emitter resolves CR/SR/SA/RA/DR by
      name). Metal firmware compiles + links with arm-gcc; full Renode bus execution lands
      with E1's mock controller.
- [x] D2 Real IRQ-driven yields state machine (§5.2/§6.1) `[metal]` — PR #14. Metal-only
      (the sim was already a full suspend/resume scheduler via pc + Activation.locals).
      Busy-poll → static frame struct + segment dispatcher (`switch(__state)`) that kicks +
      arms the completion IRQ + returns; `__BUS_IRQHandler` resumes the owner; trigger entry
      coalesces (§5.1). A wedged bus now matches the sim's `Hang` (watchdog catches it).
      Cell-borrow-across-yield safety holds via the existing "critical can't span a yield"
      check (cells are only touched inside criticals). 3 codegen tests rewritten to assert
      the state machine; all metal examples link; baseline blink/button Renode gate still
      PASS. **Interleaving on Renode** (vs just in sim) lands with E1's mock controller.
- [ ] D3 `await <cond> within <d>` (§3.2/§5.2) `[metal]`  (dep: D2)

### Cluster E — Renode Phase-1 closure + fault depth
- [x] E1 Mock I²C controller Renode peripheral + trace-order parity harness `[metal]` — PR #15.
      harness/MockBusController.cs (async bus controller @ 0x40003000, IRQ→NVIC#8) +
      harness/bus_parity.sh + examples/bus_interleave_nrf52840.si. On Renode: button runs
      DURING the sensor's bus suspension (mid-window hits=1,samples=0; post hits=1,samples=1)
      — trace-order parity with sim, impossible under a busy-poll. **Headline "device on
      Renode with trace-order parity" criterion met.** Hermetic sim oracle guards the example.
- [ ] E2 `when`-typestate + Layer-3 site map (§4.1/§5.4) `[metal]`
- [ ] E3 Bus arbitration / queues / scheduler overflow policy (§3.5/D06, §5.1/D02) `[metal]`
- [ ] E4 `reaction … within <d>` deadline → watchdog starve (§4.5/§5.6) `[metal]`

### Cluster F — exactness & capabilities (last)
- [ ] F1 Capabilities + float/FPU gating (§4.1/§4.3)
- [ ] F2 Worst-case stack analysis (§5.3/SIL-005) `[metal]`

## Completed log
_(append `item — PR #NN — date` here as items land)_
