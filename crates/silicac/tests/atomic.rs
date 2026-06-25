//! §5.5/D03 — the explicit `atomic { }` block: a group of cell updates becomes
//! ONE priority-ceiling critical section, distinct from the per-access critical
//! sections the compiler inserts automatically.

use silicac::sir::{SirModule, SirPlace, SirStmt};
use silicac::{lexer, parser, resolver};

fn resolve(src: &str) -> Result<SirModule, Vec<silicac::diag::Diag>> {
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("std");
    let tokens = lexer::lex(src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    resolver::resolve(&ast)
}

fn count_criticals(stmts: &[SirStmt]) -> usize {
    stmts
        .iter()
        .map(|s| match s {
            SirStmt::Critical { body, .. } => 1 + count_criticals(body),
            SirStmt::If { then, .. } => count_criticals(then),
            _ => 0,
        })
        .sum()
}

#[test]
fn atomic_block_is_a_single_critical_over_all_its_updates() {
    let src = r#"
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
}
program app {
  use board demo as b
  cell x : u32 = 0
  cell y : u32 = 0
  every 1000ms { atomic { x = x + 1  y = y + 1 } }
  every 700ms  { x = x + 2  y = y + 3 }
}
"#;
    let sir = resolve(src).unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()));
    let r0 = &sir.reactions[0]; // the `atomic` reaction
    assert_eq!(count_criticals(&r0.body), 1, "the atomic block is a single critical section");
    // Both updates live inside that one section.
    let crit_body = r0
        .body
        .iter()
        .find_map(|s| if let SirStmt::Critical { body, .. } = s { Some(body) } else { None })
        .expect("a critical section");
    let writes = crit_body
        .iter()
        .filter(|s| matches!(s, SirStmt::Assign { target: SirPlace::Var(_), .. }))
        .count();
    assert_eq!(writes, 2, "both cell updates are inside the one section");
    // The non-atomic reaction gets two *separate* auto-inserted sections instead.
    assert_eq!(count_criticals(&sir.reactions[1].body), 2, "auto-insertion wraps each access separately");
}

#[test]
fn atomic_block_may_not_span_a_yield() {
    // §5.5/D03: a held cell ceiling cannot survive a suspension.
    let src = r#"
board demo {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } }
  i2c0 : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bme280 { needs { bus = i2c0 } }
}
program app {
  use board demo as b
  let sensor = b.env
  cell x : u32 = 0
  every 1000ms { atomic { x = x + 1  let t = sensor.read_temp()? } }
}
"#;
    let errs = resolve(src).expect_err("a yield inside atomic must be rejected");
    assert!(
        errs.iter().any(|e| e.msg.contains("atomic") && e.msg.contains("yielding")),
        "expected an atomic-spans-yield error, got: {:?}",
        errs.iter().map(|d| &d.msg).collect::<Vec<_>>()
    );
}
