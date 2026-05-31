//! §2 guard — "no privileged built-ins".
//!
//! The compiler *core* (everything under `src/`, excluding the std-lib `.si`
//! files) must not name any concrete peripheral type.  `gpio`/`timer` are
//! ordinary std-lib devices resolved through the normal path; if a device-type
//! name leaks into the core as a string literal, the design has grown a
//! two-tier system.  This test makes that mechanically enforceable.

use std::path::Path;

fn rs_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rs_files(&path, out);
        } else if path.extension().map(|e| e == "rs").unwrap_or(false) {
            out.push(path);
        }
    }
}

#[test]
fn compiler_core_does_not_name_peripheral_types() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    rs_files(&src, &mut files);

    // Device-type names that must only ever appear in std-lib `.si` files or in
    // user source — never as a literal in the compiler core.
    let forbidden = ["\"gpio\"", "\"timer\"", "\"uart\"", "\"i2c\"", "\"spi\""];

    for file in &files {
        // The test files themselves legitimately mention these names.
        if file.components().any(|c| c.as_os_str() == "tests") {
            continue;
        }
        let text = std::fs::read_to_string(file).unwrap();
        for needle in &forbidden {
            assert!(
                !text.contains(needle),
                "compiler core file {} contains forbidden device-type literal {} (§2)",
                file.display(),
                needle
            );
        }
    }
}
