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
- [x] A2 Number model: casts / mixed-sign / narrowing (§4.3) — PR #23. Explicit cast `<expr> as
      <type>` (AST::Cast → SirExpr::Cast; sim truncates to width, C emits a fixed-width cast). A
      resolver `value_type` pass (Int{width,signed}/Literal/Flexible) rejects implicit narrowing,
      mixed signed/unsigned operands, and out-of-range literals; literals + device/register results
      stay flexible to avoid false positives. examples/casts.si + tests/casts.rs (9). Sim is the gate
      (not `[metal]`-tagged); metal C compiles with arm-gcc. NOTE: `.le`/`.be` endianness, odd-width
      fields (u7/u24) in expressions, and a checked/fallible narrowing cast are deferred — only the
      truncating `as` is built.
- [x] A3 instant/duration type rules + `now()` (§4.5) — PR #22. `instant`/`duration` are distinct
      `SirType`s (both u64 ns); `now()` is a bare-ident call lowered to `SirExpr::Now` (sim → virtual
      time, host → `clock_gettime`, metal → SysTick uptime counter). A resolver `time_kind` pass
      enforces §4.5: `instant - instant → duration`, `instant ± duration → instant`, and rejects
      `instant + instant`, `now() + <bare int>`, scaling/comparing/assigning instants across kinds.
      A new `ExprKind::DurationLit` keeps `500ms` type-distinct from `5`, so the doc's `now() + 500ms`
      (ok) vs `now() + 5` (error) example holds. examples/instant.si + tests/instant.rs (8). Metal C
      compiles (verified with arm-gcc); sim is the gate (not `[metal]`-tagged). NOTE: D15
      exact-tick-rate conversion + `rounded` modes still unenforced; metal now() is 1ms resolution.
- [x] A4 `match` + totality (§4.4/D14) — PR #24. `match <expr> { <lit> => …, _ => … }` as the first
      surface conditional, lowered to a guarded if-chain over existing SirStmt::If (no SIR/sim/metal
      change). Enforced **total**: a `_` wildcard arm is required (compile error otherwise), duplicate
      literal arms rejected. Integer + bool literal patterns. examples/match.si + tests/match_stmt.rs
      (5); sim gate + metal compiles. NOTE: `ok`/`fault f` op-result patterns and exhaustiveness vs an
      op's declared fault-code set (the §4.4 `match usart2.write()` form) build on this — deferred.
- [x] A5 Interface semantic-property checks (§4.1/D18) — PR #25. Interface `property <name> [=
      default]`; controller `provides <iface> { name = value }`; device `needs { bus : i2c where
      <expr> }`. Resolver const-evaluates the requirement against the provider's values (over interface
      defaults, reusing A1's `where` evaluator) at board-bind — false, or an undeclared property, is a
      compile error. std/i2c.si declares max_speed/addressing; std/i2c_controller.si provides 400_000/7.
      examples/bus_speed.si + tests/interface_props.rs (4). Sim/resolve gate. NOTE: richer property set
      (atomicity/clock-stretch/recovery) expressible but not yet on the std interface; values are
      int/bool constants.

### Cluster B — arithmetic safety
- [x] B1 Saturating/wrapping ops + overflow-trap-by-default (§4.3/SIL-004) `[metal]` — PR #21.
      Lex `+% +| -% -| *% *|`; AST/parse the wrap/sat operators; SIR gains a width-checked
      `SirExpr::Arith{op,mode,width,signed}` (Add/Sub/Mul; Div/Rem stay `BinOp`). The width comes
      from the assignment-target type (cell/local/register), threaded through the resolver — so the
      same `+ 100` is safe on a u32 and a trap on a u8. Sim: trap → `OVERFLOW TRAP` trace + safe-state
      (bypasses Layer-2 disposition — a system-integrity fault); wrap/saturate at width. Metal: one
      `static inline __si_<op>_<mode>_<ty>` helper per shape; trap uses `__builtin_*_overflow` →
      `__silica_overflow_trap` → `__drive_safe()` + halt. examples/overflow.si (sim demo) +
      examples/overflow_nrf52840.si + tests/overflow.rs (5). **Renode**: harness/overflow_trap.sh
      PASS (trap froze ticks=4, wrap ran to 10). NOTE: scoped `@overflow(...)` block directive
      deferred (needs attribute syntax) — per-operator opt-out covers it; signed-`Div`/`Rem` overflow
      (INT_MIN/-1) not yet trapped.

### Cluster C — bounded-memory & atomicity
- [x] C1 `atomic { … }` multi-cell construct (§5.5/D03) — PR #16. Lex KwAtomic, parse
      `atomic { stmts }`, lower to ONE Critical whose ceiling is fixed up in analyze_cells
      (reuses the priority-ceiling machinery); reject a yield inside. Distinct from the
      per-access auto-critical. examples/atomic.si + tests/atomic.rs.
- [x] C2 Bounded type `ring<T, N>` (§5.3) — PR #26. The canonical producer/consumer queue: a
      `ring<T,N>` cell (TypeKind/SirType::Ring) with push/pop/len/is_empty/is_full, dispatched on a
      cell binding. Sim models a VecDeque per ring; metal lowers to backing array + head/tail/count,
      summed into the static RAM budget (ring<u32,16>=76B via c::ram_budget). Full ring → push
      overwrites oldest (defined bounded policy); cross-reaction sharing protected by the §5.5
      auto-critical (ring ops are cell touches). examples/ring.si + tests/ring.rs (4). Sim gate +
      metal compiles. NOTE: pool/arena/buffer/bytes deferred (ring proves the pattern); T is an
      integer scalar; fault-on-full/empty variant is a follow-up.
- [x] C3 Typed overlays — compile-time `set`/`remove` (§3.6) — PR #27. `overlay <name> for
      board.<b> { set <inst>.config.<field> = <v>; remove <name> }` (Item::Overlay). Applied to the
      target board before build_board, so the §4.1 config `where`-check validates the patched value.
      `set` checks instance+field exist & overrides (out-of-range → where violation); `remove` deletes
      an instance/pin-binding (errors if absent); unknown-board target rejected. examples/overlay.si +
      tests/overlay.rs (6). Sim/resolve gate. NOTE: `extend …needs` parse-rejected (follow-up); remove
      dangling-ref check not yet enforced; agent overlay-edit API out of scope (Phase 2).

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
- [x] D3a `poll <cond> within <d> else fault <code>` (§3.2) `[metal]` — PR #17. Non-suspending
      bounded busy-wait. Lex/parse, SirStmt::Poll, sim (deterministic check → pending_fault →
      dispose), metal (bounded spin → __faulted → reaction disposition; non-yielding poll-
      bearing reactions get the fault/retry wrapper). examples/poll_usart.si + tests/poll.rs.
      Both Renode gates still PASS (no metal regression). Builds the `<cond> within <d>`
      parsing await will reuse.
- [ ] D3b `await <cond> within <d>` (§3.2/§5.2) `[metal]`  (dep: D2) — DEFERRED, NEEDS A DESIGN CALL,
      not autonomous-default-able. Findings: `await`/`poll`/`within`/`else` all lex but none
      parse; `await` suspends on a *condition* but the spec doesn't pin the wakeup trigger
      (re-check cadence? event-driven dep-tracking?); and the sim doesn't model hardware-
      driven register changes, so a condition-wait can't naturally succeed without injection
      machinery. Its non-suspending sibling `poll` is also unbuilt and is the parsing
      prerequisite. Surface the resume-model decision before implementing.

### Cluster E — Renode Phase-1 closure + fault depth
- [x] E1 Mock I²C controller Renode peripheral + trace-order parity harness `[metal]` — PR #15.
      harness/MockBusController.cs (async bus controller @ 0x40003000, IRQ→NVIC#8) +
      harness/bus_parity.sh + examples/bus_interleave_nrf52840.si. On Renode: button runs
      DURING the sensor's bus suspension (mid-window hits=1,samples=0; post hits=1,samples=1)
      — trace-order parity with sim, impossible under a busy-poll. **Headline "device on
      Renode with trace-order parity" criterion met.** Hermetic sim oracle guards the example.
- [x] E2 `when`-typestate — static half (§4.1/D07) `[metal]` — PR #28. `states { … }`, op `when
      <state>`, `become <state>`. Resolver tracks each device's provable state through a reaction's
      straight-line flow (cleared per reaction); a `when S` call without a dominating `become S` is a
      compile error; `when`/`become` on an undeclared state rejected at the device (check_states).
      examples/typestate.si + tests/typestate.rs (5). Compile-time-only → metal unaffected: example
      ELF compiles, metal_vs_sim gate still PASS. NOTE: runtime-precondition lowering (unprovable
      cases → Layer-3 fault) + the Layer-3 site-map debug info are follow-ups; op transitions read
      from the op's own top-level `become` (not nested sub-op inlining).
- [x] E3 Scheduler overflow policy (§5.1/D02) `[metal]` — PR #29. `every/on … on overflow
      <coalesce|drop_newest|fault>` clause (Reaction.overflow → SirReaction.overflow, default
      Coalesce). On a re-fire-while-in-flight the sim's fire() applies it: coalesce collapses,
      drop_newest discards (distinct EventOverflow trace), fault → drive_safe + stop. Metal
      yielding-reaction trigger entry branches the same (coalesce/drop → return; fault →
      __drive_safe()+halt; __drive_safe emission extended for fault policy). examples/overflow_policy.si
      (1µs every vs 2µs bus → overflow) + tests/overflow_policy.rs (5). bus_parity gate PASS;
      ms-scale fault-policy program compiles+links to metal ELF (sub-µs every is sim-only — metal
      SysTick base is 1ms). NOTE: pending capacity >1 and per-event-source (vs per-reaction)
      declaration deferred; multi-consumer bus arbitration / bounded per-bus wait queue not built.
- [x] E4 `reaction … within <d>` deadline → watchdog starve (§4.5/§5.6) — PR #18. Parse
      `every/on … within <d>`, lower to SirReaction.deadline_ns. Sim: arm a per-activation
      deadline event on fire (generation-guarded); overrun while still in-flight →
      DeadlineMissed reset. examples/deadline.si + tests/deadline.rs. NOTE: sim-enforced only
      — the **metal watchdog itself isn't wired yet** (the backend never feeds a wdt), so
      on-metal deadline enforcement is a follow-up gated on building the metal watchdog (a
      new item). Metal firmware still compiles (deadline_ns unused on metal); blink gate PASS.

### Cluster E (cont.) — discovered follow-ups
- [x] E5 Metal hardware watchdog wiring (§5.6) `[metal]` — PR #19. SIR carries watchdog_device;
      backend configures+starts the wdt at boot (RLR/CR/KR) and feeds it in the idle loop
      gated on all yielding frames being idle (a hung/suspended reaction → never fed → reset).
      Codegen test + compile/link; existing Renode gates unaffected (non-wdt programs unchanged).
- [x] E5b Renode mock watchdog + reset validation `[metal]` — PR #20. harness/MockWatchdog.cs
      (CR/RLR/KR @ 0x40010000, latches SR on expiry-unfed) + harness/watchdog_reset.sh +
      examples/bus_watchdog_nrf52840.si. On Renode: wedged bus (mock latency 100s) → idle loop
      stops feeding → watchdog fires (SR=1); healthy bus (1ms) → kept fed (SR=0). On-hardware
      proof of the scheduler-fed watchdog (§5.6), parallel to E1's bus parity.
- [ ] E4-metal: enforce `within <d>` on metal (per-reaction deadline timer) — builds on E5.

### Cluster F — exactness & capabilities (last)
- [ ] F1 Capabilities + float/FPU gating (§4.1/§4.3)
- [ ] F2 Worst-case stack analysis (§5.3/SIL-005) `[metal]`

## Completed log
_(append `item — PR #NN — date` here as items land)_
