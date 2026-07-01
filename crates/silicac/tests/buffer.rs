//! §5.3 / audit #35 P7-5a — `buffer<N>`, a bounded fixed-capacity byte buffer.
//! A second real bounded container beside `ring<T,N>`: `N` bytes of statically
//! allocated storage with bounds-guarded `.set(i,v)` / `.get(i)` / `.len` ops
//! (an out-of-range access is a defined no-op / 0, never UB).

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
fn buffer_set_then_get_round_trips_in_the_sim() {
    // Write two bytes, read one back into a cell — the sim's backing store holds
    // the value across the set/get.
    let t = trace(&program(
        "  cell buf : buffer<8> = 0\n  cell out : u32 = 0\n  \
         every 100ms {\n    buf.set(3, 42)\n    out = buf.get(3)\n  }",
    ));
    assert!(t.iter().any(|l| l.contains("cell out = 42")), "expected round-trip to 42:\n{}", t.join("\n"));
}

#[test]
fn buffer_len_is_the_declared_capacity() {
    let t = trace(&program(
        "  cell buf : buffer<8> = 0\n  cell n : u32 = 0\n  every 100ms {\n    n = buf.len()\n  }",
    ));
    assert!(t.iter().any(|l| l.contains("cell n = 8")), "expected len 8:\n{}", t.join("\n"));
}

#[test]
fn out_of_range_access_is_a_bounded_no_op() {
    // An out-of-range set is dropped; an out-of-range get reads 0 — bounded,
    // never UB (§5.3).
    let t = trace(&program(
        "  cell buf : buffer<4> = 0\n  cell out : u32 = 7\n  \
         every 100ms {\n    buf.set(99, 5)\n    out = buf.get(99)\n  }",
    ));
    assert!(t.iter().any(|l| l.contains("cell out = 0")), "out-of-range get must read 0:\n{}", t.join("\n"));
}

#[test]
fn metal_c_emits_a_bounded_backing_array() {
    // The C metal backend allocates a fixed `uint8_t` backing array and
    // bounds-guards each access.
    let sir = compile(&program(
        "  cell buf : buffer<8> = 0\n  cell out : u32 = 0\n  \
         every 100ms {\n    buf.set(1, 9)\n    out = buf.get(1)\n  }",
    ));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("uint8_t __buf_buf[8]"), "no backing array:\n{out}");
    assert!(out.contains("__bi < 8U"), "set must be bounds-guarded:\n{out}");
    assert!(out.contains("< 8U ? __buf_buf["), "get must be bounds-guarded:\n{out}");
}

#[test]
fn buffer_declaration_no_longer_errors() {
    // Guard against the P7-3 placeholder: `buffer<N>` used to be "not yet
    // implemented"; it now compiles.
    let src = program("  cell buf : buffer<16> = 0\n  every 100ms { buf.set(0, 1) }");
    let tokens = lexer::lex(&src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    assert!(resolver::resolve(&ast).is_ok(), "buffer<N> must compile now");
}
