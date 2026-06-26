//! §4.3 — the number model's conversion rules.  Width is explicit; narrowing
//! and sign changes are never implicit (an `as` cast is the only way), mixed
//! signed/unsigned operands are rejected, and an out-of-range literal is a
//! compile error.

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
    format!("{BOARD}\nprogram app {{\n  use board demo as b\n{body}\n}}\nsim app_sim for app {{ run until 350ms }}\n")
}

#[test]
fn narrowing_cast_truncates_at_runtime() {
    // total reaches 300; (total as u8) truncates to 44.
    let t = trace(&program(
        "  cell total : u32 = 0\n  cell low8 : u8 = 0\n  every 100ms {\n    total = total + 100\n    low8 = total as u8\n  }",
    ));
    assert!(t.iter().any(|l| l.contains("cell low8 = 44")), "expected truncation to 44:\n{}", t.join("\n"));
}

#[test]
fn implicit_narrowing_is_rejected() {
    let errs = resolve_err(&program(
        "  cell big : u32 = 0\n  cell small : u8 = 0\n  every 100ms {\n    small = big\n  }",
    ));
    assert!(errs.iter().any(|m| m.contains("implicit narrowing")), "errs: {:?}", errs);
}

#[test]
fn narrowing_with_an_explicit_cast_is_ok() {
    let _ = compile(&program(
        "  cell big : u32 = 0\n  cell small : u8 = 0\n  every 100ms {\n    small = big as u8\n  }",
    ));
}

#[test]
fn widening_is_implicit_and_ok() {
    // u8 → u32 assignment is lossless, no cast needed.
    let _ = compile(&program(
        "  cell small : u8 = 0\n  cell big : u32 = 0\n  every 100ms {\n    big = small\n  }",
    ));
}

#[test]
fn mixed_sign_arithmetic_is_rejected() {
    let errs = resolve_err(&program(
        "  cell u : u32 = 0\n  cell s : s32 = 0\n  cell r : u32 = 0\n  every 100ms {\n    r = (u + s) as u32\n  }",
    ));
    // `u + s` mixes signedness; the outer cast cannot rescue the inner operands.
    assert!(errs.iter().any(|m| m.contains("mixed signed/unsigned")), "errs: {:?}", errs);
}

#[test]
fn mixed_sign_resolved_by_casting_is_ok() {
    let _ = compile(&program(
        "  cell u : u32 = 0\n  cell s : s32 = 0\n  cell r : u32 = 0\n  every 100ms {\n    r = u + (s as u32)\n  }",
    ));
}

#[test]
fn out_of_range_cell_initialiser_is_rejected() {
    let errs = resolve_err(&program("  cell c : u8 = 300\n  every 100ms {\n    c = c\n  }"));
    assert!(errs.iter().any(|m| m.contains("does not fit in a 8-bit")), "errs: {:?}", errs);
}

#[test]
fn out_of_range_literal_assignment_is_rejected() {
    let errs = resolve_err(&program(
        "  cell c : u8 = 0\n  every 100ms {\n    c = 300\n  }",
    ));
    assert!(errs.iter().any(|m| m.contains("does not fit in a 8-bit")), "errs: {:?}", errs);
}

#[test]
fn metal_emits_a_c_cast() {
    let sir = compile(&program(
        "  cell total : u32 = 0\n  cell low8 : u8 = 0\n  every 100ms {\n    total = total + 100\n    low8 = total as u8\n  }",
    ));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("(uint8_t)"), "expected a C narrowing cast:\n{}", out);
}
