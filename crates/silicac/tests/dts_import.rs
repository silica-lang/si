//! §8 / audit #35 P7-8a — DTS→Silica importer MVP.  Parses a flat `.dts` subset
//! and emits a `board`/`soc` skeleton: memory regions from `reg`, device
//! instances from `compatible` (mapped where a Silica device type exists, else a
//! **diagnosed** raw stub — never a silent drop, §8/D10).

use silicac::dts::{self, DtsValue};

const NRF_DTS: &str = r#"
/dts-v1/;
/ {
    model = "Nordic nRF52840 DK";
    compatible = "nordic,nrf52840-dk", "nordic,nrf52840";
    soc {
        #address-cells = <1>;
        #size-cells = <1>;
        flash0: flash@0 {
            device_type = "memory";
            reg = <0x00000000 0x100000>;
        };
        sram0: memory@20000000 {
            device_type = "memory";
            reg = <0x20000000 0x40000>;
        };
        gpio0: gpio@50000000 {
            compatible = "nordic,nrf-gpio";
            reg = <0x50000000 0x1000>;
        };
        uart0: uart@40002000 {
            compatible = "nordic,nrf-uarte";
            reg = <0x40002000 0x1000>;
        };
    };
};
"#;

#[test]
fn parses_the_node_tree_with_labels_addrs_and_props() {
    let root = dts::parse(NRF_DTS).expect("parse");
    assert_eq!(root.name, "/");
    let soc = root.children.iter().find(|c| c.name == "soc").expect("soc node");
    // Four device/memory children under soc.
    assert_eq!(soc.children.len(), 4);
    let gpio = soc.children.iter().find(|c| c.label.as_deref() == Some("gpio0")).expect("gpio0");
    assert_eq!(gpio.name, "gpio");
    assert_eq!(gpio.unit_addr, Some(0x5000_0000));
    assert_eq!(gpio.props.iter().find(|(n, _)| n == "reg").map(|(_, v)| v), Some(&DtsValue::Cells(vec![0x5000_0000, 0x1000])));
}

#[test]
fn emits_a_board_skeleton_with_memory_regions() {
    let import = dts::to_silica(&dts::parse(NRF_DTS).expect("parse"), &dts::known_device_types());
    let si = &import.board_si;
    assert!(si.contains("board nordic_nrf52840_dk {"), "board name from model:\n{si}");
    assert!(si.contains("flash : region at 0x00000000 size 1M"), "flash region:\n{si}");
    assert!(si.contains("ram   : region at 0x20000000 size 256K"), "ram region:\n{si}");
    assert!(si.contains("clocks {"), "a clocks block (default) is emitted:\n{si}");
}

#[test]
fn maps_a_known_compatible_to_a_silica_device_type() {
    let import = dts::to_silica(&dts::parse(NRF_DTS).expect("parse"), &dts::known_device_types());
    // gpio → nrf_gpio (a real std device type).
    assert!(import.board_si.contains("gpio0 : nrf_gpio at 0x50000000"), "gpio instance:\n{}", import.board_si);
}

#[test]
fn unmapped_device_is_a_diagnosed_stub_never_a_silent_drop() {
    // §8/D10: a compatible with no Silica device type becomes a commented stub
    // AND a diagnostic — the fact is never dropped without a trace.
    let import = dts::to_silica(&dts::parse(NRF_DTS).expect("parse"), &dts::known_device_types());
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
fn the_committed_worked_example_matches_the_importer_output() {
    // The checked-in `nrf52840dk.imported.si` must stay in sync with the importer.
    let dts_src = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/dts_examples/nrf52840dk.dts"),
    )
    .expect("read example dts");
    let expected = std::fs::read_to_string(
        concat!(env!("CARGO_MANIFEST_DIR"), "/dts_examples/nrf52840dk.imported.si"),
    )
    .expect("read imported si");
    let got = dts::to_silica(&dts::parse(&dts_src).expect("parse"), &dts::known_device_types()).board_si;
    assert_eq!(got, expected, "regenerate with: cargo run --bin dts_import -- dts_examples/nrf52840dk.dts");
}
