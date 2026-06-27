//! `escape_audit` — print the escape-hatch / idiom-corpus report (audit #35 P2-2).
//!
//! Walks one or more directories of `.si` files and reports, per file and in
//! total, how often the language's strictness escape hatches are used (casts,
//! wrapping/saturating ops, `.raw`, `.le`/`.be`).  With no args it audits the
//! std lib + the repo's `examples/`.  Reporting only (exit 0); the CI gate on
//! std-lib density lives in `tests/escape_hatch.rs`.

use std::path::{Path, PathBuf};

use silicac::metrics::{count_escape_hatches, EscapeHatches};

fn si_files(dir: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|e| e == "si").unwrap_or(false))
            .collect(),
        Err(_) => Vec::new(),
    };
    v.sort();
    v
}

fn audit_dir(label: &str, dir: &Path) -> EscapeHatches {
    println!("== {} ({}) ==", label, dir.display());
    let mut total = EscapeHatches::default();
    for path in si_files(dir) {
        let src = std::fs::read_to_string(&path).unwrap_or_default();
        let h = count_escape_hatches(&src).unwrap_or_default();
        total.add(h);
        if h.total() > 0 {
            println!(
                "  {:<36} casts={} wrap/sat={} raw={} endian={}  (total {})",
                path.file_name().unwrap().to_string_lossy(),
                h.casts, h.wrap_sat, h.raw, h.endian, h.total()
            );
        }
    }
    println!(
        "  -- {} total: casts={} wrap/sat={} raw={} endian={}  (total {})",
        label, total.casts, total.wrap_sat, total.raw, total.endian, total.total()
    );
    total
}

fn main() {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let args: Vec<String> = std::env::args().skip(1).collect();

    let dirs: Vec<(String, PathBuf)> = if args.is_empty() {
        vec![
            ("std".into(), silicac::default_std_dir()),
            ("examples".into(), manifest.join("../../examples")),
        ]
    } else {
        args.iter().map(|a| (a.clone(), PathBuf::from(a))).collect()
    };

    let mut grand = EscapeHatches::default();
    for (label, dir) in &dirs {
        grand.add(audit_dir(label, dir));
        println!();
    }
    println!(
        "escape-hatch audit: corpus total = {} (casts {}, wrap/sat {}, raw {}, endian {})",
        grand.total(), grand.casts, grand.wrap_sat, grand.raw, grand.endian
    );
}
