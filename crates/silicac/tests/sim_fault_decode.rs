//! Layer-3 fault decoding (§4.4/§5.4): a forced hardware fault is decoded
//! against the board's address-ownership map into a language-level diagnosis and
//! shows up as a structured trace record (the Phase-0 "forced fault → decoded
//! trace record" success criterion, §11).

use silicac::sim::{self, TraceKind};
use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver};

const SRC: &str = r#"
board b {
  soc s {
    memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }
    clocks { sysclk : clock_source = 64MHz }
  }
  gpio0 : nrf_gpio at 0x5000_0000
  led : nrf_gpio.pin = gpio0.pin(13) as output
}
program p {
  use board b as dev
  let led = dev.led
  every 500ms { led.set(true) }
}
sim p_sim for p {
  inject fault 0x4001_0000 at 800ms    // unclaimed
  inject fault 0x5000_0504 at 1200ms   // gpio0 MMIO
  run until 1500ms
}
"#;

fn compile(src: &str) -> SirModule {
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("std");
    let tokens = lexer::lex(src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()))
}

#[test]
fn forced_faults_are_decoded_to_language_level() {
    let sir = compile(SRC);
    let result = sim::run(&sir);

    let faults: Vec<(u64, String)> = result
        .trace
        .iter()
        .filter_map(|r| match &r.kind {
            TraceKind::Fault { address, diagnosis } => Some((*address, diagnosis.clone())),
            _ => None,
        })
        .collect();

    assert_eq!(faults.len(), 2, "two injected faults decoded");

    // Unclaimed address — the canonical §5.4 diagnosis.
    let (addr0, diag0) = &faults[0];
    assert_eq!(*addr0, 0x4001_0000);
    assert!(diag0.contains("no device claims this address"), "got: {diag0}");

    // Address inside a device's MMIO — attributed to the owning device.
    let (addr1, diag1) = &faults[1];
    assert_eq!(*addr1, 0x5000_0504);
    assert!(diag1.contains("within device `gpio0`"), "got: {diag1}");
}
