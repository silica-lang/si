//! Resolver typecheck pass (audit #35 P7-9b) — the per-value / per-statement
//! semantic gates split out of the resolver driver, behind the P7-9a lowering
//! boundary.  Holds the time-kind checker (`instant`/`duration` algebra) and the
//! value-type checker (widths/signs), with their mixed-sign, implicit-narrowing,
//! and literal-range diagnostics.  These are methods on `Resolver` (they read the
//! same board/scope state as the driver and lowering); moving them to their own
//! file makes the typecheck ↔ lowering seam explicit.  Behavior-identical.

use super::lower::*;
use super::*;

impl Resolver {
    /// Recursively compute an expression's time-logical kind (§4.5), emitting an
    /// error on an illegal `instant`/`duration` combination.  Called once per
    /// statement-root expression, so each node is visited exactly once.
    pub(super) fn time_kind(&mut self, expr: &Expr, scope: &Scope) -> TimeKind {
        match &expr.kind {
            ExprKind::Call { callee, args, .. } if is_now_call(callee) => {
                if !args.is_empty() {
                    self.err(expr.span, "now() takes no arguments");
                }
                TimeKind::Instant
            }
            ExprKind::Ident(id) => match scope.lookup(&id.name) {
                Some(Binding::Local(_, t)) | Some(Binding::Cell(_, t)) => sirtype_time_kind(t),
                _ => TimeKind::Scalar,
            },
            ExprKind::DurationLit(_) => TimeKind::Duration,
            ExprKind::BinOp { op, lhs, rhs } => {
                let l = self.time_kind(lhs, scope);
                let r = self.time_kind(rhs, scope);
                self.combine_time(*op, l, r, expr.span)
            }
            ExprKind::Not(inner) => {
                let _ = self.time_kind(inner, scope);
                TimeKind::Scalar
            }
            ExprKind::Assign(lhs, rhs) => {
                let target = self.place_time_kind(lhs, scope);
                let val = self.time_kind(rhs, scope);
                self.check_assign_time(target, val, expr.span);
                val
            }
            ExprKind::CompoundAssign(op, lhs, rhs) => {
                let target = self.place_time_kind(lhs, scope);
                let r = self.time_kind(rhs, scope);
                let combined = self.combine_time(*op, target, r, expr.span);
                self.check_assign_time(target, combined, expr.span);
                combined
            }
            ExprKind::Try(inner) => self.time_kind(inner, scope),
            _ => TimeKind::Scalar,
        }
    }

    /// The time-kind of an assignment target (a cell/local's declared type).
    pub(super) fn place_time_kind(&self, lhs: &Expr, scope: &Scope) -> TimeKind {
        match &lhs.kind {
            ExprKind::Ident(id) => match scope.lookup(&id.name) {
                Some(Binding::Local(_, t)) | Some(Binding::Cell(_, t)) => sirtype_time_kind(t),
                _ => TimeKind::Scalar,
            },
            _ => TimeKind::Scalar,
        }
    }

    /// The result kind of `l <op> r`, enforcing the §4.5 arithmetic rules.
    pub(super) fn combine_time(&mut self, op: BinOp, l: TimeKind, r: TimeKind, span: Span) -> TimeKind {
        use TimeKind::{Duration, Instant, Scalar};
        if l == Scalar && r == Scalar {
            return Scalar; // ordinary integer arithmetic — no time rules apply
        }
        match op {
            BinOp::Add | BinOp::AddWrap | BinOp::AddSat => match (l, r) {
                (Instant, Instant) => {
                    self.err(span, "cannot add two instants (§4.5) — add a duration to an instant");
                    Instant
                }
                (Instant, Duration) | (Duration, Instant) => Instant,
                (Instant, Scalar) | (Scalar, Instant) => {
                    self.err(span, "can only add a duration to an instant, not a bare integer (§4.5)");
                    Instant
                }
                _ => Duration, // duration + duration / scalar
            },
            BinOp::Sub | BinOp::SubWrap | BinOp::SubSat => match (l, r) {
                (Instant, Instant) => Duration, // elapsed time between two instants
                (Instant, Duration) => Instant, // instant minus a span
                (Instant, Scalar) => {
                    self.err(span, "can only subtract a duration from an instant, not a bare integer (§4.5)");
                    Instant
                }
                (_, Instant) => {
                    self.err(span, "cannot subtract an instant from a non-instant (§4.5)");
                    Instant
                }
                _ => Duration,
            },
            BinOp::Mul | BinOp::MulWrap | BinOp::MulSat | BinOp::Div | BinOp::Rem => {
                if l == Instant || r == Instant {
                    self.err(span, "cannot scale an instant (§4.5) — only duration arithmetic is defined");
                    Instant
                } else {
                    Duration
                }
            }
            BinOp::EqEq | BinOp::NotEq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                if matches!((l, r), (Instant, Instant)) {
                    Scalar // comparing two instants is fine → bool
                } else if l == Instant || r == Instant {
                    self.err(span, "cannot compare an instant with a non-instant (§4.5)");
                    Scalar
                } else {
                    Scalar
                }
            }
            BinOp::And | BinOp::Or => Scalar,
        }
    }

    /// Reject assigning an `instant` to a non-instant target (or vice versa).
    pub(super) fn check_assign_time(&mut self, target: TimeKind, val: TimeKind, span: Span) {
        let bad = matches!(
            (target, val),
            (TimeKind::Instant, TimeKind::Duration)
                | (TimeKind::Instant, TimeKind::Scalar)
                | (TimeKind::Duration, TimeKind::Instant)
                | (TimeKind::Scalar, TimeKind::Instant)
        );
        if bad {
            self.err(
                span,
                "instant and non-instant values don't mix in an assignment (§4.5)",
            );
        }
    }

    /// Recursively compute an expression's width/sign category (§4.3), emitting
    /// errors on implicit narrowing and mixed signed/unsigned operands.  Called
    /// once per statement-root expression so each node is visited once.
    /// Best-effort source `SirType` of an expression — used by `lower_cast` to
    /// find the fixed-point scale being converted *from* (§4.3, P0-3a).
    pub(super) fn expr_sirtype(&self, expr: &Expr, scope: &Scope) -> SirType {
        match &expr.kind {
            ExprKind::Ident(id) => match scope.lookup(&id.name) {
                Some(Binding::Local(_, t)) | Some(Binding::Cell(_, t)) => t.clone(),
                _ => SirType::U32,
            },
            ExprKind::Cast(_, ty) => resolve_type_expr(ty),
            ExprKind::Try(inner) => self.expr_sirtype(inner, scope),
            ExprKind::BinOp { lhs, .. } => self.expr_sirtype(lhs, scope),
            ExprKind::FixedLit(..) => SirType::Fixed { int_bits: 16, frac_bits: 16, signed: true },
            // A composed-device op call adopts the op's declared return type — so
            // `let t = sensor.read_temp_c()` is `fixed<…>`, not the default u32.
            ExprKind::Call { callee, .. } => {
                if let ExprKind::Field(dev_expr, op) = &callee.kind {
                    if let Some(root) = expr_root_ident(dev_expr) {
                        if let Some(Binding::Device(inst)) = scope.lookup(root) {
                            if let Some(op_decl) = self.find_op(&inst.ty, &op.name) {
                                return resolve_type_expr(&op_decl.ret.ty);
                            }
                        }
                    }
                }
                SirType::U32
            }
            _ => SirType::U32,
        }
    }

    pub(super) fn value_type(&mut self, expr: &Expr, scope: &Scope) -> ValType {
        match &expr.kind {
            ExprKind::IntLit(_) => ValType::Literal,
            // A fixed literal is a literal — it adopts the target fixed scale.
            ExprKind::FixedLit(..) => ValType::Literal,
            ExprKind::Ident(id) => match scope.lookup(&id.name) {
                Some(Binding::Local(_, t)) | Some(Binding::Cell(_, t)) => sirtype_valtype(t),
                _ => ValType::Flexible,
            },
            ExprKind::Cast(inner, ty) => {
                let _ = self.value_type(inner, scope); // check inside the cast
                sirtype_valtype(&self.resolve_type_expr_checked(ty))
            }
            ExprKind::Not(inner) => {
                let _ = self.value_type(inner, scope);
                ValType::Flexible
            }
            ExprKind::BinOp { op, lhs, rhs } => {
                let l = self.value_type(lhs, scope);
                let r = self.value_type(rhs, scope);
                self.check_mixed_sign(l, r, expr.span);
                if is_comparison(*op) || matches!(op, BinOp::And | BinOp::Or) {
                    ValType::Flexible // boolean result
                } else {
                    arith_result(l, r)
                }
            }
            ExprKind::Assign(lhs, rhs) => {
                let target = self.place_valtype(lhs, scope);
                let v = self.value_type(rhs, scope);
                self.check_assign_type(target, v, expr.span);
                self.check_literal_range(target, rhs, expr.span);
                v
            }
            ExprKind::CompoundAssign(op, lhs, rhs) => {
                let target = self.place_valtype(lhs, scope);
                let r = self.value_type(rhs, scope);
                self.check_mixed_sign(target, r, expr.span);
                let combined = arith_result(target, r);
                self.check_assign_type(target, combined, expr.span);
                let _ = op;
                combined
            }
            ExprKind::Try(inner) => self.value_type(inner, scope),
            ExprKind::Call { callee, args, .. } => {
                let _ = self.value_type(callee, scope);
                for a in args {
                    let _ = self.value_type(a, scope);
                }
                ValType::Flexible
            }
            ExprKind::Field(base, _) => {
                let _ = self.value_type(base, scope);
                ValType::Flexible
            }
            _ => ValType::Flexible, // bool / string / etc.
        }
    }

    pub(super) fn place_valtype(&self, lhs: &Expr, scope: &Scope) -> ValType {
        match &lhs.kind {
            ExprKind::Ident(id) => match scope.lookup(&id.name) {
                Some(Binding::Local(_, t)) | Some(Binding::Cell(_, t)) => sirtype_valtype(t),
                _ => ValType::Flexible,
            },
            _ => ValType::Flexible,
        }
    }

    /// Reject `signed <op> unsigned` between two declared-typed operands (§4.3).
    pub(super) fn check_mixed_sign(&mut self, l: ValType, r: ValType, span: Span) {
        use ValType::{Fixed, Int, Literal};
        match (l, r) {
            (Int { signed: sl, .. }, Int { signed: sr, .. }) if sl != sr => {
                self.err(span, "mixed signed/unsigned operands (§4.3) — add an explicit cast");
            }
            // Two fixed operands must share the exact same scale to add/sub.
            (Fixed { int_bits: il, frac_bits: fl, signed: sl }, Fixed { int_bits: ir, frac_bits: fr, signed: sr }) => {
                if (il, fl, sl) != (ir, fr, sr) {
                    self.err(span, "fixed-point operands have different `fixed<I,F>` scales (§4.3) — add an explicit cast");
                }
            }
            // Fixed mixed with a *declared* integer needs an explicit cast (a
            // bare literal is fine — it adopts the fixed scale).
            (Fixed { .. }, Int { .. }) | (Int { .. }, Fixed { .. }) => {
                self.err(span, "cannot mix fixed-point and integer operands (§4.3) — add an explicit cast");
            }
            (Fixed { .. }, Literal) | (Literal, Fixed { .. }) => {}
            _ => {}
        }
    }

    /// Reject an implicit narrowing or sign change on assignment (§4.3).
    pub(super) fn check_assign_type(&mut self, target: ValType, v: ValType, span: Span) {
        use ValType::{Fixed, Int, Literal};
        match (target, v) {
            (Int { width: tw, signed: ts }, Int { width: vw, signed: vs }) => {
                if ts != vs {
                    self.err(span, "assigning across signedness (§4.3) — add an explicit cast");
                } else if vw > tw {
                    self.err(
                        span,
                        format!("implicit narrowing from {vw}-bit to {tw}-bit (§4.3) — add an explicit cast"),
                    );
                }
            }
            // Fixed target accepts the same fixed scale or a bare literal; an
            // integer or a different scale needs an explicit cast.
            (Fixed { int_bits: ti, frac_bits: tf, signed: ts }, Fixed { int_bits: vi, frac_bits: vf, signed: vs }) => {
                if (ti, tf, ts) != (vi, vf, vs) {
                    self.err(span, "assigning a different `fixed<I,F>` scale (§4.3) — add an explicit cast");
                }
            }
            (Fixed { .. }, Int { .. }) => {
                self.err(span, "assigning an integer to a fixed-point binding (§4.3) — cast with `as fixed<…>`");
            }
            (Int { .. }, Fixed { .. }) => {
                self.err(span, "assigning fixed-point to an integer binding (§4.3) — cast with `as <int>`");
            }
            (Fixed { .. }, Literal) | (Literal, _) | (_, Literal) => {}
            _ => {}
        }
    }

    /// Reject an integer literal that does not fit its target type (§4.3).
    pub(super) fn check_literal_range(&mut self, target: ValType, rhs: &Expr, span: Span) {
        if let (ValType::Int { width, signed }, ExprKind::IntLit(n)) = (target, &rhs.kind) {
            let max: u128 = if signed { (1u128 << (width - 1)) - 1 } else { (1u128 << width) - 1 };
            if (*n as u128) > max {
                self.err(
                    span,
                    format!(
                        "literal {n} does not fit in a {width}-bit {} integer (§4.3)",
                        if signed { "signed" } else { "unsigned" }
                    ),
                );
            }
        }
    }

}
