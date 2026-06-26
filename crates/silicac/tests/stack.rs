//! §5.3/SIL-005 — worst-case stack analysis.  The metal RAM budget's stack term
//! is now computed from the SIR (per-reaction frame × ISR nesting by priority),
//! not a flat 2048 stub; and recursion — which would make the bound unbounded —
//! is a compile error.

use silicac::backend::c;
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
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
}
"#;

#[test]
fn the_stack_term_is_computed_not_the_old_flat_stub() {
    // A trivial one-reaction program: the computed worst-case is well under the
    // old flat 2048 reserve.
    let sir = compile(&format!(
        "{BOARD}\nprogram app {{\n  use board demo as b\n  cell n : u32 = 0\n  every 100ms {{ n = n + 1 }}\n}}\nsim s for app {{ run until 150ms }}\n"
    ));
    let b = c::ram_budget(&sir).expect("budget");
    assert_eq!(b.stack_reserve, c::worst_case_stack(&sir));
    assert!(b.stack_reserve >= 512, "at least the base context, got {}", b.stack_reserve);
    assert!(b.stack_reserve < 2048, "trivial program is below the old stub, got {}", b.stack_reserve);
}

#[test]
fn more_priority_levels_grow_the_worst_case_stack() {
    // One reaction (a single `every` priority level)…
    let one = compile(&format!(
        "{BOARD}\nprogram app {{\n  use board demo as b\n  cell n : u32 = 0\n  every 100ms {{ n = n + 1 }}\n}}\nsim s for app {{ run until 150ms }}\n"
    ));
    // …vs adding a higher-priority `on sys.start` reaction (a second level that
    // can nest above the timer level): the worst-case stack must grow.
    let two = compile(&format!(
        "{BOARD}\nprogram app {{\n  use board demo as b\n  cell n : u32 = 0\n  on sys.start {{ n = 1 }}\n  every 100ms {{ n = n + 1 }}\n}}\nsim s for app {{ run until 150ms }}\n"
    ));
    assert!(
        c::worst_case_stack(&two) > c::worst_case_stack(&one),
        "two priority levels ({}) must exceed one ({})",
        c::worst_case_stack(&two),
        c::worst_case_stack(&one)
    );
}

#[test]
fn recursion_is_banned() {
    // A device that needs its own interface and an instance wired to itself makes
    // `peer.f()` call back into `f` — recursion, which is rejected.
    let src = r#"
interface ff { op f() -> () }
device d implements ff {
  needs { peer : ff }
  ops { op f() -> () { peer.f() } }
}
board b {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  x : d at 0x5000_0000 { needs { peer = x } }
}
program app {
  use board b as bd
  let dev = bd.x
  cell n : u32 = 0
  every 100ms { dev.f()  n = n + 1 }
}
sim s for app { run until 150ms }
"#;
    let errs = resolve_err(src);
    assert!(errs.iter().any(|m| m.contains("recursive") && m.contains("banned")), "errs: {:?}", errs);
}
