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
    /// Scripted Layer-3 fault injections from a `sim` block (§5.4).
    pub fault_injections: Vec<SirFaultInjection>,
    /// FIFO of fault codes to fail successive bus transactions with (each entry
    /// fails one transaction); from `inject bus_fault <code> times <n>`.
    pub bus_fault_queue: Vec<String>,
    /// Lowered device safe sequences (§5.6): driven on a `safe` disposition.
    pub safe_seqs: Vec<SafeSeq>,
    /// Virtual-time horizon from `run until <dur>` (None ⇒ run until idle).
    pub run_until_ns: Option<u64>,
    /// SoC memory regions (flash/RAM), for the generated linker script (§6.4).
    pub memory: Vec<SirRegion>,
    /// Resolved pin bindings, for generated startup pin configuration (§6.4).
    pub pins: Vec<SirPin>,
    /// Core clock in Hz (from `board.soc.clocks`), for lowering `every` periods
    /// to timer ticks (§4.5).  0 if unknown.
    pub core_hz: u64,
    /// Hardware watchdog timeout in ns (§5.6/SIL-006), if the board declares one.
    pub watchdog_timeout_ns: Option<u64>,
    /// The `SirDevice` id of the system watchdog, so the metal backend can
    /// configure and feed it over its declared CR/RLR/KR registers (§5.6).
    pub watchdog_device: Option<usize>,
    /// Number of bus transactions to *hang* (wedged bus, never complete); from
    /// `inject bus_hang times <n>`.
    pub bus_hangs: u32,
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
    /// Layer-2 fault disposition at the reaction boundary (§4.4/§5.4).
    pub disposition: SirDisposition,
    /// True if the body contains a yielding bus transaction — the handler is a
    /// state machine that suspends and resumes (§5.2).
    pub yields: bool,
    /// `within <d>` deadline budget in ns (§4.5/§5.6): the reaction must return
    /// to idle within this of firing, else it overruns and the watchdog resets.
    /// `None` = no declared deadline.
    pub deadline_ns: Option<u64>,
}

/// Reaction-boundary fault disposition (§4.4): what happens when a fault
/// propagates out of the handler body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SirDisposition {
    /// Raise to the Layer-3 handler (the conservative default).
    Escalate,
    /// Re-run the reaction up to `max` times before escalating.
    Retry { max: u32 },
    /// Drop this activation, keep scheduling.
    Skip,
    /// Drive devices to safe state (§5.6) — modelled as escalate for now.
    Safe,
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

/// A device's lowered safe op (§5.6): the bounded, non-yielding statement
/// sequence that drives the device to `state` on an unrecovered fault.
#[derive(Debug)]
pub struct SafeSeq {
    pub device: usize,
    pub state: String,
    pub body: Vec<SirStmt>,
}

/// A scripted Layer-3 hardware-fault injection (§5.4): a fault to `addr` at a
/// virtual time, decoded against the address-ownership map.
#[derive(Debug)]
pub struct SirFaultInjection {
    pub at_ns: u64,
    pub addr: u64,
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
    /// `ring.push(v)` (§5.3): enqueue `value`; on a full ring the oldest element
    /// is overwritten (a defined, bounded overflow policy).
    RingPush { ring: String, value: SirExpr },
    /// `let x = ring.pop()` (§5.3): dequeue the oldest into `dst` (0 if empty).
    RingPop { ring: String, dst: String },
    /// `if <cond> { <then> }` — no else for now.
    If { cond: SirExpr, then: Vec<SirStmt> },
    /// `exit(code)` — terminate the process (host only).
    Exit(SirExpr),
    /// A priority-ceiling critical section around a shared-cell access (§5.5).
    /// On a single-threaded host the body runs without masking, but the section
    /// is recorded so the analysis is observable; a metal backend lowers
    /// `ceiling` to a BASEPRI raise/restore.
    Critical { ceiling: u8, body: Vec<SirStmt> },
    /// `poll <cond> within <d> else fault <code>` (§3.2): a bounded busy-wait
    /// that does **not** yield.  If `cond` does not hold within the bound it
    /// raises `fault_code`, which propagates to the reaction's disposition like a
    /// failed transaction.  On the host the check is deterministic (nothing
    /// changes during a non-yielding wait); on metal it is a bounded spin loop.
    Poll { cond: SirExpr, fault_code: String, within_ns: u64 },
    /// A device op call over a substrate (§3.5).  Defined now as the Phase-1
    /// hook for composed devices; the slice lowers GPIO set/get directly to a
    /// register access instead (leaf MMIO, §6.5).
    DeviceOp { device: usize, op: String, args: Vec<SirExpr> },
    /// A yielding bus transaction on a substrate controller (the keystone, §3.5):
    /// a primitive (empty-bodied, `yields`) op of a leaf controller.  The handler
    /// **suspends** here; the scheduler runs other work; on completion `dst` is
    /// bound to the result, or — if `propagate` — a fault short-circuits to the
    /// reaction's disposition.  On the host the controller is a mock serviced by
    /// the sim's bus model (§7.1); on metal it lowers to the controller's MMIO.
    BusXfer {
        device: usize,
        op: String,
        args: Vec<SirExpr>,
        /// Local to bind the transaction result to (a fresh temp).
        dst: String,
        /// True if the call site applied `?` (fault propagates out).
        propagate: bool,
        /// Fault codes the op declares it can raise (§4.4/D14).
        fault_codes: Vec<String>,
    },
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
    /// Width-checked integer arithmetic (§4.3 / SIL-004).  Carries the result
    /// width + signedness (from the assignment-target type) and the overflow
    /// disposition: `Trap` (default — overflow drives the system to safe-state),
    /// `Wrap`, or `Saturate`.  Add/Sub/Mul lower here; Div/Rem stay `BinOp`.
    Arith {
        op: SirArithOp,
        mode: OverflowMode,
        width: u8,
        signed: bool,
        lhs: Box<SirExpr>,
        rhs: Box<SirExpr>,
    },
    /// `now()` — the current time as an `instant`, nanoseconds since boot (§4.5).
    /// The sim reads its virtual clock; metal/host read a monotonic counter.
    Now,
    /// `<inner> as <type>` — an explicit numeric cast (§4.3): truncate to
    /// `to_width` bits (narrowing) or zero/sign-extend (widening); `signed`
    /// records the target signedness for the C emission.
    Cast { inner: Box<SirExpr>, to_width: u8, signed: bool },
    /// `ring.len()` — current element count (§5.3).
    RingLen(String),
    /// `ring.is_empty()` — count == 0.
    RingEmpty(String),
    /// `ring.is_full()` — count == cap.
    RingFull(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SirArithOp {
    Add,
    Sub,
    Mul,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum OverflowMode {
    /// Overflow drives the system to its safe state (SIL-004 trap-by-default).
    Trap,
    /// Two's-complement wraparound at `width` (`+%`/`-%`/`*%`).
    Wrap,
    /// Clamp to the type's min/max at `width` (`+|`/`-|`/`*|`).
    Saturate,
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
    /// A point in time — nanoseconds since boot (§4.5).  Distinct from a plain
    /// `u64` at the type level (the resolver enforces `instant`/`duration`
    /// arithmetic rules); stored as `uint64_t`.
    Instant,
    /// A span of time — nanoseconds (§4.5).  Stored as `uint64_t`.
    Duration,
    /// `ring<T, N>` — a bounded ring buffer (§5.3): `cap` elements each of
    /// `elem_bytes` bytes, plus head/tail/count indices.  Statically counted.
    Ring { elem_bytes: u8, cap: u32 },
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
            SirType::Instant | SirType::Duration => "uint64_t",
            // Rings are not scalar values; they are emitted as a named struct,
            // never via `c_type` in expression position.
            SirType::Ring { .. } => "struct __ring",
        }
    }

    /// The element C type for a ring (`uint8_t`/`uint16_t`/`uint32_t`/`uint64_t`).
    pub fn ring_elem_ctype(elem_bytes: u8) -> &'static str {
        match elem_bytes {
            1 => "uint8_t",
            2 => "uint16_t",
            8 => "uint64_t",
            _ => "uint32_t",
        }
    }

    /// Storage size in bytes — used to sum the static RAM footprint (§5.3).
    pub fn byte_size(&self) -> u64 {
        match self {
            SirType::Bool | SirType::U8 | SirType::S8 => 1,
            SirType::U16 | SirType::S16 => 2,
            SirType::U32 | SirType::S32 | SirType::Bytes => 4,
            SirType::U64 | SirType::S64 | SirType::Instant | SirType::Duration => 8,
            // cap elements + head/tail/count (3 × u32).
            SirType::Ring { elem_bytes, cap } => (*cap as u64) * (*elem_bytes as u64) + 12,
        }
    }
}
