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

const SENSOR: &str = r#"
board b {
  soc s {
    memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }
    clocks { sysclk : clock_source = 64MHz }
  }
  i2c0 : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bme280 { needs { bus = i2c0 } }
}
program app {
  use board b as bd
  let sensor = bd.env
  cell samples : u32 = 0
  every 1000ms on fault retry(max = 3) {
    let t = sensor.read_temp()?
    samples += 1
  }
}
"#;

#[test]
fn bus_xfer_lowers_to_irq_driven_segment_state_machine() {
    // A composed-device read (`sensor.read_temp()` → `i2c read_reg`) lowers to an
    // IRQ-driven segment state machine (§5.2): a static frame, a dispatcher that
    // kicks the transaction over the controller's *declared* registers
    // (CR/SR/SA/RA/DR, base 0x4000_3000) and suspends, and a bus IRQ that resumes
    // it.  No busy-wait — the scheduler runs other work while it is in flight.
    let sir = compile(SENSOR);
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    // A static frame holds the dispatcher state across the suspension.
    assert!(out.contains("static volatile struct {") && out.contains("} __rf_0 = {0};"), "frame struct:\n{out}");
    assert!(out.contains("static void __react_0_run(void)") && out.contains("switch (__rf_0.__state)"), "segment dispatcher:\n{out}");
    // Segment 0 kicks over the controller's declared register offsets…
    assert!(out.contains("0x40003008UL = (uint32_t)(118ULL); /* SA */"), "slave addr 0x76 to SA:\n{out}");
    assert!(out.contains("0x4000300cUL = (uint32_t)(250ULL); /* RA */"), "reg 0xFA to RA:\n{out}");
    assert!(out.contains("0x40003000UL = (__I2C_CR_START | __I2C_CR_DIR_RD)"), "CR kick (read):\n{out}");
    // …arms the completion IRQ, records the owner, and suspends.
    assert!(out.contains("__bus_owner = (int32_t)0;"), "records bus owner:\n{out}");
    assert!(out.contains("__bus_irq_enable();"), "arms completion IRQ:\n{out}");
    assert!(out.contains("__rf_0.__state = 1U;") && out.contains("return; /* suspend on the bus"), "suspend after kick:\n{out}");
    // The resumed segment reads the result; success requires `done` AND no error.
    assert!(out.contains("0x40003004UL; /* SR */"), "read SR on resume:\n{out}");
    assert!(out.contains("0x40003010UL; /* DR (read result) */"), "read DR on done:\n{out}");
    assert!(out.contains("(__sr & __I2C_SR_DONE) && !(__sr & __I2C_SR_ERR)"), "done-without-error success:\n{out}");
    assert!(out.contains("__DMB();"), "ordering barriers around the kick:\n{out}");
    // The trigger entry coalesces a re-fire while still in flight (§5.1).
    assert!(out.contains("if (__rf_0.__state != 0U) return; /* coalesce"), "coalesce guard:\n{out}");
    // The bus completion IRQ resumes the owner's dispatcher, and is vectored.
    assert!(out.contains("void __BUS_IRQHandler(void)") && out.contains("case 0: __react_0_run();"), "bus IRQ resumes owner:\n{out}");
    assert!(out.contains("__BUS_IRQHandler,") && out.contains("bus completion"), "bus IRQ vectored:\n{out}");
    // The old bounded busy-wait is gone.
    assert!(!out.contains("__I2C_POLL_BOUND") && !out.contains("__spins"), "busy-wait must be gone:\n{out}");
}

#[test]
fn propagated_bus_fault_lowers_to_the_reaction_disposition() {
    // On a propagated fault at the resumed segment, `retry(max = 3)` re-runs from
    // segment 0 (re-kicking the transaction) until the bounded retry count is
    // exhausted, then escalates — mirroring the simulator's `dispose`, but across
    // the IRQ-driven suspension rather than a busy loop.
    let sir = compile(SENSOR);
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    // The fault is detected in the resumed segment, guarded on the frame flag.
    assert!(out.contains("if (__rf_0.__faulted) {"), "fault guard on the frame flag:\n{out}");
    // Retry re-runs from segment 0 without resetting the retry counter.
    assert!(out.contains("if (__rf_0.__retry < 3U) { __rf_0.__retry++; __rf_0.__faulted = 0U; goto __seg_0_0; }"), "bounded retry re-kicks:\n{out}");
    assert!(out.contains("__rf_0.__state = 0U; return; /* retries exhausted → escalate */"), "escalate after exhaustion:\n{out}");
}

#[test]
fn safe_disposition_drives_safe_state_on_metal() {
    // A `safe` disposition over a bus fault calls the emitted `__drive_safe`,
    // which runs each device's bounded safe-op register writes, then holds.
    let src = r#"
device motor {
  regs { CTRL : reg32 at 0x00 access rw { enable: bit[0] } }
  states { running, off }
  safe_state = off
  ops { op run() -> () { CTRL.enable = 1 }  op safe() -> () { CTRL.enable = 0 } }
}
board b {
  soc s {
    memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }
    clocks { sysclk : clock_source = 64MHz }
  }
  i2c0 : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bme280 { needs { bus = i2c0 } }
  m    : motor at 0x5001_0000
}
program app {
  use board b as bd
  let sensor = bd.env
  let mot = bd.m
  on sys.start { mot.run() }
  every 1000ms on fault safe { let t = sensor.read_temp()? }
}
"#;
    let sir = compile(src);
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("static void __drive_safe(void)"), "drive_safe emitted:\n{out}");
    // The motor's safe op (CTRL.enable = 0) writes its MMIO @ 0x5001_0000.
    assert!(out.contains("(volatile uint32_t *)0x50010000UL"), "safe register write:\n{out}");
    assert!(out.contains("__drive_safe();"), "disposition calls drive_safe:\n{out}");
    assert!(out.contains("for (;;) { __asm__ volatile(\"wfi\"); }"), "hold after safe:\n{out}");
    // Interrupts must be masked BEFORE driving safe-state (no concurrent ISR);
    // the cpsid must precede the __drive_safe() call.
    let cpsid = out.find("cpsid i").expect("cpsid emitted");
    let drive = out.find("__drive_safe();").expect("drive_safe call");
    assert!(cpsid < drive, "interrupts masked before driving safe-state:\n{out}");
}

#[test]
fn safe_disposition_without_a_safe_device_still_defines_drive_safe() {
    // `on fault safe` with no device declaring a `safe_state` leaves `safe_seqs`
    // empty.  The disposition still calls `__drive_safe()`, so the function must
    // be defined (with an empty body) — otherwise the firmware fails to link.
    let src = r#"
board b {
  soc s {
    memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }
    clocks { sysclk : clock_source = 64MHz }
  }
  i2c0 : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bme280 { needs { bus = i2c0 } }
}
program app {
  use board b as bd
  let sensor = bd.env
  every 1000ms on fault safe { let t = sensor.read_temp()? }
}
"#;
    let sir = compile(src);
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    assert!(out.contains("static void __drive_safe(void)"), "drive_safe must be defined even with no safe_seqs:\n{out}");
    assert!(out.contains("__drive_safe();"), "disposition calls drive_safe:\n{out}");
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

/// A composed sensor over the **SPI** bus (a second bus interface) lowers its
/// `BusXfer` through the same generic bounded-poll path as I²C — proving the
/// metal bus lowering is not special-cased to one interface (§3.5/D1).
const SENSOR_SPI: &str = r#"
device bmp280_spi {
  needs { bus : spi }
  ops {
    op read_temp() -> u32 or fault{timeout, overrun} yields {
      return bus.read_reg(0, 0xFA)?
    }
  }
}
board b {
  soc s {
    memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }
    clocks { sysclk : clock_source = 64MHz }
  }
  spi0 : spi_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bmp280_spi { needs { bus = spi0 } }
}
program app {
  use board b as bd
  let sensor = bd.env
  cell samples : u32 = 0
  every 1000ms on fault retry(max = 3) {
    let t = sensor.read_temp()?
    samples += 1
  }
}
"#;

#[test]
fn spi_bus_xfer_lowers_to_irq_driven_segment_state_machine() {
    let sir = compile(SENSOR_SPI);
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    // Same IRQ-driven segment lowering as i2c, over the spi controller's declared
    // registers: cs = 0 → SA; remote reg 0xFA (250) → RA; base 0x4000_3000.
    assert!(out.contains("static void __react_0_run(void)") && out.contains("switch (__rf_0.__state)"), "segment dispatcher:\n{out}");
    assert!(out.contains("0x40003008UL = (uint32_t)(0ULL); /* SA */"), "cs 0 to SA:\n{out}");
    assert!(out.contains("0x4000300cUL = (uint32_t)(250ULL); /* RA */"), "reg 0xFA to RA:\n{out}");
    assert!(out.contains("0x40003000UL = (__I2C_CR_START | __I2C_CR_DIR_RD)"), "CR kick (read):\n{out}");
    assert!(out.contains("__bus_irq_enable();") && out.contains("return; /* suspend on the bus"), "arms IRQ + suspends:\n{out}");
    assert!(out.contains("0x40003004UL; /* SR */"), "read SR on resume:\n{out}");
    assert!(out.contains("0x40003010UL; /* DR (read result) */"), "read DR on done:\n{out}");
    assert!(out.contains("void __BUS_IRQHandler(void)"), "bus IRQ handler emitted:\n{out}");
    // No busy-wait, no unlowered-device-op stub.
    assert!(!out.contains("__I2C_POLL_BOUND") && !out.contains("__spins"), "busy-wait must be gone:\n{out}");
    assert!(!out.contains("not yet lowered"), "no unlowered-device-op stub:\n{out}");
}

/// §5.6 (E5) — the system watchdog is configured + started at boot and fed on a
/// clean return to idle, gated on no yielding reaction being mid-transaction.
const WDT_SENSOR: &str = r#"
board b {
  soc s {
    memory { flash : region at 0x0 size 1024K   ram : region at 0x2000_0000 size 256K }
    clocks { sysclk : clock_source = 64MHz }
  }
  i2c0 : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bme280 { needs { bus = i2c0 } }
  wdt0 : wdt at 0x4001_0000 { config { timeout = 100ms } }
}
program app {
  use board b as bd
  let sensor = bd.env
  cell samples : u32 = 0
  every 1000ms { let t = sensor.read_temp()?  samples = samples + 1 }
}
"#;

#[test]
fn watchdog_is_configured_and_fed_only_when_idle_on_metal() {
    let sir = compile(WDT_SENSOR);
    let out = c::CBackend::with_target(Target::MetalNrf52840).emit(&sir);
    // Boot: reload (100ms) → RLR, start → CR, initial feed → KR, off base 0x4001_0000.
    assert!(out.contains("0x40010004UL = 100UL; /* RLR: reload (ms) */"), "reload from timeout:\n{out}");
    assert!(out.contains("0x40010000UL = 0x1UL;  /* CR: start */"), "start:\n{out}");
    assert!(out.contains("0x40010008UL = 0xAAAAUL; /* KR: feed */"), "initial feed:\n{out}");
    // Idle loop feeds KR only when the yielding reaction's frame is idle (§5.6):
    // a hung/suspended reaction (state != 0) is never fed → the watchdog resets.
    assert!(out.contains("if (__rf_0.__state == 0U) { *(volatile uint32_t *)0x40010008UL = 0xAAAAUL; }"), "idle-gated feed:\n{out}");
}
