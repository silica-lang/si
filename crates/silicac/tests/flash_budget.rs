//! §5.3 — flash / code-size budget gate (audit #35, P1-3).  Hermetic tests of
//! the `arm-none-eabi-size` output parser, the enforce check, and the flash
//! region lookup; the end-to-end gate is `harness/flash_budget.sh`.

use silicac::backend::c;
use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver};

fn compile(src: &str) -> SirModule {
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("std");
    let tokens = lexer::lex(src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()))
}

// Verbatim Berkeley-format output from arm-none-eabi-size.
const SIZE_OUT: &str = "   text\t   data\t    bss\t    dec\t    hex\tfilename\n\
                        512\t      8\t     16\t    536\t    218\t/tmp/b.elf\n";

#[test]
fn parses_size_output() {
    assert_eq!(c::parse_size(SIZE_OUT), Some((512, 8, 16)));
    assert_eq!(c::parse_size("no numbers here\n"), None);
}

#[test]
fn enforce_flash_passes_and_fails() {
    // .text+.rodata + .data = 520 fits 1 MiB.
    assert_eq!(c::enforce_flash(512, 8, 1 << 20).unwrap(), 520);
    // Over a tiny flash region: hard error.
    let err = c::enforce_flash(2000, 8, 1024).unwrap_err();
    assert!(err.contains("flash budget exceeded"), "got: {err}");
}

#[test]
fn flash_region_size_comes_from_board_memory() {
    let sir = compile(
        "board b { soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } } }\nprogram p { use board b as d  on sys.start { } }",
    );
    assert_eq!(c::flash_region_size(&sir), Some(1024 * 1024));
}
