//! C backend — lowers SIR to a freestanding C source file.
//!
//! Design constraints (§6.2):
//!   - Fixed-width types only (`uint32_t`, `uint8_t`, etc.) — never bare `int`.
//!   - Explicit checked arithmetic (no reliance on signed-overflow UB).
//!   - No C bitfields.
//!   - Explicit barriers where needed.
//!   - No libc beyond what the host target explicitly needs (`stdio.h` for
//!     host_io, `time.h` / `unistd.h` for the `every` timer).
//!
//! Output is a single `.c` file.  The caller invokes `cc` to compile it.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write;

use crate::backend::Target;
use crate::layer3;
use crate::sir::*;

// ─── Entry point ─────────────────────────────────────────────────────────────

pub struct CBackend {
    buf: String,
    indent: usize,
    target: Target,
    /// device id → MMIO base address (populated at the start of `emit`).
    device_bases: HashMap<usize, u64>,
    /// device id → (register name → absolute MMIO address), for `BusXfer`
    /// lowering that resolves a controller's declared registers by name.
    device_regs: HashMap<usize, HashMap<String, u64>>,
    /// Names of module-level variables (cells + `let`s); used to tell a reaction
    /// temporary (needs a local C declaration) from a global.
    global_vars: HashSet<String>,
    /// Max reaction priority in the module, for the priority-ceiling → BASEPRI
    /// mapping (§5.5).
    max_priority: u8,
    /// While emitting a yielding reaction's body on metal, the temps that live
    /// in its static frame struct (§5.2) and the struct's name, so var refs are
    /// rewritten `name` → `__rf_N.name` (they must survive the dispatcher
    /// returning between segments).  Empty/None otherwise.
    frame_vars: HashSet<String>,
    frame_name: Option<String>,
    /// The disposition of the reaction currently being emitted, so a `poll`
    /// timeout (§3.2) inside it routes to the right Layer-2 terminal.  None
    /// outside a reaction body.
    current_disposition: Option<SirDisposition>,
    /// Distinct width-checked-arithmetic shapes used in the module (§4.3); each
    /// gets a `static inline` helper emitted once in the preamble.
    arith_combos: HashSet<(SirArithOp, OverflowMode, u8, bool)>,
    fixed_combos: HashSet<(FixedArithOp, OverflowMode, u8, bool, u8)>,
    /// Ring cell name → (element bytes, capacity), for ring op codegen (§5.3).
    ring_info: HashMap<String, (u8, u32)>,
    /// Yielding-reaction id → its `within <d>` deadline in SysTick base ticks
    /// (§4.5/§5.6).  Populated only when a watchdog exists (the reset mechanism);
    /// an overrun sets `__deadline_missed`, which stops the watchdog feed.
    deadline_ticks: HashMap<usize, u32>,
}

impl CBackend {
    pub fn new() -> Self {
        Self::with_target(Target::Host)
    }

    pub fn with_target(target: Target) -> Self {
        CBackend {
            buf: String::new(),
            indent: 0,
            target,
            device_bases: HashMap::new(),
            device_regs: HashMap::new(),
            global_vars: HashSet::new(),
            max_priority: 0,
            frame_vars: HashSet::new(),
            frame_name: None,
            current_disposition: None,
            arith_combos: HashSet::new(),
            fixed_combos: HashSet::new(),
            ring_info: HashMap::new(),
            deadline_ticks: HashMap::new(),
        }
    }

    /// A C reference to a SIR var: prefixed with the current yielding-reaction
    /// frame (`__rf_N.name`) when `name` is one of its cross-yield temps, else
    /// the bare name (function-local or module global).
    fn var_ref(&self, name: &str) -> String {
        match &self.frame_name {
            Some(f) if self.frame_vars.contains(name) => format!("{}.{}", f, name),
            _ => name.to_string(),
        }
    }

    /// nRF52840 has 3 NVIC priority bits; priorities occupy the top 3 bits.
    const PRIO_SHIFT: u8 = 5;

    /// Map an abstract reaction priority (higher = more urgent) to a hardware
    /// BASEPRI/NVIC byte (lower number = more urgent), starting at level 1 so a
    /// ceiling is never 0 (BASEPRI 0 disables masking).  §5.5.
    fn basepri_byte(&self, our_priority: u8) -> u8 {
        let level = (self.max_priority - our_priority) + 1;
        level << Self::PRIO_SHIFT
    }

    /// Emit a complete C translation unit from a `SirModule` and return it as
    /// a `String`.
    pub fn emit(mut self, module: &SirModule) -> String {
        for dev in &module.devices {
            if let Some(base) = dev.base_addr {
                self.device_bases.insert(dev.id, base);
                let regs = dev.regs.iter().map(|r| (r.name.clone(), base + r.offset)).collect();
                self.device_regs.insert(dev.id, regs);
            }
        }
        for v in &module.vars {
            if let SirType::Ring { elem_bytes, cap } = v.ty {
                self.ring_info.insert(v.name.clone(), (elem_bytes, cap));
            }
        }
        self.global_vars = module.vars.iter().map(|v| v.name.clone()).collect();
        self.max_priority = module.reactions.iter().map(|r| r.priority).max().unwrap_or(0);
        self.arith_combos = collect_arith(module);
        self.fixed_combos = collect_fixed(module);
        match self.target {
            Target::Host => {
                self.emit_header(module);
                self.emit_newline();
                self.emit_globals(module);
                self.emit_arith_helpers();
                self.emit_now_helper(module);
                self.emit_newline();
                self.emit_reaction_fns(module);
                self.emit_newline();
                self.emit_main(module);
            }
            Target::MetalNrf52840 => {
                // §4.5/§5.6 — per-reaction `within` deadlines are enforced on
                // metal only when a watchdog exists (the reset mechanism): an
                // overrun stops the feed.  Yielding reactions only — a non-yielding
                // one runs to completion in its ISR and is bounded by construction.
                if module.watchdog_device.is_some() {
                    for r in &module.reactions {
                        if r.yields {
                            if let Some(ns) = r.deadline_ns {
                                let ticks = ns.div_ceil(1_000_000).max(1) as u32; // 1ms SysTick base
                                self.deadline_ticks.insert(r.id, ticks);
                            }
                        }
                    }
                }
                self.emit_header_metal(module);
                self.emit_newline();
                self.emit_globals(module);
                self.emit_now_helper(module);
                self.emit_deadline_state();
                self.emit_newline();
                self.emit_drive_safe(module);
                self.emit_arith_helpers();
                self.emit_reaction_fns(module);
                self.emit_newline();
                self.emit_metal_startup(module);
            }
        }
        self.buf
    }

    // ── Preamble ─────────────────────────────────────────────────────────────

    fn emit_header(&mut self, module: &SirModule) {
        // Conditionally include host-only headers.  The scans must recurse into
        // nested statements: after cell analysis, intrinsics / `exit` can be
        // wrapped inside `Critical` (and `If`) bodies, so a top-level-only scan
        // would miss required includes and emit C that won't compile.
        let needs_stdio = module
            .reactions
            .iter()
            .any(|r| any_stmt(&r.body, &stmt_uses_stdio));
        let needs_timer = module.reactions.iter().any(|r| {
            matches!(r.trigger, SirTrigger::EveryNs(_))
        }) || module_uses_now(module); // `now()` needs the host monotonic clock (§4.5)
        let needs_exit = module
            .reactions
            .iter()
            .any(|r| any_stmt(&r.body, &|s| matches!(s, SirStmt::Exit(_))));

        self.line("/* Generated by silicac — do not edit.  §6.2 freestanding-C subset. */");
        // Request POSIX.1-2008 when we use nanosleep / clock_gettime / struct
        // timespec.  This must appear before any system header.
        if needs_timer {
            self.line("#define _POSIX_C_SOURCE 200809L");
        }
        self.line("#include <stdint.h>");

        if needs_stdio {
            self.line("#include <stdio.h>");
        }
        if needs_timer {
            self.line("#include <time.h>");
        }
        if needs_exit {
            self.line("#include <stdlib.h>");
        }
        if needs_stdio || needs_timer {
            self.line("");
            self.line("/* host_io intrinsic — §7.1 mock device */");
            self.line("static void __host_io_print(const uint8_t *s, uint32_t len) {");
            self.line("    /* fwrite/size_t are C library interface types, not SIR types (§6.2) */");
            self.line("    fwrite(s, 1U, (size_t)len, stdout);");
            self.line("}");
        }
        if needs_timer {
            self.line("");
            self.line("/* nanosleep helper — C library wrapper; time_t/long are POSIX interface types */");
            self.line("static void __host_sleep_ns(uint64_t ns) {");
            self.line("    struct timespec __ts;");
            self.line("    __ts.tv_sec  = (time_t)(ns / UINT64_C(1000000000));");
            self.line("    __ts.tv_nsec = (long)(ns % UINT64_C(1000000000));");
            self.line("    nanosleep(&__ts, (struct timespec *)0);");
            self.line("}");
            self.line("");
            self.line("/* monotonic-time helper — host target; §4.5/D15 fixed-rate scheduling */");
            self.line("static uint64_t __get_mono_ns(void) {");
            self.line("    struct timespec __ts;");
            self.line("    clock_gettime(CLOCK_MONOTONIC, &__ts);");
            self.line("    /* tv_sec/tv_nsec: POSIX interface types, cast to fixed-width (§6.2) */");
            self.line("    return (uint64_t)__ts.tv_sec * UINT64_C(1000000000)");
            self.line("         + (uint64_t)__ts.tv_nsec;");
            self.line("}");
        }
    }

    // ── Global variables (cells) ──────────────────────────────────────────────

    fn emit_globals(&mut self, module: &SirModule) {
        if module.vars.is_empty() {
            return;
        }
        self.line("/* program variables and cells */");
        // On metal, cells are touched by ISRs and must not be optimized away or
        // cached — emit them volatile (and it keeps the boot-test observable).
        let qualifier = if self.target == Target::MetalNrf52840 { "static volatile" } else { "static" };
        for var in &module.vars {
            // A bounded ring (§5.3): backing array + head/tail/count, counted in
            // the static RAM budget via SirType::Ring::byte_size.
            if let SirType::Ring { elem_bytes, cap } = var.ty {
                let elem = SirType::ring_elem_ctype(elem_bytes);
                self.line(&format!(
                    "{q} {e} __ring_{n}_buf[{c}]; {q} uint32_t __ring_{n}_head = 0, __ring_{n}_tail = 0, __ring_{n}_count = 0; /* ring<{e},{c}> */",
                    q = qualifier, e = elem, n = var.name, c = cap
                ));
                continue;
            }
            let c_ty = var.ty.c_type();
            let c_init = expr_to_c_literal(&var.init);
            let comment = if var.is_cell { " /* cell */" } else { "" };
            let decl = format!("{} {} {} = {};{}", qualifier, c_ty, var.name, c_init, comment);
            self.line(&decl);
        }
        self.emit_newline();
    }

    // Emit the `static inline` helpers for every width-checked arithmetic shape
    // used in the module (§4.3 / SIL-004).
    fn emit_arith_helpers(&mut self) {
        if self.arith_combos.is_empty() && self.fixed_combos.is_empty() {
            return;
        }
        let mut combos: Vec<_> = self.arith_combos.iter().copied().collect();
        combos.sort_by_key(|(op, mode, w, s)| (*w, *s as u8, *op as u8, *mode as u8));
        self.line("/* §4.3 width-checked integer arithmetic (SIL-004): plain +/-/* trap on */");
        self.line("/* overflow; `+%`-family wrap, `+|`-family saturate, at the target width. */");
        // The trap helper is shared by integer arith and fixed mul/div (div-by-
        // zero and out-of-range results both trap to safe-state).
        let fixed_needs_trap = self
            .fixed_combos
            .iter()
            .any(|(op, m, ..)| *op == FixedArithOp::Div || *m == OverflowMode::Trap);
        if combos.iter().any(|(_, m, _, _)| *m == OverflowMode::Trap) || fixed_needs_trap {
            match self.target {
                Target::MetalNrf52840 => {
                    self.line("static void __silica_overflow_trap(void) {");
                    self.line("    __asm__ volatile(\"cpsid i\" ::: \"memory\"); /* overflow → halt */");
                    self.line("    __drive_safe();");
                    self.line("    for (;;) { __asm__ volatile(\"wfi\"); } /* hold in safe state (§4.3) */");
                    self.line("}");
                }
                Target::Host => {
                    self.line("static void __silica_overflow_trap(void) { __builtin_trap(); }");
                }
            }
        }
        for (op, mode, w, s) in combos {
            self.line(&arith_helper_def(op, mode, w, s));
        }
        // Fixed-point multiply/divide helpers (§4.3, P0-3c).
        let mut fixed: Vec<_> = self.fixed_combos.iter().copied().collect();
        fixed.sort_by_key(|(op, mode, w, s, f)| (*w, *s as u8, *op as u8, *mode as u8, *f));
        for (op, mode, w, s, f) in fixed {
            self.line(&fixed_helper_def(op, mode, w, s, f));
        }
        self.emit_newline();
    }

    /// Emit `__now_ns()` (§4.5) backing the `now()` expression, when used.  Host
    /// reads the POSIX monotonic clock; metal returns a SysTick-driven uptime
    /// counter (1 ms resolution — the reactive scheduling granularity).
    fn emit_now_helper(&mut self, module: &SirModule) {
        if !module_uses_now(module) {
            return;
        }
        match self.target {
            Target::Host => {
                self.line("/* now() — current time, ns (§4.5) */");
                self.line("static uint64_t __now_ns(void) { return __get_mono_ns(); }");
            }
            Target::MetalNrf52840 => {
                self.line("/* now() — uptime in ns, advanced by SysTick (1ms base, §4.5) */");
                self.line("static volatile uint64_t __uptime_ns = 0ULL;");
                self.line("static uint64_t __now_ns(void) { return __uptime_ns; }");
            }
        }
        self.emit_newline();
    }

    /// Declare the per-reaction `within`-deadline countdowns + the shared
    /// `__deadline_missed` flag (§4.5/§5.6).  Referenced by the trigger fns
    /// (arm), the SysTick handler (decrement), and the idle loop (gate the feed).
    fn emit_deadline_state(&mut self) {
        if self.deadline_ticks.is_empty() {
            return;
        }
        self.line("/* §4.5/§5.6 `within` deadline countdowns (SysTick ticks); overrun stops the watchdog feed */");
        let mut ids: Vec<usize> = self.deadline_ticks.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            self.line(&format!("static volatile uint32_t __deadline_{} = 0U;", id));
        }
        self.line("static volatile uint32_t __deadline_missed = 0U;");
    }

    // ── Reaction functions ────────────────────────────────────────────────────

    fn emit_reaction_fns(&mut self, module: &SirModule) {
        for reaction in &module.reactions {
            self.emit_reaction_fn(reaction);
            self.emit_newline();
        }
    }

    fn emit_reaction_fn(&mut self, reaction: &SirReaction) {
        let fn_name = reaction_fn_name(reaction.id);
        let comment = trigger_comment(&reaction.trigger);
        // On metal a yielding reaction lowers to an IRQ-driven segment state
        // machine (§5.2): it suspends on each bus transaction so the scheduler
        // runs other work, and the bus-completion IRQ resumes it.  This replaces
        // the earlier bounded busy-wait, which could not interleave.
        if self.target == Target::MetalNrf52840 && reaction.yields {
            self.emit_yielding_reaction_metal(reaction, &fn_name, &comment);
            return;
        }
        self.line(&format!("/* {} */", comment));
        self.line(&format!("static void {}(void) {{", fn_name));
        self.indent += 1;
        if self.target == Target::MetalNrf52840 {
            // Declare reaction-local temporaries (`__busN`, `__rN`, op-inlining
            // `__argN`, …) that the sim keeps in its activation frame; globals
            // (cells / module `let`s) are declared module-wide already.
            for name in self.collect_reaction_temps(reaction) {
                self.line(&format!("uint32_t {} = 0U;", name));
            }
        }
        // A non-yielding reaction can still fault via a `poll` timeout (§3.2); if
        // so, give it the fault flag + Layer-2 disposition (a `retry` disposition
        // wraps the body in a bounded loop, as the yielding path does).
        let has_poll = self.target == Target::MetalNrf52840 && body_has_poll(&reaction.body);
        if has_poll {
            self.line("uint8_t __faulted = 0U;");
            self.current_disposition = Some(reaction.disposition);
            let is_retry = matches!(reaction.disposition, SirDisposition::Retry { .. });
            if is_retry {
                self.line("for (uint32_t __retry = 0U; ; __retry++) {");
                self.indent += 1;
                self.line("__faulted = 0U;");
            }
            for stmt in &reaction.body {
                self.emit_stmt(stmt);
            }
            if is_retry {
                self.line("return; /* clean completion exits the retry loop */");
                self.indent -= 1;
                self.line("}");
            }
            self.current_disposition = None;
        } else {
            for stmt in &reaction.body {
                self.emit_stmt(stmt);
            }
        }
        self.indent -= 1;
        self.line("}");
    }

    /// Lower a yielding reaction to an IRQ-driven segment state machine (§5.2).
    /// The flat body is split at each top-level `BusXfer` into segments held in a
    /// static frame (`__rf_N`: segment state + retry counter + fault flag + every
    /// cross-yield temp — these must survive the dispatcher returning, so they
    /// live in the frame, not on the C stack).  Each transaction kicks the
    /// controller, enables its completion IRQ, and **returns**; the scheduler
    /// runs other reactions while it is in flight, and the bus IRQ handler
    /// re-enters the dispatcher at the next segment, which reads the result and
    /// continues.  A wedged bus never resumes, so the reaction stays in flight
    /// and the watchdog catches it (§5.6) — matching the simulator's `Hang`.
    fn emit_yielding_reaction_metal(&mut self, reaction: &SirReaction, fn_name: &str, comment: &str) {
        let n = reaction.id;
        let frame = format!("__rf_{}", n);

        // Segment the body at each top-level BusXfer (yields are hoisted to the
        // top level by the resolver, and a critical cannot span one).
        let mut segs: Vec<(Vec<&SirStmt>, Option<&SirStmt>)> = Vec::new();
        let mut cur: Vec<&SirStmt> = Vec::new();
        for stmt in &reaction.body {
            if matches!(stmt, SirStmt::BusXfer { .. }) {
                segs.push((std::mem::take(&mut cur), Some(stmt)));
            } else {
                cur.push(stmt);
            }
        }
        segs.push((cur, None));

        // Frame struct: dispatcher state + retry/fault + every cross-yield temp.
        let temps = self.collect_reaction_temps(reaction);
        self.line(&format!("/* {} — IRQ-driven yielding reaction (§5.2) */", comment));
        self.line("static volatile struct {");
        self.indent += 1;
        self.line("uint32_t __state;   /* 0 = idle/ready; s = awaiting segment s's resume */");
        self.line("uint32_t __retry;");
        self.line("uint8_t  __faulted;");
        for t in &temps {
            self.line(&format!("uint32_t {};", t));
        }
        self.indent -= 1;
        self.line(&format!("}} {} = {{0}};", frame));

        // Rewrite body var refs to frame members while emitting the dispatcher.
        self.frame_name = Some(frame.clone());
        self.frame_vars = temps.iter().cloned().collect();

        self.line(&format!("static void __react_{}_run(void) {{", n));
        self.indent += 1;
        self.line(&format!("switch ({}.__state) {{", frame));
        self.indent += 1;
        self.line(&format!("case 0U: {f}.__retry = 0U; {f}.__faulted = 0U; goto __seg_{n}_0;", f = frame, n = n));
        for s in 1..segs.len() {
            self.line(&format!("case {s}U: goto __seg_{n}_{s};", s = s, n = n));
        }
        self.line("default: return;");
        self.indent -= 1;
        self.line("}");

        for (i, (pre, xfer)) in segs.iter().enumerate() {
            // Segment label (labels sit at the dispatcher's brace level).
            self.indent -= 1;
            self.line(&format!("__seg_{}_{}:", n, i));
            self.indent += 1;
            // A resumed segment first reads the prior transaction's result and,
            // on a propagated fault, applies the Layer-2 disposition.
            if i >= 1 {
                if let Some(SirStmt::BusXfer { device, op, dst, propagate, .. }) = segs[i - 1].1 {
                    let dref = self.var_ref(dst);
                    for l in self.emit_bus_resume_metal(*device, op, &dref, &frame) {
                        self.line(&l);
                    }
                    if *propagate {
                        self.line(&format!("if ({}.__faulted) {{", frame));
                        self.indent += 1;
                        self.emit_disposition_frame(reaction.disposition, &frame, n);
                        self.indent -= 1;
                        self.line("}");
                    }
                }
            }
            for &stmt in pre.iter() {
                self.emit_stmt(stmt);
            }
            // Terminate the segment: kick the next transaction (suspend) or, on
            // the tail segment, complete and become ready to fire again.
            if let Some(SirStmt::BusXfer { device, op, args, .. }) = xfer {
                for l in self.emit_bus_kick_metal(*device, op, args, n) {
                    self.line(&l);
                }
                self.line(&format!("{}.__state = {}U;", frame, i + 1));
                self.line("return; /* suspend on the bus (§5.2) */");
            } else {
                self.line(&format!("{}.__state = 0U; /* complete; ready to fire again */", frame));
                self.line("return;");
            }
        }
        self.indent -= 1;
        self.line("}");

        self.frame_name = None;
        self.frame_vars.clear();

        // Trigger entry (SysTick/GPIOTE/main): coalesce a re-fire that arrives
        // while a prior activation is still in flight (§5.1), as the sim does.
        self.line(&format!("void {}(void) {{", fn_name));
        self.indent += 1;
        // §5.1/D02 overflow policy when a re-fire arrives still-in-flight.  With a
        // single pending slot and no event payload, coalesce and drop-newest both
        // drop the re-fire; `fault` drives the system to its safe state.
        match reaction.overflow {
            SirOverflow::Coalesce => {
                self.line(&format!("if ({}.__state != 0U) return; /* coalesce: still in flight (§5.1) */", frame));
            }
            SirOverflow::DropNewest => {
                self.line(&format!("if ({}.__state != 0U) return; /* drop-newest: still in flight (§5.1/D02) */", frame));
            }
            SirOverflow::Fault => {
                self.line(&format!("if ({}.__state != 0U) {{ /* fault: event overflow (§5.1/D02) */", frame));
                self.indent += 1;
                self.line("__asm__ volatile(\"cpsid i\" ::: \"memory\");");
                self.line("__drive_safe();");
                self.line("for (;;) { __asm__ volatile(\"wfi\"); }");
                self.indent -= 1;
                self.line("}");
            }
        }
        // §4.5/§5.6 — arm this activation's `within` deadline (in SysTick ticks);
        // if it is still in flight when the countdown elapses, it overran.
        if let Some(ticks) = self.deadline_ticks.get(&n).copied() {
            self.line(&format!("__deadline_{} = {}U; /* arm `within` deadline (§4.5) */", n, ticks));
        }
        self.line(&format!("__react_{}_run();", n));
        self.indent -= 1;
        self.line("}");
    }

    /// Layer-2 disposition at a resumed segment's propagated fault (§4.4/§5.4),
    /// the frame-state counterpart of `emit_disposition_terminal`.
    fn emit_disposition_frame(&mut self, disp: SirDisposition, frame: &str, n: usize) {
        match disp {
            SirDisposition::Retry { max } => {
                // Re-run from segment 0 (re-kicks the first transaction) without
                // resetting `__retry` (only a fresh fire, case 0, resets it).
                self.line(&format!(
                    "if ({f}.__retry < {max}U) {{ {f}.__retry++; {f}.__faulted = 0U; goto __seg_{n}_0; }}",
                    f = frame, max = max, n = n
                ));
                self.line(&format!("{}.__state = 0U; return; /* retries exhausted → escalate */", frame));
            }
            SirDisposition::Skip => {
                self.line(&format!("{}.__state = 0U; return; /* skip: drop this activation */", frame))
            }
            SirDisposition::Safe => {
                // Mask interrupts before driving safe-state so nothing runs
                // concurrently with (or after) the safe writes — the sim's
                // `drive_safe` halts the whole machine.
                self.line("__asm__ volatile(\"cpsid i\" ::: \"memory\"); /* no further reactions */");
                self.line("__drive_safe();");
                self.line("for (;;) { __asm__ volatile(\"wfi\"); } /* hold in safe state */");
            }
            SirDisposition::Escalate => {
                self.line(&format!("{}.__state = 0U; return; /* escalate → Layer-3 */", frame))
            }
        }
    }

    /// Reaction-local temporaries: every `SirPlace::Var` assignment target and
    /// `BusXfer` destination that is not a module-level global, in first-seen
    /// order (deduped).
    fn collect_reaction_temps(&self, reaction: &SirReaction) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        self.collect_temps_in(&reaction.body, &mut seen, &mut out);
        out
    }

    /// Recurse over a statement list (including `If`/`Critical` bodies) adding
    /// each non-global `Var` assignment target and `BusXfer` destination.
    fn collect_temps_in(&self, stmts: &[SirStmt], seen: &mut HashSet<String>, out: &mut Vec<String>) {
        for stmt in stmts {
            match stmt {
                SirStmt::Assign { target: SirPlace::Var(name), .. } => self.add_temp(name, seen, out),
                SirStmt::RingPop { dst, .. } => self.add_temp(dst, seen, out),
                SirStmt::BusXfer { dst, .. } => self.add_temp(dst, seen, out),
                SirStmt::If { then, .. } => self.collect_temps_in(then, seen, out),
                SirStmt::Critical { body, .. } => self.collect_temps_in(body, seen, out),
                _ => {}
            }
        }
    }

    fn add_temp(&self, name: &str, seen: &mut HashSet<String>, out: &mut Vec<String>) {
        if !self.global_vars.contains(name) && seen.insert(name.to_string()) {
            out.push(name.to_string());
        }
    }

    /// Resolve a bus controller's CR/SR/SA/RA/DR MMIO addresses (declared by
    /// name), or an `#error` line if the device has no base or is missing one.
    fn bus_regs_metal(&self, device: usize) -> Result<(u64, u64, u64, u64, u64), String> {
        let regs = self
            .device_regs
            .get(&device)
            .ok_or_else(|| format!("#error \"bus controller device {} has no MMIO base\"", device))?;
        let a = |name: &str| regs.get(name).copied();
        match (a("CR"), a("SR"), a("SA"), a("RA"), a("DR")) {
            (Some(cr), Some(sr), Some(sa), Some(ra), Some(dr)) => Ok((cr, sr, sa, ra, dr)),
            _ => Err(format!(
                "#error \"bus controller device {} is missing a CR/SR/SA/RA/DR register\"",
                device
            )),
        }
    }

    /// Kick a `read_reg`/`write_reg` transaction over the controller's declared
    /// registers and enable its completion IRQ; the caller then suspends (sets
    /// the next state and returns).  Mirrors the write side of the simulator's
    /// bus servicing.  `n` is the owning reaction id (recorded in `__bus_owner`).
    fn emit_bus_kick_metal(&self, device: usize, op: &str, args: &[SirExpr], n: usize) -> Vec<String> {
        let (cr, _sr, sa, ra, dr) = match self.bus_regs_metal(device) {
            Ok(t) => t,
            Err(e) => return vec![e],
        };
        let is_read = op == "read_reg";
        if !is_read && op != "write_reg" {
            return vec![format!("#error \"unsupported bus op '{}' (expected read_reg/write_reg)\"", op)];
        }
        // read_reg(addr, reg); write_reg(addr, reg, val).
        let addr_arg = args.first().map(|a| self.emit_expr(a)).unwrap_or_else(|| "0U".into());
        let reg_arg = args.get(1).map(|a| self.emit_expr(a)).unwrap_or_else(|| "0U".into());
        let val_arg = args.get(2).map(|a| self.emit_expr(a));

        let mut out = vec![format!("/* bus {} → kick + arm completion IRQ (§3.5/§5.2) */", op)];
        out.push(format!("*(volatile uint32_t *)0x{:08x}UL = (uint32_t)({}); /* SA */", sa, addr_arg));
        out.push(format!("*(volatile uint32_t *)0x{:08x}UL = (uint32_t)({}); /* RA */", ra, reg_arg));
        if let Some(val) = &val_arg {
            out.push(format!("*(volatile uint32_t *)0x{:08x}UL = (uint32_t)({}); /* DR (write) */", dr, val));
        }
        out.push("__DMB();".into());
        let cr_kick = if is_read { "__I2C_CR_START | __I2C_CR_DIR_RD" } else { "__I2C_CR_START" };
        out.push(format!("*(volatile uint32_t *)0x{:08x}UL = ({}); /* CR: kick */", cr, cr_kick));
        out.push("__DMB();".into());
        out.push(format!("__bus_owner = (int32_t){}; /* this reaction now owns the bus */", n));
        out.push("__bus_irq_enable();".into());
        out
    }

    /// Read a completed transaction's result at the resumed segment: success →
    /// `dst` from DR (reads) or 0 (writes); a `done`-with-error or a non-`done`
    /// wake sets the frame's fault flag.  Mirrors the sim's bus outcome.
    fn emit_bus_resume_metal(&self, device: usize, op: &str, dst: &str, frame: &str) -> Vec<String> {
        let (_cr, sr, _sa, _ra, dr) = match self.bus_regs_metal(device) {
            Ok(t) => t,
            Err(e) => return vec![e],
        };
        let is_read = op == "read_reg";
        let mut out = vec!["{ /* bus completion: read result (§5.2) */".into()];
        out.push(format!("    uint32_t __sr = *(volatile uint32_t *)0x{:08x}UL; /* SR */", sr));
        if is_read {
            out.push("    if ((__sr & __I2C_SR_DONE) && !(__sr & __I2C_SR_ERR)) {".into());
            out.push(format!("        {} = *(volatile uint32_t *)0x{:08x}UL; /* DR (read result) */", dst, dr));
            out.push(format!("    }} else {{ {}.__faulted = 1U; }}", frame));
        } else {
            out.push(format!(
                "    if ((__sr & __I2C_SR_DONE) && !(__sr & __I2C_SR_ERR)) {{ {} = 0U; }} else {{ {}.__faulted = 1U; }}",
                dst, frame
            ));
        }
        out.push("}".into());
        out
    }

    /// Emit the terminal control flow for a Layer-2 disposition inside the
    /// `if (__faulted)` block of a propagated bus fault (§4.4/§5.4), mirroring
    /// the simulator's `dispose`.
    fn emit_disposition_terminal(&mut self, disp: SirDisposition) {
        match disp {
            SirDisposition::Retry { max } => {
                self.line(&format!("if (__retry < {}U) continue; /* retry */", max));
                self.line("return; /* retries exhausted → escalate */");
            }
            SirDisposition::Skip => self.line("return; /* skip: drop this activation */"),
            SirDisposition::Safe => {
                // Mask interrupts *before* driving safe-state so no other ISR /
                // reaction can run concurrently with the safe writes or after —
                // the sim's `drive_safe` halts the whole machine (`stop = true`).
                // The `memory` clobber keeps the mask from being reordered past
                // the device writes.
                self.line("__asm__ volatile(\"cpsid i\" ::: \"memory\"); /* no further reactions */");
                self.line("__drive_safe();");
                self.line("for (;;) { __asm__ volatile(\"wfi\"); } /* hold in safe state */");
            }
            SirDisposition::Escalate => self.line("return; /* escalate → Layer-3 */"),
        }
    }

    /// Emit `__drive_safe()` (§5.6): drive every device with a declared safe op
    /// to its safe state by running its bounded, non-yielding register writes.
    /// Reuses the ordered-MMIO store lowering.
    fn emit_drive_safe(&mut self, module: &SirModule) {
        // Emit the function whenever it can be called — a `safe` disposition
        // references it even if no device declares a safe state (in which case
        // the body is empty, matching the sim's no-op `drive_safe`).
        let has_safe_disp = module
            .reactions
            .iter()
            .any(|r| matches!(r.disposition, SirDisposition::Safe));
        // An overflow trap (§4.3) also drives the safe state, so the function
        // must exist whenever any arithmetic can trap.
        let has_arith_trap = self
            .arith_combos
            .iter()
            .any(|(_, m, _, _)| *m == OverflowMode::Trap);
        // A `fault` overflow policy (§5.1/D02) also drives the safe state on a
        // re-fire-while-in-flight, so the function must exist for it too.
        let has_overflow_fault = module
            .reactions
            .iter()
            .any(|r| r.overflow == SirOverflow::Fault);
        if module.safe_seqs.is_empty() && !has_safe_disp && !has_arith_trap && !has_overflow_fault {
            return;
        }
        self.line("/* Safe-state drive (§5.6): bounded, non-yielding register writes. */");
        self.line("static void __drive_safe(void) {");
        self.indent += 1;
        for seq in &module.safe_seqs {
            self.line(&format!("/* device {} → safe state '{}' */", seq.device, seq.state));
            for stmt in &seq.body {
                self.emit_stmt(stmt);
            }
        }
        self.indent -= 1;
        self.line("}");
        self.emit_newline();
    }

    fn emit_stmt(&mut self, stmt: &SirStmt) {
        match stmt {
            SirStmt::Intrinsic(intr) => self.emit_intrinsic(intr),
            SirStmt::Poll { cond, fault_code, within_ns } => match self.target {
                Target::MetalNrf52840 => {
                    // Bounded busy-wait (§3.2): spin until `cond` holds; on the
                    // bound elapsing, set `__faulted` and route to the reaction's
                    // Layer-2 disposition.  Does not yield.
                    let c = self.emit_expr(cond);
                    let bound = (*within_ns).max(1);
                    self.line(&format!("{{ /* poll `{}` within {}ns: bounded busy-wait (§3.2) */", fault_code, within_ns));
                    self.indent += 1;
                    self.line("uint32_t __spins = 0U;");
                    self.line(&format!("while (!({})) {{", c));
                    self.indent += 1;
                    self.line(&format!("if (++__spins > {}UL) {{ __faulted = 1U; break; }}", bound));
                    self.indent -= 1;
                    self.line("}");
                    self.indent -= 1;
                    self.line("}");
                    if let Some(disp) = self.current_disposition {
                        self.line("if (__faulted) {");
                        self.indent += 1;
                        self.emit_disposition_terminal(disp);
                        self.indent -= 1;
                        self.line("}");
                    }
                }
                Target::Host => self.line(&format!("/* host: sim services poll `{}` */", fault_code)),
            },
            SirStmt::Await { cond, fault_code, within_ns, recheck_ns } => match self.target {
                Target::MetalNrf52840 => {
                    // §5.2 — a suspending wait.  On metal this is currently lowered
                    // as a bounded re-check loop respecting `within` (the condition
                    // can be set by an ISR between checks); a full D2-style
                    // suspend/resume of the handler frame is a follow-up.
                    let c = self.emit_expr(cond);
                    let bound = (*within_ns / (*recheck_ns).max(1)).max(1);
                    self.line(&format!("{{ /* await `{}` within {}ns: bounded re-check (§5.2) */", fault_code, within_ns));
                    self.indent += 1;
                    self.line("uint32_t __waits = 0U;");
                    self.line(&format!("while (!({})) {{", c));
                    self.indent += 1;
                    self.line(&format!("if (++__waits > {}UL) {{ __faulted = 1U; break; }}", bound));
                    self.line("__asm__ volatile(\"wfi\"); /* yield to ISRs between re-checks */");
                    self.indent -= 1;
                    self.line("}");
                    self.indent -= 1;
                    self.line("}");
                    if let Some(disp) = self.current_disposition {
                        self.line("if (__faulted) {");
                        self.indent += 1;
                        self.emit_disposition_terminal(disp);
                        self.indent -= 1;
                        self.line("}");
                    }
                }
                Target::Host => self.line(&format!("/* host: sim services await `{}` */", fault_code)),
            },
            SirStmt::Assign { target, value } => match target {
                SirPlace::Var(name) => {
                    let val = self.emit_expr(value);
                    self.line(&format!("{} = {};", self.var_ref(name), val));
                }
                place @ SirPlace::Reg { device, reg_offset, .. } => {
                    match self.target {
                        Target::MetalNrf52840 => {
                            let stmt = self.emit_mmio_store(place, value);
                            for l in stmt {
                                self.line(&l);
                            }
                        }
                        Target::Host => {
                            // On the host the register has no MMIO meaning; the
                            // simulator (`--sim`) services this same SIR node.
                            self.line(&format!(
                                "/* host: no MMIO; sim services device {} reg 0x{:x} */",
                                device, reg_offset
                            ));
                        }
                    }
                }
            },
            SirStmt::RegWrite { device, reg_offset, width, writes } => match self.target {
                Target::MetalNrf52840 => {
                    for l in self.emit_mmio_store_multi(*device, *reg_offset, *width, writes) {
                        self.line(&l);
                    }
                }
                Target::Host => {
                    self.line(&format!(
                        "/* host: no MMIO; sim services device {} reg 0x{:x} (multi-field) */",
                        device, reg_offset
                    ));
                }
            },
            SirStmt::RingPush { ring, value } => {
                let v = self.emit_expr(value);
                let cap = self.ring_info.get(ring).map(|(_, c)| *c).unwrap_or(1).max(1);
                // Bounded ring (§5.3): on a full ring overwrite the oldest.
                self.line(&format!("{{ /* ring {}.push */", ring));
                self.indent += 1;
                self.line(&format!("if (__ring_{r}_count >= {cap}U) {{ __ring_{r}_head = (__ring_{r}_head + 1U) % {cap}U; __ring_{r}_count--; }}", r = ring, cap = cap));
                self.line(&format!("__ring_{r}_buf[__ring_{r}_tail] = ({v});", r = ring, v = v));
                self.line(&format!("__ring_{r}_tail = (__ring_{r}_tail + 1U) % {cap}U; __ring_{r}_count++;", r = ring, cap = cap));
                self.indent -= 1;
                self.line("}");
            }
            SirStmt::RingPop { ring, dst } => {
                let cap = self.ring_info.get(ring).map(|(_, c)| *c).unwrap_or(1).max(1);
                let d = self.var_ref(dst);
                self.line(&format!("{{ /* ring {}.pop */", ring));
                self.indent += 1;
                self.line(&format!("if (__ring_{r}_count > 0U) {{ {d} = __ring_{r}_buf[__ring_{r}_head]; __ring_{r}_head = (__ring_{r}_head + 1U) % {cap}U; __ring_{r}_count--; }} else {{ {d} = 0; }}", r = ring, d = d, cap = cap));
                self.indent -= 1;
                self.line("}");
            }
            SirStmt::If { cond, then } => {
                let c = self.emit_expr(cond);
                self.line(&format!("if ({}) {{", c));
                self.indent += 1;
                for s in then {
                    self.emit_stmt(s);
                }
                self.indent -= 1;
                self.line("}");
            }
            SirStmt::Critical { ceiling, body } => match self.target {
                Target::MetalNrf52840 => {
                    // Priority-ceiling protocol (§5.5): raise BASEPRI to the
                    // ceiling so no cell-sharing reaction can preempt the access,
                    // then restore.  Masks exactly the racing interrupts.
                    let bp = self.basepri_byte(*ceiling);
                    self.line(&format!("{{ /* critical-section: raise to ceiling {} (BASEPRI 0x{:02x}) */", ceiling, bp));
                    self.indent += 1;
                    self.line("uint32_t __bp_saved = __get_BASEPRI();");
                    self.line(&format!("__set_BASEPRI(0x{:02x}U);", bp));
                    self.line("__DMB();");
                    for s in body {
                        self.emit_stmt(s);
                    }
                    self.line("__DMB();");
                    self.line("__set_BASEPRI(__bp_saved);");
                    self.indent -= 1;
                    self.line("}");
                }
                Target::Host => {
                    // No interrupts to mask on the host; the simulator services
                    // the section. Keep it visible as a comment.
                    self.line(&format!("/* critical-section enter (ceiling {}) */", ceiling));
                    for s in body {
                        self.emit_stmt(s);
                    }
                    self.line("/* critical-section exit */");
                }
            },
            SirStmt::DeviceOp { device, op, .. } => match self.target {
                // The resolver currently inlines composed-device ops down to
                // register accesses / `BusXfer`, so this node is not emitted on
                // the keystone path.  If a non-inlined dispatch path ever does
                // emit it, refuse loudly rather than silently no-op on metal.
                Target::MetalNrf52840 => {
                    self.line(&format!("#error \"device op {} on device {} not yet lowered on metal\"", op, device));
                }
                Target::Host => {
                    self.line(&format!("/* host: sim services device {} op {} */", device, op));
                }
            },
            SirStmt::BusXfer { device, op, dst, .. } => {
                // On metal, a yielding reaction is lowered as a whole by
                // `emit_yielding_reaction_metal` (which handles the transaction
                // + disposition), so this arm is only reached on the host, where
                // the simulator (`--sim`) services the transfer (§7.1).
                self.line(&format!("/* host: sim services bus xfer device {} op {} -> {} */", device, op, dst));
            }
            SirStmt::Exit(code) => {
                let c = self.emit_expr(code);
                self.line(&format!("exit((int)({}));", c));
            }
        }
    }

    fn emit_intrinsic(&mut self, intr: &SirIntrinsic) {
        match intr {
            SirIntrinsic::HostIoPrintStr(s) => {
                // Emit a string as a static byte array, then call __host_io_print.
                // Escape the string for C.
                let escaped = c_escape_bytes(s.as_bytes());
                let len = s.len();
                let line = format!(
                    "__host_io_print((const uint8_t *)\"{}\" , {}U);",
                    escaped, len
                );
                self.line(&line);
            }
            SirIntrinsic::HostIoPrint(expr) => {
                // Generic: requires a (ptr, len) pair.  In Phase 0 only Bytes
                // literals reach here.
                match expr {
                    SirExpr::Bytes(bytes) => {
                        let escaped = c_escape_bytes(bytes);
                        let len = bytes.len();
                        let line = format!(
                            "__host_io_print((const uint8_t *)\"{}\" , {}U);",
                            escaped, len
                        );
                        self.line(&line);
                    }
                    _ => {
                        // Variable-length bytes — emit a helper call.
                        let e = self.emit_expr(expr);
                        self.line(&format!("/* TODO: dynamic bytes print: {} */", e));
                    }
                }
            }
            SirIntrinsic::HostIoFlush => {
                self.line("fflush(stdout);");
            }
        }
    }

    // ── main() ────────────────────────────────────────────────────────────────

    fn emit_main(&mut self, module: &SirModule) {
        self.line("int main(void) {");
        self.indent += 1;

        // 1. Fire all SysStart reactions first (they run before the event loop).
        for reaction in &module.reactions {
            if matches!(reaction.trigger, SirTrigger::SysStart) {
                let call = format!("{}();", reaction_fn_name(reaction.id));
                self.line(&call);
            }
        }

        // 2. If there are no periodic reactions, we're done.
        let periodic: Vec<(usize, u64)> = module.reactions.iter()
            .filter_map(|r| if let SirTrigger::EveryNs(ns) = r.trigger {
                Some((r.id, ns))
            } else {
                None
            })
            .collect();

        if !periodic.is_empty() {
            self.emit_newline();

            // Declare one deadline variable per periodic reaction at function
            // scope so they survive across loop iterations.  Initialise each to
            // now + period (first fire time).
            //
            // Fixed-rate scheduling (§4.5/D15): the next deadline is always
            // advanced by the period from the *scheduled* time, not from when
            // the handler returns, so handler execution time does not accumulate
            // as drift.  Overruns are coalesced (§4.5/D15): if a handler misses
            // one or more ticks the scheduler drops them and advances to the
            // next future deadline rather than queueing catch-up firings.
            self.line("/* fixed-rate deadline tracking — §4.5/D15 */");
            for (id, ns) in &periodic {
                self.line(&format!(
                    "uint64_t __deadline_{} = __get_mono_ns() + {}ULL;",
                    id, ns
                ));
            }

            self.emit_newline();
            self.line("/* event loop */");
            self.line("for (;;) {");
            self.indent += 1;

            // Sleep until the nearest upcoming deadline.
            self.line("uint64_t __now = __get_mono_ns();");
            self.line("uint64_t __sleep_ns = 0ULL;");
            for (id, _) in &periodic {
                self.line(&format!("if (__deadline_{} > __now) {{", id));
                self.indent += 1;
                self.line(&format!("uint64_t __d = __deadline_{} - __now;", id));
                self.line("if (__sleep_ns == 0ULL || __d < __sleep_ns) __sleep_ns = __d;");
                self.indent -= 1;
                self.line("}");
            }
            self.line("if (__sleep_ns > 0ULL) __host_sleep_ns(__sleep_ns);");

            self.emit_newline();
            // Fire all due reactions; advance each deadline (coalescing overruns).
            self.line("__now = __get_mono_ns();");
            for (id, ns) in &periodic {
                self.line(&format!("if (__now >= __deadline_{}) {{", id));
                self.indent += 1;
                self.line(&format!("{}();", reaction_fn_name(*id)));
                // Advance by period until the deadline is in the future.
                // This drops any missed ticks (coalesce — §4.5/D15).
                self.line(&format!(
                    "do {{ __deadline_{} += {}ULL; }} while (__deadline_{} <= __now);",
                    id, ns, id
                ));
                self.indent -= 1;
                self.line("}");
            }

            self.indent -= 1;
            self.line("}");
        } else {
            self.line("return 0;");
        }

        self.indent -= 1;
        self.line("}");
    }

    // ── Metal (bare-metal nRF52840) ───────────────────────────────────────────

    /// Lower a `SirPlace::Reg` store to ordered volatile MMIO (§4.2/§6.2, gate
    /// #3).  A read/write register is a barrier-bracketed read-modify-write; a
    /// write-1-to-set/clear register writes the field directly (no RMW, so a
    /// status bit in the same register is never clobbered).
    fn emit_mmio_store(&self, place: &SirPlace, value: &SirExpr) -> Vec<String> {
        let SirPlace::Reg { device, reg_offset: offset, width, field_mask: mask, field_shift: shift, access } = place
        else {
            return vec!["#error \"internal: emit_mmio_store on a non-register place\"".into()];
        };
        let (device, offset, width, mask, shift, access) = (*device, *offset, *width, *mask, *shift, *access);
        // A missing base would silently write to 0x0; refuse instead.
        let Some(&base) = self.device_bases.get(&device) else {
            return vec![format!(
                "#error \"device {} has no MMIO base address (instance needs `at <addr>`)\"",
                device
            )];
        };
        let cty = match width {
            8 => "uint8_t",
            16 => "uint16_t",
            32 => "uint32_t",
            _ => return vec![format!("#error \"unsupported register width {} (expected 8/16/32)\"", width)],
        };
        let addr = base + offset;
        let v = self.emit_expr(value);
        // Field math is computed in uint32_t then narrowed to the register width.
        let field = format!("(((uint32_t)({}) << {}) & 0x{:x}UL)", v, shift, mask);
        let mut out = vec![format!("{{ /* MMIO store: device {} reg 0x{:x} ({}-bit, §4.2 ordered) */", device, offset, width)];
        out.push(format!("    volatile {ct} *__p = (volatile {ct} *)0x{:08x}UL;", addr, ct = cty));
        match access {
            // write-1-to-clear / write-only: write just the field, no RMW.
            SirRegAccess::W1c | SirRegAccess::Wo => {
                out.push("    __DMB();".into());
                out.push(format!("    *__p = ({ct})({field});", ct = cty, field = field));
                out.push("    __DMB();".into());
            }
            // read/write: read-modify-write the field, bracketed by barriers.
            _ => {
                out.push(format!("    {ct} __v = *__p;", ct = cty));
                out.push(format!("    __v = ({ct})(((uint32_t)__v & ~0x{:x}UL) | {field});", mask, ct = cty, field = field));
                out.push("    __DMB();".into());
                out.push("    *__p = __v;".into());
                out.push("    __DMB();".into());
            }
        }
        out.push("}".into());
        out
    }

    /// Lower a `SirStmt::RegWrite` (multi-field) to ONE ordered volatile store
    /// (§4.2/§6.2, audit #35 P0-2c).  All fields are OR-combined; if none needs a
    /// read (every field is w1c/wo) it is a single masked write, else a single
    /// read-modify-write over the union mask — never one RMW per field.
    fn emit_mmio_store_multi(
        &self,
        device: usize,
        offset: u64,
        width: u8,
        writes: &[(u64, u8, SirRegAccess, SirExpr)],
    ) -> Vec<String> {
        let Some(&base) = self.device_bases.get(&device) else {
            return vec![format!(
                "#error \"device {} has no MMIO base address (instance needs `at <addr>`)\"",
                device
            )];
        };
        let cty = match width {
            8 => "uint8_t",
            16 => "uint16_t",
            32 => "uint32_t",
            _ => return vec![format!("#error \"unsupported register width {} (expected 8/16/32)\"", width)],
        };
        let addr = base + offset;
        // OR the fields into one value; union of the touched masks.
        let mut union_mask = 0u64;
        let mut terms = Vec::new();
        for (mask, shift, _access, value) in writes {
            union_mask |= *mask;
            let v = self.emit_expr(value);
            terms.push(format!("(((uint32_t)({}) << {}) & 0x{:x}UL)", v, shift, mask));
        }
        let combined = terms.join(" | ");
        // No read needed only when every field is a single-write kind (w1c/wo).
        let single_write = writes
            .iter()
            .all(|(_, _, a, _)| matches!(a, SirRegAccess::W1c | SirRegAccess::Wo));

        let mut out = vec![format!(
            "{{ /* MMIO multi-field store: device {} reg 0x{:x} ({}-bit, {} fields, §4.2 ordered) */",
            device, offset, width, writes.len()
        )];
        out.push(format!("    volatile {ct} *__p = (volatile {ct} *)0x{:08x}UL;", addr, ct = cty));
        if single_write {
            out.push("    __DMB();".into());
            out.push(format!("    *__p = ({ct})({combined});", ct = cty, combined = combined));
            out.push("    __DMB();".into());
        } else {
            out.push(format!("    {ct} __v = *__p;", ct = cty));
            out.push(format!(
                "    __v = ({ct})(((uint32_t)__v & ~0x{:x}UL) | ({combined}));",
                union_mask, ct = cty, combined = combined
            ));
            out.push("    __DMB();".into());
            out.push("    *__p = __v;".into());
            out.push("    __DMB();".into());
        }
        out.push("}".into());
        out
    }

    /// Emit a `SirExpr` as a C expression string.  Target-aware so that a
    /// `RegLoad` can resolve the owning device's MMIO base; recurses through
    /// `self` so a nested `RegLoad` (e.g. `reg + 1`) is lowered the same way.
    fn emit_expr(&self, expr: &SirExpr) -> String {
        match expr {
            SirExpr::Bool(b) => if *b { "1U".into() } else { "0U".into() },
            SirExpr::U64(n) => format!("{}ULL", n),
            SirExpr::Bytes(bytes) => {
                // Bytes as a cast-to-const-pointer string literal.
                let escaped = c_escape_bytes(bytes);
                format!("(const uint8_t *)\"{}\"", escaped)
            }
            SirExpr::Load(name) => self.var_ref(name),
            SirExpr::RegLoad { device, reg_offset, width, field_mask, field_shift, access } => {
                self.emit_reg_load(*device, *reg_offset, *width, *field_mask, *field_shift, *access)
            }
            SirExpr::Not(inner) => format!("(!({}))", self.emit_expr(inner)),
            SirExpr::BinOp(op, lhs, rhs) => {
                let l = self.emit_expr(lhs);
                let r = self.emit_expr(rhs);
                let op_str = match op {
                    SirBinOp::Add => "+",
                    SirBinOp::Sub => "-",
                    SirBinOp::Mul => "*",
                    SirBinOp::Div => "/",
                    SirBinOp::Rem => "%",
                    SirBinOp::And => "&&",
                    SirBinOp::Or => "||",
                    SirBinOp::EqEq => "==",
                    SirBinOp::NotEq => "!=",
                    SirBinOp::Lt => "<",
                    SirBinOp::Le => "<=",
                    SirBinOp::Gt => ">",
                    SirBinOp::Ge => ">=",
                };
                format!("({} {} {})", l, op_str, r)
            }
            SirExpr::Arith { op, mode, width, signed, lhs, rhs } => {
                let l = self.emit_expr(lhs);
                let r = self.emit_expr(rhs);
                format!("{}({}, {})", arith_helper_name(*op, *mode, *width, *signed), l, r)
            }
            // `now()` — current time in ns (§4.5), from the monotonic source.
            SirExpr::Now => "__now_ns()".to_string(),
            // Explicit cast (§4.3): a C cast to the target fixed-width type does
            // the narrowing/widening/sign reinterpretation.
            SirExpr::Cast { inner, to_width, signed } => {
                let cty = format!("{}int{}_t", if *signed { "" } else { "u" }, to_width);
                format!("(({}){})", cty, self.emit_expr(inner))
            }
            // Fixed-point multiply/divide with rescale (§4.3, P0-3c) → helper.
            SirExpr::FixedArith { op, mode, frac_bits, width, signed, lhs, rhs } => {
                let name = fixed_helper_name(*op, *mode, *width, *signed, *frac_bits);
                format!("{}({}, {})", name, self.emit_expr(lhs), self.emit_expr(rhs))
            }
            // Fixed-point rescale (§4.3, P0-3a): shift the binary point in a
            // 64-bit (sign-aware) intermediate, then narrow to the target type.
            SirExpr::FixedCast { inner, shift, to_width, signed } => {
                let cty = format!("{}int{}_t", if *signed { "" } else { "u" }, to_width);
                let wide = if *signed { "int64_t" } else { "uint64_t" };
                let v = self.emit_expr(inner);
                let scaled = if *shift >= 0 {
                    format!("(({}){} << {})", wide, v, shift)
                } else {
                    format!("(({}){} >> {})", wide, v, -(*shift as i32))
                };
                format!("(({}){})", cty, scaled)
            }
            // Bounded-ring reads (§5.3).
            SirExpr::RingLen(r) => format!("__ring_{}_count", r),
            SirExpr::RingEmpty(r) => format!("(__ring_{}_count == 0U)", r),
            SirExpr::RingFull(r) => {
                let cap = self.ring_info.get(r).map(|(_, c)| *c).unwrap_or(1).max(1);
                format!("(__ring_{}_count >= {}U)", r, cap)
            }
        }
    }

    /// Lower a `SirExpr::RegLoad` to a C expression: the read counterpart of
    /// `emit_mmio_store` (§4.2/§6.2).  On metal this is a volatile MMIO read
    /// masked and shifted to the field, matching the simulator exactly
    /// (`(reg & field_mask) >> field_shift`, `sim/mod.rs`).  A pure read is not
    /// a read-modify-write, so a `w1c` status bit in the same register is never
    /// disturbed; it needs no barrier in expression position.
    fn emit_reg_load(
        &self,
        device: usize,
        offset: u64,
        width: u8,
        mask: u64,
        shift: u8,
        access: SirRegAccess,
    ) -> String {
        match self.target {
            Target::Host => {
                // On the host the register has no MMIO meaning; the simulator
                // (`--sim`) services this same SIR node.  Mirror the host Store
                // arm: read as 0 with a visible note.
                format!("0U /* host: no MMIO; sim services device {} reg 0x{:x} */", device, offset)
            }
            Target::MetalNrf52840 => {
                let Some(&base) = self.device_bases.get(&device) else {
                    return format!(
                        "0U /* #error device {} has no MMIO base address (instance needs `at <addr>`) */",
                        device
                    );
                };
                let cty = match width {
                    8 => "uint8_t",
                    16 => "uint16_t",
                    32 => "uint32_t",
                    _ => return format!("0U /* #error unsupported register width {} (expected 8/16/32) */", width),
                };
                let addr = base + offset;
                // `Rc` (read-clears) registers have a read side effect; this
                // expression reads exactly once, matching the sim.
                let note = if matches!(access, SirRegAccess::Rc) { " /* Rc: read-clears */" } else { "" };
                format!(
                    "((((uint32_t)(*(volatile {ct} *)0x{addr:08x}UL)) & 0x{mask:x}UL) >> {shift}){note}",
                    ct = cty, addr = addr, mask = mask, shift = shift, note = note
                )
            }
        }
    }

    /// Emit the Layer-3 fault decoder (§5.4): the address-ownership table (no
    /// on-device strings — §4.3; the host renders labels from indices) plus a
    /// `HardFault_Handler` that reads BFAR, finds the owning region, and records
    /// `{addr, owner-index, pending}` to fixed RAM the harness reads back.
    fn emit_fault_decoder(&mut self, module: &SirModule) {
        let owners = layer3::ownership_map(module);
        self.emit_newline();
        self.line("/* Layer-3 fault decoder: address-ownership map (§5.4) */");
        if owners.is_empty() {
            self.line("#define __OWNER_COUNT 0U");
            self.line("static const uint32_t __owner_start[1] = {0U};");
            self.line("static const uint32_t __owner_end[1]   = {0U};");
        } else {
            self.line(&format!("#define __OWNER_COUNT {}U", owners.len()));
            let starts: Vec<String> = owners.iter().map(|r| format!("0x{:08x}U", r.start)).collect();
            let ends: Vec<String> = owners.iter().map(|r| format!("0x{:08x}U", r.end)).collect();
            self.line(&format!("static const uint32_t __owner_start[__OWNER_COUNT] = {{ {} }};", starts.join(", ")));
            self.line(&format!("static const uint32_t __owner_end[__OWNER_COUNT]   = {{ {} }};", ends.join(", ")));
            // Index → label, as a comment, for the host-side decoder.
            for (i, r) in owners.iter().enumerate() {
                self.line(&format!("/* owner[{}] = {} [0x{:08x}, 0x{:08x}) */", i, r.label, r.start, r.end));
            }
        }
        self.line("/* fault record (read by the host decoder) — structured, no strings (§4.3) */");
        self.line("volatile uint32_t __fault_addr = 0U;");
        self.line("volatile uint32_t __fault_owner = 0xFFFFFFFFUL; /* index, or 0xFFFFFFFF = unclaimed/invalid */");
        self.line("volatile uint32_t __fault_cfsr = 0U;");
        self.line("volatile uint32_t __fault_pending = 0U;");
        self.line("void HardFault_Handler(void) {");
        self.indent += 1;
        self.line("uint32_t __cfsr = *(volatile uint32_t *)0xE000ED28UL; /* SCB CFSR */");
        self.line("uint32_t __a = 0U;");
        self.line("uint32_t __o = 0xFFFFFFFFUL;");
        // BFAR holds the faulting address only when CFSR.BFARVALID (bit 15) is set;
        // otherwise it is stale/undefined, so don't attribute an address.
        self.line("if (__cfsr & 0x00008000UL) { /* BFARVALID */");
        self.indent += 1;
        self.line("__a = *(volatile uint32_t *)0xE000ED38UL; /* SCB BFAR */");
        self.line("for (uint32_t __i = 0U; __i < __OWNER_COUNT; __i++) {");
        self.indent += 1;
        self.line("if (__a >= __owner_start[__i] && __a < __owner_end[__i]) { __o = __i; break; }");
        self.indent -= 1;
        self.line("}");
        self.indent -= 1;
        self.line("}");
        self.line("__fault_addr = __a; __fault_owner = __o; __fault_cfsr = __cfsr; __fault_pending = 1U;");
        self.line("for (;;) { /* halt; safe-state drive is a later phase (§5.6) */ }");
        self.indent -= 1;
        self.line("}");
    }

    fn emit_header_metal(&mut self, module: &SirModule) {
        self.line("/* Generated by silicac — do not edit.  Freestanding metal target (§6.2/§6.4). */");
        self.line("#include <stdint.h>");
        self.line("");
        self.line("/* Memory barriers (§4.2/§6.2): ordered MMIO + DMA/IRQ hand-off. */");
        self.line("#define __DSB() __asm__ volatile(\"dsb 0xf\" ::: \"memory\")");
        self.line("#define __DMB() __asm__ volatile(\"dmb 0xf\" ::: \"memory\")");
        self.line("");
        if module_has_bus_xfer(module) {
            self.line("/* I²C controller bit protocol (§3.5) — see std/i2c_controller.si.  The");
            self.line("   register *addresses* come from the device's declared regs; these are the");
            self.line("   bit conventions the bounded-poll `BusXfer` lowering encodes. */");
            self.line("#define __I2C_CR_START 0x1U          /* CR.start */");
            self.line("#define __I2C_CR_DIR_RD 0x2U         /* CR.dir = read */");
            self.line("#define __I2C_SR_DONE 0x1U           /* SR.done */");
            self.line("#define __I2C_SR_ERR 0xEU            /* SR.nak | arblost | timeout */");
            self.line("");
            self.line("/* Cooperative bus suspension (§5.2): the reaction that owns the in-flight");
            self.line("   transaction, and NVIC enable/disable for the controller's completion IRQ.");
            self.line("   A wedged bus never raises the IRQ → the owner stays in flight and the");
            self.line("   watchdog catches it (§5.6), matching the simulator's `Hang`. */");
            self.line("static volatile int32_t __bus_owner = -1;");
            self.line("#define __NVIC_ISER0 (*(volatile uint32_t *)0xE000E100UL)");
            self.line("#define __NVIC_ICER0 (*(volatile uint32_t *)0xE000E180UL)");
            self.line("#define __BUS_IRQN 8U                /* nRF52840 SPI0/TWI0 line; the mock controller raises it (E1) */");
            self.line("static inline void __bus_irq_enable(void)  { __NVIC_ISER0 = (1UL << __BUS_IRQN); }");
            self.line("static inline void __bus_irq_disable(void) { __NVIC_ICER0 = (1UL << __BUS_IRQN); }");
            self.line("");
        }
        self.line("/* BASEPRI access for priority-ceiling critical sections (§5.5). */");
        self.line("#define __set_BASEPRI(v) __asm__ volatile(\"msr basepri, %0\" :: \"r\"((uint32_t)(v)) : \"memory\")");
        self.line("static inline uint32_t __get_BASEPRI(void) { uint32_t __r; __asm__ volatile(\"mrs %0, basepri\" : \"=r\"(__r)); return __r; }");
        self.line("");
        self.line("/* Symbols provided by the generated linker script (§6.4). */");
        self.line("extern uint32_t _estack;");
        self.line("extern uint32_t _sidata, _sdata, _edata, _sbss, _ebss;");
    }

    /// Emit the vector table + reset/startup (§6.4): copy `.data`, zero `.bss`,
    /// configure pins, run `sys.start`, program SysTick (`every`) and GPIOTE/NVIC
    /// (`on <pin>.falling`), then idle in WFI.  `every` is dispatched from
    /// `SysTick_Handler` (§4.5); `on` events from `GPIOTE_IRQHandler` (§4.1).
    ///
    /// GPIOTE/NVIC register details are nRF-specific and live in this nRF52840
    /// target (SIR stays neutral); SysTick/NVIC are at architectural SCS
    /// addresses.  Modelling GPIOTE as a std device with full event routing is a
    /// documented refinement.
    fn emit_metal_startup(&mut self, module: &SirModule) {
        const GPIOTE_BASE: u64 = 0x4000_6000;
        const GPIOTE_IRQN: usize = 6;
        const BUS_IRQN: usize = 8; // nRF52840 SPI0/TWI0 line (matches __BUS_IRQN)

        let systick = match systick_plan(module) {
            Ok(p) => p,
            Err(e) => {
                self.line(&format!("#error \"{}\"", e));
                None
            }
        };

        // Collect `on <pin>.falling` bindings → one GPIOTE channel per event.
        // (channel, port_base, pin, hw_priority_byte, [reaction ids])
        let mut events: Vec<(usize, u64, u8, u8, Vec<usize>)> = Vec::new();
        for ev in &module.events {
            let rs: Vec<usize> = module
                .reactions
                .iter()
                .filter(|r| matches!(r.trigger, SirTrigger::Event(e) if e == ev.id))
                .map(|r| r.id)
                .collect();
            if rs.is_empty() {
                continue;
            }
            let prio = module
                .reactions
                .iter()
                .filter(|r| rs.contains(&r.id))
                .map(|r| r.priority)
                .max()
                .unwrap_or(self.max_priority);
            let base = self.device_bases.get(&ev.device).copied().unwrap_or(0);
            events.push((events.len(), base, ev.pin_index.unwrap_or(0), self.basepri_byte(prio), rs));
        }

        self.line("/* default handler for unused vectors */");
        self.line("static void __default_handler(void) { for (;;) {} }");
        self.line("void Reset_Handler(void);");

        // Layer-3 fault decoder: address-ownership table + HardFault handler.
        self.emit_fault_decoder(module);

        // SysTick handler: software-prescaled per-reaction counters (§4.5).
        if let Some(plan) = &systick {
            self.emit_newline();
            self.line("/* every -> SysTick dispatch (1ms base, per-reaction counters) */");
            for (id, threshold) in &plan.thresholds {
                self.line(&format!("static volatile uint32_t __systick_ctr_{} = {}U;", id, threshold));
            }
            self.line("void SysTick_Handler(void) {");
            self.indent += 1;
            if module_uses_now(module) {
                // 1ms base tick → advance the uptime clock backing now() (§4.5).
                self.line("__uptime_ns += UINT64_C(1000000);");
            }
            // §4.5/§5.6 — tick down each armed `within` deadline.  A reaction that
            // is back to idle disarms; one still in flight when its countdown hits
            // zero overran → latch `__deadline_missed` (stops the watchdog feed).
            if !self.deadline_ticks.is_empty() {
                let mut ids: Vec<usize> = self.deadline_ticks.keys().copied().collect();
                ids.sort_unstable();
                for id in ids {
                    self.line(&format!(
                        "if (__rf_{id}.__state == 0U) {{ __deadline_{id} = 0U; }} else if (__deadline_{id} != 0U && --__deadline_{id} == 0U) {{ __deadline_missed = 1U; }}",
                    ));
                }
            }
            for (id, threshold) in &plan.thresholds {
                self.line(&format!("if (--__systick_ctr_{} == 0U) {{", id));
                self.indent += 1;
                self.line(&format!("__systick_ctr_{} = {}U;", id, threshold));
                self.line(&format!("{}();", reaction_fn_name(*id)));
                self.indent -= 1;
                self.line("}");
            }
            self.indent -= 1;
            self.line("}");
        }

        // GPIOTE handler: clear the channel event, dispatch the bound reactions.
        if !events.is_empty() {
            self.emit_newline();
            self.line("/* on <pin>.falling -> GPIOTE IRQ dispatch (§4.1) */");
            self.line("void GPIOTE_IRQHandler(void) {");
            self.indent += 1;
            for (ch, _base, _pin, _prio, rs) in &events {
                let events_in = GPIOTE_BASE + 0x100 + 4 * (*ch as u64);
                self.line(&format!("if (*(volatile uint32_t *)0x{:08x}UL != 0U) {{", events_in));
                self.indent += 1;
                self.line(&format!("*(volatile uint32_t *)0x{:08x}UL = 0U; /* clear EVENTS_IN[{}] */", events_in, ch));
                for id in rs {
                    self.line(&format!("{}();", reaction_fn_name(*id)));
                }
                self.indent -= 1;
                self.line("}");
            }
            self.indent -= 1;
            self.line("}");
        }

        // Bus completion IRQ (§5.2): resume the in-flight reaction's dispatcher.
        // Only one transaction is in flight at a time, tracked by `__bus_owner`.
        let bus_reactions: Vec<usize> = module.reactions.iter().filter(|r| r.yields).map(|r| r.id).collect();
        if module_has_bus_xfer(module) && !bus_reactions.is_empty() {
            self.emit_newline();
            self.line("/* bus completion IRQ (§5.2): resume the in-flight reaction (§5.1 single owner) */");
            self.line("void __BUS_IRQHandler(void) {");
            self.indent += 1;
            self.line("__bus_irq_disable();");
            self.line("int32_t __o = __bus_owner;");
            self.line("__bus_owner = -1;");
            self.line("switch (__o) {");
            self.indent += 1;
            for id in &bus_reactions {
                self.line(&format!("case {id}: __react_{id}_run(); break;", id = id));
            }
            self.line("default: break;");
            self.indent -= 1;
            self.line("}");
            self.indent -= 1;
            self.line("}");
        }
        self.emit_newline();

        // Vector table: system exceptions + external IRQs up to the highest used.
        self.line("/* Cortex-M vector table — placed at flash base by the linker (§6.4). */");
        self.line("__attribute__((section(\".vectors\"), used))");
        self.line("const void *const __vectors[] = {");
        self.indent += 1;
        let systick_entry = if systick.is_some() { "SysTick_Handler" } else { "__default_handler" };
        let mut entries: Vec<(String, String)> = vec![
            ("&_estack".into(), "0  initial SP".into()),
            ("Reset_Handler".into(), "1  reset".into()),
            ("__default_handler".into(), "2  NMI".into()),
            ("HardFault_Handler".into(), "3  HardFault".into()),
        ];
        for i in 4..=10 {
            entries.push(("0".into(), format!("{}  reserved", i)));
        }
        entries.push(("__default_handler".into(), "11 SVCall".into()));
        entries.push(("0".into(), "12".into()));
        entries.push(("0".into(), "13".into()));
        entries.push(("__default_handler".into(), "14 PendSV".into()));
        entries.push((systick_entry.into(), "15 SysTick".into()));
        // External IRQs (index 16 + n).  Extend to the highest line used by
        // GPIOTE (`on` events) and/or the bus completion IRQ (yielding reactions).
        let has_bus = module_has_bus_xfer(module) && !bus_reactions.is_empty();
        let max_irq = [
            (!events.is_empty()).then_some(GPIOTE_IRQN),
            has_bus.then_some(BUS_IRQN),
        ]
        .into_iter()
        .flatten()
        .max();
        if let Some(maxq) = max_irq {
            for irq in 0..=maxq {
                let (sym, note) = if irq == GPIOTE_IRQN && !events.is_empty() {
                    ("GPIOTE_IRQHandler".to_string(), format!("{} GPIOTE", 16 + irq))
                } else if irq == BUS_IRQN && has_bus {
                    ("__BUS_IRQHandler".to_string(), format!("{} bus completion", 16 + irq))
                } else {
                    ("__default_handler".to_string(), format!("{} IRQ{}", 16 + irq, irq))
                };
                entries.push((sym, note));
            }
        }
        for (sym, comment) in &entries {
            let cast = if sym == "0" { "0".to_string() } else { format!("(void *)&{}", strip_amp(sym)) };
            self.line(&format!("{:<28} /* {} */", format!("{},", cast), comment));
        }
        self.indent -= 1;
        self.line("};");
        self.emit_newline();

        self.line("void Reset_Handler(void) {");
        self.indent += 1;
        self.line("/* copy .data (flash LMA -> RAM VMA) */");
        self.line("uint32_t *src = &_sidata, *dst = &_sdata;");
        self.line("while (dst < &_edata) { *dst++ = *src++; }");
        self.line("/* zero .bss */");
        self.line("for (dst = &_sbss; dst < &_ebss; ) { *dst++ = 0U; }");
        self.emit_newline();

        // Device init: output-pin directions (§6.4).
        let outputs: Vec<&SirPin> = module.pins.iter().filter(|p| p.output).collect();
        if !outputs.is_empty() {
            self.line("/* configure output pin directions */");
            for pin in outputs {
                let place = SirPlace::Reg {
                    device: pin.device,
                    reg_offset: pin.dir_reg_offset,
                    width: pin.dir_reg_width,
                    field_mask: 1u64 << pin.index,
                    field_shift: pin.index,
                    access: SirRegAccess::Rw,
                };
                let value = SirExpr::Bool(true);
                for l in self.emit_mmio_store(&place, &value) {
                    self.line(&l);
                }
            }
            self.emit_newline();
        }

        // Input pins with a pull resistor → PIN_CNF (nRF: 0x700 + 4*pin).
        let pulls: Vec<&SirPin> = module.pins.iter().filter(|p| !p.output && p.pull_up).collect();
        if !pulls.is_empty() {
            self.line("/* configure input pins: connect buffer + pull-up (PIN_CNF) */");
            for pin in pulls {
                let base = self.device_bases.get(&pin.device).copied().unwrap_or(0);
                let addr = base + 0x700 + 4 * pin.index as u64;
                self.line(&format!(
                    "*(volatile uint32_t *)0x{:08x}UL = 0xCUL; /* PIN_CNF[{}]: input, pull-up */",
                    addr, pin.index
                ));
            }
            self.emit_newline();
        }

        let sys_start: Vec<usize> = module
            .reactions
            .iter()
            .filter(|r| matches!(r.trigger, SirTrigger::SysStart))
            .map(|r| r.id)
            .collect();
        if !sys_start.is_empty() {
            self.line("/* run sys.start reactions, in declaration order */");
            for id in sys_start {
                self.line(&format!("{}();", reaction_fn_name(id)));
            }
            self.emit_newline();
        }

        // Program SysTick (architectural system timer at the SCS, §4.5).
        if let Some(plan) = &systick {
            self.line("/* SysTick: 1ms base tick (§4.5) */");
            self.line(&format!("*(volatile uint32_t *)0xE000E014UL = {}UL; /* SYST_RVR */", plan.reload));
            self.line("*(volatile uint32_t *)0xE000E018UL = 0UL;        /* SYST_CVR */");
            self.line("__DSB();");
            self.line("*(volatile uint32_t *)0xE000E010UL = 0x7UL;      /* SYST_CSR: ENABLE|TICKINT|CLKSOURCE */");
            // SysTick exception priority (SHPR3 byte [31:24]) for the ceiling (§5.5).
            let sys_prio = systick_priority(module).map(|p| self.basepri_byte(p)).unwrap_or(0);
            self.line(&format!("*(volatile uint8_t *)0xE000ED23UL = 0x{:02x}U; /* SysTick priority */", sys_prio));
            self.emit_newline();
        }

        // Configure GPIOTE channels + NVIC for `on` events (§4.1, §5.5).
        if !events.is_empty() {
            self.line("/* GPIOTE channels (falling edge) + NVIC enable for `on` events */");
            for (ch, base, pin, prio, _rs) in &events {
                let cfg = GPIOTE_BASE + 0x510 + 4 * (*ch as u64);
                let intenset = GPIOTE_BASE + 0x304;
                // CONFIG: MODE=event(1) | PSEL(pin) | POLARITY=HiToLo(2).  Port 0
                // for the slice's single GPIO instance (multi-port is a refinement).
                let port = if *base == 0x5000_0300 { 1u64 } else { 0u64 };
                let config = 1u64 | ((*pin as u64) << 8) | (port << 13) | (2u64 << 16);
                self.line(&format!("*(volatile uint32_t *)0x{:08x}UL = 0x{:x}UL; /* GPIOTE CONFIG[{}] */", cfg, config, ch));
                self.line(&format!("*(volatile uint32_t *)0x{:08x}UL = 0x{:x}UL; /* GPIOTE INTENSET IN[{}] */", intenset, 1u64 << ch, ch));
                self.line(&format!("*(volatile uint8_t *)0x{:08x}UL = 0x{:02x}U; /* NVIC IPR IRQ{} priority */", 0xE000_E400u64 + GPIOTE_IRQN as u64, prio, GPIOTE_IRQN));
            }
            self.line(&format!("*(volatile uint32_t *)0xE000E100UL = 0x{:x}UL; /* NVIC ISER0: enable GPIOTE */", 1u64 << GPIOTE_IRQN));
            self.emit_newline();
        }

        // Bus completion IRQ priority (§5.5): resume the reaction at its own
        // level so its cell critical sections still mask the right interrupts.
        // The IRQ is *enabled per-transaction* by `__bus_irq_enable()`, not here.
        if module_has_bus_xfer(module) && !bus_reactions.is_empty() {
            let prio = module
                .reactions
                .iter()
                .filter(|r| r.yields)
                .map(|r| r.priority)
                .max()
                .unwrap_or(self.max_priority);
            let pb = self.basepri_byte(prio);
            self.line(&format!(
                "*(volatile uint8_t *)0x{:08x}UL = 0x{:02x}U; /* NVIC IPR IRQ{}: bus completion priority */",
                0xE000_E400u64 + BUS_IRQN as u64,
                pb,
                BUS_IRQN
            ));
            self.emit_newline();
        }

        // Configure + start the system watchdog over its declared CR/RLR/KR
        // (§5.6).  Reload = the timeout; CR.start begins it; a KR write feeds it.
        let wdt_kr = module.watchdog_device.and_then(|wdt| {
            let regs = self.device_regs.get(&wdt);
            let (cr, rlr, kr) = match regs.and_then(|r| Some((r.get("CR")?, r.get("RLR")?, r.get("KR")?))) {
                Some((cr, rlr, kr)) => (*cr, *rlr, *kr),
                None => {
                    self.line("#error \"watchdog device missing a CR/RLR/KR register or MMIO base (§5.6)\"");
                    return None;
                }
            };
            let timeout_ms = module.watchdog_timeout_ns.unwrap_or(0) / 1_000_000;
            self.line("/* configure + start the system watchdog (§5.6) */");
            self.line(&format!("*(volatile uint32_t *)0x{:08x}UL = {}UL; /* RLR: reload (ms) */", rlr, timeout_ms));
            self.line(&format!("*(volatile uint32_t *)0x{:08x}UL = 0x1UL;  /* CR: start */", cr));
            self.line(&format!("*(volatile uint32_t *)0x{:08x}UL = 0xAAAAUL; /* KR: feed */", kr));
            self.emit_newline();
            Some(kr)
        });

        if systick.is_some() || !events.is_empty() || module_has_bus_xfer(module) {
            self.line("__DSB();");
            self.line("__asm__ volatile(\"cpsie i\"); /* enable interrupts */");
            self.emit_newline();
        }

        // Idle loop.  With a watchdog, feed it on a clean return to idle — only
        // when no yielding reaction is mid-transaction (§5.6).  A hung reaction
        // never reaches here (non-yielding → stuck in its ISR) or never goes idle
        // (yielding → frame state != 0), so it is not fed and the watchdog resets.
        if let Some(kr) = wdt_kr {
            let yielding: Vec<usize> = module.reactions.iter().filter(|r| r.yields).map(|r| r.id).collect();
            let mut idle = if yielding.is_empty() {
                "1".to_string()
            } else {
                yielding.iter().map(|id| format!("__rf_{}.__state == 0U", id)).collect::<Vec<_>>().join(" && ")
            };
            // §4.5/§5.6 — a missed `within` deadline permanently stops the feed.
            if !self.deadline_ticks.is_empty() {
                idle = format!("!__deadline_missed && ({})", idle);
            }
            self.line("for (;;) {");
            self.indent += 1;
            self.line(&format!("if ({}) {{ *(volatile uint32_t *)0x{:08x}UL = 0xAAAAUL; }} /* feed on clean idle (gated on deadline, §4.5/§5.6) */", idle, kr));
            self.line("__asm__ volatile(\"wfi\");");
            self.indent -= 1;
            self.line("}");
        } else {
            self.line("for (;;) { __asm__ volatile(\"wfi\"); }");
        }
        self.indent -= 1;
        self.line("}");
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn line(&mut self, s: &str) {
        let indent = "    ".repeat(self.indent);
        self.buf.push_str(&indent);
        self.buf.push_str(s);
        self.buf.push('\n');
    }

    fn emit_newline(&mut self) {
        self.buf.push('\n');
    }
}

// ─── Free helpers ─────────────────────────────────────────────────────────────

fn reaction_fn_name(id: usize) -> String {
    format!("__reaction_{}", id)
}

/// Strip a leading `&` (vector-table symbols are pre-formatted with one for
/// `_estack`; functions don't carry it).
fn strip_amp(s: &str) -> &str {
    s.strip_prefix('&').unwrap_or(s)
}

/// Does any statement in `stmts` (recursing into `Critical`/`If` bodies)
/// satisfy `pred`?
// ── Width-checked arithmetic helpers (§4.3 / SIL-004) ───────────────────────

/// The C type name for a `(width, signed)` scalar (`uint32_t`, `int8_t`, …).
fn arith_cint(width: u8, signed: bool) -> String {
    format!("{}int{}_t", if signed { "" } else { "u" }, width)
}

/// Stable name for the helper implementing one arithmetic shape.
fn arith_helper_name(op: SirArithOp, mode: OverflowMode, width: u8, signed: bool) -> String {
    let o = match op {
        SirArithOp::Add => "add",
        SirArithOp::Sub => "sub",
        SirArithOp::Mul => "mul",
    };
    let m = match mode {
        OverflowMode::Trap => "trap",
        OverflowMode::Wrap => "wrap",
        OverflowMode::Saturate => "sat",
    };
    format!("__si_{}_{}_{}{}", o, m, if signed { "s" } else { "u" }, width)
}

/// The full `static inline` definition for one arithmetic helper.
fn arith_helper_def(op: SirArithOp, mode: OverflowMode, width: u8, signed: bool) -> String {
    let name = arith_helper_name(op, mode, width, signed);
    let t = arith_cint(width, signed);
    let ut = arith_cint(width, false);
    let (c_op, bop) = match op {
        SirArithOp::Add => ("+", "add"),
        SirArithOp::Sub => ("-", "sub"),
        SirArithOp::Mul => ("*", "mul"),
    };
    match mode {
        // Wrap: compute in the unsigned counterpart (defined two's-complement
        // wraparound) and reinterpret at the result type.
        OverflowMode::Wrap => format!(
            "static inline {t} {name}({t} a, {t} b) {{ return ({t})(({ut})a {c_op} ({ut})b); }}"
        ),
        // Trap: the default (SIL-004).  Overflow → safe-state.
        OverflowMode::Trap => format!(
            "static inline {t} {name}({t} a, {t} b) {{ {t} __r; \
             if (__builtin_{bop}_overflow(a, b, &__r)) __silica_overflow_trap(); return __r; }}"
        ),
        // Saturate: clamp to the type's min/max in the overflow direction.
        OverflowMode::Saturate => {
            let max = if signed {
                format!("({t})(((({ut})~(({ut})0)) >> 1))")
            } else {
                format!("({t})~(({ut})0)")
            };
            let min = if signed {
                format!("({t})(-__max - 1)")
            } else {
                format!("({t})0")
            };
            // Direction of a saturating overflow.
            let pos = if !signed {
                // unsigned: add/mul can only overflow high; sub only underflows.
                if op == SirArithOp::Sub { "0" } else { "1" }.to_string()
            } else {
                match op {
                    SirArithOp::Add | SirArithOp::Sub => "(a >= 0)".to_string(),
                    SirArithOp::Mul => "((a < 0) == (b < 0))".to_string(),
                }
            };
            format!(
                "static inline {t} {name}({t} a, {t} b) {{ {t} __r; \
                 if (__builtin_{bop}_overflow(a, b, &__r)) {{ {t} __max = {max}; {t} __min = {min}; \
                 __r = ({pos}) ? __max : __min; }} return __r; }}"
            )
        }
    }
}

/// Collect every distinct arithmetic shape used in the module so each gets one
/// emitted helper.
fn collect_arith(module: &SirModule) -> HashSet<(SirArithOp, OverflowMode, u8, bool)> {
    let mut set = HashSet::new();
    for r in &module.reactions {
        collect_arith_stmts(&r.body, &mut set);
    }
    for v in &module.vars {
        collect_arith_expr(&v.init, &mut set);
    }
    for s in &module.safe_seqs {
        collect_arith_stmts(&s.body, &mut set);
    }
    set
}

type FixedCombo = (FixedArithOp, OverflowMode, u8, bool, u8);

/// Stable name for a fixed-point mul/div helper (§4.3, P0-3c).
fn fixed_helper_name(op: FixedArithOp, mode: OverflowMode, width: u8, signed: bool, frac: u8) -> String {
    let o = match op {
        FixedArithOp::Mul => "fixmul",
        FixedArithOp::Div => "fixdiv",
    };
    let m = match mode {
        OverflowMode::Trap => "trap",
        OverflowMode::Wrap => "wrap",
        OverflowMode::Saturate => "sat",
    };
    format!("__si_{}_{}_{}{}_f{}", o, m, if signed { "s" } else { "u" }, width, frac)
}

/// `static inline` definition for one fixed-point mul/div helper.  Computes in a
/// 64-bit intermediate (so a `width ≤ 32` product/quotient cannot overflow it),
/// rescales by `frac`, then applies the overflow mode at `width`.
fn fixed_helper_def(op: FixedArithOp, mode: OverflowMode, width: u8, signed: bool, frac: u8) -> String {
    let name = fixed_helper_name(op, mode, width, signed, frac);
    let t = arith_cint(width, signed);
    let wide = if signed { "int64_t" } else { "uint64_t" };
    let (lo, hi): (i128, i128) = if signed {
        (-(1i128 << (width as u32 - 1)), (1i128 << (width as u32 - 1)) - 1)
    } else {
        (0, (1i128 << width as u32) - 1)
    };
    // The rescaled raw value, in the 64-bit intermediate.
    let raw = match op {
        FixedArithOp::Mul => format!("(({wide})a * ({wide})b) >> {frac}"),
        FixedArithOp::Div => format!("((({wide})a) << {frac}) / ({wide})b"),
    };
    let guard_div0 = if op == FixedArithOp::Div { "if (b == 0) __silica_overflow_trap(); " } else { "" };
    match mode {
        OverflowMode::Wrap => format!(
            "static inline {t} {name}({t} a, {t} b) {{ {guard_div0}{wide} __r = {raw}; return ({t})__r; }}"
        ),
        OverflowMode::Trap => format!(
            "static inline {t} {name}({t} a, {t} b) {{ {guard_div0}{wide} __r = {raw}; \
             if (__r < ({wide}){lo} || __r > ({wide}){hi}) __silica_overflow_trap(); return ({t})__r; }}"
        ),
        OverflowMode::Saturate => format!(
            "static inline {t} {name}({t} a, {t} b) {{ {guard_div0}{wide} __r = {raw}; \
             if (__r > ({wide}){hi}) __r = ({wide}){hi}; else if (__r < ({wide}){lo}) __r = ({wide}){lo}; return ({t})__r; }}"
        ),
    }
}

fn collect_fixed(module: &SirModule) -> HashSet<FixedCombo> {
    let mut set = HashSet::new();
    for r in &module.reactions {
        collect_fixed_stmts(&r.body, &mut set);
    }
    for v in &module.vars {
        collect_fixed_expr(&v.init, &mut set);
    }
    for s in &module.safe_seqs {
        collect_fixed_stmts(&s.body, &mut set);
    }
    set
}

fn collect_fixed_stmts(stmts: &[SirStmt], set: &mut HashSet<FixedCombo>) {
    for s in stmts {
        match s {
            SirStmt::Assign { value, .. } => collect_fixed_expr(value, set),
            SirStmt::RegWrite { writes, .. } => {
                writes.iter().for_each(|(_, _, _, v)| collect_fixed_expr(v, set));
            }
            SirStmt::If { cond, then } => {
                collect_fixed_expr(cond, set);
                collect_fixed_stmts(then, set);
            }
            SirStmt::Exit(e) => collect_fixed_expr(e, set),
            SirStmt::Critical { body, .. } => collect_fixed_stmts(body, set),
            SirStmt::Poll { cond, .. } | SirStmt::Await { cond, .. } => collect_fixed_expr(cond, set),
            SirStmt::DeviceOp { args, .. } | SirStmt::BusXfer { args, .. } => {
                args.iter().for_each(|a| collect_fixed_expr(a, set));
            }
            SirStmt::RingPush { value, .. } => collect_fixed_expr(value, set),
            SirStmt::Intrinsic(SirIntrinsic::HostIoPrint(e)) => collect_fixed_expr(e, set),
            _ => {}
        }
    }
}

fn collect_fixed_expr(expr: &SirExpr, set: &mut HashSet<FixedCombo>) {
    match expr {
        SirExpr::FixedArith { op, mode, frac_bits, width, signed, lhs, rhs } => {
            set.insert((*op, *mode, *width, *signed, *frac_bits));
            collect_fixed_expr(lhs, set);
            collect_fixed_expr(rhs, set);
        }
        SirExpr::Not(i) | SirExpr::Cast { inner: i, .. } | SirExpr::FixedCast { inner: i, .. } => {
            collect_fixed_expr(i, set)
        }
        SirExpr::BinOp(_, l, r) => {
            collect_fixed_expr(l, set);
            collect_fixed_expr(r, set);
        }
        SirExpr::Arith { lhs, rhs, .. } => {
            collect_fixed_expr(lhs, set);
            collect_fixed_expr(rhs, set);
        }
        _ => {}
    }
}

fn collect_arith_stmts(stmts: &[SirStmt], set: &mut HashSet<(SirArithOp, OverflowMode, u8, bool)>) {
    for s in stmts {
        match s {
            SirStmt::Assign { value, .. } => collect_arith_expr(value, set),
            SirStmt::RegWrite { writes, .. } => {
                writes.iter().for_each(|(_, _, _, v)| collect_arith_expr(v, set));
            }
            SirStmt::If { cond, then } => {
                collect_arith_expr(cond, set);
                collect_arith_stmts(then, set);
            }
            SirStmt::Exit(e) => collect_arith_expr(e, set),
            SirStmt::Critical { body, .. } => collect_arith_stmts(body, set),
            SirStmt::Poll { cond, .. } => collect_arith_expr(cond, set),
            SirStmt::Await { cond, .. } => collect_arith_expr(cond, set),
            SirStmt::DeviceOp { args, .. } | SirStmt::BusXfer { args, .. } => {
                args.iter().for_each(|a| collect_arith_expr(a, set));
            }
            SirStmt::RingPush { value, .. } => collect_arith_expr(value, set),
            SirStmt::RingPop { .. } => {}
            SirStmt::Intrinsic(SirIntrinsic::HostIoPrint(e)) => collect_arith_expr(e, set),
            SirStmt::Intrinsic(_) => {}
        }
    }
}

fn collect_arith_expr(expr: &SirExpr, set: &mut HashSet<(SirArithOp, OverflowMode, u8, bool)>) {
    match expr {
        SirExpr::Not(i) => collect_arith_expr(i, set),
        SirExpr::BinOp(_, l, r) => {
            collect_arith_expr(l, set);
            collect_arith_expr(r, set);
        }
        SirExpr::Arith { op, mode, width, signed, lhs, rhs } => {
            set.insert((*op, *mode, *width, *signed));
            collect_arith_expr(lhs, set);
            collect_arith_expr(rhs, set);
        }
        SirExpr::Cast { inner, .. } | SirExpr::FixedCast { inner, .. } => collect_arith_expr(inner, set),
        SirExpr::FixedArith { lhs, rhs, .. } => {
            collect_arith_expr(lhs, set);
            collect_arith_expr(rhs, set);
        }
        _ => {}
    }
}

fn any_stmt(stmts: &[SirStmt], pred: &dyn Fn(&SirStmt) -> bool) -> bool {
    stmts.iter().any(|s| {
        pred(s)
            || match s {
                SirStmt::Critical { body, .. } => any_stmt(body, pred),
                SirStmt::If { then, .. } => any_stmt(then, pred),
                _ => false,
            }
    })
}

fn stmt_uses_stdio(s: &SirStmt) -> bool {
    matches!(
        s,
        SirStmt::Intrinsic(SirIntrinsic::HostIoPrint(_))
            | SirStmt::Intrinsic(SirIntrinsic::HostIoPrintStr(_))
            | SirStmt::Intrinsic(SirIntrinsic::HostIoFlush)
    )
}

fn trigger_comment(trigger: &SirTrigger) -> String {
    match trigger {
        SirTrigger::SysStart => "reaction: on sys.start".into(),
        SirTrigger::EveryNs(ns) => format!("reaction: every {}ns", ns),
        SirTrigger::Event(id) => format!("reaction: on event {}", id),
    }
}

/// True if any expression in the module reads the clock via `now()` (§4.5) —
/// gates the `__now_ns()` helper and the metal uptime counter.
fn module_uses_now(module: &SirModule) -> bool {
    module.reactions.iter().any(|r| stmts_have_now(&r.body))
}

fn stmts_have_now(stmts: &[SirStmt]) -> bool {
    stmts.iter().any(|s| match s {
        SirStmt::Assign { value, .. } => expr_has_now(value),
        SirStmt::RegWrite { writes, .. } => writes.iter().any(|(_, _, _, v)| expr_has_now(v)),
        SirStmt::If { cond, then } => expr_has_now(cond) || stmts_have_now(then),
        SirStmt::Exit(e) => expr_has_now(e),
        SirStmt::Critical { body, .. } => stmts_have_now(body),
        SirStmt::Poll { cond, .. } => expr_has_now(cond),
        SirStmt::Await { cond, .. } => expr_has_now(cond),
        SirStmt::DeviceOp { args, .. } | SirStmt::BusXfer { args, .. } => args.iter().any(expr_has_now),
        SirStmt::RingPush { value, .. } => expr_has_now(value),
        SirStmt::RingPop { .. } => false,
        SirStmt::Intrinsic(SirIntrinsic::HostIoPrint(e)) => expr_has_now(e),
        SirStmt::Intrinsic(_) => false,
    })
}

fn expr_has_now(e: &SirExpr) -> bool {
    match e {
        SirExpr::Now => true,
        SirExpr::Not(i) => expr_has_now(i),
        SirExpr::BinOp(_, l, r) => expr_has_now(l) || expr_has_now(r),
        _ => false,
    }
}

/// True if any reaction body contains a yielding bus transaction.
fn module_has_bus_xfer(module: &SirModule) -> bool {
    module
        .reactions
        .iter()
        .any(|r| r.body.iter().any(|s| matches!(s, SirStmt::BusXfer { .. })))
}

/// True if any statement in `stmts` (recursively, through `if`/critical bodies)
/// is a `poll` — i.e. the reaction can fault via a poll timeout (§3.2).
fn body_has_poll(stmts: &[SirStmt]) -> bool {
    // `await` shares the bounded-fault wrapper (`__faulted` → disposition).
    stmts.iter().any(|s| match s {
        SirStmt::Poll { .. } | SirStmt::Await { .. } => true,
        SirStmt::If { then, .. } => body_has_poll(then),
        SirStmt::Critical { body, .. } => body_has_poll(body),
        _ => false,
    })
}

fn expr_to_c_literal(expr: &SirExpr) -> String {
    match expr {
        SirExpr::Bool(b) => if *b { "1U".into() } else { "0U".into() },
        SirExpr::U64(n) => format!("{}ULL", n),
        _ => "0".into(),
    }
}

/// Escape a byte slice into a C string literal body (no surrounding quotes).
fn c_escape_bytes(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len());
    for &b in bytes {
        match b {
            b'\n' => s.push_str("\\n"),
            b'\t' => s.push_str("\\t"),
            b'\r' => s.push_str("\\r"),
            b'\\' => s.push_str("\\\\"),
            b'"' => s.push_str("\\\""),
            0x20..=0x7e => s.push(b as char),
            // Fixed-width 3-digit octal, not `\xNN`: a C `\x` escape greedily
            // consumes *all* following hex digits, so `\xA9` before an ASCII
            // hex digit would merge into one larger value and corrupt the byte
            // stream.  An octal escape consumes at most 3 digits, so emitting
            // exactly 3 is unambiguous regardless of the next character.
            _ => {
                let _ = write!(s, "\\{:03o}", b);
            }
        }
    }
    s
}

// ─── Metal: linker script + RAM budget (§6.4, §5.3) ──────────────────────────

// ── Worst-case stack analysis (§5.3/SIL-005) ────────────────────────────────
// Reactions and ops never recurse (the resolver bans it), so the call graph is
// acyclic and the worst-case stack is a compile-time sum.  We over-approximate
// soundly from the SIR: each reaction's frame = its frame-local count × a word
// plus a fixed per-frame overhead; the worst-case ISR nesting stacks at most one
// frame per *distinct* static priority level (a reaction cannot preempt one at
// its own level — non-reentrant, run-to-completion), so the bound is the sum of
// the largest frame at each level, plus an exception frame per level and a base.

/// Machine word (Cortex-M, AAPCS) — each spilled local rounds up to this.
const STACK_WORD: u64 = 4;
/// Per active handler frame: saved registers + call args + spill headroom.
const FRAME_OVERHEAD: u64 = 96;
/// Cortex-M hardware exception stack frame pushed per nesting level.
const EXC_FRAME: u64 = 64;
/// Base context (main / startup / idle loop) plus headroom.
const STACK_BASE: u64 = 512;

#[derive(Debug, Clone, Copy)]
pub struct RamBudget {
    pub statics: u64,
    pub stack_reserve: u64,
    pub ram_size: u64,
}

impl RamBudget {
    pub fn used(&self) -> u64 {
        self.statics + self.stack_reserve
    }
}

/// Compute the static RAM footprint and check it against the RAM region
/// (validation gate #1, §5.3).  No dynamic allocation exists in the language, so
/// the total is `statics + reserved stack`; exceeding the region is an error.
pub fn ram_budget(module: &SirModule) -> Result<RamBudget, String> {
    let ram = module
        .memory
        .iter()
        .find(|r| r.is_ram())
        .ok_or("no RAM region found in board.soc.memory")?;
    // Model the generated C layout conservatively: lay statics out in declaration
    // order, aligning each to its natural alignment (so a u8 before a u64 incurs
    // the padding the C compiler would), so the gate doesn't undercount.
    let mut off = 0u64;
    for v in &module.vars {
        let size = v.ty.byte_size();
        let align = size.max(1); // scalars: align == size (1/2/4/8); Bytes/ptr = 4
        off = off.div_ceil(align) * align;
        off += size;
    }
    let statics = off;
    let stack = worst_case_stack(module);
    let budget = RamBudget { statics, stack_reserve: stack, ram_size: ram.size };
    if budget.used() > budget.ram_size {
        return Err(format!(
            "RAM budget exceeded: {} B (statics {} + worst-case stack {}) > {} B region '{}'",
            budget.used(), statics, stack, ram.size, ram.name
        ));
    }
    Ok(budget)
}

/// The worst-case stack high-water mark (§5.3/SIL-005), in bytes.  A sound
/// over-approximation from the SIR: the sum, over distinct static priority
/// levels, of the largest reaction frame at that level (+ an exception frame),
/// plus a base context.  Recursion would make this unbounded, so it is banned in
/// the resolver.
pub fn worst_case_stack(module: &SirModule) -> u64 {
    let globals: HashSet<&str> = module.vars.iter().map(|v| v.name.as_str()).collect();
    // Largest frame seen at each distinct priority level.
    let mut by_level: BTreeMap<u8, u64> = BTreeMap::new();
    for r in &module.reactions {
        let mut frame = HashSet::new();
        count_frame_vars(&r.body, &globals, &mut frame);
        let bytes = frame.len() as u64 * STACK_WORD + FRAME_OVERHEAD + EXC_FRAME;
        let slot = by_level.entry(r.priority).or_insert(0);
        *slot = (*slot).max(bytes);
    }
    STACK_BASE + by_level.values().sum::<u64>()
}

/// Count a reaction's distinct frame-local slots (assignment targets + bus
/// destinations that are not module globals), recursing into `if`/critical.
fn count_frame_vars<'a>(stmts: &'a [SirStmt], globals: &HashSet<&str>, seen: &mut HashSet<&'a str>) {
    for s in stmts {
        match s {
            SirStmt::Assign { target: SirPlace::Var(n), .. } if !globals.contains(n.as_str()) => {
                seen.insert(n.as_str());
            }
            SirStmt::BusXfer { dst, .. } => {
                seen.insert(dst.as_str());
            }
            SirStmt::If { then, .. } => count_frame_vars(then, globals, seen),
            SirStmt::Critical { body, .. } => count_frame_vars(body, globals, seen),
            _ => {}
        }
    }
}

// ─── Metal: `every` → SysTick plan (§4.5) ─────────────────────────────────────

/// Default core clock for the nRF52840 if the board declares none.
pub const NRF52840_CORE_HZ: u64 = 64_000_000;
/// SysTick base period: a 1 ms tick, software-prescaled per reaction.  A single
/// 24-bit SysTick cannot hold long periods directly (500 ms at 64 MHz overflows),
/// so the handler counts base ticks per `every` reaction.
pub const SYSTICK_BASE_NS: u64 = 1_000_000;

#[derive(Debug, Clone)]
pub struct SysTickPlan {
    /// SysTick reload value (RVR): `core_hz * base_ns / 1e9 - 1`, must fit 24 bits.
    pub reload: u64,
    /// Per-`every`-reaction base-tick threshold: `(reaction_id, ticks)`.
    pub thresholds: Vec<(usize, u64)>,
}

/// Plan the SysTick programming for the module's `every` reactions (§4.5).
/// `Ok(None)` if there are no periodic reactions.  Errors if a period is not a
/// whole base tick or the reload does not fit SysTick's 24-bit counter.
pub fn systick_plan(module: &SirModule) -> Result<Option<SysTickPlan>, String> {
    let everys: Vec<(usize, u64)> = module
        .reactions
        .iter()
        .filter_map(|r| match r.trigger {
            SirTrigger::EveryNs(ns) => Some((r.id, ns)),
            _ => None,
        })
        .collect();
    if everys.is_empty() {
        return Ok(None);
    }
    let core_hz = if module.core_hz != 0 { module.core_hz } else { NRF52840_CORE_HZ };
    let ticks_per_base = core_hz * SYSTICK_BASE_NS / 1_000_000_000;
    if ticks_per_base == 0 {
        return Err(format!("core clock {} Hz too slow for a {} ns SysTick base", core_hz, SYSTICK_BASE_NS));
    }
    let reload = ticks_per_base - 1;
    if reload > 0x00FF_FFFF {
        return Err(format!("SysTick reload {} exceeds 24 bits (core {} Hz)", reload, core_hz));
    }
    let mut thresholds = Vec::new();
    for (id, ns) in everys {
        if ns % SYSTICK_BASE_NS != 0 {
            return Err(format!(
                "`every` period {} ns is not a whole {} ns SysTick base tick",
                ns, SYSTICK_BASE_NS
            ));
        }
        thresholds.push((id, ns / SYSTICK_BASE_NS));
    }
    Ok(Some(SysTickPlan { reload, thresholds }))
}

/// The abstract priority of the module's periodic (`every`) reactions, used to
/// set the SysTick exception priority for the ceiling protocol (§5.5).
fn systick_priority(module: &SirModule) -> Option<u8> {
    module
        .reactions
        .iter()
        .filter(|r| matches!(r.trigger, SirTrigger::EveryNs(_)))
        .map(|r| r.priority)
        .max()
}

/// Generate the linker script from the board's memory regions (§6.4).
pub fn emit_linker_script(module: &SirModule) -> Result<String, String> {
    let flash = module
        .memory
        .iter()
        .find(|r| !r.is_ram())
        .ok_or("no flash/code region found in board.soc.memory")?;
    let ram = module
        .memory
        .iter()
        .find(|r| r.is_ram())
        .ok_or("no RAM region found in board.soc.memory")?;

    let mut s = String::new();
    let _ = writeln!(s, "/* Generated by silicac from board.soc.memory (§6.4) — do not edit. */");
    let _ = writeln!(s, "MEMORY");
    let _ = writeln!(s, "{{");
    let _ = writeln!(s, "  FLASH (rx) : ORIGIN = 0x{:08x}, LENGTH = {}", flash.origin, flash.size);
    let _ = writeln!(s, "  RAM  (rwx) : ORIGIN = 0x{:08x}, LENGTH = {}", ram.origin, ram.size);
    let _ = writeln!(s, "}}");
    let _ = writeln!(s, "ENTRY(Reset_Handler)");
    let _ = writeln!(s, "_estack = ORIGIN(RAM) + LENGTH(RAM);");
    let _ = writeln!(s, "SECTIONS");
    let _ = writeln!(s, "{{");
    let _ = writeln!(s, "  .vectors : {{ KEEP(*(.vectors)) }} > FLASH");
    let _ = writeln!(s, "  .text    : {{ *(.text*) *(.rodata*) }} > FLASH");
    let _ = writeln!(s, "  _sidata = LOADADDR(.data);");
    let _ = writeln!(s, "  .data : {{ _sdata = .; *(.data*) _edata = .; }} > RAM AT > FLASH");
    let _ = writeln!(s, "  .bss  : {{ _sbss = .; *(.bss*) *(COMMON) _ebss = .; }} > RAM");
    let _ = writeln!(s, "}}");
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn c_escape_basics() {
        assert_eq!(c_escape_bytes(b"Hello\n"), "Hello\\n");
        assert_eq!(c_escape_bytes(b"say \"hi\""), "say \\\"hi\\\"");
    }

    #[test]
    fn c_escape_nonprintable_uses_fixed_octal() {
        // 0xA9 followed by an ASCII hex digit must NOT use `\xNN` (which would
        // greedily merge into one escape).  Fixed 3-digit octal is unambiguous.
        assert_eq!(c_escape_bytes(&[0xA9, b'A']), "\\251A");
        assert_eq!(c_escape_bytes(&[0x00, b'1']), "\\0001");
    }

    #[test]
    fn includes_stdio_for_print_nested_in_critical() {
        // After cell analysis a print can live inside a `Critical` body; the
        // header scan must recurse to still include <stdio.h> + the helper.
        let module = SirModule {
            reactions: vec![SirReaction {
                id: 0,
                trigger: SirTrigger::SysStart,
                body: vec![SirStmt::Critical {
                    ceiling: 1,
                    body: vec![SirStmt::Intrinsic(SirIntrinsic::HostIoPrintStr("hi\n".into()))],
                }],
                priority: 0,
                disposition: SirDisposition::Escalate,
                yields: false,
                deadline_ns: None,
                overflow: SirOverflow::Coalesce,
            }],
            ..Default::default()
        };
        let c = CBackend::new().emit(&module);
        assert!(c.contains("#include <stdio.h>"), "missing stdio include for nested print");
        assert!(c.contains("__host_io_print"), "missing host_io helper for nested print");
    }

    #[test]
    fn emit_hello_world() {
        let module = SirModule {
            reactions: vec![SirReaction {
                id: 0,
                trigger: SirTrigger::SysStart,
                body: vec![SirStmt::Intrinsic(SirIntrinsic::HostIoPrintStr(
                    "Hello, World!\n".into(),
                ))],
                priority: 0,
                disposition: SirDisposition::Escalate,
                yields: false,
                deadline_ns: None,
                overflow: SirOverflow::Coalesce,
            }],
            ..Default::default()
        };
        let c = CBackend::new().emit(&module);
        assert!(c.contains("__host_io_print"));
        assert!(c.contains("Hello, World!\\n"));
        assert!(c.contains("int main(void)"));
        assert!(c.contains("__reaction_0();"));
    }
}
