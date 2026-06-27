//! §4.1/D07 — static `when`/`become` typestate.  A `when S` op is callable only
//! when a dominating `become S` proves the device is in state S; otherwise it is
//! a compile error.

use silicac::backend::{c, Target};
use silicac::sim;
use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver};

fn compile(src: &str) -> SirModule {
    let tokens = lexer::lex(src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()))
}

fn trace(src: &str) -> Vec<String> {
    let sir = compile(src);
    sim::run(&sir).render(&sir).lines().map(|s| s.to_string()).collect()
}

fn resolve_err(src: &str) -> Vec<String> {
    let tokens = lexer::lex(src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    match resolver::resolve(&ast) {
        Ok(_) => panic!("expected a resolve error, got success"),
        Err(e) => e.iter().map(|d| d.msg.clone()).collect(),
    }
}

/// A `thermostat` device (states idle→ready) plus a reaction `<body>`.
fn program(dev_extra: &str, body: &str) -> String {
    format!(
        r#"
device thermostat {{
  regs {{
    CTRL : reg32 at 0x00 access rw {{ enable: bit[0] }}
    TEMP : reg32 at 0x04 access ro {{}}
  }}
  states {{ idle, ready }}
  ops {{
    op power_on() -> () {{ CTRL.enable = 1  become ready }}
    op read() when ready -> u32 {{ return TEMP }}
    {dev_extra}
  }}
}}
board demo {{
  soc s {{ memory {{ flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }} clocks {{ sysclk : clock_source = 64MHz }} }}
  t : thermostat at 0x5000_0000
}}
program app {{
  use board demo as b
  let dev = b.t
  cell n : u32 = 0
  every 100ms {{ {body} }}
}}
sim app_sim for app {{ run until 150ms }}
"#
    )
}

#[test]
fn a_guarded_op_after_a_dominating_become_type_checks() {
    let _ = compile(&program("", "dev.power_on()  let v = dev.read()  n = n + 1"));
}

#[test]
fn state_persists_across_multiple_calls_in_one_reaction() {
    // Two reads after one power_on: the device stays `ready`.
    let _ = compile(&program("", "dev.power_on()  let a = dev.read()  let b = dev.read()  n = a + b"));
}

/// A `thermostat` device + board, with two program reactions woven in
/// (`sys.start` body + an `every` body) — for cross-reaction typestate.
fn two_reaction_program(sys_start_body: &str, every_body: &str) -> String {
    format!(
        r#"
device thermostat {{
  regs {{ CTRL : reg32 at 0x00 access rw {{ enable: bit[0] }}  TEMP : reg32 at 0x04 access ro {{}} }}
  states {{ idle, ready }}
  ops {{
    op power_on() -> () {{ CTRL.enable = 1  become ready }}
    op read() when ready -> u32 {{ return TEMP }}
  }}
}}
board demo {{
  soc s {{ memory {{ flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }} clocks {{ sysclk : clock_source = 64MHz }} }}
  t : thermostat at 0x5000_0000
}}
program app {{
  use board demo as b
  let dev = b.t
  cell n : u32 = 0
  on sys.start {{ {sys_start_body} }}
  every 100ms {{ {every_body} }}
}}
sim app_sim for app {{ run until 150ms }}
"#
    )
}

#[test]
fn sys_start_typestate_persists_into_later_reactions() {
    // `power_on()` in `sys.start` leaves the device `ready`; a `read()` in a
    // *separate* `every` reaction is now provably in state `ready` (audit #35
    // P2-2) — this was a compile error under reaction-local typestate.
    let _ = compile(&two_reaction_program("dev.power_on()", "let v = dev.read()  n = v"));
}

#[test]
fn without_sys_start_init_a_later_guarded_op_still_errors() {
    // No boot-time `power_on()` → the device is still `idle` in the `every`
    // reaction, so `read()` is a compile error (persistence is sound, not blind).
    let errs = resolve_err(&two_reaction_program("n = 0", "let v = dev.read()  n = v"));
    assert!(
        errs.iter().any(|m| m.contains("to be in state 'ready'") && m.contains("'idle'")),
        "errs: {:?}",
        errs
    );
}

#[test]
fn calling_a_guarded_op_in_the_wrong_state_is_a_compile_error() {
    // No `power_on()` → the device is still in its initial `idle` state.
    let errs = resolve_err(&program("", "let v = dev.read()  n = n + 1"));
    assert!(
        errs.iter().any(|m| m.contains("to be in state 'ready'") && m.contains("'idle'")),
        "errs: {:?}",
        errs
    );
}

#[test]
fn an_op_guarded_on_an_undeclared_state_is_rejected() {
    let errs = resolve_err(&program(
        "op bad() when flying -> () { }",
        "dev.power_on()  n = n + 1",
    ));
    assert!(errs.iter().any(|m| m.contains("no such state") && m.contains("flying")), "errs: {:?}", errs);
}

#[test]
fn a_become_to_an_undeclared_state_is_rejected() {
    let errs = resolve_err(&program(
        "op weird() -> () { become flying }",
        "dev.power_on()  n = n + 1",
    ));
    assert!(errs.iter().any(|m| m.contains("no such state") && m.contains("flying")), "errs: {:?}", errs);
}

// ─── Runtime typestate precondition (§4.1/D07, audit #35 P3-3) ────────────────

/// A `thermostat` (idle→ready, + an unreachable `broken` state) plus a program
/// body of one or more reactions.  `dev_ops` injects extra device ops.
fn rt(dev_ops: &str, reactions: &str) -> String {
    format!(
        r#"
device thermostat {{
  regs {{
    CTRL : reg32 at 0x00 access rw {{ enable: bit[0] }}
    TEMP : reg32 at 0x04 access ro {{}}
  }}
  states {{ idle, ready, broken }}
  ops {{
    op power_on() -> () {{ CTRL.enable = 1  become ready }}
    op read() when ready -> u32 {{ return TEMP }}
    {dev_ops}
  }}
}}
board demo {{
  soc s {{ memory {{ flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }} clocks {{ sysclk : clock_source = 64MHz }} }}
  t : thermostat at 0x4000_5000
}}
program app {{
  use board demo as b
  let dev = b.t
  cell n : u32 = 0
{reactions}
}}
sim app_sim for app {{ run until 250ms }}
"#
    )
}

#[test]
fn cross_reaction_when_lowers_a_runtime_guard_instead_of_erroring() {
    // `read()` (when ready) is used in a reaction that doesn't establish `ready`
    // (it is configured in another reaction) → compiles now (runtime guard).
    let _ = compile(&rt(
        "",
        "  every 50ms { dev.power_on() }\n  every 100ms { let v = dev.read()  n = n + 1 }",
    ));
}

#[test]
fn runtime_guard_passes_when_the_state_was_established_first() {
    // power_on @50ms arms the device; the read @100/200ms passes → n climbs.
    let t = trace(&rt(
        "",
        "  every 50ms { dev.power_on() }\n  every 100ms { let v = dev.read()  n = n + 1 }",
    ));
    assert!(t.iter().any(|l| l.contains("cell n = 1")), "read should pass once armed:\n{}", t.join("\n"));
}

#[test]
fn runtime_guard_drives_safe_when_the_state_is_not_yet_established() {
    // power_on is configured in a *slow* reaction (1000ms) but the read fires
    // first (100ms) — at runtime the device is still idle, so the guard fires
    // and drives the safe state before `n` is ever incremented.
    let t = trace(&rt(
        "",
        "  every 1000ms { dev.power_on() }\n  every 100ms { let v = dev.read()  n = n + 1 }",
    ));
    assert!(!t.iter().any(|l| l.contains("cell n = 1")), "guard must stop the early read:\n{}", t.join("\n"));
}

#[test]
fn a_when_on_an_unreachable_state_is_still_a_compile_error() {
    // `broken` is declared but no op `become`s it → an op guarded on it can never
    // be callable → compile error (not a runtime guard).
    let errs = resolve_err(&rt(
        "op zap() when broken -> () { }",
        "  every 100ms { dev.zap() }",
    ));
    assert!(
        errs.iter().any(|m| m.contains("no reaction establishes 'broken'")),
        "errs: {:?}",
        errs
    );
}

#[test]
fn metal_emits_the_runtime_state_cell_guard_and_safe_drive() {
    let sir = compile(&rt(
        "",
        "  every 50ms { dev.power_on() }\n  every 100ms { let v = dev.read()  n = n + 1 }",
    ));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    // A per-device runtime state cell exists and `become` writes it.
    assert!(out.contains("_state"), "runtime state cell:\n{}", out);
    // The guard drives the safe state on a mismatch.
    assert!(out.contains("__drive_safe"), "safe-state drive:\n{}", out);
}
