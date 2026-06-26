//! §4.5 — the `instant` / `duration` time model and `now()`.  `instant` is a
//! distinct type (a point in time); the resolver enforces the arithmetic that is
//! physically meaningful and rejects the rest.  Both are `u64` ns at runtime.

use silicac::sim;
use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver};

fn compile(src: &str) -> SirModule {
    let tokens = lexer::lex(src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()))
}

/// Resolve, expecting failure; return the diagnostic messages.
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

fn program(body: &str) -> String {
    format!("{BOARD}\nprogram app {{\n  use board demo as b\n{body}\n}}\nsim app_sim for app {{ run until 350ms }}\n")
}

#[test]
fn now_minus_instant_yields_the_elapsed_duration() {
    // Each tick records its instant and reports the gap since the last — an
    // `instant - instant` that settles at the 100ms cadence.
    let t = trace(&program(
        "  cell last : instant = 0\n  cell period : duration = 0\n  every 100ms {\n    period = now() - last\n    last = now()\n  }",
    ));
    // now() reads virtual time, so the second tick onward measures exactly 100ms.
    assert!(
        t.iter().filter(|l| l.contains("cell period = 100000000")).count() >= 2,
        "expected period to settle at 100ms:\n{}",
        t.join("\n")
    );
}

#[test]
fn now_reads_the_current_virtual_time() {
    let t = trace(&program(
        "  cell stamp : instant = 0\n  every 100ms {\n    stamp = now()\n  }",
    ));
    assert!(t.iter().any(|l| l.contains("cell stamp = 100000000")), "tick1 stamp:\n{}", t.join("\n"));
    assert!(t.iter().any(|l| l.contains("cell stamp = 300000000")), "tick3 stamp:\n{}", t.join("\n"));
}

#[test]
fn adding_two_instants_is_rejected() {
    let errs = resolve_err(&program(
        "  cell a : instant = 0\n  cell b : instant = 0\n  cell c : instant = 0\n  every 100ms {\n    c = a + b\n  }",
    ));
    assert!(errs.iter().any(|m| m.contains("add two instants")), "errs: {:?}", errs);
}

#[test]
fn comparing_an_instant_with_a_scalar_is_rejected() {
    let errs = resolve_err(&program(
        "  cell flag : u32 = 0\n  every 100ms {\n    flag = now() > 5\n  }",
    ));
    assert!(
        errs.iter().any(|m| m.contains("compare an instant with a non-instant")),
        "errs: {:?}",
        errs
    );
}

#[test]
fn assigning_an_instant_to_a_scalar_cell_is_rejected() {
    let errs = resolve_err(&program(
        "  cell c : u32 = 0\n  every 100ms {\n    c = now()\n  }",
    ));
    assert!(
        errs.iter().any(|m| m.contains("instant and non-instant")),
        "errs: {:?}",
        errs
    );
}

#[test]
fn instant_minus_duration_stays_an_instant() {
    // `last - period` is instant - duration → instant; assignable to an instant
    // cell with no error.  (Purely a resolve-time check.)
    let _ = compile(&program(
        "  cell last : instant = 0\n  cell period : duration = 0\n  cell shifted : instant = 0\n  every 100ms {\n    shifted = last - period\n  }",
    ));
}

#[test]
fn instant_plus_duration_literal_is_ok_but_plus_bare_int_is_not() {
    // The defining §4.5 example: `now() + 500ms` types, `now() + 5` does not.
    let _ok = compile(&program(
        "  cell deadline : instant = 0\n  every 100ms {\n    deadline = now() + 500ms\n  }",
    ));
    let errs = resolve_err(&program(
        "  cell deadline : instant = 0\n  every 100ms {\n    deadline = now() + 5\n  }",
    ));
    assert!(
        errs.iter().any(|m| m.contains("add a duration to an instant") || m.contains("bare integer")),
        "errs: {:?}",
        errs
    );
}

#[test]
fn now_takes_no_arguments() {
    let errs = resolve_err(&program(
        "  cell c : instant = 0\n  every 100ms {\n    c = now(5)\n  }",
    ));
    assert!(errs.iter().any(|m| m.contains("now() takes no arguments")), "errs: {:?}", errs);
}
