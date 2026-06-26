//! §3.2 — `poll <cond> within <d> else fault <code>`: a bounded busy-wait that
//! does NOT yield.  When the condition holds the reaction proceeds; when the
//! bound elapses it raises the fault, which flows to the Layer-2 disposition.

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

/// A USART-style leaf whose `send` op busy-waits on `SR.txe` before writing `DR`.
const USART: &str = r#"
device usart {
  regs {
    SR : reg32 at 0x00 access rw { txe: bit[7] }
    DR : reg32 at 0x04 access rw {}
  }
  ops {
    op set_ready() -> () { SR.txe = 1 }
    op send(b: u32) -> () or fault{timeout} {
      poll SR.txe == 1 within 2ms else fault timeout
      DR = b
    }
  }
}
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  u : usart at 0x4000_4000
}
"#;

fn program(body: &str) -> String {
    format!(
        "{USART}\nprogram app {{\n  use board demo as b\n  let dev = b.u\n  cell sent : u32 = 0\n{body}\n}}\nsim app_sim for app {{ run until 1100ms }}\n"
    )
}

fn trace(src: &str) -> Vec<String> {
    let sir = compile(src);
    sim::run(&sir).render(&sir).lines().map(|s| s.to_string()).collect()
}

#[test]
fn poll_satisfied_lets_the_reaction_proceed() {
    // `set_ready` sets SR.txe, so the poll is satisfied and the send completes.
    let t = trace(&program("  every 1000ms on fault skip {\n    dev.set_ready()\n    dev.send(0x41)?\n    sent = sent + 1\n  }"));
    assert!(t.iter().any(|l| l.contains("poll — satisfied")), "poll satisfied:\n{}", t.join("\n"));
    assert!(t.iter().any(|l| l.contains("cell sent = 1")), "send completed:\n{}", t.join("\n"));
}

#[test]
fn poll_timeout_raises_the_fault_and_runs_the_disposition() {
    // SR.txe is never set → the bound elapses → fault `timeout` → `skip` drops
    // the activation, so the post-poll work never runs.
    let t = trace(&program("  every 1000ms on fault skip {\n    dev.send(0x41)?\n    sent = sent + 1\n  }"));
    assert!(t.iter().any(|l| l.contains("poll — timeout, FAULT timeout")), "poll timed out:\n{}", t.join("\n"));
    assert!(t.iter().any(|l| l.contains("disposition: skip")), "skip disposition fired:\n{}", t.join("\n"));
    assert!(!t.iter().any(|l| l.contains("cell sent = 1")), "the skipped activation must not complete:\n{}", t.join("\n"));
}

#[test]
fn poll_lowers_to_a_bounded_spin_with_disposition_on_metal() {
    let sir = compile(&program("  every 1000ms on fault skip {\n    dev.send(0x41)?\n    sent = sent + 1\n  }"));
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    // A bounded busy-wait over the polled register (SR.txe @ 0x4000_4000 bit 7).
    assert!(out.contains("bounded busy-wait (§3.2)"), "poll comment:\n{out}");
    assert!(out.contains("uint32_t __spins = 0U;") && out.contains("__faulted = 1U; break;"), "bounded spin → fault:\n{out}");
    assert!(out.contains("0x40004000UL") && out.contains(">> 7"), "reads the polled SR.txe field:\n{out}");
    // On timeout the reaction's `skip` disposition runs (drops the activation).
    assert!(out.contains("if (__faulted) {") && out.contains("return; /* skip: drop this activation */"), "skip on timeout:\n{out}");
    // Non-yielding poll path — no IRQ state machine.
    assert!(!out.contains("__BUS_IRQHandler"), "poll does not need the bus IRQ machinery:\n{out}");
}
