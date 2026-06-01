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

const BLINK: &str = r#"
board b {
  soc s {
    memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }
    clocks { sysclk : clock_source = 64MHz }
  }
  gpio0 : nrf_gpio at 0x5000_0000
  led : nrf_gpio.pin = gpio0.pin(13) as output
}
program p {
  use board b as dev
  let led = dev.led
  cell lit : bool = false
  every 500ms { lit = not lit  led.set(lit) }
}
"#;

#[test]
fn every_lowers_to_systick_plan() {
    let sir = compile(BLINK);
    let plan = c::systick_plan(&sir).expect("plan ok").expect("has periodic reactions");
    assert_eq!(plan.reload, 63_999); // 64 MHz / 1000 - 1
    assert_eq!(plan.thresholds.len(), 1);
    assert_eq!(plan.thresholds[0].1, 500); // 500 ms / 1 ms base
}

#[test]
fn every_emits_systick_handler_and_config() {
    let sir = compile(BLINK);
    let src = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(src.contains("void SysTick_Handler(void)"));
    assert!(src.contains("__systick_ctr_"));
    assert!(src.contains("= 500U;"), "per-reaction threshold");
    assert!(src.contains("15 SysTick"), "SysTick in the vector table");
    assert!(src.contains("(void *)&SysTick_Handler"), "vector entry");
    assert!(src.contains("0xE000E014UL = 63999UL"), "SYST_RVR config");
    assert!(src.contains("0xE000E010UL = 0x7UL"), "SYST_CSR enable");
    assert!(src.contains("cpsie i"), "interrupts enabled");
}

#[test]
fn non_whole_millisecond_period_is_an_error() {
    let src = r#"
board b { soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 64MHz } } }
program p { use board b as dev  every 1500us { } }
"#;
    let sir = compile(src);
    let err = c::systick_plan(&sir).expect_err("expected timing error");
    assert!(err.contains("whole"), "got: {err}");
}

#[test]
fn systick_reload_overflow_is_an_error() {
    // A core clock so fast that a 1 ms reload exceeds SysTick's 24-bit counter.
    let src = r#"
board b { soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } clocks { sysclk : clock_source = 20000MHz } } }
program p { use board b as dev  every 500ms { } }
"#;
    let sir = compile(src);
    let err = c::systick_plan(&sir).expect_err("expected reload overflow");
    assert!(err.contains("24 bits"), "got: {err}");
}

const BLINK_BTN: &str = r#"
board b {
  soc s {
    memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }
    clocks { sysclk : clock_source = 64MHz }
  }
  gpio0 : nrf_gpio at 0x5000_0000
  led_user : nrf_gpio.pin = gpio0.pin(13) as output
  btn_user : nrf_gpio.pin = gpio0.pin(11) as input pulling up
}
program p {
  use board b as dev
  let led = dev.led_user
  let button = dev.btn_user
  cell lit : bool = false
  every 500ms { lit = not lit  led.set(lit) }
  on button.falling { lit = not lit  led.set(lit) }
}
"#;

#[test]
fn on_event_emits_gpiote_handler_and_nvic_config() {
    let sir = compile(BLINK_BTN);
    let src = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);

    // GPIOTE IRQ handler + vector entry (#22 = 16 + GPIOTE IRQ 6).
    assert!(src.contains("void GPIOTE_IRQHandler(void)"));
    assert!(src.contains("22 GPIOTE"));
    assert!(src.contains("(void *)&GPIOTE_IRQHandler"));
    // GPIOTE channel config: event mode, pin 11, HiToLo polarity (0x20b01).
    assert!(src.contains("0x20b01UL"), "GPIOTE CONFIG:\n{src}");
    // NVIC enable of IRQ 6 (bit 6 = 0x40) + input pull-up (PIN_CNF).
    assert!(src.contains("0xE000E100UL = 0x40UL"), "NVIC ISER0");
    assert!(src.contains("PIN_CNF"), "input pull config");
}

#[test]
fn shared_cell_critical_lowers_to_basepri_ceiling() {
    // `lit` is shared by the timer and the button (different priorities), so the
    // critical section masks to the ceiling via BASEPRI (§5.5).
    let sir = compile(BLINK_BTN);
    let src = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);

    assert!(src.contains("#define __set_BASEPRI"));
    assert!(src.contains("__bp_saved = __get_BASEPRI()"), "save BASEPRI");
    assert!(src.contains("__set_BASEPRI(0x20U)"), "raise to ceiling (button prio)");
    assert!(src.contains("__set_BASEPRI(__bp_saved)"), "restore BASEPRI");
    // Distinct interrupt priorities so the ceiling is meaningful: GPIOTE (button)
    // more urgent (0x20) than SysTick (timer, 0x40).
    assert!(src.contains("0x20U; /* NVIC IPR IRQ6 priority */"));
    assert!(src.contains("0x40U; /* SysTick priority */"));
}

#[test]
fn metal_emits_layer3_fault_decoder() {
    // The metal target emits the address-ownership table + a HardFault handler
    // that reads BFAR and records the decoded owner (§5.4).
    let sir = compile(BLINK_BTN);
    let src = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);

    assert!(src.contains("void HardFault_Handler(void)"));
    assert!(src.contains("3  HardFault") && src.contains("(void *)&HardFault_Handler"), "vector entry");
    assert!(src.contains("__owner_start") && src.contains("__owner_end"), "ownership table");
    assert!(src.contains("0xE000ED38UL"), "reads SCB BFAR");
    assert!(src.contains("__fault_addr") && src.contains("__fault_owner"), "fault record");
    // gpio0's MMIO region is in the table (base 0x5000_0000).
    assert!(src.contains("0x50000000U"), "device region in ownership map");
}

#[test]
fn mmio_access_width_follows_register_width() {
    // An 8-bit register must lower to a `volatile uint8_t *` access, not uint32_t
    // (wrong size / misaligned MMIO otherwise).
    let src = r#"
device tiny {
  regs { CTRL : reg8 at 0x0 access rw {} }
  ops { op set(level: bool) -> () {} }
}
board b {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } }
  t0 : tiny at 0x4000_0000
  led : tiny.pin = t0.pin(2) as output
}
program p {
  use board b as dev
  let led = dev.led
  on sys.start { led.set(true) }
}
"#;
    let sir = compile(src);
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("volatile uint8_t *__p"), "8-bit MMIO access:\n{out}");
    assert!(!out.contains("volatile uint32_t *__p = (volatile uint32_t *)0x40000000UL"),
        "must not use a 32-bit access for an 8-bit register");
}

#[test]
fn reg_load_lowers_to_volatile_masked_read() {
    // A register *read* (`button.get()` on an input pin) lowers to a volatile
    // MMIO load masked/shifted to the field — the read counterpart of the store
    // lowering, matching the simulator's `(reg & mask) >> shift` exactly. No
    // read-modify-write and no `0U` stub.
    let src = r#"
board b {
  soc s {
    memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }
    clocks { sysclk : clock_source = 64MHz }
  }
  gpio0 : nrf_gpio at 0x5000_0000
  btn : nrf_gpio.pin = gpio0.pin(11) as input pulling up
}
program p {
  use board b as dev
  let button = dev.btn
  cell state : bool = false
  every 500ms { state = button.get() }
}
"#;
    let sir = compile(src);
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    // IN register of gpio0 @ 0x5000_0000 + 0x510, pin 11 → mask 0x800, shift 11.
    assert!(out.contains("*(volatile uint32_t *)0x50000510UL"), "volatile MMIO read of IN:\n{out}");
    assert!(out.contains("& 0x800UL"), "field mask for pin 11:\n{out}");
    assert!(out.contains(">> 11"), "field shift for pin 11:\n{out}");
    // The metal stub must be gone.
    assert!(!out.contains("TODO(metal): MMIO load"), "RegLoad stub must be lowered:\n{out}");
}

#[test]
fn missing_mmio_base_is_a_hard_error() {
    // A device instance without `at <addr>` must not silently lower to address 0.
    let src = r#"
device tiny {
  regs { CTRL : reg32 at 0x0 access rw {} }
  ops { op set(level: bool) -> () {} }
}
board b {
  soc s { memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K } }
  t0 : tiny
  led : tiny.pin = t0.pin(2) as output
}
program p {
  use board b as dev
  let led = dev.led
  on sys.start { led.set(true) }
}
"#;
    let sir = compile(src);
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("#error") && out.contains("no MMIO base address"),
        "expected a #error for the missing base:\n{out}");
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
