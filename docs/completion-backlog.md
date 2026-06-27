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
- [x] D3b `await <cond> within <d> else fault <code>` (§3.2/§5.2) `[metal]` — PR #33. The suspending
      sibling of `poll`. **Resume model chosen: yield + periodic re-check** — on reaching `await` the
      handler yields; cond is re-checked every `within/8` (≥1µs) until it holds (resume) or the budget
      elapses (fault → Layer-2 disposition). Lex (existed) + parse (mirror poll) + SirStmt::Await. Sim:
      true suspend via the event queue (Payload::AwaitRecheck, Activation.await_deadline), so another
      reaction can make cond true (proven: resume strictly after fire). Exempt from the §5.5
      auto-critical (an await polls its cond), rejected inside `atomic`. Metal: bounded re-check loop
      (wfi between checks) respecting `within` — full D2-style frame suspend across await is the noted
      follow-up. examples/await.si + tests/await.rs (5). metal_vs_sim + bus_parity gates PASS.

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
- [x] E4-metal: enforce `within <d>` on metal `[metal]` — PR #30. Per yielding reaction with a
      deadline + a board watchdog: a `__deadline_N` countdown (SysTick ticks) armed at trigger entry,
      ticked down in SysTick (disarmed when the frame returns to idle), latches `__deadline_missed` on
      overrun — which gates off the idle-loop watchdog feed → reset. Catches a *too-slow* handler the
      bare watchdog wouldn't. examples/deadline_nrf52840.si + harness/deadline_reset.sh (Renode PASS:
      within 30ms over a 50ms bus → missed=1; within 80ms → 0) + tests/deadline_metal.rs (3).
      metal_vs_sim + bus_parity gates still PASS. NOTE: needs a declared watchdog (the reset path);
      non-yielding reactions are bounded by ISR run-to-completion; SysTick 1ms granularity.

### Cluster F — exactness & capabilities (last)
- [x] F1 Float/FPU capability gating (§4.1/§4.3) — PR #31. SoC declares `fpu` (SocDef.fpu, parsed as
      a soc-block line; BoardContext.fpu). `float`/`f32`/`f64`/`double` → SirType::F32/F64 (c_type
      float/double, byte_size 4/8). A `float` cell/let on a non-FPU SoC is a compile error
      (float_needs_fpu, checked at program cell/let + reaction-body let). examples/fpu.si +
      tests/fpu.rs (5). Sim/resolve gate; metal ELF compiles (float→C float). NOTE: the broader
      capability system (unforgeable device grants + handler-touches-only-granted check) and float
      *arithmetic* at runtime are follow-ups — float values are carried but not yet computed on.
- [x] F2 Worst-case stack analysis (§5.3/SIL-005) `[metal]` — PR #32. Replaced the flat
      `STACK_RESERVE = 2048` stub with `worst_case_stack(module)`: per-reaction frame = frame-locals ×
      word + fixed overhead; sum over distinct priority levels of the max frame (+ exc frame) + base
      (the worst-case ISR nest — a reaction can't preempt its own level). Recursion banned in the
      resolver (inlining-path re-entry → compile error; also keeps the inliner finite).
      examples/stack_budget.si + tests/stack.rs (3: computed-not-stub, more-levels-grow-it, recursion
      rejected). ram_budget now prints the computed stack (blink 992B, stack_budget 832B vs 2048).
      metal_vs_sim + bus_parity gates PASS. NOTE: sound over-approximation (conservative overheads;
      yielding __rf temps counted as stack) not exact -fstack-usage; frame-union opt not yet applied.

### Cluster P0 — earn the headline guarantees (audit #35)
From the deep audit (issue #35): three headline correctness guarantees are not yet earned —
worst-case stack bound is not sound (F1 over-approximation), register access enforces ordering but
not access *semantics* (`rc`/`pop_on_read`/per-field/multi-field), and the FPU-less numeric path
(`fixed<I,F>`) is non-functional. Plan: `~/.claude/plans/as-an-embedded-firmware-functional-pebble.md`.
Each item is its own branch (`feat/p0-<id>`) + PR behind the hard gate.

- [x] P0-1a Measured worst-case stack via `-fcallgraph-info=su,da` (`.ci`; `-fstack-usage`/`.su`
      fallback) — PR #37. `backend::stackinfo` parses GCC per-function frames + call edges and walks
      the recursion-banned acyclic graph from entry points (`Reset_Handler`/`SysTick_Handler`/
      `GPIOTE_IRQHandler`/`__BUS_IRQHandler`/`HardFault_Handler`); metal `cc_flags` request the dumps,
      `main.rs` pins `-dumpdir`/`-dumpbase` and prints `measured worst-case stack N B` beside the SIR
      estimate (blink: 704 measured vs 992 estimate). tests/stackinfo.rs + 6 unit tests over fixture
      `.ci`/`.su`. metal_vs_sim Renode gate PASS. NOTE: reporting only — P0-1b folds the measured
      number into the enforced `ram_budget()` and hard-errors on over-RAM / non-static frames.
- [x] P0-1b Enforce measured budget in `ram_budget()` — PR #38. `backend::stackinfo::enforce` makes
      the measured stack the authoritative metal budget: hard-errors on `statics + stack > RAM` or any
      non-static (alloca/VLA) frame; `main.rs` runs it post-compile, deletes the over-budget ELF, and
      prints `RAM budget (measured) …`. SIR estimate kept as pre-compile fast-fail / host fallback.
      examples/stack_over_budget_nrf52840.si + harness/stack_budget.sh (healthy reports measured;
      oversized rejected, no ELF) + enforce unit/integration tests. metal_vs_sim Renode gate PASS.
      Completes Finding 1. NOTE: frame-union optimisation not yet applied (only shrinks the budget).
- [x] P0-2a Thread per-field access resolver→SIR→backend — PR #39. `RegInfo.fields` now carries
      `(mask, shift, access)` (field qualifier, else register's); `reg_place`/`reg_load` take an
      explicit access; reads/writes reject `ro` writes and `wo` reads (compile errors). Effect: a
      `w1c` field in an `rw` register lowers to a single masked write (not a sibling-clobbering RMW).
      tests/reg_access.rs (4 negative + a w1c-vs-rw codegen contrast). metal_vs_sim Renode sanity PASS.
- [x] P0-2b `rc`/`pop_on_read` as tracked read-effects — PR #40. Parser captures the previously-
      swallowed `pop_on_read`/`side_effect` → `RegDecl.read_side_effect`; `RegInfo.read_side_effect`
      (rc / modifier / any rc field). A field write that RMWs such a register is a compile error
      (write the whole register, use w1c, or `.raw`); w1c/wo field writes still allowed. Sim models
      `rc` read-to-clear at assignment sites. tests/reg_access.rs (rc/pop_on_read rejected; w1c-on-
      pop_on_read allowed; sim read-clear 5→0). metal_vs_sim Renode gate PASS. NOTE: rc read-clear in
      conditions (not assignment RHS) and `reserved`/`width=` enforcement deferred.
- [x] P0-2c Multi-field single write `REG{a=1, b=1}` — PR #41. `Stmt::RegWrite`→`SirStmt::RegWrite`
      with `(mask,shift,access,value)` per field; parser detects `IDENT {` via peek2; backend
      `emit_mmio_store_multi` ORs the fields into ONE store (single write when all w1c/wo, else one RMW
      over the union mask); resolver rejects unknown/duplicate/`ro` fields + read-side-effect RMW; sim
      applies all fields. tests/reg_multifield.rs (5) + examples/reg_multifield.si. metal_vs_sim Renode
      gate PASS. **Completes Finding 2** (register access semantics).
- [x] P0-3a `fixed<I,F>` type + casts + add/sub — PR #42. `SirType::Fixed{int_bits,frac_bits,signed}`
      (2's-complement int of smallest 8/16/32/64 ≥ I+F); `ValType::Fixed` distinct from int (mixing
      int / different scales is a compile error); `SirExpr::FixedCast` rescales (int↔fixed `<<`/`>>F`,
      fixed↔fixed by frac diff) via scope-aware `expr_sirtype`; same-scale add/sub reuse `Arith` at the
      storage width. examples/fixed.si (sum=7) + tests/fixed.rs (6). metal_vs_sim Renode sanity PASS.
- [x] P0-3c Fixed multiply/divide with rescale — PR #43. `FixedArithOp{Mul,Div}` + `SirExpr::FixedArith`;
      `make_binop_typed` routes fixed `*`/`*%`/`*|`/`/` (add/sub stay raw `Arith`); backend
      `__si_fixmul_*`/`__si_fixdiv_*` helpers (64-bit intermediate, mul `>>F`, div `<<F` then divide,
      div0/out-of-range → trap), sim `eval_fixed`. tests/fixed.rs (+4: 2*3=6, (7/2)*2=7, mul overflow
      traps, metal helper) + examples/fixed.si (prod 12, half 3). metal_vs_sim Renode sanity PASS.
- [x] P0-3b Decimal + voltage literals — PR #44. Lexer: a `.`/`v` between digits →
      `Token::FixedLit(mantissa, frac_digits)` (`0.5`→(5,1), `3v3`→(33,1)); `ExprKind::FixedLit`
      adopts the enclosing fixed scale (default Q16.16) via `arith_frac` threaded through let/cell/
      assign/reg-write. tests/fixed.rs (+3: 0.5*2=1, 3v3*10=33, fixed<8,8> 0.5 scales at F=8) +
      examples/fixed.si (gained = 3.0*1.5 → 4). metal_vs_sim Renode sanity PASS.
- [x] P0-3d BME280 datasheet compensation end-to-end (the proof point) — PR #45. `std/bme280.si`
      gains `read_temp_c() -> fixed<16,16>`: reads the raw ADC over I²C (yielding) and compensates
      with fixed cast + subtract + divide `(adc - T0)/span`. resolver `expr_sirtype` resolves a
      composed-op call's return type so `let t = sensor.read_temp_c()` is `fixed`. tests/bme280.rs
      (raw 0x5AB0 → 25.00 °C; deg 25, centi 2500; raw not passed through) + examples/sensor_temp_c.si.
      metal_vs_sim + bus_parity Renode gates PASS. **Completes Finding 3 and the P0 cluster.**

### Cluster P1 — precision & cost visibility (audit #35)
From the deep audit (issue #35), the performance/embedded recommendations: barriers that match the
ARM architecture (cheaper *and* correct), `-Os` by default + a flash budget gate as hard as the RAM
gate, and `every` on real timer hardware instead of a 1ms SysTick grid. Plan:
`~/.claude/plans/as-an-embedded-firmware-functional-pebble.md`. Each item is its own branch
(`feat/p1-<id>`, **independent off `main`**) + PR targeting `main` (not auto-merged). All `[metal]`.

- [x] P1-1 Barrier model: ARM-conformant + de-duplicated `[metal]` — PR #48. Defined `__ISB`; `__ISB`
      after `MSR BASEPRI` raise (replaces `__DMB`); `__DSB` after the GPIOTE event clear before ISR
      return + `__DSB;__ISB` after the bus NVIC-disable; `__DSB` before the bus completion-IRQ enable;
      `__ISB` after `cpsie i`. Collapsed the double-`__DMB` per MMIO store to a single trailing one
      (Device memory is architecturally ordered). tests/metal_codegen.rs (single-DMB, ISB-after-BASEPRI,
      DSB-at-event-clear); metal_vs_sim + bus_parity Renode gates PASS. NOTE: fully dropping the
      trailing `__DMB` for runs with no Normal↔Device dependency is a deferred follow-up.
- [x] P1-2 Default `-Os` + `--opt <level>` override `[metal]` — PR #49. Metal `cc_flags` now `-Os`;
      `backend::opt_override_flag` + a `--opt` CLI flag drop the default `-O…` and append the override
      in `run()` (keeps cc_flags `&'static`). backend::tests (default -Os; flag forming). Blink text
      512B (-Os) vs 552B (-O2) proves it reaches cc; metal_vs_sim Renode gate PASS at -Os.
- [x] P1-3 Flash / code-size budget gate `[metal]` — PR #50/#52. `flash_region_size`/
      `parse_size`/`enforce_flash` in backend/c.rs; `run()` sizes the ELF via `arm-none-eabi-size`
      (derived from the cc prefix) and prints `flash budget … of … B` (blink 520 B of 1 MiB), deleting
      the ELF + erroring on overflow. The linker region check is the first-line hard enforcer;
      enforce_flash is the clean-message backstop. tests/flash_budget.rs + examples/flash_over_budget_
      nrf52840.si + harness/flash_budget.sh (healthy reports; oversized rejected, no ELF). metal_vs_sim PASS.
- [x] P1-4 `every` on a hardware timer `[metal]` — PR #51. `every` lowers onto nRF52840 TIMER1
      (1MHz free-running 32-bit, one CC channel per reaction, `TIMER1_IRQHandler` re-arms `CC+=period`;
      `timer_plan` does exact-or-error period→ticks at 1µs — `every 1500us` now works, no 1ms grid/
      24-bit cap). SysTick kept for now()/deadlines + the watchdog feed cadence. Renode models TIMER1
      (no mock needed). tests/metal_codegen.rs (timer plan/handler/config + error cases) +
      examples/every_timer_nrf52840.si. metal_vs_sim/bus_parity/deadline_reset/watchdog_reset PASS.
      **Completes Cluster P1.** NOTE (deferred): re-base now()/deadlines onto the TIMER + retire SysTick.

### Cluster P2 — prove the thesis & reduce foreclosure (audit #35)
From the deep audit (issue #35), the strategic items: prove SIR is genuinely target-neutral (an LLVM
canary), measure the agentic-native thesis (an escape-hatch corpus metric), and make the type system
more expressive (persistent typestate + match-over-fault-codes). Plan:
`~/.claude/plans/as-an-embedded-firmware-functional-pebble.md`. Each item is its own branch
(`feat/p2-<id>`, **independent off `main`**) + PR targeting `main` (not auto-merged).

- [x] P2-2 Escape-hatch / idiom-corpus metric (audit #9) — PR #54. `metrics::count_escape_hatches`
      (token-based, comment-safe) counts `as <type>` casts + wrap/sat ops (`.raw`/`.le`/`.be` at 0);
      `escape_audit` bin + harness/escape_hatch_audit.sh report per-file; tests/escape_hatch.rs gates
      the std lib at ≤3 and locks the baseline. Corpus total 11 (9 casts, 2 wrap/sat), std=1. cargo
      test green (35 binaries). Validates risk #4; a live agent eval (risk #5) is future work.
- [ ] P2-3 Persistent cross-reaction typestate for single-owner devices (audit #10a) — device-ownership
      pre-pass (analog of analyze_cells, keyed by device id; single_owner = touched by 1 reaction);
      carry a single-owner device's end-state across reactions instead of clearing it. tests/typestate.rs.
- [ ] P2-1 Thin SIR→LLVM-IR canary (audit #8) `[llvm]` — `brew install llvm`; backend/llvm.rs emits
      textual LLVM IR for a SIR subset (sys.start, cell assigns, integer Arith via
      `llvm.*.with.overflow`+`llvm.trap`, exit, host-io) with NO libc/`__builtin`; `--emit-llvm` flag;
      hermetic test + harness/llvm_canary.sh (`llvm-as`/`opt -verify` + run). Validates risk #2.
- [ ] P2-4 `match` over an op's fault codes (audit #10b, full sim+metal) `[metal]` — `MatchPat::Ok/Fault`,
      parse `ok`/`fault <code>` arms; new SIR fault-or-value result; exhaustiveness vs
      `op.ret.fault_codes`; sim + metal (yields state machine). Largest/riskiest. tests/match_stmt.rs +
      Renode. (PAUSE & report if metal can't be validated.)

## Completed log
_(append `item — PR #NN — date` here as items land)_
