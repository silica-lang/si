//! Name resolution and lowering to SIR — Phase 0.
//!
//! This pass walks the AST produced by the parser and:
//!
//! 1. Resolves identifiers to their definitions (devices, variables, cells,
//!    intrinsics).
//! 2. Validates that event references are known (e.g. `sys.start`).
//! 3. Lowers `program` reactions to `SirReaction` values.
//! 4. Lowers expressions to `SirExpr`.
//!
//! Errors carry a `Span` so the caller can print source-location context.

use std::collections::HashMap;

use crate::ast::*;
use crate::sir::*;

// ─── Error ────────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct ResolveError {
    pub span: Span,
    pub msg: String,
}

impl std::fmt::Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error at {}..{}: {}", self.span.start, self.span.end, self.msg)
    }
}

// ─── Scope ────────────────────────────────────────────────────────────────────

/// What a name resolves to in the current scope.
#[derive(Debug, Clone)]
enum Binding {
    /// A local `let` variable (immutable).
    Local(String, SirType),
    /// A `cell` (shared mutable).
    Cell(String, SirType),
    /// A host-mode intrinsic device.
    IntrinsicDevice(IntrinsicDevice),
    /// A `use` alias pointing to another name.
    #[allow(dead_code)]
    Alias(Vec<String>),
}

#[derive(Debug, Clone, Copy)]
enum IntrinsicDevice {
    HostIo,
    Sys,
}

/// Flat symbol table for one program.
struct Scope {
    bindings: HashMap<String, Binding>,
}

impl Scope {
    fn new() -> Self {
        Scope { bindings: HashMap::new() }
    }

    fn insert(&mut self, name: &str, binding: Binding) {
        self.bindings.insert(name.to_string(), binding);
    }

    fn lookup(&self, name: &str) -> Option<&Binding> {
        self.bindings.get(name)
    }
}

// ─── Resolver ─────────────────────────────────────────────────────────────────

pub struct Resolver {
    errors: Vec<ResolveError>,
}

impl Resolver {
    pub fn new() -> Self {
        Resolver { errors: Vec::new() }
    }

    fn err(&mut self, span: Span, msg: impl Into<String>) {
        self.errors.push(ResolveError { span, msg: msg.into() });
    }

    pub fn resolve_module(mut self, module: &Module) -> Result<SirModule, Vec<ResolveError>> {
        let mut reactions: Vec<SirReaction> = Vec::new();
        let mut vars: Vec<SirVar> = Vec::new();

        for item in &module.items {
            match item {
                Item::Program(prog) => {
                    self.resolve_program(prog, &mut reactions, &mut vars);
                }
                Item::Device(_) => {
                    // Device definitions are parsed but not lowered in Phase 0
                    // (only intrinsic devices are used on the host target).
                }
            }
        }

        if self.errors.is_empty() {
            Ok(SirModule { reactions, vars })
        } else {
            Err(self.errors)
        }
    }

    fn resolve_program(&mut self, prog: &ProgramDef, out: &mut Vec<SirReaction>, module_vars: &mut Vec<SirVar>) {
        let mut scope = Scope::new();

        // Pre-populate the scope with host intrinsics always available.
        scope.insert("sys", Binding::IntrinsicDevice(IntrinsicDevice::Sys));
        scope.insert("host_io", Binding::IntrinsicDevice(IntrinsicDevice::HostIo));

        // First pass: register all let/cell declarations so reactions can
        // reference them (forward-reference support is not needed in Phase 0,
        // but we at least register them before lowering reactions).
        let mut vars: Vec<SirVar> = Vec::new();

        for item in &prog.items {
            match item {
                ProgramItem::UseDecl(u) => {
                    // `use host_io as console` etc.
                    // Resolve the path and bind the alias.
                    let resolved = self.resolve_use_path(&u.path);
                    if let Some(binding) = resolved {
                        scope.insert(&u.alias.name, binding);
                    } else {
                        self.err(
                            u.span,
                            format!(
                                "cannot resolve use path '{}'",
                                u.path.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().join(".")
                            ),
                        );
                    }
                }
                ProgramItem::LetDecl(l) => {
                    let ty = infer_type_from_expr(&l.init);
                    let init = self.lower_expr(&l.init, &scope);
                    scope.insert(&l.name.name, Binding::Local(l.name.name.clone(), ty.clone()));
                    vars.push(SirVar {
                        name: l.name.name.clone(),
                        ty,
                        init,
                        is_cell: false,
                    });
                }
                ProgramItem::CellDecl(c) => {
                    let ty = resolve_type_expr(&c.ty);
                    let init = self.lower_expr(&c.init, &scope);
                    scope.insert(&c.name.name, Binding::Cell(c.name.name.clone(), ty.clone()));
                    vars.push(SirVar {
                        name: c.name.name.clone(),
                        ty,
                        init,
                        is_cell: true,
                    });
                }
                ProgramItem::Reaction(_) => {}
            }
        }

        // Second pass: lower reactions.
        for item in &prog.items {
            if let ProgramItem::Reaction(r) = item {
                let id = out.len();
                if let Some(reaction) = self.lower_reaction(id, r, &scope, &vars) {
                    out.push(reaction);
                }
            }
        }

        // Export vars to the module-level collection.
        module_vars.extend(vars);
    }

    fn resolve_use_path(&mut self, path: &[Ident]) -> Option<Binding> {
        // Intrinsic devices available by path.
        match path.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().as_slice() {
            ["host_io"] => Some(Binding::IntrinsicDevice(IntrinsicDevice::HostIo)),
            ["sys"] => Some(Binding::IntrinsicDevice(IntrinsicDevice::Sys)),
            _ => None,
        }
    }

    fn lower_reaction(
        &mut self,
        id: usize,
        r: &Reaction,
        scope: &Scope,
        vars: &[SirVar],
    ) -> Option<SirReaction> {
        let trigger = match &r.trigger {
            Trigger::On(event_ref) => {
                self.lower_event_trigger(event_ref, scope)?
            }
            Trigger::Every(dur) => SirTrigger::EveryNs(dur.to_ns()),
        };

        let body = self.lower_block(&r.body, scope, vars);

        Some(SirReaction { id, trigger, body })
    }

    fn lower_event_trigger(
        &mut self,
        event_ref: &EventRef,
        scope: &Scope,
    ) -> Option<SirTrigger> {
        // Resolve the device expression to a binding.
        let device_name = expr_root_ident(&event_ref.device);
        let event_name = &event_ref.event.name;

        let device_name = match device_name {
            Some(n) => n,
            None => {
                self.err(event_ref.span, "event device must be a simple identifier");
                return None;
            }
        };

        match scope.lookup(device_name) {
            Some(Binding::IntrinsicDevice(IntrinsicDevice::Sys)) => {
                match event_name.as_str() {
                    "start" => Some(SirTrigger::SysStart),
                    other => {
                        self.err(
                            event_ref.span,
                            format!("unknown sys event '{}'; known events: start", other),
                        );
                        None
                    }
                }
            }
            Some(other) => {
                self.err(
                    event_ref.span,
                    format!("'{}' is not an event-emitting device ({:?})", device_name, other),
                );
                None
            }
            None => {
                self.err(
                    event_ref.span,
                    format!("undefined device '{}'", device_name),
                );
                None
            }
        }
    }

    fn lower_block(&mut self, block: &Block, scope: &Scope, vars: &[SirVar]) -> Vec<SirStmt> {
        // Build a local scope for this block (extends the outer scope).
        let mut local_scope = Scope::new();
        for (name, binding) in &scope.bindings {
            local_scope.insert(name, binding.clone());
        }

        let mut stmts = Vec::new();
        for stmt in &block.stmts {
            if let Some(s) = self.lower_stmt(stmt, &mut local_scope, vars) {
                stmts.push(s);
            }
        }
        stmts
    }

    fn lower_stmt(
        &mut self,
        stmt: &Stmt,
        scope: &mut Scope,
        _vars: &[SirVar],
    ) -> Option<SirStmt> {
        match stmt {
            Stmt::Expr(expr) => self.lower_expr_stmt(expr, scope),
            Stmt::Let(l) => {
                let ty = infer_type_from_expr(&l.init);
                let value = self.lower_expr(&l.init, scope);
                scope.insert(&l.name.name, Binding::Local(l.name.name.clone(), ty));
                Some(SirStmt::Assign {
                    target: SirPlace::Var(l.name.name.clone()),
                    value,
                })
            }
            Stmt::Become(state, span) => {
                // `become <state>` — not lowered in Phase 0, emit nothing but
                // record it for later phases.
                // TODO Phase 1: generate typestate transition.
                let _ = (state, span);
                None
            }
            Stmt::Return(expr, _) => {
                // In Phase 0 all ops are host intrinsics; return is a no-op at
                // the top level.  Inside a device op body it terminates the op.
                if let Some(e) = expr {
                    let val = self.lower_expr(e, scope);
                    // Emit an assignment to a synthetic return variable.
                    Some(SirStmt::Assign {
                        target: SirPlace::Var("__ret".into()),
                        value: val,
                    })
                } else {
                    None
                }
            }
            Stmt::Exit(code, _) => {
                let val = self.lower_expr(code, scope);
                Some(SirStmt::Exit(val))
            }
        }
    }

    /// Lower an expression used as a statement (i.e. a call or assignment).
    fn lower_expr_stmt(&mut self, expr: &Expr, scope: &mut Scope) -> Option<SirStmt> {
        match &expr.kind {
            // Assignment: `x = value` or `x += value`
            ExprKind::Assign(lhs, rhs) => {
                let place = self.expr_to_place(lhs, scope)?;
                let value = self.lower_expr(rhs, scope);
                Some(SirStmt::Assign { target: place, value })
            }
            ExprKind::CompoundAssign(op, lhs, rhs) => {
                let place = self.expr_to_place(lhs, scope)?;
                let lhs_val = self.lower_expr(lhs, scope);
                let rhs_val = self.lower_expr(rhs, scope);
                let sir_op = ast_binop_to_sir(*op);
                let combined = SirExpr::BinOp(sir_op, Box::new(lhs_val), Box::new(rhs_val));
                Some(SirStmt::Assign { target: place, value: combined })
            }
            // A call expression: `device.op(args)` or `intrinsic.op(args)`.
            ExprKind::Call { callee, args, named: _ } => {
                // Decode `<device>.<method>(<args>)` pattern.
                if let ExprKind::Field(dev_expr, method) = &callee.kind {
                    if let Some(device_name) = expr_root_ident(dev_expr) {
                        match scope.lookup(device_name) {
                            Some(Binding::IntrinsicDevice(IntrinsicDevice::HostIo)) => {
                                return self.lower_host_io_call(method, args, scope);
                            }
                            Some(Binding::IntrinsicDevice(IntrinsicDevice::Sys)) => {
                                self.err(expr.span, "sys device has no callable ops");
                                return None;
                            }
                            None => {
                                self.err(
                                    dev_expr.span,
                                    format!("undefined device '{}'", device_name),
                                );
                                return None;
                            }
                            _ => {
                                // TODO Phase 1: user-defined device calls.
                                self.err(
                                    dev_expr.span,
                                    format!(
                                        "'{}' is not a device (user-defined devices not yet supported in Phase 0)",
                                        device_name
                                    ),
                                );
                                return None;
                            }
                        }
                    }
                }
                self.err(expr.span, "unsupported call expression form in Phase 0");
                None
            }
            // Any other expression used as a statement — just lower and discard.
            _ => {
                self.lower_expr(expr, scope);
                None
            }
        }
    }

    fn lower_host_io_call(
        &mut self,
        method: &Ident,
        args: &[Expr],
        scope: &Scope,
    ) -> Option<SirStmt> {
        match method.name.as_str() {
            "print" => {
                if args.len() != 1 {
                    self.err(method.span, "host_io.print takes exactly 1 argument");
                    return None;
                }
                // Fast path: string literal → HostIoPrintStr (no runtime bytes needed).
                if let ExprKind::StringLit(s) = &args[0].kind {
                    return Some(SirStmt::Intrinsic(SirIntrinsic::HostIoPrintStr(s.clone())));
                }
                let arg = self.lower_expr(&args[0], scope);
                Some(SirStmt::Intrinsic(SirIntrinsic::HostIoPrint(arg)))
            }
            "flush" => {
                if !args.is_empty() {
                    self.err(method.span, "host_io.flush takes no arguments");
                    return None;
                }
                Some(SirStmt::Intrinsic(SirIntrinsic::HostIoFlush))
            }
            other => {
                self.err(method.span, format!("unknown host_io op '{}'", other));
                None
            }
        }
    }

    fn lower_expr(&mut self, expr: &Expr, scope: &Scope) -> SirExpr {
        match &expr.kind {
            ExprKind::BoolLit(b) => SirExpr::Bool(*b),
            ExprKind::IntLit(n) => SirExpr::U64(*n),
            ExprKind::StringLit(s) => SirExpr::Bytes(s.as_bytes().to_vec()),
            ExprKind::Ident(ident) => {
                match scope.lookup(&ident.name) {
                    Some(Binding::Local(name, _)) | Some(Binding::Cell(name, _)) => {
                        SirExpr::Load(name.clone())
                    }
                    _ => {
                        // May be an unresolved name; emit a load and record error.
                        self.err(ident.span, format!("undefined variable '{}'", ident.name));
                        SirExpr::Load(ident.name.clone())
                    }
                }
            }
            ExprKind::Not(inner) => {
                let inner_sir = self.lower_expr(inner, scope);
                SirExpr::Not(Box::new(inner_sir))
            }
            ExprKind::BinOp { op, lhs, rhs } => {
                let l = self.lower_expr(lhs, scope);
                let r = self.lower_expr(rhs, scope);
                SirExpr::BinOp(ast_binop_to_sir(*op), Box::new(l), Box::new(r))
            }
            ExprKind::Assign(_lhs, rhs) => {
                // As an expression (not statement), this returns the rhs value.
                // The assignment side-effect is not captured here.
                self.lower_expr(rhs, scope)
            }
            ExprKind::CompoundAssign(_, _, rhs) => self.lower_expr(rhs, scope),
            ExprKind::Try(inner) => {
                // `?` propagation — Phase 0: just lower the inner expression.
                // TODO Phase 1: insert fault propagation logic.
                self.lower_expr(inner, scope)
            }
            ExprKind::Field(_, _) | ExprKind::Call { .. } => {
                // Field access / call as value — Phase 0 only supports these as
                // statements.  If they appear as a value expression (e.g. storing
                // the return of a device call), flag it for now.
                self.err(expr.span, "call/field expressions as values not yet supported");
                SirExpr::U64(0)
            }
        }
    }

    fn expr_to_place(&mut self, expr: &Expr, scope: &Scope) -> Option<SirPlace> {
        match &expr.kind {
            ExprKind::Ident(ident) => match scope.lookup(&ident.name) {
                Some(Binding::Local(name, _)) | Some(Binding::Cell(name, _)) => {
                    Some(SirPlace::Var(name.clone()))
                }
                _ => {
                    self.err(ident.span, format!("'{}' is not assignable", ident.name));
                    None
                }
            },
            _ => {
                self.err(expr.span, "expected an assignable place (variable name)");
                None
            }
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Extract the root identifier name from an expression like `foo` or `foo.bar`.
fn expr_root_ident(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Ident(ident) => Some(&ident.name),
        ExprKind::Field(inner, _) => expr_root_ident(inner),
        _ => None,
    }
}

/// Minimal type inference for initialiser expressions (Phase 0).
fn infer_type_from_expr(expr: &Expr) -> SirType {
    match &expr.kind {
        ExprKind::BoolLit(_) => SirType::Bool,
        ExprKind::IntLit(_) => SirType::U32,
        ExprKind::StringLit(_) => SirType::Bytes,
        _ => SirType::U32,
    }
}

/// Resolve an AST type annotation to a SIR type.
fn resolve_type_expr(ty: &TypeExpr) -> SirType {
    match &ty.kind {
        TypeKind::Named(ident) => match ident.name.as_str() {
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
            _ => SirType::U32, // unknown — default to u32 for now
        },
        TypeKind::Unit => SirType::U8, // unit is zero-sized; use u8 as placeholder
        TypeKind::Bytes => SirType::Bytes,
        _ => SirType::U32,
    }
}

fn ast_binop_to_sir(op: BinOp) -> SirBinOp {
    match op {
        BinOp::Add => SirBinOp::Add,
        BinOp::Sub => SirBinOp::Sub,
        BinOp::Mul => SirBinOp::Mul,
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

// ─── Public entry point ───────────────────────────────────────────────────────

pub fn resolve(module: &Module) -> Result<SirModule, Vec<ResolveError>> {
    Resolver::new().resolve_module(module)
}
