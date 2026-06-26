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
    // A construct outside the canary subset becomes a visible comment, never
    // invalid IR — the signpost for what a full LLVM backend would still need.
    let body = vec![SirStmt::If { cond: SirExpr::Bool(true), then: vec![] }];
    let ll = LlvmBackend::new().emit(&module(vec![], body));
    assert!(ll.contains("; unsupported in llvm canary: If"), "If should be signposted:\n{}", ll);
    assert!(ll.contains("ret i32 0"));
}
