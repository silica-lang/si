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
    // invalid IR — the signpost for what a full LLVM backend would still need
    // (a ring push has no lowering yet; the MMIO/bus paths are P3-4b/c).
    let body = vec![SirStmt::RingPush { ring: "q".into(), value: SirExpr::U64(1) }];
    let ll = LlvmBackend::new().emit(&module(vec![], body));
    assert!(ll.contains("; unsupported in llvm canary: RingPush"), "RingPush should be signposted:\n{}", ll);
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
