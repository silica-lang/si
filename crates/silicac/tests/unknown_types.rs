//! §4.3 / audit #35 P7-3 (Finding C) — unknown or unsupported type annotations
//! are a hard compile error, not a silent `u32`.  Before P7-3, `resolve_type_expr`
//! ended both matches with `_ => SirType::U32`, so a misspelled/undeclared type
//! name and a parsed-but-unlowered `buffer<N>` compiled as `u32` — a
//! "nothing statically unknowable" hole.

use silicac::{lexer, parser, resolver};

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
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
}
"#;

fn program(body: &str) -> String {
    format!("{BOARD}\nprogram app {{\n  use board demo as b\n{body}\n}}\nsim app_sim for app {{ run until 350ms }}\n")
}

#[test]
fn a_misspelled_cell_type_is_rejected() {
    // `Flaot` is not a built-in type — it must not silently become `u32`.
    let errs = resolve_err(&program("  cell x : Flaot = 0\n  every 100ms { x = x }"));
    assert!(
        errs.iter().any(|m| m.contains("unknown type `Flaot`")),
        "expected an unknown-type error, got: {errs:?}"
    );
}

#[test]
fn buffer_type_now_compiles() {
    // P7-5a replaced P7-3's `buffer<N>` "not yet implemented" placeholder with a
    // real bounded byte buffer — declaring one is no longer an error (behaviour is
    // covered by tests/buffer.rs).
    let src = program("  cell b : buffer<8> = 0\n  every 100ms { b.set(0, 1) }");
    let tokens = lexer::lex(&src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    assert!(resolver::resolve(&ast).is_ok(), "buffer<N> must compile after P7-5a");
}

#[test]
fn an_unknown_local_annotation_is_rejected() {
    let errs = resolve_err(&program(
        "  every 100ms {\n    let y : nonesuch = 1\n  }",
    ));
    assert!(
        errs.iter().any(|m| m.contains("unknown type `nonesuch`")),
        "expected an unknown-type error on the local, got: {errs:?}"
    );
}

#[test]
fn an_unknown_cast_target_is_rejected() {
    let errs = resolve_err(&program(
        "  cell x : u32 = 0\n  every 100ms {\n    x = 1 as widget\n  }",
    ));
    assert!(
        errs.iter().any(|m| m.contains("unknown type `widget`")),
        "expected an unknown-type error on the cast target, got: {errs:?}"
    );
}

#[test]
fn an_unknown_ring_element_type_is_rejected() {
    // The error must reach a `ring<T,N>` element, not just top-level names.
    let errs = resolve_err(&program("  cell q : ring<nope, 4> = 0\n  every 100ms { }"));
    assert!(
        errs.iter().any(|m| m.contains("unknown type `nope`")),
        "expected an unknown-type error on the ring element, got: {errs:?}"
    );
}

#[test]
fn an_unknown_type_is_reported_exactly_once() {
    // The same annotation can be resolved at more than one lowering site; the
    // hard error is deduped by span so it is reported once, not N times.
    let errs = resolve_err(&program(
        "  cell x : u32 = 0\n  every 100ms {\n    x = 1 as widget\n  }",
    ));
    let n = errs.iter().filter(|m| m.contains("unknown type `widget`")).count();
    assert_eq!(n, 1, "expected exactly one diagnostic, got {n}: {errs:?}");
}

#[test]
fn a_valid_program_still_compiles() {
    // Guard against over-rejection: the built-in types must all still resolve.
    let src = program(
        "  cell a : u8 = 0\n  cell b : s16 = 0\n  cell c : fixed<16,16> = 0\n  \
         cell d : ring<u32,4> = 0\n  cell e : bool = 0\n  cell f : instant = 0\n  \
         every 100ms {\n    a = a\n  }",
    );
    let tokens = lexer::lex(&src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    assert!(resolver::resolve(&ast).is_ok(), "valid built-in types must still compile");
}
