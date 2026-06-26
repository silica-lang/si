//! Host simulator — a deterministic interpreter of SIR (§7.1).
//!
//! This is the *sim-direction* consumer of SIR, peer to the C backend
//! (`backend::c`).  It is a **discrete-event simulator over a virtual clock**:
//! the clock advances only to the next scheduled event (§7.1, "advances only
//! when the program would wait"), reaction handlers execute instantaneously in
//! virtual time, and there are no wall-clock calls anywhere — so a run is
//! reproducible (§7.1/D19).  The same source always yields the same trace.
//!
//! Devices are modelled uniformly as mock register arrays: a `SirPlace::Reg`
//! store masks/shifts into the array and emits a structured trace record; the
//! C/metal backend will service the identical SIR node with a volatile MMIO
//! store.  Nothing here is `gpio`-specific (§2).

use std::collections::HashMap;

use crate::layer3;
use crate::sir::*;

// ─── Trace records (§11/D13 — structured, text rendered host-side) ──────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TraceRecord {
    pub at_ns: u64,
    pub kind: TraceKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceKind {
    /// A reaction began executing.
    ReactionFire { reaction: usize, source: String },
    /// A cell / local was written.
    CellWrite { name: String, value: u64 },
    /// A register field was written (the mock-MMIO side effect).
    RegWrite { device: usize, offset: u64, bit: u8, value: u64 },
    /// Entered a priority-ceiling critical section (§5.5).
    CriticalEnter { ceiling: u8 },
    /// Left a critical section.
    CriticalExit,
    /// `host_io.print(...)`.
    Print { text: String },
    /// A decoded Layer-3 hardware fault (§5.4): the faulting address and the
    /// language-level diagnosis from the address-ownership map.
    Fault { address: u64, diagnosis: String },
    /// A `poll <cond> within <d>` was evaluated (§3.2): `satisfied` = the
    /// condition held; otherwise the bound elapsed and `code` was raised.
    Poll { code: String, satisfied: bool },
    /// A suspending `await <cond> within <d>` resolved (§5.2): `resolved` = the
    /// condition held; otherwise the budget elapsed and `code` was raised.
    Await { code: String, resolved: bool },
    /// A reaction suspended on a bus transaction (§5.2/§3.5).
    BusStart { device: usize, op: String },
    /// A bus transaction completed: `code` = None on success, Some on fault.
    BusDone { device: usize, op: String, code: Option<String> },
    /// A Layer-2 disposition fired at the reaction boundary (§4.4): retry/skip/
    /// escalate.
    Dispose { reaction: usize, action: String },
    /// A re-fire was coalesced because the reaction was still in-flight (§5.1).
    Coalesced { reaction: usize },
    /// An event re-fired while in flight and its pending slot was full (§5.1/D02);
    /// `policy` = the declared non-coalesce overflow policy that applied.
    EventOverflow { reaction: usize, policy: String },
    /// A device was driven to its safe state on an unrecovered fault (§5.6).
    SafeState { device: usize, state: String },
    /// The scheduler-fed hardware watchdog fired — a reaction starved it (§5.6).
    WatchdogReset { timeout_ns: u64 },
    /// A reaction overran its `within <d>` deadline (§4.5/§5.6) — it starved the
    /// watchdog, so the system resets.
    DeadlineMissed { reaction: usize, budget_ns: u64 },
    /// An integer operation overflowed its result type under the default trap
    /// disposition (§4.3/SIL-004) — the system is driven to its safe state.
    Overflow { reaction: usize, width: u8 },
    /// A bounded ring buffer op (§5.3): `op` is push/pop, `len` the resulting count.
    RingOp { ring: String, op: String, len: u64 },
}

#[derive(Debug, Default)]
pub struct SimResult {
    pub trace: Vec<TraceRecord>,
    pub stdout: String,
}

impl SimResult {
    /// Render the trace deterministically as text (used by `--sim` and by the
    /// determinism test).  Device ids are resolved to names via `module`.
    pub fn render(&self, module: &SirModule) -> String {
        let name_of = |id: usize| {
            module
                .devices
                .iter()
                .find(|d| d.id == id)
                .map(|d| d.name.as_str())
                .unwrap_or("?")
        };
        let mut out = String::new();
        for r in &self.trace {
            let ms = r.at_ns as f64 / 1_000_000.0;
            let line = match &r.kind {
                TraceKind::ReactionFire { reaction, source } => {
                    format!("[{:>8.3}ms] fire reaction#{} ({})", ms, reaction, source)
                }
                TraceKind::CellWrite { name, value } => {
                    format!("[{:>8.3}ms]   cell {} = {}", ms, name, value)
                }
                TraceKind::RegWrite { device, offset, bit, value } => format!(
                    "[{:>8.3}ms]   write {}.reg(0x{:x}).bit({}) = {}",
                    ms,
                    name_of(*device),
                    offset,
                    bit,
                    value
                ),
                TraceKind::CriticalEnter { ceiling } => {
                    format!("[{:>8.3}ms]   critical-enter (ceiling {})", ms, ceiling)
                }
                TraceKind::CriticalExit => format!("[{:>8.3}ms]   critical-exit", ms),
                TraceKind::Print { text } => {
                    format!("[{:>8.3}ms]   print {:?}", ms, text)
                }
                TraceKind::Fault { address, diagnosis } => {
                    format!("[{:>8.3}ms] FAULT (layer 3): {} (addr 0x{:08x})", ms, diagnosis, address)
                }
                TraceKind::Poll { code, satisfied } => match satisfied {
                    true => format!("[{:>8.3}ms]   poll — satisfied", ms),
                    false => format!("[{:>8.3}ms]   poll — timeout, FAULT {}", ms, code),
                },
                TraceKind::Await { code, resolved } => match resolved {
                    true => format!("[{:>8.3}ms]   await — condition met, resume (§5.2)", ms),
                    false => format!("[{:>8.3}ms]   await — timeout, FAULT {}", ms, code),
                },
                TraceKind::BusStart { device, op } => {
                    format!("[{:>8.3}ms]   bus {}.{}() — suspend (yields)", ms, name_of(*device), op)
                }
                TraceKind::BusDone { device, op, code } => match code {
                    None => format!("[{:>8.3}ms]   bus {}.{}() — done, resume", ms, name_of(*device), op),
                    Some(c) => format!("[{:>8.3}ms]   bus {}.{}() — FAULT {}", ms, name_of(*device), op, c),
                },
                TraceKind::Dispose { reaction, action } => {
                    format!("[{:>8.3}ms]   reaction#{} disposition: {}", ms, reaction, action)
                }
                TraceKind::Coalesced { reaction } => {
                    format!("[{:>8.3}ms]   reaction#{} re-fire coalesced (in-flight, §5.1)", ms, reaction)
                }
                TraceKind::EventOverflow { reaction, policy } => {
                    format!("[{:>8.3}ms]   reaction#{} EVENT OVERFLOW — re-fire while in-flight, policy={} (§5.1/D02)", ms, reaction, policy)
                }
                TraceKind::SafeState { device, state } => {
                    format!("[{:>8.3}ms]   SAFE-STATE: {} -> {} (§5.6)", ms, name_of(*device), state)
                }
                TraceKind::DeadlineMissed { reaction, budget_ns } => {
                    format!("[{:>8.3}ms] DEADLINE MISSED — reaction#{} overran its {}ns `within` budget → reset (§4.5/§5.6)", ms, reaction, budget_ns)
                }
                TraceKind::RingOp { ring, op, len } => {
                    format!("[{:>8.3}ms]   ring {}.{}() — len {}", ms, ring, op, len)
                }
                TraceKind::WatchdogReset { timeout_ns } => {
                    format!("[{:>8.3}ms] WATCHDOG RESET — handler starved the watchdog ({}ms timeout, §5.6)", ms, timeout_ns / 1_000_000)
                }
                TraceKind::Overflow { reaction, width } => {
                    format!("[{:>8.3}ms] OVERFLOW TRAP — reaction#{} overflowed a {}-bit integer op → safe-state (§4.3/SIL-004)", ms, reaction, width)
                }
            };
            out.push_str(&line);
            out.push('\n');
        }
        out
    }
}

// ─── Event queue ────────────────────────────────────────────────────────────────

enum Payload {
    /// A periodic `every` timer tick.
    TimerTick { reaction: usize, period_ns: u64 },
    /// A scripted event injection (§7.1).
    Inject { event: usize },
    /// A scripted Layer-3 fault injection (§5.4).
    Fault { addr: u64 },
    /// Resume a suspended handler when its bus transaction completes (§5.2).
    Resume {
        act: Activation,
        device: usize,
        op: String,
        dst: String,
        propagate: bool,
        outcome: Result<u64, String>,
    },
    /// Re-check a suspended `await` (§5.2): re-enter the handler at the same `pc`
    /// to re-evaluate its condition (or time out).
    AwaitRecheck { act: Activation },
    /// The hardware watchdog deadline for arming generation `gen` (§5.6).
    WdtTimeout { gen: u64 },
    /// A reaction's `within <d>` deadline elapsed (§4.5/§5.6); if `reaction` is
    /// still in flight on the same activation `gen`, it overran.
    Deadline { reaction: usize, gen: u64 },
}

struct QItem {
    at_ns: u64,
    priority: u8,
    seq: u64,
    payload: Payload,
}

/// A handler activation: the saved state of one (possibly suspended) reaction
/// run — the §5.2 statically-sized frame plus a top-level program counter.
#[derive(Clone)]
struct Activation {
    reaction: usize,
    /// Next top-level body index to execute on resume.
    pc: usize,
    /// Locals live across yields (the frame), distinct from shared cells.
    locals: HashMap<String, u64>,
    /// Layer-2 retry count so far (§4.4).
    retries: u32,
    /// Absolute time the current `await`'s `within` budget elapses (§5.2); set on
    /// first reaching the await, cleared when it resolves.  `None` when not in one.
    await_deadline: Option<u64>,
}

// ─── Simulator ──────────────────────────────────────────────────────────────────

/// The result of a mock bus transaction.
enum BusOutcome {
    /// Wedged: never completes (no resume scheduled) — exercises the watchdog.
    Hang,
    /// Completes after a latency, with data or a fault code.
    Done(u64, Result<u64, String>),
}

/// Virtual-time latency the mock bus model gives a transaction (§7.1).
const BUS_LATENCY_NS: u64 = 2_000;
/// Fixed data a successful mock bus read returns.
const BUS_DATA: u64 = 0x0000_5AB0;

struct Sim<'m> {
    module: &'m SirModule,
    now: u64,
    /// device id → (register offset → current value).
    regs: HashMap<usize, HashMap<u64, u64>>,
    /// Shared `cell` storage (persistent, cross-reaction).  Locals live in each
    /// activation's frame, not here.
    cells: HashMap<String, u64>,
    /// Names that are cells (vs per-activation locals).
    cell_names: std::collections::HashSet<String>,
    /// Bounded `ring<T, N>` storage (§5.3): name → (queue, capacity).
    rings: HashMap<String, (std::collections::VecDeque<u64>, u32)>,
    /// Per-reaction single-live-activation flag (§5.1).
    in_flight: Vec<bool>,
    /// Per-reaction activation generation, bumped each fire, so a stale `within`
    /// deadline event from a completed activation is ignored (§4.5).
    react_gen: Vec<u64>,
    /// Index into `module.bus_fault_queue` (transactions failed so far).
    bus_fault_idx: usize,
    /// Bus transactions left to hang (wedged bus, §5.6 watchdog demo).
    bus_hangs_left: u32,
    /// Watchdog (§5.6/SIL-006): the active arming generation (None = fed/idle),
    /// and a monotonic generation so stale timeout events are ignored.
    wdt_active: Option<u64>,
    wdt_gen: u64,
    /// Event queue + deterministic tie-break counter.
    queue: Vec<QItem>,
    seq: u64,
    trace: Vec<TraceRecord>,
    stdout: String,
    stop: bool,
    /// A `poll` raised a fault this step; the activation loop disposes of it
    /// (§3.2).  `None` between checks.
    pending_fault: Option<String>,
    /// Set when an arithmetic op overflowed under `Trap` (§4.3).  Interior
    /// mutability because expression evaluation is `&self`; the activation loop
    /// observes it after each statement and drives the system to safe-state.
    overflow_trap: std::cell::Cell<Option<u8>>,
}

/// Run the program in `module` under the deterministic host simulator.
pub fn run(module: &SirModule) -> SimResult {
    // Module-level vars (cells + program-level lets) are global/shared; all other
    // names are per-activation locals (the frame).
    let cell_names = module.vars.iter().map(|v| v.name.clone()).collect();
    let mut sim = Sim {
        module,
        now: 0,
        regs: HashMap::new(),
        cells: HashMap::new(),
        cell_names,
        rings: HashMap::new(),
        in_flight: vec![false; module.reactions.len()],
        react_gen: vec![0; module.reactions.len()],
        bus_fault_idx: 0,
        bus_hangs_left: module.bus_hangs,
        wdt_active: None,
        wdt_gen: 0,
        queue: Vec::new(),
        seq: 0,
        trace: Vec::new(),
        stdout: String::new(),
        stop: false,
        pending_fault: None,
        overflow_trap: std::cell::Cell::new(None),
    };
    sim.run();
    SimResult { trace: sim.trace, stdout: sim.stdout }
}

impl<'m> Sim<'m> {
    fn run(&mut self) {
        // Initialise register arrays from declared reset values (§4.2).
        for dev in &self.module.devices {
            let map = self.regs.entry(dev.id).or_default();
            for reg in &dev.regs {
                map.insert(reg.offset, reg.reset);
            }
        }
        // Initialise cells/locals from their declared initialisers; bounded rings
        // get an empty queue sized to their capacity (§5.3).
        for var in &self.module.vars {
            if let SirType::Ring { cap, .. } = var.ty {
                self.rings.insert(var.name.clone(), (std::collections::VecDeque::new(), cap));
            } else {
                self.cells.insert(var.name.clone(), const_value(&var.init));
            }
        }

        // Seed the event queue (timers / injects / faults).
        for (idx, r) in self.module.reactions.iter().enumerate() {
            if let SirTrigger::EveryNs(period) = r.trigger {
                let seq = self.next_seq();
                self.queue.push(QItem {
                    at_ns: period, // fixed-rate: first deadline at one period (§4.5/D15)
                    priority: r.priority,
                    seq,
                    payload: Payload::TimerTick { reaction: idx, period_ns: period },
                });
            }
        }
        for inj in &self.module.injections {
            let priority = self.event_priority(inj.event);
            let seq = self.next_seq();
            self.queue.push(QItem { at_ns: inj.at_ns, priority, seq, payload: Payload::Inject { event: inj.event } });
        }
        for f in &self.module.fault_injections {
            let seq = self.next_seq();
            self.queue.push(QItem { at_ns: f.at_ns, priority: u8::MAX, seq, payload: Payload::Fault { addr: f.addr } });
        }

        // SysStart reactions run once at t=0 (may suspend → enqueue a resume).
        let starts: Vec<usize> = self
            .module
            .reactions
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r.trigger, SirTrigger::SysStart))
            .map(|(i, _)| i)
            .collect();
        for idx in starts {
            self.fire(idx);
        }

        let horizon = self.module.run_until_ns.unwrap_or(u64::MAX);

        // Discrete-event loop: advance virtual time to each event in turn.
        while !self.stop {
            let next = match pop_min(&mut self.queue) {
                Some(item) => item,
                None => break,
            };
            if next.at_ns >= horizon {
                break;
            }
            self.now = next.at_ns;
            match next.payload {
                Payload::TimerTick { reaction, period_ns } => {
                    self.fire(reaction);
                    let seq = self.next_seq();
                    self.queue.push(QItem {
                        at_ns: next.at_ns + period_ns,
                        priority: next.priority,
                        seq,
                        payload: Payload::TimerTick { reaction, period_ns },
                    });
                }
                Payload::Inject { event } => {
                    let mut bound: Vec<usize> = self
                        .module
                        .reactions
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| matches!(r.trigger, SirTrigger::Event(e) if e == event))
                        .map(|(idx, _)| idx)
                        .collect();
                    bound.sort_by(|&a, &b| {
                        self.module.reactions[b].priority.cmp(&self.module.reactions[a].priority).then(a.cmp(&b))
                    });
                    for idx in bound {
                        self.fire(idx);
                    }
                }
                Payload::Fault { addr } => {
                    let decoded = layer3::decode_address(self.module, addr);
                    self.trace.push(TraceRecord {
                        at_ns: self.now,
                        kind: TraceKind::Fault { address: addr, diagnosis: decoded.diagnosis },
                    });
                }
                Payload::Resume { act, device, op, dst, propagate, outcome } => {
                    self.resume(act, device, op, dst, propagate, outcome);
                }
                Payload::AwaitRecheck { act } => {
                    // Re-enter at the same pc — run_activation re-evaluates the await.
                    self.run_activation(act);
                }
                Payload::WdtTimeout { gen } => {
                    // Fire a reset only if this is still the active arming — a
                    // later feed (clean idle) would have superseded it (§5.6).
                    if self.wdt_active == Some(gen) {
                        let timeout = self.module.watchdog_timeout_ns.unwrap_or(0);
                        self.trace.push(TraceRecord {
                            at_ns: self.now,
                            kind: TraceKind::WatchdogReset { timeout_ns: timeout },
                        });
                        self.stop = true; // hardware master reset
                    }
                    continue; // a watchdog tick is not a reaction; no idle check
                }
                Payload::Deadline { reaction, gen } => {
                    // Overrun only if this exact activation is still in flight; a
                    // completed-and-refired reaction has a newer generation.
                    if self.in_flight[reaction] && self.react_gen[reaction] == gen {
                        let budget = self.module.reactions[reaction].deadline_ns.unwrap_or(0);
                        self.trace.push(TraceRecord {
                            at_ns: self.now,
                            kind: TraceKind::DeadlineMissed { reaction, budget_ns: budget },
                        });
                        self.stop = true; // overrun starves the watchdog → reset
                    }
                    continue; // a deadline tick is not a reaction; no idle check
                }
            }
            // The scheduler feeds the watchdog on a clean return to idle (§5.6).
            if !self.any_in_flight() {
                self.disarm_watchdog();
            }
        }
    }

    fn any_in_flight(&self) -> bool {
        self.in_flight.iter().any(|&b| b)
    }

    /// Arm the watchdog when the scheduler leaves idle (a reaction starts): it
    /// will reset unless fed (a clean return to idle) before the timeout.
    fn arm_watchdog(&mut self) {
        if let Some(t) = self.module.watchdog_timeout_ns {
            self.wdt_gen += 1;
            let gen = self.wdt_gen;
            self.wdt_active = Some(gen);
            let seq = self.next_seq();
            // Lowest priority: any reaction work scheduled at the same instant
            // (e.g. a resume that returns to idle exactly at the deadline) runs
            // first and feeds/disarms the watchdog before this check (§5.6).
            self.queue.push(QItem { at_ns: self.now + t, priority: 0, seq, payload: Payload::WdtTimeout { gen } });
        }
    }

    /// Feed/disarm the watchdog on a clean return to idle (§5.6).
    fn disarm_watchdog(&mut self) {
        self.wdt_active = None;
    }

    fn next_seq(&mut self) -> u64 {
        let s = self.seq;
        self.seq += 1;
        s
    }

    fn event_priority(&self, event: usize) -> u8 {
        self.module
            .reactions
            .iter()
            .filter(|r| matches!(r.trigger, SirTrigger::Event(e) if e == event))
            .map(|r| r.priority)
            .max()
            .unwrap_or(2)
    }

    /// Start a fresh activation of reaction `idx` (or coalesce if it is still
    /// in-flight — single live activation, §5.1).
    fn fire(&mut self, idx: usize) {
        if self.in_flight[idx] {
            // The single pending slot is full (§5.1/D02): apply the declared
            // overflow policy for this re-fire.
            match self.module.reactions[idx].overflow {
                SirOverflow::Coalesce => {
                    self.trace.push(TraceRecord { at_ns: self.now, kind: TraceKind::Coalesced { reaction: idx } });
                }
                SirOverflow::DropNewest => {
                    self.trace.push(TraceRecord {
                        at_ns: self.now,
                        kind: TraceKind::EventOverflow { reaction: idx, policy: "drop-newest".into() },
                    });
                }
                SirOverflow::Fault => {
                    self.trace.push(TraceRecord {
                        at_ns: self.now,
                        kind: TraceKind::EventOverflow { reaction: idx, policy: "fault".into() },
                    });
                    // Raise to the safe-state handler (§5.4/§5.6): an event-queue
                    // overflow under `fault` policy is a system-integrity fault.
                    self.drive_safe();
                    self.stop = true;
                }
            }
            return;
        }
        // Leaving idle: arm the watchdog (§5.6) for this in-flight period.
        if !self.any_in_flight() {
            self.arm_watchdog();
        }
        self.in_flight[idx] = true;
        self.react_gen[idx] += 1;
        // Arm this activation's `within <d>` deadline (§4.5/§5.6): if it is still
        // in flight when the budget elapses, it overran and the watchdog resets.
        if let Some(d) = self.module.reactions[idx].deadline_ns {
            let gen = self.react_gen[idx];
            let seq = self.next_seq();
            self.queue.push(QItem { at_ns: self.now + d, priority: 0, seq, payload: Payload::Deadline { reaction: idx, gen } });
        }
        let source = trigger_desc(&self.module.reactions[idx].trigger);
        self.trace.push(TraceRecord {
            at_ns: self.now,
            kind: TraceKind::ReactionFire { reaction: idx, source },
        });
        let act = Activation { reaction: idx, pc: 0, locals: HashMap::new(), retries: 0, await_deadline: None };
        self.run_activation(act);
    }

    /// Run an activation from its `pc` over the reaction's top-level body until
    /// it suspends on a `BusXfer` (§5.2) or completes.
    fn run_activation(&mut self, mut act: Activation) {
        let module = self.module; // copy `&'m` ref so the body is borrow-independent
        let body = &module.reactions[act.reaction].body;
        while act.pc < body.len() {
            if self.stop {
                return;
            }
            match &body[act.pc] {
                SirStmt::BusXfer { device, op, args, dst, propagate, .. } => {
                    let argvals: Vec<u64> = args.iter().map(|a| self.eval_expr(a, &act.locals)).collect();
                    let _ = argvals; // args drive a real controller on metal; the mock is value-fixed
                    self.trace.push(TraceRecord {
                        at_ns: self.now,
                        kind: TraceKind::BusStart { device: *device, op: op.clone() },
                    });
                    act.pc += 1; // resume after this transaction
                    match self.service_bus() {
                        // Wedged bus: the transaction never completes, so the
                        // handler stays in-flight forever — the watchdog catches
                        // it (§5.6).  No resume is scheduled.
                        BusOutcome::Hang => return,
                        BusOutcome::Done(latency, outcome) => {
                            let seq = self.next_seq();
                            let priority = module.reactions[act.reaction].priority;
                            self.queue.push(QItem {
                                at_ns: self.now + latency,
                                priority,
                                seq,
                                payload: Payload::Resume {
                                    act,
                                    device: *device,
                                    op: op.clone(),
                                    dst: dst.clone(),
                                    propagate: *propagate,
                                    outcome,
                                },
                            });
                            return; // suspend — the scheduler runs other work meanwhile
                        }
                    }
                }
                SirStmt::Await { cond, fault_code, within_ns, recheck_ns } => {
                    // Suspending bounded wait (§5.2): set the budget on first entry,
                    // then resume / time out / re-check.
                    let deadline = *act.await_deadline.get_or_insert(self.now + *within_ns);
                    if self.eval_expr(cond, &act.locals) != 0 {
                        act.await_deadline = None;
                        self.trace.push(TraceRecord {
                            at_ns: self.now,
                            kind: TraceKind::Await { code: fault_code.clone(), resolved: true },
                        });
                        act.pc += 1;
                    } else if self.now >= deadline {
                        act.await_deadline = None;
                        self.trace.push(TraceRecord {
                            at_ns: self.now,
                            kind: TraceKind::Await { code: fault_code.clone(), resolved: false },
                        });
                        self.dispose(act, fault_code.clone());
                        return;
                    } else {
                        // Suspend: re-check at the next cadence tick (capped at the
                        // deadline), letting the scheduler run other work meanwhile.
                        let next = (self.now + *recheck_ns).min(deadline);
                        let seq = self.next_seq();
                        let priority = module.reactions[act.reaction].priority;
                        self.queue.push(QItem { at_ns: next, priority, seq, payload: Payload::AwaitRecheck { act } });
                        return;
                    }
                }
                stmt => {
                    self.eval_stmt(stmt, &mut act.locals);
                    // An integer op may have overflowed under the default trap
                    // disposition (§4.3/SIL-004): drive the system safe and end
                    // this activation — overflow is a system-integrity fault, not
                    // a per-reaction recoverable one, so it bypasses disposition.
                    if let Some(width) = self.overflow_trap.take() {
                        self.trace.push(TraceRecord {
                            at_ns: self.now,
                            kind: TraceKind::Overflow { reaction: act.reaction, width },
                        });
                        self.in_flight[act.reaction] = false;
                        self.drive_safe();
                        return;
                    }
                    // A `poll` inside this statement may have raised a fault
                    // (§3.2); short-circuit to the reaction's disposition.
                    if let Some(code) = self.pending_fault.take() {
                        self.dispose(act, code);
                        return;
                    }
                    act.pc += 1;
                }
            }
        }
        // Completed without an unhandled fault.
        self.in_flight[act.reaction] = false;
    }

    /// Resume a suspended activation when its bus transaction completes.
    fn resume(&mut self, mut act: Activation, device: usize, op: String, dst: String, propagate: bool, outcome: Result<u64, String>) {
        let code = outcome.as_ref().err().cloned();
        self.trace.push(TraceRecord {
            at_ns: self.now,
            kind: TraceKind::BusDone { device, op, code: code.clone() },
        });
        match outcome {
            Ok(v) => {
                act.locals.insert(dst, v);
                self.run_activation(act);
            }
            Err(c) if propagate => self.dispose(act, c),
            Err(_) => {
                act.locals.insert(dst, 0);
                self.run_activation(act);
            }
        }
    }

    /// Apply the reaction's Layer-2 fault disposition (§4.4/§5.4).
    fn dispose(&mut self, mut act: Activation, code: String) {
        let disp = self.module.reactions[act.reaction].disposition;
        match disp {
            SirDisposition::Retry { max } if act.retries < max => {
                act.retries += 1;
                self.trace.push(TraceRecord {
                    at_ns: self.now,
                    kind: TraceKind::Dispose { reaction: act.reaction, action: format!("retry {}/{}", act.retries, max) },
                });
                act.pc = 0;
                act.locals.clear();
                act.await_deadline = None;
                self.run_activation(act);
            }
            SirDisposition::Skip => {
                self.trace.push(TraceRecord {
                    at_ns: self.now,
                    kind: TraceKind::Dispose { reaction: act.reaction, action: format!("skip ({})", code) },
                });
                self.in_flight[act.reaction] = false;
            }
            SirDisposition::Safe => {
                self.trace.push(TraceRecord {
                    at_ns: self.now,
                    kind: TraceKind::Dispose { reaction: act.reaction, action: format!("safe ({})", code) },
                });
                self.in_flight[act.reaction] = false;
                self.drive_safe();
            }
            _ => {
                // Escalate / retry-exhausted → Layer-3.
                self.trace.push(TraceRecord {
                    at_ns: self.now,
                    kind: TraceKind::Dispose { reaction: act.reaction, action: format!("escalate ({})", code) },
                });
                self.in_flight[act.reaction] = false;
            }
        }
    }

    /// Drive every device with a declared safe op to its safe state (§5.6) and
    /// hold.  Each safe sequence is bounded, non-yielding register writes.
    fn drive_safe(&mut self) {
        let module = self.module;
        for seq in &module.safe_seqs {
            let mut frame = HashMap::new();
            for stmt in &seq.body {
                self.eval_stmt(stmt, &mut frame);
            }
            self.trace.push(TraceRecord {
                at_ns: self.now,
                kind: TraceKind::SafeState { device: seq.device, state: seq.state.clone() },
            });
        }
        // Post-safe policy: hold (the system is now in a safe state).
        self.stop = true;
    }

    /// The mock bus model (§7.1): a wedged transaction (hang), a fault from the
    /// injected queue, or success with fixed data after a fixed latency.
    fn service_bus(&mut self) -> BusOutcome {
        if self.bus_hangs_left > 0 {
            self.bus_hangs_left -= 1;
            return BusOutcome::Hang;
        }
        if self.bus_fault_idx < self.module.bus_fault_queue.len() {
            let code = self.module.bus_fault_queue[self.bus_fault_idx].clone();
            self.bus_fault_idx += 1;
            BusOutcome::Done(BUS_LATENCY_NS, Err(code))
        } else {
            BusOutcome::Done(BUS_LATENCY_NS, Ok(BUS_DATA))
        }
    }

    fn eval_stmts(&mut self, body: &[SirStmt], frame: &mut HashMap<String, u64>) {
        for stmt in body {
            if self.stop {
                break;
            }
            self.eval_stmt(stmt, frame);
        }
    }

    fn eval_stmt(&mut self, stmt: &SirStmt, frame: &mut HashMap<String, u64>) {
        match stmt {
            SirStmt::Intrinsic(intr) => self.eval_intrinsic(intr),
            SirStmt::Assign { target, value } => {
                let v = self.eval_expr(value, frame);
                match target {
                    SirPlace::Var(name) => {
                        // Module-level vars (cells) are shared/global and traced;
                        // everything else is a per-activation local (the frame).
                        if self.cell_names.contains(name) {
                            self.cells.insert(name.clone(), v);
                            self.trace.push(TraceRecord {
                                at_ns: self.now,
                                kind: TraceKind::CellWrite { name: name.clone(), value: v },
                            });
                        } else {
                            frame.insert(name.clone(), v);
                        }
                    }
                    SirPlace::Reg { device, reg_offset, field_mask, field_shift, access, .. } => {
                        self.write_reg(*device, *reg_offset, *field_mask, *field_shift, *access, v);
                    }
                }
            }
            SirStmt::RingPush { ring, value } => {
                let v = self.eval_expr(value, frame);
                if let Some((q, cap)) = self.rings.get_mut(ring) {
                    if q.len() as u32 >= *cap {
                        q.pop_front(); // bounded: overwrite the oldest (§5.3)
                    }
                    q.push_back(v);
                    let len = q.len() as u64;
                    self.trace.push(TraceRecord {
                        at_ns: self.now,
                        kind: TraceKind::RingOp { ring: ring.clone(), op: "push".into(), len },
                    });
                }
            }
            SirStmt::RingPop { ring, dst } => {
                let v = self.rings.get_mut(ring).and_then(|(q, _)| q.pop_front()).unwrap_or(0);
                let len = self.rings.get(ring).map(|(q, _)| q.len() as u64).unwrap_or(0);
                frame.insert(dst.clone(), v);
                self.trace.push(TraceRecord {
                    at_ns: self.now,
                    kind: TraceKind::RingOp { ring: ring.clone(), op: "pop".into(), len },
                });
            }
            SirStmt::If { cond, then } => {
                if self.eval_expr(cond, frame) != 0 {
                    self.eval_stmts(then, frame);
                }
            }
            SirStmt::Critical { ceiling, body } => {
                self.trace.push(TraceRecord {
                    at_ns: self.now,
                    kind: TraceKind::CriticalEnter { ceiling: *ceiling },
                });
                self.eval_stmts(body, frame);
                self.trace.push(TraceRecord { at_ns: self.now, kind: TraceKind::CriticalExit });
            }
            SirStmt::Poll { cond, fault_code, .. } => {
                // Bounded busy-wait: nothing changes during a non-yielding wait,
                // so the condition is checked once.  If it does not hold, the
                // bound elapses and the fault is raised (the activation loop
                // disposes of it — §3.2/§4.4).
                let ok = self.eval_expr(cond, frame) != 0;
                self.trace.push(TraceRecord {
                    at_ns: self.now,
                    kind: TraceKind::Poll { code: fault_code.clone(), satisfied: ok },
                });
                if !ok {
                    self.pending_fault = Some(fault_code.clone());
                }
            }
            SirStmt::Await { cond, fault_code, .. } => {
                // Top-level `await` suspends in `run_activation`; this path is only
                // reached for an `await` nested in an `if`/critical, which cannot
                // suspend — fall back to a single deterministic check (§5.2).
                let ok = self.eval_expr(cond, frame) != 0;
                self.trace.push(TraceRecord {
                    at_ns: self.now,
                    kind: TraceKind::Await { code: fault_code.clone(), resolved: ok },
                });
                if !ok {
                    self.pending_fault = Some(fault_code.clone());
                }
            }
            SirStmt::DeviceOp { .. } => { /* Phase-1 composed-device hook */ }
            SirStmt::BusXfer { dst, .. } => {
                // Unreachable: top-level transactions are handled by
                // `run_activation`, and the resolver rejects yields nested in
                // `if`/critical-section (§5.2/§5.5).  Kept as a defensive no-op.
                debug_assert!(false, "nested BusXfer reached eval_stmt — resolver should have rejected it");
                frame.insert(dst.clone(), 0);
            }
            SirStmt::Exit(_) => {
                self.stop = true;
            }
        }
    }

    fn eval_intrinsic(&mut self, intr: &SirIntrinsic) {
        match intr {
            SirIntrinsic::HostIoPrintStr(s) => {
                self.stdout.push_str(s);
                self.trace.push(TraceRecord {
                    at_ns: self.now,
                    kind: TraceKind::Print { text: s.clone() },
                });
            }
            SirIntrinsic::HostIoPrint(e) => {
                if let SirExpr::Bytes(b) = e {
                    let s = String::from_utf8_lossy(b).into_owned();
                    self.stdout.push_str(&s);
                    self.trace.push(TraceRecord {
                        at_ns: self.now,
                        kind: TraceKind::Print { text: s },
                    });
                }
            }
            SirIntrinsic::HostIoFlush => {}
        }
    }

    fn write_reg(
        &mut self,
        device: usize,
        offset: u64,
        mask: u64,
        shift: u8,
        access: SirRegAccess,
        value: u64,
    ) {
        let map = self.regs.entry(device).or_default();
        let cur = *map.get(&offset).unwrap_or(&0);
        let field = (value << shift) & mask;
        let new = match access {
            // write-1-to-clear: writing a 1 clears that bit (§4.2/D04).
            SirRegAccess::W1c => cur & !field,
            // read/write and write-only registers do a masked read-modify-write.
            _ => (cur & !mask) | field,
        };
        map.insert(offset, new);
        let bit = shift;
        let bitval = (new >> shift) & 1;
        self.trace.push(TraceRecord {
            at_ns: self.now,
            kind: TraceKind::RegWrite { device, offset, bit, value: bitval },
        });
    }

    fn eval_expr(&self, expr: &SirExpr, frame: &HashMap<String, u64>) -> u64 {
        match expr {
            SirExpr::Bool(b) => *b as u64,
            SirExpr::U64(n) => *n,
            SirExpr::Bytes(_) => 0,
            SirExpr::Load(name) => {
                // Shared cells from global storage; everything else is a local.
                if self.cell_names.contains(name) {
                    *self.cells.get(name).unwrap_or(&0)
                } else {
                    *frame.get(name).unwrap_or(&0)
                }
            }
            SirExpr::RegLoad { device, reg_offset, field_mask, field_shift, .. } => {
                let cur = self
                    .regs
                    .get(device)
                    .and_then(|m| m.get(reg_offset))
                    .copied()
                    .unwrap_or(0);
                (cur & field_mask) >> field_shift
            }
            SirExpr::Not(inner) => (self.eval_expr(inner, frame) == 0) as u64,
            SirExpr::BinOp(op, l, r) => {
                let a = self.eval_expr(l, frame);
                let b = self.eval_expr(r, frame);
                match op {
                    SirBinOp::Add => a.wrapping_add(b),
                    SirBinOp::Sub => a.wrapping_sub(b),
                    SirBinOp::Mul => a.wrapping_mul(b),
                    SirBinOp::Div => a.checked_div(b).unwrap_or(0),
                    SirBinOp::Rem => a.checked_rem(b).unwrap_or(0),
                    SirBinOp::And => ((a != 0) && (b != 0)) as u64,
                    SirBinOp::Or => ((a != 0) || (b != 0)) as u64,
                    SirBinOp::EqEq => (a == b) as u64,
                    SirBinOp::NotEq => (a != b) as u64,
                    SirBinOp::Lt => (a < b) as u64,
                    SirBinOp::Le => (a <= b) as u64,
                    SirBinOp::Gt => (a > b) as u64,
                    SirBinOp::Ge => (a >= b) as u64,
                }
            }
            SirExpr::Arith { op, mode, width, signed, lhs, rhs } => {
                let a = self.eval_expr(lhs, frame);
                let b = self.eval_expr(rhs, frame);
                self.eval_arith(*op, *mode, *width, *signed, a, b)
            }
            // `now()` — current virtual time in ns since boot (§4.5).
            SirExpr::Now => self.now,
            // Explicit cast (§4.3): a narrowing cast truncates to `to_width`
            // bits; widening is identity.  (Signed reinterpretation is not
            // separately modelled — the sim's value domain is unsigned u64.)
            SirExpr::Cast { inner, to_width, .. } => {
                let v = self.eval_expr(inner, frame);
                if *to_width >= 64 {
                    v
                } else {
                    v & ((1u64 << to_width) - 1)
                }
            }
            // Bounded-ring reads (§5.3).
            SirExpr::RingLen(r) => self.rings.get(r).map(|(q, _)| q.len() as u64).unwrap_or(0),
            SirExpr::RingEmpty(r) => self.rings.get(r).map(|(q, _)| q.is_empty() as u64).unwrap_or(1),
            SirExpr::RingFull(r) => self
                .rings
                .get(r)
                .map(|(q, cap)| (q.len() as u32 >= *cap) as u64)
                .unwrap_or(0),
        }
    }

    /// Evaluate width-checked integer arithmetic (§4.3).  Computes the exact
    /// mathematical result in `i128`, then applies the overflow disposition at
    /// `width`: `Trap` latches `overflow_trap` (the activation loop goes
    /// safe-state), `Wrap` truncates two's-complement, `Saturate` clamps.
    fn eval_arith(&self, op: SirArithOp, mode: OverflowMode, width: u8, signed: bool, a: u64, b: u64) -> u64 {
        let bits = width as u32;
        let (lo, hi): (i128, i128) = if signed {
            (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
        } else {
            (0, (1i128 << bits) - 1)
        };
        // Interpret operands at `width` (sign-extend signed operands).
        let interp = |v: u64| -> i128 {
            if signed && bits < 64 && (v >> (bits - 1)) & 1 == 1 {
                (v as i128) - (1i128 << bits)
            } else {
                v as i128
            }
        };
        let (x, y) = (interp(a), interp(b));
        let full: i128 = match op {
            SirArithOp::Add => x + y,
            SirArithOp::Sub => x - y,
            SirArithOp::Mul => x * y,
        };
        let fits = full >= lo && full <= hi;
        let result: i128 = match mode {
            OverflowMode::Trap => {
                if !fits {
                    self.overflow_trap.set(Some(width));
                }
                full // value is unused once the trap latches
            }
            OverflowMode::Saturate => full.clamp(lo, hi),
            OverflowMode::Wrap => {
                let m = 1i128 << bits;
                let mut w = full % m;
                if w < 0 {
                    w += m;
                }
                // Re-interpret the low `bits` as signed if needed.
                if signed && w > hi {
                    w - m
                } else {
                    w
                }
            }
        };
        // Store back as raw bits at `width` (two's-complement for negatives).
        let masked = if bits >= 64 {
            result as u64
        } else if result < 0 {
            ((result + (1i128 << bits)) as u64) & ((1u64 << bits) - 1)
        } else {
            (result as u64) & ((1u64 << bits) - 1)
        };
        masked
    }
}

// ─── Helpers ──────────────────────────────────────────────────────────────────

/// Remove and return the earliest-deadline item, breaking ties by higher
/// priority then lower seq — the §5.1/D19 deterministic order.
fn pop_min(queue: &mut Vec<QItem>) -> Option<QItem> {
    if queue.is_empty() {
        return None;
    }
    let mut best = 0usize;
    for i in 1..queue.len() {
        if better(&queue[i], &queue[best]) {
            best = i;
        }
    }
    Some(queue.swap_remove(best))
}

/// Is `a` scheduled before `b` in the deterministic order?
fn better(a: &QItem, b: &QItem) -> bool {
    (a.at_ns, std::cmp::Reverse(a.priority), a.seq)
        < (b.at_ns, std::cmp::Reverse(b.priority), b.seq)
}

fn const_value(expr: &SirExpr) -> u64 {
    match expr {
        SirExpr::Bool(b) => *b as u64,
        SirExpr::U64(n) => *n,
        _ => 0,
    }
}

fn trigger_desc(trigger: &SirTrigger) -> String {
    match trigger {
        SirTrigger::SysStart => "sys.start".into(),
        SirTrigger::EveryNs(ns) => format!("every {}ns", ns),
        SirTrigger::Event(e) => format!("event#{}", e),
    }
}
