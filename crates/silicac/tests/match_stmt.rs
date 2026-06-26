//! §4.4/D14 — the `match` statement: total case analysis.  A `match` must be
//! exhaustive (a `_` arm is required), literal arms are mutually exclusive, and
//! the wildcard runs when none matched.

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

fn program(body: &str) -> String {
    format!("{BOARD}\nprogram app {{\n  use board demo as b\n{body}\n}}\nsim app_sim for app {{ run until 450ms }}\n")
}

#[test]
fn literal_arms_and_wildcard_select_correctly() {
    let t = trace(&program(
        "  cell phase : u32 = 0\n  cell state : u32 = 0\n  every 100ms {\n    match phase {\n      0 => { state = 10 }\n      1 => { state = 20 }\n      _ => { state = 99 }\n    }\n    phase = (phase + 1) % 3\n  }",
    ));
    assert!(t.iter().any(|l| l.contains("cell state = 10")), "arm 0:\n{}", t.join("\n"));
    assert!(t.iter().any(|l| l.contains("cell state = 20")), "arm 1:\n{}", t.join("\n"));
    assert!(t.iter().any(|l| l.contains("cell state = 99")), "wildcard:\n{}", t.join("\n"));
}

#[test]
fn a_match_without_a_wildcard_is_rejected() {
    let errs = resolve_err(&program(
        "  cell phase : u32 = 0\n  cell state : u32 = 0\n  every 100ms {\n    match phase {\n      0 => { state = 1 }\n      1 => { state = 2 }\n    }\n  }",
    ));
    assert!(errs.iter().any(|m| m.contains("exhaustive")), "errs: {:?}", errs);
}

#[test]
fn duplicate_literal_arms_are_rejected() {
    let errs = resolve_err(&program(
        "  cell phase : u32 = 0\n  cell state : u32 = 0\n  every 100ms {\n    match phase {\n      0 => { state = 1 }\n      0 => { state = 2 }\n      _ => { state = 3 }\n    }\n  }",
    ));
    assert!(errs.iter().any(|m| m.contains("duplicate match arm")), "errs: {:?}", errs);
}

#[test]
fn bool_match_works() {
    let t = trace(&program(
        "  cell flag : bool = true\n  cell out : u32 = 0\n  every 100ms {\n    match flag {\n      true => { out = 1 }\n      _ => { out = 0 }\n    }\n  }",
    ));
    assert!(t.iter().any(|l| l.contains("cell out = 1")), "true arm:\n{}", t.join("\n"));
}

#[test]
fn metal_lowers_match_to_an_if_chain() {
    let sir = compile(&program(
        "  cell phase : u32 = 0\n  cell state : u32 = 0\n  every 100ms {\n    match phase {\n      0 => { state = 10 }\n      _ => { state = 99 }\n    }\n  }",
    ));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("if ("), "expected guarded ifs:\n{}", out);
    // The match temp and matched-flag both appear.
    assert!(out.contains("__match"), "match temp:\n{}", out);
    assert!(out.contains("__matched"), "matched flag:\n{}", out);
}
