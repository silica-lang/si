//! Phase 1b: scheduler-fed hardware watchdog (§5.6/SIL-006).  The scheduler feeds
//! it on a clean return to idle; a reaction that hangs starves it → reset.

use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver, sim};

fn resolve_str(src: &str) -> SirModule {
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("std");
    let tokens = lexer::lex(src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()))
}

fn board(extra_sim: &str) -> String {
    format!(
        r#"
board rig {{
  soc s {{ memory {{ flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }} clocks {{ sysclk : clock_source = 64MHz }} }}
  i2c0 : i2c_controller at 0x4000_3000 {{ needs {{ clock = soc.sysclk }} }}
  env  : bme280 {{ needs {{ bus = i2c0 }} }}
  wdt0 : wdt at 0x4001_0000 {{ config {{ timeout = 100ms }} }}
}}
program app {{
  use board rig as r
  let sensor = r.env
  every 1000ms {{ let t = sensor.read_temp()? }}
}}
sim app_sim for app {{ {extra_sim}   run until 1500ms }}
"#
    )
}

fn lines(sir: &SirModule) -> Vec<String> {
    sim::run(sir).render(sir).lines().map(str::to_string).collect()
}

#[test]
fn the_watchdog_picks_up_the_timeout_from_the_board() {
    let sir = resolve_str(&board(""));
    assert_eq!(sir.watchdog_timeout_ns, Some(100_000_000));
}

#[test]
fn a_hung_reaction_starves_the_watchdog_and_resets() {
    let sir = resolve_str(&board("inject bus_hang times 1"));
    let ls = lines(&sir);
    let joined = ls.join("\n");
    assert!(joined.contains("bus i2c0.read_reg() — suspend"), "the read started:\n{joined}");
    assert!(joined.contains("WATCHDOG RESET"), "the hung handler reset the chip:\n{joined}");
    // The reset is one timeout (100 ms) after the read began (1000 ms).
    assert!(ls.iter().any(|l| l.contains("WATCHDOG RESET") && l.contains("1100.000ms")), "{joined}");
}

#[test]
fn more_than_one_watchdog_is_an_error() {
    let src = r#"
board rig {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } }
  wdt0 : wdt at 0x4001_0000 { config { timeout = 100ms } }
  wdt1 : wdt at 0x4001_1000 { config { timeout = 50ms } }
}
program app { use board rig as r  on sys.start { } }
"#;
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("std");
    let tokens = lexer::lex(src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    let errs = resolver::resolve(&ast).expect_err("expected a multiple-watchdog error");
    assert!(errs.iter().any(|e| e.msg.contains("more than one watchdog")),
        "got: {:?}", errs.iter().map(|e| &e.msg).collect::<Vec<_>>());
}

#[test]
fn a_healthy_reaction_feeds_the_watchdog_so_it_never_fires() {
    // No hang: the read completes in 2 µs, the scheduler returns to idle and
    // feeds the watchdog, so it never resets.
    let sir = resolve_str(&board(""));
    let joined = lines(&sir).join("\n");
    assert!(joined.contains("bus i2c0.read_reg() — done, resume"), "the read completed:\n{joined}");
    assert!(!joined.contains("WATCHDOG RESET"), "a fed watchdog must not fire:\n{joined}");
}
