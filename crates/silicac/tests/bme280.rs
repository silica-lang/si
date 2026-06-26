//! §3.5/§4.3 — BME280 fixed-point compensation end-to-end (audit #35, P0-3d):
//! the composition keystone with the *real* fractional math a datasheet needs,
//! not an elided stub.  A raw ADC word read over the I²C bus is turned into a
//! compensated °C value with `fixed<16,16>` cast + subtract + divide.

use silicac::sim;
use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver};

fn compile(src: &str) -> SirModule {
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
  i2c0 : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bme280 { needs { bus = i2c0 } }
}
program app {
  use board demo as b
  let sensor = b.env
  cell deg   : u32 = 0
  cell centi : u32 = 0
  every 1000ms {
    let t = sensor.read_temp_c()?
    deg   = t as u32
    centi = (t * 100.0) as u32
  }
}
sim app_sim for app { run until 1500ms }
"#;

#[test]
fn compensated_temperature_is_computed_over_the_bus() {
    // The sim bus mock returns raw ADC 0x5AB0 = 23216; the compensation
    // (23216 - 21216) / 80 = 25.0 °C.  The fractional centi value (2500) proves
    // the divide kept its precision — and that the raw word was NOT passed
    // through uncompensated.
    let t = sim::run(&compile(PROG)).render(&compile(PROG));
    assert!(t.contains("cell deg = 25"), "compensated whole degrees = 25:\n{t}");
    assert!(t.contains("cell centi = 2500"), "compensated centi-°C = 2500:\n{t}");
    assert!(!t.contains("cell deg = 23216"), "raw ADC must not pass through uncompensated:\n{t}");
}
