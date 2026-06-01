//! Name resolution and lowering to SIR.
//!
//! This pass walks the AST produced by the parser and:
//!
//! 1. Collects device/board/interface type declarations (incl. the std-lib).
//! 2. Resolves a program's `use board` into concrete [`SirDevice`] instances
//!    with a resolved register layout, and its pin bindings into typed pin
//!    references (with a duplicate-pad check — §3.3, Phase-0 gate #2).
//! 3. Lowers reactions: `every <dur>` → a periodic trigger, `on <pin>.<event>`
//!    → a resolved event source, pin ops (`led.set(x)`) → target-neutral
//!    register accesses (§6.5).
//! 4. Computes the static reaction↔cell access graph and the per-cell
//!    priority-ceiling critical sections (§5.5).
//! 5. Resolves a `sim` block's scripted injections to event ids (§7.1).
//!
//! Errors carry a [`Span`] (via [`Diag`]) so the caller can print
//! source-location context.

use std::collections::HashMap;

use crate::ast::*;
use crate::diag::Diag;
use crate::sir::*;

// ─── Error ────────────────────────────────────────────────────────────────────

/// Resolver diagnostics share the common [`Diag`] type with the parser.
pub type ResolveError = Diag;

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
    /// A `use board <name> as <alias>` import.
    Board(String),
    /// A pin alias (`let led = board.led_user`).
    Pin(PinRef),
    /// A board device-instance alias (`let sensor = board.env`) — for composed
    /// devices whose ops are called (`sensor.read_temp()`).
    Device(InstanceRef),
    /// A `use` alias pointing to another name.
    #[allow(dead_code)]
    Alias(Vec<String>),
}

/// A resolved reference to a board device instance.
#[derive(Debug, Clone)]
struct InstanceRef {
    device: usize,
    ty: String,
}

/// What a device instance's `needs` relation resolves to.
#[derive(Debug, Clone)]
enum NeedVal {
    /// A reference to another device instance (e.g. `bus = i2c0`).
    Device(InstanceRef),
    /// A constant value (e.g. `addr = 0x76`).
    Const(u64),
}

#[derive(Debug, Clone, Copy)]
enum IntrinsicDevice {
    HostIo,
    Sys,
}

/// A resolved reference to one physical pin of a GPIO-like port instance.
#[derive(Debug, Clone)]
struct PinRef {
    /// `SirDevice` id of the owning port instance.
    port_device: usize,
    /// Pin index within the port.
    index: u8,
    dir: PinDir,
    /// The port's device *type* name, for looking up its regs / ops / emits.
    port_type: String,
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

/// The resolved board context for one program.
#[derive(Clone)]
struct BoardContext {
    /// Pin-binding name → resolved pin reference (`led_user` → …).
    pins: HashMap<String, PinRef>,
    /// Instance name → device instance (`env` → bme280 #3) for composed devices.
    instances: HashMap<String, InstanceRef>,
}

// ─── Resolver ─────────────────────────────────────────────────────────────────

pub struct Resolver {
    errors: Vec<Diag>,
    /// Device *types* by name (std-lib + user), e.g. `gpio` → its `DeviceDef`.
    device_defs: HashMap<String, DeviceDef>,
    /// Interface declarations by name (`i2c` → its `InterfaceDef`).
    interfaces: HashMap<String, InterfaceDef>,
    /// Board declarations by name.
    boards: HashMap<String, BoardDef>,
    /// Device id → its resolved `needs` (`bus` → i2c0, `addr` → 0x76).
    instance_needs: HashMap<usize, HashMap<String, NeedVal>>,
    /// Counter for fresh temporaries introduced by op inlining / `?`.
    tmp_counter: usize,
    /// Stack of result-locals for ops currently being inlined; `return X` in an
    /// op body binds the top.
    op_result: Vec<String>,

    // ── output accumulators ──
    devices: Vec<SirDevice>,
    events: Vec<SirEvent>,
    cells: Vec<CellInfo>,
    injections: Vec<SirInjection>,
    fault_injections: Vec<SirFaultInjection>,
    bus_fault_queue: Vec<String>,
    run_until_ns: Option<u64>,
    memory: Vec<SirRegion>,
    pins: Vec<SirPin>,
    core_hz: u64,

    /// Device id → device-type name (for reg/op/emit lookups).
    dev_types: HashMap<usize, String>,
    /// The board context for the program *currently* being resolved.
    board_ctx: Option<BoardContext>,
    /// Per-program board context, so a `sim` block resolves against its own
    /// program's board rather than whichever program was lowered last.
    program_ctx: HashMap<String, BoardContext>,
    /// Boards already built, so a board used by more than one program (or used
    /// twice) does not append its devices/memory/pins to the module twice.
    boards_built: HashMap<String, BoardContext>,
}

impl Default for Resolver {
    fn default() -> Self {
        Self::new()
    }
}

impl Resolver {
    pub fn new() -> Self {
        Resolver {
            errors: Vec::new(),
            device_defs: HashMap::new(),
            interfaces: HashMap::new(),
            boards: HashMap::new(),
            instance_needs: HashMap::new(),
            tmp_counter: 0,
            op_result: Vec::new(),
            devices: Vec::new(),
            events: Vec::new(),
            cells: Vec::new(),
            injections: Vec::new(),
            fault_injections: Vec::new(),
            bus_fault_queue: Vec::new(),
            run_until_ns: None,
            memory: Vec::new(),
            pins: Vec::new(),
            core_hz: 0,
            dev_types: HashMap::new(),
            board_ctx: None,
            program_ctx: HashMap::new(),
            boards_built: HashMap::new(),
        }
    }

    fn err(&mut self, span: Span, msg: impl Into<String>) {
        self.errors.push(Diag::new(span, msg));
    }

    pub fn resolve_module(mut self, module: &Module) -> Result<SirModule, Vec<ResolveError>> {
        // ── Pre-pass: collect type declarations ──
        for item in &module.items {
            match item {
                Item::Device(d) => {
                    self.device_defs.insert(d.name.name.clone(), d.clone());
                }
                Item::Interface(i) => {
                    self.interfaces.insert(i.name.name.clone(), i.clone());
                }
                Item::Board(b) => {
                    self.boards.insert(b.name.name.clone(), b.clone());
                }
                _ => {}
            }
        }

        // ── Check `implements` conformance (§4.1/D18) ──
        for item in &module.items {
            if let Item::Device(d) = item {
                self.check_conformance(d);
            }
        }

        let mut reactions: Vec<SirReaction> = Vec::new();
        let mut vars: Vec<SirVar> = Vec::new();

        // ── Resolve programs ──
        for item in &module.items {
            if let Item::Program(prog) = item {
                self.resolve_program(prog, &mut reactions, &mut vars);
            }
        }

        // ── Resolve sim scripts (after their program, reusing board context) ──
        for item in &module.items {
            if let Item::Sim(sim) = item {
                self.resolve_sim(sim);
            }
        }

        if self.errors.is_empty() {
            Ok(SirModule {
                reactions,
                vars,
                devices: self.devices,
                events: self.events,
                cells: self.cells,
                injections: self.injections,
                fault_injections: self.fault_injections,
                bus_fault_queue: self.bus_fault_queue,
                run_until_ns: self.run_until_ns,
                memory: self.memory,
                pins: self.pins,
                core_hz: self.core_hz,
            })
        } else {
            Err(self.errors)
        }
    }

    fn resolve_program(
        &mut self,
        prog: &ProgramDef,
        out: &mut Vec<SirReaction>,
        module_vars: &mut Vec<SirVar>,
    ) {
        let mut scope = Scope::new();
        scope.insert("sys", Binding::IntrinsicDevice(IntrinsicDevice::Sys));
        scope.insert("host_io", Binding::IntrinsicDevice(IntrinsicDevice::HostIo));

        let mut vars: Vec<SirVar> = Vec::new();

        // First pass: declarations.
        for item in &prog.items {
            match item {
                ProgramItem::UseDecl(u) => self.resolve_use(u, &mut scope),
                ProgramItem::LetDecl(l) => {
                    // A `let` whose initialiser names a board pin / device instance
                    // is a compile-time alias, not storage.
                    if let Some(pin) = self.try_resolve_pin_expr(&l.init, &scope) {
                        scope.insert(&l.name.name, Binding::Pin(pin));
                    } else if let Some(inst) = self.try_resolve_instance_expr(&l.init, &scope) {
                        scope.insert(&l.name.name, Binding::Device(inst));
                    } else {
                        let ty = infer_type_from_expr(&l.init);
                        let init = self.lower_expr(&l.init, &scope);
                        scope.insert(&l.name.name, Binding::Local(l.name.name.clone(), ty.clone()));
                        vars.push(SirVar { name: l.name.name.clone(), ty, init, is_cell: false });
                    }
                }
                ProgramItem::CellDecl(c) => {
                    let ty = resolve_type_expr(&c.ty);
                    let init = self.lower_expr(&c.init, &scope);
                    scope.insert(&c.name.name, Binding::Cell(c.name.name.clone(), ty.clone()));
                    vars.push(SirVar { name: c.name.name.clone(), ty, init, is_cell: true });
                }
                ProgramItem::Reaction(_) => {}
            }
        }

        // Second pass: lower reactions.
        let first = out.len();
        for item in &prog.items {
            if let ProgramItem::Reaction(r) = item {
                let id = out.len();
                if let Some(reaction) = self.lower_reaction(id, r, &scope, &vars) {
                    out.push(reaction);
                }
            }
        }

        // Cell concurrency analysis + critical-section insertion (§5.5).
        self.analyze_cells(&mut out[first..], &vars);

        // §5.5/D03: a cell borrow may not cross a yield.  In this lowering a
        // shared-cell access is wrapped in a `Critical` (a single straight-line
        // statement) and yields are hoisted out as separate `BusXfer`s, so a
        // `Critical` containing a `BusXfer` would be a yield inside a held
        // section — reject it (defensive; the lowering does not produce it).
        for r in &out[first..] {
            if critical_contains_yield(&r.body) {
                self.err(prog.span, "a cell critical section may not span a yield (§5.5/D03)");
            }
        }

        module_vars.extend(vars);

        // Stash this program's board context so its `sim` block resolves against
        // the right board (the field is reused for the next program).
        if let Some(ctx) = self.board_ctx.take() {
            self.program_ctx.insert(prog.name.name.clone(), ctx);
        }
    }

    // ── use / board ───────────────────────────────────────────────────────────

    fn resolve_use(&mut self, u: &UseDecl, scope: &mut Scope) {
        match u.kind {
            UseKind::Board => {
                let board_name = u.path.last().map(|i| i.name.clone()).unwrap_or_default();
                if !self.boards.contains_key(&board_name) {
                    self.err(u.span, format!("unknown board '{}'", board_name));
                    return;
                }
                self.build_board(&board_name, u.span);
                scope.insert(&u.alias.name, Binding::Board(board_name));
            }
            UseKind::Plain => {
                if let Some(binding) = self.resolve_use_path(&u.path) {
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
        }
    }

    fn resolve_use_path(&mut self, path: &[Ident]) -> Option<Binding> {
        match path.iter().map(|i| i.name.as_str()).collect::<Vec<_>>().as_slice() {
            ["host_io"] => Some(Binding::IntrinsicDevice(IntrinsicDevice::HostIo)),
            ["sys"] => Some(Binding::IntrinsicDevice(IntrinsicDevice::Sys)),
            _ => None,
        }
    }

    /// Build a board's device instances + pin bindings, populate `self.devices`
    /// and the `BoardContext`, and run the duplicate-pad check.
    fn build_board(&mut self, board_name: &str, use_span: Span) {
        // If this board was already built (another program uses it), reuse its
        // context instead of appending its devices/memory/pins to the module
        // again.
        if let Some(ctx) = self.boards_built.get(board_name) {
            self.board_ctx = Some(ctx.clone());
            return;
        }
        let board = match self.boards.get(board_name) {
            Some(b) => b.clone(),
            None => return,
        };

        // SoC memory regions → linker script (§6.4).
        if let Some(soc) = &board.soc {
            for region in &soc.memory {
                self.memory.push(SirRegion {
                    name: region.name.name.clone(),
                    origin: region.at,
                    size: region.size,
                });
            }
            // Core clock → `every` tick lowering (§4.5).  Prefer a clock named
            // `sysclk`, else the first clock; only constant frequencies are read.
            let pick = soc
                .clocks
                .iter()
                .find(|c| c.name.name == "sysclk")
                .or_else(|| soc.clocks.first());
            if let Some(clk) = pick {
                if let ExprKind::IntLit(hz) = clk.init.kind {
                    self.core_hz = hz;
                }
            }
        }

        let mut pins: HashMap<String, PinRef> = HashMap::new();
        // instance name → SirDevice id
        let mut instance_ids: HashMap<String, usize> = HashMap::new();

        // Peripheral instances.
        for inst in &board.instances {
            let ty_name = inst.device_ty.name.clone();
            let regs = match self.device_defs.get(&ty_name) {
                Some(def) => lower_regs(def),
                None => {
                    self.err(
                        inst.span,
                        format!("instance '{}' has unknown device type '{}'", inst.name.name, ty_name),
                    );
                    Vec::new()
                }
            };
            let id = self.devices.len();
            self.devices.push(SirDevice {
                id,
                name: inst.name.name.clone(),
                base_addr: inst.at,
                // The compiler core does not special-case device types by name
                // (§2): the simulator models every device uniformly as a
                // register array, so no `kind` is derived from `ty_name`.
                kind: SirDeviceKind::Generic,
                regs,
            });
            self.dev_types.insert(id, ty_name);
            instance_ids.insert(inst.name.name.clone(), id);
        }

        // Resolve each instance's `needs` relations (e.g. `bus = i2c0`) so a
        // composed device's op body can reach its substrate.  A `needs` typed by
        // an interface must be satisfied by an instance whose device implements
        // that interface (§4.1/D18).
        let instances: HashMap<String, InstanceRef> = instance_ids
            .iter()
            .map(|(name, &id)| {
                let ty = self.dev_types.get(&id).cloned().unwrap_or_default();
                (name.clone(), InstanceRef { device: id, ty })
            })
            .collect();
        for inst in &board.instances {
            let Some(&dev_id) = instance_ids.get(&inst.name.name) else { continue };
            let declared = self.device_defs.get(&inst.device_ty.name).and_then(|d| d.sections.needs.clone());
            let mut resolved: HashMap<String, NeedVal> = HashMap::new();
            for (need_name, path) in &inst.needs {
                let head = &path[0].name;
                if let Some(target) = instances.get(head) {
                    // If the need is interface-typed, check conformance (D18).
                    if let Some(ns) = &declared {
                        if let Some(nd) = ns.needs.iter().find(|n| n.name.name == need_name.name) {
                            let iface = &nd.ty.name;
                            if self.interfaces.contains_key(iface) && !self.implements(&target.ty, iface) {
                                self.err(
                                    inst.span,
                                    format!(
                                        "instance '{}': `{}` requires an `{}` provider, but '{}' (type '{}') does not implement it",
                                        inst.name.name, need_name.name, iface, head, target.ty
                                    ),
                                );
                            }
                        }
                    }
                    resolved.insert(need_name.name.clone(), NeedVal::Device(target.clone()));
                }
            }
            self.instance_needs.insert(dev_id, resolved);
        }

        // Pin bindings — with duplicate physical-pad detection (§3.3).
        let mut pad_owner: HashMap<(String, u64), (String, Span)> = HashMap::new();
        let mut claim_pad = |errs: &mut Vec<Diag>, port: &str, index: u64, owner: &str, span: Span| {
            if let Some((prev, _)) = pad_owner.get(&(port.to_string(), index)) {
                errs.push(Diag::new(
                    span,
                    format!(
                        "physical pad {}.pin({}) is already owned by '{}'; it cannot also be bound to '{}'",
                        port, index, prev, owner
                    ),
                ));
            } else {
                pad_owner.insert((port.to_string(), index), (owner.to_string(), span));
            }
        };

        for pb in &board.pin_bindings {
            claim_pad(&mut self.errors, &pb.port.name, pb.index, &pb.name.name, pb.span);
            match instance_ids.get(&pb.port.name) {
                Some(&dev_id) => {
                    let port_type = self
                        .dev_types
                        .get(&dev_id)
                        .cloned()
                        .unwrap_or_default();
                    pins.insert(
                        pb.name.name.clone(),
                        PinRef { port_device: dev_id, index: pb.index as u8, dir: pb.dir, port_type: port_type.clone() },
                    );
                    // Record for generated startup pin configuration (§6.4).
                    if let Some((off, w)) = self.find_dir_reg(&port_type) {
                        self.pins.push(SirPin {
                            device: dev_id,
                            index: pb.index as u8,
                            output: pb.dir == PinDir::Output,
                            pull_up: pb.pull == Pull::Up,
                            dir_reg_offset: off,
                            dir_reg_width: w,
                        });
                    }
                }
                None => {
                    self.err(pb.span, format!("pin '{}' references unknown port '{}'", pb.name.name, pb.port.name));
                }
            }
        }

        // Pinmux pads also claim physical pads (same ownership rule).
        for mux in &board.pinctrl {
            for pa in &mux.pins {
                let owner = format!("{}.{}", mux.name.name, pa.role.name);
                claim_pad(&mut self.errors, &pa.port.name, pa.index, &owner, pa.span);
            }
        }

        let _ = use_span;
        let ctx = BoardContext { pins, instances };
        self.boards_built.insert(board_name.to_string(), ctx.clone());
        self.board_ctx = Some(ctx);
    }

    /// If `expr` is `<board-alias>.<pin-binding>`, resolve it to a [`PinRef`].
    fn try_resolve_pin_expr(&mut self, expr: &Expr, scope: &Scope) -> Option<PinRef> {
        if let ExprKind::Field(base, field) = &expr.kind {
            if let Some(root) = expr_root_ident(base) {
                if let Some(Binding::Board(_)) = scope.lookup(root) {
                    if let Some(ctx) = &self.board_ctx {
                        return ctx.pins.get(&field.name).cloned();
                    }
                }
            }
        }
        None
    }

    /// If `expr` is `<board-alias>.<instance>`, resolve it to an [`InstanceRef`]
    /// (a composed/leaf device whose ops are called).
    fn try_resolve_instance_expr(&mut self, expr: &Expr, scope: &Scope) -> Option<InstanceRef> {
        if let ExprKind::Field(base, field) = &expr.kind {
            if let Some(root) = expr_root_ident(base) {
                if let Some(Binding::Board(_)) = scope.lookup(root) {
                    if let Some(ctx) = &self.board_ctx {
                        return ctx.instances.get(&field.name).cloned();
                    }
                }
            }
        }
        None
    }

    // ── Reactions & triggers ────────────────────────────────────────────────

    fn lower_reaction(
        &mut self,
        id: usize,
        r: &Reaction,
        scope: &Scope,
        vars: &[SirVar],
    ) -> Option<SirReaction> {
        let (trigger, priority) = match &r.trigger {
            Trigger::On(event_ref) => {
                let t = self.lower_event_trigger(event_ref, scope)?;
                let prio = match t {
                    SirTrigger::SysStart => 0,
                    _ => 2, // device/IRQ events outrank periodic timers (§5.1)
                };
                (t, prio)
            }
            Trigger::Every(dur) => (SirTrigger::EveryNs(dur.to_ns()), 1),
        };

        let body = self.lower_block(&r.body, scope, vars);
        let yields = body_yields(&body);
        let disposition = lower_disposition(&r.fault_disp);
        Some(SirReaction { id, trigger, body, priority, disposition, yields })
    }

    /// Does `ty` implement interface `iface` (a declared `implements`)?
    fn implements(&self, ty: &str, iface: &str) -> bool {
        self.device_defs
            .get(ty)
            .map(|d| d.implements.iter().any(|i| i.name == iface))
            .unwrap_or(false)
    }

    /// Check a device's `implements` claims against the interface op signatures
    /// (§4.1/D18): every interface op must have a matching device op (name +
    /// arity).  Nominal (the `implements` is a declared claim) + structural.
    fn check_conformance(&mut self, d: &DeviceDef) {
        let dev_ops: Vec<(&str, usize)> = d
            .sections
            .ops
            .as_ref()
            .map(|s| {
                s.items
                    .iter()
                    .map(|OpsItem::Op(o)| (o.name.name.as_str(), o.params.len()))
                    .collect()
            })
            .unwrap_or_default();
        for iface_name in &d.implements {
            // Snapshot the interface's op shapes so the immutable borrow is
            // released before reporting errors.
            let iface_ops: Vec<(String, usize)> = match self.interfaces.get(&iface_name.name) {
                Some(iface) => iface.ops.iter().map(|o| (o.name.name.clone(), o.params.len())).collect(),
                None => {
                    self.err(iface_name.span, format!("unknown interface '{}'", iface_name.name));
                    continue;
                }
            };
            for (opname, arity) in &iface_ops {
                let ok = dev_ops.iter().any(|(n, a)| n == opname && a == arity);
                if !ok {
                    self.err(
                        d.name.span,
                        format!(
                            "device '{}' implements '{}' but is missing op `{}({} args)`",
                            d.name.name, iface_name.name, opname, arity
                        ),
                    );
                }
            }
        }
    }

    fn lower_event_trigger(&mut self, event_ref: &EventRef, scope: &Scope) -> Option<SirTrigger> {
        let device_name = match expr_root_ident(&event_ref.device) {
            Some(n) => n,
            None => {
                self.err(event_ref.span, "event device must be a simple identifier");
                return None;
            }
        };
        let event_name = &event_ref.event.name;

        match scope.lookup(device_name).cloned() {
            Some(Binding::IntrinsicDevice(IntrinsicDevice::Sys)) => match event_name.as_str() {
                "start" => Some(SirTrigger::SysStart),
                other => {
                    self.err(event_ref.span, format!("unknown sys event '{}'; known: start", other));
                    None
                }
            },
            Some(Binding::Pin(pin)) => {
                let ev = self.resolve_pin_event(&pin, event_name, event_ref.span)?;
                Some(SirTrigger::Event(ev))
            }
            Some(_) => {
                self.err(event_ref.span, format!("'{}' is not an event-emitting device", device_name));
                None
            }
            None => {
                self.err(event_ref.span, format!("undefined device '{}'", device_name));
                None
            }
        }
    }

    /// Check that the pin's device type declares the named `emits` event, and
    /// intern a [`SirEvent`] for `(device, name, index)`.
    fn resolve_pin_event(&mut self, pin: &PinRef, event_name: &str, span: Span) -> Option<usize> {
        let declared = self
            .device_defs
            .get(&pin.port_type)
            .map(|d| d.sections.emits.iter().any(|e| e.name.name == event_name))
            .unwrap_or(false);
        if !declared {
            self.err(
                span,
                format!("device type '{}' does not emit event '{}'", pin.port_type, event_name),
            );
            return None;
        }
        Some(self.intern_event(pin.port_device, event_name, pin.index))
    }

    fn intern_event(&mut self, device: usize, name: &str, index: u8) -> usize {
        if let Some(ev) = self
            .events
            .iter()
            .find(|e| e.device == device && e.name == name && e.pin_index == Some(index))
        {
            return ev.id;
        }
        let id = self.events.len();
        self.events.push(SirEvent { id, name: name.to_string(), device, pin_index: Some(index) });
        id
    }

    // ── Statements & expressions ──────────────────────────────────────────────

    fn lower_block(&mut self, block: &Block, scope: &Scope, vars: &[SirVar]) -> Vec<SirStmt> {
        let mut local_scope = Scope::new();
        for (name, binding) in &scope.bindings {
            local_scope.insert(name, binding.clone());
        }
        let mut out = Vec::new();
        for stmt in &block.stmts {
            self.lower_stmt(stmt, &mut local_scope, vars, &mut out);
        }
        out
    }

    fn lower_stmt(&mut self, stmt: &Stmt, scope: &mut Scope, vars: &[SirVar], out: &mut Vec<SirStmt>) {
        match stmt {
            Stmt::Expr(expr) => self.lower_expr_stmt(expr, scope, vars, out),
            Stmt::Let(l) => {
                let ty = infer_type_from_expr(&l.init);
                let value = self.lower_expr_emit(&l.init, scope, vars, out);
                scope.insert(&l.name.name, Binding::Local(l.name.name.clone(), ty));
                out.push(SirStmt::Assign { target: SirPlace::Var(l.name.name.clone()), value });
            }
            Stmt::Become(_, _) => {} // typestate transition — later
            Stmt::Return(expr, _) => {
                // Inside an inlined op body, `return X` binds the op's result
                // local; at the reaction top level it is a no-op.
                let target = self.op_result.last().cloned();
                if let (Some(e), Some(t)) = (expr, target) {
                    let val = self.lower_expr_emit(e, scope, vars, out);
                    out.push(SirStmt::Assign { target: SirPlace::Var(t), value: val });
                } else if let Some(e) = expr {
                    let _ = self.lower_expr_emit(e, scope, vars, out);
                }
            }
            Stmt::Exit(code, _) => {
                let val = self.lower_expr_emit(code, scope, vars, out);
                out.push(SirStmt::Exit(val));
            }
        }
    }

    fn lower_expr_stmt(&mut self, expr: &Expr, scope: &mut Scope, vars: &[SirVar], out: &mut Vec<SirStmt>) {
        match &expr.kind {
            ExprKind::Assign(lhs, rhs) => {
                if let Some(place) = self.expr_to_place(lhs, scope) {
                    let value = self.lower_expr_emit(rhs, scope, vars, out);
                    out.push(SirStmt::Assign { target: place, value });
                }
            }
            ExprKind::CompoundAssign(op, lhs, rhs) => {
                if let Some(place) = self.expr_to_place(lhs, scope) {
                    let lhs_val = self.lower_expr(lhs, scope);
                    let rhs_val = self.lower_expr_emit(rhs, scope, vars, out);
                    let combined = SirExpr::BinOp(ast_binop_to_sir(*op), Box::new(lhs_val), Box::new(rhs_val));
                    out.push(SirStmt::Assign { target: place, value: combined });
                }
            }
            ExprKind::Call { callee, args, named: _ } => {
                if let ExprKind::Field(dev_expr, method) = &callee.kind {
                    if let Some(device_name) = expr_root_ident(dev_expr) {
                        match scope.lookup(device_name).cloned() {
                            Some(Binding::IntrinsicDevice(IntrinsicDevice::HostIo)) => {
                                if let Some(s) = self.lower_host_io_call(method, args, scope) {
                                    out.push(s);
                                }
                                return;
                            }
                            Some(Binding::IntrinsicDevice(IntrinsicDevice::Sys)) => {
                                self.err(expr.span, "sys device has no callable ops");
                                return;
                            }
                            Some(Binding::Pin(pin)) => {
                                if let Some(s) = self.lower_pin_call(&pin, method, args, expr.span, scope) {
                                    out.push(s);
                                }
                                return;
                            }
                            Some(Binding::Device(inst)) => {
                                // op call as a statement: lower, discard the result.
                                self.lower_op_call(&inst, &method.name, args, false, expr.span, scope, vars, out);
                                return;
                            }
                            None => {
                                self.err(dev_expr.span, format!("undefined device '{}'", device_name));
                                return;
                            }
                            _ => {
                                self.err(dev_expr.span, format!("'{}' is not a callable device", device_name));
                                return;
                            }
                        }
                    }
                }
                self.err(expr.span, "unsupported call expression form");
            }
            _ => {
                let _ = self.lower_expr_emit(expr, scope, vars, out);
            }
        }
    }

    /// Like `lower_expr`, but may **emit statements** into `out` (for device-op
    /// calls, which lower to a `BusXfer` or to inlined op bodies) and returns the
    /// value expression to use.  Pure expressions delegate to `lower_expr`.
    fn lower_expr_emit(&mut self, expr: &Expr, scope: &mut Scope, vars: &[SirVar], out: &mut Vec<SirStmt>) -> SirExpr {
        match &expr.kind {
            // `<call>?` — fault propagates out of the enclosing op/reaction (§4.4).
            ExprKind::Try(inner) => {
                if let ExprKind::Call { callee, args, .. } = &inner.kind {
                    if let ExprKind::Field(dev_expr, method) = &callee.kind {
                        if let Some(root) = expr_root_ident(dev_expr) {
                            if let Some(Binding::Device(inst)) = scope.lookup(root).cloned() {
                                return self.lower_op_call(&inst, &method.name, args, true, expr.span, scope, vars, out);
                            }
                        }
                    }
                }
                self.lower_expr_emit(inner, scope, vars, out)
            }
            ExprKind::Call { callee, args, .. } => {
                if let ExprKind::Field(dev_expr, method) = &callee.kind {
                    if let Some(root) = expr_root_ident(dev_expr) {
                        if let Some(Binding::Device(inst)) = scope.lookup(root).cloned() {
                            return self.lower_op_call(&inst, &method.name, args, false, expr.span, scope, vars, out);
                        }
                    }
                }
                self.lower_expr(expr, scope) // pin reads etc.
            }
            ExprKind::BinOp { op, lhs, rhs } => {
                let l = self.lower_expr_emit(lhs, scope, vars, out);
                let r = self.lower_expr_emit(rhs, scope, vars, out);
                SirExpr::BinOp(ast_binop_to_sir(*op), Box::new(l), Box::new(r))
            }
            ExprKind::Not(inner) => SirExpr::Not(Box::new(self.lower_expr_emit(inner, scope, vars, out))),
            _ => self.lower_expr(expr, scope),
        }
    }

    /// Lower a composed/leaf device-op call (the keystone, §3.5).  A primitive
    /// (empty-bodied, `yields`) op is a bus transaction → `SirStmt::BusXfer`;
    /// an op with a body is **inlined** (params + `needs` substituted), recursing
    /// to the substrate.  Returns the result value.
    #[allow(clippy::too_many_arguments)] // inherent lowering context (call + scope/vars/out)
    fn lower_op_call(
        &mut self,
        inst: &InstanceRef,
        op_name: &str,
        args: &[Expr],
        propagate: bool,
        span: Span,
        scope: &mut Scope,
        vars: &[SirVar],
        out: &mut Vec<SirStmt>,
    ) -> SirExpr {
        let Some(op) = self.find_op(&inst.ty, op_name).cloned() else {
            self.err(span, format!("device type '{}' has no op '{}'", inst.ty, op_name));
            return SirExpr::U64(0);
        };
        if args.len() != op.params.len() {
            self.err(span, format!("op '{}' takes {} arg(s), got {}", op_name, op.params.len(), args.len()));
            return SirExpr::U64(0);
        }
        let arg_vals: Vec<SirExpr> = args.iter().map(|a| self.lower_expr_emit(a, scope, vars, out)).collect();

        // Primitive bus transaction: empty body + yields → serviced by the
        // substrate (sim bus model / metal MMIO).
        if op.body.stmts.is_empty() && op.yields {
            let dst = self.fresh("bus");
            let fault_codes = op.ret.fault_codes.iter().map(|c| c.name.clone()).collect();
            out.push(SirStmt::BusXfer {
                device: inst.device,
                op: op_name.to_string(),
                args: arg_vals,
                dst: dst.clone(),
                propagate,
                fault_codes,
            });
            return SirExpr::Load(dst);
        }

        // Otherwise inline the op body with params + needs substituted.
        let mut inner = Scope::new();
        for (p, v) in op.params.iter().zip(arg_vals) {
            let tmp = self.fresh("arg");
            out.push(SirStmt::Assign { target: SirPlace::Var(tmp.clone()), value: v });
            inner.insert(&p.name.name, Binding::Local(tmp, SirType::U32));
        }
        if let Some(needs) = self.instance_needs.get(&inst.device).cloned() {
            for (name, val) in needs {
                match val {
                    NeedVal::Device(r) => inner.insert(&name, Binding::Device(r)),
                    NeedVal::Const(c) => {
                        let tmp = self.fresh("need");
                        out.push(SirStmt::Assign { target: SirPlace::Var(tmp.clone()), value: SirExpr::U64(c) });
                        inner.insert(&name, Binding::Local(tmp, SirType::U32));
                    }
                }
            }
        }
        let result = self.fresh("r");
        out.push(SirStmt::Assign { target: SirPlace::Var(result.clone()), value: SirExpr::U64(0) });
        self.op_result.push(result.clone());
        for stmt in &op.body.stmts {
            self.lower_stmt(stmt, &mut inner, vars, out);
        }
        self.op_result.pop();
        SirExpr::Load(result)
    }

    fn fresh(&mut self, prefix: &str) -> String {
        let n = self.tmp_counter;
        self.tmp_counter += 1;
        format!("__{}{}", prefix, n)
    }

    /// Lower a pin op call (`led.set(x)`) to a register access (§6.5).
    ///
    /// The op→register mapping is data-driven: a pin op with parameters is a
    /// *write* (targets the port's writable data register); a parameterless op
    /// returning a value is a *read* (its input register).  The bit is the
    /// bound pin index.  This keeps the register addresses as std-lib data and
    /// uses only the op's *shape* — the compiler core has no `gpio` knowledge.
    fn lower_pin_call(
        &mut self,
        pin: &PinRef,
        method: &Ident,
        args: &[Expr],
        span: Span,
        scope: &Scope,
    ) -> Option<SirStmt> {
        let op = self.find_op(&pin.port_type, &method.name).cloned();
        let op = match op {
            Some(o) => o,
            None => {
                self.err(span, format!("device type '{}' has no op '{}'", pin.port_type, method.name));
                return None;
            }
        };

        if op.params.is_empty() {
            // A read op as a statement has no effect; ignore (reads are used as
            // values, handled in `lower_expr`).
            self.err(span, format!("op '{}' returns a value; use it in an expression", method.name));
            return None;
        }

        // A write-shaped pin op takes exactly its argument; reject wrong arity
        // (and avoid indexing `args[0]` blindly).
        if args.len() != op.params.len() {
            self.err(
                span,
                format!(
                    "op '{}' takes {} argument(s), got {}",
                    method.name,
                    op.params.len(),
                    args.len()
                ),
            );
            return None;
        }

        // Write op → store the argument into the output register bit.
        let reg = match self.find_output_reg(&pin.port_type) {
            Some(r) => r,
            None => {
                self.err(span, format!("device type '{}' has no writable data register", pin.port_type));
                return None;
            }
        };
        let value = self.lower_expr(&args[0], scope);
        let place = SirPlace::Reg {
            device: pin.port_device,
            reg_offset: reg.0,
            width: reg.1,
            field_mask: 1u64 << pin.index,
            field_shift: pin.index,
            access: reg.2,
        };
        Some(SirStmt::Assign { target: place, value })
    }

    fn lower_host_io_call(&mut self, method: &Ident, args: &[Expr], scope: &Scope) -> Option<SirStmt> {
        match method.name.as_str() {
            "print" => {
                if args.len() != 1 {
                    self.err(method.span, "host_io.print takes exactly 1 argument");
                    return None;
                }
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
            ExprKind::Ident(ident) => match scope.lookup(&ident.name) {
                Some(Binding::Local(name, _)) | Some(Binding::Cell(name, _)) => SirExpr::Load(name.clone()),
                _ => {
                    self.err(ident.span, format!("undefined variable '{}'", ident.name));
                    SirExpr::Load(ident.name.clone())
                }
            },
            ExprKind::Not(inner) => SirExpr::Not(Box::new(self.lower_expr(inner, scope))),
            ExprKind::BinOp { op, lhs, rhs } => {
                let l = self.lower_expr(lhs, scope);
                let r = self.lower_expr(rhs, scope);
                SirExpr::BinOp(ast_binop_to_sir(*op), Box::new(l), Box::new(r))
            }
            ExprKind::Assign(_lhs, rhs) => self.lower_expr(rhs, scope),
            ExprKind::CompoundAssign(_, _, rhs) => self.lower_expr(rhs, scope),
            ExprKind::Try(inner) => self.lower_expr(inner, scope), // fault `?` — Phase 1
            ExprKind::Call { callee, args, named: _ } => {
                // A pin *read* op used as a value (`pin.get()`).  Only a
                // read-shaped op (declared, parameterless) lowers to a register
                // read — using the op name and arity, so `led.set(true)` in
                // value position is diagnosed, not silently turned into a read.
                if let ExprKind::Field(dev_expr, method) = &callee.kind {
                    if let Some(root) = expr_root_ident(dev_expr) {
                        if let Some(Binding::Pin(pin)) = scope.lookup(root).cloned() {
                            match self.find_op(&pin.port_type, &method.name).cloned() {
                                None => {
                                    self.err(
                                        expr.span,
                                        format!("device type '{}' has no op '{}'", pin.port_type, method.name),
                                    );
                                    return SirExpr::U64(0);
                                }
                                Some(op) if !op.params.is_empty() || !args.is_empty() => {
                                    self.err(
                                        expr.span,
                                        format!("op '{}' is not a value-returning read op", method.name),
                                    );
                                    return SirExpr::U64(0);
                                }
                                Some(_) => {
                                    if let Some(reg) = self.find_input_reg(&pin.port_type) {
                                        return SirExpr::RegLoad {
                                            device: pin.port_device,
                                            reg_offset: reg.0,
                                            width: reg.1,
                                            field_mask: 1u64 << pin.index,
                                            field_shift: pin.index,
                                            access: reg.2,
                                        };
                                    }
                                    self.err(
                                        expr.span,
                                        format!("device type '{}' has no readable input register", pin.port_type),
                                    );
                                    return SirExpr::U64(0);
                                }
                            }
                        }
                    }
                }
                self.err(expr.span, "call/field expression not supported as a value here");
                SirExpr::U64(0)
            }
            ExprKind::Field(_, _) => {
                self.err(expr.span, "field expression not supported as a value here");
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

    // ── Device-type register / op lookups (data-driven, §2) ───────────────────

    fn find_op<'a>(&'a self, ty: &str, name: &str) -> Option<&'a OpDecl> {
        let def = self.device_defs.get(ty)?;
        let ops = def.sections.ops.as_ref()?;
        ops.items.iter().find_map(|it| {
            let OpsItem::Op(o) = it;
            (o.name.name == name).then_some(o)
        })
    }

    /// The port's writable data register: `(offset, width, access)`.
    fn find_output_reg(&self, ty: &str) -> Option<(u64, u8, SirRegAccess)> {
        let def = self.device_defs.get(ty)?;
        let regs = def.sections.regs.as_ref()?;
        regs.regs
            .iter()
            .find(|r| matches!(r.access, RegAccess::Rw | RegAccess::Wo))
            .map(|r| (r.offset, r.width, map_access(r.access)))
    }

    /// The port's direction register: a writable register distinct from the
    /// output *data* register (for nrf_gpio: `DIR`, vs the `OUT` data reg).
    /// Heuristic for the slice — a faithful model would tag the register's role
    /// or drive direction from a device init op; documented as a Phase-1 swap.
    fn find_dir_reg(&self, ty: &str) -> Option<(u64, u8)> {
        let out = self.find_output_reg(ty)?;
        let def = self.device_defs.get(ty)?;
        let regs = def.sections.regs.as_ref()?;
        regs.regs
            .iter()
            .find(|r| matches!(r.access, RegAccess::Rw | RegAccess::Wo) && r.offset != out.0)
            .map(|r| (r.offset, r.width))
    }

    /// The port's readable input register: `(offset, width, access)`.
    fn find_input_reg(&self, ty: &str) -> Option<(u64, u8, SirRegAccess)> {
        let def = self.device_defs.get(ty)?;
        let regs = def.sections.regs.as_ref()?;
        regs.regs
            .iter()
            .find(|r| matches!(r.access, RegAccess::Ro))
            .map(|r| (r.offset, r.width, map_access(r.access)))
    }

    // ── Cell concurrency analysis (§5.5) ──────────────────────────────────────

    /// Build the static reaction↔cell access graph, compute each cell's
    /// priority ceiling, and wrap shared-cell accesses in `SirStmt::Critical`.
    fn analyze_cells(&mut self, reactions: &mut [SirReaction], vars: &[SirVar]) {
        let cell_names: Vec<String> =
            vars.iter().filter(|v| v.is_cell).map(|v| v.name.clone()).collect();
        if cell_names.is_empty() {
            return;
        }

        // touched_by[cell] = reactions that read or write it; ceiling = max prio.
        let mut touched: HashMap<String, Vec<usize>> = HashMap::new();
        let mut ceiling: HashMap<String, u8> = HashMap::new();
        for r in reactions.iter() {
            for cell in &cell_names {
                if stmts_touch_cell(&r.body, cell) {
                    touched.entry(cell.clone()).or_default().push(r.id);
                    let c = ceiling.entry(cell.clone()).or_insert(0);
                    *c = (*c).max(r.priority);
                }
            }
        }

        // Shared = touched by ≥2 reactions → needs a critical section.
        let shared: HashMap<String, u8> = touched
            .iter()
            .filter(|(_, rs)| rs.len() >= 2)
            .map(|(name, _)| (name.clone(), ceiling[name]))
            .collect();

        // Wrap each shared-cell access in a priority-ceiling critical section.
        for r in reactions.iter_mut() {
            let body = std::mem::take(&mut r.body);
            r.body = body
                .into_iter()
                .map(|stmt| {
                    let ceil = shared
                        .iter()
                        .filter(|(cell, _)| stmt_touches_cell(&stmt, cell))
                        .map(|(_, &c)| c)
                        .max();
                    match ceil {
                        Some(c) => SirStmt::Critical { ceiling: c, body: vec![stmt] },
                        None => stmt,
                    }
                })
                .collect();
        }

        // Record the analysis (§5.5): single-owner cells proved section-free.
        for cell in &cell_names {
            let by = touched.get(cell).cloned().unwrap_or_default();
            self.cells.push(CellInfo {
                name: cell.clone(),
                ceiling: ceiling.get(cell).copied().unwrap_or(0),
                single_owner: by.len() == 1,
                touched_by: by,
            });
        }
    }

    // ── Sim script ────────────────────────────────────────────────────────────

    fn resolve_sim(&mut self, sim: &SimDef) {
        // Resolve against *this sim's* program's board context, not whichever
        // program happened to be lowered last.
        let ctx = match self.program_ctx.get(&sim.program.name) {
            Some(c) => c.clone(),
            None => {
                self.err(
                    sim.span,
                    format!("sim '{}' targets unknown program '{}'", sim.name.name, sim.program.name),
                );
                return;
            }
        };
        for inj in &sim.injections {
            // The sim references board pin-binding names directly (e.g.
            // `btn_user.falling`), resolved via the program's board context.
            let device_name = match expr_root_ident(&inj.event.device) {
                Some(n) => n.to_string(),
                None => {
                    self.err(inj.span, "injected event device must be a simple identifier");
                    continue;
                }
            };
            let pin = match ctx.pins.get(&device_name).cloned() {
                Some(p) => p,
                None => {
                    self.err(inj.span, format!("unknown pin '{}' in sim injection", device_name));
                    continue;
                }
            };
            if let Some(ev) = self.resolve_pin_event(&pin, &inj.event.event.name, inj.span) {
                self.injections.push(SirInjection { at_ns: inj.at.to_ns(), event: ev });
            }
        }
        for f in &sim.faults {
            self.fault_injections.push(SirFaultInjection { at_ns: f.at.to_ns(), addr: f.addr });
        }
        for (code, times) in &sim.bus_faults {
            for _ in 0..*times {
                self.bus_fault_queue.push(code.name.clone());
            }
        }
        if let Some(d) = sim.run_until {
            self.run_until_ns = Some(d.to_ns());
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

fn map_access(a: RegAccess) -> SirRegAccess {
    match a {
        RegAccess::Ro => SirRegAccess::Ro,
        RegAccess::Wo => SirRegAccess::Wo,
        RegAccess::Rw => SirRegAccess::Rw,
        RegAccess::W1c => SirRegAccess::W1c,
        RegAccess::Rc => SirRegAccess::Rc,
    }
}

fn lower_regs(def: &DeviceDef) -> Vec<SirReg> {
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
fn stmts_touch_cell(stmts: &[SirStmt], cell: &str) -> bool {
    stmts.iter().any(|s| stmt_touches_cell(s, cell))
}

fn stmt_touches_cell(stmt: &SirStmt, cell: &str) -> bool {
    match stmt {
        SirStmt::Assign { target, value } => {
            place_touches_cell(target, cell) || expr_touches_cell(value, cell)
        }
        SirStmt::If { cond, then } => expr_touches_cell(cond, cell) || stmts_touch_cell(then, cell),
        SirStmt::Critical { body, .. } => stmts_touch_cell(body, cell),
        SirStmt::Exit(e) => expr_touches_cell(e, cell),
        SirStmt::Intrinsic(intr) => match intr {
            SirIntrinsic::HostIoPrint(e) => expr_touches_cell(e, cell),
            _ => false,
        },
        SirStmt::DeviceOp { args, .. } => args.iter().any(|a| expr_touches_cell(a, cell)),
        SirStmt::BusXfer { args, .. } => args.iter().any(|a| expr_touches_cell(a, cell)),
    }
}

/// True if any `Critical` section in the body (transitively) contains a yielding
/// bus transfer — the §5.5/D03 violation.
fn critical_contains_yield(stmts: &[SirStmt]) -> bool {
    stmts.iter().any(|s| match s {
        SirStmt::Critical { body, .. } => body_yields(body) || critical_contains_yield(body),
        SirStmt::If { then, .. } => critical_contains_yield(then),
        _ => false,
    })
}

/// True if a (possibly nested) statement list contains a yielding bus transfer.
fn body_yields(stmts: &[SirStmt]) -> bool {
    stmts.iter().any(|s| match s {
        SirStmt::BusXfer { .. } => true,
        SirStmt::Critical { body, .. } => body_yields(body),
        SirStmt::If { then, .. } => body_yields(then),
        _ => false,
    })
}

/// Lower a parsed fault disposition to its SIR form (§4.4); default `escalate`.
fn lower_disposition(d: &Option<FaultDisp>) -> SirDisposition {
    match d.as_ref().map(|f| &f.kind) {
        Some(FaultDispKind::Retry { max }) => SirDisposition::Retry { max: max.unwrap_or(1) },
        Some(FaultDispKind::Skip) => SirDisposition::Skip,
        Some(FaultDispKind::Safe) => SirDisposition::Safe,
        Some(FaultDispKind::Escalate) | None => SirDisposition::Escalate,
    }
}

fn place_touches_cell(place: &SirPlace, cell: &str) -> bool {
    matches!(place, SirPlace::Var(n) if n == cell)
}

fn expr_touches_cell(expr: &SirExpr, cell: &str) -> bool {
    match expr {
        SirExpr::Load(n) => n == cell,
        SirExpr::Not(inner) => expr_touches_cell(inner, cell),
        SirExpr::BinOp(_, l, r) => expr_touches_cell(l, cell) || expr_touches_cell(r, cell),
        _ => false,
    }
}

/// Extract the root identifier name from an expression like `foo` or `foo.bar`.
fn expr_root_ident(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Ident(ident) => Some(&ident.name),
        ExprKind::Field(inner, _) => expr_root_ident(inner),
        _ => None,
    }
}

/// Minimal type inference for initialiser expressions.
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
            _ => SirType::U32,
        },
        TypeKind::Unit => SirType::U8,
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

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lexer::lex;
    use crate::parser::parse;

    /// A self-contained gpio device + board prelude reused by the tests.
    const PRELUDE: &str = r#"
device gpio {
    regs {
        IDR : reg32 at 0x10 access ro {}
        ODR : reg32 at 0x14 access rw {}
    }
    needs { clock : clock_source }
    ops {
        op set(level: bool) -> () {}
        op get() -> bool {}
    }
    emits falling : event
}

board demo_board {
    soc demo_soc {
        clocks { sysclk : clock_source = 8MHz }
    }
    gpio_a : gpio at 0x4002_0000 { needs { clock = soc.sysclk } }
    gpio_c : gpio at 0x4002_0800 { needs { clock = soc.sysclk } }
    led_user : gpio.pin = gpio_a.pin(5)  as output
    btn_user : gpio.pin = gpio_c.pin(13) as input pulling up
}
"#;

    fn resolve_src(src: &str) -> Result<SirModule, Vec<ResolveError>> {
        let tokens = lex(src).expect("lex failed");
        let ast = parse(tokens).expect("parse failed");
        resolve(&ast)
    }

    fn count_criticals(stmts: &[SirStmt]) -> usize {
        stmts
            .iter()
            .map(|s| match s {
                SirStmt::Critical { body, .. } => 1 + count_criticals(body),
                SirStmt::If { then, .. } => count_criticals(then),
                _ => 0,
            })
            .sum()
    }

    #[test]
    fn blink_button_resolves_with_shared_cell() {
        let src = format!(
            "{PRELUDE}
program blink {{
    use board demo_board as dev
    let led = dev.led_user
    let button = dev.btn_user
    cell lit : bool = false
    every 500ms {{ lit = not lit  led.set(lit) }}
    on button.falling {{ lit = not lit  led.set(lit) }}
}}
sim blink_demo for blink {{
    inject btn_user.falling at 1200ms
    run until 3000ms
}}
"
        );
        let sir = resolve_src(&src).expect("resolve failed");

        // Two reactions: every (id 0) + on button.falling (id 1).
        assert_eq!(sir.reactions.len(), 2);
        assert!(matches!(sir.reactions[0].trigger, SirTrigger::EveryNs(500_000_000)));
        assert!(matches!(sir.reactions[1].trigger, SirTrigger::Event(_)));
        assert_eq!(sir.reactions[0].priority, 1);
        assert_eq!(sir.reactions[1].priority, 2);

        // `lit` is shared (touched by both), ceiling = button priority (2),
        // not a single owner (§5.5).
        let lit = sir.cells.iter().find(|c| c.name == "lit").expect("lit cell");
        assert_eq!(lit.ceiling, 2);
        assert!(!lit.single_owner);
        assert_eq!(lit.touched_by.len(), 2);

        // Both statements in each reaction touch `lit` → both wrapped in a
        // critical section.  (`lit = not lit` writes; `led.set(lit)` reads.)
        assert_eq!(count_criticals(&sir.reactions[0].body), 2);
        assert_eq!(count_criticals(&sir.reactions[1].body), 2);

        // The injection resolved to the same event the `on` reaction binds.
        assert_eq!(sir.injections.len(), 1);
        if let SirTrigger::Event(ev) = sir.reactions[1].trigger {
            assert_eq!(sir.injections[0].event, ev);
        }
        assert_eq!(sir.run_until_ns, Some(3_000_000_000));

        // The LED write lowered to a register access on the ODR (rw) register
        // at offset 0x14, bit 5 — a target-neutral MMIO node (§6.5).
        assert!(find_reg_write(&sir.reactions[0].body, 0x14, 5));
    }

    fn find_reg_write(stmts: &[SirStmt], offset: u64, bit: u8) -> bool {
        stmts.iter().any(|s| match s {
            SirStmt::Assign { target: SirPlace::Reg { reg_offset, field_shift, .. }, .. } => {
                *reg_offset == offset && *field_shift == bit
            }
            SirStmt::Critical { body, .. } => find_reg_write(body, offset, bit),
            _ => false,
        })
    }

    #[test]
    fn single_owner_cell_needs_no_critical_section() {
        let src = format!(
            "{PRELUDE}
program blink {{
    use board demo_board as dev
    let led = dev.led_user
    cell lit : bool = false
    every 500ms {{ lit = not lit  led.set(lit) }}
}}
"
        );
        let sir = resolve_src(&src).expect("resolve failed");
        let lit = sir.cells.iter().find(|c| c.name == "lit").expect("lit cell");
        assert!(lit.single_owner);
        assert_eq!(lit.touched_by.len(), 1);
        // No critical section is inserted for a proven single-owner cell.
        assert_eq!(count_criticals(&sir.reactions[0].body), 0);
    }

    #[test]
    fn duplicate_pad_is_a_compile_error() {
        // Two pin bindings claiming the same physical pad gpio_a.pin(5).
        let src = r#"
device gpio {
    regs { ODR : reg32 at 0x14 access rw {} }
    ops { op set(level: bool) -> () {} }
    emits falling : event
}
board demo_board {
    gpio_a : gpio at 0x4002_0000 {}
    led_user : gpio.pin = gpio_a.pin(5) as output
    other    : gpio.pin = gpio_a.pin(5) as output
}
program p {
    use board demo_board as dev
    let led = dev.led_user
}
"#;
        let errs = resolve_src(src).expect_err("expected a duplicate-pad error");
        assert!(
            errs.iter().any(|e| e.msg.contains("already owned")),
            "expected a duplicate-pad diagnostic, got: {:?}",
            errs.iter().map(|e| &e.msg).collect::<Vec<_>>()
        );
    }

    #[test]
    fn pin_write_with_wrong_arity_is_a_compile_error() {
        let src = format!(
            "{PRELUDE}
program p {{
    use board demo_board as dev
    let led = dev.led_user
    every 500ms {{ led.set() }}
}}
"
        );
        let errs = resolve_src(&src).expect_err("expected arity error");
        assert!(
            errs.iter().any(|e| e.msg.contains("op 'set' takes 1 argument(s), got 0")),
            "got: {:?}",
            errs.iter().map(|e| &e.msg).collect::<Vec<_>>()
        );
    }

    #[test]
    fn pin_write_used_as_a_value_is_a_compile_error() {
        // `led.set(true)` in value position must be diagnosed, not silently
        // lowered to an input-register read.
        let src = format!(
            "{PRELUDE}
program p {{
    use board demo_board as dev
    let led = dev.led_user
    every 500ms {{ let x = led.set(true) }}
}}
"
        );
        let errs = resolve_src(&src).expect_err("expected value-position error");
        assert!(
            errs.iter().any(|e| e.msg.contains("not a value-returning read op")),
            "got: {:?}",
            errs.iter().map(|e| &e.msg).collect::<Vec<_>>()
        );
    }

    #[test]
    fn unknown_event_is_a_compile_error() {
        let src = format!(
            "{PRELUDE}
program p {{
    use board demo_board as dev
    let button = dev.btn_user
    on button.rising {{ }}
}}
"
        );
        let errs = resolve_src(&src).expect_err("expected unknown-event error");
        assert!(errs.iter().any(|e| e.msg.contains("does not emit event 'rising'")));
    }
}
