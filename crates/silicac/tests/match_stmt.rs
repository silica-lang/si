//! §4.4/D14 — the `match` statement: total case analysis.  A `match` must be
//! exhaustive (a `_` arm is required), literal arms are mutually exclusive, and
//! the wildcard runs when none matched.

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
    format!("{BOARD}\nprogram app {{\n  use board demo as b\n{body}\n}}\nsim app_sim for app {{ run until 450ms }}\n")
}

#[test]
fn literal_arms_and_wildcard_select_correctly() {
    let t = trace(&program(
        "  cell phase : u32 = 0\n  cell state : u32 = 0\n  every 100ms {\n    match phase {\n      0 => { state = 10 }\n      1 => { state = 20 }\n      _ => { state = 99 }\n    }\n    phase = (phase + 1) % 3\n  }",
    ));
    assert!(t.iter().any(|l| l.contains("cell state = 10")), "arm 0:\n{}", t.join("\n"));
    assert!(t.iter().any(|l| l.contains("cell state = 20")), "arm 1:\n{}", t.join("\n"));
    assert!(t.iter().any(|l| l.contains("cell state = 99")), "wildcard:\n{}", t.join("\n"));
}

#[test]
fn a_match_without_a_wildcard_is_rejected() {
    let errs = resolve_err(&program(
        "  cell phase : u32 = 0\n  cell state : u32 = 0\n  every 100ms {\n    match phase {\n      0 => { state = 1 }\n      1 => { state = 2 }\n    }\n  }",
    ));
    assert!(errs.iter().any(|m| m.contains("exhaustive")), "errs: {:?}", errs);
}

#[test]
fn duplicate_literal_arms_are_rejected() {
    let errs = resolve_err(&program(
        "  cell phase : u32 = 0\n  cell state : u32 = 0\n  every 100ms {\n    match phase {\n      0 => { state = 1 }\n      0 => { state = 2 }\n      _ => { state = 3 }\n    }\n  }",
    ));
    assert!(errs.iter().any(|m| m.contains("duplicate match arm")), "errs: {:?}", errs);
}

#[test]
fn bool_match_works() {
    let t = trace(&program(
        "  cell flag : bool = true\n  cell out : u32 = 0\n  every 100ms {\n    match flag {\n      true => { out = 1 }\n      _ => { out = 0 }\n    }\n  }",
    ));
    assert!(t.iter().any(|l| l.contains("cell out = 1")), "true arm:\n{}", t.join("\n"));
}

#[test]
fn metal_lowers_match_to_an_if_chain() {
    let sir = compile(&program(
        "  cell phase : u32 = 0\n  cell state : u32 = 0\n  every 100ms {\n    match phase {\n      0 => { state = 10 }\n      _ => { state = 99 }\n    }\n  }",
    ));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("if ("), "expected guarded ifs:\n{}", out);
    // The match temp and matched-flag both appear.
    assert!(out.contains("__match"), "match temp:\n{}", out);
    assert!(out.contains("__matched"), "matched flag:\n{}", out);
}

// ─── `match` over an op's fault codes (§4.4/D14, audit #35 P2-4) ──────────────

/// An inline bus controller with a primitive (yielding) fault-returning op, plus
/// a board instance — the std-less fixture for result-match tests.
const CTRL: &str = r#"
device busctl {
  regs {
    CR : reg32 at 0x00 access rw { start: bit[0], dir: bit[1] }
    SR : reg32 at 0x04 access ro { done: bit[0], nak: bit[1], arblost: bit[2], timeout: bit[3] }
    SA : reg32 at 0x08 access rw {}
    RA : reg32 at 0x0C access rw {}
    DR : reg32 at 0x10 access rw {}
  }
  ops {
    op read_reg(addr: u32, reg: u32) -> u32 or fault{nak, timeout, arblost} yields
  }
}
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  i2c0 : busctl at 0x4000_3000
}
"#;

/// A program whose `every 1000ms` body is a result-match with the given `arms`,
/// plus a `sim` block carrying `inject`s.  Cells `reads/naks/timeouts/arblosts/
/// last` record which arm fired.
fn rprogram(arms: &str, injects: &str) -> String {
    format!(
        "{CTRL}\nprogram app {{\n  use board demo as b\n  let bus = b.i2c0\n  \
         cell reads : u32 = 0\n  cell naks : u32 = 0\n  cell timeouts : u32 = 0\n  \
         cell arblosts : u32 = 0\n  cell last : u32 = 0\n  \
         every 1000ms {{\n    match bus.read_reg(0x40, 0x00) {{\n{arms}\n    }}\n  }}\n}}\n\
         sim app_sim for app {{ {injects} run until 3500ms }}\n"
    )
}

const ALL_ARMS: &str = "      ok v          => { reads = reads + 1  last = v }\n      \
                         fault nak     => { naks = naks + 1 }\n      \
                         fault timeout => { timeouts = timeouts + 1 }\n      \
                         fault arblost => { arblosts = arblosts + 1 }";

#[test]
fn result_match_dispatches_ok_and_each_fault_code() {
    // Inject nak then timeout; the 3rd transaction drains the queue → ok.
    let t = trace(&rprogram(
        ALL_ARMS,
        "inject bus_fault nak times 1  inject bus_fault timeout times 1",
    ));
    let joined = t.join("\n");
    assert!(t.iter().any(|l| l.contains("cell naks = 1")), "nak arm:\n{joined}");
    assert!(t.iter().any(|l| l.contains("cell timeouts = 1")), "timeout arm:\n{joined}");
    assert!(t.iter().any(|l| l.contains("cell reads = 1")), "ok arm:\n{joined}");
    // The `ok v` binding captured the bus value.
    assert!(t.iter().any(|l| l.contains("cell last = ")), "ok binding:\n{joined}");
    // arblost was never injected, so its arm never ran.
    assert!(!t.iter().any(|l| l.contains("cell arblosts = 1")), "arblost should not fire:\n{joined}");
}

#[test]
fn a_non_exhaustive_result_match_is_rejected() {
    // Missing `fault arblost` and no `_`.
    let errs = resolve_err(&rprogram(
        "      ok            => { reads = reads + 1 }\n      \
         fault nak     => { naks = naks + 1 }\n      \
         fault timeout => { timeouts = timeouts + 1 }",
        "",
    ));
    assert!(
        errs.iter().any(|m| m.contains("non-exhaustive") && m.contains("arblost")),
        "errs: {:?}",
        errs
    );
}

#[test]
fn a_wildcard_makes_a_result_match_exhaustive() {
    // `_` covers the un-listed fault codes — must compile.
    let _ = compile(&rprogram(
        "      ok            => { reads = reads + 1 }\n      \
         fault nak     => { naks = naks + 1 }\n      _ => { timeouts = timeouts + 1 }",
        "",
    ));
}

#[test]
fn an_undeclared_fault_code_is_rejected() {
    let errs = resolve_err(&rprogram(
        "      ok            => { reads = reads + 1 }\n      \
         fault nak     => { naks = naks + 1 }\n      \
         fault timeout => { timeouts = timeouts + 1 }\n      \
         fault bogus   => { arblosts = arblosts + 1 }",
        "",
    ));
    assert!(
        errs.iter().any(|m| m.contains("cannot raise fault 'bogus'")),
        "errs: {:?}",
        errs
    );
}

#[test]
fn a_question_mark_scrutinee_is_rejected() {
    let errs = resolve_err(&rprogram(
        &ALL_ARMS.replace("bus.read_reg", "bus.read_reg"), // arms unchanged
        "",
    ).replace("match bus.read_reg(0x40, 0x00)", "match bus.read_reg(0x40, 0x00)?"));
    assert!(errs.iter().any(|m| m.contains("drop the `?`")), "errs: {:?}", errs);
}

#[test]
fn a_duplicate_fault_arm_is_rejected() {
    let errs = resolve_err(&rprogram(
        "      ok            => { reads = reads + 1 }\n      \
         fault nak     => { naks = naks + 1 }\n      \
         fault nak     => { naks = naks + 2 }\n      _ => { timeouts = timeouts + 1 }",
        "",
    ));
    assert!(errs.iter().any(|m| m.contains("duplicate `fault nak`")), "errs: {:?}", errs);
}

#[test]
fn metal_kick_clears_stale_bus_pending_before_arming() {
    // P3-1: a yielding `every` reaction must re-fire on metal.  The completion
    // IRQ line is level, so each kick must clear any stale NVIC pending from the
    // previous transaction before enabling — else the next kick re-takes the old
    // pending spuriously and the reaction fires only once.
    let sir = compile(&rprogram(ALL_ARMS, ""));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("__bus_irq_clear_pending"), "missing clear-pending helper:\n{}", out);
    // The clear must precede the enable within the kick sequence.
    let kick = out.find("__bus_owner = (int32_t)").expect("bus kick present");
    let tail = &out[kick..];
    let clear = tail.find("__bus_irq_clear_pending()").expect("clear-pending in kick");
    let enable = tail.find("__bus_irq_enable()").expect("enable in kick");
    assert!(clear < enable, "clear-pending must precede enable in the kick");
}

#[test]
fn metal_decodes_each_fault_code_from_the_sr_bits() {
    let sir = compile(&rprogram(ALL_ARMS, ""));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    // The resumed transaction decodes the outcome for the match (not a dispose).
    assert!(out.contains("decode outcome for `match`"), "missing match decode:\n{}", out);
    // Each declared fault code maps to its named SR bit: nak=0x2, arblost=0x4,
    // timeout=0x8.
    assert!(out.contains("& 0x2U") && out.contains("/* fault nak */"), "nak bit:\n{}", out);
    assert!(out.contains("& 0x8U") && out.contains("/* fault timeout */"), "timeout bit:\n{}", out);
    assert!(out.contains("& 0x4U") && out.contains("/* fault arblost */"), "arblost bit:\n{}", out);
    // The if-chain dispatches on the decoded code.
    assert!(out.contains("__matched"), "match dispatch:\n{}", out);
}

// ─── `match` over a COMPOSED (inlined) op (§3.5/§4.4/D14, audit #35 P3-2) ─────

/// A leaf bus controller (implements an interface) + a **composed** sensor whose
/// op wraps one bus transaction — the std-less fixture for composed-op matches.
/// `{extra_ops}` injects additional sensor ops (e.g. a multi-transaction one).
fn composed(extra_ops: &str) -> String {
    format!(
        r#"
interface bus_if {{
  op read_reg(addr: u32, reg: u32) -> u32 or fault{{nak, timeout, arblost}} yields
}}
device busctl implements bus_if {{
  regs {{
    CR : reg32 at 0x00 access rw {{ start: bit[0], dir: bit[1] }}
    SR : reg32 at 0x04 access ro {{ done: bit[0], nak: bit[1], arblost: bit[2], timeout: bit[3] }}
    SA : reg32 at 0x08 access rw {{}}
    RA : reg32 at 0x0C access rw {{}}
    DR : reg32 at 0x10 access rw {{}}
  }}
  ops {{
    op read_reg(addr: u32, reg: u32) -> u32 or fault{{nak, timeout, arblost}} yields
  }}
}}
device sensor {{
  needs {{ bus : bus_if }}
  ops {{
    op read_temp() -> u32 or fault{{nak, timeout, arblost}} yields {{
      return bus.read_reg(0x76, 0xFA)?
    }}
    {extra_ops}
  }}
}}
board demo {{
  soc s {{ memory {{ flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }} clocks {{ sysclk : clock_source = 64MHz }} }}
  i2c0 : busctl at 0x4000_3000
  env  : sensor {{ needs {{ bus = i2c0 }} }}
}}
program app {{
  use board demo as b
  let dev = b.env
  cell reads : u32 = 0
  cell naks : u32 = 0
  cell timeouts : u32 = 0
  cell arblosts : u32 = 0
  every 1000ms {{
    match dev.read_temp() {{
      ok v          => {{ reads = reads + 1 }}
      fault nak     => {{ naks = naks + 1 }}
      fault timeout => {{ timeouts = timeouts + 1 }}
      fault arblost => {{ arblosts = arblosts + 1 }}
    }}
  }}
}}
sim app_sim for app {{ inject bus_fault nak times 1  inject bus_fault timeout times 1  run until 3500ms }}
"#
    )
}

#[test]
fn match_over_a_composed_op_dispatches_each_outcome() {
    // `dev.read_temp()` is a composed op whose body is `return bus.read_reg(...)?`.
    // The match's outcome rides the single inner bus transaction (P3-2).
    let t = trace(&composed(""));
    let joined = t.join("\n");
    assert!(t.iter().any(|l| l.contains("cell naks = 1")), "nak arm:\n{joined}");
    assert!(t.iter().any(|l| l.contains("cell timeouts = 1")), "timeout arm:\n{joined}");
    assert!(t.iter().any(|l| l.contains("cell reads = 1")), "ok arm:\n{joined}");
}

#[test]
fn match_over_a_composed_op_lowers_the_same_on_metal() {
    // Metal needs no new lowering: the inlined inner BusXfer carries `code_dst`,
    // so the SR-decode + match if-chain are emitted exactly as for a leaf op.
    let sir = compile(&composed(""));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("decode outcome for `match`"), "missing match decode:\n{}", out);
    assert!(out.contains("/* fault nak */") && out.contains("/* fault timeout */"), "fault decode:\n{}", out);
}

#[test]
fn match_over_a_multi_transaction_composed_op_is_rejected() {
    // A composed op with two bus transactions has no single matchable outcome.
    let extra = r#"
    op read2() -> u32 or fault{nak, timeout, arblost} yields {
      let a = bus.read_reg(0x10, 0x00)?
      let c = bus.read_reg(0x10, 0x01)?
      return a + c
    }"#;
    let mut src = composed(extra);
    src = src.replace("match dev.read_temp()", "match dev.read2()");
    let errs = resolve_err(&src);
    assert!(
        errs.iter().any(|m| m.contains("2 bus transactions") || m.contains("single-transaction")),
        "errs: {:?}",
        errs
    );
}
