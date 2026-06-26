//! §4.5/§5.6 — a reaction's `within <d>` deadline budget: if the reaction does
//! not return to idle within `d` of firing, it overruns and the watchdog resets
//! the system (the language-level effect is a `DeadlineMissed` reset).

use silicac::sim::{self, TraceKind};
use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver};

fn run(within: &str) -> Vec<TraceKind> {
    let src = format!(
        r#"
board rig {{
  soc s {{ memory {{ flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }} clocks {{ sysclk : clock_source = 64MHz }} }}
  i2c0 : i2c_controller at 0x4000_3000 {{ needs {{ clock = soc.sysclk }} }}
  env  : bme280 {{ needs {{ bus = i2c0 }} }}
}}
program app {{
  use board rig as r
  let sensor = r.env
  cell samples : u32 = 0
  every 1000ms within {within} {{ let t = sensor.read_temp()?   samples = samples + 1 }}
}}
sim app_sim for app {{ run until 1100ms }}
"#
    );
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("std");
    let tokens = lexer::lex(&src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    let sir: SirModule = resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()));
    // The deadline reached the SIR.
    assert!(sir.reactions.iter().any(|r| r.deadline_ns.is_some()), "within lowered to a deadline");
    sim::run(&sir).trace.into_iter().map(|r| r.kind).collect()
}

#[test]
fn overrunning_the_within_budget_resets() {
    // The bus read takes ~2µs; a 1µs budget is overrun → reset, and the post-bus
    // work (samples) never runs.
    let kinds = run("1us");
    assert!(
        kinds.iter().any(|k| matches!(k, TraceKind::DeadlineMissed { reaction: 0, .. })),
        "a 1µs deadline must be missed by a 2µs bus read: {:#?}", kinds
    );
    assert!(
        !kinds.iter().any(|k| matches!(k, TraceKind::CellWrite { name, value } if name == "samples" && *value == 1)),
        "the overrun reaction must not complete: {:#?}", kinds
    );
}

#[test]
fn meeting_the_within_budget_completes_normally() {
    // A 5ms budget comfortably covers the 2µs bus read → no overrun, sample taken.
    let kinds = run("5ms");
    assert!(
        !kinds.iter().any(|k| matches!(k, TraceKind::DeadlineMissed { .. })),
        "a 5ms deadline must not be missed: {:#?}", kinds
    );
    assert!(
        kinds.iter().any(|k| matches!(k, TraceKind::CellWrite { name, value } if name == "samples" && *value == 1)),
        "the reaction completes within budget: {:#?}", kinds
    );
}
