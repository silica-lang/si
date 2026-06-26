//! §3.6 — typed overlays.  A `set`/`remove` patch over a board, applied and
//! type-checked at compile time: `set` must name a real config field and satisfy
//! its `where` constraint; `remove` must name an entity that exists.

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

/// A board with a watchdog (timeout 50ms) + a spare sensor, optionally patched by
/// `<overlay>`, plus a trivial program/sim that builds it.
fn program(overlay: &str) -> String {
    format!(
        r#"
board base {{
  soc s {{ memory {{ flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }} clocks {{ sysclk : clock_source = 64MHz }} }}
  i2c0  : i2c_controller at 0x4000_3000 {{ needs {{ clock = soc.sysclk }} }}
  env   : bme280 {{ needs {{ bus = i2c0 }} }}
  spare : bme280 {{ needs {{ bus = i2c0 }} }}
  wdt0  : wdt at 0x4001_0000 {{ config {{ timeout = 50ms }} }}
}}
{overlay}
program app {{
  use board base as b
  let sensor = b.env
  cell n : u32 = 0
  every 100ms {{ let t = sensor.read_temp()?  n = n + 1 }}
}}
sim app_sim for app {{ run until 150ms }}
"#
    )
}

#[test]
fn set_overrides_a_config_value() {
    // Without an overlay the watchdog window is 50ms; the overlay retunes it.
    let base = compile(&program(""));
    assert_eq!(base.watchdog_timeout_ns, Some(50_000_000));
    let tuned = compile(&program("overlay t for board.base { set wdt0.config.timeout = 200ms }"));
    assert_eq!(tuned.watchdog_timeout_ns, Some(200_000_000), "overlay should retune the watchdog");
}

#[test]
fn remove_drops_the_named_instance() {
    let base = compile(&program(""));
    assert!(base.devices.iter().any(|d| d.name == "spare"), "spare present without overlay");
    let patched = compile(&program("overlay t for board.base { remove spare }"));
    assert!(!patched.devices.iter().any(|d| d.name == "spare"), "spare should be removed");
}

#[test]
fn set_violating_the_where_constraint_is_rejected() {
    // The wdt `timeout` constraint is `> 0ns and <= 1s`; 2s violates it.
    let errs = resolve_err(&program("overlay t for board.base { set wdt0.config.timeout = 2s }"));
    assert!(errs.iter().any(|m| m.contains("where") && m.contains("constraint")), "errs: {:?}", errs);
}

#[test]
fn set_on_an_unknown_config_field_is_rejected() {
    let errs = resolve_err(&program("overlay t for board.base { set wdt0.config.nonsense = 1 }"));
    assert!(errs.iter().any(|m| m.contains("no config field")), "errs: {:?}", errs);
}

#[test]
fn removing_a_nonexistent_entity_is_rejected() {
    let errs = resolve_err(&program("overlay t for board.base { remove ghost }"));
    assert!(errs.iter().any(|m| m.contains("has no 'ghost'")), "errs: {:?}", errs);
}

#[test]
fn an_overlay_targeting_an_unknown_board_is_rejected() {
    let errs = resolve_err(&program("overlay t for board.nope { remove spare }"));
    assert!(errs.iter().any(|m| m.contains("unknown board")), "errs: {:?}", errs);
}
