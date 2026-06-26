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
    /// A device register, in scope inside that device's own op bodies (leaf
    /// MMIO): `OUT = 0` / `OUT.enable = 0` lowers to a `SirPlace::Reg`.
    Reg(RegInfo),
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

/// A device register (+ its fields), for lowering register references inside the
/// device's own op bodies to `SirPlace::Reg`.
#[derive(Debug, Clone)]
struct RegInfo {
    device: usize,
    offset: u64,
    width: u8,
    access: SirRegAccess,
    /// field name → (mask, shift, access).  Access is the field's own qualifier
    /// when it declares one, else the register's (§4.2/D04, audit #35 P0-2a) —
    /// so a `w1c` status bit inside an `rw` register lowers to a single masked
    /// write, not a read-modify-write that would clobber its siblings.
    fields: HashMap<String, (u64, u8, SirRegAccess)>,
    /// Reading the register has a side effect (`rc`, `pop_on_read`/`side_effect`,
    /// or any `rc` field), so an implicit read-modify-write of one field would
    /// disturb it (§4.2/D04, audit #35 P0-2b).
    read_side_effect: bool,
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
    /// Whether this board's SoC declares an FPU (§4.1/§4.3), gating `float`.
    fpu: bool,
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
    /// §4.1/D07 static typestate: device id → its provable current state within
    /// the reaction being lowered (cleared at each reaction boundary).
    device_states: HashMap<usize, String>,
    /// (device id, op name) currently being inlined — the active call path.  A
    /// re-entry of the same entry is recursion, which is banned (§5.3/SIL-005)
    /// and would also hang the inliner.
    inlining: Vec<(usize, String)>,

    // ── output accumulators ──
    devices: Vec<SirDevice>,
    events: Vec<SirEvent>,
    cells: Vec<CellInfo>,
    injections: Vec<SirInjection>,
    fault_injections: Vec<SirFaultInjection>,
    bus_fault_queue: Vec<String>,
    safe_seqs: Vec<SafeSeq>,
    watchdog_timeout_ns: Option<u64>,
    watchdog_device: Option<usize>,
    bus_hangs: u32,
    run_until_ns: Option<u64>,
    memory: Vec<SirRegion>,
    pins: Vec<SirPin>,
    core_hz: u64,

    /// Device id → device-type name (for reg/op/emit lookups).
    dev_types: HashMap<usize, String>,
    /// Result width + signedness for arithmetic currently being lowered, taken
    /// from the enclosing assignment/`let` target type (§4.3 overflow checks).
    /// Defaults to 32-bit unsigned (the implicit integer type).
    arith_width: u8,
    arith_signed: bool,
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
            device_states: HashMap::new(),
            inlining: Vec::new(),
            devices: Vec::new(),
            events: Vec::new(),
            cells: Vec::new(),
            injections: Vec::new(),
            fault_injections: Vec::new(),
            bus_fault_queue: Vec::new(),
            safe_seqs: Vec::new(),
            watchdog_timeout_ns: None,
            watchdog_device: None,
            bus_hangs: 0,
            run_until_ns: None,
            memory: Vec::new(),
            pins: Vec::new(),
            core_hz: 0,
            arith_width: 32,
            arith_signed: false,
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

        // ── Apply typed overlays (§3.6) to their target boards, before any
        //    board is built — so the config `where`-check below validates the
        //    overlaid value, not the original. ──
        for item in &module.items {
            if let Item::Overlay(o) = item {
                self.apply_overlay(o);
            }
        }

        // ── Check `implements` conformance (§4.1/D18) + register layout ──
        for item in &module.items {
            if let Item::Device(d) = item {
                self.check_conformance(d);
                self.check_regs(d);
                self.check_states(d);
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
                safe_seqs: self.safe_seqs,
                run_until_ns: self.run_until_ns,
                memory: self.memory,
                pins: self.pins,
                core_hz: self.core_hz,
                watchdog_timeout_ns: self.watchdog_timeout_ns,
                watchdog_device: self.watchdog_device,
                bus_hangs: self.bus_hangs,
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
                        // An explicit annotation may name `float` → FPU-gate it.
                        let ty = match &l.ty {
                            Some(t) => {
                                let st = resolve_type_expr(t);
                                self.float_needs_fpu(&st, t.span);
                                st
                            }
                            None => infer_type_from_expr(&l.init),
                        };
                        let init = self.lower_expr(&l.init, &scope);
                        scope.insert(&l.name.name, Binding::Local(l.name.name.clone(), ty.clone()));
                        vars.push(SirVar { name: l.name.name.clone(), ty, init, is_cell: false });
                    }
                }
                ProgramItem::CellDecl(c) => {
                    let ty = resolve_type_expr(&c.ty);
                    // §4.3: a cell's initialiser must fit its declared type.
                    self.check_literal_range(sirtype_valtype(&ty), &c.init, c.init.span);
                    self.float_needs_fpu(&ty, c.ty.span); // §4.3 FPU gate
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
        // A yielding op must sit at the reaction's top level in this slice: the
        // simulator's stepper splits segments only there.  Reject yields nested
        // in a critical section (§5.5/D03) or an `if` (fail fast — otherwise the
        // sim would silently skip the suspension).
        for r in &out[first..] {
            if critical_contains_yield(&r.body) {
                self.err(prog.span, "a cell critical section may not span a yield (§5.5/D03)");
            }
            if if_contains_yield(&r.body) {
                self.err(prog.span, "a yielding op inside `if` is not supported in this slice (§5.2: yields must be at the reaction top level)");
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

    /// Apply a typed overlay (§3.6) to its target board, type-checking each edit.
    /// `set` overrides a config field (whose `where` constraint is then checked by
    /// `build_board` on the new value); `remove` deletes an instance/pin binding.
    fn apply_overlay(&mut self, o: &OverlayDef) {
        let target = o.target.name.clone();
        let Some(mut board) = self.boards.remove(&target) else {
            self.err(o.span, format!("overlay '{}' targets unknown board '{}'", o.name.name, target));
            return;
        };
        for edit in &o.edits {
            match edit {
                OverlayEdit::Set { path, value, span } => {
                    if path.len() != 3 || path[1].name != "config" {
                        self.err(*span, "overlay `set` path must be `<instance>.config.<field>` (§3.6)");
                        continue;
                    }
                    let inst_name = path[0].name.clone();
                    let field = path[2].name.clone();
                    let Some(inst) = board.instances.iter_mut().find(|i| i.name.name == inst_name) else {
                        self.err(*span, format!("overlay `set`: board '{target}' has no instance '{inst_name}'"));
                        continue;
                    };
                    let dev_ty = inst.device_ty.name.clone();
                    let is_field = self
                        .device_defs
                        .get(&dev_ty)
                        .and_then(|d| d.sections.config.as_ref())
                        .map(|c| c.fields.iter().any(|f| f.name.name == field))
                        .unwrap_or(false);
                    if !is_field {
                        self.err(*span, format!("overlay `set`: device '{dev_ty}' has no config field '{field}' (§3.6)"));
                        continue;
                    }
                    if let Some(slot) = inst.config.iter_mut().find(|(k, _)| k.name == field) {
                        slot.1 = value.clone();
                    } else {
                        inst.config.push((path[2].clone(), value.clone()));
                    }
                }
                OverlayEdit::Remove { path, span } => {
                    if path.len() != 1 {
                        self.err(*span, "overlay `remove` takes a single instance/binding name (§3.6)");
                        continue;
                    }
                    let name = path[0].name.clone();
                    let before = board.instances.len() + board.pin_bindings.len();
                    board.instances.retain(|i| i.name.name != name);
                    board.pin_bindings.retain(|p| p.name.name != name);
                    if before == board.instances.len() + board.pin_bindings.len() {
                        self.err(*span, format!("overlay `remove`: board '{target}' has no '{name}'"));
                    }
                }
            }
        }
        self.boards.insert(target, board);
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

        // §3.2/§4.1 — enforce `where` constraints on config fields. Each field's
        // effective value (instance override, else device default) is bound, then
        // every constrained field's predicate is const-evaluated; anything that does
        // not evaluate to `true` is a compile error rather than a parsed-but-ignored
        // annotation.
        for inst in &board.instances {
            let Some(def) = self.device_defs.get(&inst.device_ty.name).cloned() else { continue };
            let Some(cfg) = def.sections.config.as_ref() else { continue };
            // Effective config environment (constants only).
            let mut env: HashMap<String, ConstVal> = HashMap::new();
            for f in &cfg.fields {
                let val_expr = inst
                    .config
                    .iter()
                    .find(|(k, _)| k.name == f.name.name)
                    .map(|(_, e)| e)
                    .or(f.default.as_ref());
                if let Some(e) = val_expr {
                    if let Some(v) = const_eval(e, &env) {
                        env.insert(f.name.name.clone(), v);
                    }
                }
            }
            for f in &cfg.fields {
                let Some(c) = &f.constraint else { continue };
                if !env.contains_key(&f.name.name) {
                    continue; // no concrete value (e.g. required field unset) — nothing to check
                }
                match const_eval(c, &env) {
                    Some(ConstVal::Bool(true)) => {}
                    Some(ConstVal::Bool(false)) => {
                        let span = inst
                            .config
                            .iter()
                            .find(|(k, _)| k.name == f.name.name)
                            .map(|(_, e)| e.span)
                            .unwrap_or(inst.span);
                        let shown = match env.get(&f.name.name) {
                            Some(ConstVal::Int(n)) => format!(" = {}", n),
                            _ => String::new(),
                        };
                        self.err(
                            span,
                            format!(
                                "instance '{}': config `{}`{} violates its `where` constraint (§4.1)",
                                inst.name.name, f.name.name, shown
                            ),
                        );
                    }
                    Some(ConstVal::Int(_)) => self.err(
                        c.span,
                        format!("`where` constraint on `{}` must be a boolean expression (§4.1)", f.name.name),
                    ),
                    None => {} // non-constant constraint — left unchecked, not a hard error
                }
            }
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
                            let iface = nd.ty.name.clone();
                            if self.interfaces.contains_key(&iface) && !self.implements(&target.ty, &iface) {
                                self.err(
                                    inst.span,
                                    format!(
                                        "instance '{}': `{}` requires an `{}` provider, but '{}' (type '{}') does not implement it",
                                        inst.name.name, need_name.name, iface, head, target.ty
                                    ),
                                );
                            }
                            // §4.1/D18: check the `where` constraint over the
                            // provider's semantic properties.
                            if let Some(constraint) = nd.constraint.clone() {
                                self.check_need_properties(&inst.name.name, &need_name.name, &iface, &target.ty, &constraint, inst.span);
                            }
                        }
                    }
                    resolved.insert(need_name.name.clone(), NeedVal::Device(target.clone()));
                }
            }
            self.instance_needs.insert(dev_id, resolved);
        }

        // Identify the system watchdog (§5.6/SIL-006): the instance whose device
        // implements the `watchdog` interface (structural, not by name — §2).
        // Its `timeout` comes from the instance config or the device default.
        let mut wdt_seen = false;
        for inst in &board.instances {
            if !self.implements(&inst.device_ty.name, "watchdog") {
                continue;
            }
            if wdt_seen {
                self.err(inst.span, "more than one watchdog on the board; only one system watchdog is supported (§5.6)");
                continue;
            }
            wdt_seen = true;
            // Instance config `timeout` overrides the device default; if present
            // it must be a constant (don't silently fall back to the default).
            let from_inst = inst.config.iter().find(|(k, _)| k.name == "timeout").map(|(_, e)| {
                if let Some(n) = lit_int(&e.kind) { Some(n) } else {
                    self.err(e.span, "watchdog `timeout` must be a constant duration");
                    None
                }
            });
            let timeout = match from_inst {
                Some(v) => v, // instance set it (Some(n) or None after the error)
                None => self
                    .device_defs
                    .get(&inst.device_ty.name)
                    .and_then(|d| d.sections.config.as_ref())
                    .and_then(|c| c.fields.iter().find(|f| f.name.name == "timeout"))
                    .and_then(|f| f.default.as_ref())
                    .and_then(|e| lit_int(&e.kind)),
            };
            self.watchdog_timeout_ns = timeout;
            self.watchdog_device = instance_ids.get(&inst.name.name).copied();
        }

        // Lower each instance's safe op (§5.6), if its type declares one.
        // Iterate `board.instances` (a Vec) for a deterministic order (§7.1/D19),
        // not the `instances` HashMap.
        for inst in &board.instances {
            if let Some(target) = instances.get(&inst.name.name).cloned() {
                if let Some(seq) = self.lower_safe_seq(target.device, &target.ty) {
                    self.safe_seqs.push(seq);
                }
            }
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
        let fpu = board.soc.as_ref().map(|s| s.fpu).unwrap_or(false);
        let ctx = BoardContext { pins, instances, fpu };
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

        // §4.1/D07: each reaction starts with every device in its declared
        // initial state — typestate is not carried across an event boundary.
        self.device_states.clear();
        let body = self.lower_block(&r.body, scope, vars);
        let yields = body_yields(&body);
        let disposition = lower_disposition(&r.fault_disp);
        let deadline_ns = r.within.as_ref().map(|d| d.to_ns());
        let overflow = match r.overflow {
            Some(OverflowPolicy::DropNewest) => SirOverflow::DropNewest,
            Some(OverflowPolicy::Fault) => SirOverflow::Fault,
            _ => SirOverflow::Coalesce, // default (§5.1/D02)
        };
        Some(SirReaction { id, trigger, body, priority, disposition, yields, deadline_ns, overflow })
    }

    /// §4.1/D07 — validate a device's typestate declarations: every op `when S`
    /// and every `become X` must name a state the device declares.
    fn check_states(&mut self, d: &DeviceDef) {
        let declared: Vec<String> = d
            .sections
            .states
            .as_ref()
            .map(|s| s.states.iter().map(|i| i.name.clone()).collect())
            .unwrap_or_default();
        let Some(ops) = &d.sections.ops else { return };
        for OpsItem::Op(op) in &ops.items {
            if let Some(when) = &op.when {
                if !declared.contains(&when.name) {
                    self.err(when.span, format!(
                        "op '{}' is guarded `when {}`, but device '{}' declares no such state (§4.1)",
                        op.name.name, when.name, d.name.name
                    ));
                }
            }
            for stmt in &op.body.stmts {
                if let Stmt::Become(state, span) = stmt {
                    if !declared.contains(&state.name) {
                        self.err(*span, format!(
                            "`become {}` in op '{}', but device '{}' declares no such state (§4.1)",
                            state.name, op.name.name, d.name.name
                        ));
                    }
                }
            }
        }
    }

    /// The device's provable current typestate (§4.1/D07): the tracked state if a
    /// `become` has run this reaction, else the type's declared initial state
    /// (the first listed), or `None` if the type declares no states.
    fn current_device_state(&self, device_id: usize, ty: &str) -> Option<String> {
        if let Some(s) = self.device_states.get(&device_id) {
            return Some(s.clone());
        }
        self.device_defs
            .get(ty)
            .and_then(|d| d.sections.states.as_ref())
            .and_then(|s| s.states.first())
            .map(|i| i.name.clone())
    }

    /// §4.1/§4.3 — `float`/`f64` is allowed only on a board whose SoC declares an
    /// `fpu` capability; otherwise it is a compile error (no silent soft-float).
    fn float_needs_fpu(&mut self, ty: &SirType, span: Span) {
        if matches!(ty, SirType::F32 | SirType::F64) {
            let fpu = self.board_ctx.as_ref().map(|c| c.fpu).unwrap_or(false);
            if !fpu {
                self.err(
                    span,
                    "`float` requires an FPU, but this program's SoC declares none (§4.3) — use `fixed<…>`, or add `fpu` to the soc on an FPU-bearing part",
                );
            }
        }
    }

    /// Build the register bindings for a device type (its `regs` + fields), to
    /// be put in scope inside that device's own op bodies (leaf MMIO).
    fn reg_infos(&self, device: usize, ty: &str) -> HashMap<String, RegInfo> {
        let mut m = HashMap::new();
        if let Some(def) = self.device_defs.get(ty) {
            if let Some(rs) = &def.sections.regs {
                for r in &rs.regs {
                    let reg_access = map_access(r.access);
                    let fields: HashMap<String, (u64, u8, SirRegAccess)> = r
                        .fields
                        .iter()
                        .map(|f| {
                            let (mask, shift) = bitspec_mask_shift(f.bits);
                            // A field's own `access` overrides the register's.
                            let access = f.access.map(map_access).unwrap_or(reg_access);
                            (f.name.name.clone(), (mask, shift, access))
                        })
                        .collect();
                    // Reading has a side effect if the register is rc, carries a
                    // pop_on_read/side_effect modifier, or any field is rc.
                    let read_side_effect = r.read_side_effect
                        || matches!(reg_access, SirRegAccess::Rc)
                        || fields.values().any(|(_, _, a)| matches!(a, SirRegAccess::Rc));
                    m.insert(
                        r.name.name.clone(),
                        RegInfo { device, offset: r.offset, width: r.width, access: reg_access, fields, read_side_effect },
                    );
                }
            }
        }
        m
    }

    /// The union of all fault codes declared by any op (interface or device),
    /// used to validate fault-injection scripts (§4.4/D14).
    fn declared_fault_codes(&self) -> std::collections::HashSet<String> {
        let mut codes = std::collections::HashSet::new();
        let iface_ops = self.interfaces.values().flat_map(|i| i.ops.iter());
        let dev_ops = self
            .device_defs
            .values()
            .filter_map(|d| d.sections.ops.as_ref())
            .flat_map(|s| s.items.iter().map(|OpsItem::Op(o)| o));
        for op in iface_ops.chain(dev_ops) {
            for c in &op.ret.fault_codes {
                codes.insert(c.name.clone());
            }
        }
        codes
    }

    /// Validate register field bit-specs against the register width (§4.2): a
    /// `bit[n]` or `field[hi:lo]` must fit in the register and have `hi >= lo`.
    fn check_regs(&mut self, d: &DeviceDef) {
        let Some(regs) = d.sections.regs.as_ref() else { return };
        for reg in &regs.regs {
            for f in &reg.fields {
                let bad = match f.bits {
                    BitSpec::Bit(n) => n as u16 >= reg.width as u16,
                    BitSpec::Range(hi, lo) => hi < lo || hi as u16 >= reg.width as u16,
                };
                if bad {
                    self.err(
                        f.span,
                        format!(
                            "field '{}' does not fit in {}-bit register '{}'",
                            f.name.name, reg.width, reg.name.name
                        ),
                    );
                }
            }
        }
    }

    /// §4.1/D18: const-evaluate an interface-need's `where` constraint against the
    /// provider's declared semantic properties (`provides <iface> { … }`), falling
    /// back to the interface's `property` defaults.  A false result — or a
    /// reference to a property the provider neither sets nor the interface
    /// defaults — is a compile error.
    #[allow(clippy::too_many_arguments)]
    fn check_need_properties(
        &mut self,
        inst_name: &str,
        need_name: &str,
        iface: &str,
        provider_ty: &str,
        constraint: &Expr,
        span: Span,
    ) {
        let mut env: HashMap<String, ConstVal> = HashMap::new();
        // 1. interface property defaults.
        if let Some(idef) = self.interfaces.get(iface) {
            for p in &idef.properties {
                if let Some(def) = &p.default {
                    if let Some(v) = const_eval(def, &HashMap::new()) {
                        env.insert(p.name.name.clone(), v);
                    }
                }
            }
        }
        // 2. the provider's declared values for this interface (override defaults).
        if let Some(dev) = self.device_defs.get(provider_ty) {
            for block in &dev.sections.provides {
                if block.iface.name == iface {
                    for (k, vexpr) in &block.values {
                        if let Some(v) = const_eval(vexpr, &HashMap::new()) {
                            env.insert(k.name.clone(), v);
                        }
                    }
                }
            }
        }
        // 3. any property referenced but undeclared → error (typo / unsupported).
        let mut missing: Option<String> = None;
        collect_idents(constraint, &mut |name| {
            if missing.is_none() && !env.contains_key(name) {
                missing = Some(name.to_string());
            }
        });
        if let Some(prop) = missing {
            self.err(span, format!(
                "instance '{inst_name}': `{need_name}` constrains property `{prop}`, which the `{iface}` provider (type '{provider_ty}') does not declare (§4.1/D18)"
            ));
            return;
        }
        // 4. evaluate the constraint.
        match const_eval(constraint, &env) {
            Some(ConstVal::Bool(true)) => {}
            Some(ConstVal::Bool(false)) => self.err(span, format!(
                "instance '{inst_name}': the `{iface}` provider (type '{provider_ty}') does not satisfy the property constraint on `{need_name}` (§4.1/D18)"
            )),
            _ => self.err(span, format!(
                "instance '{inst_name}': the `where` constraint on `{need_name}` did not reduce to a boolean (§4.1/D18)"
            )),
        }
    }

    /// Does `ty` implement interface `iface` (a declared `implements`)?
    fn implements(&self, ty: &str, iface: &str) -> bool {
        self.device_defs
            .get(ty)
            .map(|d| d.implements.iter().any(|i| i.name == iface))
            .unwrap_or(false)
    }

    /// Check a device's `implements` claims against the interface op signatures
    /// (§4.1/D18): every interface op must have a matching device op with the
    /// same shape — name, arity, **`yields`**, and **fallibility** (a mismatch
    /// would break composition / fault propagation at runtime).
    fn check_conformance(&mut self, d: &DeviceDef) {
        // (name, arity, yields, fallible)
        let dev_ops: Vec<(&str, usize, bool, bool)> = d
            .sections
            .ops
            .as_ref()
            .map(|s| {
                s.items
                    .iter()
                    .map(|OpsItem::Op(o)| (o.name.name.as_str(), o.params.len(), o.yields, o.ret.fallible))
                    .collect()
            })
            .unwrap_or_default();
        for iface_name in &d.implements {
            // Snapshot the interface's op shapes so the immutable borrow is
            // released before reporting errors.
            let iface_ops: Vec<(String, usize, bool, bool)> = match self.interfaces.get(&iface_name.name) {
                Some(iface) => iface
                    .ops
                    .iter()
                    .map(|o| (o.name.name.clone(), o.params.len(), o.yields, o.ret.fallible))
                    .collect(),
                None => {
                    self.err(iface_name.span, format!("unknown interface '{}'", iface_name.name));
                    continue;
                }
            };
            for (opname, arity, iy, ifal) in &iface_ops {
                match dev_ops.iter().find(|(n, ..)| n == opname) {
                    None => self.err(
                        d.name.span,
                        format!(
                            "device '{}' implements '{}' but is missing op `{}`",
                            d.name.name, iface_name.name, opname
                        ),
                    ),
                    Some((_, a, dy, dfal)) if a != arity || dy != iy || dfal != ifal => self.err(
                        d.name.span,
                        format!(
                            "device '{}' op `{}` does not match interface '{}' (arity/yields/fallibility differ)",
                            d.name.name, opname, iface_name.name
                        ),
                    ),
                    Some(_) => {}
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

    /// Build a binary-op SIR node.  Arithmetic ops (Add/Sub/Mul + their
    /// wrapping/saturating forms) become a width-checked [`SirExpr::Arith`] using
    /// the current target-type context; everything else stays an untyped `BinOp`.
    fn make_binop(&self, op: BinOp, l: SirExpr, r: SirExpr) -> SirExpr {
        if let Some((aop, mode)) = arith_mode(op) {
            SirExpr::Arith {
                op: aop,
                mode,
                width: self.arith_width,
                signed: self.arith_signed,
                lhs: Box::new(l),
                rhs: Box::new(r),
            }
        } else {
            SirExpr::BinOp(ast_binop_to_sir(op), Box::new(l), Box::new(r))
        }
    }

    /// The `(width_bits, signed)` overflow context implied by an assignment
    /// target — a register field's width, or a cell/local's declared type.
    fn place_ctx(&self, place: &SirPlace, scope: &Scope, vars: &[SirVar]) -> (u8, bool) {
        match place {
            SirPlace::Reg { width, .. } => (*width, false),
            SirPlace::Var(name) => {
                if let Some(v) = vars.iter().find(|v| &v.name == name) {
                    sirtype_ctx(&v.ty)
                } else if let Some(Binding::Local(_, ty)) = scope.lookup(name) {
                    sirtype_ctx(ty)
                } else {
                    (32, false)
                }
            }
        }
    }

    fn lower_stmt(&mut self, stmt: &Stmt, scope: &mut Scope, vars: &[SirVar], out: &mut Vec<SirStmt>) {
        match stmt {
            Stmt::Expr(expr) => self.lower_expr_stmt(expr, scope, vars, out),
            Stmt::Let(l) => {
                // §4.5 time-kind (so `let t = now()` is an instant) + §4.3
                // width/sign checks + FPU gate; an explicit annotation wins but is checked.
                let tk = self.time_kind(&l.init, scope);
                let init_vt = self.value_type(&l.init, scope);
                let ty = match &l.ty {
                    Some(t) => {
                        let ann = resolve_type_expr(t);
                        self.check_assign_time(sirtype_time_kind(&ann), tk, l.init.span);
                        let annvt = sirtype_valtype(&ann);
                        self.check_assign_type(annvt, init_vt, l.init.span);
                        self.check_literal_range(annvt, &l.init, l.init.span);
                        self.float_needs_fpu(&ann, t.span); // §4.3 FPU gate
                        ann
                    }
                    None => time_kind_sirtype(tk).unwrap_or_else(|| {
                        // Scope-aware inference picks up a fixed-point scale (§4.3
                        // P0-3a); otherwise keep the existing literal inference.
                        let scoped = self.expr_sirtype(&l.init, scope);
                        if matches!(scoped, SirType::Fixed { .. }) {
                            scoped
                        } else {
                            infer_type_from_expr(&l.init)
                        }
                    }),
                };
                // §4.3: arithmetic in the initialiser is checked at the binding width.
                let saved = (self.arith_width, self.arith_signed);
                (self.arith_width, self.arith_signed) = sirtype_ctx(&ty);
                let value = self.lower_expr_emit(&l.init, scope, vars, out);
                (self.arith_width, self.arith_signed) = saved;
                scope.insert(&l.name.name, Binding::Local(l.name.name.clone(), ty));
                out.push(SirStmt::Assign { target: SirPlace::Var(l.name.name.clone()), value });
            }
            Stmt::Atomic(block, span) => {
                // Lower the block into one critical section.  The ceiling is
                // filled in by `analyze_cells` (which knows cross-reaction cell
                // sharing); here we only emit the grouping + reject a yield.
                let mut body = Vec::new();
                for s in &block.stmts {
                    self.lower_stmt(s, scope, vars, &mut body);
                }
                if body_yields(&body) || body.iter().any(|s| matches!(s, SirStmt::Await { .. })) {
                    self.err(
                        *span,
                        "an `atomic` block may not contain a yielding op or `await` (§5.5: a critical section cannot span a suspension)",
                    );
                }
                out.push(SirStmt::Critical { ceiling: 0, body });
            }
            Stmt::Poll { cond, within, fault_code, .. } => {
                // Bounded busy-wait, no suspension.  Lower the condition; the
                // metal backend turns `within` into a spin bound, the sim checks
                // it deterministically.
                let _ = self.time_kind(cond, scope); // §4.5 time-type the condition
                let _ = self.value_type(cond, scope); // §4.3 width/sign check
                let cond = self.lower_expr_emit(cond, scope, vars, out);
                out.push(SirStmt::Poll {
                    cond,
                    fault_code: fault_code.name.clone(),
                    within_ns: within.to_ns(),
                });
            }
            Stmt::Await { cond, within, fault_code, .. } => {
                // Suspending bounded wait (§5.2): re-check on a cadence — a small
                // fraction of the budget — until `cond` holds or it elapses.
                let cond = self.lower_expr_emit(cond, scope, vars, out);
                let within_ns = within.to_ns();
                let recheck_ns = (within_ns / 8).max(1_000); // ≥ 1µs cadence
                out.push(SirStmt::Await {
                    cond,
                    fault_code: fault_code.name.clone(),
                    within_ns,
                    recheck_ns,
                });
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
            Stmt::Match { scrutinee, arms, span } => {
                self.lower_match(scrutinee, arms, *span, scope, vars, out);
            }
            Stmt::RegWrite { reg, writes, span } => {
                self.lower_reg_write(reg, writes, *span, scope, vars, out);
            }
        }
    }

    /// Lower `REG{ a = .., b = .. }` to a single `SirStmt::RegWrite` (§4.2 P0-2c).
    fn lower_reg_write(
        &mut self,
        reg: &Ident,
        writes: &[(Ident, Expr)],
        span: Span,
        scope: &mut Scope,
        vars: &[SirVar],
        out: &mut Vec<SirStmt>,
    ) {
        let Some(Binding::Reg(ri)) = scope.lookup(&reg.name) else {
            self.err(reg.span, format!("'{}' is not a register", reg.name));
            return;
        };
        let (device, reg_offset, width, read_side_effect) =
            (ri.device, ri.offset, ri.width, ri.read_side_effect);
        // Resolve each field to (mask, shift, access) up front (immutable borrow
        // of scope) so we can then lower the value exprs (which need &mut self).
        let mut resolved = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut needs_rmw = false;
        for (field, _) in writes {
            match ri.fields.get(&field.name) {
                Some(&(mask, shift, access)) => {
                    if !seen.insert(field.name.clone()) {
                        self.err(field.span, format!("field '{}' written more than once in one `{}{{…}}`", field.name, reg.name));
                    }
                    if matches!(access, SirRegAccess::Ro) {
                        self.err(field.span, format!("cannot write read-only field '{}.{}' (§4.2)", reg.name, field.name));
                    }
                    if !matches!(access, SirRegAccess::W1c | SirRegAccess::Wo) {
                        needs_rmw = true;
                    }
                    resolved.push((mask, shift, access));
                }
                None => {
                    self.err(field.span, format!("register '{}' has no field '{}'", reg.name, field.name));
                    resolved.push((0, 0, SirRegAccess::Rw));
                }
            }
        }
        // A combined write that still needs a read (any non-w1c/wo field) must
        // not RMW a register whose read has a side effect (§4.2/D04, P0-2b).
        if needs_rmw && read_side_effect {
            self.err(span, format!(
                "register '{}' has a read-side-effect (rc/pop_on_read); a multi-field write that needs a read-modify-write would disturb it — write only w1c/wo fields, or the whole register, or `.raw` (§4.2)",
                reg.name
            ));
        }
        // Lower the value exprs at the register width.
        let saved = (self.arith_width, self.arith_signed);
        (self.arith_width, self.arith_signed) = (width, false);
        let sir_writes = writes
            .iter()
            .zip(resolved)
            .map(|((_, value), (mask, shift, access))| {
                let v = self.lower_expr_emit(value, scope, vars, out);
                (mask, shift, access, v)
            })
            .collect();
        (self.arith_width, self.arith_signed) = saved;
        out.push(SirStmt::RegWrite { device, reg_offset, width, writes: sir_writes });
    }

    /// Lower `match <scrut> { <pat> => <body>, … }` (§4.4/D14) to a guarded
    /// if-chain, enforcing **totality**: exactly one `_` wildcard arm is required
    /// (no case may be silently unhandled).  Literal arms are mutually exclusive,
    /// so each is an independent `if __m == lit { … }`; the wildcard runs iff none
    /// matched (`__matched == 0`).
    fn lower_match(
        &mut self,
        scrutinee: &Expr,
        arms: &[MatchArm],
        span: Span,
        scope: &mut Scope,
        vars: &[SirVar],
        out: &mut Vec<SirStmt>,
    ) {
        let wild_count = arms.iter().filter(|a| matches!(a.pattern, MatchPat::Wild)).count();
        if wild_count == 0 {
            self.err(span, "a `match` must be exhaustive — add a `_` arm (§4.4/D14)");
        } else if wild_count > 1 {
            self.err(span, "a `match` has more than one `_` arm");
        }

        let m = self.fresh("match");
        let matched = self.fresh("matched");
        let scrut_val = self.lower_expr_emit(scrutinee, scope, vars, out);
        out.push(SirStmt::Assign { target: SirPlace::Var(m.clone()), value: scrut_val });
        out.push(SirStmt::Assign { target: SirPlace::Var(matched.clone()), value: SirExpr::U64(0) });

        let mut wildcard: Option<&MatchArm> = None;
        let mut seen_lits: Vec<u64> = Vec::new();
        for arm in arms {
            match &arm.pattern {
                MatchPat::Wild => wildcard = Some(arm),
                MatchPat::Lit(lit) => {
                    if let Some(k) = lit_const(lit) {
                        if seen_lits.contains(&k) {
                            self.err(arm.span, format!("duplicate match arm for `{k}`"));
                        }
                        seen_lits.push(k);
                    } else {
                        self.err(arm.span, "match patterns must be integer or boolean literals");
                    }
                    let cond = SirExpr::BinOp(
                        SirBinOp::EqEq,
                        Box::new(SirExpr::Load(m.clone())),
                        Box::new(self.lower_expr(lit, scope)),
                    );
                    let mut then = self.lower_block(&arm.body, scope, vars);
                    then.push(SirStmt::Assign {
                        target: SirPlace::Var(matched.clone()),
                        value: SirExpr::U64(1),
                    });
                    out.push(SirStmt::If { cond, then });
                }
            }
        }
        if let Some(arm) = wildcard {
            let then = self.lower_block(&arm.body, scope, vars);
            let cond = SirExpr::BinOp(
                SirBinOp::EqEq,
                Box::new(SirExpr::Load(matched.clone())),
                Box::new(SirExpr::U64(0)),
            );
            out.push(SirStmt::If { cond, then });
        }
    }

    /// Recursively compute an expression's time-logical kind (§4.5), emitting an
    /// error on an illegal `instant`/`duration` combination.  Called once per
    /// statement-root expression, so each node is visited exactly once.
    fn time_kind(&mut self, expr: &Expr, scope: &Scope) -> TimeKind {
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
    fn place_time_kind(&self, lhs: &Expr, scope: &Scope) -> TimeKind {
        match &lhs.kind {
            ExprKind::Ident(id) => match scope.lookup(&id.name) {
                Some(Binding::Local(_, t)) | Some(Binding::Cell(_, t)) => sirtype_time_kind(t),
                _ => TimeKind::Scalar,
            },
            _ => TimeKind::Scalar,
        }
    }

    /// The result kind of `l <op> r`, enforcing the §4.5 arithmetic rules.
    fn combine_time(&mut self, op: BinOp, l: TimeKind, r: TimeKind, span: Span) -> TimeKind {
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
    fn check_assign_time(&mut self, target: TimeKind, val: TimeKind, span: Span) {
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
    fn expr_sirtype(&self, expr: &Expr, scope: &Scope) -> SirType {
        match &expr.kind {
            ExprKind::Ident(id) => match scope.lookup(&id.name) {
                Some(Binding::Local(_, t)) | Some(Binding::Cell(_, t)) => t.clone(),
                _ => SirType::U32,
            },
            ExprKind::Cast(_, ty) => resolve_type_expr(ty),
            ExprKind::Try(inner) => self.expr_sirtype(inner, scope),
            ExprKind::BinOp { lhs, .. } => self.expr_sirtype(lhs, scope),
            _ => SirType::U32,
        }
    }

    fn value_type(&mut self, expr: &Expr, scope: &Scope) -> ValType {
        match &expr.kind {
            ExprKind::IntLit(_) => ValType::Literal,
            ExprKind::Ident(id) => match scope.lookup(&id.name) {
                Some(Binding::Local(_, t)) | Some(Binding::Cell(_, t)) => sirtype_valtype(t),
                _ => ValType::Flexible,
            },
            ExprKind::Cast(inner, ty) => {
                let _ = self.value_type(inner, scope); // check inside the cast
                sirtype_valtype(&resolve_type_expr(ty))
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

    fn place_valtype(&self, lhs: &Expr, scope: &Scope) -> ValType {
        match &lhs.kind {
            ExprKind::Ident(id) => match scope.lookup(&id.name) {
                Some(Binding::Local(_, t)) | Some(Binding::Cell(_, t)) => sirtype_valtype(t),
                _ => ValType::Flexible,
            },
            _ => ValType::Flexible,
        }
    }

    /// Reject `signed <op> unsigned` between two declared-typed operands (§4.3).
    fn check_mixed_sign(&mut self, l: ValType, r: ValType, span: Span) {
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
    fn check_assign_type(&mut self, target: ValType, v: ValType, span: Span) {
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
    fn check_literal_range(&mut self, target: ValType, rhs: &Expr, span: Span) {
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

    fn lower_expr_stmt(&mut self, expr: &Expr, scope: &mut Scope, vars: &[SirVar], out: &mut Vec<SirStmt>) {
        // §4.5 time-type + §4.3 width/sign checks over the whole statement (one pass each).
        let _ = self.time_kind(expr, scope);
        let _ = self.value_type(expr, scope);
        match &expr.kind {
            ExprKind::Assign(lhs, rhs) => {
                if let Some(place) = self.expr_to_place(lhs, scope) {
                    let (w, s) = self.place_ctx(&place, scope, vars);
                    let saved = (self.arith_width, self.arith_signed);
                    (self.arith_width, self.arith_signed) = (w, s);
                    let value = self.lower_expr_emit(rhs, scope, vars, out);
                    (self.arith_width, self.arith_signed) = saved;
                    out.push(SirStmt::Assign { target: place, value });
                }
            }
            ExprKind::CompoundAssign(op, lhs, rhs) => {
                if let Some(place) = self.expr_to_place(lhs, scope) {
                    let (w, s) = self.place_ctx(&place, scope, vars);
                    let saved = (self.arith_width, self.arith_signed);
                    (self.arith_width, self.arith_signed) = (w, s);
                    let lhs_val = self.lower_expr(lhs, scope);
                    let rhs_val = self.lower_expr_emit(rhs, scope, vars, out);
                    let combined = self.make_binop(*op, lhs_val, rhs_val);
                    (self.arith_width, self.arith_signed) = saved;
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
                            Some(Binding::Cell(cell, ty)) if matches!(ty, SirType::Ring { .. }) => {
                                self.lower_ring_stmt(&cell, &method.name, args, expr.span, scope, vars, out);
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
                        match scope.lookup(root).cloned() {
                            Some(Binding::Device(inst)) => {
                                return self.lower_op_call(&inst, &method.name, args, false, expr.span, scope, vars, out);
                            }
                            Some(Binding::Cell(cell, ty)) if matches!(ty, SirType::Ring { .. }) => {
                                return self.lower_ring_value(&cell, &method.name, args, expr.span, scope, vars, out);
                            }
                            _ => {}
                        }
                    }
                }
                self.lower_expr(expr, scope) // pin reads etc.
            }
            ExprKind::BinOp { op, lhs, rhs } => {
                let l = self.lower_expr_emit(lhs, scope, vars, out);
                let r = self.lower_expr_emit(rhs, scope, vars, out);
                self.make_binop(*op, l, r)
            }
            ExprKind::Not(inner) => SirExpr::Not(Box::new(self.lower_expr_emit(inner, scope, vars, out))),
            ExprKind::Cast(inner, ty) => {
                let from = self.expr_sirtype(inner, scope);
                let v = self.lower_expr_emit(inner, scope, vars, out);
                lower_cast(v, &from, &resolve_type_expr(ty))
            }
            _ => self.lower_expr(expr, scope),
        }
    }

    /// Lower a ring op used as a **statement** (§5.3): `r.push(v)` enqueues;
    /// `r.pop()` discards the dequeued value.
    fn lower_ring_stmt(&mut self, cell: &str, method: &str, args: &[Expr], span: Span, scope: &mut Scope, vars: &[SirVar], out: &mut Vec<SirStmt>) {
        match method {
            "push" => {
                if args.len() != 1 {
                    self.err(span, "ring `push` takes exactly one argument");
                    return;
                }
                let value = self.lower_expr_emit(&args[0], scope, vars, out);
                out.push(SirStmt::RingPush { ring: cell.to_string(), value });
            }
            "pop" => {
                let dst = self.fresh("ringpop");
                out.push(SirStmt::RingPop { ring: cell.to_string(), dst });
            }
            other => self.err(span, format!("ring has no op '{other}' (expected push/pop/len/is_empty/is_full)")),
        }
    }

    /// Lower a ring op used as a **value** (§5.3): `r.pop()` dequeues into a temp;
    /// `r.len()/is_empty()/is_full()` are pure reads.
    fn lower_ring_value(&mut self, cell: &str, method: &str, _args: &[Expr], span: Span, _scope: &mut Scope, _vars: &[SirVar], out: &mut Vec<SirStmt>) -> SirExpr {
        match method {
            "pop" => {
                let dst = self.fresh("ringpop");
                out.push(SirStmt::RingPop { ring: cell.to_string(), dst: dst.clone() });
                SirExpr::Load(dst)
            }
            "len" => SirExpr::RingLen(cell.to_string()),
            "is_empty" => SirExpr::RingEmpty(cell.to_string()),
            "is_full" => SirExpr::RingFull(cell.to_string()),
            "push" => {
                self.err(span, "ring `push` returns nothing — use it as a statement");
                SirExpr::U64(0)
            }
            other => {
                self.err(span, format!("ring has no op '{other}' (expected push/pop/len/is_empty/is_full)"));
                SirExpr::U64(0)
            }
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
        // §4.1/D07 — static typestate check: a `when S` op is callable only when
        // the device is provably in state S (a dominating `become` in this
        // reaction's straight-line flow).  Otherwise it is a compile error.
        if let Some(required) = &op.when {
            let current = self.current_device_state(inst.device, &inst.ty);
            if current.as_deref() != Some(required.name.as_str()) {
                self.err(
                    span,
                    format!(
                        "op '{}' requires device type '{}' to be in state '{}', but it is in state '{}' here — add a `become {}` before the call (§4.1/D07)",
                        op_name,
                        inst.ty,
                        required.name,
                        current.as_deref().unwrap_or("<unknown>"),
                        required.name,
                    ),
                );
            }
        }
        // Apply this op's typestate transition, if it ends by `become`-ing a state.
        if let Some(target) = op_become_target(&op) {
            self.device_states.insert(inst.device, target);
        }
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

        // §5.3/SIL-005 — recursion is banned: a re-entry of an op already on the
        // active inline path is a compile error (and would otherwise hang the
        // inliner with no fixed stack bound).
        if self.inlining.iter().any(|(d, o)| *d == inst.device && o == op_name) {
            self.err(
                span,
                format!("op '{op_name}' is recursive — recursion is banned so the worst-case stack stays bounded (§5.3/SIL-005)"),
            );
            return SirExpr::U64(0);
        }
        self.inlining.push((inst.device, op_name.to_string()));

        // Otherwise inline the op body with params + needs substituted.
        let mut inner = Scope::new();
        for (p, v) in op.params.iter().zip(arg_vals) {
            let tmp = self.fresh("arg");
            out.push(SirStmt::Assign { target: SirPlace::Var(tmp.clone()), value: v });
            inner.insert(&p.name.name, Binding::Local(tmp, resolve_type_expr(&p.ty)));
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
        // The device's own registers are in scope inside its op bodies (leaf MMIO).
        for (name, ri) in self.reg_infos(inst.device, &inst.ty) {
            inner.insert(&name, Binding::Reg(ri));
        }
        let result = self.fresh("r");
        out.push(SirStmt::Assign { target: SirPlace::Var(result.clone()), value: SirExpr::U64(0) });
        self.op_result.push(result.clone());
        for stmt in &op.body.stmts {
            self.lower_stmt(stmt, &mut inner, vars, out);
        }
        self.op_result.pop();
        self.inlining.pop();
        SirExpr::Load(result)
    }

    fn fresh(&mut self, prefix: &str) -> String {
        let n = self.tmp_counter;
        self.tmp_counter += 1;
        format!("__{}{}", prefix, n)
    }

    /// Lower a device instance's `safe` op (§5.6) into a statement sequence, with
    /// the device's registers + needs in scope.  Returns `None` if the device
    /// declares no `safe_state` / `safe` op.  A safe op must be **non-yielding**
    /// (it must not depend on the scheduler it may be escaping).
    fn lower_safe_seq(&mut self, device: usize, ty: &str) -> Option<SafeSeq> {
        let def = self.device_defs.get(ty)?.clone();
        let state = def.sections.safe_state.clone();
        let op = self.find_op(ty, "safe").cloned();
        // `safe_state` and a `safe` op go together (§5.6); one without the other
        // is a mistake, not silent no-op.
        match (&state, &op) {
            (None, None) => return None, // device has no safe behaviour
            (Some(_), None) => {
                self.err(def.name.span, format!("device '{}' declares `safe_state` but has no `safe` op (§5.6)", ty));
                return None;
            }
            (None, Some(o)) => {
                self.err(o.span, format!("device '{}' has a `safe` op but no `safe_state` (§5.6)", ty));
                return None;
            }
            (Some(_), Some(_)) => {}
        }
        let op = op.unwrap();
        if op.yields {
            self.err(op.span, "a `safe` op may not yield (§5.6: it must not depend on the scheduler)");
            return None;
        }
        let mut inner = Scope::new();
        // needs before regs — same order as `lower_op_call`, so name resolution
        // is consistent between safe and non-safe ops.
        if let Some(needs) = self.instance_needs.get(&device).cloned() {
            for (name, val) in needs {
                if let NeedVal::Device(r) = val {
                    inner.insert(&name, Binding::Device(r));
                }
            }
        }
        for (name, ri) in self.reg_infos(device, ty) {
            inner.insert(&name, Binding::Reg(ri));
        }
        let mut body = Vec::new();
        for stmt in &op.body.stmts {
            self.lower_stmt(stmt, &mut inner, &[], &mut body);
        }
        // §5.6: a safe op must not yield even *indirectly* (via a yielding sub-op
        // that lowered to a bus transaction).
        if body_yields(&body) {
            self.err(op.span, "a `safe` op may not yield, even indirectly via a bus transaction (§5.6)");
            return None;
        }
        Some(SafeSeq { device, state: state.unwrap().name, body })
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
            ExprKind::IntLit(n) | ExprKind::DurationLit(n) => SirExpr::U64(*n),
            ExprKind::StringLit(s) => SirExpr::Bytes(s.as_bytes().to_vec()),
            ExprKind::Ident(ident) => match scope.lookup(&ident.name) {
                Some(Binding::Local(name, _)) | Some(Binding::Cell(name, _)) => SirExpr::Load(name.clone()),
                // Whole-register read inside an op body.
                Some(Binding::Reg(ri)) => {
                    if matches!(ri.access, SirRegAccess::Wo) {
                        self.err(ident.span, format!("cannot read write-only register '{}' (§4.2)", ident.name));
                    }
                    reg_load(ri, full_mask(ri.width), 0, ri.access)
                }
                _ => {
                    self.err(ident.span, format!("undefined variable '{}'", ident.name));
                    SirExpr::Load(ident.name.clone())
                }
            },
            // `REG.field` read inside an op body.
            ExprKind::Field(base, field) => {
                if let ExprKind::Ident(reg) = &base.kind {
                    if let Some(Binding::Reg(ri)) = scope.lookup(&reg.name) {
                        if let Some(&(mask, shift, access)) = ri.fields.get(&field.name) {
                            if matches!(access, SirRegAccess::Wo) {
                                self.err(field.span, format!("cannot read write-only field '{}.{}' (§4.2)", reg.name, field.name));
                            }
                            return reg_load(ri, mask, shift, access);
                        }
                        self.err(field.span, format!("register '{}' has no field '{}'", reg.name, field.name));
                        return SirExpr::U64(0);
                    }
                }
                self.err(expr.span, "field expression not supported as a value here");
                SirExpr::U64(0)
            }
            ExprKind::Not(inner) => SirExpr::Not(Box::new(self.lower_expr(inner, scope))),
            ExprKind::Cast(inner, ty) => {
                let from = self.expr_sirtype(inner, scope);
                let v = self.lower_expr(inner, scope);
                lower_cast(v, &from, &resolve_type_expr(ty))
            }
            ExprKind::BinOp { op, lhs, rhs } => {
                let l = self.lower_expr(lhs, scope);
                let r = self.lower_expr(rhs, scope);
                self.make_binop(*op, l, r)
            }
            ExprKind::Assign(_lhs, rhs) => self.lower_expr(rhs, scope),
            ExprKind::CompoundAssign(_, _, rhs) => self.lower_expr(rhs, scope),
            ExprKind::Try(inner) => self.lower_expr(inner, scope), // fault `?` — Phase 1
            ExprKind::Call { callee, args, named: _ } => {
                // `now()` — the current time as an `instant` (§4.5).
                if is_now_call(callee) {
                    if !args.is_empty() {
                        self.err(expr.span, "now() takes no arguments");
                    }
                    return SirExpr::Now;
                }
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
        }
    }

    fn expr_to_place(&mut self, expr: &Expr, scope: &Scope) -> Option<SirPlace> {
        match &expr.kind {
            ExprKind::Ident(ident) => match scope.lookup(&ident.name) {
                Some(Binding::Local(name, _)) | Some(Binding::Cell(name, _)) => {
                    Some(SirPlace::Var(name.clone()))
                }
                // `REG = expr` — a whole-register write inside an op body.
                Some(Binding::Reg(ri)) => {
                    if matches!(ri.access, SirRegAccess::Ro) {
                        self.err(ident.span, format!("cannot write read-only register '{}' (§4.2)", ident.name));
                    }
                    Some(reg_place(ri, full_mask(ri.width), 0, ri.access))
                }
                _ => {
                    self.err(ident.span, format!("'{}' is not assignable", ident.name));
                    None
                }
            },
            // `REG.field = expr` — a register field write.
            ExprKind::Field(base, field) => {
                if let ExprKind::Ident(reg) = &base.kind {
                    if let Some(Binding::Reg(ri)) = scope.lookup(&reg.name) {
                        if let Some(&(mask, shift, access)) = ri.fields.get(&field.name) {
                            if matches!(access, SirRegAccess::Ro) {
                                self.err(field.span, format!("cannot write read-only field '{}.{}' (§4.2)", reg.name, field.name));
                            }
                            // A field write that lowers to a read-modify-write
                            // (rw/rc fields) must not RMW a register whose READ
                            // has a side effect — the implicit read would disturb
                            // it (§4.2/D04, P0-2b).  w1c/wo fields are single
                            // writes (no read), so they are fine.
                            if ri.read_side_effect && matches!(access, SirRegAccess::Rw | SirRegAccess::Rc) {
                                self.err(field.span, format!(
                                    "cannot write field '{}.{}' with a read-modify-write: register '{}' has a read-side-effect (rc/pop_on_read), so the implicit read would disturb it — write the whole register, use a w1c field, or `.raw` (§4.2)",
                                    reg.name, field.name, reg.name
                                ));
                            }
                            return Some(reg_place(ri, mask, shift, access));
                        }
                        self.err(field.span, format!("register '{}' has no field '{}'", reg.name, field.name));
                        return None;
                    }
                }
                self.err(expr.span, "expected an assignable place");
                None
            }
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
        // An explicit `atomic { }` block is already a top-level `Critical` at this
        // point (lowering emits it with a placeholder ceiling); set its ceiling to
        // protect every cell it touches — the max priority among reactions that
        // touch any of them — rather than wrapping it again.
        for r in reactions.iter_mut() {
            let rprio = r.priority;
            let body = std::mem::take(&mut r.body);
            r.body = body
                .into_iter()
                .map(|stmt| match stmt {
                    SirStmt::Critical { body, .. } => {
                        let c = cell_names
                            .iter()
                            .filter(|cell| stmts_touch_cell(&body, cell))
                            .filter_map(|cell| ceiling.get(cell).copied())
                            .max()
                            .unwrap_or(rprio);
                        SirStmt::Critical { ceiling: c, body }
                    }
                    stmt => {
                        let ceil = shared
                            .iter()
                            .filter(|(cell, _)| stmt_touches_cell(&stmt, cell))
                            .map(|(_, &c)| c)
                            .max();
                        match ceil {
                            Some(c) => SirStmt::Critical { ceiling: c, body: vec![stmt] },
                            None => stmt,
                        }
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
        let declared = self.declared_fault_codes();
        for (code, times) in &sim.bus_faults {
            // Fault injection is keyed to declared codes (§4.4/D14): reject typos.
            if !declared.contains(&code.name) {
                self.err(code.span, format!("injected bus fault code '{}' is not declared by any op's `or fault{{...}}`", code.name));
            }
            for _ in 0..*times {
                self.bus_fault_queue.push(code.name.clone());
            }
        }
        self.bus_hangs += sim.bus_hangs;
        if let Some(d) = sim.run_until {
            self.run_until_ns = Some(d.to_ns());
        }
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Mask + shift for a register field bit-spec.  Shift-safe: out-of-range or
/// inverted bit-specs (validated/diagnosed in `check_regs`) degrade to a 0 mask
/// rather than panicking, since bit indices are user input.
fn bitspec_mask_shift(b: BitSpec) -> (u64, u8) {
    match b {
        BitSpec::Bit(n) => (1u64.checked_shl(n as u32).unwrap_or(0), n),
        BitSpec::Range(hi, lo) if hi >= lo => {
            let width = (hi - lo + 1) as u32;
            let span = if width >= 64 { u64::MAX } else { (1u64 << width) - 1 };
            (span.checked_shl(lo as u32).unwrap_or(0), lo)
        }
        BitSpec::Range(_, lo) => (0, lo),
    }
}

/// Full-width mask for a register of the given bit width.
fn full_mask(width: u8) -> u64 {
    if width >= 64 { u64::MAX } else { (1u64 << width) - 1 }
}

fn reg_place(ri: &RegInfo, mask: u64, shift: u8, access: SirRegAccess) -> SirPlace {
    SirPlace::Reg {
        device: ri.device,
        reg_offset: ri.offset,
        width: ri.width,
        field_mask: mask,
        field_shift: shift,
        access,
    }
}

fn reg_load(ri: &RegInfo, mask: u64, shift: u8, access: SirRegAccess) -> SirExpr {
    SirExpr::RegLoad {
        device: ri.device,
        reg_offset: ri.offset,
        width: ri.width,
        field_mask: mask,
        field_shift: shift,
        access,
    }
}

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
        // A register multi-field write touches a cell only via its value exprs
        // (the target is MMIO, not a cell).
        SirStmt::RegWrite { writes, .. } => writes.iter().any(|(_, _, _, v)| expr_touches_cell(v, cell)),
        SirStmt::If { cond, then } => expr_touches_cell(cond, cell) || stmts_touch_cell(then, cell),
        SirStmt::Poll { cond, .. } => expr_touches_cell(cond, cell),
        // An `await` *polls* its condition (it must observe a cell another reaction
        // changes during the suspension), so it is deliberately NOT a synchronized
        // cell access — it is never wrapped in the §5.5 auto-critical.
        SirStmt::Await { .. } => false,
        SirStmt::Critical { body, .. } => stmts_touch_cell(body, cell),
        SirStmt::Exit(e) => expr_touches_cell(e, cell),
        SirStmt::Intrinsic(intr) => match intr {
            SirIntrinsic::HostIoPrint(e) => expr_touches_cell(e, cell),
            _ => false,
        },
        SirStmt::DeviceOp { args, .. } => args.iter().any(|a| expr_touches_cell(a, cell)),
        SirStmt::BusXfer { args, .. } => args.iter().any(|a| expr_touches_cell(a, cell)),
        SirStmt::RingPush { ring, value } => ring == cell || expr_touches_cell(value, cell),
        SirStmt::RingPop { ring, .. } => ring == cell,
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

/// True if any `if` in the body (transitively) contains a yielding bus transfer.
fn if_contains_yield(stmts: &[SirStmt]) -> bool {
    stmts.iter().any(|s| match s {
        SirStmt::If { then, .. } => body_yields(then) || if_contains_yield(then),
        SirStmt::Critical { body, .. } => if_contains_yield(body),
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
        SirExpr::Cast { inner, .. } | SirExpr::FixedCast { inner, .. } => expr_touches_cell(inner, cell),
        SirExpr::BinOp(_, l, r) => expr_touches_cell(l, cell) || expr_touches_cell(r, cell),
        SirExpr::Arith { lhs, rhs, .. } => expr_touches_cell(lhs, cell) || expr_touches_cell(rhs, cell),
        SirExpr::RingLen(r) | SirExpr::RingEmpty(r) | SirExpr::RingFull(r) => r == cell,
        _ => false,
    }
}

/// Extract the root identifier name from an expression like `foo` or `foo.bar`.
/// The typestate an op transitions the device to, if its body ends by `become`-ing
/// a state (the last top-level `become` wins).  `None` if it makes no transition.
fn op_become_target(op: &OpDecl) -> Option<String> {
    op.body.stmts.iter().rev().find_map(|s| match s {
        Stmt::Become(state, _) => Some(state.name.clone()),
        _ => None,
    })
}

fn expr_root_ident(expr: &Expr) -> Option<&str> {
    match &expr.kind {
        ExprKind::Ident(ident) => Some(&ident.name),
        ExprKind::Field(inner, _) => expr_root_ident(inner),
        _ => None,
    }
}

/// `(width_bits, signed)` for an integer SIR type; `None` for non-integers.
fn sirtype_width_sign(t: &SirType) -> Option<(u8, bool)> {
    match t {
        SirType::U8 => Some((8, false)),
        SirType::U16 => Some((16, false)),
        SirType::U32 => Some((32, false)),
        SirType::U64 => Some((64, false)),
        SirType::S8 => Some((8, true)),
        SirType::S16 => Some((16, true)),
        SirType::S32 => Some((32, true)),
        SirType::S64 => Some((64, true)),
        _ => None, // bool / bytes — not width-checked
    }
}

/// The width/sign category of a value (§4.3).  `Int` is a declared-typed value;
/// `Literal` is an integer literal (adopts any width/sign that fits, so it never
/// triggers narrowing/sign errors); `Flexible` is an unknown (a device-op
/// result, register read, bool, …) — also exempt, to avoid false positives.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum ValType {
    Int { width: u8, signed: bool },
    /// `fixed<I,F>` (§4.3, P0-3a): distinct from `Int` so mixing fixed with an
    /// integer — or a different fixed scale — is a type error (needs a cast).
    Fixed { int_bits: u8, frac_bits: u8, signed: bool },
    Literal,
    Flexible,
}

fn sirtype_valtype(t: &SirType) -> ValType {
    match t {
        SirType::Fixed { int_bits, frac_bits, signed } => {
            ValType::Fixed { int_bits: *int_bits, frac_bits: *frac_bits, signed: *signed }
        }
        _ => match sirtype_width_sign(t) {
            Some((width, signed)) => ValType::Int { width, signed },
            None => ValType::Flexible,
        },
    }
}

/// `(frac_bits)` of a fixed type, else 0 — the scale used by `lower_cast`.
fn fixed_frac_bits(t: &SirType) -> u8 {
    match t {
        SirType::Fixed { frac_bits, .. } => *frac_bits,
        _ => 0,
    }
}

fn is_comparison(op: BinOp) -> bool {
    matches!(op, BinOp::EqEq | BinOp::NotEq | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge)
}

/// The width/sign of an arithmetic result (sign already mixed-checked): the
/// wider of two declared types, or the typed operand when the other is a literal.
fn arith_result(l: ValType, r: ValType) -> ValType {
    use ValType::{Fixed, Flexible, Int, Literal};
    match (l, r) {
        (Int { width: wl, signed }, Int { width: wr, .. }) => Int { width: wl.max(wr), signed },
        (Int { width, signed }, Literal) | (Literal, Int { width, signed }) => Int { width, signed },
        // Fixed add/sub: both operands must share the same scale (checked in
        // check_mixed_sign); a literal adopts the fixed scale.
        (f @ Fixed { .. }, Fixed { .. }) => f,
        (f @ Fixed { .. }, Literal) | (Literal, f @ Fixed { .. }) => f,
        (Literal, Literal) => Literal,
        _ => Flexible,
    }
}

/// Lower a `<value> as <ty>` cast to a SIR `Cast` (numeric target) or a
/// pass-through (non-numeric target — the type checker already flagged it).
fn lower_cast(value: SirExpr, from: &SirType, to: &SirType) -> SirExpr {
    // A cast touching fixed-point rescales by shifting the binary point
    // (§4.3, P0-3a): int→fixed shifts left F, fixed→int shifts right F, and
    // fixed→fixed shifts by the frac-bit difference.
    if matches!(from, SirType::Fixed { .. }) || matches!(to, SirType::Fixed { .. }) {
        let shift = (fixed_frac_bits(to) as i16 - fixed_frac_bits(from) as i16).clamp(-64, 64) as i8;
        let (to_width, signed) = sirtype_ctx(to);
        return SirExpr::FixedCast { inner: Box::new(value), shift, to_width, signed };
    }
    match sirtype_width_sign(to) {
        Some((to_width, signed)) => SirExpr::Cast { inner: Box::new(value), to_width, signed },
        None => value,
    }
}

/// The constant value of an integer or boolean literal pattern, if it is one.
fn lit_const(e: &Expr) -> Option<u64> {
    match &e.kind {
        ExprKind::IntLit(n) => Some(*n),
        ExprKind::BoolLit(b) => Some(*b as u64),
        _ => None,
    }
}

/// Minimal type inference for initialiser expressions.
fn infer_type_from_expr(expr: &Expr) -> SirType {
    match &expr.kind {
        ExprKind::BoolLit(_) => SirType::Bool,
        ExprKind::IntLit(_) => SirType::U32,
        ExprKind::DurationLit(_) => SirType::Duration,
        ExprKind::StringLit(_) => SirType::Bytes,
        // `<e> as <T>` initialises a binding at the cast's target type.
        ExprKind::Cast(_, ty) => resolve_type_expr(ty),
        _ => SirType::U32,
    }
}

/// The constant integer value of an integer or duration literal (ns), if either.
fn lit_int(kind: &ExprKind) -> Option<u64> {
    match kind {
        ExprKind::IntLit(n) | ExprKind::DurationLit(n) => Some(*n),
        _ => None,
    }
}

/// `true` if `callee` is the bare `now` identifier — `now()` reads the clock.
fn is_now_call(callee: &Expr) -> bool {
    matches!(&callee.kind, ExprKind::Ident(id) if id.name == "now")
}

/// The time-logical category of a value (§4.5).  `Instant` and `Duration` are
/// both `u64` ns at runtime but obey distinct arithmetic rules; everything else
/// is `Scalar` (a plain integer / bool).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TimeKind {
    Instant,
    Duration,
    Scalar,
}

fn sirtype_time_kind(t: &SirType) -> TimeKind {
    match t {
        SirType::Instant => TimeKind::Instant,
        SirType::Duration => TimeKind::Duration,
        _ => TimeKind::Scalar,
    }
}

fn time_kind_sirtype(k: TimeKind) -> Option<SirType> {
    match k {
        TimeKind::Instant => Some(SirType::Instant),
        TimeKind::Duration => Some(SirType::Duration),
        TimeKind::Scalar => None,
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
            "instant" => SirType::Instant,
            "duration" => SirType::Duration,
            "float" | "f32" => SirType::F32,
            "f64" | "double" => SirType::F64,
            _ => SirType::U32,
        },
        // `fixed<I, F>` — 2's-complement binary fixed-point (§4.3, P0-3a).
        TypeKind::Fixed(int_bits, frac_bits) => SirType::Fixed {
            int_bits: (*int_bits).min(64) as u8,
            frac_bits: (*frac_bits).min(64) as u8,
            signed: true,
        },
        TypeKind::Unit => SirType::U8,
        TypeKind::Bytes => SirType::Bytes,
        // `ring<T, N>` — element size from `T`, capacity from the const `N`.
        TypeKind::Ring(elem, n) => {
            let elem_bytes = resolve_type_expr(elem).byte_size().clamp(1, 8) as u8;
            let cap = match const_eval(n, &HashMap::new()) {
                Some(ConstVal::Int(c)) => c as u32,
                _ => 0,
            };
            SirType::Ring { elem_bytes, cap }
        }
        _ => SirType::U32,
    }
}

/// Map an AST arithmetic operator to its SIR op + overflow disposition (§4.3).
/// `None` for non-arithmetic operators (comparisons / logic / div / rem), which
/// stay on the untyped `SirBinOp` path.
fn arith_mode(op: BinOp) -> Option<(SirArithOp, OverflowMode)> {
    use OverflowMode::{Saturate, Trap, Wrap};
    use SirArithOp::{Add, Mul, Sub};
    Some(match op {
        BinOp::Add => (Add, Trap),
        BinOp::Sub => (Sub, Trap),
        BinOp::Mul => (Mul, Trap),
        BinOp::AddWrap => (Add, Wrap),
        BinOp::AddSat => (Add, Saturate),
        BinOp::SubWrap => (Sub, Wrap),
        BinOp::SubSat => (Sub, Saturate),
        BinOp::MulWrap => (Mul, Wrap),
        BinOp::MulSat => (Mul, Saturate),
        _ => return None,
    })
}

/// `(width_bits, signed)` for a SIR scalar type — drives overflow checks.
fn sirtype_ctx(ty: &SirType) -> (u8, bool) {
    match ty {
        SirType::U8 => (8, false),
        SirType::U16 => (16, false),
        SirType::U32 => (32, false),
        SirType::U64 => (64, false),
        SirType::S8 => (8, true),
        SirType::S16 => (16, true),
        SirType::S32 => (32, true),
        SirType::S64 => (64, true),
        SirType::Bool => (8, false),
        SirType::Bytes => (32, false),
        // instant/duration are u64 ns at runtime (§4.5).
        SirType::Instant | SirType::Duration => (64, false),
        // a ring is not a scalar — never an arithmetic operand.
        SirType::Ring { .. } => (32, false),
        // floats are not integer-overflow-checked (§4.3); width is unused here.
        SirType::F32 | SirType::F64 => (32, false),
        // fixed-point is integer math at its storage width (§4.3, P0-3a), so
        // add/sub are overflow-checked exactly like the backing integer.
        SirType::Fixed { int_bits, frac_bits, signed } => {
            (SirType::fixed_storage_bits(*int_bits, *frac_bits) as u8, *signed)
        }
    }
}

fn ast_binop_to_sir(op: BinOp) -> SirBinOp {
    match op {
        BinOp::Add | BinOp::AddWrap | BinOp::AddSat => SirBinOp::Add,
        BinOp::Sub | BinOp::SubWrap | BinOp::SubSat => SirBinOp::Sub,
        BinOp::Mul | BinOp::MulWrap | BinOp::MulSat => SirBinOp::Mul,
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

// ─── Compile-time constant evaluation (§3.2/§4.1 `where`-constraints) ──────────

/// A constant value produced by [`const_eval`]. Durations are already lowered to
/// `IntLit` nanoseconds by the parser, so every numeric constant is an `Int`.
#[derive(Clone, Copy)]
enum ConstVal {
    Int(u64),
    Bool(bool),
}

/// Evaluate a constraint/config expression against the bound config `env`.
/// Returns `None` for anything not reducible to a constant (the caller decides
/// whether that is an error in context); never panics on bad operands.
/// Visit every identifier name referenced in a constant expression.
fn collect_idents(e: &Expr, f: &mut impl FnMut(&str)) {
    match &e.kind {
        ExprKind::Ident(id) => f(&id.name),
        ExprKind::BinOp { lhs, rhs, .. } => {
            collect_idents(lhs, f);
            collect_idents(rhs, f);
        }
        ExprKind::Not(inner) => collect_idents(inner, f),
        _ => {}
    }
}

fn const_eval(e: &Expr, env: &HashMap<String, ConstVal>) -> Option<ConstVal> {
    match &e.kind {
        ExprKind::IntLit(n) | ExprKind::DurationLit(n) => Some(ConstVal::Int(*n)),
        ExprKind::BoolLit(b) => Some(ConstVal::Bool(*b)),
        ExprKind::Ident(id) => env.get(&id.name).copied(),
        ExprKind::Not(inner) => match const_eval(inner, env)? {
            ConstVal::Bool(b) => Some(ConstVal::Bool(!b)),
            ConstVal::Int(_) => None,
        },
        ExprKind::BinOp { op, lhs, rhs } => {
            let l = const_eval(lhs, env)?;
            let r = const_eval(rhs, env)?;
            const_binop(*op, l, r)
        }
        _ => None,
    }
}

fn const_binop(op: BinOp, l: ConstVal, r: ConstVal) -> Option<ConstVal> {
    use ConstVal::{Bool, Int};
    match (op, l, r) {
        // Integer arithmetic (saturating on the rare overflow; div/rem-by-zero → None).
        (BinOp::Add | BinOp::AddWrap | BinOp::AddSat, Int(a), Int(b)) => Some(Int(a.saturating_add(b))),
        (BinOp::Sub | BinOp::SubWrap | BinOp::SubSat, Int(a), Int(b)) => Some(Int(a.saturating_sub(b))),
        (BinOp::Mul | BinOp::MulWrap | BinOp::MulSat, Int(a), Int(b)) => Some(Int(a.saturating_mul(b))),
        (BinOp::Div, Int(a), Int(b)) => (b != 0).then(|| Int(a / b)),
        (BinOp::Rem, Int(a), Int(b)) => (b != 0).then(|| Int(a % b)),
        // Integer comparisons → Bool.
        (BinOp::EqEq, Int(a), Int(b)) => Some(Bool(a == b)),
        (BinOp::NotEq, Int(a), Int(b)) => Some(Bool(a != b)),
        (BinOp::Lt, Int(a), Int(b)) => Some(Bool(a < b)),
        (BinOp::Le, Int(a), Int(b)) => Some(Bool(a <= b)),
        (BinOp::Gt, Int(a), Int(b)) => Some(Bool(a > b)),
        (BinOp::Ge, Int(a), Int(b)) => Some(Bool(a >= b)),
        // Boolean logic.
        (BinOp::And, Bool(a), Bool(b)) => Some(Bool(a && b)),
        (BinOp::Or, Bool(a), Bool(b)) => Some(Bool(a || b)),
        (BinOp::EqEq, Bool(a), Bool(b)) => Some(Bool(a == b)),
        (BinOp::NotEq, Bool(a), Bool(b)) => Some(Bool(a != b)),
        _ => None, // type-mismatched operands
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

    /// §3.2/§4.1 — a `where` constraint on a config field is enforced at
    /// instantiation, not merely parsed and ignored.
    #[test]
    fn config_where_constraint_is_enforced() {
        let board = |speed: &str| {
            format!(
                r#"
device throttle {{
    regs {{ CR : reg32 at 0x00 access rw {{}} }}
    config {{ speed : u32 where speed <= 400_000 = 100_000 }}
    needs {{ clock : clock_source }}
}}
board b {{
    soc s {{ clocks {{ sysclk : clock_source = 8MHz }} }}
    t : throttle at 0x4000_0000 {{ config {{ speed = {speed} }} needs {{ clock = soc.sysclk }} }}
}}
program p {{ use board b as dev  on sys.start {{ }} }}
"#
            )
        };
        // Within the bound (boundary value): resolves cleanly.
        assert!(resolve_src(&board("400_000")).is_ok(), "in-range config should resolve");
        // Over the bound: a compile error naming the field and its constraint.
        let errs = resolve_src(&board("500_000")).expect_err("expected where-constraint violation");
        assert!(
            errs.iter().any(|e| e.msg.contains("where") && e.msg.contains("speed")),
            "expected a `where`-constraint violation on `speed`, got: {:?}",
            errs
        );
    }
}

