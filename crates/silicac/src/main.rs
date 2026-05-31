//! `silicac` — the Silica language compiler, Phase 0.
//!
//! Usage:
//!   silicac <input.si> [-o <output>] [--emit-c] [--cc <compiler>]
//!
//! Pipeline:
//!   source → lex → parse → resolve → SIR → C backend → cc → binary

use silicac::{backend, lexer, parser, resolver, sim};

use std::path::{Path, PathBuf};
use std::process;

const USAGE: &str =
    "usage: silicac <input.si> [-o <output>] [--emit-c] [--sim] [--cc <compiler>] [--std <dir>]";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cfg = match Config::parse(&args[1..]) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("silicac: {}", e);
            eprintln!("{}", USAGE);
            process::exit(1);
        }
    };

    if let Err(e) = run(&cfg) {
        eprintln!("silicac: {}", e);
        process::exit(1);
    }
}

// ─── Configuration ────────────────────────────────────────────────────────────

struct Config {
    input: PathBuf,
    output: PathBuf,
    emit_c: bool,
    sim: bool,
    cc: String,
    std_dir: PathBuf,
}

impl Config {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut input: Option<PathBuf> = None;
        let mut output: Option<PathBuf> = None;
        let mut emit_c = false;
        let mut sim = false;
        let mut cc = "cc".to_string();
        let mut std_dir: Option<PathBuf> = None;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-o" => {
                    i += 1;
                    output = Some(PathBuf::from(args.get(i).ok_or("-o requires a value")?));
                }
                "--emit-c" => emit_c = true,
                "--sim" => sim = true,
                "--cc" => {
                    i += 1;
                    cc = args.get(i).ok_or("--cc requires a value")?.clone();
                }
                "--std" => {
                    i += 1;
                    std_dir = Some(PathBuf::from(args.get(i).ok_or("--std requires a value")?));
                }
                arg if !arg.starts_with('-') => {
                    if input.is_some() {
                        return Err(format!("unexpected argument: {}", arg));
                    }
                    input = Some(PathBuf::from(arg));
                }
                arg => return Err(format!("unknown flag: {}", arg)),
            }
            i += 1;
        }

        let input = input.ok_or("no input file specified")?;
        let output = output.unwrap_or_else(|| {
            // Default output: strip extension, place in current directory.
            Path::new(input.file_stem().unwrap_or_else(|| std::ffi::OsStr::new("out"))).to_path_buf()
        });
        let std_dir = std_dir.unwrap_or_else(silicac::default_std_dir);

        Ok(Config { input, output, emit_c, sim, cc, std_dir })
    }
}

// ─── Main pipeline ────────────────────────────────────────────────────────────

fn run(cfg: &Config) -> Result<(), String> {
    // ── 1. Read source ────────────────────────────────────────────────────────
    let src = std::fs::read_to_string(&cfg.input)
        .map_err(|e| format!("cannot read '{}': {}", cfg.input.display(), e))?;

    // ── 2. Lex ────────────────────────────────────────────────────────────────
    let tokens = lexer::lex(&src).map_err(|e| {
        format!("{}:{}", cfg.input.display(), e)
    })?;

    // ── 3. Parse ──────────────────────────────────────────────────────────────
    let mut ast = parser::parse(tokens).map_err(|e| {
        let (line, col) = offset_to_line_col(&src, e.span.start);
        format!("{}:{}:{}: {}", cfg.input.display(), line, col, e.msg)
    })?;

    // ── 3b. Prepend std-lib devices (gpio/timer …) as ordinary items (§2) ──────
    let std_items = silicac::load_std_items(&cfg.std_dir)?;
    ast.items.splice(0..0, std_items);

    // ── 4. Resolve → SIR ──────────────────────────────────────────────────────
    let sir = resolver::resolve(&ast).map_err(|errs| {
        let msgs: Vec<String> = errs
            .iter()
            .map(|e| {
                let (line, col) = offset_to_line_col(&src, e.span.start);
                format!("{}:{}:{}: {}", cfg.input.display(), line, col, e.msg)
            })
            .collect();
        msgs.join("\n")
    })?;

    // ── 5a. Host simulator path (§7.1) — a SIR consumer peer to the C backend ──
    if cfg.sim {
        let result = sim::run(&sir);
        print!("{}", result.render(&sir));
        return Ok(());
    }

    // ── 5b. C backend ──────────────────────────────────────────────────────────
    let c_src = backend::c::CBackend::new().emit(&sir);

    // ── 6. Emit C or compile ──────────────────────────────────────────────────
    if cfg.emit_c {
        // Just write the C source to <output>.c
        let c_path = cfg.output.with_extension("c");
        std::fs::write(&c_path, &c_src)
            .map_err(|e| format!("cannot write '{}': {}", c_path.display(), e))?;
        eprintln!("silicac: wrote C source to '{}'", c_path.display());
    } else {
        // Write to a temp file, then invoke cc.
        let c_path = tmp_c_path(&cfg.input);
        std::fs::write(&c_path, &c_src)
            .map_err(|e| format!("cannot write temp file '{}': {}", c_path.display(), e))?;

        let status = process::Command::new(&cfg.cc)
            .arg("-o")
            .arg(&cfg.output)
            .arg(&c_path)
            .status()
            .map_err(|e| format!("cannot run '{}': {}", cfg.cc, e))?;

        // Clean up the temp file regardless of cc outcome.
        let _ = std::fs::remove_file(&c_path);

        if !status.success() {
            return Err(format!(
                "C compiler '{}' exited with status {}",
                cfg.cc,
                status.code().unwrap_or(-1)
            ));
        }
        eprintln!("silicac: compiled '{}' → '{}'", cfg.input.display(), cfg.output.display());
    }

    Ok(())
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Convert a byte offset to (1-indexed line, 1-indexed col).
fn offset_to_line_col(src: &str, offset: usize) -> (usize, usize) {
    let prefix = &src[..offset.min(src.len())];
    let line = prefix.bytes().filter(|&b| b == b'\n').count() + 1;
    let col = prefix.rfind('\n').map_or(offset, |p| offset - p - 1) + 1;
    (line, col)
}

/// A deterministic temp file path derived from the input path.
fn tmp_c_path(input: &Path) -> PathBuf {
    let stem = input.file_stem().unwrap_or_else(|| std::ffi::OsStr::new("silicac_out"));
    let mut p = std::env::temp_dir();
    p.push(format!("silicac_{}.c", stem.to_string_lossy()));
    p
}
