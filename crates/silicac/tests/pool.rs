//! §5.3 / audit #35 P7-5b — `pool<T, N>`, a bounded fixed-capacity pool.  A
//! second real bounded container (beside `ring`/`buffer`): `N` slots claimed by
//! `alloc` (returning a handle, or the exhausted sentinel `cap` when full),
//! released by `free`, and read/written by handle — no dynamic allocation.

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
fn alloc_set_get_count_cap_round_trip() {
    let t = trace(&program(
        "  cell p : pool<u32, 2> = 0\n  cell n : u32 = 0\n  \
         cell a : u32 = 9\n  cell got : u32 = 0\n  cell cnt : u32 = 0\n  cell cp : u32 = 0\n  \
         every 100ms {\n    n = n + 1\n    match n {\n      1 => {\n        \
         let x = p.alloc()\n        a = x\n        p.set(x, 77)\n        got = p.get(x)\n        \
         cnt = p.count()\n        cp = p.cap()\n      }\n      _ => {}\n    }\n  }",
    ));
    let j = t.join("\n");
    assert!(t.iter().any(|l| l.contains("cell a = 0")), "first alloc must be handle 0:\n{j}");
    assert!(t.iter().any(|l| l.contains("cell got = 77")), "set/get must round-trip:\n{j}");
    assert!(t.iter().any(|l| l.contains("cell cnt = 1")), "count must reflect one allocation:\n{j}");
    assert!(t.iter().any(|l| l.contains("cell cp = 2")), "cap must be 2:\n{j}");
}

#[test]
fn alloc_hands_out_distinct_slots_then_the_exhausted_sentinel() {
    let t = trace(&program(
        "  cell p : pool<u32, 2> = 0\n  cell n : u32 = 0\n  \
         cell a : u32 = 9\n  cell b : u32 = 9\n  cell exh : u32 = 9\n  \
         every 100ms {\n    n = n + 1\n    match n {\n      1 => {\n        \
         a = p.alloc()\n        b = p.alloc()\n        exh = p.alloc()\n      }\n      _ => {}\n    }\n  }",
    ));
    let j = t.join("\n");
    assert!(t.iter().any(|l| l.contains("cell a = 0")), "first alloc = 0:\n{j}");
    assert!(t.iter().any(|l| l.contains("cell b = 1")), "second alloc = 1:\n{j}");
    // Exhausted: the third alloc on a cap-2 pool returns the sentinel `cap` (= 2).
    assert!(t.iter().any(|l| l.contains("cell exh = 2")), "exhausted alloc must return the sentinel cap=2:\n{j}");
}

#[test]
fn free_returns_a_slot_to_the_pool() {
    let t = trace(&program(
        "  cell p : pool<u32, 2> = 0\n  cell n : u32 = 0\n  \
         cell reused : u32 = 9\n  cell cnt : u32 = 9\n  \
         every 100ms {\n    n = n + 1\n    match n {\n      1 => {\n        \
         let x = p.alloc()\n        let y = p.alloc()\n        p.free(x)\n        \
         reused = p.alloc()\n        cnt = p.count()\n      }\n      _ => {}\n    }\n  }",
    ));
    let j = t.join("\n");
    // After freeing slot 0 and re-allocating, slot 0 is handed back out.
    assert!(t.iter().any(|l| l.contains("cell reused = 0")), "freed slot must be reusable:\n{j}");
    assert!(t.iter().any(|l| l.contains("cell cnt = 2")), "count must be 2 after free+realloc:\n{j}");
}

#[test]
fn metal_c_emits_pool_backing_arrays() {
    let sir = compile(&program(
        "  cell p : pool<u32, 4> = 0\n  cell h : u32 = 0\n  \
         every 100ms {\n    h = p.alloc()\n    p.set(h, 5)\n  }",
    ));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("__pool_p_slot[4]"), "no slot array:\n{out}");
    assert!(out.contains("__pool_p_used[4]"), "no used-flag array:\n{out}");
    assert!(out.contains("__pool_p_count"), "no allocated count:\n{out}");
}

#[test]
fn pool_declaration_compiles() {
    let src = program("  cell p : pool<u16, 8> = 0\n  every 100ms { let h = p.alloc() }");
    let tokens = lexer::lex(&src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    assert!(resolver::resolve(&ast).is_ok(), "pool<T,N> must compile");
}
