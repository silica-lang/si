//! §4.2/D04 — register access semantics (audit #35, P0-2a).
//!
//! Per-field access is threaded resolver→SIR→backend, and illegal directions
//! (writing `ro`, reading `wo`) are compile errors.  The codegen test proves a
//! `w1c` *field* inside an `rw` register reaches the backend as a single masked
//! write (no read-modify-write that would clobber its sibling status bits).

use silicac::backend::{c, Target};
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

fn emit_metal(src: &str) -> String {
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("std");
    let tokens = lexer::lex(src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    let sir: SirModule = resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()));
    c::CBackend::with_target(Target::MetalNrf52840).emit(&sir)
}

const BOARD: &str = r#"
board b {
  soc nrf52840 {
    memory { flash : region at 0x0 size 256K   ram : region at 0x2000_0000 size 64K }
    clocks { sysclk : clock_source = 64MHz }
  }
  dev0 : statusdev at 0x40000000
}
"#;

/// Wrap a `statusdev` device definition + a program that calls `op_name` so the
/// op body is actually lowered (uncalled ops are never lowered).
fn program(device: &str, op_name: &str) -> String {
    format!(
        "{device}\n{BOARD}\nprogram p {{\n  use board b as bb\n  let d = bb.dev0\n  on sys.start {{ d.{op_name}() }}\n}}\n"
    )
}

#[test]
fn writing_a_read_only_register_is_an_error() {
    let dev = "device statusdev { regs { R : reg32 at 0x0 access ro {} } ops { op bad() -> () { R = 1 } } }";
    let errs = resolve_err(&program(dev, "bad"));
    assert!(errs.iter().any(|e| e.contains("cannot write read-only register")), "{errs:?}");
}

#[test]
fn reading_a_write_only_register_is_an_error() {
    let dev = "device statusdev { regs { W : reg32 at 0x0 access wo {} } ops { op bad() -> u32 { return W } } }";
    let errs = resolve_err(&program(dev, "bad"));
    assert!(errs.iter().any(|e| e.contains("cannot read write-only register")), "{errs:?}");
}

#[test]
fn writing_a_read_only_field_is_an_error() {
    let dev = "device statusdev { regs { SR : reg32 at 0x0 access rw { st: bit[0] access ro } } ops { op bad() -> () { SR.st = 1 } } }";
    let errs = resolve_err(&program(dev, "bad"));
    assert!(errs.iter().any(|e| e.contains("cannot write read-only field")), "{errs:?}");
}

#[test]
fn reading_a_write_only_field_is_an_error() {
    let dev = "device statusdev { regs { CR : reg32 at 0x0 access rw { k: bit[0] access wo } } ops { op bad() -> u32 { return CR.k } } }";
    let errs = resolve_err(&program(dev, "bad"));
    assert!(errs.iter().any(|e| e.contains("cannot read write-only field")), "{errs:?}");
}

#[test]
fn per_field_w1c_in_an_rw_register_lowers_to_a_single_write() {
    // A `w1c` field overriding its `rw` register: writing it must NOT read-
    // modify-write (which would clobber other status bits) — proving the
    // per-field access reaches the backend.
    let w1c = "device statusdev { regs { SR : reg32 at 0x0 access rw { flag: bit[0] access w1c } } ops { op clearflag() -> () { SR.flag = 1 } } }";
    let out = emit_metal(&program(w1c, "clearflag"));
    assert!(out.contains("0x40000000UL"), "store to SR:\n{out}");
    assert!(!out.contains("__v = *__p"), "w1c field must be a single write, not RMW:\n{out}");

    // Contrast: the same field with no override inherits `rw` → a real RMW.
    let rw = "device statusdev { regs { SR : reg32 at 0x0 access rw { flag: bit[0] } } ops { op clearflag() -> () { SR.flag = 1 } } }";
    let out_rw = emit_metal(&program(rw, "clearflag"));
    assert!(out_rw.contains("__v = *__p"), "plain rw field is a read-modify-write:\n{out_rw}");
}
