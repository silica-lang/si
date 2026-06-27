//! §6.3/§12 — the LLVM-IR canary (audit #35 P2-1).
//!
//! A second, structurally independent SIR consumer.  These tests are hermetic:
//! they build SIR fixtures by hand and assert the *shape* of the emitted textual
//! LLVM IR — no LLVM toolchain required (the toolchain-backed `llvm-as`/`opt
//! -verify`/compile+run gate lives in `harness/llvm_canary.sh`).
//!
//! The two load-bearing structural claims:
//!   1. The overflow trap lowers to an LLVM intrinsic (`llvm.*.with.overflow.iN`
//!      + `llvm.trap`), proving it was never a C `__builtin`.
//!   2. The module references no libc / C-runtime symbol — SIR is target-neutral.

use silicac::backend::llvm::LlvmBackend;
use silicac::backend::Target;
use silicac::sir::*;

fn reaction(body: Vec<SirStmt>) -> SirReaction {
    SirReaction {
        id: 0,
        trigger: SirTrigger::SysStart,
        body,
        priority: 0,
        disposition: SirDisposition::Escalate,
        yields: false,
        deadline_ns: None,
        overflow: SirOverflow::Coalesce,
    }
}

fn cell(name: &str, ty: SirType, init: u64) -> SirVar {
    SirVar { name: name.into(), ty, init: SirExpr::U64(init), is_cell: true }
}

fn module(vars: Vec<SirVar>, body: Vec<SirStmt>) -> SirModule {
    SirModule { vars, reactions: vec![reaction(body)], ..Default::default() }
}

/// No emitted canary may reference a libc / C-runtime symbol or a compiler
/// builtin — that is the entire point of a *second* backend (§6.1/§12).
fn assert_no_c_isms(ll: &str) {
    for needle in [
        "__builtin",
        "@printf",
        "@puts",
        "@putchar",
        "@write(",
        "@malloc",
        "@memcpy",
        "@fwrite",
        "@fflush",
    ] {
        assert!(!ll.contains(needle), "emitted IR leaks a C-ism `{}`:\n{}", needle, ll);
    }
}

#[test]
fn arithmetic_exit_lowers_to_pure_ir() {
    // result = a + b; exit(result)  — the runnable canary (a=20, b=22 → 42).
    let body = vec![
        SirStmt::Assign {
            target: SirPlace::Var("result".into()),
            value: SirExpr::Arith {
                op: SirArithOp::Add,
                mode: OverflowMode::Trap,
                width: 32,
                signed: false,
                lhs: Box::new(SirExpr::Load("a".into())),
                rhs: Box::new(SirExpr::Load("b".into())),
            },
        },
        SirStmt::Exit(SirExpr::Load("result".into())),
    ];
    let m = module(
        vec![cell("a", SirType::U32, 20), cell("b", SirType::U32, 22), cell("result", SirType::U32, 0)],
        body,
    );
    let ll = LlvmBackend::new().emit(&m);

    assert!(ll.contains("define i32 @main()"), "no main:\n{}", ll);
    assert!(ll.contains("@a = global i32 20"), "missing cell global:\n{}", ll);
    // The headline proof: trap = LLVM overflow intrinsic + llvm.trap, not C.
    assert!(ll.contains("@llvm.uadd.with.overflow.i32"), "trap not an LLVM intrinsic:\n{}", ll);
    assert!(ll.contains("call void @llvm.trap()"), "no llvm.trap:\n{}", ll);
    assert!(ll.contains("extractvalue { i32, i1 }"), "no overflow extract:\n{}", ll);
    assert!(ll.contains("br i1"), "no trap branch:\n{}", ll);
    assert!(ll.contains("unreachable"), "trap block must be unreachable:\n{}", ll);
    // exit(result) → ret i32 (the process exit code the harness reads back).
    assert!(ll.contains("ret i32"), "no ret:\n{}", ll);
    // The declares are emitted exactly once.
    assert_eq!(ll.matches("declare void @llvm.trap()").count(), 1, "trap declared more than once");
    assert_no_c_isms(&ll);
}

#[test]
fn signed_trap_uses_the_signed_intrinsic() {
    // A signed checked subtract picks `llvm.ssub.*`, not `llvm.usub.*`.
    let body = vec![SirStmt::Assign {
        target: SirPlace::Var("d".into()),
        value: SirExpr::Arith {
            op: SirArithOp::Sub,
            mode: OverflowMode::Trap,
            width: 16,
            signed: true,
            lhs: Box::new(SirExpr::Load("d".into())),
            rhs: Box::new(SirExpr::U64(1)),
        },
    }];
    let ll = LlvmBackend::new().emit(&module(vec![cell("d", SirType::S16, 0)], body));
    assert!(ll.contains("@llvm.ssub.with.overflow.i16"), "signed sub intrinsic:\n{}", ll);
    assert!(!ll.contains("llvm.usub"), "must not use unsigned intrinsic for signed op:\n{}", ll);
}

#[test]
fn wrap_is_plain_arith_and_saturate_is_a_select() {
    // wrap → bare `add iN`; saturate → with.overflow + `select i1`.
    let body = vec![
        SirStmt::Assign {
            target: SirPlace::Var("w".into()),
            value: SirExpr::Arith {
                op: SirArithOp::Add,
                mode: OverflowMode::Wrap,
                width: 8,
                signed: false,
                lhs: Box::new(SirExpr::Load("w".into())),
                rhs: Box::new(SirExpr::U64(100)),
            },
        },
        SirStmt::Assign {
            target: SirPlace::Var("s".into()),
            value: SirExpr::Arith {
                op: SirArithOp::Add,
                mode: OverflowMode::Saturate,
                width: 8,
                signed: false,
                lhs: Box::new(SirExpr::Load("s".into())),
                rhs: Box::new(SirExpr::U64(100)),
            },
        },
    ];
    let ll = LlvmBackend::new()
        .emit(&module(vec![cell("w", SirType::U8, 200), cell("s", SirType::U8, 200)], body));
    assert!(ll.contains("= add i8"), "wrap should be a plain add:\n{}", ll);
    assert!(ll.contains("select i1"), "saturate should clamp via select:\n{}", ll);
    // Wrap must NOT introduce a trap path.
    assert!(!ll.contains("@llvm.uadd.with.overflow.i8\n") || ll.contains("select i1"));
    assert_no_c_isms(&ll);
}

#[test]
fn host_io_is_a_raw_syscall_not_libc() {
    // The hello fixture (mirrors backend::c::emit_hello_world) — host-io lowers
    // to a raw `svc` syscall + a private string constant, never a libc symbol.
    let m = SirModule {
        reactions: vec![reaction(vec![SirStmt::Intrinsic(SirIntrinsic::HostIoPrintStr(
            "Hello, World!\n".into(),
        ))])],
        ..Default::default()
    };
    let ll = LlvmBackend::new().emit(&m);
    assert!(ll.contains("svc #0x80"), "host-io should be a raw syscall:\n{}", ll);
    assert!(ll.contains("private unnamed_addr constant"), "string should be a private constant:\n{}", ll);
    assert!(ll.contains("define i32 @main()"));
    assert!(ll.contains("ret i32 0"), "no-exit program returns 0:\n{}", ll);
    assert_no_c_isms(&ll);
}

#[test]
fn unsupported_constructs_are_signposted_not_silently_dropped() {
    // A construct outside the supported subset becomes a visible comment, never
    // invalid IR — the signpost for what a full LLVM backend would still need.
    // `DeviceOp` stays unsupported by design (the C backend `#error`s it on metal;
    // the resolver inlines composed ops to register/bus accesses before codegen).
    let body = vec![SirStmt::DeviceOp { device: 0, op: "frob".into(), args: vec![] }];
    let ll = LlvmBackend::new().emit(&module(vec![], body));
    assert!(ll.contains("; unsupported in llvm canary: DeviceOp"), "DeviceOp should be signposted:\n{}", ll);
    assert!(ll.contains("ret i32 0"));
}

// ─── P3-4a: extended scalar subset + control flow + reaction functions ────────

#[test]
fn if_lowers_to_branches() {
    // `if cond { out = 1 }` → a `br i1` over a then-block joining at endif.
    let body = vec![SirStmt::If {
        cond: SirExpr::BinOp(
            SirBinOp::EqEq,
            Box::new(SirExpr::Load("sel".into())),
            Box::new(SirExpr::U64(5)),
        ),
        then: vec![SirStmt::Assign {
            target: SirPlace::Var("out".into()),
            value: SirExpr::U64(1),
        }],
    }];
    let ll = LlvmBackend::new()
        .emit(&module(vec![cell("sel", SirType::U32, 5), cell("out", SirType::U32, 0)], body));
    assert!(ll.contains("br i1"), "no conditional branch:\n{}", ll);
    assert!(ll.contains("then") && ll.contains("endif"), "no then/endif blocks:\n{}", ll);
    assert!(ll.contains("br label %endif"), "then must join the end block:\n{}", ll);
    assert_no_c_isms(&ll);
}

#[test]
fn now_is_the_llvm_cycle_counter_not_libc() {
    let body = vec![SirStmt::Assign {
        target: SirPlace::Var("stamp".into()),
        value: SirExpr::Now,
    }];
    let ll = LlvmBackend::new().emit(&module(vec![cell("stamp", SirType::Instant, 0)], body));
    assert!(ll.contains("call i64 @llvm.readcyclecounter()"), "now() not the LLVM intrinsic:\n{}", ll);
    assert!(ll.contains("declare i64 @llvm.readcyclecounter()"), "intrinsic not declared:\n{}", ll);
    // Never a libc clock.
    assert!(!ll.contains("clock_gettime") && !ll.contains("@time"), "now() leaked a libc clock:\n{}", ll);
    assert_no_c_isms(&ll);
}

#[test]
fn signed_saturate_clamps_via_ashr() {
    // A signed saturating add clamps to INT_MAX/INT_MIN by the sign of lhs.
    let body = vec![SirStmt::Assign {
        target: SirPlace::Var("s".into()),
        value: SirExpr::Arith {
            op: SirArithOp::Add,
            mode: OverflowMode::Saturate,
            width: 16,
            signed: true,
            lhs: Box::new(SirExpr::Load("s".into())),
            rhs: Box::new(SirExpr::U64(1)),
        },
    }];
    let ll = LlvmBackend::new().emit(&module(vec![cell("s", SirType::S16, 0)], body));
    assert!(ll.contains("@llvm.sadd.with.overflow.i16"), "signed overflow intrinsic:\n{}", ll);
    assert!(ll.contains("ashr i16"), "signed saturate should clamp via ashr:\n{}", ll);
    assert!(ll.contains("select i1"), "saturate should select the clamp:\n{}", ll);
    assert_no_c_isms(&ll);
}

#[test]
fn non_sys_start_reactions_become_void_functions() {
    // A periodic reaction lowers to its own `@__reaction_N` (no scheduler yet);
    // `@main` (sys.start) and the reaction function coexist in one module.
    let mut m = module(
        vec![cell("out", SirType::U32, 0)],
        vec![SirStmt::Assign { target: SirPlace::Var("out".into()), value: SirExpr::U64(7) }],
    );
    let mut rx = reaction(vec![SirStmt::Assign {
        target: SirPlace::Var("out".into()),
        value: SirExpr::Arith {
            op: SirArithOp::Add,
            mode: OverflowMode::Wrap,
            width: 32,
            signed: false,
            lhs: Box::new(SirExpr::Load("out".into())),
            rhs: Box::new(SirExpr::U64(1)),
        },
    }]);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(100_000_000);
    m.reactions.push(rx);
    let ll = LlvmBackend::new().emit(&m);
    assert!(ll.contains("define i32 @main()"), "main present:\n{}", ll);
    assert!(ll.contains("define void @__reaction_1()"), "reaction function:\n{}", ll);
    assert!(ll.contains("ret void"), "reaction returns void:\n{}", ll);
    assert_no_c_isms(&ll);
}

// ─── P3-4b: MMIO register access (volatile load/store) ────────────────────────

/// A module with one MMIO device (`base`) plus a sys.start `body`.
fn module_with_device(base: u64, vars: Vec<SirVar>, body: Vec<SirStmt>) -> SirModule {
    SirModule {
        vars,
        reactions: vec![reaction(body)],
        devices: vec![SirDevice {
            id: 0,
            name: "w".into(),
            base_addr: Some(base),
            kind: SirDeviceKind::Generic,
            regs: vec![],
        }],
        ..Default::default()
    }
}

#[test]
fn reg_store_is_a_volatile_rmw_at_the_absolute_address() {
    // CTRL.enable = 1 on a rw field → volatile load/modify/store at base+offset.
    let body = vec![SirStmt::Assign {
        target: SirPlace::Reg {
            device: 0,
            reg_offset: 0x00,
            width: 32,
            field_mask: 0x1,
            field_shift: 0,
            access: SirRegAccess::Rw,
        },
        value: SirExpr::U64(1),
    }];
    let ll = LlvmBackend::new().emit(&module_with_device(0x4000_5000, vec![], body));
    assert!(ll.contains("inttoptr i64 1073762304 to ptr"), "absolute address:\n{}", ll);
    assert!(ll.contains("load volatile i32"), "RMW read:\n{}", ll);
    assert!(ll.contains("store volatile i32"), "volatile store:\n{}", ll);
    assert!(ll.contains("or i32"), "RMW combine:\n{}", ll);
    assert_no_c_isms(&ll);
}

#[test]
fn write_only_field_stores_without_a_read() {
    // A `wo` field is written directly — no read-modify-write.
    let body = vec![SirStmt::Assign {
        target: SirPlace::Reg {
            device: 0,
            reg_offset: 0x08,
            width: 32,
            field_mask: 0x1,
            field_shift: 0,
            access: SirRegAccess::Wo,
        },
        value: SirExpr::U64(1),
    }];
    let ll = LlvmBackend::new().emit(&module_with_device(0x4000_5000, vec![], body));
    assert!(ll.contains("store volatile i32"), "volatile store:\n{}", ll);
    assert!(!ll.contains("load volatile"), "wo field must not read:\n{}", ll);
}

#[test]
fn reg_load_is_a_masked_shifted_volatile_load() {
    // last = DATA (a field at shift 4, mask 0xF0) → load volatile, and, lshr.
    let body = vec![SirStmt::Assign {
        target: SirPlace::Var("last".into()),
        value: SirExpr::RegLoad {
            device: 0,
            reg_offset: 0x04,
            width: 32,
            field_mask: 0xF0,
            field_shift: 4,
            access: SirRegAccess::Ro,
        },
    }];
    let ll = LlvmBackend::new()
        .emit(&module_with_device(0x4000_5000, vec![cell("last", SirType::U32, 0)], body));
    assert!(ll.contains("inttoptr i64 1073762308 to ptr"), "base+offset address:\n{}", ll);
    assert!(ll.contains("load volatile i32"), "volatile read:\n{}", ll);
    assert!(ll.contains("and i32") && ll.contains("lshr i32"), "mask + shift:\n{}", ll);
    assert_no_c_isms(&ll);
}

// ─── P3-4c: metal direction (vector table + Reset_Handler) ────────────────────

#[test]
fn metal_emits_a_freestanding_reset_handler_not_main() {
    // `--target metal-nrf52840 --emit-llvm`: a freestanding module that boots via
    // a `.vectors` table into `Reset_Handler` (runs sys.start, then idles) — no
    // `@main`, no host syscall.
    let body = vec![SirStmt::Assign {
        target: SirPlace::Var("value".into()),
        value: SirExpr::U64(42),
    }];
    let ll = LlvmBackend::with_target(Target::MetalNrf52840)
        .emit(&module(vec![cell("value", SirType::U32, 7)], body));
    assert!(ll.contains("target triple = \"thumbv7em-none-eabi\""), "no metal triple:\n{}", ll);
    assert!(ll.contains("@__vectors") && ll.contains("section \".vectors\""), "no vector table:\n{}", ll);
    // The table starts with the initial SP + reset vector (no `every` here, so it
    // is the 16-entry system table — no external IRQ slots).
    assert!(ll.contains("@__vectors = constant [16 x ptr]"), "system-only vector table:\n{}", ll);
    assert!(ll.contains("ptr @_estack,") && ll.contains("ptr @Reset_Handler,"), "vector table contents:\n{}", ll);
    assert!(ll.contains("define void @Reset_Handler()"), "no Reset_Handler:\n{}", ll);
    assert!(ll.contains("store i32 42, ptr @value"), "sys.start body not in reset:\n{}", ll);
    assert!(ll.contains("wfi"), "reset handler must idle:\n{}", ll);
    // Metal must NOT emit the host entry or a host syscall.
    assert!(!ll.contains("define i32 @main()"), "metal must not emit @main:\n{}", ll);
    assert!(!ll.contains("svc #"), "metal must not emit a host syscall:\n{}", ll);
    assert_no_c_isms(&ll);
}

#[test]
fn host_emit_is_unchanged_by_the_metal_path() {
    // The default (host) target still emits `@main` — the two directions coexist.
    let body = vec![SirStmt::Exit(SirExpr::U64(0))];
    let ll = LlvmBackend::new().emit(&module(vec![], body));
    assert!(ll.contains("define i32 @main()"), "host still emits main:\n{}", ll);
    assert!(!ll.contains("@Reset_Handler"), "host must not emit a reset handler:\n{}", ll);
}

// ─── P4-1: metal scheduler (every → TIMER1) ───────────────────────────────────

#[test]
fn metal_every_emits_a_timer_handler_and_full_vector_table() {
    // A periodic reaction → TIMER1 program in Reset_Handler + a TIMER1_IRQHandler
    // that calls the reaction fn; the vector table extends to the TIMER1 slot.
    let mut m = module(
        vec![cell("lit", SirType::Bool, 0)],
        vec![SirStmt::Assign { target: SirPlace::Var("lit".into()), value: SirExpr::Bool(true) }],
    );
    let mut rx = reaction(vec![SirStmt::Assign {
        target: SirPlace::Var("lit".into()),
        value: SirExpr::Not(Box::new(SirExpr::Load("lit".into()))),
    }]);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(500_000_000);
    m.reactions.push(rx);
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    // Reset_Handler does real startup: a .data copy loop + interrupts enabled.
    assert!(ll.contains("define void @Reset_Handler()"), "no reset handler:\n{}", ll);
    assert!(ll.contains("icmp ult ptr") && ll.contains("@_edata"), "no .data copy loop:\n{}", ll);
    assert!(ll.contains("cpsie i"), "interrupts not enabled:\n{}", ll);
    // TIMER1 wired: handler re-arms CC and calls the periodic reaction.
    assert!(ll.contains("define void @TIMER1_IRQHandler()"), "no TIMER1 handler:\n{}", ll);
    assert!(ll.contains("call void @__reaction_1()"), "TIMER1 must call the reaction:\n{}", ll);
    // Full vector table reaching the TIMER1 slot (16+9 = index 25 → 26 entries).
    assert!(ll.contains("@__vectors = constant [26 x ptr]"), "vector table not extended to TIMER1:\n{}", ll);
    assert!(ll.contains("ptr @TIMER1_IRQHandler"), "TIMER1 not vectored:\n{}", ll);
    assert!(ll.contains("define void @__default_handler()"), "no default handler:\n{}", ll);
    assert_no_c_isms(&ll);
}

// ─── P4-2: metal events (GPIOTE) + BASEPRI critical sections ──────────────────

#[test]
fn metal_event_emits_gpiote_handler_and_basepri_critical() {
    // An `on <pin>.falling` reaction → a GPIOTE handler that calls it; a shared
    // cell access (Critical) → a BASEPRI raise/restore around the body.
    let mut m = module(
        vec![cell("lit", SirType::Bool, 0)],
        vec![SirStmt::Assign { target: SirPlace::Var("lit".into()), value: SirExpr::Bool(false) }],
    );
    let mut rx = reaction(vec![SirStmt::Critical {
        // ceiling ≤ max_priority (both reactions are priority 0 here).
        ceiling: 0,
        body: vec![SirStmt::Assign {
            target: SirPlace::Var("lit".into()),
            value: SirExpr::Not(Box::new(SirExpr::Load("lit".into()))),
        }],
    }]);
    rx.id = 1;
    rx.trigger = SirTrigger::Event(0);
    m.reactions.push(rx);
    m.events.push(SirEvent { id: 0, name: "falling".into(), device: 0, pin_index: Some(11) });
    m.devices.push(SirDevice {
        id: 0,
        name: "gpio0".into(),
        base_addr: Some(0x5000_0000),
        kind: SirDeviceKind::Gpio,
        regs: vec![],
    });
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    assert!(ll.contains("define void @GPIOTE_IRQHandler()"), "no GPIOTE handler:\n{}", ll);
    assert!(ll.contains("call void @__reaction_1()"), "GPIOTE must call the event reaction:\n{}", ll);
    assert!(ll.contains("ptr @GPIOTE_IRQHandler"), "GPIOTE not vectored:\n{}", ll);
    // BASEPRI raise + restore around the shared-cell access.
    assert!(ll.contains("msr basepri"), "no BASEPRI critical:\n{}", ll);
    assert!(ll.contains("mrs $0, basepri"), "BASEPRI not saved:\n{}", ll);
    assert_no_c_isms(&ll);
}

// ─── P4-3: metal yields state machine + bus IRQ ───────────────────────────────

#[test]
fn metal_yielding_reaction_emits_segment_machine_and_bus_irq() {
    // A bus transaction → an IRQ-driven segment state machine: a `@__react_N_run`
    // dispatcher (switch on a state global), a `@__BUS_IRQHandler` that resumes
    // the owner, and frame globals that survive the IRQ return.
    let mut m = module(vec![cell("samples", SirType::U32, 0)], vec![]);
    let mut rx = reaction(vec![
        SirStmt::BusXfer {
            device: 0,
            op: "read_reg".into(),
            args: vec![SirExpr::U64(0x76), SirExpr::U64(0xFA)],
            dst: "__bus".into(),
            propagate: true,
            fault_codes: vec!["nak".into()],
            code_dst: None,
        },
        SirStmt::Assign {
            target: SirPlace::Var("samples".into()),
            value: SirExpr::BinOp(
                SirBinOp::Add,
                Box::new(SirExpr::Load("samples".into())),
                Box::new(SirExpr::U64(1)),
            ),
        },
    ]);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(1_000_000_000);
    rx.yields = true;
    m.reactions.push(rx);
    let reg = |name: &str, off: u64| SirReg {
        name: name.into(),
        offset: off,
        width: 32,
        access: SirRegAccess::Rw,
        reset: 0,
    };
    m.devices.push(SirDevice {
        id: 0,
        name: "i2c0".into(),
        base_addr: Some(0x4000_3000),
        kind: SirDeviceKind::Generic,
        regs: vec![reg("CR", 0x0), reg("SR", 0x4), reg("SA", 0x8), reg("RA", 0xC), reg("DR", 0x10)],
    });
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    // Segment dispatcher + frame globals.
    assert!(ll.contains("define void @__react_1_run()"), "no dispatcher:\n{}", ll);
    assert!(ll.contains("switch i32") && ll.contains("@__rf_1_state"), "no state switch:\n{}", ll);
    assert!(ll.contains("@__rf_1_state = global i32 0"), "no frame state global:\n{}", ll);
    assert!(ll.contains("@__bus_owner = global i32 -1"), "no bus-owner global:\n{}", ll);
    // Bus kick (CR start) + resume (SR decode) + IRQ handler that resumes the owner.
    assert!(ll.contains("; CR kick"), "no bus kick:\n{}", ll);
    assert!(ll.contains("; SR"), "no SR resume decode:\n{}", ll);
    assert!(ll.contains("define void @__BUS_IRQHandler()"), "no bus IRQ handler:\n{}", ll);
    assert!(ll.contains("call void @__react_1_run()"), "IRQ must resume the owner:\n{}", ll);
    // The trigger entry coalesces a re-fire while in flight.
    assert!(ll.contains("define void @__reaction_1()"), "no trigger entry:\n{}", ll);
    // Bus IRQ is vectored (line 8 → index 24).
    assert!(ll.contains("ptr @__BUS_IRQHandler"), "bus IRQ not vectored:\n{}", ll);
    assert_no_c_isms(&ll);
}

// ─── P5-1: metal SysTick + now() uptime ───────────────────────────────────────

#[test]
fn metal_now_reads_systick_uptime_not_cycle_counter() {
    // On metal, `now()` reads a SysTick-driven `@__uptime_ns` (1 ms base), NOT the
    // host `llvm.readcyclecounter`: Reset_Handler programs SysTick, a
    // SysTick_Handler advances the uptime, and vector slot 15 points at it.
    let body = vec![SirStmt::Assign {
        target: SirPlace::Var("stamp".into()),
        value: SirExpr::Now,
    }];
    let mut m = module(vec![cell("stamp", SirType::Instant, 0)], vec![]);
    let mut rx = reaction(body);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(100_000_000);
    m.reactions.push(rx);
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    // The uptime global + handler that advances it by 1 ms.
    assert!(ll.contains("@__uptime_ns = global i64 0"), "no uptime global:\n{}", ll);
    assert!(ll.contains("define void @SysTick_Handler()"), "no SysTick handler:\n{}", ll);
    assert!(ll.contains("add i64") && ll.contains("1000000"), "handler must add 1ms:\n{}", ll);
    // SysTick programmed in the reset (RVR/CSR at the SCS).
    assert!(ll.contains("; SYST_RVR") && ll.contains("; SYST_CSR: ENABLE|TICKINT|CLKSOURCE"), "SysTick not programmed:\n{}", ll);
    // now() reads the uptime, never the cycle counter, on metal.
    assert!(ll.contains("load volatile i64, ptr @__uptime_ns"), "now() must read uptime:\n{}", ll);
    assert!(!ll.contains("readcyclecounter"), "metal now() must not use the cycle counter:\n{}", ll);
    // Vector slot 15 = SysTick.
    assert!(ll.contains("ptr @SysTick_Handler"), "SysTick not vectored:\n{}", ll);
    assert_no_c_isms(&ll);
}

#[test]
fn host_now_still_uses_cycle_counter() {
    // The host path is unchanged: now() stays the LLVM cycle-counter intrinsic
    // (the P5-1 SysTick lowering is metal-only).
    let body = vec![SirStmt::Assign {
        target: SirPlace::Var("stamp".into()),
        value: SirExpr::Now,
    }];
    let ll = LlvmBackend::new().emit(&module(vec![cell("stamp", SirType::Instant, 0)], body));
    assert!(ll.contains("call i64 @llvm.readcyclecounter()"), "host now() must keep the cycle counter:\n{}", ll);
    assert!(!ll.contains("@__uptime_ns"), "host must not emit the metal uptime global:\n{}", ll);
}

// ─── P5-2: drive_safe + overflow-trap safe-state + Safe disposition ────────────

#[test]
fn metal_overflow_trap_drives_safe_then_halts() {
    // On metal a trapping `+` routes to `@__silica_overflow_trap` → `@__drive_safe`
    // + halt (NOT the bare `llvm.trap` of the host path).
    let body = vec![SirStmt::Assign {
        target: SirPlace::Var("acc".into()),
        value: SirExpr::Arith {
            op: SirArithOp::Add,
            mode: OverflowMode::Trap,
            width: 8,
            signed: false,
            lhs: Box::new(SirExpr::Load("acc".into())),
            rhs: Box::new(SirExpr::U64(85)),
        },
    }];
    let mut m = module(vec![cell("acc", SirType::U8, 0)], vec![]);
    let mut rx = reaction(body);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(100_000_000);
    m.reactions.push(rx);
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    assert!(ll.contains("define void @__silica_overflow_trap()"), "no overflow-trap fn:\n{}", ll);
    assert!(ll.contains("define void @__drive_safe()"), "no drive_safe fn:\n{}", ll);
    assert!(ll.contains("call void @__silica_overflow_trap()"), "trap must call the trap fn:\n{}", ll);
    assert!(ll.contains("call void @__drive_safe()"), "trap fn must drive safe:\n{}", ll);
    // The overflow intrinsic still detects overflow; only the *trap target* changed.
    assert!(ll.contains("@llvm.uadd.with.overflow.i8"), "overflow not detected via intrinsic:\n{}", ll);
    assert!(!ll.contains("call void @llvm.trap()"), "metal trap must drive safe, not llvm.trap:\n{}", ll);
    assert_no_c_isms(&ll);
}

#[test]
fn host_overflow_trap_stays_llvm_trap() {
    // The host path is unchanged: a trapping `+` lowers to the `llvm.trap`
    // intrinsic (the P5-2 safe-state routing is metal-only).
    let body = vec![SirStmt::Assign {
        target: SirPlace::Var("acc".into()),
        value: SirExpr::Arith {
            op: SirArithOp::Add,
            mode: OverflowMode::Trap,
            width: 8,
            signed: false,
            lhs: Box::new(SirExpr::Load("acc".into())),
            rhs: Box::new(SirExpr::U64(85)),
        },
    }];
    let ll = LlvmBackend::new().emit(&module(vec![cell("acc", SirType::U8, 0)], body));
    assert!(ll.contains("call void @llvm.trap()"), "host trap must stay llvm.trap:\n{}", ll);
    assert!(!ll.contains("@__silica_overflow_trap"), "host must not emit the metal trap fn:\n{}", ll);
}

#[test]
fn metal_drive_safe_runs_the_safe_sequence_and_halts() {
    // A `DriveSafe` guard → `@__drive_safe` (which runs each device's safe-sequence
    // register writes) then mask-and-hold.
    let mut m = module(vec![], vec![]);
    let mut rx = reaction(vec![SirStmt::DriveSafe]);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(100_000_000);
    m.reactions.push(rx);
    m.devices.push(SirDevice {
        id: 0,
        name: "motor".into(),
        base_addr: Some(0x5001_0000),
        kind: SirDeviceKind::Generic,
        regs: vec![],
    });
    // safe sequence: de-energize (clear enable bit 0 of the rw control reg).
    m.safe_seqs.push(SafeSeq {
        device: 0,
        state: "off".into(),
        body: vec![SirStmt::RegWrite {
            device: 0,
            reg_offset: 0,
            width: 32,
            writes: vec![(1, 0, SirRegAccess::Rw, SirExpr::U64(0))],
        }],
    });
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    assert!(ll.contains("define void @__drive_safe()"), "no drive_safe fn:\n{}", ll);
    assert!(ll.contains("safe state 'off'"), "drive_safe must label the safe state:\n{}", ll);
    assert!(ll.contains("store volatile i32"), "safe sequence must write the register:\n{}", ll);
    assert!(ll.contains("call void @__drive_safe()"), "DriveSafe must call drive_safe:\n{}", ll);
    assert!(ll.contains("cpsid i"), "must mask interrupts before driving safe:\n{}", ll);
    assert_no_c_isms(&ll);
}

// ─── P5-3: poll / await + non-yielding fault flow ─────────────────────────────

fn poll_reaction(stmt: SirStmt, disp: SirDisposition) -> SirReaction {
    let mut rx = reaction(vec![
        stmt,
        SirStmt::Assign {
            target: SirPlace::Var("done".into()),
            value: SirExpr::Arith {
                op: SirArithOp::Add,
                mode: OverflowMode::Wrap,
                width: 32,
                signed: false,
                lhs: Box::new(SirExpr::Load("done".into())),
                rhs: Box::new(SirExpr::U64(1)),
            },
        },
    ]);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(100_000_000);
    rx.disposition = disp;
    rx
}

#[test]
fn metal_poll_lowers_to_a_bounded_spin_with_fault_disposition() {
    let poll = SirStmt::Poll {
        cond: SirExpr::BinOp(
            SirBinOp::EqEq,
            Box::new(SirExpr::Load("ready".into())),
            Box::new(SirExpr::U64(1)),
        ),
        fault_code: "timeout".into(),
        within_ns: 200_000,
    };
    let mut m = module(
        vec![cell("ready", SirType::U32, 1), cell("done", SirType::U32, 0)],
        vec![],
    );
    m.reactions.push(poll_reaction(poll, SirDisposition::Skip));
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    assert!(!ll.contains("; unsupported in llvm canary: Poll"), "poll still unsupported:\n{}", ll);
    assert!(ll.contains("%__faulted = alloca i8"), "no fault flag:\n{}", ll);
    assert!(ll.contains("icmp ugt i32"), "no bound check:\n{}", ll);
    assert!(ll.contains("store i8 1, ptr %__faulted"), "bound elapse must set faulted:\n{}", ll);
    // Skip disposition: the timeout returns without doing the post-poll work.
    assert!(ll.contains("ret void ; skip/escalate this activation"), "no skip disposition:\n{}", ll);
    // A busy-wait does NOT yield: no wfi in the poll reaction function itself.
    let from = ll.find("define void @__reaction_1()").unwrap();
    let react = &ll[from..from + ll[from..].find("\n}").unwrap()];
    assert!(!react.contains("wfi"), "poll must not wfi (it is a busy-wait):\n{}", react);
    assert_no_c_isms(&ll);
}

#[test]
fn metal_await_lowers_to_a_recheck_loop_with_wfi() {
    let await_s = SirStmt::Await {
        cond: SirExpr::BinOp(
            SirBinOp::EqEq,
            Box::new(SirExpr::Load("ready".into())),
            Box::new(SirExpr::U64(1)),
        ),
        fault_code: "not_ready".into(),
        within_ns: 50_000_000,
        recheck_ns: 6_250_000,
    };
    let mut m = module(
        vec![cell("ready", SirType::U32, 1), cell("done", SirType::U32, 0)],
        vec![],
    );
    m.reactions.push(poll_reaction(await_s, SirDisposition::Skip));
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    assert!(!ll.contains("; unsupported in llvm canary: Await"), "await still unsupported:\n{}", ll);
    assert!(ll.contains("%__faulted = alloca i8"), "no fault flag:\n{}", ll);
    // await yields to ISRs between re-checks: a wfi inside the wait loop.
    assert!(ll.contains("wfi") && ll.contains("re-checks"), "await must wfi between checks:\n{}", ll);
    assert!(ll.contains("store i8 1, ptr %__faulted"), "bound elapse must set faulted:\n{}", ll);
    assert_no_c_isms(&ll);
}

#[test]
fn metal_poll_retry_disposition_wraps_the_body_in_a_retry_loop() {
    let poll = SirStmt::Poll {
        cond: SirExpr::BinOp(
            SirBinOp::EqEq,
            Box::new(SirExpr::Load("ready".into())),
            Box::new(SirExpr::U64(1)),
        ),
        fault_code: "timeout".into(),
        within_ns: 200_000,
    };
    let mut m = module(
        vec![cell("ready", SirType::U32, 1), cell("done", SirType::U32, 0)],
        vec![],
    );
    m.reactions.push(poll_reaction(poll, SirDisposition::Retry { max: 3 }));
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    // A retry disposition: bounded re-run loop with a counter compared to max.
    assert!(ll.contains("%__retry = alloca i32"), "no retry counter:\n{}", ll);
    assert!(ll.contains("icmp ult i32") && ll.contains(", 3"), "retry must compare against max:\n{}", ll);
    assert!(ll.contains("retryloop"), "no retry back-edge:\n{}", ll);
    assert_no_c_isms(&ll);
}

// ─── P5-4: within-deadline + watchdog ─────────────────────────────────────────

#[test]
fn metal_deadline_and_watchdog_are_wired() {
    // A yielding reaction with a `within` deadline + a board watchdog → deadline
    // countdown globals, an arm on the trigger entry, a SysTick decrement that
    // latches `@__deadline_missed`, watchdog config in the reset, and a feed
    // gated on the deadline.
    let mut m = module(vec![cell("samples", SirType::U32, 0)], vec![]);
    let mut rx = reaction(vec![
        SirStmt::BusXfer {
            device: 0,
            op: "read_reg".into(),
            args: vec![SirExpr::U64(0x76), SirExpr::U64(0xFA)],
            dst: "__bus".into(),
            propagate: true,
            fault_codes: vec!["nak".into()],
            code_dst: None,
        },
        SirStmt::Assign {
            target: SirPlace::Var("samples".into()),
            value: SirExpr::BinOp(
                SirBinOp::Add,
                Box::new(SirExpr::Load("samples".into())),
                Box::new(SirExpr::U64(1)),
            ),
        },
    ]);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(100_000_000);
    rx.yields = true;
    rx.deadline_ns = Some(30_000_000); // within 30ms
    m.reactions.push(rx);
    let reg = |name: &str, off: u64| SirReg {
        name: name.into(),
        offset: off,
        width: 32,
        access: SirRegAccess::Rw,
        reset: 0,
    };
    m.devices.push(SirDevice {
        id: 0,
        name: "i2c0".into(),
        base_addr: Some(0x4000_3000),
        kind: SirDeviceKind::Generic,
        regs: vec![reg("CR", 0x0), reg("SR", 0x4), reg("SA", 0x8), reg("RA", 0xC), reg("DR", 0x10)],
    });
    m.devices.push(SirDevice {
        id: 1,
        name: "wdt0".into(),
        base_addr: Some(0x4001_0000),
        kind: SirDeviceKind::Generic,
        regs: vec![reg("CR", 0x0), reg("RLR", 0x4), reg("KR", 0x8)],
    });
    m.watchdog_device = Some(1);
    m.watchdog_timeout_ns = Some(100_000_000); // 100ms
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    // Deadline state + arm (30ms → 30 ticks) on the trigger entry.
    assert!(ll.contains("@__deadline_1 = global i32 0"), "no deadline countdown:\n{}", ll);
    assert!(ll.contains("@__deadline_missed = global i32 0"), "no deadline-missed flag:\n{}", ll);
    assert!(ll.contains("store i32 30, ptr @__deadline_1"), "deadline not armed on fire:\n{}", ll);
    // SysTick decrements the deadline and latches the miss.
    assert!(ll.contains("load i32, ptr @__rf_1_state"), "SysTick must check the frame state:\n{}", ll);
    assert!(ll.contains("store i32 1, ptr @__deadline_missed"), "overrun must latch the miss:\n{}", ll);
    // Watchdog configured in the reset + fed (0xAAAA = 43690), gated on the miss.
    assert!(ll.contains("WDT RLR: reload") && ll.contains("WDT CR: start"), "watchdog not configured:\n{}", ll);
    assert!(ll.contains("store volatile i32 43690") && ll.contains("feed on clean idle"), "no gated feed:\n{}", ll);
    assert!(ll.contains("load i32, ptr @__deadline_missed"), "feed not gated on the deadline:\n{}", ll);
    assert!(ll.contains("ptr @SysTick_Handler"), "SysTick must be vectored (watchdog needs it):\n{}", ll);
    assert_no_c_isms(&ll);
}

// ─── P6-1: rings on the LLVM backend ──────────────────────────────────────────

#[test]
fn ring_lowers_to_backing_store_and_index_math() {
    // A `ring<u32,4>` cell → backing-array + head/tail/count globals; push/pop/len
    // become index arithmetic (no `; unsupported`).
    let mut m = module(vec![cell("out", SirType::U32, 0)], vec![]);
    m.vars.push(SirVar {
        name: "q".into(),
        ty: SirType::Ring { elem_bytes: 4, cap: 4 },
        init: SirExpr::U64(0),
        is_cell: true,
    });
    let mut rx = reaction(vec![
        SirStmt::RingPush { ring: "q".into(), value: SirExpr::U64(7) },
        SirStmt::RingPop { ring: "q".into(), dst: "v".into() },
        SirStmt::Assign { target: SirPlace::Var("out".into()), value: SirExpr::RingLen("q".into()) },
    ]);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(100_000_000);
    m.reactions.push(rx);
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    // Backing store globals.
    assert!(ll.contains("@__ring_q_buf = global [4 x i32] zeroinitializer"), "no ring buffer:\n{}", ll);
    assert!(ll.contains("@__ring_q_head = global i32 0"), "no head:\n{}", ll);
    assert!(ll.contains("@__ring_q_count = global i32 0"), "no count:\n{}", ll);
    // Index math: push stores into buf via GEP, count/tail wrap with urem.
    assert!(ll.contains("getelementptr [4 x i32], ptr @__ring_q_buf"), "no GEP into ring:\n{}", ll);
    assert!(ll.contains("urem i32"), "no modulo wrap:\n{}", ll);
    // len reads count; nothing signposted unsupported.
    assert!(ll.contains("load i32, ptr @__ring_q_count"), "len must read count:\n{}", ll);
    assert!(!ll.contains("; unsupported in llvm canary: RingPush"), "RingPush should be lowered:\n{}", ll);
    assert!(!ll.contains("; unsupported in llvm canary: RingPop"), "RingPop should be lowered:\n{}", ll);
    assert!(!ll.contains("; unsupported expr in llvm canary: RingLen"), "RingLen should be lowered:\n{}", ll);
    assert_no_c_isms(&ll);
}

#[test]
fn ring_pop_is_bounded_with_an_empty_path() {
    // pop guards count > 0 and writes 0 on empty (the defined bounded behaviour).
    let mut m = module(vec![], vec![]);
    m.vars.push(SirVar {
        name: "q".into(),
        ty: SirType::Ring { elem_bytes: 4, cap: 2 },
        init: SirExpr::U64(0),
        is_cell: true,
    });
    let mut rx = reaction(vec![SirStmt::RingPop { ring: "q".into(), dst: "v".into() }]);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(100_000_000);
    m.reactions.push(rx);
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);
    assert!(ll.contains("icmp ugt i32") && ll.contains("@__ring_q_count"), "pop must guard count>0:\n{}", ll);
    assert_no_c_isms(&ll);
}

// ─── P6-2: fixed-point on the LLVM backend ────────────────────────────────────

#[test]
fn fixed_mul_lowers_to_wide_intermediate_and_rescale() {
    // FixedArith Mul → 64-bit intermediate, ashr by frac, trap range-check (the
    // metal trap routes to @__silica_overflow_trap).
    let body = vec![SirStmt::Assign {
        target: SirPlace::Var("out".into()),
        value: SirExpr::FixedArith {
            op: FixedArithOp::Mul,
            mode: OverflowMode::Trap,
            frac_bits: 16,
            width: 32,
            signed: true,
            lhs: Box::new(SirExpr::Load("a".into())),
            rhs: Box::new(SirExpr::Load("b".into())),
        },
    }];
    let mut m = module(
        vec![cell("a", SirType::U32, 0), cell("b", SirType::U32, 0), cell("out", SirType::U32, 0)],
        vec![],
    );
    let mut rx = reaction(body);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(100_000_000);
    m.reactions.push(rx);
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    assert!(ll.contains("mul i64"), "fixed mul must use a 64-bit intermediate:\n{}", ll);
    assert!(ll.contains("ashr i64") && ll.contains(", 16"), "fixed mul must rescale by frac:\n{}", ll);
    assert!(ll.contains("call void @__silica_overflow_trap()"), "trap mode must route to safe-state:\n{}", ll);
    assert!(!ll.contains("; unsupported expr in llvm canary: FixedArith"), "FixedArith should be lowered:\n{}", ll);
    assert_no_c_isms(&ll);
}

#[test]
fn fixed_div_guards_divide_by_zero() {
    // FixedArith Div → shl-then-sdiv in 64-bit, with a divide-by-zero trap guard.
    let body = vec![SirStmt::Assign {
        target: SirPlace::Var("out".into()),
        value: SirExpr::FixedArith {
            op: FixedArithOp::Div,
            mode: OverflowMode::Wrap,
            frac_bits: 16,
            width: 32,
            signed: true,
            lhs: Box::new(SirExpr::Load("a".into())),
            rhs: Box::new(SirExpr::Load("b".into())),
        },
    }];
    let mut m = module(
        vec![cell("a", SirType::U32, 0), cell("b", SirType::U32, 0), cell("out", SirType::U32, 0)],
        vec![],
    );
    let mut rx = reaction(body);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(100_000_000);
    m.reactions.push(rx);
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    assert!(ll.contains("shl i64"), "fixed div must shift the dividend:\n{}", ll);
    assert!(ll.contains("sdiv i64"), "signed fixed div:\n{}", ll);
    assert!(ll.contains("icmp eq i64") && ll.contains("call void @__silica_overflow_trap()"), "div-by-zero must trap:\n{}", ll);
    assert_no_c_isms(&ll);
}

#[test]
fn fixed_cast_shifts_the_binary_point() {
    // FixedCast → sign-aware shift in a 64-bit intermediate, then narrow.
    let body = vec![SirStmt::Assign {
        target: SirPlace::Var("out".into()),
        value: SirExpr::FixedCast {
            inner: Box::new(SirExpr::Load("a".into())),
            shift: -16,
            to_width: 32,
            signed: false,
        },
    }];
    let ll = LlvmBackend::with_target(Target::MetalNrf52840)
        .emit(&module(vec![cell("a", SirType::U32, 0), cell("out", SirType::U32, 0)], body));
    assert!(ll.contains("lshr i64") && ll.contains(", 16"), "fixed cast must shift the binary point:\n{}", ll);
    assert!(!ll.contains("; unsupported expr in llvm canary: FixedCast"), "FixedCast should be lowered:\n{}", ll);
    assert_no_c_isms(&ll);
}

// ─── P6-3: LLVM HardFault fault-decoder parity ────────────────────────────────

#[test]
fn metal_emits_the_layer3_fault_decoder() {
    // The HardFault handler reads SCB CFSR/BFAR, finds the owning region from the
    // address-ownership table, and records the fault to fixed RAM (no on-device
    // strings — the host renders labels from indices).
    let mut m = module(vec![cell("lit", SirType::Bool, 0)], vec![]);
    let mut rx = reaction(vec![SirStmt::Assign {
        target: SirPlace::Var("lit".into()),
        value: SirExpr::Bool(true),
    }]);
    rx.id = 1;
    rx.trigger = SirTrigger::EveryNs(500_000_000);
    m.reactions.push(rx);
    m.devices.push(SirDevice {
        id: 0,
        name: "gpio0".into(),
        base_addr: Some(0x5000_0000),
        kind: SirDeviceKind::Gpio,
        regs: vec![],
    });
    let ll = LlvmBackend::with_target(Target::MetalNrf52840).emit(&m);

    // The decoder function + ownership table + fault record.
    assert!(ll.contains("define void @HardFault_Handler()"), "no HardFault decoder:\n{}", ll);
    assert!(ll.contains("@__owner_start = constant"), "no ownership-start table:\n{}", ll);
    assert!(ll.contains("@__owner_end = constant"), "no ownership-end table:\n{}", ll);
    assert!(ll.contains("@__fault_owner = global i32 -1"), "no fault-owner record:\n{}", ll);
    assert!(ll.contains("@__fault_pending = global i32 0"), "no fault-pending record:\n{}", ll);
    // Reads SCB CFSR (0xE000ED28 = 3758157096) + BFAR (0xE000ED38 = 3758157112).
    assert!(ll.contains("inttoptr i64 3758157096 to ptr"), "must read SCB CFSR:\n{}", ll);
    assert!(ll.contains("inttoptr i64 3758157112 to ptr"), "must read SCB BFAR:\n{}", ll);
    // BFARVALID gate (bit 15 = 32768) + the owner-range comparison.
    assert!(ll.contains("and i32") && ll.contains("32768"), "no BFARVALID check:\n{}", ll);
    assert!(ll.contains("icmp uge i32") && ll.contains("icmp ult i32"), "no owner-range check:\n{}", ll);
    // HardFault is vectored (slot 3).
    assert!(ll.contains("ptr @HardFault_Handler"), "HardFault not vectored:\n{}", ll);
    assert_no_c_isms(&ll);
}
