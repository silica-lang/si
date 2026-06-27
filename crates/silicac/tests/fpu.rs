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

#[test]
fn float_arithmetic_computes_in_the_simulator() {
    // P6-8: `+ - * /` on floats compute IEEE values (carried as bit patterns in
    // the sim's u64 model).  3 ticks: acc = 0+1.5*3 = 4.5; out = acc*2 = 9.0.
    let src = r#"
board dk {
  soc s {
    memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }
    clocks { sysclk : clock_source = 64MHz }
    fpu
  }
}
program app {
  use board dk as b
  cell acc : float = 0.0
  cell out : float = 0.0
  every 100ms { acc = acc + 1.5  out = acc * 2.0 }
}
sim s for app { run until 350ms }
"#;
    let sir = compile(src);
    let trace = silicac::sim::run(&sir).render(&sir);
    // 4.5f = 0x40900000 = 1083179008; 9.0f = 0x41100000 = 1091567616 (IEEE bits).
    assert!(trace.contains("cell acc = 1083179008"), "float add wrong:\n{}", trace);
    assert!(trace.contains("cell out = 1091567616"), "float mul wrong:\n{}", trace);
}

#[test]
fn decimal_literal_is_float_in_a_float_context_fixed_otherwise() {
    // The SAME literal `2.5` is a float (bits 0x40200000 = 1075838976) assigned to
    // a float cell, but a Q16.16 fixed raw (163840) assigned to a fixed cell.
    let src = r#"
board dk {
  soc s {
    memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }
    clocks { sysclk : clock_source = 64MHz }
    fpu
  }
}
program app {
  use board dk as b
  cell f : float       = 0.0
  cell q : fixed<16,16> = 0
  on sys.start { f = 2.5  q = 2.5 }
}
sim s for app { run until 1ms }
"#;
    let sir = compile(src);
    let trace = silicac::sim::run(&sir).render(&sir);
    assert!(trace.contains("cell f = 1075838976"), "decimal should be float in a float cell:\n{}", trace);
    assert!(trace.contains("cell q = 163840"), "decimal should be Q16.16 in a fixed cell:\n{}", trace);
}
