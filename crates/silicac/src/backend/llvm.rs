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
        // 0. Device MMIO bases (P3-4b): for `Reg`/`RegLoad` volatile addressing.
        for dev in &module.devices {
            if let Some(base) = dev.base_addr {
                self.device_bases.insert(dev.id, base);
            }
        }
        // 1. Cells → module globals.
        for v in &module.vars {
            let bits = store_bits(sir_bits(&v.ty));
            let signed = sir_signed(&v.ty);
            self.vars.insert(v.name.clone(), (bits, signed, true));
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
        if metal {
            // Metal (P3-4c): `Reset_Handler` runs `sys.start` then idles; no
            // `@main`, no host syscalls.  A `.vectors` table + `_estack` make it
            // boot under the generated linker script.
            self.lower_reset_handler(&sys);
        } else {
            // Host: `@main` runs every `on sys.start` body, in order.
            self.lower_function("i32 @main()", true, &sys);
        }

        // Every non-`sys.start` reaction lowers to its own `void` function (no
        // scheduler yet — wiring `every`/`on` to a timer/IRQ is the remaining
        // P3-4c work).  This proves the backend lowers bodies beyond `sys.start`.
        for r in &module.reactions {
            if !matches!(r.trigger, SirTrigger::SysStart) {
                let sig = format!("void @__reaction_{}()", r.id);
                self.lower_function(&sig, false, &[r.body.as_slice()]);
            }
        }

        // Assemble the module.
        let mut out = String::new();
        out.push_str("; Silica LLVM-IR backend (audit #35 P2-1 + P3-4a/b/c, DESIGN §6.3/§12)\n");
        out.push_str("; A second, structurally independent SIR consumer — proves SIR is\n");
        out.push_str("; target-neutral and the overflow trap is not a C-ism.\n\n");
        if metal {
            out.push_str(&format!("target triple = \"{}\"\n\n", METAL_TRIPLE));
            // The stack top + the vector table the Cortex-M core reads on reset:
            // [0] = initial SP (= address of `_estack`, a linker symbol), [1] =
            // the reset vector.  `KEEP(*(.vectors))` places it at flash base.
            out.push_str("@_estack = external global i8\n");
            out.push_str("@__vectors = constant [2 x ptr] [ptr @_estack, ptr @Reset_Handler], section \".vectors\", align 4\n\n");
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

    /// Metal entry (P3-4c): lower the `sys.start` bodies into `@Reset_Handler`,
    /// which runs them then **idles** in a `wfi` loop — a bare-metal reset
    /// handler must never return (there is no caller).  The cell writes land in
    /// RAM; the linker script + `.vectors` table make the core boot straight into
    /// this on reset.
    fn lower_reset_handler(&mut self, bodies: &[&[SirStmt]]) {
        self.body.clear();
        self.allocas.clear();
        self.terminated = false;
        self.ret_i32 = false;
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
            let idle = self.fresh_label("idle");
            self.inst(&format!("br label %{}", idle));
            self.label(&idle);
            self.inst("call void asm sideeffect \"wfi\", \"\"()");
            self.inst(&format!("br label %{}", idle));
        }
        self.functions.push_str("define void @Reset_Handler() {\nentry:\n");
        self.functions.push_str(&self.allocas);
        self.functions.push_str(&self.body);
        self.functions.push_str("}\n\n");
    }

    /// Walk a body and `alloca` any Assign-target that is not a known global.
    fn collect_locals(&mut self, body: &[SirStmt]) {
        for stmt in body {
            if let SirStmt::Assign { target: SirPlace::Var(name), value } = stmt {
                if !self.vars.contains_key(name) {
                    let bits = store_bits(self.natural_bits(value));
                    self.vars.insert(name.clone(), (bits, false, false));
                    self.allocas
                        .push_str(&format!("  %{}.addr = alloca i{}\n", name, bits));
                }
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
            // A critical section is invisible to a single-threaded `@main` — its
            // body lowers inline (the priority-ceiling raise is metal-only, §5.5).
            SirStmt::Critical { body, .. } => {
                for s in body {
                    if self.terminated {
                        break;
                    }
                    self.emit_stmt(s);
                }
            }
            other => {
                self.inst(&format!("; unsupported in llvm canary: {}", stmt_kind(other)));
            }
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
            SirIntrinsic::HostIoPrint(_) => {
                self.inst("; unsupported in llvm canary: dynamic host_io.print");
            }
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
            // `now()` — a monotonic counter.  Lowers to the LLVM cycle-counter
            // intrinsic (i64), never a libc `clock_gettime` (§4.5).
            SirExpr::Now => {
                self.declare("declare i64 @llvm.readcyclecounter()");
                let t = self.fresh();
                self.inst(&format!("{} = call i64 @llvm.readcyclecounter()", t));
                self.convert(&t, 64, want, false)
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
                self.declare("declare void @llvm.trap()");
                self.inst("call void @llvm.trap()");
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
