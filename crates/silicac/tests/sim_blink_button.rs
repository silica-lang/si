//! End-to-end test of the reactive-core slice: compile the canonical
//! blink+button program (DESIGN.md §3.4) with the std-lib `gpio` device, run it
//! in the deterministic host simulator, and assert the observable behaviour.

use std::path::{Path, PathBuf};

use silicac::sim::{self, TraceKind};
use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver};

fn repo_path(rel: &str) -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/silicac; the repo root is two levels up.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..").join(rel)
}

/// Compile `examples/blink_button.si` with the std-lib prepended — exactly the
/// pipeline the `silicac` binary runs.
fn compile_demo() -> SirModule {
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("load std");
    let src = std::fs::read_to_string(repo_path("examples/blink_button.si")).expect("read demo");
    let tokens = lexer::lex(&src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    resolver::resolve(&ast).unwrap_or_else(|errs| {
        panic!("resolve failed: {:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>())
    })
}

/// All writes to the LED pad (gpio_a ODR @ 0x14, bit 5): `(time_ns, value)`.
fn led_writes(trace: &[sim::TraceRecord]) -> Vec<(u64, u64)> {
    trace
        .iter()
        .filter_map(|r| match &r.kind {
            TraceKind::RegWrite { offset: 0x14, bit: 5, value, .. } => Some((r.at_ns, *value)),
            _ => None,
        })
        .collect()
}

#[test]
fn led_toggles_from_both_timer_and_button() {
    let sir = compile_demo();
    let result = sim::run(&sir);

    let writes = led_writes(&result.trace);
    // Timer fires at 500/1000/1500/2000/2500ms; the button is injected at
    // 1200/1800ms.  The LED toggles on every one of them, from BOTH sources.
    let expected = [
        (500_000_000u64, 1u64),  // timer
        (1_000_000_000, 0),      // timer
        (1_200_000_000, 1),      // button
        (1_500_000_000, 0),      // timer
        (1_800_000_000, 1),      // button
        (2_000_000_000, 0),      // timer
        (2_500_000_000, 1),      // timer
    ];
    assert_eq!(writes, expected, "LED toggle sequence (time_ns, value)");
}

#[test]
fn every_led_write_is_inside_a_critical_section() {
    // The compiler computed a priority-ceiling critical section for the shared
    // `lit` cell (§5.5); no `disable_irq` appears in the source.  Assert that
    // every shared-cell-touching access (incl. each LED write) executes with a
    // critical section open.
    let sir = compile_demo();
    let result = sim::run(&sir);

    let mut depth = 0i32;
    let mut led_writes_seen = 0;
    for r in &result.trace {
        match &r.kind {
            TraceKind::CriticalEnter { ceiling } => {
                assert_eq!(*ceiling, 2, "ceiling = button-event priority");
                depth += 1;
            }
            TraceKind::CriticalExit => depth -= 1,
            TraceKind::RegWrite { offset: 0x14, bit: 5, .. } => {
                assert!(depth >= 1, "LED write occurred outside a critical section");
                led_writes_seen += 1;
            }
            _ => {}
        }
    }
    assert_eq!(led_writes_seen, 7);
    assert_eq!(depth, 0, "critical sections are balanced");
}

#[test]
fn cell_analysis_marks_lit_shared() {
    let sir = compile_demo();
    let lit = sir.cells.iter().find(|c| c.name == "lit").expect("lit cell analyzed");
    assert!(!lit.single_owner, "lit is touched by two reactions");
    assert_eq!(lit.ceiling, 2);
    assert_eq!(lit.touched_by.len(), 2);
}

#[test]
fn simulation_is_deterministic() {
    // §7.1/D19: the sim has no wall-clock dependence, so two independent runs of
    // the same program produce byte-identical traces.
    let sir = compile_demo();
    let a = sim::run(&sir).render(&sir);
    let b = sim::run(&sir).render(&sir);
    assert_eq!(a, b, "two sim runs must produce identical traces");
}
