//! §4.5/§5.6 — on-metal enforcement of a reaction `within <d>` deadline.  A
//! yielding reaction with a deadline and a board watchdog gets a per-reaction
//! countdown: armed on fire, ticked by SysTick, and an overrun latches
//! `__deadline_missed`, which gates off the watchdog feed (→ reset).

use silicac::backend::{c, Target};
use silicac::{lexer, parser, resolver};

fn metal_c(src: &str) -> String {
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("std");
    let tokens = lexer::lex(src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    let sir = resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()));
    c::CBackend::with_target(Target::MetalNrf52840).emit(&sir)
}

const BOARD: &str = r#"
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  i2c0 : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bme280 { needs { bus = i2c0 } }
  WDT
}
"#;

fn program(within: &str, wdt: bool) -> String {
    let board = BOARD.replace(
        "  WDT\n",
        if wdt { "  wdt0 : wdt at 0x4001_0000 { config { timeout = 100ms } }\n" } else { "" },
    );
    format!(
        "{board}\nprogram app {{\n  use board demo as b\n  let sensor = b.env\n  cell n : u32 = 0\n  every 100ms {within} {{ let t = sensor.read_temp()?  n = n + 1 }}\n}}\nsim app_sim for app {{ run until 350ms }}\n"
    )
}

#[test]
fn a_deadline_with_a_watchdog_emits_the_countdown_machinery() {
    let out = metal_c(&program("within 30ms", true));
    assert!(out.contains("__deadline_0 = 30U"), "arm to 30 ticks:\n{}", out);
    assert!(out.contains("__deadline_missed = 1U"), "latch on overrun:\n{}", out);
    // The watchdog feed is gated on the deadline flag.
    assert!(out.contains("!__deadline_missed"), "feed gated on deadline:\n{}", out);
    // Disarm when the reaction is back to idle.
    assert!(out.contains("__rf_0.__state == 0U) { __deadline_0 = 0U; }"), "disarm on idle:\n{}", out);
}

#[test]
fn no_deadline_emits_no_countdown() {
    let out = metal_c(&program("", true));
    assert!(!out.contains("__deadline_"), "no deadline machinery without `within`:\n{}", out);
}

#[test]
fn a_deadline_without_a_watchdog_is_not_enforced_on_metal() {
    // The reset mechanism is watchdog starvation; with no watchdog there is no
    // metal enforcement (the sim still enforces it).  No countdown is emitted.
    let out = metal_c(&program("within 30ms", false));
    assert!(!out.contains("__deadline_missed"), "no enforcement without a watchdog:\n{}", out);
}
