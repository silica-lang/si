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
    Board(BoardDef),
    /// `interface <name> { ... }` — parsed as a thin stub so a top-level
    /// interface declaration is not a syntax error (§3.5).  The body is not
    /// consumed by the slice.
    Interface(InterfaceDef),
    /// `sim <name> for <program> { inject ...; run until ... }` — the
    /// deterministic host-simulation script (§7.1).
    Sim(SimDef),
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
    /// What kind of entity is being imported.
    pub kind: UseKind,
    pub path: Vec<Ident>,    // e.g. ["nucleo_f401re"]
    pub alias: Ident,
    pub span: Span,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseKind {
    /// `use board <name> as <alias>` — imports a board declaration.
    Board,
    /// `use <path> as <alias>` — imports an intrinsic device / module.
    Plain,
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
    /// `within <d>` deadline budget (§4.5/§5.6): the reaction must return to idle
    /// within `d` of firing, else it overruns and the watchdog resets the system.
    pub within: Option<Duration>,
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
    pub regs: Option<RegsSection>,
    pub config: Option<ConfigSection>,
    pub needs: Option<NeedsSection>,
    pub ops: Option<OpsSection>,
    pub states: Option<StatesSection>,
    pub safe_state: Option<Ident>,
    pub emits: Vec<EmitDecl>,
}

// ─── Device: regs (§4.2) ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct RegsSection {
    pub regs: Vec<RegDecl>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct RegDecl {
    pub name: Ident,
    /// Register storage width, in bits (8/16/32) — from `reg8`/`reg16`/`reg32`.
    pub width: u8,
    /// Byte offset from the device's base address.
    pub offset: u64,
    pub access: RegAccess,
    pub fields: Vec<FieldDecl>,
    pub span: Span,
}

/// Access semantics of a register or field (§4.2/D04).  Getting these wrong
/// silently corrupts hardware state, so they are first-class even in the sim.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegAccess {
    Ro,
    Wo,
    Rw,
    /// write-1-to-clear
    W1c,
    /// read-to-clear / read-has-side-effects
    Rc,
}

#[derive(Debug, Clone)]
pub struct FieldDecl {
    pub name: Ident,
    pub bits: BitSpec,
    pub access: Option<RegAccess>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy)]
pub enum BitSpec {
    /// `bit[n]`
    Bit(u8),
    /// `field[hi:lo]`
    Range(u8, u8),
}

// ─── Device: config (§3.2) ─────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct ConfigSection {
    pub fields: Vec<ConfigField>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ConfigField {
    pub name: Ident,
    pub ty: TypeExpr,
    /// `where <expr>` constraint, const-evaluated and enforced at instantiation (§4.1).
    pub constraint: Option<Expr>,
    pub default: Option<Expr>,
    pub span: Span,
}

// ─── Device: needs (§4.1) ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct NeedsSection {
    pub needs: Vec<NeedDecl>,
    pub span: Span,
}

/// A declared typed relation the device requires (`clock : clock_source`).
#[derive(Debug, Clone)]
pub struct NeedDecl {
    pub name: Ident,
    pub ty: Ident,
    pub span: Span,
}

// ─── Device: emits (§4.1) ──────────────────────────────────────────────────────

/// `emits <name> : event [when <expr>]` — an event source the device exposes
/// that `on <instance>.<name>` reactions bind to.
#[derive(Debug, Clone)]
pub struct EmitDecl {
    pub name: Ident,
    pub when: Option<Expr>,
    pub span: Span,
}

// ─── Interface (§3.5) ──────────────────────────────────────────────────────────

/// A named contract a device provides (`implements i2c`) or requires
/// (`needs bus: i2c`): a set of op signatures (bodies empty) + type members.
#[derive(Debug, Clone)]
pub struct InterfaceDef {
    pub name: Ident,
    /// Op signatures the interface declares (`op transfer(...) -> ... yields`).
    pub ops: Vec<OpDecl>,
    /// `type address = u7` members (parsed; the alias target is recorded).
    pub types: Vec<(Ident, Ident)>,
    pub span: Span,
}

// ─── Board (§3.3) ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct BoardDef {
    pub name: Ident,
    pub soc: Option<SocDef>,
    /// Peripheral instances: `gpio_a : gpio at 0x... { ... }`.
    pub instances: Vec<Instance>,
    /// Pad multiplexing groups: `pinctrl { name : pinmux { ... } }`.
    pub pinctrl: Vec<PinmuxDef>,
    /// Single-pin bindings: `led_user : gpio.pin = gpio_a.pin(5) as output`.
    pub pin_bindings: Vec<PinBinding>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct SocDef {
    pub name: Ident,
    pub memory: Vec<RegionDecl>,
    pub clocks: Vec<ClockDecl>,
    pub irqs: Vec<IrqDecl>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct RegionDecl {
    pub name: Ident,
    pub at: u64,
    pub size: u64,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct ClockDecl {
    pub name: Ident,
    /// `8MHz` or `pll(hse, mul = 84, div = 8)` — left as an expr for the slice.
    pub init: Expr,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct IrqDecl {
    pub name: Ident,
    pub num: u32,
    pub span: Span,
}

/// A typed peripheral instance on a board.
#[derive(Debug, Clone)]
pub struct Instance {
    pub name: Ident,
    pub device_ty: Ident,
    /// MMIO base address (`at 0x...`).
    pub at: Option<u64>,
    pub config: Vec<(Ident, Expr)>,
    /// `needs { clock = soc.sysclk }` — each entry maps a need name to a path.
    pub needs: Vec<(Ident, Vec<Ident>)>,
    pub span: Span,
}

/// `<name> : gpio.pin = <port>.pin(<index>) as <dir> [pulling up|down]`.
#[derive(Debug, Clone)]
pub struct PinBinding {
    pub name: Ident,
    pub port: Ident,
    pub index: u64,
    pub dir: PinDir,
    pub pull: Pull,
    pub alt: Option<u32>,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinDir {
    Input,
    Output,
    /// Used by a pinmux alternate-function assignment.
    Alt,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pull {
    None,
    Up,
    Down,
}

/// `pinctrl` group of alternate-function pin assignments.
#[derive(Debug, Clone)]
pub struct PinmuxDef {
    pub name: Ident,
    pub pins: Vec<PinAssign>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct PinAssign {
    pub role: Ident,
    pub port: Ident,
    pub index: u64,
    pub alt: Option<u32>,
    pub pull: Pull,
    pub span: Span,
}

// ─── Sim script (§7.1) ──────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SimDef {
    pub name: Ident,
    /// The program this scenario drives.
    pub program: Ident,
    pub injections: Vec<Injection>,
    /// `inject fault <addr> at <duration>` — Layer-3 hardware-fault injections.
    pub faults: Vec<FaultInjection>,
    /// `inject bus_fault <code> times <n>` — fail the next n bus transactions
    /// with `code` (for exercising the fault path / retry, §12).
    pub bus_faults: Vec<(Ident, u32)>,
    /// `inject bus_hang times <n>` — hang the next n bus transactions (a wedged
    /// bus that never completes), for exercising the watchdog (§5.6).
    pub bus_hangs: u32,
    /// `run until <duration>` — virtual-time horizon.
    pub run_until: Option<Duration>,
    pub span: Span,
}

/// `inject <device>.<event> at <duration>`.
#[derive(Debug, Clone)]
pub struct Injection {
    pub event: EventRef,
    pub at: Duration,
    pub span: Span,
}

/// `inject fault <addr> at <duration>` — a simulated Layer-3 hardware fault to a
/// memory address, decoded against the address-ownership map (§5.4).
#[derive(Debug, Clone)]
pub struct FaultInjection {
    pub addr: u64,
    pub at: Duration,
    pub span: Span,
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
    /// Declared fault codes (§4.4/D14): `or fault{nak, timeout}`. Empty when
    /// `or fault` is written without an explicit code set.
    pub fault_codes: Vec<Ident>,
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
    /// `poll <cond> within <d> else fault <code>` — a bounded *busy-wait* that
    /// does **not** yield (§3.2/§5.2): spin until `cond` holds, or raise fault
    /// `code` once the bound elapses.  (Its suspending sibling is `await`.)
    Poll { cond: Expr, within: Duration, fault_code: Ident, span: Span },
    /// `atomic { <stmts> }` — an explicit multi-statement critical section
    /// (§5.5/D03): the whole block runs at the priority ceiling of every cell it
    /// touches, so a group of cell updates is indivisible w.r.t. other reactions.
    Atomic(Block, Span),
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
    /// Explicit wrapping/saturating arithmetic (§4.3).  Plain Add/Sub/Mul trap on
    /// overflow by default (SIL-004); these opt out per-operation.
    AddWrap,
    AddSat,
    SubWrap,
    SubSat,
    MulWrap,
    MulSat,
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
