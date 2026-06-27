//! `silicac` — the Silica language compiler library.
//!
//! The crate is split into a thin binary (`src/main.rs`, the CLI) and this
//! library, which holds the whole compiler pipeline so it can be driven from
//! integration tests in `tests/` as well as from the CLI:
//!
//!   source → [`lexer`] → [`parser`] → [`resolver`] → [`sir`] → consumer
//!
//! There are two SIR *consumers* (§6.1 — "SIR is the contract, backends are
//! just consumers"):
//!   - [`backend::c`] — emits a freestanding C translation unit (the
//!     metal-direction consumer);
//!   - [`sim`] — a deterministic host interpreter of SIR (the sim-direction
//!     consumer, §7.1).

// Many AST/SIR/resolver fields and variants are forward-looking stubs — parsed
// and stored for later lowering passes but not yet consumed.  Suppress the
// resulting dead_code noise so the output stays clean.
#![allow(dead_code)]

pub mod ast;
pub mod backend;
pub mod diag;
pub mod layer3;
pub mod lexer;
pub mod metrics;
pub mod parser;
pub mod resolver;
pub mod sim;
pub mod sir;

use std::path::{Path, PathBuf};

/// Default standard-library directory (`crates/silicac/std`), resolved at build
/// time.  Overridable on the CLI with `--std <dir>`.
pub fn default_std_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("std")
}

/// Lex and parse every `.si` file in `dir` (sorted for determinism) and return
/// their top-level items.  These are the un-privileged std-lib devices (§2,
/// §7.4): they flow into the module as ordinary `Item::Device` entries.
///
/// A missing directory yields an empty item list (the language core still
/// works for programs that declare their own devices inline).
pub fn load_std_items(dir: &Path) -> Result<Vec<ast::Item>, String> {
    let mut paths: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| p.extension().map(|e| e == "si").unwrap_or(false))
            .collect(),
        Err(_) => return Ok(Vec::new()),
    };
    paths.sort();

    let mut items = Vec::new();
    for path in paths {
        let src = std::fs::read_to_string(&path)
            .map_err(|e| format!("cannot read std file '{}': {}", path.display(), e))?;
        let tokens = lexer::lex(&src).map_err(|e| format!("{}: {}", path.display(), e))?;
        let module = parser::parse(tokens).map_err(|e| format!("{}: {}", path.display(), e))?;
        items.extend(module.items);
    }
    Ok(items)
}
