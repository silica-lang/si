//! §4.3 — fixed-point `fixed<I, F>` (audit #35, P0-3a): type, scale-shifting
//! casts (int↔fixed, fixed↔fixed), and same-scale add/sub.  The FPU-less
//! fractional path the `float`-needs-FPU error points users to.

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

const BOARD: &str = r#"
board demo {
  soc s { memory { flash : region at 0x0 size 256K   ram : region at 0x2000_0000 size 64K } clocks { sysclk : clock_source = 64MHz } }
}
"#;

fn program(body: &str) -> String {
    format!("{BOARD}\nprogram app {{\n  use board demo as b\n{body}\n}}\nsim app_sim for app {{ run until 1ms }}\n")
}

#[test]
fn int_to_fixed_and_back_scales_by_frac_bits() {
    // 3 → Q16.16 (raw 3<<16) → back to int = 3.  No precision lost for integers.
    let t = sim::run(&compile(&program(
        "  cell n : u32 = 0\n  on sys.start { let q = 3 as fixed<16,16>  n = q as u32 }",
    )))
    .render(&compile(&program("  cell n : u32 = 0\n  on sys.start { let q = 3 as fixed<16,16>  n = q as u32 }")));
    assert!(t.contains("cell n = 3"), "int→fixed→int round-trips:\n{t}");
}

#[test]
fn same_scale_fixed_add_preserves_scale() {
    // (3 + 2) in Q16.16, read back as int = 5 — the add is raw integer math at
    // the shared scale.
    let src = program(
        "  cell n : u32 = 0\n  on sys.start { let a = 3 as fixed<16,16>  let b2 = 2 as fixed<16,16>  let c = a + b2  n = c as u32 }",
    );
    let t = sim::run(&compile(&src)).render(&compile(&src));
    assert!(t.contains("cell n = 5"), "fixed add keeps the scale:\n{t}");
}

#[test]
fn fixed_metal_compiles() {
    let src = program("  cell q : fixed<16,16> = 0\n  on sys.start { q = 7 as fixed<16,16> }");
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&compile(&src));
    // Q16.16 → 32-bit signed storage; the int→fixed cast shifts left 16.
    assert!(out.contains("int32_t q") || out.contains("int32_t  q"), "fixed cell storage:\n{out}");
    assert!(out.contains("<< 16"), "int→fixed scales up:\n{out}");
}

#[test]
fn mixing_fixed_and_integer_is_an_error() {
    let errs = resolve_err(&program(
        "  cell n : u32 = 0\n  on sys.start { let i : u32 = 1  let q = 2 as fixed<16,16>  let c = q + i  n = c as u32 }",
    ));
    assert!(errs.iter().any(|e| e.contains("mix fixed-point and integer")), "{errs:?}");
}

#[test]
fn mixing_different_fixed_scales_is_an_error() {
    let errs = resolve_err(&program(
        "  cell n : u32 = 0\n  on sys.start { let a = 1 as fixed<16,16>  let b2 = 1 as fixed<8,8>  let c = a + b2  n = c as u32 }",
    ));
    assert!(errs.iter().any(|e| e.contains("different `fixed<I,F>` scales")), "{errs:?}");
}

#[test]
fn fixed_multiply_rescales() {
    // 2.0 * 3.0 = 6.0 in Q16.16 — the helper multiplies in a wider intermediate
    // then shifts the binary point back by 16.
    let src = program(
        "  cell n : u32 = 0\n  on sys.start { let a = 2 as fixed<16,16>  let b2 = 3 as fixed<16,16>  let c = a * b2  n = c as u32 }",
    );
    let t = sim::run(&compile(&src)).render(&compile(&src));
    assert!(t.contains("cell n = 6"), "2*3 = 6:\n{t}");
}

#[test]
fn fixed_divide_keeps_fractional_precision() {
    // (7 / 2) * 2 = 7 — the 0.5 survives because div keeps 16 fractional bits.
    let src = program(
        "  cell n : u32 = 0\n  on sys.start { let a = 7 as fixed<16,16>  let two = 2 as fixed<16,16>  let half = a / two  let back = half * two  n = back as u32 }",
    );
    let t = sim::run(&compile(&src)).render(&compile(&src));
    assert!(t.contains("cell n = 7"), "(7/2)*2 = 7 — fractional precision kept:\n{t}");
}

#[test]
fn fixed_multiply_overflow_traps() {
    // 200.0 * 200.0 = 40000.0 overflows Q16.16 (int32 holds ≈ ±32767.99) → trap.
    let src = program(
        "  cell n : u32 = 0\n  on sys.start { let a = 200 as fixed<16,16>  let b2 = 200 as fixed<16,16>  let c = a * b2  n = c as u32 }",
    );
    let t = sim::run(&compile(&src)).render(&compile(&src));
    assert!(t.contains("OVERFLOW TRAP"), "fixed mul overflow traps to safe-state:\n{t}");
}

#[test]
fn fixed_mul_metal_emits_a_rescaling_helper() {
    let src = program("  cell q : fixed<16,16> = 0\n  on sys.start { let a = 3 as fixed<16,16>  q = a * a }");
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&compile(&src));
    assert!(out.contains("__si_fixmul_trap_s32_f16"), "fixed mul helper emitted:\n{out}");
    assert!(out.contains(">> 16"), "helper rescales by the frac bits:\n{out}");
}

#[test]
fn decimal_literal_carries_a_fraction() {
    // 0.5 (inferred Q16.16) × 2 = 1.0 — proves the 0.5 was real, not truncated.
    let src = program(
        "  cell n : u32 = 0\n  on sys.start { let h = 0.5  let two = 2 as fixed<16,16>  n = (h * two) as u32 }",
    );
    let t = sim::run(&compile(&src)).render(&compile(&src));
    assert!(t.contains("cell n = 1"), "0.5 * 2 = 1:\n{t}");
}

#[test]
fn voltage_literal_parses_as_fixed() {
    // 3v3 = 3.3; × 10 = 33.
    let src = program(
        "  cell n : u32 = 0\n  on sys.start { let v = 3v3  let ten = 10 as fixed<16,16>  n = (v * ten) as u32 }",
    );
    let t = sim::run(&compile(&src)).render(&compile(&src));
    assert!(t.contains("cell n = 33"), "3v3 * 10 = 33:\n{t}");
}

#[test]
fn decimal_literal_adopts_the_annotated_scale() {
    // A `fixed<8,8>` cell scales 0.5 at F=8 (raw 128), not the default F=16.
    let src = program(
        "  cell q : fixed<8,8> = 0.5\n  cell n : u32 = 0\n  on sys.start { let two = 2 as fixed<8,8>  n = (q * two) as u32 }",
    );
    let t = sim::run(&compile(&src)).render(&compile(&src));
    assert!(t.contains("cell n = 1"), "0.5(Q8.8) * 2 = 1:\n{t}");
}

#[test]
fn assigning_an_integer_to_a_fixed_binding_needs_a_cast() {
    let errs = resolve_err(&program(
        "  cell q : fixed<16,16> = 0\n  on sys.start { let i : u32 = 5  q = i }",
    ));
    assert!(errs.iter().any(|e| e.contains("integer to a fixed-point")), "{errs:?}");
}
