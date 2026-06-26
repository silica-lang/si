//! §4.1/D18 — interface semantic-property checks.  A controller declares the
//! property values it `provides`; a device declares what it requires via
//! `needs { bus : i2c where … }`; the resolver const-evaluates the requirement
//! against the provider's properties at board-bind time.

use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver};

fn with_std(src: &str) -> silicac::ast::Module {
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("std");
    let tokens = lexer::lex(src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    ast
}

fn compile(src: &str) -> SirModule {
    resolver::resolve(&with_std(src))
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()))
}

fn resolve_err(src: &str) -> Vec<String> {
    match resolver::resolve(&with_std(src)) {
        Ok(_) => panic!("expected a resolve error, got success"),
        Err(e) => e.iter().map(|d| d.msg.clone()).collect(),
    }
}

/// A program whose sensor device requires the bus property `<constraint>`.
fn program(constraint: &str) -> String {
    format!(
        r#"
device fast_sensor {{
  needs {{ bus : i2c where {constraint} }}
  ops {{
    op read() -> u32 or fault{{nak, timeout, arblost}} yields {{
      let v = bus.read_reg(0x40, 0x00)?
      return v
    }}
  }}
}}
board demo {{
  soc s {{ memory {{ flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }} clocks {{ sysclk : clock_source = 64MHz }} }}
  i2c0 : i2c_controller at 0x4000_3000 {{ needs {{ clock = soc.sysclk }} }}
  env  : fast_sensor {{ needs {{ bus = i2c0 }} }}
}}
program app {{
  use board demo as b
  let sensor = b.env
  cell n : u32 = 0
  every 100ms {{ let v = sensor.read()?  n = n + 1 }}
}}
sim app_sim for app {{ run until 150ms }}
"#
    )
}

#[test]
fn a_satisfied_property_constraint_compiles() {
    // The std controller provides max_speed = 400_000, addressing = 7.
    let _ = compile(&program("max_speed >= 400_000 and addressing == 7"));
}

#[test]
fn a_speed_the_controller_cannot_meet_is_a_compile_error() {
    let errs = resolve_err(&program("max_speed >= 1_000_000"));
    assert!(errs.iter().any(|m| m.contains("does not satisfy")), "errs: {:?}", errs);
}

#[test]
fn a_ten_bit_addressing_requirement_is_rejected() {
    let errs = resolve_err(&program("addressing == 10"));
    assert!(errs.iter().any(|m| m.contains("does not satisfy")), "errs: {:?}", errs);
}

#[test]
fn constraining_an_undeclared_property_is_rejected() {
    let errs = resolve_err(&program("clock_stretch == 1"));
    assert!(
        errs.iter().any(|m| m.contains("does not declare") && m.contains("clock_stretch")),
        "errs: {:?}",
        errs
    );
}
