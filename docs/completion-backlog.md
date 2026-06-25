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

## Backlog (check off as completed; record PR #)

### Cluster A — enforcement on already-parsed syntax
- [ ] A1 `where`-constraint enforcement (§3.2/§4.1)
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
- [ ] D1 spi controller leaf + composed example (§3.5) `[metal]`
- [ ] D2 Real IRQ-driven yields state machine (§5.2/§6.1) `[metal]`  ← critical path
- [ ] D3 `await <cond> within <d>` (§3.2/§5.2) `[metal]`  (dep: D2)

### Cluster E — Renode Phase-1 closure + fault depth
- [ ] E1 Mock I²C controller Renode peripheral + trace-order parity harness `[metal]`  (dep: D2)
- [ ] E2 `when`-typestate + Layer-3 site map (§4.1/§5.4) `[metal]`
- [ ] E3 Bus arbitration / queues / scheduler overflow policy (§3.5/D06, §5.1/D02) `[metal]`
- [ ] E4 `reaction … within <d>` deadline → watchdog starve (§4.5/§5.6) `[metal]`

### Cluster F — exactness & capabilities (last)
- [ ] F1 Capabilities + float/FPU gating (§4.1/§4.3)
- [ ] F2 Worst-case stack analysis (§5.3/SIL-005) `[metal]`

## Completed log
_(append `item — PR #NN — date` here as items land)_
