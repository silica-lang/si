//! Silica IR (SIR).
//!
//! SIR sits *below* source sugar and *above* backend detail.  It is the
//! boundary described in §6.1 of the design: handlers lowered to explicit
//! control flow, device accesses resolved to typed register operations, the
//! schedule and event sources resolved, comptime values folded.
//!
//! SIR is **target-neutral** (the load-bearing invariant of this slice, §6.2):
//! a register access is `{device, offset, mask, shift, access}` — never a host
//! pointer or a C expression — so the two consumers (`backend::c`, the
//! metal-direction printer, and `sim`, the host interpreter) service the *same*
//! node.  The simulator masks/shifts into a mock register array; a future C/LLVM
//! backend emits a `volatile` MMIO load/store with barriers.

// ─── Module ───────────────────────────────────────────────────────────────────

/// The output of lowering one `.si` source file.
#[derive(Debug, Default)]
pub struct SirModule {
    /// All reactions, in declaration order.
    pub reactions: Vec<SirReaction>,
    /// Module-level variables: `let` and `cell` declarations from programs.
    pub vars: Vec<SirVar>,
    /// Resolved device instances (from the board), keyed by `id`.
    pub devices: Vec<SirDevice>,
    /// Resolved event sources (e.g. a GPIO pin's `falling`), keyed by `id`.
    pub events: Vec<SirEvent>,
    /// Per-cell concurrency analysis results (§5.5).
    pub cells: Vec<CellInfo>,
    /// Scripted event injections from a `sim` block (§7.1).
    pub injections: Vec<SirInjection>,
    /// Virtual-time horizon from `run until <dur>` (None ⇒ run until idle).
    pub run_until_ns: Option<u64>,
    /// SoC memory regions (flash/RAM), for the generated linker script (§6.4).
    pub memory: Vec<SirRegion>,
    /// Resolved pin bindings, for generated startup pin configuration (§6.4).
    pub pins: Vec<SirPin>,
}

/// A board pin binding (`led_user : gpio.pin = gpio_a.pin(5) as output`),
/// resolved for startup configuration: the generated reset handler sets each
/// output pin's direction before running `sys.start`.
#[derive(Debug, Clone)]
pub struct SirPin {
    pub device: usize,
    pub index: u8,
    pub output: bool,
    pub pull_up: bool,
    /// Offset + width of the device's direction register (1 = output).
    pub dir_reg_offset: u64,
    pub dir_reg_width: u8,
}

/// A named memory region with a base address and size, from `board.soc.memory`.
#[derive(Debug, Clone)]
pub struct SirRegion {
    pub name: String,
    pub origin: u64,
    pub size: u64,
}

impl SirRegion {
    /// Heuristic: the executable region (lowest origin / contains the reset
    /// vector) is flash; the region at the SRAM origin is RAM.  Used by the
    /// linker-script generator (§6.4).
    pub fn is_ram(&self) -> bool {
        // ARMv7-M SRAM is the 0x2000_0000 bit-band region.
        (self.origin & 0xF000_0000) == 0x2000_0000
    }
}

// ─── Reactions ────────────────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SirReaction {
    /// Unique index within the module.
    pub id: usize,
    pub trigger: SirTrigger,
    pub body: Vec<SirStmt>,
    /// Static priority for deterministic scheduling and the priority-ceiling
    /// protocol (§5.1, §5.5).  Higher = more urgent.
    pub priority: u8,
}

#[derive(Debug, Clone)]
pub enum SirTrigger {
    /// Fires once at program startup, before the event loop.
    SysStart,
    /// Fires periodically every `period_ns` nanoseconds.
    EveryNs(u64),
    /// Fires when the named event source fires (resolved to an event id).
    Event(usize),
}

// ─── Devices & registers ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SirDevice {
    pub id: usize,
    /// Instance name from the board (e.g. `gpio_a`).
    pub name: String,
    /// MMIO base address (`at 0x...`), if any.
    pub base_addr: Option<u64>,
    pub kind: SirDeviceKind,
    /// Resolved register layout (the std-lib device type's `regs`).
    pub regs: Vec<SirReg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SirDeviceKind {
    Gpio,
    Timer,
    Generic,
}

#[derive(Debug, Clone)]
pub struct SirReg {
    pub name: String,
    pub offset: u64,
    /// Storage width in bits (8/16/32).
    pub width: u8,
    pub access: SirRegAccess,
    /// Power-on reset value (§4.2 `reset=`); 0 if unspecified.
    pub reset: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SirRegAccess {
    Ro,
    Wo,
    Rw,
    W1c,
    Rc,
}

// ─── Events & injections ───────────────────────────────────────────────────────

#[derive(Debug)]
pub struct SirEvent {
    pub id: usize,
    /// Event name as declared in the device `emits` (e.g. `falling`).
    pub name: String,
    /// The device instance the event source belongs to.
    pub device: usize,
    /// The pin index, for GPIO pin events.
    pub pin_index: Option<u8>,
}

#[derive(Debug)]
pub struct SirInjection {
    pub at_ns: u64,
    pub event: usize,
}

// ─── Cell concurrency analysis (§5.5) ────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct CellInfo {
    pub name: String,
    /// Priority-ceiling = max priority of the reactions that touch this cell.
    pub ceiling: u8,
    /// True if exactly one reaction touches the cell — then it needs no
    /// critical section and the compiler has *proved* it (§5.5).
    pub single_owner: bool,
    /// Reaction ids that read or write this cell.
    pub touched_by: Vec<usize>,
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
    /// A priority-ceiling critical section around a shared-cell access (§5.5).
    /// On a single-threaded host the body runs without masking, but the section
    /// is recorded so the analysis is observable; a metal backend lowers
    /// `ceiling` to a BASEPRI raise/restore.
    Critical { ceiling: u8, body: Vec<SirStmt> },
    /// A device op call over a substrate (§3.5).  Defined now as the Phase-1
    /// hook for composed devices; the slice lowers GPIO set/get directly to a
    /// register access instead (leaf MMIO, §6.5).
    DeviceOp { device: usize, op: String, args: Vec<SirExpr> },
}

/// An assignable place (left-hand side).
#[derive(Debug, Clone)]
pub enum SirPlace {
    /// A named local / cell variable.
    Var(String),
    /// A device register field: `(base + reg_offset)`, masked/shifted.  This is
    /// the target-neutral MMIO node (§6.2): the sim writes a mock register
    /// array, a metal backend emits a volatile store.
    Reg {
        device: usize,
        reg_offset: u64,
        width: u8,
        field_mask: u64,
        field_shift: u8,
        access: SirRegAccess,
    },
}

// ─── Intrinsics ──────────────────────────────────────────────────────────────

/// Built-in host-mode device operations.
///
/// `host_io`/`sys` are the *only* compiler-known host intrinsics — the sim's
/// semihosting/lifecycle boundary.  Real peripherals (gpio/timer) are ordinary
/// std-lib devices, never intrinsics (§2, "no privileged built-ins").
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
    /// Read a device register field (the read counterpart of `SirPlace::Reg`).
    RegLoad {
        device: usize,
        reg_offset: u64,
        width: u8,
        field_mask: u64,
        field_shift: u8,
        access: SirRegAccess,
    },
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

    /// Storage size in bytes — used to sum the static RAM footprint (§5.3).
    pub fn byte_size(&self) -> u64 {
        match self {
            SirType::Bool | SirType::U8 | SirType::S8 => 1,
            SirType::U16 | SirType::S16 => 2,
            SirType::U32 | SirType::S32 | SirType::Bytes => 4,
            SirType::U64 | SirType::S64 => 8,
        }
    }
}
