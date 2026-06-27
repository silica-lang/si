//! `silicac` — the Silica language compiler, Phase 0.
//!
//! Usage:
//!   silicac <input.si> [-o <output>] [--emit-c] [--cc <compiler>]
//!
//! Pipeline:
//!   source → lex → parse → resolve → SIR → C backend → cc → binary

use silicac::backend::{self, Target};
use silicac::{lexer, parser, resolver, sim};

use std::path::{Path, PathBuf};
use std::process;

const USAGE: &str =
    "usage: silicac <input.si> [-o <output>] [--emit-c] [--emit-llvm] [--sim] [--target host|metal-nrf52840] [--cc <compiler>] [--opt <level>] [--std <dir>]";

/// `-dumpbase` for the metal stack-accounting dumps (`silica_stack.{su,ci}`).
const STACK_DUMP_BASE: &str = "silica_stack";

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
    /// `--emit-llvm` — emit textual LLVM IR via the canary backend (§6.3/§12).
    emit_llvm: bool,
    sim: bool,
    target: Target,
    cc: Option<String>,
    std_dir: PathBuf,
    /// `--opt <level>` — override the target's default optimisation level.
    opt: Option<String>,
}

impl Config {
    fn parse(args: &[String]) -> Result<Self, String> {
        let mut input: Option<PathBuf> = None;
        let mut output: Option<PathBuf> = None;
        let mut emit_c = false;
        let mut emit_llvm = false;
        let mut sim = false;
        let mut target = Target::Host;
        let mut cc: Option<String> = None;
        let mut std_dir: Option<PathBuf> = None;
        let mut opt: Option<String> = None;

        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "-o" => {
                    i += 1;
                    output = Some(PathBuf::from(args.get(i).ok_or("-o requires a value")?));
                }
                "--emit-c" => emit_c = true,
                "--emit-llvm" => emit_llvm = true,
                "--sim" => sim = true,
                "--target" => {
                    i += 1;
                    target = match args.get(i).map(|s| s.as_str()) {
                        Some("host") => Target::Host,
                        Some("metal-nrf52840") => Target::MetalNrf52840,
                        other => return Err(format!("unknown --target {:?} (host|metal-nrf52840)", other)),
                    };
                }
                "--cc" => {
                    i += 1;
                    cc = Some(args.get(i).ok_or("--cc requires a value")?.clone());
                }
                "--std" => {
                    i += 1;
                    std_dir = Some(PathBuf::from(args.get(i).ok_or("--std requires a value")?));
                }
                "--opt" => {
                    i += 1;
                    opt = Some(args.get(i).ok_or("--opt requires a value (e.g. s, 2, z)")?.clone());
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

        Ok(Config { input, output, emit_c, emit_llvm, sim, target, cc, std_dir, opt })
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

    // ── 5a′. LLVM-IR canary path (§6.3/§12) — a second SIR consumer ────────────
    // Writes textual LLVM IR.  Orthogonal to `--target`: it lowers the SIR
    // subset to `.ll`, proving SIR is target-neutral and the overflow trap is a
    // first-class `llvm.*` intrinsic (not a C `__builtin`).
    if cfg.emit_llvm {
        let ll = backend::llvm::LlvmBackend::new().emit(&sir);
        let ll_path = cfg.output.with_extension("ll");
        std::fs::write(&ll_path, &ll)
            .map_err(|e| format!("cannot write '{}': {}", ll_path.display(), e))?;
        eprintln!("silicac: wrote LLVM IR to '{}'", ll_path.display());
        return Ok(());
    }

    // ── 5b. RAM-budget gate (metal only, validation gate #1, §5.3) ─────────────
    // The SIR estimate is a fast pre-compile fail; the *measured* budget after
    // compile (P0-1b, audit #35) is the authoritative bound — kept here so its
    // `statics`/`ram_size` are in scope for that check.
    let metal_budget = if cfg.target == Target::MetalNrf52840 {
        let budget = backend::c::ram_budget(&sir)?;
        eprintln!(
            "silicac: RAM budget (estimate) {} B used ({} statics + {} stack) of {} B",
            budget.used(), budget.statics, budget.stack_reserve, budget.ram_size
        );
        Some(budget)
    } else {
        None
    };

    // ── 5c. C backend (host or metal — both consume the same SIR, §6.1) ────────
    let c_src = backend::c::CBackend::with_target(cfg.target).emit(&sir);
    let cc = cfg.cc.clone().unwrap_or_else(|| cfg.target.default_cc().to_string());

    // ── 6. Emit sources or compile ─────────────────────────────────────────────
    if cfg.emit_c {
        let c_path = cfg.output.with_extension("c");
        std::fs::write(&c_path, &c_src)
            .map_err(|e| format!("cannot write '{}': {}", c_path.display(), e))?;
        eprintln!("silicac: wrote C source to '{}'", c_path.display());
        if cfg.target == Target::MetalNrf52840 {
            let ld = backend::c::emit_linker_script(&sir)?;
            let ld_path = cfg.output.with_extension("ld");
            std::fs::write(&ld_path, ld)
                .map_err(|e| format!("cannot write '{}': {}", ld_path.display(), e))?;
            eprintln!("silicac: wrote linker script to '{}'", ld_path.display());
        }
    } else {
        let c_path = tmp_c_path(&cfg.input);
        std::fs::write(&c_path, &c_src)
            .map_err(|e| format!("cannot write temp file '{}': {}", c_path.display(), e))?;

        let mut cmd = process::Command::new(&cc);
        // `--opt <level>` replaces the target's default `-O…` flag (P1-2).
        let opt_override = backend::opt_override_flag(cfg.opt.as_deref());
        for flag in cfg.target.cc_flags() {
            if opt_override.is_some() && flag.starts_with("-O") {
                continue; // dropped in favour of the override appended below
            }
            cmd.arg(flag);
        }
        if let Some(o) = &opt_override {
            cmd.arg(o);
        }
        // Metal needs the generated linker script (§6.4); it must persist for
        // the link, so write it next to the output.
        let ld_path = cfg.output.with_extension("ld");
        let dump_dir = std::env::temp_dir();
        if cfg.target == Target::MetalNrf52840 {
            let ld = backend::c::emit_linker_script(&sir)?;
            std::fs::write(&ld_path, ld)
                .map_err(|e| format!("cannot write '{}': {}", ld_path.display(), e))?;
            cmd.arg("-T").arg(&ld_path);
            // Pin where GCC writes the `-fstack-usage`/`-fcallgraph-info` dumps
            // (audit #35, §5.3) so we can read them deterministically below.
            cmd.arg("-dumpdir")
                .arg(format!("{}{}", dump_dir.display(), std::path::MAIN_SEPARATOR))
                .arg("-dumpbase")
                .arg(STACK_DUMP_BASE);
        }
        let status = cmd
            .arg("-o")
            .arg(&cfg.output)
            .arg(&c_path)
            .status()
            .map_err(|e| format!("cannot run '{}': {}", cc, e))?;

        let _ = std::fs::remove_file(&c_path);

        if !status.success() {
            return Err(format!("C compiler '{}' exited with status {}", cc, status.code().unwrap_or(-1)));
        }
        eprintln!("silicac: compiled '{}' → '{}'", cfg.input.display(), cfg.output.display());

        // Measured RAM-budget gate (§5.3, audit #35, P0-1b): fold the toolchain's
        // own frame accounting over the (recursion-banned, acyclic) call graph
        // into the *authoritative* budget — hard-error on over-RAM or any
        // non-static (alloca/VLA) frame.  The SIR estimate above is only a
        // fast pre-compile fail / host fallback.
        if cfg.target == Target::MetalNrf52840 {
            match backend::stackinfo::from_dump_dir(&dump_dir, STACK_DUMP_BASE) {
                Some(m) => {
                    let budget = metal_budget.expect("metal budget computed before compile");
                    let verdict = backend::stackinfo::enforce(&m, budget.statics, budget.ram_size);
                    backend::stackinfo::cleanup_dump_dir(&dump_dir, STACK_DUMP_BASE);
                    match verdict {
                        Ok(used) => eprintln!(
                            "silicac: RAM budget (measured) {} B used ({} statics + {} stack, {} source) of {} B",
                            used, budget.statics, m.bytes, m.source, budget.ram_size
                        ),
                        Err(e) => {
                            // The over-budget artifact must not look like a valid build.
                            let _ = std::fs::remove_file(&cfg.output);
                            return Err(e);
                        }
                    }
                }
                None => {
                    backend::stackinfo::cleanup_dump_dir(&dump_dir, STACK_DUMP_BASE);
                    eprintln!(
                        "silicac: note: no measured stack dump (non-GCC --cc?); RAM budget is the SIR estimate only"
                    );
                }
            }

            // Flash / code-size budget gate (§5.3, audit #35, P1-3): size the
            // linked ELF and hard-error if .text+.rodata+.data exceeds the flash
            // region — symmetric to the RAM gate, same delete-the-ELF contract.
            if let Some(flash_size) = backend::c::flash_region_size(&sir) {
                let size_tool = size_tool_for(&cc);
                match process::Command::new(&size_tool).arg(&cfg.output).output() {
                    Ok(out) if out.status.success() => {
                        let text = String::from_utf8_lossy(&out.stdout);
                        match backend::c::parse_size(&text) {
                            Some((t, d, _bss)) => match backend::c::enforce_flash(t, d, flash_size) {
                                Ok(used) => eprintln!(
                                    "silicac: flash budget {} B used (.text+.rodata {} + .data {}) of {} B",
                                    used, t, d, flash_size
                                ),
                                Err(e) => {
                                    let _ = std::fs::remove_file(&cfg.output);
                                    return Err(e);
                                }
                            },
                            None => eprintln!(
                                "silicac: note: could not parse '{}' output; flash budget not enforced",
                                size_tool
                            ),
                        }
                    }
                    _ => eprintln!(
                        "silicac: note: '{}' unavailable; flash budget not enforced",
                        size_tool
                    ),
                }
            }
        }
    }

    Ok(())
}

/// Derive the `size` tool path from the C compiler (`arm-none-eabi-gcc` →
/// `arm-none-eabi-size`); fall back to the ARM `size` (audit #35 P1-3).
fn size_tool_for(cc: &str) -> String {
    match cc.strip_suffix("gcc") {
        Some(prefix) => format!("{prefix}size"),
        None => "arm-none-eabi-size".to_string(),
    }
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
