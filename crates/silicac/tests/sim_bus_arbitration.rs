//! P6-9 (§5.2) — multi-consumer bus arbitration in the simulator (the oracle).
//!
//! Two reactions reading two sensors on the SAME I²C controller contend for the
//! one bus.  A low-priority `every` owns the bus first; a higher-priority button
//! contends mid-transfer, BLOCKS (joins the waiter queue), and is GRANTED the bus
//! the instant the first transfer completes — priority-ordered, no read lost.

use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver, sim};

fn resolve_str(src: &str) -> SirModule {
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("std");
    let tokens = lexer::lex(src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()))
}

const PROG: &str = r#"
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  gpio0 : nrf_gpio at 0x5000_0000
  i2c0  : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env_a : bme280 { needs { bus = i2c0 } }
  env_b : bme280 { needs { bus = i2c0 } }
  btn   : nrf_gpio.pin = gpio0.pin(11) as input pulling up
}
program app {
  use board demo as b
  let sensor_a = b.env_a
  let sensor_b = b.env_b
  let button   = b.btn
  cell sa : u32 = 0
  cell sb : u32 = 0
  every 1000ms { let t = sensor_a.read_temp()?   sa = sa + 1 }
  on button.falling { let t = sensor_b.read_temp()?   sb = sb + 1 }
}
sim app_sim for app {
  inject btn.falling at 1000001us
  run until 1100ms
}
"#;

fn lines(sir: &SirModule) -> Vec<String> {
    sim::run(sir).render(sir).lines().map(str::to_string).collect()
}

#[test]
fn contending_reactions_block_then_get_granted_priority_ordered() {
    let sir = resolve_str(PROG);
    let ls = lines(&sir);
    let joined = ls.join("\n");

    // The low-priority `every` (reaction#0) claims the bus first at 1000ms.
    assert!(
        ls.iter().any(|l| l.contains("bus i2c0.read_reg() — suspend") && l.contains("1000.000ms")),
        "the every reaction must claim the bus at 1000ms:\n{joined}"
    );
    // The higher-priority button (reaction#1) contends 1µs later and BLOCKS.
    assert!(joined.contains("BLOCKED reaction#1"), "the button must block on the busy bus:\n{joined}");
    // When the first transfer completes the bus is GRANTED to the waiter.
    assert!(joined.contains("GRANTED reaction#1"), "the waiter must be granted the freed bus:\n{joined}");
    // BLOCKED precedes GRANTED for that reaction.
    let blocked = ls.iter().position(|l| l.contains("BLOCKED reaction#1")).unwrap();
    let granted = ls.iter().position(|l| l.contains("GRANTED reaction#1")).unwrap();
    assert!(blocked < granted, "block must precede grant:\n{joined}");

    // BOTH reads complete — neither is lost (the whole point of arbitration).
    assert!(
        ls.iter().any(|l| l.contains("cell sa = 1")),
        "the every's read must complete (sa=1):\n{joined}"
    );
    assert!(
        ls.iter().any(|l| l.contains("cell sb = 1")),
        "the button's read must complete (sb=1):\n{joined}"
    );
}
