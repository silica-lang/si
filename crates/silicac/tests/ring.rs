//! §5.3 — the bounded `ring<T, N>` buffer.  FIFO push/pop, a defined
//! overwrite-oldest overflow policy, and storage that the compiler sums into the
//! static RAM budget (no heap).

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
    format!("{BOARD}\nprogram app {{\n  use board demo as b\n{body}\n}}\nsim app_sim for app {{ run until 750ms }}\n")
}

#[test]
fn push_and_pop_are_fifo() {
    // Producer pushes 1,2,3,…; consumer pops the oldest first.
    let t = trace(&program(
        "  cell q : ring<u32, 8> = 0\n  cell produced : u32 = 0\n  cell consumed : u32 = 0\n  every 100ms { produced = produced + 1  q.push(produced) }\n  every 300ms { let v = q.pop()  consumed = consumed + v }",
    ));
    // First pop at 300ms returns 1 (the oldest); second at 600ms returns 2 → 1+2=3.
    assert!(t.iter().any(|l| l.contains("cell consumed = 1")), "first pop = 1:\n{}", t.join("\n"));
    assert!(t.iter().any(|l| l.contains("cell consumed = 3")), "second pop = 2:\n{}", t.join("\n"));
}

#[test]
fn the_ring_is_bounded_and_overwrites_oldest() {
    // Push one per tick into a 3-slot ring, never pop: len must never exceed 3.
    let t = trace(&program(
        "  cell q : ring<u32, 3> = 0\n  cell n : u32 = 0\n  every 100ms { n = n + 1  q.push(n) }",
    ));
    // Parse the highest "len N" the ring reported across all pushes.
    let max = t
        .iter()
        .filter(|l| l.contains("ring q.push()"))
        .filter_map(|l| l.rsplit("len ").next().and_then(|s| s.trim().parse::<u32>().ok()))
        .max()
        .unwrap_or(0);
    assert_eq!(max, 3, "len capped at cap=3:\n{}", t.join("\n"));
}

#[test]
fn ring_storage_is_counted_in_the_ram_budget() {
    // A ring<u32, 16> is 16*4 + 12 (head/tail/count) = 76 bytes of statics.
    let with = compile(&program("  cell n : u32 = 0\n  cell q : ring<u32, 16> = 0\n  every 100ms { n = n + 1  q.push(n) }"));
    let without = compile(&program("  cell n : u32 = 0\n  every 100ms { n = n + 1 }"));
    let b_with = c::ram_budget(&with).expect("budget");
    let b_without = c::ram_budget(&without).expect("budget");
    assert!(
        b_with.statics >= b_without.statics + 76,
        "ring<u32,16> must add >= 76 B of statics: with={} without={}",
        b_with.statics,
        b_without.statics
    );
}

#[test]
fn metal_emits_the_ring_buffer_and_ops() {
    let sir = compile(&program(
        "  cell q : ring<u32, 4> = 0\n  cell n : u32 = 0\n  every 100ms { n = n + 1  q.push(n) }",
    ));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("__ring_q_buf[4]"), "ring backing array:\n{}", out);
    assert!(out.contains("__ring_q_count"), "ring count index:\n{}", out);
    assert!(out.contains("% 4U"), "modular index arithmetic:\n{}", out);
}
