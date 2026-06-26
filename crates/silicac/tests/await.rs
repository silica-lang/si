//! §3.2/§5.2 — `await <cond> within <d> else fault <code>`, the suspending
//! sibling of `poll`.  The handler yields; the condition is re-checked on a
//! cadence until it holds (resume) or the budget elapses (raise the fault to the
//! reaction's Layer-2 disposition).

use silicac::backend::{c, Target};
use silicac::sim;
use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver};

fn compile(src: &str) -> SirModule {
    let tokens = lexer::lex(src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()))
}

fn resolve_err(src: &str) -> Vec<String> {
    let tokens = lexer::lex(src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    match resolver::resolve(&ast) {
        Ok(_) => panic!("expected a resolve error, got success"),
        Err(e) => e.iter().map(|d| d.msg.clone()).collect(),
    }
}

fn trace(src: &str) -> Vec<String> {
    let sir = compile(src);
    sim::run(&sir).render(&sir).lines().map(|s| s.to_string()).collect()
}

const BOARD: &str = r#"
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
}
"#;

#[test]
fn await_times_out_when_the_condition_never_holds() {
    // No setter → `ready` stays 0 → the budget elapses → fault → `skip` drops the
    // activation, so the post-await `done` is never reached.
    let t = trace(&format!(
        "{BOARD}\nprogram app {{\n  use board demo as b\n  cell ready : u32 = 0\n  cell done : u32 = 0\n  every 100ms on fault skip {{ await ready == 1 within 50ms else fault timeout  done = done + 1 }}\n}}\nsim s for app {{ run until 250ms }}\n"
    ));
    assert!(t.iter().any(|l| l.contains("await — timeout") && l.contains("timeout")), "expected a timeout:\n{}", t.join("\n"));
    assert!(!t.iter().any(|l| l.contains("cell done = 1")), "skip should drop the activation:\n{}", t.join("\n"));
}

#[test]
fn await_suspends_then_resumes_when_another_reaction_sets_the_condition() {
    let t = trace(&format!(
        "{BOARD}\nprogram app {{\n  use board demo as b\n  cell ready : u32 = 0\n  cell done : u32 = 0\n  every 100ms on fault skip {{ await ready == 1 within 500ms else fault not_ready  done = done + 1 }}\n  every 130ms {{ ready = 1 }}\n}}\nsim s for app {{ run until 300ms }}\n"
    ));
    assert!(t.iter().any(|l| l.contains("await — condition met")), "await should resume:\n{}", t.join("\n"));
    assert!(t.iter().any(|l| l.contains("cell done = 1")), "post-await work runs after resume:\n{}", t.join("\n"));
}

#[test]
fn await_actually_yields_resume_is_later_than_the_fire() {
    // The first activation fires at 100ms but cannot resume until the condition is
    // set at 130ms, so the resume timestamp is strictly after the fire — proof it
    // suspended rather than busy-waited or resolved synchronously.
    let t = trace(&format!(
        "{BOARD}\nprogram app {{\n  use board demo as b\n  cell ready : u32 = 0\n  cell done : u32 = 0\n  every 100ms on fault skip {{ await ready == 1 within 500ms else fault not_ready  done = done + 1 }}\n  every 130ms {{ ready = 1 }}\n}}\nsim s for app {{ run until 200ms }}\n"
    ));
    let resume_ms: f64 = t
        .iter()
        .find(|l| l.contains("await — condition met"))
        .and_then(|l| l.split("ms]").next())
        .and_then(|p| p.trim_start_matches('[').trim().parse().ok())
        .expect("a resume line");
    assert!(resume_ms > 100.0, "resume {resume_ms}ms must be after the 100ms fire (it suspended)");
}

#[test]
fn await_inside_atomic_is_rejected() {
    let errs = resolve_err(&format!(
        "{BOARD}\nprogram app {{\n  use board demo as b\n  cell ready : u32 = 0\n  every 100ms {{ atomic {{ await ready == 1 within 50ms else fault t }} }}\n}}\nsim s for app {{ run until 150ms }}\n"
    ));
    assert!(
        errs.iter().any(|m| m.contains("atomic") && (m.contains("await") || m.contains("suspension"))),
        "errs: {:?}",
        errs
    );
}

#[test]
fn metal_emits_the_bounded_recheck_loop() {
    let sir = compile(&format!(
        "{BOARD}\nprogram app {{\n  use board demo as b\n  cell ready : u32 = 0\n  every 100ms on fault skip {{ await ready == 1 within 50ms else fault timeout  ready = ready }}\n}}\nsim s for app {{ run until 150ms }}\n"
    ));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("bounded re-check"), "await re-check loop:\n{}", out);
    assert!(out.contains("__faulted = 1U"), "times out into the fault path:\n{}", out);
}
