//! Silica AST — Phase 0 subset.
//!
//! Covers: `program`, `device` (ops-only, no regs), `on`/`every` reactions,
//! let/cell declarations, and enough expression syntax to call device ops and
//! write arithmetic.  Everything is deliberately kept in one file so the whole
//! AST can be read in one sitting.

use crate::lexer::DurationUnit;

// ─── Span ────────────────────────────────────────────────────────────────────

/// Byte range in the source file (for error messages).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Span { start, end }
    }

    pub fn merge(self, other: Span) -> Span {
        Span {
            start: self.start.min(other.start),
            end: self.end.max(other.end),
        }
    }
}

// ─── Identifier ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

impl PartialEq for Ident {
    fn eq(&self, other: &Self) -> bool { self.name == other.name }
}
impl Eq for Ident {}
impl std::hash::Hash for Ident {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) { self.name.hash(state); }
}

impl Ident {
    pub fn new(name: impl Into<String>, span: Span) -> Self {
        Ident { name: name.into(), span }
    }
}

impl std::fmt::Display for Ident {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)
    }
}

// ─── Duration ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct Duration {
    pub value: u64,
    pub unit: DurationUnit,
    pub span: Span,
}

impl Duration {
    pub fn to_ns(self) -> u64 {
        self.unit.to_ns(self.value)
    }
}

// ─── Top-level items ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Module {
    pub items: Vec<Item>,
}

#[derive(Debug, Clone)]
pub enum Item {
    Program(ProgramDef),
    Device(DeviceDef),
}

// ─── Program ─────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ProgramDef {
    pub name: Ident,
    pub span: Span,
    pub items: Vec<ProgramItem>,
}

#[derive(Debug, Clone)]
pub enum ProgramItem {
    /// `use <device-ident> as <local-ident>`
    UseDecl(UseDecl),
    /// `let <ident> = <expr>`
    LetDecl(LetDecl),
    /// `cell <ident> : <type> = <expr>`
    CellDecl(CellDecl),
    /// `on <event>` or `every <duration>` reaction block
    Reaction(Reaction),
}

#[derive(Debug, Clone)]
pub struct UseDecl {
    pub path: Vec<Ident>,    // e.g. ["board", "nucleo_f401re"]
    pub alias: Ident,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct LetDecl {
    pub name: Ident,
    pub ty: Option<TypeExpr>,
    pub init: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct CellDecl {
    pub name: Ident,
    pub ty: TypeExpr,
    pub init: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Reaction {
    pub trigger: Trigger,
    pub fault_disp: Option<FaultDisp>,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Trigger {
    /// `on <device>.<event>`
    On(EventRef),
    /// `every <duration>`
    Every(Duration),
}

/// A reference to a device event: `device_expr.event_name`.
#[derive(Debug, Clone)]
pub struct EventRef {
    /// The leading device expression (e.g. `sys`, `btn`).
    pub device: Expr,
    /// The event name (e.g. `start`, `falling`).
    pub event: Ident,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct FaultDisp {
    pub kind: FaultDispKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum FaultDispKind {
    Retry { max: Option<u32> },
    Skip,
    Safe,
    Escalate,
}

// ─── Device ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct DeviceDef {
    pub name: Ident,
    pub implements: Vec<Ident>,
    pub span: Span,
    pub sections: DeviceSections,
}

#[derive(Debug, Clone, Default)]
pub struct DeviceSections {
    pub ops: Option<OpsSection>,
    pub states: Option<StatesSection>,
    pub safe_state: Option<Ident>,
}

#[derive(Debug, Clone)]
pub struct OpsSection {
    pub items: Vec<OpsItem>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum OpsItem {
    Op(OpDecl),
    // emits: deferred
}

#[derive(Debug, Clone)]
pub struct OpDecl {
    pub name: Ident,
    pub params: Vec<Param>,
    pub when: Option<Ident>,
    pub ret: ReturnType,
    pub yields: bool,
    pub body: Block,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Param {
    pub name: Ident,
    pub ty: TypeExpr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ReturnType {
    pub ty: TypeExpr,
    pub fallible: bool, // `-> T or fault`
}

#[derive(Debug, Clone)]
pub struct StatesSection {
    pub states: Vec<Ident>,
    pub span: Span,
}

// ─── Statements ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Block {
    pub stmts: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// `let <name> [: <type>] = <expr>`
    Let(LetDecl),
    /// An expression used as a statement (typically a call or assignment).
    Expr(Expr),
    /// `become <state>`
    Become(Ident, Span),
    /// `return <expr>`
    Return(Option<Expr>, Span),
    /// `exit(<code>)` — host intrinsic
    Exit(Expr, Span),
}

// ─── Expressions ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    /// A simple identifier: `counter`, `led`.
    Ident(Ident),
    /// `<lhs>.<field>` — field access.
    Field(Box<Expr>, Ident),
    /// `<callee>(<args>)` — function/op call.
    Call {
        callee: Box<Expr>,
        args: Vec<Expr>,
        named: Vec<(Ident, Expr)>, // named args: `mul = 84`
    },
    /// Integer literal.
    IntLit(u64),
    /// Boolean literal.
    BoolLit(bool),
    /// String literal.
    StringLit(String),
    /// Binary operation.
    BinOp {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    /// Unary `not <expr>`.
    Not(Box<Expr>),
    /// `<expr>?` — fault propagation.
    Try(Box<Expr>),
    /// `<expr> = <expr>` — assignment.
    Assign(Box<Expr>, Box<Expr>),
    /// `<expr> += <expr>` — compound assignment.
    CompoundAssign(BinOp, Box<Expr>, Box<Expr>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    And,
    Or,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
}

// ─── Types ───────────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct TypeExpr {
    pub kind: TypeKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum TypeKind {
    /// Named type: `u8`, `bool`, `uart`, etc.
    Named(Ident),
    /// `()` — unit type.
    Unit,
    /// `bytes` — byte slice.
    Bytes,
    /// `buffer<N>`
    Buffer(Box<Expr>),
    /// `fixed<I, F>` — fixed-point.
    Fixed(u32, u32),
    /// `enum { a, b, c }` — anonymous enum.
    AnonEnum(Vec<Ident>),
}
