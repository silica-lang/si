//! Phase 1b: safe-state (§5.6).  On an unrecovered fault, the `safe` disposition
//! drives every device to its declared safe state by running its bounded,
//! non-yielding `safe` op (real register writes via leaf op-body lowering).

use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver, sim};

fn resolve_str(src: &str) -> Result<SirModule, Vec<silicac::diag::Diag>> {
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("std");
    let tokens = lexer::lex(src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    resolver::resolve(&ast)
}

const RIG: &str = r#"
device motor {
  regs { CTRL : reg32 at 0x00 access rw { enable: bit[0] } }
  states { running, off }
  safe_state = off
  ops {
    op run()  -> () { CTRL.enable = 1 }
    op safe() -> () { CTRL.enable = 0 }
  }
}
board rig {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  i2c0   : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env    : bme280 { needs { bus = i2c0 } }
  motor0 : motor at 0x5001_0000
}
program ctrl {
  use board rig as r
  let sensor = r.env
  let m      = r.motor0
  on sys.start { m.run() }
  every 1000ms on fault safe { let t = sensor.read_temp()? }
}
sim ctrl_sim for ctrl {
  inject bus_fault nak times 1
  run until 1500ms
}
"#;

#[test]
fn safe_disposition_drives_the_actuator_to_its_safe_state() {
    let sir = resolve_str(RIG)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()));
    let lines: Vec<String> = sim::run(&sir).render(&sir).lines().map(str::to_string).collect();
    let joined = lines.join("\n");

    // The motor is energized at boot, then driven off when the sensor faults.
    assert!(joined.contains("write motor0.reg(0x0).bit(0) = 1"), "energized at boot:\n{joined}");
    assert!(joined.contains("disposition: safe (nak)"), "safe disposition fired:\n{joined}");
    assert!(joined.contains("write motor0.reg(0x0).bit(0) = 0"), "safe op de-energized the motor:\n{joined}");
    assert!(joined.contains("SAFE-STATE: motor0 -> off"), "driven to declared safe state:\n{joined}");

    // The de-energize must come after the energize (ordering).
    let on = lines.iter().position(|l| l.contains("bit(0) = 1")).unwrap();
    let off = lines.iter().position(|l| l.contains("bit(0) = 0")).unwrap();
    assert!(on < off, "safe drive must follow the energize");
}

#[test]
fn a_safe_op_may_not_yield() {
    let src = r#"
device bad {
  regs { CTRL : reg32 at 0x00 access rw {} }
  safe_state = off
  ops { op safe() -> () yields }
}
board rig {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } }
  d0 : bad at 0x5001_0000
}
program p { use board rig as r  let x = r.d0  on sys.start { } }
"#;
    let errs = resolve_str(src).expect_err("expected a safe-op-yields error");
    assert!(
        errs.iter().any(|e| e.msg.contains("`safe` op may not yield")),
        "got: {:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>()
    );
}
