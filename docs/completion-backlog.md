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
- [x] P2-3 Persistent cross-reaction typestate (audit #10a) — PR #55. State established in `on sys.start`
      (the boot-time single state-writer, runs once before the scheduler) persists into every later
      reaction via `persistent_states` (seed instead of clear); a device not initialised at boot still
      resets to its initial state (sound, not blind). tests/typestate.rs (persists; negative control) +
      examples/typestate_persist.si. cargo test green. NOTE: realized scope is the sound configure-at-
      boot pattern; broader single-owner-firing persistence deferred.
- [x] P2-1 Thin SIR→LLVM-IR canary (audit #8) `[llvm]` — PR #56. `backend/llvm.rs` (`LlvmBackend`)
      emits textual LLVM IR for a SIR subset (sys.start, cell globals, `Assign(Var)`, integer `Arith`
      → `llvm.{u,s}{add,sub,mul}.with.overflow.iN` + `llvm.trap`, wrap → plain `iN`, saturate →
      `select`, `Cast`/`BinOp`/`Not`, `Exit` → `ret i32`, host-io → raw `svc` syscall) with NO
      libc/`__builtin`; `--emit-llvm` flag (orthogonal to `--target`). examples/llvm_canary.si (exit
      42 = 20+22). tests/llvm_canary.rs (5, hermetic shape + no-C-ism) + harness/llvm_canary.sh
      (`llvm-as` + `opt -verify` + compile/run + libc grep) — **PASS** with Homebrew LLVM 22.1.8.
      Validates risk #2 / the §6.2 purity guard. NOTE: subset only — non-sys.start reactions, the
      scheduler/event loop, MMIO, the yields state machine, and metal startup/linker are a full-backend
      follow-up (each unsupported construct emits a visible `; unsupported` signpost).
- [x] P2-4 `match` over an op's fault codes (audit #10b, full sim+metal) `[metal]` — PR #57.
      `MatchPat::Ok(Option<Ident>)`/`Fault(Ident)`; parse `ok`/`ok v`/`fault <code>` arms. The op runs
      without propagating; `BusXfer` gains `code_dst` (the outcome code index: `0` = ok, `1+i` = the
      i-th declared fault code). Resolver `lower_result_match` enforces **exhaustiveness vs
      `op.ret.fault_codes`** (every declared code + `ok`, or a `_`), rejects undeclared/duplicate
      codes and a `?` scrutinee; bounded to a primitive (yielding) bus op. Sim maps an injected
      `bus_fault <code>` → arm; metal decodes the controller's named SR error bits (nak=0x2,
      arblost=0x4, timeout=0x8 — by NAME, not declaration order) → same arm via the yields state
      machine. examples/fault_match.si + tests/match_stmt.rs (7 new) + harness/fault_match.sh.
      **Renode: PASS** — fresh-boot per outcome gives a clean identity matrix (ok→reads, nak→naks,
      timeout→timeouts, arblost→arblosts, no cross-talk); bus_parity.sh still PASS (no regression).
      NOTE: a yielding `every` reaction's *multi-fire* re-arm on metal is a pre-existing, orthogonal
      limitation (the 2nd transaction's completion IRQ lands pending-but-masked) — the gate validates
      the decode on each reaction's first fire; multi-fire re-arm + match over a composed op are
      follow-ups.

### Cluster P3 — P2 follow-ups
The deferred `NOTE:`/`Remaining` items from Cluster P2 (PRs #54–#57). Plan:
`~/.claude/plans/as-an-embedded-firmware-functional-pebble.md`. Each item is its own branch
(`feat/p3-<id>`, **independent off `main`**) + PR targeting `main` (not auto-merged). Order:
smallest/highest-value → largest. (The first feature PR also introduces this section.)

- [x] P3-1 Multi-fire re-arm of a yielding `every` reaction (P2-4 follow-up) `[metal]` — PR #59.
      Root cause (Renode access log): the completion IRQ line is **level** (held asserted until the next
      transaction); after `__BUS_IRQHandler` disabled NVIC#8 a stale **pending** survived, and the next
      kick's `__bus_irq_enable()` re-took it spuriously — resuming before the new transfer completed, so
      the reaction fired only ONCE. Fix: each kick clears the bus IRQ pending (`__bus_irq_clear_pending`
      → NVIC ICPR) before arming (`emit_bus_kick_metal`). Also added `default-run = "silicac"` so
      `cargo run` in the harnesses isn't ambiguous after P2-2's `escape_audit` bin. New
      harness/bus_refire.sh (3 fires/one boot → reads==3); harness/fault_match.sh tightened to **true
      multi-fire** (nak→timeout→arblost→ok in one boot, all PASS); tests/match_stmt.rs
      (clear-before-enable). **Renode: PASS**; bus_parity.sh still PASS.
- [x] P3-2 `match` over a composed (inlined) op (P2-4 follow-up) — PR #60. Lifted the primitive-bus-op
      restriction in `resolver.rs` lower_result_match: the match's outcome rides the *single* bus
      transaction reached through inlining — a composed op like std bme280 `read_temp` = `return
      bus.read_reg(...)?` — so `ok v`/`fault <code>` work one composition hop up, unaware of the bus.
      Fault codes + exhaustiveness come from that transaction (the leaf controller's runtime codes); a
      multi-transaction composed op is rejected with a clear error. No backend change — the sim/metal
      `code_dst` path already services an inlined BusXfer. examples/fault_match_composed.si +
      tests/match_stmt.rs (composed); harness/fault_match.sh parametrized (runs on both examples).
      sim + **Renode PASS**.
- [x] P3-3 Broader typestate: runtime-precondition lowering (P2-3 follow-up) `[metal]` — PR #61. A
      `when <state>` op call NOT proven by a dominating `become <state>` in the same reaction (e.g. the
      state is configured in another reaction) now lowers a RUNTIME guard → safe state on mismatch,
      instead of a conservative compile error; a state no op can establish stays a compile error. A
      generated per-device state cell tracks the runtime state (every `become` writes it); new
      `SirStmt::DriveSafe`. tests/typestate.rs + examples/typestate_runtime.si; sim + **Renode** (guard
      fires -> ticks freeze; with the config reaction it runs clean). NOTE: across-yield preemption of a
      shared device's proven state + the rich Layer-3 site map remain follow-ups.
- [x] P3-4a Fuller LLVM backend — extended scalar subset (P2-1 follow-up) `[llvm]` — PR #62. `backend/llvm.rs`: `If` control flow (branches), `now()` → `llvm.readcyclecounter` (no libc), signed saturate (ashr clamp), and every non-`sys.start` reaction lowered to its own `void @__reaction_N` (no scheduler yet — P3-4c). examples/llvm_features.si + tests/llvm_canary.rs (9) + harness/llvm_canary.sh (`opt -verify` the extended IR). **LLVM 22.1.8 verify OK.**
- [x] P3-4b Fuller LLVM backend — MMIO register access (P2-1 follow-up) `[llvm]` — PR #63. `SirPlace::Reg`
      store + `RegLoad` lower to a `volatile` load/store at `base + offset` via `inttoptr` (rw field =
      read-modify-write; wo/w1c = direct store; read = masked + shifted). device_bases in the LLVM
      backend. examples/llvm_mmio.si + tests/llvm_canary.rs (12) + harness/llvm_canary.sh (`opt -verify`
      + `llc` to object). **LLVM 22.1.8 OK.**
- [x] P3-4c Fuller LLVM backend — metal startup/linker, boots on Renode (P2-1 follow-up) `[metal]` — PR #64.
      `LlvmBackend::with_target(metal)` emits a freestanding module: `thumbv7em-none-eabi` triple, a
      `.vectors` table `[_estack, Reset_Handler]`, and a `Reset_Handler` that runs `sys.start` then idles
      (`wfi`) — no `@main`/syscall. Links against the generated linker script (now drops `.ARM.exidx`) →
      **boots on Renode**: SIR → LLVM IR → `llc` → ELF → reads the cell back as 42, C backend uninvolved.
      examples/boot_nrf52840.si + tests/llvm_canary.rs (14) + harness/llvm_metal.sh. **Renode PASS.** NOTE:
      minimal-viable — only `sys.start` runs on metal via LLVM; the metal scheduler (every/on → TIMER/GPIOTE)
      + yields state machine are the next LLVM step (the periodic/event/bus runtime).
### Cluster P4 — metal LLVM runtime (scheduler + yields)
Build the metal LLVM runtime on top of P3-4c's boot, replaying the C metal scheduler+yields arc in
LLVM IR so a real reactive program runs on Renode built **only through LLVM**. Plan:
`~/.claude/plans/as-an-embedded-firmware-functional-pebble.md`. Stacked branches (feat/p4-1 off main,
p4-2 off p4-1, p4-3 off p4-2); PRs target `main` (not auto-merged).

- [x] P4-1 Metal LLVM scheduler: reset init + `every` → TIMER1 (P3-4c follow-up) `[metal]` — PR #65.
      `Reset_Handler` now does real startup (.data copy / .bss zero loops, output-pin directions +
      input pull-ups) before sys.start; programs TIMER1 (MODE/BITMODE/PRESCALER/CC/INTENSET/START) +
      NVIC, `cpsie i`, idle. New `@TIMER1_IRQHandler` clears EVENTS_COMPARE, re-arms `CC += period`,
      calls the periodic `@__reaction_N`. Full Cortex-M vector table (system + IRQ slots to 16+IRQn) +
      `@__default_handler`/`@HardFault_Handler` stubs. Reuses `c::{TIMER_BASE,timer_plan,timer_priority}`.
      examples/blink_nrf52840.si + tests/llvm_canary.rs (15) + harness/llvm_metal_sched.sh. **Renode
      PASS** — LLVM-built blink toggles the LED on its 500ms period, sim ≡ metal(LLVM).
- [x] P4-2 Metal LLVM `on <pin>.falling` → GPIOTE/NVIC + BASEPRI critical sections (P3-4c follow-up)
      `[metal]` — PR #66. Reset configures GPIOTE channels (from module.events) + input pull-ups +
      NVIC; `@GPIOTE_IRQHandler` clears EVENTS_IN and calls the bound `@__reaction_N`s; GPIOTE vector
      slot 16+6. `SirStmt::Critical` → BASEPRI raise/restore (msr/mrs basepri + ISB/DMB, ceiling via
      basepri_byte). tests/llvm_canary.rs (16) + harness/metal_vs_sim.sh gains a `BUILD=llvm` mode.
      **Renode PASS** — LLVM-built blink_button LED sequence ≡ sim (button + timer); C path unchanged.
- [x] P4-3 Metal LLVM yields state machine + bus IRQ (P3-4c follow-up) `[metal]` — PR #67.
      A yielding reaction → an IRQ-driven segment state machine mirroring the C path: body split at
      each BusXfer; cross-yield temps + __state/__retry/__faulted as module globals (@__rf_N_*);
      `@__react_N_run` = switch on @__rf_N_state into per-segment blocks; resume-decode (SR → temp /
      __faulted → disposition, Retry = back-edge to seg-0); bus kick (CR/SA/RA/DR + @__bus_owner +
      NVIC clear-pending/enable) + `@__BUS_IRQHandler` (resume owner) + bus vector slot 16+8; trigger
      entry coalesces in-flight re-fires. Reuses c::i2c_fault_bit. tests/llvm_canary.rs (18) +
      harness/bus_parity.sh gains BUILD=llvm. **Renode PASS** — button interleaves during the LLVM
      firmware's bus suspension (mid hits=1,samples=0; end both 1), sim ≡ metal(LLVM). C path unchanged.

### Cluster P5 — metal LLVM runtime (finish)
Bring the LLVM metal backend (`backend/llvm.rs`) to full parity with the C backend (`backend/c.rs`) for
the remaining runtime features still stubbed/divergent after P4 — closing the metal LLVM runtime. Plan:
`~/.claude/plans/as-an-embedded-firmware-functional-pebble.md`. Stacked branches (feat/p5-1 off main,
p5-2 off p5-1, p5-3 off p5-2, p5-4 off p5-3); PRs target `main` (not auto-merged). The simulator
implements every feature, so each gate is `sim ≡ metal(LLVM)` on Renode, the same bar the C path meets.

- [x] P5-1 SysTick subsystem + `now()` uptime (P4 follow-up) `[metal]` — PR #68.
      Metal `now()` now reads a SysTick-driven uptime instead of the host `llvm.readcyclecounter`:
      `lower_reset_handler` programs SysTick (SYST_RVR/CVR/CSR at the SCS, 1 ms base via
      `c::systick_reload`) when `needs_systick` (now() ∥ watchdog, mirroring c.rs); a new
      `@SysTick_Handler` advances `@__uptime_ns` by 1 ms; vector slot 15 → SysTick; `SirExpr::Now`
      lowers to `load i64 @__uptime_ns` on metal (host unchanged). Made `c::{module_uses_now,
      body_has_poll,any_stmt}` pub for reuse. examples/uptime_nrf52840.si + tests/llvm_canary.rs (20,
      incl. host-unchanged guard) + new harness/now_uptime.sh. **Renode PASS** — the now()-stamped cell
      reads 300_000_000 ns @350ms = sim, sim ≡ metal(LLVM). C path unchanged.
- [x] P5-2 `@__drive_safe` + overflow-trap safe-state + `Safe` disposition (P4 follow-up) `[metal]` —
      PR #69. Emit `@__drive_safe` (runs module.safe_seqs via a newly-lowered single-store `RegWrite`) +
      a shared drive-safe-then-hold sequence (cpsid → call → wfi loop); route `SirStmt::DriveSafe`, the
      overflow `Trap` path (→ `@__silica_overflow_trap`, not bare `llvm.trap`), and the `Safe`
      disposition through it (mirrors c.rs `emit_drive_safe`/`__silica_overflow_trap`). Host unchanged
      (trap stays `llvm.trap`). tests/llvm_canary.rs (22, incl. host-unchanged guards) + harness/
      overflow_trap.sh gains `BUILD=llvm`. **Renode PASS** — LLVM-built `+` traps & halts (ticks froze
      at 4), `+%` wraps & runs (10); sim ≡ metal(LLVM). C path unchanged.
- [x] P5-3 `poll`/`await` + non-yielding fault flow (P4 follow-up) `[metal]` — PR #70.
      The non-yielding reaction path gains a fault flow: a reaction that can fault via a poll/await
      timeout gets a `%__faulted` flag + Layer-2 disposition routing (`Retry` wraps the body in a
      bounded re-run loop), mirroring c.rs `emit_reaction_fn`. `SirStmt::Poll` → bounded busy-wait;
      `SirStmt::Await` → bounded re-check with `wfi` between checks (full D2 frame suspend deferred, as
      on the C path). examples/{poll,await}_nrf52840.si + tests/llvm_canary.rs (25, incl. retry wrapper
      + busy-wait-has-no-wfi) + new harness/poll_await.sh (the FIRST Renode gate for poll/await — the C
      path only had sim + codegen tests). **Renode PASS** — both poll & await pass when satisfied
      (done=3) and fault→skip on timeout (done=0); sim ≡ metal(LLVM). C path unchanged.
- [x] P5-4 `within`-deadline + watchdog (P4 follow-up) `[metal]` — PR #71. Extends P5-1's SysTick.
      `@__deadline_N`/`@__deadline_missed` globals (yielding reactions, gated on a watchdog); armed on
      the yielding trigger entry; the SysTick handler disarms an idle reaction and decrements+latches a
      missed deadline; the reset configures+starts the watchdog (CR/RLR/KR, 0xAAAA feed); the idle loop
      feeds it only on a clean return to idle (`!__deadline_missed && all frame states 0`). Mirrors
      c.rs. Reuses examples/{deadline,bus_watchdog}_nrf52840.si; tests/llvm_canary.rs (26) + harness/
      {deadline_reset,watchdog_reset}.sh gain `BUILD=llvm`. **Renode PASS** — deadline tight=1/loose=0;
      watchdog wedged=1/healthy=0; sim ≡ metal(LLVM). C path unchanged. **Metal LLVM runtime complete.**

### Cluster P6 — deferred follow-ups (LLVM parity + the deferred register)
Complete the remaining deferred follow-ups: the `; unsupported` LLVM gaps (mirror `backend/c.rs`) and
the broader deferred register (some are front-to-back features: SIR + sim + resolver + both backends).
Plan: `~/.claude/plans/as-an-embedded-firmware-functional-pebble.md`. Stacked branches off `main`; PRs
target `main` (not auto-merged). Each `[metal]` item is `sim ≡ metal` Renode-gated. PAUSE-and-report if a
Renode path can't be validated or a genuine design fork is the user's call (P6-7/8/9).

- [x] P6-1 Rings on the LLVM backend (`RingPush`/`Pop` + `Len`/`Empty`/`Full`) `[metal]` — PR #72.
      A `ring<T,N>` cell lowers to backing-store globals (`@__ring_<n>_buf`=`[N x iW]` +
      `@__ring_<n>_head/_tail/_count`) with push/pop/len as index arithmetic (overwrite-oldest on full,
      0 on empty), mirroring c.rs. Added a `ring_info` field + ring-global emission; `collect_locals`
      now recurses into if/critical and allocates RingPop dst. examples/ring_nrf52840.si +
      tests/llvm_canary.rs (28, repoint the unsupported-signpost test to DeviceOp) + new
      harness/ring_metal.sh. **Renode PASS** — LLVM-built ring len=4/sum=7 = sim; sim ≡ metal(LLVM).
- [x] P6-2 Fixed-point on the LLVM backend (`FixedArith` / `FixedCast`) `[metal]` — PR #73. mul/div in a
      64-bit sign-aware intermediate + rescale by frac (mul `>>`, div `<<`-then-divide, div0 → trap),
      overflow mode at width (trap → @__silica_overflow_trap, wrap=trunc, saturate=clamp); cast shifts
      the binary point + narrows. Mirrors c.rs fixmul/fixdiv. Extended the arith-trap detection to
      FixedArith. examples/fixed_nrf52840.si (runtime operand, constant divisor) + tests/llvm_canary.rs
      (31) + harness/fixed_metal.sh (`opt -O2` folds the scale constants like the C `-Os`; runtime
      divisor would need libgcc either way). **Renode PASS** — 3·n=18 / n÷4=1 = sim; sim ≡ metal(LLVM).
- [x] P6-3 LLVM HardFault fault-decoder parity (Layer-3 region map) `[metal]` — PR #74. Port of c.rs
      emit_fault_decoder: @__owner_start/@__owner_end ownership table (layer3::ownership_map, no on-device
      strings) + a @HardFault_Handler that reads SCB CFSR/BFAR, finds the owner on a valid BFAR, records
      {addr,owner,cfsr,pending}. Replaced the bare wfi HardFault stub. Reuse examples/fault_nrf52840.si +
      tests/llvm_canary.rs (32) + harness/fault_decode_metal.sh. **Renode PASS** — decoder+tables link
      and coexist with a live program; address→owner decode validated by sim + canary (a precise BFAR
      fault can't be injected on Renode, as for the C Layer-3). Finer per-call-site map stays deferred.
- [x] P6-4 Dynamic `host_io.print` (host) — PR #75. `host_io.print(<value>)` was unimplemented on ALL
      backends (sim no-op, C TODO, LLVM `; unsupported`); now all three print the runtime value as
      unsigned decimal. LLVM host: inline udiv/urem itoa into a stack buffer + raw write syscall (no
      libc); sim: to_string; C: equivalent inline itoa. examples/print_value.si + tests/llvm_canary.rs
      (33) + harness/print_value.sh. **PASS** (host) — LLVM binary and sim both print 42 for
      print(40+2). Host-only (no metal host_io until semihosting, P6-7). NOTE: `SirExpr::Bytes` as a
      standalone operand never arises from source (literals → HostIoPrintStr), so it stays a signpost.
- [x] P6-5 `await` full D2-style frame suspend (both backends) `[metal]` — PR #76. await now reuses the
      bus state machine: the body is segmented at each top-level await; the terminator arms
      `__rf_N.__await`/`__await_deadline` (1 ms ticks) and returns; the SysTick handler re-checks each
      suspended await (resume / fault→frame-disposition / wait). The `__await` flag keeps it distinct
      from a bus suspend (bus IRQ resumes those), so the bus path is untouched and an await suspend does
      not gate the watchdog feed. `reaction.yields` stays BusXfer-only; the dispatch routes
      `yields || has_await` to the yielding emitter; the resolver rejects nested await (top-level only).
      examples/await_interleave_nrf52840.si + tests/await.rs + tests/llvm_canary.rs + harness/
      await_interleave.sh. **Renode PASS** (C and BUILD=llvm) — a peer reaction runs DURING the await
      suspension → done=3 = sim; bus_parity/bus_refire/deadline_reset/watchdog_reset/poll_await all
      still PASS. sim ≡ metal. NOTE: 1 ms SysTick recheck cadence; resumed body runs in SysTick context
      (shared cells stay protected by BASEPRI criticals) — a PendSV priority-preserving resume is a
      follow-up. poll unchanged.
- [x] P6-6 TIMER-rebase `now()`/deadlines, retire SysTick (both backends) `[metal]`. SysTick did four
      jobs — `now()` uptime, the `within`-deadline countdown (P5-4), the await re-check (P6-5), and the
      watchdog wake cadence — all migrated onto a dedicated **TIMER2** (1 MHz/1 µs, 32-bit, IRQ10;
      TIMER0/IRQ8 collides with the bus, TIMER1 drives `every`). `now()` is now **1 µs** (was 1 ms): it
      reads the live counter via a CAPTURE channel and combines it with a software wrap high word the 1 ms
      TIMER2 COMPARE tick maintains (64-bit monotonic; the tick always catches the ~4295 s wrap). The same
      1 ms tick does the deadline/await countdowns and serves as the watchdog wake. SysTick is fully
      retired (no handler, no SCS RVR/CSR programming, vector slot 15 → default). **Renode capability
      verified first** (the plan's residual risk): a bare-metal probe confirmed Renode's `NRF52840_Timer`
      models `TASKS_CAPTURE[n]`→`CC[n]` (monotonic live counter). Both backends mirrored;
      `systick_reload` deleted. tests/metal_codegen.rs + tests/llvm_canary.rs (SysTick→TIMER2) +
      harness/now_uptime.sh (µs expectation, C+LLVM switch). **Renode PASS** (C and BUILD=llvm): `now()`
      reads exactly 300000000 ns at the 300 ms tick = sim; the full metal suite (deadline_reset/
      watchdog_reset/await_interleave/poll_await/bus_*/float/semihosting/…) re-greened on the TIMER2 base,
      both backends. Follow-up: a sub-1 ms wrap-lag glitch in `now()` once every ~71 min is accepted (the
      tick catches it within 1 ms).
- [x] P6-7 Metal semihosting (`host_io` on metal, both backends) `[metal]` — PR #78. `host_io.print` on
      metal → ARM semihosting (NUL-terminated constant + BKPT 0xAB SYS_WRITE0; runtime value reuses the
      decimal itoa). Both backends (LLVM inline asm; C register-pinned bkpt). The earlier PAUSE is
      resolved: Renode has no EnableSemihosting toggle, but attaching `UART.SemihostingUart @ cpu` +
      `CreateFileBackend` captures the output headlessly. examples/semihosting_nrf52840.si +
      tests/llvm_canary.rs + harness/semihosting.sh. **Renode PASS** (C and BUILD=llvm) — captured
      stream (n=1/n=2/n=3) = sim stdout; sim ≡ metal.
- [x] P6-8 Runtime float arithmetic on metal (front-to-back) `[metal]` — PR #77. Float was storage-only
      (math silently miscompiled to integer). Now front-to-back: SIR `FloatLit`/`FloatArith`; resolver
      types it type-directed (a decimal/int literal is float in a float context — `3.5` stays Q16.16
      fixed elsewhere — and `+ - * /` route to FloatArith on a float operand); sim reinterprets the u64
      bits as f32/f64; both backends enable the FPU (CPACR) in reset + emit hardware float (LLVM: IEEE
      bits in iN + bitcast around fadd/fmul, `llc -mcpu=cortex-m4 -float-abi=hard`; C: plain ops,
      `-mfpu=fpv4-sp-d16 -mfloat-abi=hard`) — no soft-float libcalls. Surface syntax: type-directed
      `+ - * /` (user-chosen). examples/float_nrf52840.si + tests/fpu.rs (7) + tests/llvm_canary.rs (35)
      + harness/float_metal.sh. **Renode PASS** (C and LLVM) — acc=4.5/out=9.0 bit-exact = sim; sim ≡
      metal. NOTE: float compares, int↔float conversion, and f64 on the single-precision M4F are
      follow-ups; mixing int+float operands without a cast is unsupported.
- [x] P6-9 Multi-consumer bus arbitration / bounded per-bus wait queue (front-to-back) `[metal]`. Two
      reactions reading two sensors on the SAME `i2c0` used to break silently — the second kick clobbered
      the single `@__bus_owner` and the first read was lost. Now the bus is priority-arbitrated, with an
      **implicit** surface (no new syntax; sharing one controller auto-serializes) and **priority-ordered**
      grant (user-chosen). The arbitration key is `BusXfer.device` (no SIR change). Sim oracle: per-bus
      busy flag + bounded waiter queue keyed by device — a BusXfer on a busy bus joins the queue
      (`BusBlocked`); on completion the highest-priority waiter is granted (`BusGranted`, ties → lowest
      id). Both backends mirror it, **gated on contention** (≥2 reactions on one bus): a standalone
      `__bus_waiting_N` flag per contender, a claim gated on free + no higher-priority waiter (else record
      + suspend without clobbering the owner), and an IRQ-grant chain that resumes the top waiter (which
      retries its kick). A single-consumer bus keeps the simpler single-owner path byte-for-byte.
      examples/bus_contend_nrf52840.si + tests/sim_bus_arbitration.rs + tests/llvm_canary.rs (P6-9 shape +
      single-consumer no-arbitration) + harness/bus_arbitration.sh. **Renode PASS** (C and BUILD=llvm) —
      both reads complete (mid 0/0 → post 1/1) = sim; single-bus gates (bus_parity/bus_refire/
      deadline_reset/watchdog_reset) still PASS on both backends.
## Completed log
_(append `item — PR #NN — date` here as items land)_

Cluster P6 — fully landed on `main`:
- P6-1 Rings on the LLVM backend — PR #72 — 2026-06-27
- P6-2 Fixed-point on the LLVM backend — PR #73 — 2026-06-27
- P6-3 LLVM HardFault Layer-3 fault-decoder parity — PR #74 — 2026-06-27
- P6-4 Dynamic `host_io.print` (host) — PR #75 — 2026-06-27
- P6-5 `await` full D2-style frame suspend (both backends) — PR #76 — 2026-06-27
- P6-8 Runtime float arithmetic on metal (front-to-back) — PR #77 — 2026-06-27
- P6-7 Metal semihosting (`host_io` on metal, both backends) — PR #78 — 2026-06-27
- P6-9 Multi-consumer bus arbitration (priority-ordered, implicit) — PR #81 — 2026-06-27 _(recreated from #79, which auto-closed when its stacked base branch was deleted)_
- P6-6 TIMER-rebase `now()`/deadlines onto TIMER2, retire SysTick — PR #80 — 2026-06-27

Cluster P7 — post-P6 audit remediation (tracking issue #86):
- P7-1 LLVM/C MMIO barrier parity — PR #87 — 2026-06-30
- P7-2 `EXC_FRAME` soundness on the FPU path (104 B) — PR #88 — 2026-06-30
- P7-3 Hard-error on unknown/unsupported types — PR #89 — 2026-06-30
- P7-4a Layer-3 site map: PC→(handler, when-state) table generation — PR #90 — 2026-06-30
- P7-4b Layer-3 site map: fault-time PC decode + HardFault wire-up — PR #91 — 2026-06-30
- P7-5a Implement `buffer<N>` (bounded byte buffer) — PR #92 — 2026-06-30
- P7-5b Implement `pool<T,N>` (bounded pool) — PR #93 — 2026-06-30
- P7-6a Register residual: `rc`/`pop_on_read` read-clear in conditions — PR #94 — 2026-06-30
- P7-6b Register residual: `reserved`-bit preservation + `width=` enforcement — PR #95 — 2026-06-30
- P7-7a Agentic-eval harness scaffold (runner + author/edit/debug task set) — PR #96 — 2026-06-30
- P7-7b Agentic eval: run on real agent output + report (`.raw`/escape-hatch frequency) — PR #97 — 2026-06-30
- P7-8a DTS→Silica importer: MVP spike (flat `.dts` subset → board/soc skeleton) — PR #98 — 2026-06-30
- P7-8b DTS importer: pins/clocks/memory node coverage + round-trip — PR #99 — 2026-06-30
