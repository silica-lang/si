//! Agentic-eval harness (audit #35 P7-7a, risk #5).
//!
//! Risk #5 asks whether an agent can *author*, *edit*, and *debug* non-trivial
//! Silica programs without leaning on the strictness escape hatches (casts,
//! wrap/sat ops, `.raw`, endian) — if it must, the language surface is wrong.
//!
//! This module is the runner + task model.  A **task** is a self-contained
//! directory under an evals root: a `prompt.md` (the instruction), a
//! `solution.si` (the committed reference answer), and — for `edit`/`debug`
//! tasks — a `before.si` (the program to change / the buggy program).  The
//! runner compiles a *candidate* solution (lex → parse → resolve, std lib
//! spliced in) and scores it on the [`crate::metrics`] escape-hatch metric.  The
//! reference solutions are the baseline; P7-7b feeds real agent output through
//! the same [`evaluate`].

use std::path::{Path, PathBuf};

use crate::metrics::{count_escape_hatches, EscapeHatches};

/// The three agentic tasks (risk #5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskKind {
    /// Write a program from a spec.
    Author,
    /// Change an existing, working program.
    Edit,
    /// Fix a program that does not compile / behaves wrong.
    Debug,
}

impl TaskKind {
    fn from_prefix(name: &str) -> Option<TaskKind> {
        match name.split('_').next() {
            Some("author") => Some(TaskKind::Author),
            Some("edit") => Some(TaskKind::Edit),
            Some("debug") => Some(TaskKind::Debug),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            TaskKind::Author => "author",
            TaskKind::Edit => "edit",
            TaskKind::Debug => "debug",
        }
    }
}

/// One eval task loaded from disk.
#[derive(Debug, Clone)]
pub struct EvalTask {
    pub id: String,
    pub kind: TaskKind,
    pub prompt: String,
    /// The starting program for an `edit`/`debug` task, if present.
    pub before: Option<String>,
    /// The committed reference ("gold") solution.
    pub solution: String,
}

/// The result of scoring a candidate solution for one task.
#[derive(Debug, Clone)]
pub struct TaskOutcome {
    pub id: String,
    pub kind: TaskKind,
    /// Did the candidate compile (lex + parse + resolve)?
    pub compiles: bool,
    /// The first compile error, if the candidate did not compile.
    pub error: Option<String>,
    /// Escape-hatch counts of the candidate program.
    pub hatches: EscapeHatches,
    /// For a `debug` task: did the `before` program genuinely fail to compile
    /// (proving there was a real bug to fix)?  `None` for author/edit tasks.
    pub before_failed: Option<bool>,
}

impl TaskOutcome {
    /// A task passes when the candidate compiles and — for a debug task — the
    /// `before` program really was broken.
    pub fn passed(&self) -> bool {
        self.compiles && self.before_failed != Some(false)
    }
}

/// Compile a Silica source through the front end (lex → parse → resolve) with
/// the std lib spliced in.  `Ok(())` on success, else the first error message.
pub fn compile_check(src: &str) -> Result<(), String> {
    let std_items = crate::load_std_items(&crate::default_std_dir())?;
    let tokens = crate::lexer::lex(src).map_err(|e| format!("lex error: {}", e.msg))?;
    let mut ast = crate::parser::parse(tokens).map_err(|e| format!("parse error: {}", e.msg))?;
    ast.items.splice(0..0, std_items);
    crate::resolver::resolve(&ast)
        .map(|_| ())
        .map_err(|errs| errs.iter().map(|d| d.msg.clone()).collect::<Vec<_>>().join("; "))
}

/// Score a `candidate` solution against `task` (does it compile, how many escape
/// hatches, and — for debug — was the `before` genuinely broken).
pub fn evaluate(task: &EvalTask, candidate: &str) -> TaskOutcome {
    let compile = compile_check(candidate);
    let hatches = count_escape_hatches(candidate).unwrap_or_default();
    let before_failed = match task.kind {
        TaskKind::Debug => task.before.as_deref().map(|b| compile_check(b).is_err()),
        _ => None,
    };
    TaskOutcome {
        id: task.id.clone(),
        kind: task.kind,
        compiles: compile.is_ok(),
        error: compile.err(),
        hatches,
        before_failed,
    }
}

/// Score every task's committed reference solution.
pub fn run_reference(tasks: &[EvalTask]) -> Vec<TaskOutcome> {
    tasks.iter().map(|t| evaluate(t, &t.solution)).collect()
}

/// Load every task under `root` (each task is a subdirectory named
/// `<kind>_<name>` with `prompt.md` + `solution.si` [+ `before.si`]).  Tasks are
/// returned sorted by id for deterministic reporting.
pub fn load_tasks(root: &Path) -> Vec<EvalTask> {
    let mut dirs: Vec<PathBuf> = match std::fs::read_dir(root) {
        Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect(),
        Err(_) => Vec::new(),
    };
    dirs.sort();
    let mut tasks = Vec::new();
    for dir in dirs {
        let id = dir.file_name().unwrap().to_string_lossy().to_string();
        let Some(kind) = TaskKind::from_prefix(&id) else { continue };
        let Ok(solution) = std::fs::read_to_string(dir.join("solution.si")) else { continue };
        let prompt = std::fs::read_to_string(dir.join("prompt.md")).unwrap_or_default();
        let before = std::fs::read_to_string(dir.join("before.si")).ok();
        tasks.push(EvalTask { id, kind, prompt, before, solution });
    }
    tasks
}

/// The default evals root: `crates/silicac/evals` next to the manifest.
pub fn default_evals_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("evals")
}
