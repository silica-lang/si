//! Phase 1 end-to-end (sim): the composed-device keystone (§3.5) — a yielding
//! bus transaction suspends the handler, the scheduler interleaves other work,
//! faults propagate (`?`), and Layer-2 dispositions (retry/skip) act (§4.4/§5.2).

use silicac::sim::{self, TraceKind};
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

#[test]
fn handler_suspends_on_the_bus_and_another_reaction_interleaves() {
    // The `every` sensor read yields at t=1s for the bus latency (2µs); a button
    // is injected 1µs into that window and must run DURING the suspension (§5.2).
    let src = r#"
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  gpio0 : nrf_gpio at 0x5000_0000
  i2c0  : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env   : bme280 { needs { bus = i2c0 } }
  btn   : nrf_gpio.pin = gpio0.pin(11) as input pulling up
}
program app {
  use board demo as b
  let sensor = b.env
  let button = b.btn
  cell hits : u32 = 0
  every 1000ms { let t = sensor.read_temp()? }
  on button.falling { hits = hits + 1 }
}
sim app_sim for app {
  inject btn.falling at 1000001us
  run until 1100ms
}
"#;
    let sir = compile(src);
    let r = sim::run(&sir);
    let t = &r.trace;

    let bus_start = t.iter().position(|x| matches!(x.kind, TraceKind::BusStart { .. })).expect("bus start");
    let bus_done = t.iter().position(|x| matches!(x.kind, TraceKind::BusDone { .. })).expect("bus done");
    let button_fire = t
        .iter()
        .position(|x| matches!(&x.kind, TraceKind::ReactionFire { source, .. } if source.starts_with("event")))
        .expect("button fired");

    assert!(bus_start < button_fire && button_fire < bus_done,
        "button must run during the sensor's bus suspension (start {bus_start} < button {button_fire} < done {bus_done})");
}

#[test]
fn fault_propagates_and_retry_recovers() {
    // First bus transfer NAKs; `?` propagates it to the boundary; `retry(max=3)`
    // re-runs and the second transfer succeeds (§4.4).
    let src = r#"
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  i2c0 : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bme280 { needs { bus = i2c0 } }
}
program app {
  use board demo as b
  let sensor = b.env
  cell samples : u32 = 0
  every 1000ms on fault retry(max = 3) {
    let t = sensor.read_temp()?
    samples = samples + 1
  }
}
sim app_sim for app {
  inject bus_fault nak times 1
  run until 1500ms
}
"#;
    let sir = compile(src);
    let r = sir_run_strings(&sir);
    assert!(r.iter().any(|l| l.contains("FAULT nak")), "the NAK surfaced:\n{}", r.join("\n"));
    assert!(r.iter().any(|l| l.contains("retry 1/3")), "retry fired:\n{}", r.join("\n"));
    assert!(r.iter().any(|l| l.contains("cell samples = 1")), "the read eventually succeeded:\n{}", r.join("\n"));
}

#[test]
fn skip_disposition_drops_the_activation() {
    // With `on fault skip`, an unrecovered fault drops the tick (no completion).
    let src = r#"
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  i2c0 : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bme280 { needs { bus = i2c0 } }
}
program app {
  use board demo as b
  let sensor = b.env
  cell samples : u32 = 0
  every 1000ms on fault skip {
    let t = sensor.read_temp()?
    samples = samples + 1
  }
}
sim app_sim for app {
  inject bus_fault nak times 1
  run until 1500ms
}
"#;
    let sir = compile(src);
    let r = sir_run_strings(&sir);
    assert!(r.iter().any(|l| l.contains("disposition: skip")), "skip fired:\n{}", r.join("\n"));
    // The faulting tick never reached the cell write.
    assert!(!r.iter().any(|l| l.contains("cell samples = 1")), "the skipped tick must not complete:\n{}", r.join("\n"));
}

#[test]
fn cell_critical_spanning_a_yield_would_be_rejected() {
    // The lowering hoists yields out of cell-access critical sections, so a
    // program mixing a shared cell and a yielding read compiles — and the bus
    // transfer is NOT inside any critical section (§5.5/D03 by construction).
    let src = r#"
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  i2c0 : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bme280 { needs { bus = i2c0 } }
}
program app {
  use board demo as b
  let sensor = b.env
  cell shared : u32 = 0
  every 1000ms on fault skip { let t = sensor.read_temp()?   shared = shared + 1 }
  every 700ms { shared = shared + 2 }
}
"#;
    // `shared` is touched by two reactions (shared → ceiling), yet this resolves
    // cleanly: the BusXfer is hoisted out of the critical section.
    let sir = compile(src);
    assert!(sir.reactions.iter().any(|r| r.yields), "the sensor reaction yields");
    assert!(sir.cells.iter().any(|c| c.name == "shared" && !c.single_owner), "shared cell has a ceiling");
}

/// Helper: run the sim and return rendered trace lines.
fn sir_run_strings(sir: &SirModule) -> Vec<String> {
    sim::run(sir).render(sir).lines().map(|s| s.to_string()).collect()
}

#[test]
fn spi_sensor_retry_recovers_over_a_second_bus_interface() {
    // The same composition keystone over `spi` instead of `i2c`: the first
    // transfer times out, `?` propagates it, and `retry(max=3)` recovers — proof
    // the bus machinery (BusXfer, the sim bus model) is generic across bus
    // interfaces, not special-cased to I²C (§3.5/D1).
    let src = r#"
device bmp280_spi {
  needs { bus : spi }
  ops {
    op read_temp() -> u32 or fault{timeout, overrun} yields {
      return bus.read_reg(0, 0xFA)?
    }
  }
}
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  spi0 : spi_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bmp280_spi { needs { bus = spi0 } }
}
program app {
  use board demo as b
  let sensor = b.env
  cell samples : u32 = 0
  every 1000ms on fault retry(max = 3) {
    let t = sensor.read_temp()?
    samples += 1
  }
}
sim app_sim for app {
  inject bus_fault timeout times 1
  run until 1100ms
}
"#;
    let sir = compile(src);
    let r = sim::run(&sir);
    let t = &r.trace;
    // The first transfer faults (BusDone with a `timeout` code) and a retry
    // disposition fires, then a later transfer succeeds (BusDone, code = None).
    let fault = t
        .iter()
        .position(|x| matches!(&x.kind, TraceKind::BusDone { code: Some(c), .. } if c == "timeout"))
        .expect("timeout fault");
    assert!(
        t.iter().any(|x| matches!(&x.kind, TraceKind::Dispose { action, .. } if action.contains("retry"))),
        "a retry disposition fires:\n{:#?}", t
    );
    let ok = t
        .iter()
        .rposition(|x| matches!(&x.kind, TraceKind::BusDone { code: None, .. }))
        .expect("successful transfer");
    assert!(fault < ok, "the timeout fault is followed by a successful retry:\n{:#?}", t);
    // And the sample was ultimately taken (retry recovered).
    assert!(
        t.iter().any(|x| matches!(&x.kind, TraceKind::CellWrite { value, .. } if *value == 1)),
        "samples reaches 1 after retry recovery:\n{:#?}", t
    );
}
