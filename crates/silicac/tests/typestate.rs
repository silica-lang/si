//! §4.1/D07 — static `when`/`become` typestate.  A `when S` op is callable only
//! when a dominating `become S` proves the device is in state S; otherwise it is
//! a compile error.

use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver};

fn compile(src: &str) -> SirModule {
    let tokens = lexer::lex(src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()))
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
