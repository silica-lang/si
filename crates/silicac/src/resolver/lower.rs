//! Resolver lowering utilities (audit #35 P7-9a) — the typed-AST→SIR primitive
//! and analysis layer extracted from the resolver driver.  It holds the SIR
//! constructors (`reg_place`/`reg_load`/`lower_regs`/`lower_cast`), the AST→SIR
//! type/op/const conversions, and the post-lowering cell/yield analysis.  A
//! stable boundary the driver (`super`) calls into; behavior-identical to the
//! prior in-file helpers.

use std::collections::HashMap;

use crate::ast::*;
use crate::sir::*;

use super::RegInfo;

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Mask + shift for a register field bit-spec.  Shift-safe: out-of-range or
/// inverted bit-specs (validated/diagnosed in `check_regs`) degrade to a 0 mask
/// rather than panicking, since bit indices are user input.
pub(super) fn bitspec_mask_shift(b: BitSpec) -> (u64, u8) {
    match b {
        BitSpec::Bit(n) => (1u64.checked_shl(n as u32).unwrap_or(0), n),
        BitSpec::Range(hi, lo) if hi >= lo => {
            let width = (hi - lo + 1) as u32;
            let span = if width >= 64 { u64::MAX } else { (1u64 << width) - 1 };
            (span.checked_shl(lo as u32).unwrap_or(0), lo)
        }
        BitSpec::Range(_, lo) => (0, lo),
    }
}

/// Full-width mask for a register of the given bit width.
pub(super) fn full_mask(width: u8) -> u64 {
    if width >= 64 { u64::MAX } else { (1u64 << width) - 1 }
}

pub(super) fn reg_place(ri: &RegInfo, mask: u64, shift: u8, access: SirRegAccess) -> SirPlace {
    SirPlace::Reg {
        device: ri.device,
        reg_offset: ri.offset,
        width: ri.width,
        field_mask: mask,
        field_shift: shift,
        access,
    }
}

pub(super) fn reg_load(ri: &RegInfo, mask: u64, shift: u8, access: SirRegAccess) -> SirExpr {
    SirExpr::RegLoad {
        device: ri.device,
        reg_offset: ri.offset,
        width: ri.width,
        field_mask: mask,
        field_shift: shift,
        access,
        // P7-6a: reads of an `rc`/`pop_on_read` register clear it — track it here
        // so the sim models the clear for *any* read position (RHS or condition),
        // even when the field/register access is `rw` (a `pop_on_read` register).
        read_clears: ri.read_side_effect,
    }
}

pub(super) fn map_access(a: RegAccess) -> SirRegAccess {
    match a {
        RegAccess::Ro => SirRegAccess::Ro,
        RegAccess::Wo => SirRegAccess::Wo,
        RegAccess::Rw => SirRegAccess::Rw,
        RegAccess::W1c => SirRegAccess::W1c,
        RegAccess::Rc => SirRegAccess::Rc,
    }
}

pub(super) fn lower_regs(def: &DeviceDef) -> Vec<SirReg> {
    match &def.sections.regs {
        Some(rs) => rs
            .regs
            .iter()
            .map(|r| SirReg {
                name: r.name.name.clone(),
                offset: r.offset,
                width: r.width,
                access: map_access(r.access),
                reset: 0,
            })
            .collect(),
        None => Vec::new(),
    }
}

/// Does any statement in `stmts` read or write the named cell?
pub(super) fn stmts_touch_cell(stmts: &[SirStmt], cell: &str) -> bool {
    stmts.iter().any(|s| stmt_touches_cell(s, cell))
}

pub(super) fn stmt_touches_cell(stmt: &SirStmt, cell: &str) -> bool {
    match stmt {
        SirStmt::Assign { target, value } => {
            place_touches_cell(target, cell) || expr_touches_cell(value, cell)
        }
        // A register multi-field write touches a cell only via its value exprs
        // (the target is MMIO, not a cell).
        SirStmt::RegWrite { writes, .. } => writes.iter().any(|(_, _, _, v)| expr_touches_cell(v, cell)),
        SirStmt::If { cond, then } => expr_touches_cell(cond, cell) || stmts_touch_cell(then, cell),
        SirStmt::Poll { cond, .. } => expr_touches_cell(cond, cell),
        // An `await` *polls* its condition (it must observe a cell another reaction
        // changes during the suspension), so it is deliberately NOT a synchronized
        // cell access — it is never wrapped in the §5.5 auto-critical.
        SirStmt::Await { .. } => false,
        SirStmt::Critical { body, .. } => stmts_touch_cell(body, cell),
        SirStmt::Exit(e) => expr_touches_cell(e, cell),
        SirStmt::Intrinsic(intr) => match intr {
            SirIntrinsic::HostIoPrint(e) => expr_touches_cell(e, cell),
            _ => false,
        },
        SirStmt::DeviceOp { args, .. } => args.iter().any(|a| expr_touches_cell(a, cell)),
        SirStmt::BusXfer { args, .. } => args.iter().any(|a| expr_touches_cell(a, cell)),
        SirStmt::RingPush { ring, value } => ring == cell || expr_touches_cell(value, cell),
        SirStmt::RingPop { ring, .. } => ring == cell,
        SirStmt::BufferSet { buffer, index, value } => {
            buffer == cell || expr_touches_cell(index, cell) || expr_touches_cell(value, cell)
        }
        SirStmt::PoolAlloc { pool, .. } => pool == cell,
        SirStmt::PoolFree { pool, handle } => pool == cell || expr_touches_cell(handle, cell),
        SirStmt::PoolSet { pool, handle, value } => {
            pool == cell || expr_touches_cell(handle, cell) || expr_touches_cell(value, cell)
        }
        // A safe-state drive touches no cell (it halts the system).
        SirStmt::DriveSafe => false,
    }
}

/// True if any `Critical` section in the body (transitively) contains a yielding
/// bus transfer — the §5.5/D03 violation.
pub(super) fn critical_contains_yield(stmts: &[SirStmt]) -> bool {
    stmts.iter().any(|s| match s {
        SirStmt::Critical { body, .. } => body_yields(body) || critical_contains_yield(body),
        SirStmt::If { then, .. } => critical_contains_yield(then),
        _ => false,
    })
}

/// True if any `if` in the body (transitively) contains a yielding bus transfer.
pub(super) fn if_contains_yield(stmts: &[SirStmt]) -> bool {
    stmts.iter().any(|s| match s {
        SirStmt::If { then, .. } => body_yields(then) || if_contains_yield(then),
        SirStmt::Critical { body, .. } => if_contains_yield(body),
        _ => false,
    })
}

/// Map a surface binary operator to its float arithmetic op (P6-8).  Only the
/// plain `+ - * /` lower to float math (IEEE has no wrap/saturate); comparisons
/// and wrap/sat variants return `None` (handled on the raw path).
pub(super) fn float_binop(op: BinOp) -> Option<SirBinOp> {
    match op {
        BinOp::Add => Some(SirBinOp::Add),
        BinOp::Sub => Some(SirBinOp::Sub),
        BinOp::Mul => Some(SirBinOp::Mul),
        BinOp::Div => Some(SirBinOp::Div),
        _ => None,
    }
}

/// IEEE-754 bit pattern of `v` at `width` (32/64) — the value a `FloatLit`
/// carries (P6-8).  The sim's `u64` value model stores this pattern directly.
pub(super) fn float_bits(v: f64, width: u8) -> u64 {
    if width == 32 {
        (v as f32).to_bits() as u64
    } else {
        v.to_bits()
    }
}

/// True if a (possibly nested) statement list contains a yielding bus transfer.
pub(super) fn body_yields(stmts: &[SirStmt]) -> bool {
    stmts.iter().any(|s| match s {
        SirStmt::BusXfer { .. } => true,
        SirStmt::Critical { body, .. } => body_yields(body),
        SirStmt::If { then, .. } => body_yields(then),
        _ => false,
    })
}

/// True if an `await` (a suspend point, P6-5) is nested inside an `if`/`critical`
/// rather than at the reaction top level — the metal segmenter splits suspend
/// points at the top level only, so a nested `await` is rejected (as a nested
/// yield is).
pub(super) fn await_nested_in_block(stmts: &[SirStmt]) -> bool {
    fn has_await(stmts: &[SirStmt]) -> bool {
        stmts.iter().any(|s| match s {
            SirStmt::Await { .. } => true,
            SirStmt::If { then, .. } => has_await(then),
            SirStmt::Critical { body, .. } => has_await(body),
            _ => false,
        })
    }
    stmts.iter().any(|s| match s {
        SirStmt::If { then, .. } => has_await(then),
        SirStmt::Critical { body, .. } => has_await(body),
        _ => false,
    })
}

/// Lower a parsed fault disposition to its SIR form (§4.4); default `escalate`.
pub(super) fn lower_disposition(d: &Option<FaultDisp>) -> SirDisposition {
    match d.as_ref().map(|f| &f.kind) {
        Some(FaultDispKind::Retry { max }) => SirDisposition::Retry { max: max.unwrap_or(1) },
        Some(FaultDispKind::Skip) => SirDisposition::Skip,
        Some(FaultDispKind::Safe) => SirDisposition::Safe,
        Some(FaultDispKind::Escalate) | None => SirDisposition::Escalate,
    }
}

pub(super) fn place_touches_cell(place: &SirPlace, cell: &str) -> bool {
    matches!(place, SirPlace::Var(n) if n == cell)
}

pub(super) fn expr_touches_cell(expr: &SirExpr, cell: &str) -> bool {
    match expr {
        SirExpr::Load(n) => n == cell,
        SirExpr::Not(inner) => expr_touches_cell(inner, cell),
        SirExpr::Cast { inner, .. } | SirExpr::FixedCast { inner, .. } => expr_touches_cell(inner, cell),
        SirExpr::FixedArith { lhs, rhs, .. } => expr_touches_cell(lhs, cell) || expr_touches_cell(rhs, cell),
        SirExpr::BinOp(_, l, r) => expr_touches_cell(l, cell) || expr_touches_cell(r, cell),
        SirExpr::Arith { lhs, rhs, .. } => expr_touches_cell(lhs, cell) || expr_touches_cell(rhs, cell),
        SirExpr::RingLen(r) | SirExpr::RingEmpty(r) | SirExpr::RingFull(r) => r == cell,
        SirExpr::BufferGet { buffer, index } => buffer == cell || expr_touches_cell(index, cell),
        SirExpr::BufferLen(b) => b == cell,
        SirExpr::PoolGet { pool, handle } => pool == cell || expr_touches_cell(handle, cell),
        SirExpr::PoolCount(p) | SirExpr::PoolCap(p) => p == cell,
        _ => false,
    }
}

/// Extract the root identifier name from an expression like `foo` or `foo.bar`.
/// The typestate an op transitions the device to, if its body ends by `become`-ing
/// a state (the last top-level `become` wins).  `None` if it makes no transition.
pub(super) fn op_become_target(op: &OpDecl) -> Option<String> {
    op.body.stmts.iter().rev().find_map(|s| match s {
        Stmt::Become(state, _) => Some(state.name.clone()),
        _ => None,
    })
}

pub(super) fn expr_root_ident(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Ident(ident) => Some(&ident.name),
        ExprKind::Field(inner, _) => expr_root_ident(inner),
        _ => None,
    }
}

/// `(width_bits, signed)` for an integer SIR type; `None` for non-integers.
pub(super) fn sirtype_width_sign(t: &SirType) -> Option<(u8, bool)> {
    match t {
        SirType::U8 => Some((8, false)),
        SirType::U16 => Some((16, false)),
        SirType::U32 => Some((32, false)),
        SirType::U64 => Some((64, false)),
        SirType::S8 => Some((8, true)),
        SirType::S16 => Some((16, true)),
        SirType::S32 => Some((32, true)),
        SirType::S64 => Some((64, true)),
        _ => None, // bool / bytes — not width-checked
    }
}

/// The width/sign category of a value (§4.3).  `Int` is a declared-typed value;
/// `Literal` is an integer literal (adopts any width/sign that fits, so it never
/// triggers narrowing/sign errors); `Flexible` is an unknown (a device-op
/// result, register read, bool, …) — also exempt, to avoid false positives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum ValType {
    Int { width: u8, signed: bool },
    /// `fixed<I,F>` (§4.3, P0-3a): distinct from `Int` so mixing fixed with an
    /// integer — or a different fixed scale — is a type error (needs a cast).
    Fixed { int_bits: u8, frac_bits: u8, signed: bool },
    Literal,
    Flexible,
}

pub(super) fn sirtype_valtype(t: &SirType) -> ValType {
    match t {
        SirType::Fixed { int_bits, frac_bits, signed } => {
            ValType::Fixed { int_bits: *int_bits, frac_bits: *frac_bits, signed: *signed }
        }
        _ => match sirtype_width_sign(t) {
            Some((width, signed)) => ValType::Int { width, signed },
            None => ValType::Flexible,
        },
    }
}

/// `(frac_bits)` of a fixed type, else 0 — the scale used by `lower_cast`.
pub(super) fn fixed_frac_bits(t: &SirType) -> u8 {
    match t {
        SirType::Fixed { frac_bits, .. } => *frac_bits,
        _ => 0,
    }
}

/// `Some(frac_bits)` for a fixed type, else `None` — the arith-context scale.
pub(super) fn sirtype_frac(t: &SirType) -> Option<u8> {
    match t {
        SirType::Fixed { frac_bits, .. } => Some(*frac_bits),
        _ => None,
    }
}

pub(super) fn is_comparison(op: BinOp) -> bool {
    matches!(op, BinOp::EqEq | BinOp::NotEq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
}

/// The width/sign of an arithmetic result (sign already mixed-checked): the
/// wider of two declared types, or the typed operand when the other is a literal.
pub(super) fn arith_result(l: ValType, r: ValType) -> ValType {
    use ValType::{Fixed, Flexible, Int, Literal};
    match (l, r) {
        (Int { width: wl, signed }, Int { width: wr, .. }) => Int { width: wl.max(wr), signed },
        (Int { width, signed }, Literal) | (Literal, Int { width, signed }) => Int { width, signed },
        // Fixed add/sub: both operands must share the same scale (checked in
        // check_mixed_sign); a literal adopts the fixed scale.
        (f @ Fixed { .. }, Fixed { .. }) => f,
        (f @ Fixed { .. }, Literal) | (Literal, f @ Fixed { .. }) => f,
        (Literal, Literal) => Literal,
        _ => Flexible,
    }
}

/// Lower a `<value> as <ty>` cast to a SIR `Cast` (numeric target) or a
/// pass-through (non-numeric target — the type checker already flagged it).
pub(super) fn lower_cast(value: SirExpr, from: &SirType, to: &SirType) -> SirExpr {
    // A cast touching fixed-point rescales by shifting the binary point
    // (§4.3, P0-3a): int→fixed shifts left F, fixed→int shifts right F, and
    // fixed→fixed shifts by the frac-bit difference.
    if matches!(from, SirType::Fixed { .. }) || matches!(to, SirType::Fixed { .. }) {
        let shift = (fixed_frac_bits(to) as i16 - fixed_frac_bits(from) as i16).clamp(-64, 64) as i8;
        let (to_width, signed) = sirtype_ctx(to);
        return SirExpr::FixedCast { inner: Box::new(value), shift, to_width, signed };
    }
    match sirtype_width_sign(to) {
        Some((to_width, signed)) => SirExpr::Cast { inner: Box::new(value), to_width, signed },
        None => value,
    }
}

/// The constant value of an integer or boolean literal pattern, if it is one.
pub(super) fn lit_const(e: &Expr) -> Option<u64> {
    match &e.kind {
        ExprKind::IntLit(n) => Some(*n),
        ExprKind::BoolLit(b) => Some(*b as u64),
        _ => None,
    }
}

/// Minimal type inference for initialiser expressions.
pub(super) fn infer_type_from_expr(expr: &Expr) -> SirType {
    match &expr.kind {
        ExprKind::BoolLit(_) => SirType::Bool,
        ExprKind::IntLit(_) => SirType::U32,
        // A bare decimal/voltage literal defaults to Q16.16 (§4.3 P0-3b).
        ExprKind::FixedLit(..) => SirType::Fixed { int_bits: 16, frac_bits: 16, signed: true },
        ExprKind::DurationLit(_) => SirType::Duration,
        ExprKind::StringLit(_) => SirType::Bytes,
        // `<e> as <T>` initialises a binding at the cast's target type.
        ExprKind::Cast(_, ty) => resolve_type_expr(ty),
        _ => SirType::U32,
    }
}

/// The constant integer value of an integer or duration literal (ns), if either.
pub(super) fn lit_int(kind: &ExprKind) -> Option<u64> {
    match kind {
        ExprKind::IntLit(n) | ExprKind::DurationLit(n) => Some(*n),
        _ => None,
    }
}

/// `true` if `callee` is the bare `now` identifier — `now()` reads the clock.
pub(super) fn is_now_call(callee: &Expr) -> bool {
    matches!(&callee.kind, ExprKind::Ident(id) if id.name == "now")
}

/// The time-logical category of a value (§4.5).  `Instant` and `Duration` are
/// both `u64` ns at runtime but obey distinct arithmetic rules; everything else
/// is `Scalar` (a plain integer / bool).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum TimeKind {
    Instant,
    Duration,
    Scalar,
}

pub(super) fn sirtype_time_kind(t: &SirType) -> TimeKind {
    match t {
        SirType::Instant => TimeKind::Instant,
        SirType::Duration => TimeKind::Duration,
        _ => TimeKind::Scalar,
    }
}

pub(super) fn time_kind_sirtype(k: TimeKind) -> Option<SirType> {
    match k {
        TimeKind::Instant => Some(SirType::Instant),
        TimeKind::Duration => Some(SirType::Duration),
        TimeKind::Scalar => None,
    }
}

/// Map a primitive type name to its SIR type; `None` for a name that is not a
/// built-in scalar/`bytes`/`instant`/`duration`/`float` type (an unknown or
/// misspelled name, or a user type used where a value type is expected).
pub(super) fn primitive_sirtype(name: &str) -> Option<SirType> {
    Some(match name {
        "u8" => SirType::U8,
        "u16" => SirType::U16,
        "u32" => SirType::U32,
        "u64" => SirType::U64,
        "s8" | "i8" => SirType::S8,
        "s16" | "i16" => SirType::S16,
        "s32" | "i32" => SirType::S32,
        "s64" | "i64" => SirType::S64,
        "bool" => SirType::Bool,
        "bytes" => SirType::Bytes,
        "instant" => SirType::Instant,
        "duration" => SirType::Duration,
        "float" | "f32" => SirType::F32,
        "f64" | "double" => SirType::F64,
        _ => return None,
    })
}

/// Resolve an AST type annotation to a SIR type **without** reporting errors.
/// An unknown name / unsupported construct falls back to `u32` so pure type
/// *inference* (e.g. `expr_sirtype`, `infer_type_from_expr`) can proceed; the
/// hard error is raised at the type's declaration site via
/// [`Resolver::resolve_type_expr_checked`] (audit #35 P7-3).
pub(super) fn resolve_type_expr(ty: &TypeExpr) -> SirType {
    match &ty.kind {
        TypeKind::Named(ident) => primitive_sirtype(&ident.name).unwrap_or(SirType::U32),
        // `fixed<I, F>` — 2's-complement binary fixed-point (§4.3, P0-3a).
        TypeKind::Fixed(int_bits, frac_bits) => SirType::Fixed {
            int_bits: (*int_bits).min(64) as u8,
            frac_bits: (*frac_bits).min(64) as u8,
            signed: true,
        },
        TypeKind::Unit => SirType::U8,
        TypeKind::Bytes => SirType::Bytes,
        // `ring<T, N>` — element size from `T`, capacity from the const `N`.
        TypeKind::Ring(elem, n) => {
            let elem_bytes = resolve_type_expr(elem).byte_size().clamp(1, 8) as u8;
            let cap = match const_eval(n, &HashMap::new()) {
                Some(ConstVal::Int(c)) => c as u32,
                _ => 0,
            };
            SirType::Ring { elem_bytes, cap }
        }
        // `buffer<N>` — N bytes of bounded storage (§5.3, P7-5a).
        TypeKind::Buffer(n) => {
            let bytes = match const_eval(n, &HashMap::new()) {
                Some(ConstVal::Int(c)) => c as u32,
                _ => 0,
            };
            SirType::Buffer { bytes }
        }
        // `pool<T, N>` — N slots of `T` (§5.3, P7-5b).
        TypeKind::Pool(elem, n) => {
            let elem_bytes = resolve_type_expr(elem).byte_size().clamp(1, 8) as u8;
            let cap = match const_eval(n, &HashMap::new()) {
                Some(ConstVal::Int(c)) => c as u32,
                _ => 0,
            };
            SirType::Pool { elem_bytes, cap }
        }
        _ => SirType::U32,
    }
}

/// Map an AST arithmetic operator to its SIR op + overflow disposition (§4.3).
/// `None` for non-arithmetic operators (comparisons / logic / div / rem), which
/// stay on the untyped `SirBinOp` path.
pub(super) fn arith_mode(op: BinOp) -> Option<(SirArithOp, OverflowMode)> {
    use OverflowMode::{Saturate, Trap, Wrap};
    use SirArithOp::{Add, Mul, Sub};
    Some(match op {
        BinOp::Add => (Add, Trap),
        BinOp::Sub => (Sub, Trap),
        BinOp::Mul => (Mul, Trap),
        BinOp::AddWrap => (Add, Wrap),
        BinOp::AddSat => (Add, Saturate),
        BinOp::SubWrap => (Sub, Wrap),
        BinOp::SubSat => (Sub, Saturate),
        BinOp::MulWrap => (Mul, Wrap),
        BinOp::MulSat => (Mul, Saturate),
        _ => return None,
    })
}

/// `(width_bits, signed)` for a SIR scalar type — drives overflow checks.
pub(super) fn sirtype_ctx(ty: &SirType) -> (u8, bool) {
    match ty {
        SirType::U8 => (8, false),
        SirType::U16 => (16, false),
        SirType::U32 => (32, false),
        SirType::U64 => (64, false),
        SirType::S8 => (8, true),
        SirType::S16 => (16, true),
        SirType::S32 => (32, true),
        SirType::S64 => (64, true),
        SirType::Bool => (8, false),
        SirType::Bytes => (32, false),
        // instant/duration are u64 ns at runtime (§4.5).
        SirType::Instant | SirType::Duration => (64, false),
        // a ring/buffer/pool is not a scalar — never an arithmetic operand.
        SirType::Ring { .. } | SirType::Buffer { .. } | SirType::Pool { .. } => (32, false),
        // floats are not integer-overflow-checked (§4.3); width is unused here.
        SirType::F32 | SirType::F64 => (32, false),
        // fixed-point is integer math at its storage width (§4.3, P0-3a), so
        // add/sub are overflow-checked exactly like the backing integer.
        SirType::Fixed { int_bits, frac_bits, signed } => {
            (SirType::fixed_storage_bits(*int_bits, *frac_bits) as u8, *signed)
        }
    }
}

pub(super) fn ast_binop_to_sir(op: BinOp) -> SirBinOp {
    match op {
        BinOp::Add | BinOp::AddWrap | BinOp::AddSat => SirBinOp::Add,
        BinOp::Sub | BinOp::SubWrap | BinOp::SubSat => SirBinOp::Sub,
        BinOp::Mul | BinOp::MulWrap | BinOp::MulSat => SirBinOp::Mul,
        BinOp::Div => SirBinOp::Div,
        BinOp::Rem => SirBinOp::Rem,
        BinOp::And => SirBinOp::And,
        BinOp::Or => SirBinOp::Or,
        BinOp::EqEq => SirBinOp::EqEq,
        BinOp::NotEq => SirBinOp::NotEq,
        BinOp::Lt => SirBinOp::Lt,
        BinOp::Le => SirBinOp::Le,
        BinOp::Gt => SirBinOp::Gt,
        BinOp::Ge => SirBinOp::Ge,
    }
}

// ─── Compile-time constant evaluation (§3.2/§4.1 `where`-constraints) ──────────

/// A constant value produced by [`const_eval`]. Durations are already lowered to
/// `IntLit` nanoseconds by the parser, so every numeric constant is an `Int`.
#[derive(Clone, Copy)]
pub(super) enum ConstVal {
    Int(u64),
    Bool(bool),
}

/// Evaluate a constraint/config expression against the bound config `env`.
/// Returns `None` for anything not reducible to a constant (the caller decides
/// whether that is an error in context); never panics on bad operands.
/// Visit every identifier name referenced in a constant expression.
pub(super) fn collect_idents(e: &Expr, f: &mut impl FnMut(&str)) {
    match &e.kind {
        ExprKind::Ident(id) => f(&id.name),
        ExprKind::BinOp { lhs, rhs, .. } => {
            collect_idents(lhs, f);
            collect_idents(rhs, f);
        }
        ExprKind::Not(inner) => collect_idents(inner, f),
        _ => {}
    }
}

pub(super) fn const_eval(e: &Expr, env: &HashMap<String, ConstVal>) -> Option<ConstVal> {
    match &e.kind {
        ExprKind::IntLit(n) | ExprKind::DurationLit(n) => Some(ConstVal::Int(*n)),
        ExprKind::BoolLit(b) => Some(ConstVal::Bool(*b)),
        ExprKind::Ident(id) => env.get(&id.name).copied(),
        ExprKind::Not(inner) => match const_eval(inner, env)? {
            ConstVal::Bool(b) => Some(ConstVal::Bool(!b)),
            ConstVal::Int(_) => None,
        },
        ExprKind::BinOp { op, lhs, rhs } => {
            let l = const_eval(lhs, env)?;
            let r = const_eval(rhs, env)?;
            const_binop(*op, l, r)
        }
        _ => None,
    }
}

pub(super) fn const_binop(op: BinOp, l: ConstVal, r: ConstVal) -> Option<ConstVal> {
    use ConstVal::{Bool, Int};
    match (op, l, r) {
        // Integer arithmetic (saturating on the rare overflow; div/rem-by-zero → None).
        (BinOp::Add | BinOp::AddWrap | BinOp::AddSat, Int(a), Int(b)) => Some(Int(a.saturating_add(b))),
        (BinOp::Sub | BinOp::SubWrap | BinOp::SubSat, Int(a), Int(b)) => Some(Int(a.saturating_sub(b))),
        (BinOp::Mul | BinOp::MulWrap | BinOp::MulSat, Int(a), Int(b)) => Some(Int(a.saturating_mul(b))),
        (BinOp::Div, Int(a), Int(b)) => (b != 0).then(|| Int(a / b)),
        (BinOp::Rem, Int(a), Int(b)) => (b != 0).then(|| Int(a % b)),
        // Integer comparisons → Bool.
        (BinOp::EqEq, Int(a), Int(b)) => Some(Bool(a == b)),
        (BinOp::NotEq, Int(a), Int(b)) => Some(Bool(a != b)),
        (BinOp::Lt, Int(a), Int(b)) => Some(Bool(a < b)),
        (BinOp::Le, Int(a), Int(b)) => Some(Bool(a <= b)),
        (BinOp::Gt, Int(a), Int(b)) => Some(Bool(a > b)),
        (BinOp::Ge, Int(a), Int(b)) => Some(Bool(a >= b)),
        // Boolean logic.
        (BinOp::And, Bool(a), Bool(b)) => Some(Bool(a && b)),
        (BinOp::Or, Bool(a), Bool(b)) => Some(Bool(a || b)),
        (BinOp::EqEq, Bool(a), Bool(b)) => Some(Bool(a == b)),
        (BinOp::NotEq, Bool(a), Bool(b)) => Some(Bool(a != b)),
        _ => None, // type-mismatched operands
    }
}

