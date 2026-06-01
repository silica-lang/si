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
            };
            out.push_str(&line);
            out.push('\n');
        }
        out
    }
}

// ─── Event queue ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
enum Payload {
    /// A periodic `every` timer tick.
    TimerTick { reaction: usize, period_ns: u64 },
    /// A scripted event injection (§7.1).
    Inject { event: usize },
    /// A scripted Layer-3 fault injection (§5.4).
    Fault { addr: u64 },
}

struct QItem {
    at_ns: u64,
    priority: u8,
    seq: u64,
    payload: Payload,
}

// ─── Simulator ──────────────────────────────────────────────────────────────────

struct Sim<'m> {
    module: &'m SirModule,
    now: u64,
    /// device id → (register offset → current value).
    regs: HashMap<usize, HashMap<u64, u64>>,
    /// cell / local name → current value.
    cells: HashMap<String, u64>,
    trace: Vec<TraceRecord>,
    stdout: String,
    stop: bool,
}

/// Run the program in `module` under the deterministic host simulator.
pub fn run(module: &SirModule) -> SimResult {
    let mut sim = Sim {
        module,
        now: 0,
        regs: HashMap::new(),
        cells: HashMap::new(),
        trace: Vec::new(),
        stdout: String::new(),
        stop: false,
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
        // Initialise cells/locals from their declared initialisers.
        for var in &self.module.vars {
            self.cells.insert(var.name.clone(), const_value(&var.init));
        }

        // SysStart reactions run once at t=0, before the event loop.
        for (idx, r) in self.module.reactions.iter().enumerate() {
            if matches!(r.trigger, SirTrigger::SysStart) {
                self.fire(idx);
            }
        }

        // Seed the event queue.
        let mut queue: Vec<QItem> = Vec::new();
        let mut seq = 0u64;
        for (idx, r) in self.module.reactions.iter().enumerate() {
            if let SirTrigger::EveryNs(period) = r.trigger {
                queue.push(QItem {
                    at_ns: period, // fixed-rate: first deadline at one period (§4.5/D15)
                    priority: r.priority,
                    seq,
                    payload: Payload::TimerTick { reaction: idx, period_ns: period },
                });
                seq += 1;
            }
        }
        for inj in &self.module.injections {
            let priority = self.event_priority(inj.event);
            queue.push(QItem {
                at_ns: inj.at_ns,
                priority,
                seq,
                payload: Payload::Inject { event: inj.event },
            });
            seq += 1;
        }
        for f in &self.module.fault_injections {
            // Faults are highest-priority so they order first among same-time events.
            queue.push(QItem { at_ns: f.at_ns, priority: u8::MAX, seq, payload: Payload::Fault { addr: f.addr } });
            seq += 1;
        }

        let horizon = self.module.run_until_ns.unwrap_or(u64::MAX);

        // Discrete-event loop: advance virtual time to each event in turn.
        while !self.stop {
            let next = match pop_min(&mut queue) {
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
                    // Fixed-rate reschedule from the *scheduled* time (§4.5/D15).
                    queue.push(QItem {
                        at_ns: next.at_ns + period_ns,
                        priority: next.priority,
                        seq,
                        payload: Payload::TimerTick { reaction, period_ns },
                    });
                    seq += 1;
                }
                Payload::Inject { event } => {
                    // Fire every reaction bound to this event, in priority order.
                    let mut bound: Vec<usize> = self
                        .module
                        .reactions
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| matches!(r.trigger, SirTrigger::Event(e) if e == event))
                        .map(|(idx, _)| idx)
                        .collect();
                    bound.sort_by(|&a, &b| {
                        self.module.reactions[b]
                            .priority
                            .cmp(&self.module.reactions[a].priority)
                            .then(a.cmp(&b))
                    });
                    for idx in bound {
                        self.fire(idx);
                    }
                }
                Payload::Fault { addr } => {
                    // Layer-3 decode against the address-ownership map (§5.4).
                    let decoded = layer3::decode_address(self.module, addr);
                    self.trace.push(TraceRecord {
                        at_ns: self.now,
                        kind: TraceKind::Fault { address: addr, diagnosis: decoded.diagnosis },
                    });
                }
            }
        }
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

    fn fire(&mut self, idx: usize) {
        // The module is immutable for the whole run; copy the `&'m` reference so
        // the borrowed body is independent of `&mut self` (sim state).
        let module = self.module;
        let r = &module.reactions[idx];
        let source = trigger_desc(&r.trigger);
        self.trace.push(TraceRecord {
            at_ns: self.now,
            kind: TraceKind::ReactionFire { reaction: r.id, source },
        });
        self.eval_stmts(&r.body);
    }

    fn eval_stmts(&mut self, body: &[SirStmt]) {
        for stmt in body {
            if self.stop {
                break;
            }
            self.eval_stmt(stmt);
        }
    }

    fn eval_stmt(&mut self, stmt: &SirStmt) {
        match stmt {
            SirStmt::Intrinsic(intr) => self.eval_intrinsic(intr),
            SirStmt::Assign { target, value } => {
                let v = self.eval_expr(value);
                match target {
                    SirPlace::Var(name) => {
                        self.cells.insert(name.clone(), v);
                        self.trace.push(TraceRecord {
                            at_ns: self.now,
                            kind: TraceKind::CellWrite { name: name.clone(), value: v },
                        });
                    }
                    SirPlace::Reg { device, reg_offset, field_mask, field_shift, access, .. } => {
                        self.write_reg(*device, *reg_offset, *field_mask, *field_shift, *access, v);
                    }
                }
            }
            SirStmt::If { cond, then } => {
                if self.eval_expr(cond) != 0 {
                    self.eval_stmts(then);
                }
            }
            SirStmt::Critical { ceiling, body } => {
                self.trace.push(TraceRecord {
                    at_ns: self.now,
                    kind: TraceKind::CriticalEnter { ceiling: *ceiling },
                });
                self.eval_stmts(body);
                self.trace.push(TraceRecord { at_ns: self.now, kind: TraceKind::CriticalExit });
            }
            SirStmt::DeviceOp { .. } => { /* Phase-1 composed-device hook */ }
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

    fn eval_expr(&self, expr: &SirExpr) -> u64 {
        match expr {
            SirExpr::Bool(b) => *b as u64,
            SirExpr::U64(n) => *n,
            SirExpr::Bytes(_) => 0,
            SirExpr::Load(name) => *self.cells.get(name).unwrap_or(&0),
            SirExpr::RegLoad { device, reg_offset, field_mask, field_shift, .. } => {
                let cur = self
                    .regs
                    .get(device)
                    .and_then(|m| m.get(reg_offset))
                    .copied()
                    .unwrap_or(0);
                (cur & field_mask) >> field_shift
            }
            SirExpr::Not(inner) => (self.eval_expr(inner) == 0) as u64,
            SirExpr::BinOp(op, l, r) => {
                let a = self.eval_expr(l);
                let b = self.eval_expr(r);
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
        }
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
