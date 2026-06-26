//! §5.3/SIL-005 — measured worst-case stack (audit #35, P0-1a).
//!
//! Hermetic tests (no arm-gcc/Renode): exercise the `.ci`/`.su` parsing and the
//! worst-case walk over fixture strings shaped like real arm-none-eabi-gcc 15.2
//! output, and confirm the metal target actually requests the dumps.

use silicac::backend::stackinfo::{self, FuncFrame};
use silicac::backend::Target;

// arm-gcc 15.2 -Os inlines single-use statics, so a leaf/__react_*_run folds
// into its caller's frame (here, __reaction_0).
const CI: &str = r#"graph: { title: "/tmp/x.c"
node: { title: "__reaction_0" label: "__reaction_0\n/tmp/x.c:5:6\n56 bytes (static)\n0 dynamic objects" }
node: { title: "SysTick_Handler" label: "SysTick_Handler\n/tmp/x.c:6:6\n8 bytes (static)\n0 dynamic objects" }
edge: { sourcename: "SysTick_Handler" targetname: "__reaction_0" label: "/tmp/x.c:6:30" }
node: { title: "Reset_Handler" label: "Reset_Handler\n/tmp/x.c:7:6\n72 bytes (static)\n0 dynamic objects" }
edge: { sourcename: "Reset_Handler" targetname: "__reaction_0" label: "/tmp/x.c:6:30" }
}"#;

const SU: &str = "/tmp/x.c:5:6:__reaction_0\t56\tstatic\n\
                  /tmp/x.c:6:6:SysTick_Handler\t8\tstatic\n\
                  /tmp/x.c:7:6:Reset_Handler\t72\tstatic\n";

#[test]
fn callgraph_measure_sums_chains_per_priority() {
    let g = stackinfo::parse_ci(CI);
    assert_eq!(g.nodes["__reaction_0"].bytes, 56);
    assert_eq!(g.edges["SysTick_Handler"], vec!["__reaction_0".to_string()]);

    // base = max(Reset 72 + reaction 56, headroom 512) = 512;
    // + SysTick chain (8 + 56) + EXC_FRAME 64 = 128  ⇒  640
    let m = stackinfo::measure(&g).expect("measure");
    assert_eq!(m.bytes, 640);
    assert_eq!(m.source, "callgraph");
    assert!(!m.any_dynamic);
}

#[test]
fn su_fallback_is_a_sound_upper_bound() {
    let frames = stackinfo::parse_su(SU);
    assert_eq!(frames[0], FuncFrame { name: "__reaction_0".into(), bytes: 56, dynamic: false });
    // The .su fallback (no edges) sums all frames + EXC per ISR + headroom, so
    // it must be >= the tight call-graph number for the same program.
    let su = stackinfo::measure_su(&frames);
    let ci = stackinfo::measure(&stackinfo::parse_ci(CI)).unwrap();
    assert!(su.bytes >= ci.bytes, "su {} should bound ci {}", su.bytes, ci.bytes);
    assert_eq!(su.source, "stack-usage");
}

#[test]
fn enforce_is_the_authoritative_measured_budget() {
    let m = stackinfo::measure(&stackinfo::parse_ci(CI)).unwrap();
    // Fits a real 256 KB nRF52840: returns statics + measured stack.
    assert_eq!(stackinfo::enforce(&m, 1, 262_144).unwrap(), 1 + m.bytes);
    // Over a tiny region: hard error.
    assert!(stackinfo::enforce(&m, 0, 256).is_err());
    // A fabricated non-static (alloca/VLA) frame is rejected as unsound even if
    // it would fit — recursion/VLAs are banned, so this should never arise.
    let dynamic = stackinfo::parse_ci(
        r#"node: { title: "Reset_Handler" label: "Reset_Handler\n16 bytes (dynamic)\n1 dynamic objects" }"#,
    );
    let dm = stackinfo::measure(&dynamic).unwrap();
    assert!(stackinfo::enforce(&dm, 0, 262_144).is_err());
}

#[test]
fn metal_target_requests_stack_dumps() {
    let flags = Target::MetalNrf52840.cc_flags();
    assert!(flags.contains(&"-fcallgraph-info=su,da"), "flags: {flags:?}");
    assert!(flags.contains(&"-fstack-usage"), "flags: {flags:?}");
    // Host builds must stay clean of toolchain-specific dump flags.
    assert!(Target::Host.cc_flags().is_empty());
}
