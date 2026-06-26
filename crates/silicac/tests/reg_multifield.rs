//! §4.2 — multi-field single register write `REG{ a = .., b = .. }` (audit #35,
//! P0-2c).  Several fields of one register update in ONE store (a single masked
//! write for w1c/wo fields, else one read-modify-write over the union mask),
//! instead of a separate RMW per field.

use silicac::backend::{c, Target};
use silicac::sim;
use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver};

fn resolve_err(src: &str) -> Vec<String> {
    let tokens = lexer::lex(src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    match resolver::resolve(&ast) {
        Ok(_) => panic!("expected a resolve error, got success"),
        Err(e) => e.iter().map(|d| d.msg.clone()).collect(),
    }
}

fn compile(src: &str) -> SirModule {
    let tokens = lexer::lex(src).expect("lex");
    let ast = parser::parse(tokens).expect("parse");
    resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()))
}

const BOARD: &str = r#"
board b {
  soc nrf52840 {
    memory { flash : region at 0x0 size 256K   ram : region at 0x2000_0000 size 64K }
    clocks { sysclk : clock_source = 64MHz }
  }
  dev0 : d at 0x40000000
}
"#;

#[test]
fn rw_multifield_is_one_rmw_over_the_union_mask() {
    // enable=bit0, rxneie=bit5 → union mask 0x21.  Both ops are called so both
    // stores are emitted; only the rw CR1 store does an RMW, the w1c ACK does not.
    let dev = "device d { regs { CR1 : reg32 at 0x0 access rw { enable: bit[0], rxneie: bit[5] }  ACK : reg32 at 0x4 access rw { f0: bit[0] access w1c, f1: bit[1] access w1c } } ops { op cfg() -> () { CR1{ enable = 1, rxneie = 1 } }  op ack() -> () { ACK{ f0 = 1, f1 = 1 } } } }";
    let src = format!("{dev}\n{BOARD}\nprogram p {{\n  use board b as bb\n  let x = bb.dev0\n  on sys.start {{ x.cfg()  x.ack() }}\n}}\n");
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&compile(&src));

    assert!(out.contains("multi-field store"), "multi-field store emitted:\n{out}");
    // One combined RMW (CR1), not two separate ones — and the w1c ACK adds none.
    assert_eq!(out.matches("__v = *__p").count(), 1, "exactly one RMW total:\n{out}");
    assert!(out.contains("~0x21UL"), "RMW clears the union mask (bit0|bit5):\n{out}");
    // The w1c ACK store is a single write (no read) covering both bits.
    assert!(out.contains("0x40000004UL"), "ACK store present:\n{out}");
}

#[test]
fn sim_applies_all_fields() {
    let dev = "device d { regs { CR : reg32 at 0x0 access rw { en: bit[0], mode: field[2:1] } } ops { op set() -> () { CR{ en = 1, mode = 3 } }  op rd_en() -> u32 { return CR.en }  op rd_mode() -> u32 { return CR.mode } } }";
    let src = format!(
        "{dev}\n{BOARD}\nprogram p {{\n  use board b as bb\n  let x = bb.dev0\n  cell a : u32 = 0\n  cell m : u32 = 0\n  on sys.start {{ x.set()  a = x.rd_en()  m = x.rd_mode() }}\n}}\nsim s for p {{ run until 1ms }}\n"
    );
    let out = sim::run(&compile(&src)).render(&compile(&src));
    assert!(out.contains("cell a = 1"), "enable bit set:\n{out}");
    assert!(out.contains("cell m = 3"), "mode field set to 3:\n{out}");
}

#[test]
fn unknown_field_in_a_multifield_write_is_an_error() {
    let dev = "device d { regs { CR : reg32 at 0x0 access rw { en: bit[0] } } ops { op bad() -> () { CR{ bogus = 1 } } } }";
    let src = format!("{dev}\n{BOARD}\nprogram p {{ use board b as bb  let x = bb.dev0  on sys.start {{ x.bad() }} }}\n");
    assert!(resolve_err(&src).iter().any(|e| e.contains("has no field 'bogus'")));
}

#[test]
fn writing_a_read_only_field_in_a_multifield_write_is_an_error() {
    let dev = "device d { regs { CR : reg32 at 0x0 access rw { st: bit[0] access ro, en: bit[1] } } ops { op bad() -> () { CR{ st = 1, en = 1 } } } }";
    let src = format!("{dev}\n{BOARD}\nprogram p {{ use board b as bb  let x = bb.dev0  on sys.start {{ x.bad() }} }}\n");
    assert!(resolve_err(&src).iter().any(|e| e.contains("cannot write read-only field")));
}

#[test]
fn multifield_rmw_of_a_read_side_effect_register_is_an_error() {
    // An rc register: a multi-field write that needs a read would disturb it.
    let dev = "device d { regs { ST : reg32 at 0x0 access rc { a: bit[0], b: bit[1] } } ops { op bad() -> () { ST{ a = 1, b = 1 } } } }";
    let src = format!("{dev}\n{BOARD}\nprogram p {{ use board b as bb  let x = bb.dev0  on sys.start {{ x.bad() }} }}\n");
    assert!(resolve_err(&src).iter().any(|e| e.contains("read-side-effect")));
}
