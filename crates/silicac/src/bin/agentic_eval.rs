//! `agentic_eval` — run the risk-#5 agentic-eval harness (audit #35 P7-7a).
//!
//! Loads the author/edit/debug task set from `crates/silicac/evals/` (or a
//! directory given as the first argument) and scores each task's **reference**
//! solution: does it compile, how many escape hatches does it use, and — for a
//! debug task — was the `before` program genuinely broken?  Reporting only
//! (exit 0 unless a reference solution fails to compile, which is a harness bug).
//!
//! P7-7b feeds real agent output through the same [`silicac::eval::evaluate`] and
//! reports the `.raw`/escape-hatch frequency of that output vs. this baseline.

use std::path::PathBuf;

use silicac::eval::{load_tasks, run_reference, TaskOutcome};
use silicac::metrics::EscapeHatches;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let root = args
        .first()
        .map(PathBuf::from)
        .unwrap_or_else(silicac::eval::default_evals_dir);

    let tasks = load_tasks(&root);
    if tasks.is_empty() {
        eprintln!("no eval tasks found under {}", root.display());
        std::process::exit(1);
    }

    println!("== agentic-eval: {} reference solutions ({}) ==", tasks.len(), root.display());
    let outcomes = run_reference(&tasks);

    let mut total = EscapeHatches::default();
    let mut passed = 0usize;
    for o in &outcomes {
        report(o);
        total.add(o.hatches);
        if o.passed() {
            passed += 1;
        }
    }

    println!(
        "-- reference summary: {}/{} passed · escape hatches: casts={} wrap/sat={} raw={} endian={} (total {})",
        passed, outcomes.len(), total.casts, total.wrap_sat, total.raw, total.endian, total.total()
    );

    // A reference solution that does not compile (or a debug task whose `before`
    // was not actually broken) is a harness bug, not an agent result.
    let broken: Vec<&TaskOutcome> = outcomes.iter().filter(|o| !o.passed()).collect();
    if !broken.is_empty() {
        eprintln!("FAIL: {} reference solution(s) did not pass the harness", broken.len());
        std::process::exit(1);
    }
}

fn report(o: &TaskOutcome) {
    let status = if o.compiles { "ok " } else { "ERR" };
    print!(
        "  [{}] {:<24} {:<6} hatches={}",
        status, o.id, o.kind.as_str(), o.hatches.total()
    );
    if let Some(false) = o.before_failed {
        print!("  (WARN: debug `before` compiled — no real bug)");
    }
    if let Some(e) = &o.error {
        print!("  error: {e}");
    }
    println!();
}
