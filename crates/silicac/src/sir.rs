//! Silica IR (SIR) — Phase 0.
//!
//! SIR sits *below* source sugar and *above* backend detail.  It is the
//! boundary described in §6.1 of the design: handlers lowered to explicit
//! control flow, device calls resolved to typed operations, comptime values
//! already folded.
//!
//! Phase 0 is deliberately thin — only the constructs needed for hello world
//! and a periodic timer reaction.  The types are designed to be extended
//! incrementally without breaking the C backend.

// ─── Module ───────────────────────────────────────────────────────────────────

/// The output of lowering one `.si` source file.
#[derive(Debug)]
pub struct SirModule {
    /// All reactions, in declaration order.
    pub reactions: Vec<SirReaction>,
    /// Module-level variables: `let` and `cell` declarations from programs.
    pub vars: Vec<SirVar>,
}

// ─── Reactions ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SirReaction {
    /// Unique index within the module.
    pub id: usize,
    pub trigger: SirTrigger,
    pub body: Vec<SirStmt>,
}

#[derive(Debug, Clone)]
pub enum SirTrigger {
    /// Fires once at program startup, before the event loop.
    SysStart,
    /// Fires periodically every `period_ns` nanoseconds.
    EveryNs(u64),
}

// ─── Statements ──────────────────────────────────────────────────────────────

#[derive(Debug)]
pub enum SirStmt {
    /// Call an intrinsic host device op.
    Intrinsic(SirIntrinsic),
    /// Assign `target = value`.
    Assign { target: SirPlace, value: SirExpr },
    /// `if <cond> { <then> }` — no else for now.
    If { cond: SirExpr, then: Vec<SirStmt> },
    /// `exit(code)` — terminate the process (host only).
    Exit(SirExpr),
}

/// An assignable place (left-hand side).
#[derive(Debug, Clone)]
pub enum SirPlace {
    /// A named local / cell variable.
    Var(String),
}

// ─── Intrinsics ──────────────────────────────────────────────────────────────

/// Built-in host-mode device operations.
///
/// These are compiler-wired on the host target; they lower to simple C calls.
/// On embedded targets they will be replaced by real device op calls.
#[derive(Debug)]
pub enum SirIntrinsic {
    /// `host_io.print(bytes)` — write bytes to stdout.
    HostIoPrint(SirExpr),
    /// `host_io.print_str(str)` — convenience: print a UTF-8 string to stdout.
    HostIoPrintStr(String),
    /// `host_io.flush()` — flush stdout.
    HostIoFlush,
}

// ─── Expressions ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub enum SirExpr {
    /// Boolean value.
    Bool(bool),
    /// Integer constant (up to 64-bit).
    U64(u64),
    /// Byte string constant — lowered from a string literal.
    Bytes(Vec<u8>),
    /// Load a named variable / cell.
    Load(String),
    /// `!<inner>` — boolean not.
    Not(Box<SirExpr>),
    /// Binary arithmetic / comparison.
    BinOp(SirBinOp, Box<SirExpr>, Box<SirExpr>),
}

#[derive(Debug, Clone, Copy)]
pub enum SirBinOp {
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

// ─── Variable declarations (for the C backend's prologue) ────────────────────

/// A module-level or reaction-level variable that needs storage.
#[derive(Debug)]
pub struct SirVar {
    pub name: String,
    pub ty: SirType,
    pub init: SirExpr,
    /// True if this variable is a `cell` (shared mutable state).
    pub is_cell: bool,
}

#[derive(Debug, Clone)]
pub enum SirType {
    Bool,
    U8,
    U16,
    U32,
    U64,
    S8,
    S16,
    S32,
    S64,
    Bytes,
}

impl SirType {
    pub fn c_type(&self) -> &'static str {
        match self {
            SirType::Bool => "uint8_t",
            SirType::U8 => "uint8_t",
            SirType::U16 => "uint16_t",
            SirType::U32 => "uint32_t",
            SirType::U64 => "uint64_t",
            SirType::S8 => "int8_t",
            SirType::S16 => "int16_t",
            SirType::S32 => "int32_t",
            SirType::S64 => "int64_t",
            SirType::Bytes => "const uint8_t *",
        }
    }
}
