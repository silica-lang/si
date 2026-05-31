//! Stage-A on-metal backend: hermetic checks on the generated freestanding C,
//! the linker script, and the RAM-budget gate (no arm-none-eabi-gcc / Renode
//! needed — those are exercised by spike/run.sh and documented in the scope).

use silicac::backend::{c, Target};
use silicac::sir::SirModule;
use silicac::{lexer, parser, resolver};

const BOOT: &str = r#"
board nrf52840_dk {
  soc nrf52840 {
    memory {
      flash : region at 0x0 size 1024K
      ram   : region at 0x2000_0000 size 256K
    }
  }
}
program boot_test {
  use board nrf52840_dk as dev
  cell value : u32 = 7
  on sys.start { value = 42 }
}
"#;

fn compile(src: &str) -> SirModule {
    let std_items = silicac::load_std_items(&silicac::default_std_dir()).expect("std");
    let tokens = lexer::lex(src).expect("lex");
    let mut ast = parser::parse(tokens).expect("parse");
    ast.items.splice(0..0, std_items);
    resolver::resolve(&ast)
        .unwrap_or_else(|e| panic!("resolve: {:?}", e.iter().map(|d| &d.msg).collect::<Vec<_>>()))
}

#[test]
fn linker_script_comes_from_board_memory() {
    let sir = compile(BOOT);
    let ld = c::emit_linker_script(&sir).expect("linker script");
    assert!(ld.contains("ORIGIN = 0x00000000, LENGTH = 1048576"), "flash region:\n{ld}");
    assert!(ld.contains("ORIGIN = 0x20000000, LENGTH = 262144"), "ram region:\n{ld}");
    assert!(ld.contains("ENTRY(Reset_Handler)"));
    assert!(ld.contains("_estack = ORIGIN(RAM) + LENGTH(RAM);"));
    assert!(ld.contains("_sidata = LOADADDR(.data);"));
}

#[test]
fn metal_c_is_freestanding_with_vectors_and_startup() {
    let sir = compile(BOOT);
    let src = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);

    // Generated startup pieces (§6.4).
    assert!(src.contains("section(\".vectors\")"), "vector table section");
    assert!(src.contains("void Reset_Handler(void)"));
    assert!(src.contains("&_sidata") && src.contains("&_edata"), ".data copy loop");
    assert!(src.contains("&_sbss") && src.contains("&_ebss"), ".bss zero loop");
    assert!(src.contains("__reaction_0();"), "sys.start dispatched from reset");
    assert!(src.contains("wfi"), "idle loop");
    assert!(src.contains("static volatile uint32_t value"), "cell is volatile on metal");

    // Freestanding: no libc / host runtime (§6.2).
    for forbidden in ["stdio.h", "stdlib.h", "nanosleep", "fwrite", "int main"] {
        assert!(!src.contains(forbidden), "metal C must not contain `{forbidden}`");
    }
}

#[test]
fn ram_budget_within_region() {
    let sir = compile(BOOT);
    let b = c::ram_budget(&sir).expect("budget");
    assert_eq!(b.statics, 4); // one u32 cell
    assert_eq!(b.stack_reserve, c::STACK_RESERVE);
    assert_eq!(b.ram_size, 262144);
    assert!(b.used() < b.ram_size);
}

#[test]
fn ram_budget_exceeded_is_an_error() {
    // A RAM region too small to hold even the reserved stack.
    let src = r#"
board tiny {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 16 } }
}
program p {
  use board tiny as dev
  cell a : u64 = 0
  on sys.start { a = 1 }
}
"#;
    let sir = compile(src);
    let err = c::ram_budget(&sir).expect_err("expected budget overflow");
    assert!(err.contains("RAM budget exceeded"), "got: {err}");
}

const GPIO: &str = r#"
board b {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } }
  gpio0 : nrf_gpio at 0x5000_0000
  led : nrf_gpio.pin = gpio0.pin(13) as output
}
program p {
  use board b as dev
  let led = dev.led
  on sys.start { led.set(true) }
}
"#;

#[test]
fn reg_access_lowers_to_ordered_mmio_with_barriers() {
    // Validation gate #3 (§4.2/§6.2): register access is a volatile masked
    // store bracketed by barriers, and output-pin direction is configured at
    // startup.
    let sir = compile(GPIO);
    let src = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);

    assert!(src.contains("#define __DMB()"), "barrier macro defined");
    // OUT register of gpio0 @ 0x5000_0000 + 0x504.
    assert!(src.contains("0x50000504UL"), "MMIO store to OUT:\n{src}");
    assert!(src.contains("volatile uint32_t *__p"), "volatile pointer access");
    assert!(src.contains("__DMB();"), "ordering barrier around the store");
    // Direction config writes DIR @ +0x514.
    assert!(src.contains("configure output pin directions"));
    assert!(src.contains("0x50000514UL"), "DIR config write:\n{src}");
}

#[test]
fn host_target_still_emits_hosted_main() {
    // The host path is unchanged: it still produces a libc `main`, proving the
    // target switch did not regress the existing consumer.
    let sir = compile(BOOT);
    let src = c::CBackend::with_target(Target::Host).emit(&sir);
    assert!(src.contains("int main(void)"));
    assert!(!src.contains("Reset_Handler"));
}
