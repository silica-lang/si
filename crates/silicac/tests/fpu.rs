//! §4.1/§4.3 — `float` is gated on an FPU capability.  It is allowed only on a
//! board whose SoC declares `fpu`; otherwise it is a compile error (no silent
//! soft-float).

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

/// A program with a `<decl>` cell on a board whose SoC has an FPU iff `fpu`.
fn program(fpu: bool, decl: &str) -> String {
    let fpu_line = if fpu { "    fpu\n" } else { "" };
    format!(
        r#"
board dk {{
  soc s {{
    memory {{ flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }}
    clocks {{ sysclk : clock_source = 64MHz }}
{fpu_line}  }}
}}
program app {{
  use board dk as b
  {decl}
  cell ticks : u32 = 0
  every 100ms {{ ticks = ticks + 1 }}
}}
sim app_sim for app {{ run until 150ms }}
"#
    )
}

#[test]
fn float_is_allowed_when_the_soc_declares_an_fpu() {
    let _ = compile(&program(true, "cell reading : float = 0"));
}

#[test]
fn float_without_an_fpu_is_a_compile_error() {
    let errs = resolve_err(&program(false, "cell reading : float = 0"));
    assert!(errs.iter().any(|m| m.contains("requires an FPU")), "errs: {:?}", errs);
}

#[test]
fn f64_is_also_gated() {
    let errs = resolve_err(&program(false, "cell reading : f64 = 0"));
    assert!(errs.iter().any(|m| m.contains("requires an FPU")), "errs: {:?}", errs);
}

#[test]
fn a_float_let_annotation_is_gated() {
    let errs = resolve_err(&program(false, "cell c : u32 = 0").replace(
        "every 100ms { ticks = ticks + 1 }",
        "every 100ms { let x : float = 0  ticks = ticks + 1 }",
    ));
    assert!(errs.iter().any(|m| m.contains("requires an FPU")), "errs: {:?}", errs);
}

#[test]
fn integers_are_unaffected_without_an_fpu() {
    // No false positives: a plain integer cell compiles on an FPU-less SoC.
    let _ = compile(&program(false, "cell reading : u32 = 0"));
}
