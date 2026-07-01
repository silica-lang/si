//! §8 / audit #35 P7-8a/b — DTS→Silica importer.  Parses a flat `.dts` subset
//! and emits a `board`/`soc` skeleton: memory regions from `reg`, the clock from
//! `clock-frequency`, gpio pin bindings from `leds`/`buttons` groups, and device
//! instances from `compatible` (mapped where a Silica device type exists, else a
//! **diagnosed** raw stub — never a silent drop, §8/D10).  The committed
//! `nrf52840dk.dts` round-trips the real `nrf52840_dk` board.

use silicac::dts::{self, Cell, DtsValue};

fn example_dts() -> String {
    std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/dts_examples/nrf52840dk.dts"))
        .expect("read example dts")
}

fn imported() -> String {
    let root = dts::parse(&example_dts()).expect("parse");
    dts::to_silica(&root, &dts::known_device_types()).board_si
}

#[test]
fn parses_the_node_tree_with_labels_addrs_and_props() {
    let root = dts::parse(&example_dts()).expect("parse");
    assert_eq!(root.name, "/");
    let soc = root.children.iter().find(|c| c.name == "soc").expect("soc node");
    let gpio = soc.children.iter().find(|c| c.label.as_deref() == Some("gpio0")).expect("gpio0");
    assert_eq!(gpio.name, "gpio");
    assert_eq!(gpio.unit_addr, Some(0x5000_0000));
    assert_eq!(
        gpio.props.iter().find(|(n, _)| n == "reg").map(|(_, v)| v),
        Some(&DtsValue::Cells(vec![Cell::Num(0x5000_0000), Cell::Num(0x1000)]))
    );
}

#[test]
fn gpios_property_captures_the_phandle_and_cells() {
    // A `gpios = <&gpio0 13 0>` value keeps the controller phandle + numeric cells.
    let root = dts::parse(&example_dts()).expect("parse");
    let leds = root.children.iter().find(|c| c.name == "leds").expect("leds group");
    let led = &leds.children[0];
    let gpios = led.props.iter().find(|(n, _)| n == "gpios").map(|(_, v)| v).expect("gpios");
    assert_eq!(
        gpios,
        &DtsValue::Cells(vec![Cell::Phandle("gpio0".into()), Cell::Num(13), Cell::Num(0)])
    );
}

#[test]
fn imports_memory_soc_name_and_clock() {
    let si = imported();
    // board name from `model`; soc name from the soc `compatible` part.
    assert!(si.contains("board nrf52840_dk {"), "board name:\n{si}");
    assert!(si.contains("soc nrf52840 {"), "soc name from compatible:\n{si}");
    assert!(si.contains("flash : region at 0x00000000 size 1M"), "flash region:\n{si}");
    assert!(si.contains("ram   : region at 0x20000000 size 256K"), "ram region:\n{si}");
    // clock imported from `clock-frequency = <64000000>`, not the default TODO.
    assert!(si.contains("sysclk : clock_source = 64MHz"), "clock from clock-frequency:\n{si}");
}

#[test]
fn imports_gpio_pin_bindings_with_direction_and_pull() {
    // P7-8b: `leds`/`buttons` groups → typed pin bindings on the mapped gpio
    // controller, with direction (led→output, button→input) and pull from flags.
    let si = imported();
    assert!(si.contains("gpio0 : nrf_gpio at 0x50000000"), "gpio controller:\n{si}");
    assert!(
        si.contains("led_user : nrf_gpio.pin = gpio0.pin(13) as output"),
        "led output pin binding:\n{si}"
    );
    assert!(
        si.contains("btn_user : nrf_gpio.pin = gpio0.pin(11) as input pulling up"),
        "button input+pull pin binding:\n{si}"
    );
}

#[test]
fn unmapped_device_is_a_diagnosed_stub_never_a_silent_drop() {
    // §8/D10: a compatible with no Silica device type becomes a commented stub
    // AND a diagnostic — the fact is never dropped without a trace.
    let root = dts::parse(&example_dts()).expect("parse");
    let import = dts::to_silica(&root, &dts::known_device_types());
    assert!(
        import.board_si.contains("// TODO(raw stub): uart0 at 0x40002000"),
        "unmapped device must be a commented stub:\n{}",
        import.board_si
    );
    assert!(
        import.diagnostics.iter().any(|d| d.contains("uart") && d.contains("no Silica device type")),
        "unmapped device must emit a diagnostic: {:?}",
        import.diagnostics
    );
}

#[test]
fn round_trips_the_nrf52840dk_board_facts() {
    // The importer reconstructs every fact of the real `nrf52840_dk` board
    // (examples/blink_button_nrf52840.si) from its DTS: soc + memory + clock +
    // gpio instance + both pin bindings.  A semantic round-trip (1M == 1024K).
    let si = imported();
    for fact in [
        "board nrf52840_dk {",
        "soc nrf52840 {",
        "flash : region at 0x00000000 size 1M",   // 1M == the board's 1024K
        "ram   : region at 0x20000000 size 256K",
        "sysclk : clock_source = 64MHz",
        "gpio0 : nrf_gpio at 0x50000000",
        "led_user : nrf_gpio.pin = gpio0.pin(13) as output",
        "btn_user : nrf_gpio.pin = gpio0.pin(11) as input pulling up",
    ] {
        assert!(si.contains(fact), "round-trip missing board fact `{fact}`:\n{si}");
    }
}

#[test]
fn the_committed_worked_example_matches_the_importer_output() {
    // The checked-in `nrf52840dk.imported.si` must stay in sync with the importer.
    let expected = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/dts_examples/nrf52840dk.imported.si"),
    )
    .expect("read imported si");
    assert_eq!(imported(), expected, "regenerate with: cargo run --bin dts_import -- dts_examples/nrf52840dk.dts");
}
