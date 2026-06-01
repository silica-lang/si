//! Stage 1 (Phase 1): interfaces + composed-device op lowering (§3.5).
//! These inspect the resolved SIR — they don't need the sim's suspension model.

use silicac::sir::{SirModule, SirStmt, SirDisposition, SirExpr};
use silicac::{lexer, parser, resolver};

const COMPOSED: &str = r#"
interface i2c {
  op read_reg24(addr: u32, reg: u32) -> u32 or fault{nak} yields
}
device i2c0_ctrl implements i2c {
  ops { op read_reg24(addr: u32, reg: u32) -> u32 or fault{nak} yields }
}
device bme280 {
  needs { bus : i2c }
  ops {
    op read_temp() -> u32 or fault{nak} yields {
      return bus.read_reg24(0x76, 0xFA)?
    }
  }
}
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  i2c0 : i2c0_ctrl
  env  : bme280 { needs { bus = i2c0 } }
}
program app {
  use board demo as b
  let sensor = b.env
  every 1000ms on fault retry(max = 3) {
    let t = sensor.read_temp()?
  }
}
"#;

fn resolve_str(src: &str) -> Result<SirModule, Vec<silicac::diag::Diag>> {
    let tokens = lexer::lex(src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    resolver::resolve(&ast)
}

fn find_busxfer(stmts: &[SirStmt]) -> Option<&SirStmt> {
    stmts.iter().find_map(|s| match s {
        SirStmt::BusXfer { .. } => Some(s),
        SirStmt::Critical { body, .. } | SirStmt::If { then: body, .. } => find_busxfer(body),
        _ => None,
    })
}

#[test]
fn composed_op_lowers_to_a_bus_transaction_on_the_substrate() {
    let sir = resolve_str(COMPOSED)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()));

    let r = &sir.reactions[0];
    assert!(r.yields, "the reaction suspends on the bus transaction");
    assert_eq!(r.disposition, SirDisposition::Retry { max: 3 });

    // `sensor.read_temp()` inlined to a transfer on the i2c0 controller.
    let i2c0_id = sir.devices.iter().find(|d| d.name == "i2c0").expect("i2c0 device").id;
    let bx = find_busxfer(&r.body).expect("a BusXfer was emitted");
    match bx {
        SirStmt::BusXfer { device, op, args, propagate, fault_codes, .. } => {
            assert_eq!(*device, i2c0_id, "transaction targets the bound i2c0 controller");
            assert_eq!(op, "read_reg24");
            assert!(*propagate, "the `?` propagates the fault");
            assert_eq!(fault_codes, &vec!["nak".to_string()]);
            assert!(matches!(args.as_slice(), [SirExpr::U64(0x76), SirExpr::U64(0xFA)]));
        }
        _ => unreachable!(),
    }
}

#[test]
fn missing_interface_op_fails_conformance() {
    let src = r#"
interface i2c { op read_reg24(addr: u32, reg: u32) -> u32 or fault{nak} yields }
device bad implements i2c {
  ops { op write_reg(addr: u32, reg: u32, val: u32) -> () {} }
}
program p { on sys.start { } }
"#;
    let errs = resolve_str(src).expect_err("expected a conformance error");
    assert!(
        errs.iter().any(|e| e.msg.contains("missing op `read_reg24")),
        "got: {:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>()
    );
}

#[test]
fn conformance_checks_yields_and_fallibility() {
    // read_reg matches name+arity but is neither yielding nor fallible → reject.
    let src = r#"
interface i2c { op read_reg(addr: u32, reg: u32) -> u32 or fault{nak} yields }
device bad implements i2c {
  ops { op read_reg(addr: u32, reg: u32) -> u32 {} }
}
program p { on sys.start { } }
"#;
    let errs = resolve_str(src).expect_err("expected a signature-mismatch error");
    assert!(
        errs.iter().any(|e| e.msg.contains("does not match interface")),
        "got: {:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>()
    );
}

#[test]
fn undeclared_bus_fault_code_is_an_error() {
    let src = r#"
interface i2c { op read_reg(addr: u32, reg: u32) -> u32 or fault{nak} yields }
device i2c_ctrl implements i2c { ops { op read_reg(addr: u32, reg: u32) -> u32 or fault{nak} yields } }
device bme280 { needs { bus : i2c } ops { op read_temp() -> u32 or fault{nak} yields { return bus.read_reg(0x76, 0xFA)? } } }
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  i2c0 : i2c_ctrl
  env  : bme280 { needs { bus = i2c0 } }
}
program app { use board demo as b  let sensor = b.env  every 1000ms on fault skip { let t = sensor.read_temp()? } }
sim app_sim for app { inject bus_fault bogus times 1   run until 1500ms }
"#;
    let errs = resolve_str(src).expect_err("expected an undeclared-code error");
    assert!(
        errs.iter().any(|e| e.msg.contains("not declared by any op")),
        "got: {:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>()
    );
}

#[test]
fn bus_need_must_be_satisfied_by_an_implementing_device() {
    // `env.bus` requires an i2c provider, but `not_i2c` doesn't implement it.
    let src = r#"
interface i2c { op read_reg24(addr: u32, reg: u32) -> u32 or fault{nak} yields }
device not_i2c { ops { op nop() -> () {} } }
device bme280 { needs { bus : i2c } ops { op read_temp() -> u32 or fault{nak} yields { return bus.read_reg24(0x76, 0xFA)? } } }
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } }
  dev0 : not_i2c
  env  : bme280 { needs { bus = dev0 } }
}
program p { use board demo as b  let s = b.env  on sys.start { } }
"#;
    let errs = resolve_str(src).expect_err("expected a bus-conformance error");
    assert!(
        errs.iter().any(|e| e.msg.contains("does not implement")),
        "got: {:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>()
    );
}
