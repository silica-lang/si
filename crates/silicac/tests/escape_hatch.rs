//! §7.4 / audit #35 P2-2 — escape-hatch / idiom-corpus metric.  The std lib is
//! the agent's idiom corpus; if it leans on escape hatches the defaults are
//! wrong (risk #4).  This gates the std lib's escape-hatch count and locks the
//! known baseline so a regression toward `.raw`/cast-everywhere fails CI.

use std::path::{Path, PathBuf};

use silicac::metrics::{count_escape_hatches, EscapeHatches};

fn si_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()))
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().map(|e| e == "si").unwrap_or(false))
        .collect();
    v.sort();
    v
}

fn count_dir(dir: &Path) -> EscapeHatches {
    let mut total = EscapeHatches::default();
    for p in si_files(dir) {
        let src = std::fs::read_to_string(&p).unwrap();
        total.add(count_escape_hatches(&src).expect("lex"));
    }
    total
}

fn count_file(path: &Path) -> EscapeHatches {
    count_escape_hatches(&std::fs::read_to_string(path).unwrap()).expect("lex")
}

fn examples_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../examples")
}

/// The gate: the std lib (the agent's idiom corpus) must stay nearly free of
/// escape hatches.  Baseline today is 1 (a single cast in bme280.si); a small
/// ceiling catches a regression toward escape-hatch-everywhere.
#[test]
fn std_lib_escape_hatches_stay_low() {
    let std = count_dir(&silicac::default_std_dir());
    assert!(
        std.total() <= 3,
        "std-lib escape-hatch count {} exceeds the ceiling (3) — the defaults may be wrong (risk #4): {std:?}",
        std.total()
    );
}

/// Lock the std-lib baseline exactly (std is small and stable): one cast, no
/// wrap/sat, no raw/endian.
#[test]
fn std_lib_baseline_is_one_cast() {
    let std = count_dir(&silicac::default_std_dir());
    assert_eq!(std.casts, 1, "bme280.si's compensation cast");
    assert_eq!(std.wrap_sat, 0);
    assert_eq!(std.raw, 0);
    assert_eq!(std.endian, 0);
}

/// The counter actually detects the corpus's known escape hatches (anchors that
/// don't break as the example set grows): overflow.si uses the wrap+sat ops, and
/// fixed.si uses several casts.
#[test]
fn examples_anchors_are_detected() {
    let overflow = count_file(&examples_dir().join("overflow.si"));
    assert_eq!(overflow.wrap_sat, 2, "overflow.si demonstrates `+%` and `+|`");

    let fixed = count_file(&examples_dir().join("fixed.si"));
    assert!(fixed.casts >= 6, "fixed.si casts ({}) — int↔fixed conversions", fixed.casts);

    // Corpus-wide lower bound (robust to new examples being added). Examples
    // hold 8 casts today (fixed.si 6, casts.si 1, sensor_temp_c.si 1).
    let ex = count_dir(&examples_dir());
    assert!(ex.casts >= 8, "examples cast total {} below the known floor", ex.casts);
    assert!(ex.wrap_sat >= 2, "examples wrap/sat total {} below the known floor", ex.wrap_sat);
}
