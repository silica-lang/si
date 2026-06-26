//! §5.1/D02 — event-source overflow policy.  A reaction has one in-flight
//! activation; when its event re-fires while in-flight, the declared policy
//! applies: `coalesce` (default), `drop_newest`, or `fault` (safe-state).

use silicac::backend::{c, Target};
use silicac::sim;
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

fn trace(src: &str) -> Vec<String> {
    let sir = compile(src);
    sim::run(&sir).render(&sir).lines().map(|s| s.to_string()).collect()
}

/// A sensor read that yields on the bus (~2µs) but fires every 1µs, so the second
/// tick arrives while the first is still suspended.  `<clause>` sets the policy.
fn program(clause: &str) -> String {
    format!(
        r#"
board demo {{
  soc s {{ memory {{ flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }} clocks {{ sysclk : clock_source = 64MHz }} }}
  i2c0 : i2c_controller at 0x4000_3000 {{ needs {{ clock = soc.sysclk }} }}
  env  : bme280 {{ needs {{ bus = i2c0 }} }}
}}
program app {{
  use board demo as b
  let sensor = b.env
  cell samples : u32 = 0
  every 1us {clause} {{ let t = sensor.read_temp()?  samples = samples + 1 }}
}}
sim app_sim for app {{ run until 10us }}
"#
    )
}

#[test]
fn fault_policy_raises_event_overflow_and_stops() {
    let t = trace(&program("on overflow fault"));
    assert!(
        t.iter().any(|l| l.contains("EVENT OVERFLOW") && l.contains("policy=fault")),
        "expected an event-overflow fault:\n{}",
        t.join("\n")
    );
    // The system stopped: the suspended read never resumes (no BusDone).
    assert!(!t.iter().any(|l| l.contains("done, resume")), "system should have stopped:\n{}", t.join("\n"));
}

#[test]
fn drop_newest_discards_the_refire_but_keeps_running() {
    let t = trace(&program("on overflow drop_newest"));
    assert!(
        t.iter().any(|l| l.contains("EVENT OVERFLOW") && l.contains("policy=drop-newest")),
        "expected a drop-newest overflow:\n{}",
        t.join("\n")
    );
    // The original activation still completes (the bus resumes).
    assert!(t.iter().any(|l| l.contains("done, resume")), "the in-flight read should resume:\n{}", t.join("\n"));
}

#[test]
fn coalesce_is_the_default_and_keeps_running() {
    let t = trace(&program("")); // no policy clause → coalesce
    assert!(t.iter().any(|l| l.contains("re-fire coalesced")), "default should coalesce:\n{}", t.join("\n"));
    assert!(!t.iter().any(|l| l.contains("EVENT OVERFLOW")), "coalesce is not an overflow fault:\n{}", t.join("\n"));
    assert!(t.iter().any(|l| l.contains("done, resume")), "the read should resume:\n{}", t.join("\n"));
}

#[test]
fn explicit_coalesce_matches_the_default() {
    let t = trace(&program("on overflow coalesce"));
    assert!(t.iter().any(|l| l.contains("re-fire coalesced")), "explicit coalesce:\n{}", t.join("\n"));
}

#[test]
fn metal_emits_the_fault_overflow_branch() {
    let sir = compile(&program("on overflow fault"));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("fault: event overflow"), "metal fault-overflow branch:\n{}", out);
    assert!(out.contains("__drive_safe();"), "metal drives safe-state on overflow:\n{}", out);
}
