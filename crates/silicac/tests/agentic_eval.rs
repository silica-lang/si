//! §12 / audit #35 P7-7a (risk #5) — the agentic-eval harness.  The runner loads
//! the author/edit/debug task set and scores a candidate solution on compilation
//! + the escape-hatch metric.  These tests exercise the harness on the committed
//! **reference** solutions, establishing the baseline P7-7b compares agent output
//! against, and lock the task-set invariants.

use silicac::eval::{self, TaskKind};

fn tasks() -> Vec<eval::EvalTask> {
    let t = eval::load_tasks(&eval::default_evals_dir());
    assert!(!t.is_empty(), "no eval tasks found — the evals/ fixtures must be committed");
    t
}

#[test]
fn every_reference_solution_passes_the_harness() {
    for o in eval::run_reference(&tasks()) {
        assert!(o.compiles, "reference solution for '{}' must compile: {:?}", o.id, o.error);
        assert!(o.passed(), "reference solution for '{}' must pass the harness", o.id);
    }
}

#[test]
fn debug_tasks_start_from_a_genuinely_broken_program() {
    // A debug task is only meaningful if its `before` program really fails to
    // compile — otherwise there is no bug to fix.
    let tasks = tasks();
    let debug: Vec<_> = tasks.iter().filter(|t| t.kind == TaskKind::Debug).collect();
    assert!(!debug.is_empty(), "the task set must include at least one debug task");
    for t in debug {
        assert!(t.before.is_some(), "debug task '{}' needs a before.si", t.id);
        let o = eval::evaluate(t, &t.solution);
        assert_eq!(o.before_failed, Some(true), "debug task '{}' before.si must NOT compile", t.id);
    }
}

#[test]
fn the_task_set_covers_author_edit_and_debug() {
    let tasks = tasks();
    for kind in [TaskKind::Author, TaskKind::Edit, TaskKind::Debug] {
        assert!(
            tasks.iter().any(|t| t.kind == kind),
            "the task set must include an `{}` task (risk #5)",
            kind.as_str()
        );
    }
}

#[test]
fn reference_solutions_are_nearly_escape_hatch_free() {
    // The reference answers are the idiom baseline: only the debug-narrowing task
    // legitimately needs the single visible escape hatch (an `as u8` cast).  A
    // regression toward escape-hatch-everywhere in the fixtures fails here.
    let total: u32 = eval::run_reference(&tasks()).iter().map(|o| o.hatches.total()).sum();
    assert_eq!(total, 1, "reference solutions should use exactly one escape hatch (the debug cast), got {total}");
}

// ─── P7-7b: run on real agent output + report ─────────────────────────────────

#[test]
fn every_task_has_a_committed_agent_candidate() {
    for t in tasks() {
        assert!(t.candidate.is_some(), "task '{}' is missing a candidate.si (real agent output)", t.id);
    }
}

#[test]
fn every_agent_submission_compiles() {
    // The core P7-7b result: real agent output on the task set compiles.
    for o in eval::run_candidates(&tasks()) {
        assert!(o.compiles, "agent submission for '{}' must compile: {:?}", o.id, o.error);
        assert!(o.passed(), "agent submission for '{}' must pass the harness", o.id);
    }
}

#[test]
fn agent_output_does_not_reach_for_raw() {
    // Risk #5's sharpest signal: the agent stays at the language's abstraction
    // level — it never drops to `.raw`, and its total escape-hatch use matches
    // the idiom baseline (the one legitimate truncating cast).
    let cand = eval::run_candidates(&tasks());
    let raw: u32 = cand.iter().map(|o| o.hatches.raw).sum();
    let total: u32 = cand.iter().map(|o| o.hatches.total()).sum();
    assert_eq!(raw, 0, "agent output must not use `.raw`");
    assert_eq!(total, 1, "agent output should use only the one idiomatic escape hatch, got {total}");
}

#[test]
fn the_report_artifact_renders_the_key_metrics() {
    let t = tasks();
    let report = eval::format_report(&t, &eval::run_reference(&t), &eval::run_candidates(&t));
    assert!(report.contains("Agentic-eval report"), "report title:\n{report}");
    assert!(report.contains("`.raw` 0"), "report must surface the .raw frequency:\n{report}");
    assert!(report.contains("Risk #5 read:"), "report must include the risk-#5 read:\n{report}");
}
