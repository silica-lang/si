//! `dts_import` — DTS→Silica importer MVP (audit #35 P7-8a, §8).
//!
//! Reads a **flat** `.dts` file (the `cpp` preprocessing phase of §8's ingestion
//! pipeline is out of scope for this spike) and prints a `board`/`soc` skeleton
//! to stdout.  Per §8/D10 every unmapped device is a diagnosed stub on stderr,
//! never a silent drop.
//!
//!   silicac-dts-import <board.dts>

use std::path::PathBuf;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(path) = args.first().map(PathBuf::from) else {
        eprintln!("usage: dts_import <board.dts>");
        std::process::exit(2);
    };
    let src = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("cannot read {}: {e}", path.display());
            std::process::exit(2);
        }
    };
    let root = match silicac::dts::parse(&src) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("DTS parse error in {}: {e}", path.display());
            std::process::exit(1);
        }
    };
    let import = silicac::dts::to_silica(&root, &silicac::dts::known_device_types());
    print!("{}", import.board_si);
    for d in &import.diagnostics {
        eprintln!("dts_import: note: {d}");
    }
}
