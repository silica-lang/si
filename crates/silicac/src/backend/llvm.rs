//! LLVM-IR canary backend — a *second* SIR consumer (DESIGN §6.3/§12).
//!
//! The C backend claims to be "pure" (fixed-width, no UB, no C bitfields), and
//! SIR is *claimed* to be target-neutral (§6.1 — "SIR is the contract, backends
//! are just consumers").  That claim is only credible if a structurally
//! different backend can consume the same SIR.  This module is the canary: it
//! lowers a SIR subset to **textual LLVM IR**, and the whole point is that it
//! shares *nothing* with the C path — so any C-ism that leaked into SIR (a
//! `__builtin_*`, a libc dependency, an `int` width assumption) shows up here as
//! something that cannot be expressed without reaching for C.
//!
//! Two structural proofs fall out of this design:
//!   - **Overflow trap → `llvm.{u,s}{add,sub,mul}.with.overflow.iN` + `llvm.trap`**
//!     (NOT `__builtin_*_overflow`): the trap-by-default guarantee (§4.3/SIL-004)
//!     lowers to a first-class LLVM intrinsic, proving it was never C-specific.
//!   - **No libc / no C runtime**: arithmetic and `exit` lower to pure IR
//!     (`ret i32`); host-io lowers to a raw syscall via inline asm — never a
//!     `printf`/`write` symbol.  The emitted module references no external C
//!     symbol at all.
//!
//! The supported subset (extended in P3-4a — still not the whole language):
//! `@main` runs the `on sys.start` bodies and **every other reaction lowers to
//! its own `void @__reaction_N` function** (no scheduler yet — wiring `every`/`on`
//! to a timer/IRQ is P3-4c).  Statements: `Assign(SirPlace::Var)`, `If`, `Exit`,
//! the host-io intrinsics.  Expressions: `SirExpr::{Bool, U64, Load, Not, BinOp,
//! Arith (trap/wrap/saturate, signed + unsigned), Cast, Now}` (`now()` →
//! `llvm.readcyclecounter`).  MMIO is supported (P3-4b): a `SirPlace::Reg` store
//! / `RegLoad` lowers to a `volatile` load/store at `base + offset` via
//! `inttoptr` (a rw field is a read-modify-write).
//!
//! Two target directions (P3-4c — `with_target`): `Host` emits an `@main` +
//! raw-syscall host-io module; `MetalNrf52840` emits a **freestanding** module —
//! a `.vectors` table `[_estack, Reset_Handler]` + a `Reset_Handler` that runs
//! `sys.start` then idles (`wfi`), no `@main`/syscall — which links against the
//! generated linker script and **boots on Renode** (`harness/llvm_metal.sh`).
//!
//! Anything else (rings, bus transactions, and on metal the scheduler that
//! *calls* the per-reaction functions + the yields state machine) emits a
//! `; unsupported in llvm canary` comment (harmless to IR validity) — a
//! deliberate signpost for what a full LLVM backend would still need.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use crate::backend::Target;
use crate::sir::*;

/// LLVM target triple for the metal direction (Cortex-M4F, Thumb).
const METAL_TRIPLE: &str = "thumbv7em-none-eabi";

// nRF52840 peripheral/NVIC addresses mirrored from the C metal backend (§4.1/§4.5).
const GPIOTE_BASE: u64 = 0x4000_6000;
const GPIOTE_IRQN: usize = 6;
const BUS_IRQN: usize = 8;
const NVIC_ISER0: u64 = 0xE000_E100;
const NVIC_IPR0: u64 = 0xE000_E400;

/// Map an abstract reaction priority to a hardware BASEPRI/NVIC byte (§5.5),
/// mirroring `CBackend::basepri_byte`: 3 NVIC priority bits (top of the byte),
/// level starting at 1 so a ceiling is never 0.
fn basepri_byte(prio: u8, max_priority: u8) -> u8 {
    ((max_priority - prio) + 1) << 5
}

/// True if an expression contains an overflow-trapping arithmetic op (§4.3) —
/// gates the metal `@__silica_overflow_trap` + `@__drive_safe` emission (P5-2).
fn expr_has_arith_trap(e: &SirExpr) -> bool {
    match e {
        SirExpr::Arith { mode, lhs, rhs, .. } => {
            *mode == OverflowMode::Trap || expr_has_arith_trap(lhs) || expr_has_arith_trap(rhs)
        }
        // Fixed-point (P6-2): a Trap mode traps on range, and a Div always traps on
        // divide-by-zero — both route to `@__silica_overflow_trap`.
        SirExpr::FixedArith { op, mode, lhs, rhs, .. } => {
            *mode == OverflowMode::Trap
                || *op == FixedArithOp::Div
                || expr_has_arith_trap(lhs)
                || expr_has_arith_trap(rhs)
        }
        SirExpr::Not(i) | SirExpr::Cast { inner: i, .. } | SirExpr::FixedCast { inner: i, .. } => {
            expr_has_arith_trap(i)
        }
        SirExpr::BinOp(_, l, r) => expr_has_arith_trap(l) || expr_has_arith_trap(r),
        _ => false,
    }
}

/// True if any statement in `stmts` (through `if`/critical bodies) holds a
/// trapping arithmetic op.
fn stmts_have_arith_trap(stmts: &[SirStmt]) -> bool {
    crate::backend::c::any_stmt(stmts, &|s| match s {
        SirStmt::Assign { value, .. } => expr_has_arith_trap(value),
        SirStmt::Exit(e) => expr_has_arith_trap(e),
        SirStmt::If { cond, .. } => expr_has_arith_trap(cond),
        SirStmt::RegWrite { writes, .. } => writes.iter().any(|(_, _, _, v)| expr_has_arith_trap(v)),
        _ => false,
    })
}

/// Bit width of a scalar SIR type, for `iN` selection.  Non-scalars default to
/// 64 (they are outside the canary subset and never reach storage here).
fn sir_bits(ty: &SirType) -> u32 {
    match ty {
        SirType::Bool => 1,
        SirType::U8 | SirType::S8 => 8,
        SirType::U16 | SirType::S16 => 16,
        SirType::U32 | SirType::S32 | SirType::F32 => 32,
        SirType::U64 | SirType::S64 | SirType::Instant | SirType::Duration | SirType::F64 => 64,
        SirType::Fixed { int_bits, frac_bits, .. } => {
            SirType::fixed_storage_bits(*int_bits, *frac_bits)
        }
        SirType::Bytes | SirType::Ring { .. } => 64,
    }
}

fn sir_signed(ty: &SirType) -> bool {
    matches!(ty, SirType::S8 | SirType::S16 | SirType::S32 | SirType::S64)
}

/// Round an arbitrary width up to a storable `iN` (8/16/32/64); i1 stays i1.
fn store_bits(bits: u32) -> u32 {
    match bits {
        1 => 1,
        0..=8 => 8,
        9..=16 => 16,
        17..=32 => 32,
        _ => 64,
    }
}

pub struct LlvmBackend {
    /// Module-scope global definitions (cells + string constants).
    globals: String,
    /// `alloca`s for reaction-local `let`s, emitted at the top of `@main`.
    allocas: String,
    /// The instruction stream of the function currently being lowered.
    body: String,
    /// Completed `define` blocks (`@main` + one per non-`sys.start` reaction).
    functions: String,
    /// True while lowering an `i32`-returning function (`@main`); false for the
    /// `void`-returning reaction functions — so `Exit` picks the right `ret`.
    ret_i32: bool,
    /// `declare` lines for the LLVM intrinsics actually used (deduped).
    decls: String,
    declared: HashSet<String>,
    /// Next SSA temporary / basic-block label number.
    next_reg: u32,
    next_label: u32,
    next_str: u32,
    /// Var name → (storage bits, signed, is_global).  Globals live at module
    /// scope (`@name`); locals are `alloca`d (`%name.addr`).
    vars: HashMap<String, (u32, bool, bool)>,
    /// True once a terminator (`ret` from `Exit`) has been emitted into `@main`,
    /// so trailing statements are skipped rather than placed after a terminator.
    terminated: bool,
    /// device id → MMIO base address (audit P3-4b): a `SirPlace::Reg`/`RegLoad`
    /// lowers to a `volatile` load/store at `base + reg_offset` via `inttoptr`.
    device_bases: HashMap<usize, u64>,
    /// Target direction (audit P3-4c): `Host` → an `@main` + raw-syscall host-io
    /// module; `MetalNrf52840` → a freestanding module with a `.vectors` table
    /// and a `Reset_Handler` that runs `sys.start` then idles (links against the
    /// generated linker script and boots on Renode).
    target: Target,
    /// Max reaction priority in the module (audit P4-2): the `basepri_byte`
    /// reference for NVIC/BASEPRI priority-ceiling mapping (§5.5).
    max_priority: u8,
    /// device id → (register name → absolute MMIO address) (audit P4-3): a bus
    /// controller's CR/SR/SA/RA/DR, resolved by name for the yields kick/resume.
    device_regs: HashMap<usize, HashMap<String, u64>>,
    /// While lowering a yielding reaction (P4-3): `(global prefix, cross-yield
    /// temp names)`.  Those temps are module globals (`@<prefix><name>`) so they
    /// survive an IRQ return; `var_ptr` rewrites refs to them.
    frame: Option<(String, std::collections::HashSet<String>)>,
    /// Module cell names (P4-3): distinguishes a cell (real global) from a
    /// reaction-local temp when collecting a yielding reaction's frame.
    cells: std::collections::HashSet<String>,
    /// While lowering a non-yielding reaction that can fault via a `poll`/`await`
    /// timeout (P5-3): the reaction's Layer-2 disposition, routed when `__faulted`
    /// is set.  `None` outside such a reaction.
    current_disposition: Option<SirDisposition>,
    /// While lowering a `Retry`-disposition reaction (P5-3): the back-edge label
    /// to re-run the body on a retry.  `None` otherwise.
    retry_loop: Option<String>,
    /// reaction id → `within` deadline in SysTick (1 ms) ticks (P5-4).  Populated
    /// only when a watchdog exists (the reset mechanism); the SysTick handler
    /// counts these down and the idle-loop feed is gated on none having elapsed.
    deadline_ticks: HashMap<usize, u32>,
    /// ring name → (element bits, capacity) (P6-1).  A `ring<T,N>` cell lowers to
    /// backing-store globals (`@__ring_<n>_buf/head/tail/count`) + index math,
    /// mirroring the C backend's `ring_info`.
    ring_info: HashMap<String, (u32, u32)>,
}

impl LlvmBackend {
    pub fn new() -> Self {
        Self::with_target(Target::Host)
    }

    pub fn with_target(target: Target) -> Self {
        LlvmBackend {
            globals: String::new(),
            allocas: String::new(),
            body: String::new(),
            functions: String::new(),
            ret_i32: true,
            decls: String::new(),
            declared: HashSet::new(),
            next_reg: 0,
            next_label: 0,
            next_str: 0,
            vars: HashMap::new(),
            terminated: false,
            device_bases: HashMap::new(),
            target,
            max_priority: 0,
            device_regs: HashMap::new(),
            frame: None,
            cells: std::collections::HashSet::new(),
            current_disposition: None,
            retry_loop: None,
            deadline_ticks: HashMap::new(),
            ring_info: HashMap::new(),
        }
    }

    fn fresh(&mut self) -> String {
        let r = format!("%t{}", self.next_reg);
        self.next_reg += 1;
        r
    }

    fn fresh_label(&mut self, base: &str) -> String {
        let l = format!("{}{}", base, self.next_label);
        self.next_label += 1;
        l
    }

    /// Append one indented instruction to `@main`'s current block.
    fn inst(&mut self, s: &str) {
        self.body.push_str("  ");
        self.body.push_str(s);
        self.body.push('\n');
    }

    /// Open a new basic block (label at column 0).
    fn label(&mut self, name: &str) {
        self.body.push_str(name);
        self.body.push_str(":\n");
    }

    fn declare(&mut self, line: &str) {
        if self.declared.insert(line.to_string()) {
            self.decls.push_str(line);
            self.decls.push('\n');
        }
    }

    // ── Entry point ─────────────────────────────────────────────────────────

    /// Lower a `SirModule` to a textual LLVM-IR translation unit.
    pub fn emit(mut self, module: &SirModule) -> String {
        // 0. Device MMIO bases (P3-4b) + the priority-ceiling reference (P4-2).
        for dev in &module.devices {
            if let Some(base) = dev.base_addr {
                self.device_bases.insert(dev.id, base);
                let regs = dev.regs.iter().map(|r| (r.name.clone(), base + r.offset)).collect();
                self.device_regs.insert(dev.id, regs);
            }
        }
        self.max_priority = module.reactions.iter().map(|r| r.priority).max().unwrap_or(0);
        // 1. Cells → module globals.
        for v in &module.vars {
            // A bounded ring (§5.3, P6-1): backing array + head/tail/count globals
            // instead of a scalar — mirrors the C `__ring_<n>_buf/head/tail/count`.
            if let SirType::Ring { elem_bytes, cap } = v.ty {
                let ebits = (elem_bytes as u32) * 8;
                self.ring_info.insert(v.name.clone(), (ebits, cap));
                self.globals.push_str(&format!(
                    "@__ring_{n}_buf = global [{c} x i{e}] zeroinitializer\n@__ring_{n}_head = global i32 0\n@__ring_{n}_tail = global i32 0\n@__ring_{n}_count = global i32 0\n",
                    n = v.name, c = cap.max(1), e = ebits
                ));
                continue;
            }
            let bits = store_bits(sir_bits(&v.ty));
            let signed = sir_signed(&v.ty);
            self.vars.insert(v.name.clone(), (bits, signed, true));
            self.cells.insert(v.name.clone());
            let init = const_init(&v.init);
            self.globals
                .push_str(&format!("@{} = global i{} {}\n", v.name, bits, init));
        }

        let sys: Vec<&[SirStmt]> = module
            .reactions
            .iter()
            .filter(|r| matches!(r.trigger, SirTrigger::SysStart))
            .map(|r| r.body.as_slice())
            .collect();

        let metal = self.target == Target::MetalNrf52840;
        // Metal `every` → TIMER1 plan (P4-1).  A planning error degrades to "no
        // timer" here (the C build path is the authoritative validator).
        let timer = if metal {
            crate::backend::c::timer_plan(module).unwrap_or(None)
        } else {
            None
        };
        // Metal `on <pin>.falling` → GPIOTE channels (P4-2).
        let events = if metal { self.events_of(module) } else { Vec::new() };
        // Metal SysTick (P5-1): a 1 ms base tick backing `now()` uptime (and,
        // from P5-4, `within` deadline bookkeeping + the watchdog wake cadence).
        // Mirrors `c.rs` `needs_systick` — true for now() OR a watchdog.
        let uses_now = metal && crate::backend::c::module_uses_now(module);
        // P6-5: reactions that suspend on an `await` are resumed by SysTick.
        let await_ids: Vec<usize> = if metal {
            let mut v: Vec<usize> = module
                .reactions
                .iter()
                .filter(|r| crate::backend::c::body_has_await(&r.body))
                .map(|r| r.id)
                .collect();
            v.sort_unstable();
            v
        } else {
            Vec::new()
        };
        let needs_systick =
            metal && (uses_now || module.watchdog_device.is_some() || !await_ids.is_empty());
        let systick_rvr = if needs_systick {
            crate::backend::c::systick_reload(module).ok()
        } else {
            None
        };
        // `now()` reads this SysTick-driven uptime (ns, 1 ms resolution) on metal,
        // mirroring the C `__uptime_ns` (host keeps `llvm.readcyclecounter`).
        if uses_now {
            self.globals.push_str("@__uptime_ns = global i64 0\n");
        }
        // `within` deadlines (P5-4) — enforced on metal only when a watchdog
        // exists (the reset mechanism), and only for yielding reactions (a
        // non-yielding one is bounded by construction in its ISR).  Mirrors c.rs.
        if metal && module.watchdog_device.is_some() {
            for r in &module.reactions {
                if r.yields {
                    if let Some(ns) = r.deadline_ns {
                        self.deadline_ticks.insert(r.id, ns.div_ceil(1_000_000).max(1) as u32);
                    }
                }
            }
        }
        if !self.deadline_ticks.is_empty() {
            let mut ids: Vec<usize> = self.deadline_ticks.keys().copied().collect();
            ids.sort_unstable();
            for id in ids {
                self.globals.push_str(&format!("@__deadline_{} = global i32 0\n", id));
            }
            self.globals.push_str("@__deadline_missed = global i32 0\n");
        }
        if metal {
            // Metal: `Reset_Handler` does real startup (.data/.bss/pins + GPIOTE)
            // + runs `sys.start` + programs SysTick/TIMER1, then idles.  No `@main`.
            self.lower_reset_handler(module, &sys, timer.as_ref(), &events, systick_rvr);
        } else {
            // Host: `@main` runs every `on sys.start` body, in order.
            self.lower_function("i32 @main()", true, &sys);
        }

        // Every non-`sys.start` reaction lowers to its own function; on metal the
        // TIMER1 handler (P4-1) / GPIOTE (P4-2) call them.  A **yielding** reaction
        // (a bus transaction) lowers to the IRQ-driven segment state machine
        // (P4-3) instead of a flat body.
        let bus_reactions: Vec<usize> = module.reactions.iter().filter(|r| r.yields).map(|r| r.id).collect();
        for r in &module.reactions {
            if matches!(r.trigger, SirTrigger::SysStart) {
                continue;
            }
            // A bus transaction OR an `await` (P6-5) suspends → segment state machine.
            if metal && (r.yields || crate::backend::c::body_has_await(&r.body)) {
                self.emit_yielding_metal(r);
            } else {
                self.lower_reaction_fn(r);
            }
        }

        let has_bus = metal && !bus_reactions.is_empty();
        if metal {
            if let Some(plan) = &timer {
                self.emit_timer_handler(plan);
            }
            if !events.is_empty() {
                self.emit_gpiote_handler(&events);
            }
            if has_bus {
                self.emit_bus_irq_handler(&bus_reactions);
            }
            if needs_systick {
                self.emit_systick_handler(uses_now, &await_ids);
            }
            // Safe-state runtime (P5-2): emit `@__drive_safe` whenever something
            // references it (a `Safe` disposition, an overflow trap, or a
            // `DriveSafe` guard), plus `@__silica_overflow_trap` when arithmetic
            // can trap.  Mirrors the C gating in `emit_drive_safe`.
            let has_safe_disp = module
                .reactions
                .iter()
                .any(|r| matches!(r.disposition, SirDisposition::Safe));
            let has_drive_safe = module
                .reactions
                .iter()
                .any(|r| crate::backend::c::any_stmt(&r.body, &|s| matches!(s, SirStmt::DriveSafe)));
            let has_arith_trap = module.reactions.iter().any(|r| stmts_have_arith_trap(&r.body))
                || module.safe_seqs.iter().any(|sq| stmts_have_arith_trap(&sq.body));
            if has_safe_disp || has_drive_safe || has_arith_trap {
                self.emit_drive_safe_fn(module);
            }
            if has_arith_trap {
                self.emit_overflow_trap_fn();
            }
            // Layer-3 fault decoder is the HardFault vector target (P6-3).
            self.emit_fault_decoder(module);
            self.emit_default_handlers();
        }

        // Assemble the module.
        let mut out = String::new();
        out.push_str("; Silica LLVM-IR backend (audit #35 P2-1 + P3-4 + P4, DESIGN §6.3/§12)\n");
        out.push_str("; A second, structurally independent SIR consumer — proves SIR is\n");
        out.push_str("; target-neutral and the overflow trap is not a C-ism.\n\n");
        if metal {
            out.push_str(&format!("target triple = \"{}\"\n\n", METAL_TRIPLE));
            // Linker-provided symbols (addresses): stack top + .data/.bss bounds.
            out.push_str("@_estack = external global i8\n");
            for s in ["_sidata", "_sdata", "_edata", "_sbss", "_ebss"] {
                out.push_str(&format!("@{} = external global i8\n", s));
            }
            if has_bus {
                // The single in-flight bus owner (P4-3): which `@__react_N_run` the
                // shared `@__BUS_IRQHandler` resumes on completion (§5.1/§5.2).
                out.push_str("@__bus_owner = global i32 -1\n");
            }
            out.push('\n');
            // The Cortex-M vector table (placed at flash base by the linker).
            out.push_str(&self.emit_vector_table(timer.is_some(), !events.is_empty(), has_bus, needs_systick));
            out.push('\n');
        }
        if !self.globals.is_empty() {
            out.push_str(&self.globals);
            out.push('\n');
        }
        out.push_str(&self.functions);
        if !self.decls.is_empty() {
            out.push('\n');
            out.push_str(&self.decls);
        }
        out
    }

    /// Lower a list of statement bodies into one LLVM function `define`, appended
    /// to `self.functions`.  `ret_i32` selects the return type (`@main` returns
    /// `i32`; reaction functions return `void`).  Per-function state (the
    /// instruction stream, allocas, locals, terminator flag) is reset first;
    /// the SSA/label counters stay monotonic (uniqueness within each function is
    /// all LLVM requires).
    fn lower_function(&mut self, sig: &str, ret_i32: bool, bodies: &[&[SirStmt]]) {
        self.body.clear();
        self.allocas.clear();
        self.terminated = false;
        self.ret_i32 = ret_i32;
        // Drop the previous function's locals; keep module globals.
        self.vars.retain(|_, v| v.2);
        for b in bodies {
            self.collect_locals(b);
        }
        for b in bodies {
            for stmt in b.iter() {
                if self.terminated {
                    break;
                }
                self.emit_stmt(stmt);
            }
        }
        if !self.terminated {
            self.inst(if ret_i32 { "ret i32 0" } else { "ret void" });
        }
        self.functions.push_str(&format!("define {} {{\nentry:\n", sig));
        self.functions.push_str(&self.allocas);
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
    }

    /// Lower a non-`sys.start`, non-yielding reaction to its `@__reaction_N` fn.
    /// A reaction that can fault via a `poll`/`await` timeout (P5-3) gets a
    /// `%__faulted` flag + the Layer-2 disposition routing — a `Retry` disposition
    /// wraps the body in a bounded re-run loop, as the yielding path does.
    /// Mirrors the C `emit_reaction_fn` fault wrapper.
    fn lower_reaction_fn(&mut self, r: &SirReaction) {
        let sig = format!("void @__reaction_{}()", r.id);
        let metal = self.target == Target::MetalNrf52840;
        let has_poll = metal && crate::backend::c::body_has_poll(&r.body);
        if !has_poll {
            self.lower_function(&sig, false, &[r.body.as_slice()]);
            return;
        }
        self.body.clear();
        self.allocas.clear();
        self.terminated = false;
        self.ret_i32 = false;
        self.vars.retain(|_, v| v.2);
        self.collect_locals(&r.body);
        self.allocas.push_str("  %__faulted = alloca i8\n");
        self.inst("store i8 0, ptr %__faulted");
        self.current_disposition = Some(r.disposition);
        let is_retry = matches!(r.disposition, SirDisposition::Retry { .. });
        if is_retry {
            self.allocas.push_str("  %__retry = alloca i32\n");
            self.inst("store i32 0, ptr %__retry");
            let loop_l = self.fresh_label("retryloop");
            self.inst(&format!("br label %{}", loop_l));
            self.label(&loop_l);
            self.inst("store i8 0, ptr %__faulted ; clear before this attempt");
            self.retry_loop = Some(loop_l);
        }
        for stmt in &r.body {
            if self.terminated {
                break;
            }
            self.emit_stmt(stmt);
        }
        self.retry_loop = None;
        self.current_disposition = None;
        if !self.terminated {
            self.inst("ret void");
        }
        self.functions.push_str(&format!("define {} {{\nentry:\n", sig));
        self.functions.push_str(&self.allocas);
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
    }

    /// A poll/await timeout's Layer-2 disposition in a non-yielding reaction
    /// (terminal form, P5-3): retry (back-edge to the body), skip/escalate
    /// (return), or safe (drive safe + halt).  Emitted inside an `if(__faulted)`
    /// block, so it ends that block in a terminator without ending the caller's
    /// statement stream.  Mirrors the C `emit_disposition_terminal`.
    fn emit_terminal_disposition(&mut self, disp: SirDisposition) {
        match disp {
            SirDisposition::Retry { max } => {
                if let Some(loop_l) = self.retry_loop.clone() {
                    let r = self.fresh();
                    self.inst(&format!("{} = load i32, ptr %__retry", r));
                    let lt = self.fresh();
                    self.inst(&format!("{} = icmp ult i32 {}, {}", lt, r, max));
                    let again = self.fresh_label("again");
                    let giveup = self.fresh_label("giveup");
                    self.inst(&format!("br i1 {}, label %{}, label %{}", lt, again, giveup));
                    self.label(&again);
                    let r1 = self.fresh();
                    self.inst(&format!("{} = add i32 {}, 1", r1, r));
                    self.inst(&format!("store i32 {}, ptr %__retry", r1));
                    self.inst(&format!("br label %{}", loop_l));
                    self.label(&giveup);
                    self.inst("ret void ; retries exhausted → escalate");
                } else {
                    self.inst("ret void");
                }
            }
            SirDisposition::Skip | SirDisposition::Escalate => {
                self.inst("ret void ; skip/escalate this activation");
            }
            SirDisposition::Safe => {
                self.emit_drive_safe_and_halt("poll/await fault → safe (§5.6)");
            }
        }
    }

    // ── Metal helpers (P3-4c/P4) ──────────────────────────────────────────────

    /// `*(volatile u32 *)addr = val` via `inttoptr`.
    fn m_store32(&mut self, addr: u64, val: &str, comment: &str) {
        let p = self.fresh();
        self.inst(&format!("{} = inttoptr i64 {} to ptr", p, addr));
        self.inst(&format!("store volatile i32 {}, ptr {} ; {}", val, p, comment));
    }

    /// `*(volatile u8 *)addr = val` (for NVIC IPR priority bytes).
    fn m_store8(&mut self, addr: u64, val: u8, comment: &str) {
        let p = self.fresh();
        self.inst(&format!("{} = inttoptr i64 {} to ptr", p, addr));
        self.inst(&format!("store volatile i8 {}, ptr {} ; {}", val, p, comment));
    }

    /// A side-effecting inline-asm with a memory clobber (barriers / cps).
    fn m_asm(&mut self, asm: &str, comment: &str) {
        self.inst(&format!("call void asm sideeffect \"{}\", \"~{{memory}}\"() ; {}", asm, comment));
    }

    /// Emit a word loop `for (d = @dst_start; d < @dst_end; d += 4) *d = src? *src++ : 0`
    /// — the reset handler's `.data` copy (`src = Some`) and `.bss` zero (`None`).
    fn emit_init_loop(&mut self, dst_start: &str, dst_end: &str, src_start: Option<&str>) {
        let tag = self.fresh_label("init");
        let dst_addr = format!("%{}.d", tag);
        self.allocas.push_str(&format!("  {} = alloca ptr\n", dst_addr));
        self.inst(&format!("store ptr @{}, ptr {}", dst_start, dst_addr));
        let src_addr = src_start.map(|s| {
            let sa = format!("%{}.s", tag);
            self.allocas.push_str(&format!("  {} = alloca ptr\n", sa));
            self.inst(&format!("store ptr @{}, ptr {}", s, sa));
            sa
        });
        let cond = format!("{}.cond", tag);
        let body = format!("{}.body", tag);
        let done = format!("{}.done", tag);
        self.inst(&format!("br label %{}", cond));
        self.label(&cond);
        let dcur = self.fresh();
        self.inst(&format!("{} = load ptr, ptr {}", dcur, dst_addr));
        let c = self.fresh();
        self.inst(&format!("{} = icmp ult ptr {}, @{}", c, dcur, dst_end));
        self.inst(&format!("br i1 {}, label %{}, label %{}", c, body, done));
        self.label(&body);
        let word = if let Some(sa) = &src_addr {
            let scur = self.fresh();
            self.inst(&format!("{} = load ptr, ptr {}", scur, sa));
            let wv = self.fresh();
            self.inst(&format!("{} = load i32, ptr {}", wv, scur));
            let sn = self.fresh();
            self.inst(&format!("{} = getelementptr i32, ptr {}, i64 1", sn, scur));
            self.inst(&format!("store ptr {}, ptr {}", sn, sa));
            wv
        } else {
            "0".to_string()
        };
        self.inst(&format!("store i32 {}, ptr {}", word, dcur));
        let dn = self.fresh();
        self.inst(&format!("{} = getelementptr i32, ptr {}, i64 1", dn, dcur));
        self.inst(&format!("store ptr {}, ptr {}", dn, dst_addr));
        self.inst(&format!("br label %{}", cond));
        self.label(&done);
    }

    /// Metal entry (P3-4c + P4-1): `@Reset_Handler` does real startup — `.data`
    /// copy, `.bss` zero, output-pin directions + input pull-ups, the `sys.start`
    /// bodies, then programs TIMER1 (`every`), enables interrupts, and idles
    /// (`wfi`).  A bare-metal reset handler never returns.
    #[allow(clippy::type_complexity)]
    fn lower_reset_handler(
        &mut self,
        module: &SirModule,
        bodies: &[&[SirStmt]],
        plan: Option<&crate::backend::c::TimerPlan>,
        events: &[(usize, u64, u8, u8, Vec<usize>)],
        systick_rvr: Option<u64>,
    ) {
        self.body.clear();
        self.allocas.clear();
        self.terminated = false;
        self.ret_i32 = false;
        self.vars.retain(|_, v| v.2);
        for b in bodies {
            self.collect_locals(b);
        }

        // .data (flash LMA → RAM VMA) + .bss zero.
        self.emit_init_loop("_sdata", "_edata", Some("_sidata"));
        self.emit_init_loop("_sbss", "_ebss", None);

        // Output-pin directions (reuse the MMIO field store).
        for pin in module.pins.iter().filter(|p| p.output) {
            self.emit_reg_store(
                pin.device,
                pin.dir_reg_offset,
                pin.dir_reg_width,
                1u64 << pin.index,
                pin.index,
                SirRegAccess::Rw,
                &SirExpr::Bool(true),
            );
        }
        // Input pull-ups (nRF PIN_CNF at base + 0x700 + 4*pin = 0xC).
        for pin in module.pins.iter().filter(|p| !p.output && p.pull_up) {
            if let Some(&base) = self.device_bases.get(&pin.device) {
                self.m_store32(base + 0x700 + 4 * pin.index as u64, "12", "PIN_CNF: input pull-up");
            }
        }

        // sys.start bodies.
        for b in bodies {
            for stmt in b.iter() {
                if self.terminated {
                    break;
                }
                self.emit_stmt(stmt);
            }
        }

        // Program SysTick (SCS architectural timer, §4.5 P5-1): a 1 ms base tick
        // for now()/deadline bookkeeping, at the lowest urgency (touches no cells).
        if let Some(rvr) = systick_rvr {
            self.m_store32(0xE000_E014, &rvr.to_string(), "SYST_RVR");
            self.m_store32(0xE000_E018, "0", "SYST_CVR");
            self.m_asm("dsb 0xf", "ordering");
            self.m_store32(0xE000_E010, "7", "SYST_CSR: ENABLE|TICKINT|CLKSOURCE");
            self.m_store8(0xE000_ED23, 0xE0, "SysTick priority: lowest");
        }

        // Program TIMER1 for `every` (§4.5 P1-4) + enable its NVIC line.
        if let Some(plan) = plan {
            let max_p = module.reactions.iter().map(|r| r.priority).max().unwrap_or(0);
            self.m_store32(crate::backend::c::TIMER_BASE + 0x504, "0", "TIMER1 MODE = Timer");
            self.m_store32(crate::backend::c::TIMER_BASE + 0x508, "3", "BITMODE = 32-bit");
            self.m_store32(crate::backend::c::TIMER_BASE + 0x510, &crate::backend::c::TIMER_PRESCALER.to_string(), "PRESCALER → 1MHz");
            self.m_store32(crate::backend::c::TIMER_BASE + 0x00C, "1", "TASKS_CLEAR");
            let mut intenset = 0u64;
            for (idx, (_id, ticks)) in plan.channels.iter().enumerate() {
                self.m_store32(crate::backend::c::TIMER_BASE + 0x540 + 4 * idx as u64, &ticks.to_string(), "CC[i] = period");
                intenset |= 1u64 << (16 + idx);
            }
            self.m_store32(crate::backend::c::TIMER_BASE + 0x304, &intenset.to_string(), "INTENSET COMPARE[..]");
            let prio = crate::backend::c::timer_priority(module).map(|p| basepri_byte(p, max_p)).unwrap_or(0);
            self.m_store8(NVIC_IPR0 + crate::backend::c::TIMER_IRQN as u64, prio, "NVIC IPR TIMER1");
            self.m_store32(NVIC_ISER0, &(1u64 << crate::backend::c::TIMER_IRQN).to_string(), "NVIC ISER enable TIMER1");
            self.m_store32(crate::backend::c::TIMER_BASE, "1", "TASKS_START");
        }

        // Configure GPIOTE channels (falling edge) + NVIC for `on` events (P4-2).
        if !events.is_empty() {
            for (ch, base, pin, prio, _rs) in events {
                let cfg = GPIOTE_BASE + 0x510 + 4 * *ch as u64;
                let port = if *base == 0x5000_0300 { 1u64 } else { 0u64 };
                // CONFIG: MODE=event(1) | PSEL(pin)<<8 | PORT<<13 | POLARITY=HiToLo(2)<<16.
                let config = 1u64 | ((*pin as u64) << 8) | (port << 13) | (2u64 << 16);
                self.m_store32(cfg, &config.to_string(), "GPIOTE CONFIG[ch]");
                self.m_store32(GPIOTE_BASE + 0x304, &(1u64 << ch).to_string(), "GPIOTE INTENSET IN[ch]");
                self.m_store8(NVIC_IPR0 + GPIOTE_IRQN as u64, *prio, "NVIC IPR GPIOTE");
            }
            self.m_store32(NVIC_ISER0, &(1u64 << GPIOTE_IRQN).to_string(), "NVIC ISER enable GPIOTE");
        }

        // Configure + start the system watchdog over its declared CR/RLR/KR
        // (§5.6, P5-4): reload = the timeout (ms), CR.start begins it, a KR write
        // feeds it.  Mirrors the C startup.
        let wdt_regs: Option<(u64, u64, u64)> = module.watchdog_device.and_then(|wdt| {
            let regs = self.device_regs.get(&wdt)?;
            Some((*regs.get("CR")?, *regs.get("RLR")?, *regs.get("KR")?))
        });
        let wdt_kr = if let Some((cr, rlr, kr)) = wdt_regs {
            let timeout_ms = module.watchdog_timeout_ns.unwrap_or(0) / 1_000_000;
            self.m_store32(rlr, &timeout_ms.to_string(), "WDT RLR: reload (ms)");
            self.m_store32(cr, "1", "WDT CR: start");
            self.m_store32(kr, "43690", "WDT KR: feed (0xAAAA)");
            Some(kr)
        } else {
            None
        };

        // Enable interrupts, then idle forever.
        self.m_asm("cpsie i", "enable IRQs");
        let idle = self.fresh_label("idle");
        self.inst(&format!("br label %{}", idle));
        self.label(&idle);
        if let Some(kr) = wdt_kr {
            // Feed the watchdog ONLY on a clean return to idle (§5.6): no yielding
            // reaction mid-transaction AND no `within` deadline missed.  A hung
            // reaction never satisfies this, so the watchdog resets the system.
            let yielding: Vec<usize> =
                module.reactions.iter().filter(|r| r.yields).map(|r| r.id).collect();
            let mut cond: Option<String> = None;
            for id in &yielding {
                let s = self.fresh();
                self.inst(&format!("{} = load i32, ptr @__rf_{}_state", s, id));
                let z = self.fresh();
                self.inst(&format!("{} = icmp eq i32 {}, 0", z, s));
                cond = Some(match cond {
                    None => z,
                    Some(prev) => {
                        let a = self.fresh();
                        self.inst(&format!("{} = and i1 {}, {}", a, prev, z));
                        a
                    }
                });
            }
            if !self.deadline_ticks.is_empty() {
                let m = self.fresh();
                self.inst(&format!("{} = load i32, ptr @__deadline_missed", m));
                let notm = self.fresh();
                self.inst(&format!("{} = icmp eq i32 {}, 0", notm, m));
                cond = Some(match cond {
                    None => notm,
                    Some(prev) => {
                        let a = self.fresh();
                        self.inst(&format!("{} = and i1 {}, {}", a, prev, notm));
                        a
                    }
                });
            }
            let feed_l = self.fresh_label("feed");
            let after_l = self.fresh_label("afterfeed");
            match cond {
                Some(c) => self.inst(&format!("br i1 {}, label %{}, label %{}", c, feed_l, after_l)),
                None => self.inst(&format!("br label %{}", feed_l)),
            }
            self.label(&feed_l);
            self.m_store32(kr, "43690", "WDT KR: feed on clean idle (§5.6)");
            self.inst(&format!("br label %{}", after_l));
            self.label(&after_l);
            self.m_asm("wfi", "idle");
            self.inst(&format!("br label %{}", idle));
        } else {
            self.m_asm("wfi", "idle");
            self.inst(&format!("br label %{}", idle));
        }

        self.functions.push_str("define void @Reset_Handler() {\nentry:\n");
        self.functions.push_str(&self.allocas);
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
    }

    /// `@TIMER1_IRQHandler` (P4-1): per `every` channel, if its COMPARE event
    /// fired, clear it, re-arm `CC[i] += period`, and call the reaction fn —
    /// mirrors the C `TIMER1_IRQHandler`.
    fn emit_timer_handler(&mut self, plan: &crate::backend::c::TimerPlan) {
        self.body.clear();
        self.allocas.clear();
        let base = crate::backend::c::TIMER_BASE;
        for (idx, (id, ticks)) in plan.channels.iter().enumerate() {
            let evt = base + 0x140 + 4 * idx as u64;
            let cc = base + 0x540 + 4 * idx as u64;
            let ep = self.fresh();
            self.inst(&format!("{} = inttoptr i64 {} to ptr", ep, evt));
            let e = self.fresh();
            self.inst(&format!("{} = load volatile i32, ptr {}", e, ep));
            let nz = self.fresh();
            self.inst(&format!("{} = icmp ne i32 {}, 0", nz, e));
            let fire = self.fresh_label("fire");
            let skip = self.fresh_label("skip");
            self.inst(&format!("br i1 {}, label %{}, label %{}", nz, fire, skip));
            self.label(&fire);
            self.inst(&format!("store volatile i32 0, ptr {} ; clear EVENTS_COMPARE[{}]", ep, idx));
            self.m_asm("dsb 0xf", "ordering");
            let ccp = self.fresh();
            self.inst(&format!("{} = inttoptr i64 {} to ptr", ccp, cc));
            let cur = self.fresh();
            self.inst(&format!("{} = load volatile i32, ptr {}", cur, ccp));
            let next = self.fresh();
            self.inst(&format!("{} = add i32 {}, {}", next, cur, ticks));
            self.inst(&format!("store volatile i32 {}, ptr {} ; CC[{}] += period", next, ccp, idx));
            self.inst(&format!("call void @__reaction_{}()", id));
            self.inst(&format!("br label %{}", skip));
            self.label(&skip);
        }
        self.inst("ret void");
        self.functions.push_str("define void @TIMER1_IRQHandler() {\nentry:\n");
        self.functions.push_str(&self.allocas);
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
    }

    /// Collect `on <pin>.falling` bindings → one GPIOTE channel each (P4-2),
    /// mirroring the C event collection: `(channel, port_base, pin, BASEPRI byte,
    /// [reaction ids])`.
    #[allow(clippy::type_complexity)]
    fn events_of(&self, module: &SirModule) -> Vec<(usize, u64, u8, u8, Vec<usize>)> {
        let mut events = Vec::new();
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
            let ch = events.len();
            events.push((ch, base, ev.pin_index.unwrap_or(0), basepri_byte(prio, self.max_priority), rs));
        }
        events
    }

    /// `@GPIOTE_IRQHandler` (P4-2): per channel, if its `EVENTS_IN` fired, clear
    /// it and call the bound reaction fns — mirrors the C `GPIOTE_IRQHandler`.
    #[allow(clippy::type_complexity)]
    fn emit_gpiote_handler(&mut self, events: &[(usize, u64, u8, u8, Vec<usize>)]) {
        self.body.clear();
        self.allocas.clear();
        for (ch, _base, _pin, _prio, rs) in events {
            let evin = GPIOTE_BASE + 0x100 + 4 * *ch as u64;
            let ep = self.fresh();
            self.inst(&format!("{} = inttoptr i64 {} to ptr", ep, evin));
            let e = self.fresh();
            self.inst(&format!("{} = load volatile i32, ptr {}", e, ep));
            let nz = self.fresh();
            self.inst(&format!("{} = icmp ne i32 {}, 0", nz, e));
            let fire = self.fresh_label("gfire");
            let skip = self.fresh_label("gskip");
            self.inst(&format!("br i1 {}, label %{}, label %{}", nz, fire, skip));
            self.label(&fire);
            self.inst(&format!("store volatile i32 0, ptr {} ; clear EVENTS_IN[{}]", ep, ch));
            self.m_asm("dsb 0xf", "ordering");
            for id in rs {
                self.inst(&format!("call void @__reaction_{}()", id));
            }
            self.inst(&format!("br label %{}", skip));
            self.label(&skip);
        }
        self.inst("ret void");
        self.functions.push_str("define void @GPIOTE_IRQHandler() {\nentry:\n");
        self.functions.push_str(&self.allocas);
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
    }

    // ── Yields state machine (P4-3) ───────────────────────────────────────────

    /// Cross-yield temps of a reaction (frame globals): non-cell `Assign` targets,
    /// `BusXfer` dst/code_dst, `RingPop` dst — recursing into `If`/`Critical`.
    fn collect_frame_temps(&self, reaction: &SirReaction) -> Vec<String> {
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        self.collect_frame_temps_in(&reaction.body, &mut seen, &mut out);
        out
    }
    fn collect_frame_temps_in(&self, stmts: &[SirStmt], seen: &mut HashSet<String>, out: &mut Vec<String>) {
        let add = |name: &str, seen: &mut HashSet<String>, out: &mut Vec<String>| {
            if !self.cells.contains(name) && seen.insert(name.to_string()) {
                out.push(name.to_string());
            }
        };
        for stmt in stmts {
            match stmt {
                SirStmt::Assign { target: SirPlace::Var(name), .. } => add(name, seen, out),
                SirStmt::RingPop { dst, .. } => add(dst, seen, out),
                SirStmt::BusXfer { dst, code_dst, .. } => {
                    add(dst, seen, out);
                    if let Some(c) = code_dst {
                        add(c, seen, out);
                    }
                }
                SirStmt::If { then, .. } => self.collect_frame_temps_in(then, seen, out),
                SirStmt::Critical { body, .. } => self.collect_frame_temps_in(body, seen, out),
                _ => {}
            }
        }
    }

    /// Resolve a bus controller's CR/SR/SA/RA/DR absolute addresses by name.
    fn bus_regs(&self, device: usize) -> Option<(u64, u64, u64, u64, u64)> {
        let regs = self.device_regs.get(&device)?;
        Some((*regs.get("CR")?, *regs.get("SR")?, *regs.get("SA")?, *regs.get("RA")?, *regs.get("DR")?))
    }

    /// A yielding reaction (P4-3) → an IRQ-driven segment state machine
    /// (`@__react_N_run`) + a coalescing trigger entry (`@__reaction_N`),
    /// mirroring the C `emit_yielding_reaction_metal`.
    fn emit_yielding_metal(&mut self, reaction: &SirReaction) {
        let n = reaction.id;
        let prefix = format!("__rf_{}_", n);

        // Segment the body at each top-level suspend point — a BusXfer (resumed by
        // the bus IRQ) or an `await` (resumed by SysTick, P6-5).
        let mut segs: Vec<(Vec<&SirStmt>, Option<&SirStmt>)> = Vec::new();
        let mut cur: Vec<&SirStmt> = Vec::new();
        for stmt in &reaction.body {
            if matches!(stmt, SirStmt::BusXfer { .. } | SirStmt::Await { .. }) {
                segs.push((std::mem::take(&mut cur), Some(stmt)));
            } else {
                cur.push(stmt);
            }
        }
        segs.push((cur, None));

        let has_await = crate::backend::c::body_has_await(&reaction.body);

        // Frame globals: dispatcher state + retry/fault + every cross-yield temp.
        // `@__rf_N_await`/`_await_deadline` (P6-5): an await-suspended frame is
        // resumed by SysTick (not the bus IRQ) and counts its `within` down.
        let temps = self.collect_frame_temps(reaction);
        self.globals.push_str(&format!("@{p}state = global i32 0\n@{p}retry = global i32 0\n@{p}faulted = global i32 0\n", p = prefix));
        if has_await {
            self.globals.push_str(&format!("@{p}await = global i32 0\n@{p}await_deadline = global i32 0\n", p = prefix));
        }
        for t in &temps {
            self.globals.push_str(&format!("@{}{} = global i32 0\n", prefix, t));
            self.vars.insert(t.clone(), (32, false, true));
        }
        let temp_set: HashSet<String> = temps.iter().cloned().collect();
        self.frame = Some((prefix.clone(), temp_set));

        // Dispatcher body.
        self.body.clear();
        self.allocas.clear();
        self.terminated = false;
        self.ret_i32 = false;
        let state_g = format!("@{}state", prefix);
        let nseg = segs.len();
        let seg0_reset = self.fresh_label("seg0reset");
        let default_l = self.fresh_label("bdefault");
        let body_labels: Vec<String> = (0..nseg).map(|i| self.fresh_label(&format!("seg{}_", i))).collect();
        let st = self.fresh();
        self.inst(&format!("{} = load i32, ptr {}", st, state_g));
        let mut arms = vec![format!("i32 0, label %{}", seg0_reset)];
        for (i, lbl) in body_labels.iter().enumerate().skip(1) {
            arms.push(format!("i32 {}, label %{}", i, lbl));
        }
        self.inst(&format!("switch i32 {}, label %{} [ {} ]", st, default_l, arms.join(" ")));
        // case 0: reset retry/fault, then enter segment 0.
        self.label(&seg0_reset);
        self.inst(&format!("store i32 0, ptr @{}retry", prefix));
        self.inst(&format!("store i32 0, ptr @{}faulted", prefix));
        self.inst(&format!("br label %{}", body_labels[0]));

        for (i, (pre, xfer)) in segs.iter().enumerate() {
            self.label(&body_labels[i]);
            self.terminated = false;
            // Resume an `await` (P6-5): re-evaluate the condition — resume on
            // success, fault on the elapsed `within`, else stay suspended.
            if i >= 1 {
                if let Some(SirStmt::Await { cond, .. }) = segs[i - 1].1 {
                    let c = self.emit_expr(cond, 1);
                    let resolved = self.fresh_label("awok");
                    let notyet = self.fresh_label("awwait");
                    let timeout = self.fresh_label("awto");
                    let still = self.fresh_label("awstill");
                    let bodyl = self.fresh_label("awbody");
                    self.inst(&format!("br i1 {}, label %{}, label %{}", c, resolved, notyet));
                    self.label(&resolved);
                    self.inst(&format!("store i32 0, ptr @{}await", prefix));
                    self.inst(&format!("br label %{}", bodyl));
                    self.label(&notyet);
                    let dl = self.fresh();
                    self.inst(&format!("{} = load i32, ptr @{}await_deadline", dl, prefix));
                    let z = self.fresh();
                    self.inst(&format!("{} = icmp eq i32 {}, 0", z, dl));
                    self.inst(&format!("br i1 {}, label %{}, label %{}", z, timeout, still));
                    self.label(&timeout);
                    self.inst(&format!("store i32 1, ptr @{}faulted", prefix));
                    self.inst(&format!("store i32 0, ptr @{}await", prefix));
                    self.emit_disposition(reaction.disposition, &prefix, &body_labels[0]);
                    self.label(&still);
                    self.inst("ret void");
                    self.label(&bodyl);
                }
                if let Some(SirStmt::BusXfer { device, op, dst, propagate, code_dst, fault_codes, .. }) = segs[i - 1].1 {
                    let dref = self.var_ptr(dst, true);
                    if let Some(code) = code_dst {
                        let cref = self.var_ptr(code, true);
                        self.emit_bus_resume_match(*device, op, &dref, &cref, fault_codes, &prefix);
                    } else {
                        self.emit_bus_resume(*device, op, &dref, &prefix);
                        if *propagate {
                            let f = self.fresh();
                            self.inst(&format!("{} = load i32, ptr @{}faulted", f, prefix));
                            let nz = self.fresh();
                            self.inst(&format!("{} = icmp ne i32 {}, 0", nz, f));
                            let disp = self.fresh_label("disp");
                            let cont = self.fresh_label("dcont");
                            self.inst(&format!("br i1 {}, label %{}, label %{}", nz, disp, cont));
                            self.label(&disp);
                            self.emit_disposition(reaction.disposition, &prefix, &body_labels[0]);
                            self.label(&cont);
                        }
                    }
                }
            }
            for &stmt in pre.iter() {
                if self.terminated {
                    break;
                }
                self.emit_stmt(stmt);
            }
            // Terminate: kick the next transaction (suspend on bus), arm the await
            // (suspend until SysTick), or complete (tail).
            if let Some(SirStmt::BusXfer { device, op, args, .. }) = xfer {
                self.emit_bus_kick(*device, op, args, n);
                self.inst(&format!("store i32 {}, ptr @{}state", i + 1, prefix));
                self.inst("ret void");
            } else if let Some(SirStmt::Await { within_ns, .. }) = xfer {
                let ticks = within_ns.div_ceil(1_000_000).max(1);
                self.inst(&format!("store i32 1, ptr @{}await", prefix));
                self.inst(&format!("store i32 {}, ptr @{}await_deadline", ticks, prefix));
                self.inst(&format!("store i32 {}, ptr @{}state", i + 1, prefix));
                self.inst("ret void");
            } else {
                self.inst(&format!("store i32 0, ptr @{}state", prefix));
                self.inst("ret void");
            }
        }
        self.label(&default_l);
        self.inst("ret void");
        self.functions.push_str(&format!("define void @__react_{}_run() {{\nentry:\n", n));
        self.functions.push_str(&self.allocas);
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
        self.frame = None;

        // Trigger entry: coalesce a re-fire that arrives while in flight (§5.1).
        self.body.clear();
        self.allocas.clear();
        let s2 = self.fresh();
        self.inst(&format!("{} = load i32, ptr {}", s2, state_g));
        let busy = self.fresh();
        self.inst(&format!("{} = icmp ne i32 {}, 0", busy, s2));
        let inflight = self.fresh_label("inflight");
        let fire = self.fresh_label("gofire");
        self.inst(&format!("br i1 {}, label %{}, label %{}", busy, inflight, fire));
        self.label(&inflight);
        self.inst("ret void");
        self.label(&fire);
        // Arm this activation's `within` deadline (P5-4): if it is still in flight
        // when the SysTick countdown elapses, it overran (§4.5/§5.6).
        if let Some(&ticks) = self.deadline_ticks.get(&n) {
            self.inst(&format!("store i32 {}, ptr @__deadline_{} ; arm `within` deadline", ticks, n));
        }
        self.inst(&format!("call void @__react_{}_run()", n));
        self.inst("ret void");
        self.functions.push_str(&format!("define void @__reaction_{}() {{\nentry:\n", n));
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
    }

    /// Bus kick: write SA/RA/DR, barrier, CR start, own the bus, clear-pending +
    /// enable the completion IRQ (the caller then sets state and returns).
    fn emit_bus_kick(&mut self, device: usize, op: &str, args: &[SirExpr], n: usize) {
        let Some((cr, _sr, sa, ra, dr)) = self.bus_regs(device) else {
            self.inst(&format!("; unsupported: bus device {} missing CR/SR/SA/RA/DR", device));
            return;
        };
        let is_read = op == "read_reg";
        let a0 = args.first().map(|a| self.emit_expr(a, 32)).unwrap_or_else(|| "0".into());
        let a1 = args.get(1).map(|a| self.emit_expr(a, 32)).unwrap_or_else(|| "0".into());
        let a2 = args.get(2).map(|a| self.emit_expr(a, 32));
        let p_sa = self.fresh();
        self.inst(&format!("{} = inttoptr i64 {} to ptr", p_sa, sa));
        self.inst(&format!("store volatile i32 {}, ptr {} ; SA", a0, p_sa));
        let p_ra = self.fresh();
        self.inst(&format!("{} = inttoptr i64 {} to ptr", p_ra, ra));
        self.inst(&format!("store volatile i32 {}, ptr {} ; RA", a1, p_ra));
        if let Some(v) = a2 {
            let p_dr = self.fresh();
            self.inst(&format!("{} = inttoptr i64 {} to ptr", p_dr, dr));
            self.inst(&format!("store volatile i32 {}, ptr {} ; DR (write)", v, p_dr));
        }
        self.m_asm("dmb 0xf", "arm: operands before CR kick");
        let kick = if is_read { 3 } else { 1 }; // START | (DIR_RD for read)
        let p_cr = self.fresh();
        self.inst(&format!("{} = inttoptr i64 {} to ptr", p_cr, cr));
        self.inst(&format!("store volatile i32 {}, ptr {} ; CR kick", kick, p_cr));
        self.m_asm("dsb 0xf", "kick committed before IRQ enable");
        self.inst(&format!("store i32 {}, ptr @__bus_owner", n));
        self.m_store32(0xE000_E280, &(1u64 << BUS_IRQN).to_string(), "NVIC ICPR clear stale bus pending");
        self.m_store32(NVIC_ISER0, &(1u64 << BUS_IRQN).to_string(), "NVIC ISER enable bus IRQ");
    }

    /// Bus resume: read SR; on done+!err set `dst` (DR for reads, else 0), else
    /// flag `faulted`.  `dst_ptr` is the frame-global pointer operand.
    fn emit_bus_resume(&mut self, device: usize, op: &str, dst_ptr: &str, prefix: &str) {
        let Some((_cr, sr, _sa, _ra, dr)) = self.bus_regs(device) else {
            self.inst(&format!("; unsupported: bus device {} missing regs", device));
            return;
        };
        let is_read = op == "read_reg";
        let psr = self.fresh();
        self.inst(&format!("{} = inttoptr i64 {} to ptr", psr, sr));
        let srv = self.fresh();
        self.inst(&format!("{} = load volatile i32, ptr {} ; SR", srv, psr));
        let done = self.fresh();
        self.inst(&format!("{} = and i32 {}, 1", done, srv)); // SR_DONE
        let err = self.fresh();
        self.inst(&format!("{} = and i32 {}, 14", err, srv)); // SR_ERR (0xE)
        let d1 = self.fresh();
        self.inst(&format!("{} = icmp ne i32 {}, 0", d1, done));
        let e0 = self.fresh();
        self.inst(&format!("{} = icmp eq i32 {}, 0", e0, err));
        let ok = self.fresh();
        self.inst(&format!("{} = and i1 {}, {}", ok, d1, e0));
        let okl = self.fresh_label("rok");
        let badl = self.fresh_label("rbad");
        let contl = self.fresh_label("rcont");
        self.inst(&format!("br i1 {}, label %{}, label %{}", ok, okl, badl));
        self.label(&okl);
        if is_read {
            let pdr = self.fresh();
            self.inst(&format!("{} = inttoptr i64 {} to ptr", pdr, dr));
            let v = self.fresh();
            self.inst(&format!("{} = load volatile i32, ptr {} ; DR", v, pdr));
            self.inst(&format!("store i32 {}, ptr {}", v, dst_ptr));
        } else {
            self.inst(&format!("store i32 0, ptr {}", dst_ptr));
        }
        self.inst(&format!("br label %{}", contl));
        self.label(&badl);
        self.inst(&format!("store i32 1, ptr @{}faulted", prefix));
        self.inst(&format!("br label %{}", contl));
        self.label(&contl);
    }

    /// Bus resume for a `match` over the result (§4.4/D14): decode the outcome
    /// code (0 = ok; 1+i = the i-th declared fault by its SR bit) into `code_ptr`.
    fn emit_bus_resume_match(&mut self, device: usize, op: &str, dst_ptr: &str, code_ptr: &str, fault_codes: &[String], _prefix: &str) {
        let Some((_cr, sr, _sa, _ra, dr)) = self.bus_regs(device) else {
            self.inst(&format!("; unsupported: bus device {} missing regs", device));
            return;
        };
        let is_read = op == "read_reg";
        let psr = self.fresh();
        self.inst(&format!("{} = inttoptr i64 {} to ptr", psr, sr));
        let srv = self.fresh();
        self.inst(&format!("{} = load volatile i32, ptr {} ; SR", srv, psr));
        let done = self.fresh();
        self.inst(&format!("{} = and i32 {}, 1", done, srv));
        let err = self.fresh();
        self.inst(&format!("{} = and i32 {}, 14", err, srv));
        let d1 = self.fresh();
        self.inst(&format!("{} = icmp ne i32 {}, 0", d1, done));
        let e0 = self.fresh();
        self.inst(&format!("{} = icmp eq i32 {}, 0", e0, err));
        let ok = self.fresh();
        self.inst(&format!("{} = and i1 {}, {}", ok, d1, e0));
        let okl = self.fresh_label("mok");
        let chkl = self.fresh_label("mchk");
        let contl = self.fresh_label("mcont");
        self.inst(&format!("br i1 {}, label %{}, label %{}", ok, okl, chkl));
        self.label(&okl);
        if is_read {
            let pdr = self.fresh();
            self.inst(&format!("{} = inttoptr i64 {} to ptr", pdr, dr));
            let v = self.fresh();
            self.inst(&format!("{} = load volatile i32, ptr {} ; DR", v, pdr));
            self.inst(&format!("store i32 {}, ptr {}", v, dst_ptr));
        } else {
            self.inst(&format!("store i32 0, ptr {}", dst_ptr));
        }
        self.inst(&format!("store i32 0, ptr {} ; ok", code_ptr));
        self.inst(&format!("br label %{}", contl));
        // Decode each declared fault code by its SR bit (else 0xFFFFFFFF → `_`).
        self.label(&chkl);
        self.inst(&format!("store i32 0, ptr {}", dst_ptr));
        for (i, fc) in fault_codes.iter().enumerate() {
            let Some(bit) = crate::backend::c::i2c_fault_bit(fc) else { continue };
            let masked = self.fresh();
            self.inst(&format!("{} = and i32 {}, {}", masked, srv, bit));
            let hit = self.fresh();
            self.inst(&format!("{} = icmp ne i32 {}, 0", hit, masked));
            let setl = self.fresh_label("mset");
            let nextl = self.fresh_label("mnext");
            self.inst(&format!("br i1 {}, label %{}, label %{}", hit, setl, nextl));
            self.label(&setl);
            self.inst(&format!("store i32 {}, ptr {} ; fault {}", i + 1, code_ptr, fc));
            self.inst(&format!("br label %{}", contl));
            self.label(&nextl);
        }
        // No known bit matched → unknown (falls to the `_` arm).
        self.inst(&format!("store i32 4294967295, ptr {} ; unknown", code_ptr));
        self.inst(&format!("br label %{}", contl));
        self.label(&contl);
    }

    /// A propagated fault's Layer-2 disposition at a resumed segment (frame form):
    /// retry (back-edge to seg-0), skip/escalate (complete), or safe (halt).
    fn emit_disposition(&mut self, disp: SirDisposition, prefix: &str, seg0: &str) {
        match disp {
            SirDisposition::Retry { max } => {
                let r = self.fresh();
                self.inst(&format!("{} = load i32, ptr @{}retry", r, prefix));
                let lt = self.fresh();
                self.inst(&format!("{} = icmp ult i32 {}, {}", lt, r, max));
                let retry = self.fresh_label("retry");
                let exhaust = self.fresh_label("exhaust");
                self.inst(&format!("br i1 {}, label %{}, label %{}", lt, retry, exhaust));
                self.label(&retry);
                let r1 = self.fresh();
                self.inst(&format!("{} = add i32 {}, 1", r1, r));
                self.inst(&format!("store i32 {}, ptr @{}retry", r1, prefix));
                self.inst(&format!("store i32 0, ptr @{}faulted", prefix));
                self.inst(&format!("br label %{}", seg0));
                self.label(&exhaust);
                self.inst(&format!("store i32 0, ptr @{}state", prefix));
                self.inst("ret void");
            }
            SirDisposition::Skip | SirDisposition::Escalate => {
                self.inst(&format!("store i32 0, ptr @{}state", prefix));
                self.inst("ret void");
            }
            SirDisposition::Safe => {
                // Drive every device to its safe state, then hold (P5-2).
                self.emit_drive_safe_and_halt("safe disposition → halt (§5.6)");
            }
        }
    }

    /// `@__BUS_IRQHandler` (P4-3): disable the bus IRQ, then resume the in-flight
    /// owner's dispatcher (single owner, §5.1/§5.2).
    fn emit_bus_irq_handler(&mut self, bus_reactions: &[usize]) {
        self.body.clear();
        self.allocas.clear();
        self.m_store32(0xE000_E180, &(1u64 << BUS_IRQN).to_string(), "NVIC ICER disable bus IRQ");
        self.m_asm("dsb 0xf", "disable takes effect");
        self.m_asm("isb 0xf", "before proceeding");
        let o = self.fresh();
        self.inst(&format!("{} = load i32, ptr @__bus_owner", o));
        self.inst("store i32 -1, ptr @__bus_owner");
        let done = self.fresh_label("busdone");
        let arms: Vec<String> = bus_reactions
            .iter()
            .map(|id| format!("i32 {}, label %call{}", id, id))
            .collect();
        self.inst(&format!("switch i32 {}, label %{} [ {} ]", o, done, arms.join(" ")));
        for id in bus_reactions {
            self.label(&format!("call{}", id));
            self.inst(&format!("call void @__react_{}_run()", id));
            self.inst(&format!("br label %{}", done));
        }
        self.label(&done);
        self.inst("ret void");
        self.functions.push_str("define void @__BUS_IRQHandler() {\nentry:\n");
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
    }

    /// `@SysTick_Handler` (P5-1): the 1 ms base tick.  When `now()` is used it
    /// advances `@__uptime_ns` by 1 ms; P5-4 extends it with `within`-deadline
    /// countdowns.  Mirrors the C `SysTick_Handler`.
    fn emit_systick_handler(&mut self, uses_now: bool, await_ids: &[usize]) {
        self.body.clear();
        self.allocas.clear();
        if uses_now {
            let cur = self.fresh();
            self.inst(&format!("{} = load volatile i64, ptr @__uptime_ns", cur));
            let next = self.fresh();
            self.inst(&format!("{} = add i64 {}, 1000000", next, cur));
            self.inst(&format!("store volatile i64 {}, ptr @__uptime_ns ; +1ms uptime", next));
        }
        // §4.5/§5.6 (P5-4) — tick down each armed `within` deadline.  A reaction
        // back at idle (frame state 0) disarms; one still in flight when its
        // countdown hits 0 overran → latch `@__deadline_missed` (stops the feed).
        let mut ids: Vec<usize> = self.deadline_ticks.keys().copied().collect();
        ids.sort_unstable();
        for id in ids {
            let state = self.fresh();
            self.inst(&format!("{} = load i32, ptr @__rf_{}_state", state, id));
            let idle = self.fresh();
            self.inst(&format!("{} = icmp eq i32 {}, 0", idle, state));
            let disarm = self.fresh_label("disarm");
            let active = self.fresh_label("active");
            let cont = self.fresh_label("dlcont");
            self.inst(&format!("br i1 {}, label %{}, label %{}", idle, disarm, active));
            self.label(&disarm);
            self.inst(&format!("store i32 0, ptr @__deadline_{}", id));
            self.inst(&format!("br label %{}", cont));
            self.label(&active);
            let d = self.fresh();
            self.inst(&format!("{} = load i32, ptr @__deadline_{}", d, id));
            let armed = self.fresh();
            self.inst(&format!("{} = icmp ne i32 {}, 0", armed, d));
            let tick = self.fresh_label("dltick");
            self.inst(&format!("br i1 {}, label %{}, label %{}", armed, tick, cont));
            self.label(&tick);
            let d1 = self.fresh();
            self.inst(&format!("{} = sub i32 {}, 1", d1, d));
            self.inst(&format!("store i32 {}, ptr @__deadline_{}", d1, id));
            let elapsed = self.fresh();
            self.inst(&format!("{} = icmp eq i32 {}, 0", elapsed, d1));
            let miss = self.fresh_label("dlmiss");
            self.inst(&format!("br i1 {}, label %{}, label %{}", elapsed, miss, cont));
            self.label(&miss);
            self.inst("store i32 1, ptr @__deadline_missed ; `within` overrun → stop the feed");
            self.inst(&format!("br label %{}", cont));
            self.label(&cont);
        }
        // §5.2 (P6-5) — re-check each await suspended on this 1ms tick: count its
        // `within` budget down, then re-enter the dispatcher (resume / fault / wait).
        for id in await_ids {
            let aw = self.fresh();
            self.inst(&format!("{} = load i32, ptr @__rf_{}_await", aw, id));
            let susp = self.fresh();
            self.inst(&format!("{} = icmp ne i32 {}, 0", susp, aw));
            let chk = self.fresh_label("awchk");
            let acont = self.fresh_label("awcont");
            self.inst(&format!("br i1 {}, label %{}, label %{}", susp, chk, acont));
            self.label(&chk);
            // Decrement the await deadline (saturating at 0), then re-enter.
            let d = self.fresh();
            self.inst(&format!("{} = load i32, ptr @__rf_{}_await_deadline", d, id));
            let nz = self.fresh();
            self.inst(&format!("{} = icmp ne i32 {}, 0", nz, d));
            let dec = self.fresh_label("awdec");
            let run = self.fresh_label("awrun");
            self.inst(&format!("br i1 {}, label %{}, label %{}", nz, dec, run));
            self.label(&dec);
            let d1 = self.fresh();
            self.inst(&format!("{} = sub i32 {}, 1", d1, d));
            self.inst(&format!("store i32 {}, ptr @__rf_{}_await_deadline", d1, id));
            self.inst(&format!("br label %{}", run));
            self.label(&run);
            self.inst(&format!("call void @__react_{}_run()", id));
            self.inst(&format!("br label %{}", acont));
            self.label(&acont);
        }
        self.inst("ret void");
        self.functions.push_str("define void @SysTick_Handler() {\nentry:\n");
        self.functions.push_str(&self.allocas);
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
    }

    /// `@__default_handler`: the bare infinite-loop stub for unused vectors.
    /// (`@HardFault_Handler` is the Layer-3 fault decoder — `emit_fault_decoder`.)
    fn emit_default_handlers(&mut self) {
        self.functions.push_str(
            "define void @__default_handler() {\nentry:\n  br label %loop\nloop:\n  call void asm sideeffect \"wfi\", \"~{memory}\"()\n  br label %loop\n}\n\n",
        );
    }

    /// The Layer-3 fault decoder (§5.4, P6-3): an address-ownership table + a
    /// `@HardFault_Handler` that reads SCB CFSR/BFAR, finds the owning region, and
    /// records `{addr, owner-index, cfsr, pending}` to fixed RAM (read back by the
    /// host decoder).  Mirrors the C `emit_fault_decoder` (no on-device strings —
    /// the host renders labels from indices).
    fn emit_fault_decoder(&mut self, module: &SirModule) {
        let owners = crate::layer3::ownership_map(module);
        // Owner tables (constants) + the fault record (external-linkage globals so
        // `nm` finds them) — index → label is a comment for the host decoder.
        if owners.is_empty() {
            self.globals.push_str("@__owner_start = constant [1 x i32] [i32 0]\n@__owner_end = constant [1 x i32] [i32 0]\n");
        } else {
            let starts: Vec<String> = owners.iter().map(|r| format!("i32 {}", r.start as u32)).collect();
            let ends: Vec<String> = owners.iter().map(|r| format!("i32 {}", r.end as u32)).collect();
            self.globals.push_str(&format!(
                "@__owner_start = constant [{n} x i32] [{s}]\n@__owner_end = constant [{n} x i32] [{e}]\n",
                n = owners.len(), s = starts.join(", "), e = ends.join(", ")
            ));
            for (i, r) in owners.iter().enumerate() {
                self.globals.push_str(&format!("; owner[{}] = {} [0x{:08x}, 0x{:08x})\n", i, r.label, r.start, r.end));
            }
        }
        self.globals.push_str(
            "@__fault_addr = global i32 0\n@__fault_owner = global i32 -1\n@__fault_cfsr = global i32 0\n@__fault_pending = global i32 0\n",
        );

        self.body.clear();
        self.allocas.clear();
        // cfsr = *SCB_CFSR (0xE000ED28); record it.
        let cp = self.fresh();
        self.inst(&format!("{} = inttoptr i64 3758157096 to ptr", cp));
        let cfsr = self.fresh();
        self.inst(&format!("{} = load volatile i32, ptr {}", cfsr, cp));
        self.inst(&format!("store volatile i32 {}, ptr @__fault_cfsr", cfsr));
        // BFAR is valid only when CFSR.BFARVALID (bit 15) is set.
        let bv = self.fresh();
        self.inst(&format!("{} = and i32 {}, 32768", bv, cfsr));
        let valid = self.fresh();
        self.inst(&format!("{} = icmp ne i32 {}, 0", valid, bv));
        let hasaddr = self.fresh_label("hasaddr");
        let record = self.fresh_label("record");
        self.inst(&format!("br i1 {}, label %{}, label %{}", valid, hasaddr, record));
        self.label(&hasaddr);
        let bp = self.fresh();
        self.inst(&format!("{} = inttoptr i64 3758157112 to ptr", bp));
        let a = self.fresh();
        self.inst(&format!("{} = load volatile i32, ptr {}", a, bp));
        self.inst(&format!("store volatile i32 {}, ptr @__fault_addr", a));
        // Find the owning region (regions are non-overlapping; first/only match).
        for (i, r) in owners.iter().enumerate() {
            let ge = self.fresh();
            self.inst(&format!("{} = icmp uge i32 {}, {}", ge, a, r.start as u32));
            let lt = self.fresh();
            self.inst(&format!("{} = icmp ult i32 {}, {}", lt, a, r.end as u32));
            let inr = self.fresh();
            self.inst(&format!("{} = and i1 {}, {}", inr, ge, lt));
            let own = self.fresh_label("own");
            let next = self.fresh_label("nextown");
            self.inst(&format!("br i1 {}, label %{}, label %{}", inr, own, next));
            self.label(&own);
            self.inst(&format!("store volatile i32 {}, ptr @__fault_owner", i));
            self.inst(&format!("br label %{}", next));
            self.label(&next);
        }
        self.inst(&format!("br label %{}", record));
        self.label(&record);
        self.inst("store volatile i32 1, ptr @__fault_pending");
        let halt = self.fresh_label("fhalt");
        self.inst(&format!("br label %{}", halt));
        self.label(&halt);
        self.m_asm("wfi", "halt; safe-state drive is a later phase (§5.6)");
        self.inst(&format!("br label %{}", halt));
        self.functions.push_str("define void @HardFault_Handler() {\nentry:\n");
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
    }

    /// The Cortex-M vector table (P4-1): system slots + external IRQ slots up to
    /// the highest used line (index `16 + irq`).  Mirrors the C vector emission.
    fn emit_vector_table(&self, has_timer: bool, has_gpiote: bool, has_bus: bool, needs_systick: bool) -> String {
        let mut e: Vec<String> = vec![
            "ptr @_estack".into(),         // 0 SP
            "ptr @Reset_Handler".into(),   // 1 reset
            "ptr @__default_handler".into(), // 2 NMI
            "ptr @HardFault_Handler".into(), // 3 HardFault
        ];
        for _ in 4..=10 {
            e.push("ptr null".into());
        }
        e.push("ptr @__default_handler".into()); // 11 SVCall
        e.push("ptr null".into()); // 12
        e.push("ptr null".into()); // 13
        e.push("ptr @__default_handler".into()); // 14 PendSV
        e.push(if needs_systick {
            "ptr @SysTick_Handler".into() // 15 SysTick (P5-1: now()/deadline tick)
        } else {
            "ptr @__default_handler".into() // 15 SysTick (unused)
        });
        let max_irq = [
            has_gpiote.then_some(GPIOTE_IRQN),
            has_bus.then_some(BUS_IRQN),
            has_timer.then_some(crate::backend::c::TIMER_IRQN),
        ]
        .into_iter()
        .flatten()
        .max();
        if let Some(maxq) = max_irq {
            for irq in 0..=maxq {
                let sym = if irq == GPIOTE_IRQN && has_gpiote {
                    "@GPIOTE_IRQHandler"
                } else if irq == crate::backend::c::TIMER_IRQN && has_timer {
                    "@TIMER1_IRQHandler"
                } else if irq == BUS_IRQN && has_bus {
                    "@__BUS_IRQHandler"
                } else {
                    "@__default_handler"
                };
                e.push(format!("ptr {}", sym));
            }
        }
        let n = e.len();
        format!(
            "@__vectors = constant [{} x ptr] [\n  {}\n], section \".vectors\", align 4\n",
            n,
            e.join(",\n  ")
        )
    }

    /// Walk a body and `alloca` any Assign-target that is not a known global.
    fn collect_locals(&mut self, body: &[SirStmt]) {
        for stmt in body {
            match stmt {
                SirStmt::Assign { target: SirPlace::Var(name), value } => {
                    if !self.vars.contains_key(name) {
                        let bits = store_bits(self.natural_bits(value));
                        self.vars.insert(name.clone(), (bits, false, false));
                        self.allocas
                            .push_str(&format!("  %{}.addr = alloca i{}\n", name, bits));
                    }
                }
                // `let v = ring.pop()` (P6-1): the destination is a reaction-local
                // that no Assign declares, so allocate it here (matches the C
                // backend collecting RingPop dst as a reaction temp).
                SirStmt::RingPop { dst, .. } => {
                    if !self.vars.contains_key(dst) {
                        self.vars.insert(dst.clone(), (32, false, false));
                        self.allocas.push_str(&format!("  %{}.addr = alloca i32\n", dst));
                    }
                }
                // Recurse into nested bodies so a `let`/pop inside an `if`/critical
                // still gets an alloca (mirrors the C reaction-temp collection).
                SirStmt::If { then, .. } => self.collect_locals(then),
                SirStmt::Critical { body, .. } => self.collect_locals(body),
                _ => {}
            }
        }
    }

    // ── Statements ──────────────────────────────────────────────────────────

    fn emit_stmt(&mut self, stmt: &SirStmt) {
        match stmt {
            SirStmt::Assign { target: SirPlace::Var(name), value } => {
                let (bits, _signed, is_global) = self.vars[name];
                let v = self.emit_expr(value, bits);
                let ptr = self.var_ptr(name, is_global);
                self.inst(&format!("store i{} {}, ptr {}", bits, v, ptr));
            }
            // MMIO register field write (P3-4b) → a `volatile` store at the
            // absolute address; a read/write field is a read-modify-write.
            SirStmt::Assign {
                target: SirPlace::Reg { device, reg_offset, width, field_mask, field_shift, access },
                value,
            } => {
                self.emit_reg_store(*device, *reg_offset, *width, *field_mask, *field_shift, *access, value);
            }
            // Multi-field single write (§4.2 P0-2c) → ONE ordered volatile store —
            // used by safe sequences (`@__drive_safe`) and device ops (P5-2).
            SirStmt::RegWrite { device, reg_offset, width, writes } => {
                self.emit_reg_write_multi(*device, *reg_offset, *width, writes);
            }
            // Bounded ring push/pop (§5.3, P6-1) → backing-array + index math,
            // mirroring the C ring lowering (overwrite-oldest on full; 0 if empty).
            SirStmt::RingPush { ring, value } => self.emit_ring_push(ring, value),
            SirStmt::RingPop { ring, dst } => self.emit_ring_pop(ring, dst),
            // §4.1/D07 runtime typestate guard failed (P3-3): drive the system to
            // its safe state and halt (P5-2).  Host has no safe sequence — it is a
            // signposted system-integrity fault.
            SirStmt::DriveSafe => {
                if self.target == Target::MetalNrf52840 {
                    self.emit_drive_safe_and_halt("typestate guard → halt (§4.1/D07)");
                    self.terminated = true;
                } else {
                    self.inst("; drive_safe (host): system-integrity fault (§4.1/D07)");
                }
            }
            SirStmt::Exit(code) => {
                if self.ret_i32 {
                    let v = self.emit_expr(code, 32);
                    self.inst(&format!("ret i32 {}", v));
                } else {
                    // `exit()` outside `@main` has no process to end here — the
                    // reaction function just returns (a full scheduler is P3-4c).
                    self.inst("; exit() in a reaction function → return");
                    self.inst("ret void");
                }
                self.terminated = true;
            }
            // `if <cond> { <then> }` — branch over the then-block (§ control flow).
            SirStmt::If { cond, then } => {
                let c = self.emit_expr(cond, 1);
                let then_l = self.fresh_label("then");
                let end_l = self.fresh_label("endif");
                self.inst(&format!("br i1 {}, label %{}, label %{}", c, then_l, end_l));
                self.label(&then_l);
                self.terminated = false;
                for s in then {
                    if self.terminated {
                        break;
                    }
                    self.emit_stmt(s);
                }
                // Close the then-block (unless it already ended in a terminator).
                if !self.terminated {
                    self.inst(&format!("br label %{}", end_l));
                }
                // The end block is always reachable (the false edge), so the
                // continuation is live regardless of the then-block.
                self.label(&end_l);
                self.terminated = false;
            }
            SirStmt::Intrinsic(intr) => self.emit_intrinsic(intr),
            // Priority-ceiling critical section (§5.5).  On metal (P4-2): raise
            // BASEPRI to the ceiling so no cell-sharing reaction can preempt the
            // access, then restore.  On host it is a single thread — inline.
            SirStmt::Critical { ceiling, body } => {
                let metal = self.target == Target::MetalNrf52840;
                let saved = if metal {
                    let bp = basepri_byte(*ceiling, self.max_priority);
                    let s = self.fresh();
                    self.inst(&format!("{} = call i32 asm sideeffect \"mrs $0, basepri\", \"=r\"()", s));
                    self.inst(&format!("call void asm sideeffect \"msr basepri, $0\", \"r,~{{memory}}\"(i32 {})", bp));
                    self.m_asm("isb 0xf", "ceiling live before access (§5.5)");
                    Some(s)
                } else {
                    None
                };
                for s in body {
                    if self.terminated {
                        break;
                    }
                    self.emit_stmt(s);
                }
                if let Some(s) = saved {
                    self.m_asm("dmb 0xf", "order protected writes before lowering the mask");
                    self.inst(&format!("call void asm sideeffect \"msr basepri, $0\", \"r,~{{memory}}\"(i32 {})", s));
                }
            }
            // Bounded busy-wait (§3.2, P5-3): spin until `cond`, else fault.  Does
            // NOT yield the scheduler.
            SirStmt::Poll { cond, within_ns, .. } => {
                self.emit_bounded_wait(cond, (*within_ns).max(1), false);
            }
            // Bounded re-check (§5.2, P5-3): like poll but `wfi` between checks so
            // ISRs can run (a full D2-style frame suspend is a follow-up, as on the
            // C path); else fault.
            SirStmt::Await { cond, within_ns, recheck_ns, .. } => {
                let bound = (*within_ns / (*recheck_ns).max(1)).max(1);
                self.emit_bounded_wait(cond, bound, true);
            }
            other => {
                self.inst(&format!("; unsupported in llvm canary: {}", stmt_kind(other)));
            }
        }
    }

    /// Lower a `poll`/`await` to a bounded wait loop (P5-3): spin (`poll`) or
    /// `wfi`-between-checks (`await`) until `cond` holds; on the bound elapsing set
    /// `%__faulted` and route to the reaction's Layer-2 disposition.  Mirrors the C
    /// `Poll`/`Await` emission + `if(__faulted) <disposition>`.
    fn emit_bounded_wait(&mut self, cond: &SirExpr, bound: u64, is_await: bool) {
        if self.target != Target::MetalNrf52840 {
            // On the host the simulator services the wait (matches the C host path).
            self.inst("; poll/await serviced by the host simulator");
            return;
        }
        let tag = self.fresh_label("wait");
        let spins = format!("%{}.n", tag);
        self.allocas.push_str(&format!("  {} = alloca i32\n", spins));
        self.inst(&format!("store i32 0, ptr {}", spins));
        let cond_l = format!("{}.cond", tag);
        let spin_l = format!("{}.spin", tag);
        let again_l = format!("{}.again", tag);
        let fault_l = format!("{}.fault", tag);
        let done_l = format!("{}.done", tag);
        self.inst(&format!("br label %{}", cond_l));
        self.label(&cond_l);
        let c = self.emit_expr(cond, 1);
        // cond true → proceed; false → spin (bump the counter, check the bound).
        self.inst(&format!("br i1 {}, label %{}, label %{}", c, done_l, spin_l));
        self.label(&spin_l);
        let s = self.fresh();
        self.inst(&format!("{} = load i32, ptr {}", s, spins));
        let s1 = self.fresh();
        self.inst(&format!("{} = add i32 {}, 1", s1, s));
        self.inst(&format!("store i32 {}, ptr {}", s1, spins));
        let over = self.fresh();
        self.inst(&format!("{} = icmp ugt i32 {}, {}", over, s1, bound));
        self.inst(&format!("br i1 {}, label %{}, label %{}", over, fault_l, again_l));
        self.label(&again_l);
        if is_await {
            self.m_asm("wfi", "await: yield to ISRs between re-checks (§5.2)");
        }
        self.inst(&format!("br label %{}", cond_l));
        self.label(&fault_l);
        if self.current_disposition.is_some() {
            self.inst("store i8 1, ptr %__faulted");
        }
        self.inst(&format!("br label %{}", done_l));
        self.label(&done_l);
        // Route the timeout to the reaction's Layer-2 disposition.
        if let Some(disp) = self.current_disposition {
            let f = self.fresh();
            self.inst(&format!("{} = load i8, ptr %__faulted", f));
            let nz = self.fresh();
            self.inst(&format!("{} = icmp ne i8 {}, 0", nz, f));
            let on_l = self.fresh_label("onfault");
            let no_l = self.fresh_label("nofault");
            self.inst(&format!("br i1 {}, label %{}, label %{}", nz, on_l, no_l));
            self.label(&on_l);
            self.emit_terminal_disposition(disp);
            self.label(&no_l);
        }
    }

    fn emit_intrinsic(&mut self, intr: &SirIntrinsic) {
        // host-io is the host's macOS/arm64 syscall; on metal there is no
        // semihosting path yet, so it is a signposted no-op (P3-4c).
        if self.target == Target::MetalNrf52840 {
            self.inst("; host_io unsupported on metal LLVM (no semihosting yet)");
            return;
        }
        match intr {
            SirIntrinsic::HostIoPrintStr(s) => self.emit_write(s.as_bytes()),
            SirIntrinsic::HostIoPrint(SirExpr::Bytes(b)) => self.emit_write(b),
            // A runtime value (P6-4): convert to unsigned decimal then write —
            // matching the simulator's `host_io.print(<value>)` oracle.
            SirIntrinsic::HostIoPrint(e) => self.emit_print_decimal(e),
            // Stdout is unbuffered through the raw syscall — flush is a no-op.
            SirIntrinsic::HostIoFlush => self.inst("; host_io.flush: no-op (unbuffered syscall)"),
        }
    }

    /// `host_io.print(bytes)` → a raw `write(1, &str, len)` syscall via inline
    /// asm (macOS/arm64 `svc #0x80`, write = 4).  No libc `write` symbol — the
    /// whole point of the canary is that the module references no C runtime.
    fn emit_write(&mut self, bytes: &[u8]) {
        let g = format!("@.str.{}", self.next_str);
        self.next_str += 1;
        let mut lit = String::new();
        for &b in bytes {
            write!(lit, "\\{:02X}", b).unwrap();
        }
        self.globals.push_str(&format!(
            "{} = private unnamed_addr constant [{} x i8] c\"{}\"\n",
            g,
            bytes.len(),
            lit
        ));
        let r = self.fresh();
        self.inst(&format!(
            "{} = call i64 asm sideeffect \"svc #0x80\", \
             \"={{x0}},{{x16}},{{x0}},{{x1}},{{x2}},~{{memory}}\"\
             (i64 4, i64 1, ptr {}, i64 {})",
            r,
            g,
            bytes.len()
        ));
    }

    /// `host_io.print(<value>)` (P6-4): convert the runtime value to unsigned
    /// decimal into a stack buffer (digits filled from the end), then write the
    /// digit run via the raw syscall — matching the sim's decimal oracle.  No libc
    /// `printf`/`itoa`.  Host-only (metal returns before this).
    fn emit_print_decimal(&mut self, expr: &SirExpr) {
        let v0 = self.emit_expr(expr, 64);
        let tag = self.fresh_label("dec");
        let buf = format!("%{}.buf", tag);
        let vslot = format!("%{}.v", tag);
        let islot = format!("%{}.i", tag);
        self.allocas.push_str(&format!("  {} = alloca [24 x i8]\n", buf));
        self.allocas.push_str(&format!("  {} = alloca i64\n", vslot));
        self.allocas.push_str(&format!("  {} = alloca i64\n", islot));
        self.inst(&format!("store i64 {}, ptr {}", v0, vslot));
        self.inst(&format!("store i64 24, ptr {}", islot)); // fill from the end
        let loop_l = format!("{}.loop", tag);
        let done_l = format!("{}.done", tag);
        self.inst(&format!("br label %{}", loop_l));
        self.label(&loop_l);
        let v = self.fresh();
        self.inst(&format!("{} = load i64, ptr {}", v, vslot));
        let i = self.fresh();
        self.inst(&format!("{} = load i64, ptr {}", i, islot));
        let i1 = self.fresh();
        self.inst(&format!("{} = sub i64 {}, 1", i1, i));
        let d = self.fresh();
        self.inst(&format!("{} = urem i64 {}, 10", d, v));
        let c = self.fresh();
        self.inst(&format!("{} = add i64 {}, 48", c, d)); // '0'
        let c8 = self.fresh();
        self.inst(&format!("{} = trunc i64 {} to i8", c8, c));
        let slot = self.fresh();
        self.inst(&format!("{} = getelementptr [24 x i8], ptr {}, i64 0, i64 {}", slot, buf, i1));
        self.inst(&format!("store i8 {}, ptr {}", c8, slot));
        let vn = self.fresh();
        self.inst(&format!("{} = udiv i64 {}, 10", vn, v));
        self.inst(&format!("store i64 {}, ptr {}", vn, vslot));
        self.inst(&format!("store i64 {}, ptr {}", i1, islot));
        let nz = self.fresh();
        self.inst(&format!("{} = icmp ne i64 {}, 0", nz, vn));
        self.inst(&format!("br i1 {}, label %{}, label %{}", nz, loop_l, done_l));
        self.label(&done_l);
        let ifin = self.fresh();
        self.inst(&format!("{} = load i64, ptr {}", ifin, islot));
        let ptr = self.fresh();
        self.inst(&format!("{} = getelementptr [24 x i8], ptr {}, i64 0, i64 {}", ptr, buf, ifin));
        let len = self.fresh();
        self.inst(&format!("{} = sub i64 24, {}", len, ifin));
        let w = self.fresh();
        self.inst(&format!(
            "{} = call i64 asm sideeffect \"svc #0x80\", \
             \"={{x0}},{{x16}},{{x0}},{{x1}},{{x2}},~{{memory}}\"(i64 4, i64 1, ptr {}, i64 {})",
            w, ptr, len
        ));
    }

    // ── Expressions ─────────────────────────────────────────────────────────

    /// The inherent bit width of an expression (before any context conversion).
    fn natural_bits(&self, e: &SirExpr) -> u32 {
        match e {
            SirExpr::Bool(_) => 1,
            SirExpr::U64(_) => 64,
            SirExpr::Load(n) => self.vars.get(n).map(|v| v.0).unwrap_or(64),
            SirExpr::Not(_) => 1,
            SirExpr::BinOp(op, l, r) => {
                if is_cmp(*op) || matches!(op, SirBinOp::And | SirBinOp::Or) {
                    1
                } else {
                    self.natural_bits(l).max(self.natural_bits(r))
                }
            }
            SirExpr::Arith { width, .. } => *width as u32,
            SirExpr::Cast { to_width, .. } => *to_width as u32,
            SirExpr::RegLoad { width, .. } => *width as u32,
            SirExpr::Now => 64,
            _ => 64,
        }
    }

    /// A fresh `inttoptr` pointer to a device register's absolute address
    /// (`base + offset`), or `None` if the device declares no MMIO base.
    fn reg_ptr(&mut self, device: usize, offset: u64) -> Option<String> {
        let base = *self.device_bases.get(&device)?;
        let p = self.fresh();
        self.inst(&format!("{} = inttoptr i64 {} to ptr", p, base + offset));
        Some(p)
    }

    /// MMIO register field write (P3-4b): a `volatile` store at the absolute
    /// address.  A read/write field is a read-modify-write (`load`; clear the
    /// field bits; OR in `(value << shift) & mask`; `store`); a write-only /
    /// write-1-to-clear field writes just the field with no read (mirrors the C
    /// backend's `emit_mmio_store`).
    #[allow(clippy::too_many_arguments)]
    fn emit_reg_store(
        &mut self,
        device: usize,
        reg_offset: u64,
        width: u8,
        field_mask: u64,
        field_shift: u8,
        access: SirRegAccess,
        value: &SirExpr,
    ) {
        let w = width as u32;
        let v = self.emit_expr(value, w);
        let Some(p) = self.reg_ptr(device, reg_offset) else {
            self.inst(&format!("; unsupported: device {} has no MMIO base", device));
            return;
        };
        let shifted = self.fresh();
        self.inst(&format!("{} = shl i{} {}, {}", shifted, w, v, field_shift));
        let field = self.fresh();
        self.inst(&format!("{} = and i{} {}, {}", field, w, shifted, field_mask));
        match access {
            SirRegAccess::W1c | SirRegAccess::Wo => {
                self.inst(&format!("store volatile i{} {}, ptr {}", w, field, p));
            }
            _ => {
                let width_mask: u64 = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
                let notmask = (!field_mask) & width_mask;
                let old = self.fresh();
                self.inst(&format!("{} = load volatile i{}, ptr {}", old, w, p));
                let cleared = self.fresh();
                self.inst(&format!("{} = and i{} {}, {}", cleared, w, old, notmask));
                let newv = self.fresh();
                self.inst(&format!("{} = or i{} {}, {}", newv, w, cleared, field));
                self.inst(&format!("store volatile i{} {}, ptr {}", w, newv, p));
            }
        }
    }

    /// Lower a `SirStmt::RegWrite` (multi-field) to ONE ordered volatile store
    /// (§4.2/§6.2): OR every field into one value; if no field needs a read (all
    /// w1c/wo) it is a single masked write, else a single read-modify-write over
    /// the union mask — mirrors the C `emit_mmio_store_multi`.
    fn emit_reg_write_multi(
        &mut self,
        device: usize,
        reg_offset: u64,
        width: u8,
        writes: &[(u64, u8, SirRegAccess, SirExpr)],
    ) {
        let w = width as u32;
        let Some(p) = self.reg_ptr(device, reg_offset) else {
            self.inst(&format!("; unsupported: device {} has no MMIO base", device));
            return;
        };
        // OR the fields into one combined value; track the union of touched masks.
        let mut union_mask = 0u64;
        let mut acc: Option<String> = None;
        for (mask, shift, _access, value) in writes {
            union_mask |= *mask;
            let v = self.emit_expr(value, w);
            let sh = self.fresh();
            self.inst(&format!("{} = shl i{} {}, {}", sh, w, v, shift));
            let term = self.fresh();
            self.inst(&format!("{} = and i{} {}, {}", term, w, sh, mask));
            acc = Some(match acc {
                None => term,
                Some(prev) => {
                    let o = self.fresh();
                    self.inst(&format!("{} = or i{} {}, {}", o, w, prev, term));
                    o
                }
            });
        }
        let combined = acc.unwrap_or_else(|| "0".to_string());
        let single_write = writes
            .iter()
            .all(|(_, _, a, _)| matches!(a, SirRegAccess::W1c | SirRegAccess::Wo));
        if single_write {
            self.inst(&format!("store volatile i{} {}, ptr {}", w, combined, p));
        } else {
            let width_mask: u64 = if w >= 64 { u64::MAX } else { (1u64 << w) - 1 };
            let notmask = (!union_mask) & width_mask;
            let old = self.fresh();
            self.inst(&format!("{} = load volatile i{}, ptr {}", old, w, p));
            let cleared = self.fresh();
            self.inst(&format!("{} = and i{} {}, {}", cleared, w, old, notmask));
            let newv = self.fresh();
            self.inst(&format!("{} = or i{} {}, {}", newv, w, cleared, combined));
            self.inst(&format!("store volatile i{} {}, ptr {}", w, newv, p));
        }
    }

    /// The metal "drive safe + hold" sequence (§4.3/§5.6, P5-2): mask interrupts
    /// (so no reaction runs during/after the safe writes), run `@__drive_safe`,
    /// then spin in `wfi` forever.  Ends the current block in a self-loop; the
    /// caller decides whether the wider statement stream is terminated.
    fn emit_drive_safe_and_halt(&mut self, comment: &str) {
        self.m_asm("cpsid i", comment);
        self.inst("call void @__drive_safe()");
        let halt = self.fresh_label("safehalt");
        self.inst(&format!("br label %{}", halt));
        self.label(&halt);
        self.m_asm("wfi", "hold in safe state (§4.3/§5.6)");
        self.inst(&format!("br label %{}", halt));
    }

    /// `@__drive_safe` (§5.6, P5-2): drive every device with a declared safe
    /// sequence to its safe state by running its bounded, non-yielding register
    /// writes (empty body if none — matching the sim's no-op).  Mirrors the C
    /// `__drive_safe`.
    fn emit_drive_safe_fn(&mut self, module: &SirModule) {
        self.body.clear();
        self.allocas.clear();
        self.terminated = false;
        self.ret_i32 = false;
        self.vars.retain(|_, v| v.2);
        for seq in &module.safe_seqs {
            self.collect_locals(&seq.body);
        }
        for seq in &module.safe_seqs {
            self.inst(&format!("; device {} → safe state '{}'", seq.device, seq.state));
            for stmt in &seq.body {
                if self.terminated {
                    break;
                }
                self.emit_stmt(stmt);
            }
        }
        if !self.terminated {
            self.inst("ret void");
        }
        self.functions.push_str("define void @__drive_safe() {\nentry:\n");
        self.functions.push_str(&self.allocas);
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
    }

    /// `@__silica_overflow_trap` (§4.3, P5-2): the overflow-trap target on metal —
    /// drive safe + hold.  Mirrors the C `__silica_overflow_trap` (host uses the
    /// `llvm.trap` intrinsic inline instead).
    fn emit_overflow_trap_fn(&mut self) {
        self.body.clear();
        self.allocas.clear();
        self.emit_drive_safe_and_halt("overflow → halt (§4.3/SIL-004)");
        self.functions.push_str("define void @__silica_overflow_trap() {\nentry:\n");
        self.functions.push_str(&self.allocas);
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
    }

    /// `ring.push(v)` (§5.3, P6-1): on a full ring overwrite the oldest, then
    /// store at `tail` and advance.  Mirrors the C `RingPush` lowering.
    fn emit_ring_push(&mut self, ring: &str, value: &SirExpr) {
        let (ebits, cap) = self.ring_info.get(ring).copied().unwrap_or((32, 1));
        let cap = cap.max(1);
        let v = self.emit_expr(value, ebits);
        // Evict the oldest if full: if count >= cap { head=(head+1)%cap; count-- }.
        let cnt = self.fresh();
        self.inst(&format!("{} = load i32, ptr @__ring_{}_count", cnt, ring));
        let full = self.fresh();
        self.inst(&format!("{} = icmp uge i32 {}, {}", full, cnt, cap));
        let evict = self.fresh_label("evict");
        let store = self.fresh_label("rstore");
        self.inst(&format!("br i1 {}, label %{}, label %{}", full, evict, store));
        self.label(&evict);
        let h = self.fresh();
        self.inst(&format!("{} = load i32, ptr @__ring_{}_head", h, ring));
        let h2 = self.fresh();
        self.inst(&format!("{} = add i32 {}, 1", h2, h));
        let h3 = self.fresh();
        self.inst(&format!("{} = urem i32 {}, {}", h3, h2, cap));
        self.inst(&format!("store i32 {}, ptr @__ring_{}_head", h3, ring));
        let cd = self.fresh();
        self.inst(&format!("{} = sub i32 {}, 1", cd, cnt));
        self.inst(&format!("store i32 {}, ptr @__ring_{}_count", cd, ring));
        self.inst(&format!("br label %{}", store));
        self.label(&store);
        // buf[tail] = v; tail=(tail+1)%cap; count++.
        let t = self.fresh();
        self.inst(&format!("{} = load i32, ptr @__ring_{}_tail", t, ring));
        let t64 = self.fresh();
        self.inst(&format!("{} = zext i32 {} to i64", t64, t));
        let slot = self.fresh();
        self.inst(&format!("{} = getelementptr [{} x i{}], ptr @__ring_{}_buf, i64 0, i64 {}", slot, cap, ebits, ring, t64));
        self.inst(&format!("store i{} {}, ptr {}", ebits, v, slot));
        let t2 = self.fresh();
        self.inst(&format!("{} = add i32 {}, 1", t2, t));
        let t3 = self.fresh();
        self.inst(&format!("{} = urem i32 {}, {}", t3, t2, cap));
        self.inst(&format!("store i32 {}, ptr @__ring_{}_tail", t3, ring));
        let cc = self.fresh();
        self.inst(&format!("{} = load i32, ptr @__ring_{}_count", cc, ring));
        let cc1 = self.fresh();
        self.inst(&format!("{} = add i32 {}, 1", cc1, cc));
        self.inst(&format!("store i32 {}, ptr @__ring_{}_count", cc1, ring));
    }

    /// `let d = ring.pop()` (§5.3, P6-1): dequeue the oldest into `dst`, or 0 if
    /// empty.  Mirrors the C `RingPop` lowering.
    fn emit_ring_pop(&mut self, ring: &str, dst: &str) {
        let (ebits, cap) = self.ring_info.get(ring).copied().unwrap_or((32, 1));
        let cap = cap.max(1);
        let (dbits, _signed, dglobal) = self.vars.get(dst).copied().unwrap_or((32, false, false));
        let dptr = self.var_ptr(dst, dglobal);
        let cnt = self.fresh();
        self.inst(&format!("{} = load i32, ptr @__ring_{}_count", cnt, ring));
        let ne = self.fresh();
        self.inst(&format!("{} = icmp ugt i32 {}, 0", ne, cnt));
        let pop = self.fresh_label("rpop");
        let empty = self.fresh_label("rempty");
        let done = self.fresh_label("rpdone");
        self.inst(&format!("br i1 {}, label %{}, label %{}", ne, pop, empty));
        self.label(&pop);
        let h = self.fresh();
        self.inst(&format!("{} = load i32, ptr @__ring_{}_head", h, ring));
        let h64 = self.fresh();
        self.inst(&format!("{} = zext i32 {} to i64", h64, h));
        let slot = self.fresh();
        self.inst(&format!("{} = getelementptr [{} x i{}], ptr @__ring_{}_buf, i64 0, i64 {}", slot, cap, ebits, ring, h64));
        let val = self.fresh();
        self.inst(&format!("{} = load i{}, ptr {}", val, ebits, slot));
        let conv = self.convert(&val, ebits, dbits, false);
        self.inst(&format!("store i{} {}, ptr {}", dbits, conv, dptr));
        let h2 = self.fresh();
        self.inst(&format!("{} = add i32 {}, 1", h2, h));
        let h3 = self.fresh();
        self.inst(&format!("{} = urem i32 {}, {}", h3, h2, cap));
        self.inst(&format!("store i32 {}, ptr @__ring_{}_head", h3, ring));
        let cd = self.fresh();
        self.inst(&format!("{} = sub i32 {}, 1", cd, cnt));
        self.inst(&format!("store i32 {}, ptr @__ring_{}_count", cd, ring));
        self.inst(&format!("br label %{}", done));
        self.label(&empty);
        self.inst(&format!("store i{} 0, ptr {}", dbits, dptr));
        self.inst(&format!("br label %{}", done));
        self.label(&done);
    }

    /// Emit `e` as an operand of exactly `want` bits, inserting conversions.
    fn emit_expr(&mut self, e: &SirExpr, want: u32) -> String {
        match e {
            SirExpr::Bool(b) => {
                if want == 1 {
                    (if *b { "true" } else { "false" }).into()
                } else {
                    (if *b { "1" } else { "0" }).into()
                }
            }
            // An integer literal is typed by its use site — no conversion needed.
            SirExpr::U64(n) => format!("{}", n),
            SirExpr::Load(name) => {
                let (bits, signed, is_global) = self.vars[name];
                let ptr = self.var_ptr(name, is_global);
                let t = self.fresh();
                self.inst(&format!("{} = load i{}, ptr {}", t, bits, ptr));
                self.convert(&t, bits, want, signed)
            }
            SirExpr::Not(inner) => {
                let v = self.emit_expr(inner, 1);
                let t = self.fresh();
                self.inst(&format!("{} = xor i1 {}, true", t, v));
                self.convert(&t, 1, want, false)
            }
            SirExpr::BinOp(op, l, r) => self.emit_binop(*op, l, r, want),
            SirExpr::Arith { op, mode, width, signed, lhs, rhs } => {
                let res = self.emit_arith(*op, *mode, *width as u32, *signed, lhs, rhs);
                self.convert(&res, *width as u32, want, *signed)
            }
            SirExpr::Cast { inner, to_width, signed } => {
                let nat = self.natural_bits(inner);
                let v = self.emit_expr(inner, nat);
                let c = self.convert(&v, nat, *to_width as u32, *signed);
                self.convert(&c, *to_width as u32, want, *signed)
            }
            // MMIO register field read (P3-4b) → a `volatile` load at the
            // absolute address, masked + shifted to the field.
            SirExpr::RegLoad { device, reg_offset, width, field_mask, field_shift, .. } => {
                let w = *width as u32;
                match self.reg_ptr(*device, *reg_offset) {
                    Some(p) => {
                        let raw = self.fresh();
                        self.inst(&format!("{} = load volatile i{}, ptr {}", raw, w, p));
                        let masked = self.fresh();
                        self.inst(&format!("{} = and i{} {}, {}", masked, w, raw, field_mask));
                        let val = self.fresh();
                        self.inst(&format!("{} = lshr i{} {}, {}", val, w, masked, field_shift));
                        self.convert(&val, w, want, false)
                    }
                    None => {
                        self.inst(&format!("; unsupported: device {} has no MMIO base", device));
                        "0".into()
                    }
                }
            }
            // `now()` — a monotonic counter (§4.5).  Metal (P5-1) reads the
            // SysTick-driven `@__uptime_ns` (ns, 1 ms resolution); host lowers to
            // the LLVM cycle-counter intrinsic (i64), never a libc `clock_gettime`.
            SirExpr::Now => {
                if self.target == Target::MetalNrf52840 {
                    let t = self.fresh();
                    self.inst(&format!("{} = load volatile i64, ptr @__uptime_ns", t));
                    self.convert(&t, 64, want, false)
                } else {
                    self.declare("declare i64 @llvm.readcyclecounter()");
                    let t = self.fresh();
                    self.inst(&format!("{} = call i64 @llvm.readcyclecounter()", t));
                    self.convert(&t, 64, want, false)
                }
            }
            // Bounded-ring reads (§5.3, P6-1): count / count==0 / count>=cap.
            SirExpr::RingLen(r) => {
                let t = self.fresh();
                self.inst(&format!("{} = load i32, ptr @__ring_{}_count", t, r));
                self.convert(&t, 32, want, false)
            }
            SirExpr::RingEmpty(r) => {
                let c = self.fresh();
                self.inst(&format!("{} = load i32, ptr @__ring_{}_count", c, r));
                let t = self.fresh();
                self.inst(&format!("{} = icmp eq i32 {}, 0", t, c));
                self.convert(&t, 1, want, false)
            }
            SirExpr::RingFull(r) => {
                let cap = self.ring_info.get(r).map(|(_, c)| *c).unwrap_or(1).max(1);
                let c = self.fresh();
                self.inst(&format!("{} = load i32, ptr @__ring_{}_count", c, r));
                let t = self.fresh();
                self.inst(&format!("{} = icmp uge i32 {}, {}", t, c, cap));
                self.convert(&t, 1, want, false)
            }
            // Fixed-point mul/div + rescale cast (§4.3, P6-2).
            SirExpr::FixedArith { op, mode, frac_bits, width, signed, lhs, rhs } => {
                self.emit_fixed_arith(*op, *mode, *frac_bits, *width as u32, *signed, lhs, rhs, want)
            }
            SirExpr::FixedCast { inner, shift, to_width, signed } => {
                self.emit_fixed_cast(inner, *shift, *to_width as u32, *signed, want)
            }
            other => {
                self.inst(&format!("; unsupported expr in llvm canary: {}", expr_kind(other)));
                "0".into()
            }
        }
    }

    fn emit_binop(&mut self, op: SirBinOp, l: &SirExpr, r: &SirExpr, want: u32) -> String {
        use SirBinOp::*;
        if matches!(op, And | Or) {
            let lo = self.emit_expr(l, 1);
            let ro = self.emit_expr(r, 1);
            let t = self.fresh();
            let opc = if matches!(op, And) { "and" } else { "or" };
            self.inst(&format!("{} = {} i1 {}, {}", t, opc, lo, ro));
            return self.convert(&t, 1, want, false);
        }
        if is_cmp(op) {
            let w = self.natural_bits(l).max(self.natural_bits(r)).max(1);
            let lo = self.emit_expr(l, w);
            let ro = self.emit_expr(r, w);
            let pred = match op {
                EqEq => "eq",
                NotEq => "ne",
                Lt => "ult",
                Le => "ule",
                Gt => "ugt",
                Ge => "uge",
                _ => unreachable!(),
            };
            let t = self.fresh();
            self.inst(&format!("{} = icmp {} i{} {}, {}", t, pred, w, lo, ro));
            return self.convert(&t, 1, want, false);
        }
        // Plain (non-width-checked) arithmetic — Div/Rem reach here (§4.3).
        let lo = self.emit_expr(l, want);
        let ro = self.emit_expr(r, want);
        let opc = match op {
            Add => "add",
            Sub => "sub",
            Mul => "mul",
            Div => "udiv",
            Rem => "urem",
            _ => unreachable!(),
        };
        let t = self.fresh();
        self.inst(&format!("{} = {} i{} {}, {}", t, opc, want, lo, ro));
        t
    }

    /// Width-checked arithmetic (§4.3/SIL-004) at `width` bits.  Trap mode is the
    /// canary's headline proof: it lowers to an LLVM overflow intrinsic plus
    /// `llvm.trap`, never `__builtin_*_overflow`.
    fn emit_arith(
        &mut self,
        op: SirArithOp,
        mode: OverflowMode,
        width: u32,
        signed: bool,
        lhs: &SirExpr,
        rhs: &SirExpr,
    ) -> String {
        let l = self.emit_expr(lhs, width);
        let r = self.emit_expr(rhs, width);
        let opc = match op {
            SirArithOp::Add => "add",
            SirArithOp::Sub => "sub",
            SirArithOp::Mul => "mul",
        };
        match mode {
            OverflowMode::Wrap => {
                let t = self.fresh();
                self.inst(&format!("{} = {} i{} {}, {}", t, opc, width, l, r));
                t
            }
            OverflowMode::Trap => {
                let (_agg, ov, val) = self.with_overflow(opc, width, signed, &l, &r);
                let trap = self.fresh_label("trap");
                let cont = self.fresh_label("cont");
                self.inst(&format!("br i1 {}, label %{}, label %{}", ov, trap, cont));
                self.label(&trap);
                // Metal (P5-2): drive the system to its safe state, then hold.
                // Host: the LLVM trap intrinsic (proves the trap is not a C-ism).
                if self.target == Target::MetalNrf52840 {
                    self.inst("call void @__silica_overflow_trap()");
                } else {
                    self.declare("declare void @llvm.trap()");
                    self.inst("call void @llvm.trap()");
                }
                self.inst("unreachable");
                self.label(&cont);
                val
            }
            OverflowMode::Saturate => {
                let (_agg, ov, raw) = self.with_overflow(opc, width, signed, &l, &r);
                let clamp = if signed {
                    // Signed saturate (P3-4a): on overflow clamp to INT_MAX when
                    // the result should be positive, INT_MIN when negative.  The
                    // sign of `lhs` decides it: `(lhs >>s (W-1)) ^ INT_MAX` is
                    // INT_MAX for lhs ≥ 0 (0 ^ INT_MAX) and INT_MIN for lhs < 0
                    // (-1 ^ INT_MAX).
                    let int_max = (1i128 << (width - 1)) - 1;
                    let sign = self.fresh();
                    self.inst(&format!("{} = ashr i{w} {}, {}", sign, l, width - 1, w = width));
                    let cl = self.fresh();
                    self.inst(&format!("{} = xor i{w} {}, {}", cl, sign, int_max, w = width));
                    cl
                } else {
                    // Unsigned: clamp to 0 on sub underflow, else to all-ones (max).
                    match op {
                        SirArithOp::Sub => "0".to_string(),
                        _ => "-1".to_string(),
                    }
                };
                let t = self.fresh();
                self.inst(&format!(
                    "{} = select i1 {}, i{} {}, i{} {}",
                    t, ov, width, clamp, width, raw
                ));
                t
            }
        }
    }

    /// Emit a `llvm.{u,s}<op>.with.overflow.iN` call and extract (overflow bit,
    /// result value).  Returns (aggregate, overflow %reg, value %reg).
    fn with_overflow(
        &mut self,
        opc: &str,
        width: u32,
        signed: bool,
        l: &str,
        r: &str,
    ) -> (String, String, String) {
        let intr = format!("llvm.{}{}.with.overflow.i{}", if signed { "s" } else { "u" }, opc, width);
        self.declare(&format!(
            "declare {{ i{w}, i1 }} @{intr}(i{w}, i{w})",
            w = width,
            intr = intr
        ));
        let agg = self.fresh();
        self.inst(&format!(
            "{} = call {{ i{w}, i1 }} @{intr}(i{w} {}, i{w} {})",
            agg,
            l,
            r,
            w = width,
            intr = intr
        ));
        let ov = self.fresh();
        self.inst(&format!("{} = extractvalue {{ i{w}, i1 }} {}, 1", ov, agg, w = width));
        let val = self.fresh();
        self.inst(&format!("{} = extractvalue {{ i{w}, i1 }} {}, 0", val, agg, w = width));
        (agg, ov, val)
    }

    /// Emit a conditional trap: if `cond` (an i1), go to the safe-state trap
    /// (`@__silica_overflow_trap` on metal, `llvm.trap` on host) + `unreachable`;
    /// else fall through.  Shared by the fixed-point trap/div-by-zero paths (P6-2).
    fn emit_trap_if(&mut self, cond: &str) {
        let trap = self.fresh_label("trap");
        let cont = self.fresh_label("cont");
        self.inst(&format!("br i1 {}, label %{}, label %{}", cond, trap, cont));
        self.label(&trap);
        if self.target == Target::MetalNrf52840 {
            self.inst("call void @__silica_overflow_trap()");
        } else {
            self.declare("declare void @llvm.trap()");
            self.inst("call void @llvm.trap()");
        }
        self.inst("unreachable");
        self.label(&cont);
    }

    /// Fixed-point mul/div with rescale (§4.3 P0-3c, P6-2): compute in a 64-bit
    /// (sign-aware) intermediate so a width≤32 product/quotient can't overflow it,
    /// rescale by `frac`, then apply the overflow mode at `width`.  Mirrors the C
    /// `fixmul`/`fixdiv` helpers; the result is returned at `want` bits.
    fn emit_fixed_arith(
        &mut self,
        op: FixedArithOp,
        mode: OverflowMode,
        frac: u8,
        width: u32,
        signed: bool,
        lhs: &SirExpr,
        rhs: &SirExpr,
        want: u32,
    ) -> String {
        let a = self.emit_expr(lhs, width);
        let b = self.emit_expr(rhs, width);
        // Widen both operands to a 64-bit intermediate (sign-aware).
        let a64 = self.convert(&a, width, 64, signed);
        let b64 = self.convert(&b, width, 64, signed);
        let raw = match op {
            FixedArithOp::Mul => {
                let p = self.fresh();
                self.inst(&format!("{} = mul i64 {}, {}", p, a64, b64));
                let r = self.fresh();
                let sh = if signed { "ashr" } else { "lshr" };
                self.inst(&format!("{} = {} i64 {}, {}", r, sh, p, frac));
                r
            }
            FixedArithOp::Div => {
                // Divide-by-zero → safe-state trap (regardless of mode).
                let z = self.fresh();
                self.inst(&format!("{} = icmp eq i64 {}, 0", z, b64));
                self.emit_trap_if(&z);
                let an = self.fresh();
                self.inst(&format!("{} = shl i64 {}, {}", an, a64, frac));
                let r = self.fresh();
                let d = if signed { "sdiv" } else { "udiv" };
                self.inst(&format!("{} = {} i64 {}, {}", r, d, an, b64));
                r
            }
        };
        // Apply the overflow mode at `width` on the 64-bit raw value.
        let (lo, hi): (i128, i128) = if signed {
            (-(1i128 << (width - 1)), (1i128 << (width - 1)) - 1)
        } else {
            (0, (1i128 << width) - 1)
        };
        let narrowed = match mode {
            OverflowMode::Wrap => self.convert(&raw, 64, width, signed),
            OverflowMode::Trap => {
                let (lt, gt) = if signed { ("slt", "sgt") } else { ("ult", "ugt") };
                let below = self.fresh();
                self.inst(&format!("{} = icmp {} i64 {}, {}", below, lt, raw, lo));
                let above = self.fresh();
                self.inst(&format!("{} = icmp {} i64 {}, {}", above, gt, raw, hi));
                let ov = self.fresh();
                self.inst(&format!("{} = or i1 {}, {}", ov, below, above));
                self.emit_trap_if(&ov);
                self.convert(&raw, 64, width, signed)
            }
            OverflowMode::Saturate => {
                let (lt, gt) = if signed { ("slt", "sgt") } else { ("ult", "ugt") };
                let above = self.fresh();
                self.inst(&format!("{} = icmp {} i64 {}, {}", above, gt, raw, hi));
                let c1 = self.fresh();
                self.inst(&format!("{} = select i1 {}, i64 {}, i64 {}", c1, above, hi, raw));
                let below = self.fresh();
                self.inst(&format!("{} = icmp {} i64 {}, {}", below, lt, c1, lo));
                let c2 = self.fresh();
                self.inst(&format!("{} = select i1 {}, i64 {}, i64 {}", c2, below, lo, c1));
                self.convert(&c2, 64, width, signed)
            }
        };
        self.convert(&narrowed, width, want, signed)
    }

    /// Fixed-point rescale cast (§4.3 P0-3a, P6-2): shift the binary point in a
    /// 64-bit sign-aware intermediate, then narrow to the target width.  Mirrors
    /// the C `FixedCast`.  Returned at `want` bits.
    fn emit_fixed_cast(&mut self, inner: &SirExpr, shift: i8, to_width: u32, signed: bool, want: u32) -> String {
        // The inner value's storage width — best-effort from its natural bits.
        let from = store_bits(self.natural_bits(inner));
        let v = self.emit_expr(inner, from);
        let v64 = self.convert(&v, from, 64, signed);
        let scaled = self.fresh();
        if shift >= 0 {
            self.inst(&format!("{} = shl i64 {}, {}", scaled, v64, shift));
        } else {
            let sh = if signed { "ashr" } else { "lshr" };
            self.inst(&format!("{} = {} i64 {}, {}", scaled, sh, v64, -(shift as i32)));
        }
        let narrowed = self.convert(&scaled, 64, to_width, signed);
        self.convert(&narrowed, to_width, want, signed)
    }

    /// Convert an SSA value from `from` bits to `to` bits.  Literals (operands
    /// that aren't `%`-registers) are typed by context, so they pass through.
    fn convert(&mut self, val: &str, from: u32, to: u32, signed: bool) -> String {
        if from == to || !val.starts_with('%') {
            return val.to_string();
        }
        let t = self.fresh();
        if to < from {
            self.inst(&format!("{} = trunc i{} {} to i{}", t, from, val, to));
        } else {
            let opc = if signed { "sext" } else { "zext" };
            self.inst(&format!("{} = {} i{} {} to i{}", t, opc, from, val, to));
        }
        t
    }

    fn var_ptr(&self, name: &str, is_global: bool) -> String {
        // A yielding reaction's cross-yield temps are module globals `@<prefix><name>`.
        if let Some((prefix, temps)) = &self.frame {
            if temps.contains(name) {
                return format!("@{}{}", prefix, name);
            }
        }
        if is_global {
            format!("@{}", name)
        } else {
            format!("%{}.addr", name)
        }
    }
}

/// Const-evaluate a `SirVar` initializer to an LLVM constant operand.
fn const_init(e: &SirExpr) -> String {
    match e {
        SirExpr::U64(n) => format!("{}", n),
        SirExpr::Bool(b) => (if *b { "1" } else { "0" }).into(),
        _ => "0".into(),
    }
}

fn is_cmp(op: SirBinOp) -> bool {
    use SirBinOp::*;
    matches!(op, EqEq | NotEq | Lt | Le | Gt | Ge)
}

fn stmt_kind(s: &SirStmt) -> &'static str {
    match s {
        SirStmt::Intrinsic(_) => "Intrinsic",
        SirStmt::Assign { .. } => "Assign",
        SirStmt::RegWrite { .. } => "RegWrite",
        SirStmt::RingPush { .. } => "RingPush",
        SirStmt::RingPop { .. } => "RingPop",
        SirStmt::If { .. } => "If",
        SirStmt::Exit(_) => "Exit",
        SirStmt::DriveSafe => "DriveSafe",
        SirStmt::Critical { .. } => "Critical",
        SirStmt::Poll { .. } => "Poll",
        SirStmt::Await { .. } => "Await",
        SirStmt::DeviceOp { .. } => "DeviceOp",
        SirStmt::BusXfer { .. } => "BusXfer",
    }
}

fn expr_kind(e: &SirExpr) -> &'static str {
    match e {
        SirExpr::Bool(_) => "Bool",
        SirExpr::U64(_) => "U64",
        SirExpr::Bytes(_) => "Bytes",
        SirExpr::Load(_) => "Load",
        SirExpr::RegLoad { .. } => "RegLoad",
        SirExpr::Not(_) => "Not",
        SirExpr::BinOp(..) => "BinOp",
        SirExpr::Arith { .. } => "Arith",
        SirExpr::Now => "Now",
        SirExpr::Cast { .. } => "Cast",
        SirExpr::FixedCast { .. } => "FixedCast",
        SirExpr::FixedArith { .. } => "FixedArith",
        SirExpr::RingLen(_) => "RingLen",
        SirExpr::RingEmpty(_) => "RingEmpty",
        SirExpr::RingFull(_) => "RingFull",
    }
}
