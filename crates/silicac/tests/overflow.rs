//! §4.3 / SIL-004 — width-checked integer arithmetic.  Plain `+`/`-`/`*` trap on
//! overflow by default (the system is driven to its safe state); the explicit
//! `+%`/`-%`/`*%` operators wrap two's-complement and `+|`/`-|`/`*|` saturate,
//! all at the result type's width.

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

/// A program whose `every 100ms` reaction applies `<op>` to a `u8` cell that
/// starts at 200, so the third tick (200 → 300) is the overflow point.
fn program(op: &str) -> String {
    format!(
        r#"
board demo {{
  soc s {{ memory {{ flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }} clocks {{ sysclk : clock_source = 64MHz }} }}
}}
program app {{
  use board demo as b
  cell c : u8 = 200
  every 100ms {{
    c = c {op} 100
  }}
}}
sim app_sim for app {{ run until 350ms }}
"#
    )
}

fn trace(src: &str) -> Vec<String> {
    let sir = compile(src);
    sim::run(&sir).render(&sir).lines().map(|s| s.to_string()).collect()
}

#[test]
fn default_add_traps_on_overflow_and_drives_safe_state() {
    // 200 + 100 = 300 does not fit in u8 → trap → safe-state (§4.3/SIL-004).
    let t = trace(&program("+"));
    assert!(
        t.iter().any(|l| l.contains("OVERFLOW TRAP") && l.contains("8-bit")),
        "expected an 8-bit overflow trap:\n{}",
        t.join("\n")
    );
}

#[test]
fn wrapping_add_wraps_at_the_type_width() {
    // 200 +% 100 = 300 mod 256 = 44, no trap.
    let t = trace(&program("+%"));
    assert!(
        !t.iter().any(|l| l.contains("OVERFLOW TRAP")),
        "wrapping must not trap:\n{}",
        t.join("\n")
    );
    assert!(
        t.iter().any(|l| l.contains("cell c = 44")),
        "expected wrap to 44:\n{}",
        t.join("\n")
    );
}

#[test]
fn saturating_add_clamps_to_the_type_max() {
    // 200 +| 100 clamps to u8 max 255, no trap.
    let t = trace(&program("+|"));
    assert!(
        !t.iter().any(|l| l.contains("OVERFLOW TRAP")),
        "saturating must not trap:\n{}",
        t.join("\n")
    );
    assert!(
        t.iter().any(|l| l.contains("cell c = 255")),
        "expected saturation to 255:\n{}",
        t.join("\n")
    );
}

#[test]
fn wider_cell_does_not_trap_at_the_narrow_boundary() {
    // The same arithmetic in a u32 cell has headroom — 300 fits, no trap, and the
    // value is the true sum (proves the trap width tracks the target type).
    let src = r#"
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
}
program app {
  use board demo as b
  cell c : u32 = 200
  every 100ms { c = c + 100 }
}
sim app_sim for app { run until 250ms }
"#;
    let t = trace(src);
    assert!(!t.iter().any(|l| l.contains("OVERFLOW TRAP")), "u32 must not trap at 300:\n{}", t.join("\n"));
    assert!(t.iter().any(|l| l.contains("cell c = 400")), "expected true sum 400:\n{}", t.join("\n"));
}

/// The metal backend must emit a trap helper using `__builtin_add_overflow` and
/// route overflow to the safe-state halt, plus distinct wrap/saturate helpers.
#[test]
fn metal_emits_trap_wrap_and_saturate_helpers() {
    let src = r#"
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
}
program app {
  use board demo as b
  cell a : u8 = 0
  cell w : u8 = 0
  cell s : u8 = 0
  every 100ms {
    a = a + 1
    w = w +% 1
    s = s +| 1
  }
}
sim app_sim for app { run until 150ms }
"#;
    let sir = compile(src);
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("__si_add_trap_u8"), "trap helper:\n{}", out);
    assert!(out.contains("__builtin_add_overflow"), "uses overflow builtin:\n{}", out);
    assert!(out.contains("__silica_overflow_trap"), "trap routine:\n{}", out);
    assert!(out.contains("__drive_safe();"), "trap drives safe-state:\n{}", out);
    assert!(out.contains("__si_add_wrap_u8"), "wrap helper:\n{}", out);
    assert!(out.contains("__si_add_sat_u8"), "saturate helper:\n{}", out);
}
