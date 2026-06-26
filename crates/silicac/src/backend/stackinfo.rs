//! §5.3/SIL-005 — *measured* worst-case stack (audit #35, P0-1a).
//!
//! The SIR-only estimate in [`super::c::worst_case_stack`] sums synthetic
//! per-frame constants (`FRAME_OVERHEAD`/`EXC_FRAME`), which is **not** a sound
//! upper bound for the C the backend emits — the C compiler can spill or
//! materialise temporaries beyond any constant.  This module instead parses the
//! toolchain's own stack accounting:
//!
//!   - **`-fcallgraph-info=su,da`** → a `.ci` file (VCG): per-function stack
//!     usage *and* call edges.  The tight, preferred source — we walk the
//!     (recursion-banned ⇒ acyclic) call graph for the true cumulative depth.
//!   - **`-fstack-usage`** → a `.su` file: per-function frames only (no edges).
//!     The conservative fallback when callgraph-info is unavailable.
//!
//! P0-1a prints the measured number beside the estimate; P0-1b folds it into the
//! RAM budget and hard-errors on over-RAM or any non-static (alloca/VLA) frame.

use std::collections::HashMap;
use std::path::Path;

/// Cortex-M hardware exception stack frame pushed per preemption level (basic
/// frame is 32 B; 104 B with FP context — 64 B is the conservative middle, and
/// the same value the SIR estimate uses, `super::c::EXC_FRAME`).
const EXC_FRAME: u64 = 64;
/// Thread-mode headroom (startup / idle context) — a floor on the base context.
const STACK_HEADROOM: u64 = 512;

/// Thread-mode entry (startup + WFI idle loop); does not push an `EXC_FRAME`.
pub const THREAD_ENTRY: &str = "Reset_Handler";
/// Interrupt entry points: each can preempt, so each adds one `EXC_FRAME` on top
/// of the thread context (a sound over-approximation of nesting depth).
pub const ISR_ENTRIES: &[&str] = &[
    "SysTick_Handler",
    "GPIOTE_IRQHandler",
    "__BUS_IRQHandler",
    "HardFault_Handler",
];

/// One function's measured frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FuncFrame {
    pub name: String,
    pub bytes: u64,
    /// `true` if GCC reported a non-`static` (alloca/VLA) frame.  This must
    /// never happen in Silica — recursion and VLAs are banned — so P0-1b
    /// hard-errors on it rather than reporting an unbounded budget.
    pub dynamic: bool,
}

/// Parsed `.ci` call graph: per-function frames + caller→callee edges.
#[derive(Debug, Default, Clone)]
pub struct CallGraph {
    pub nodes: HashMap<String, FuncFrame>,
    pub edges: HashMap<String, Vec<String>>,
}

/// The computed measured worst-case stack.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MeasuredStack {
    pub bytes: u64,
    pub any_dynamic: bool,
    /// `"callgraph"` (`.ci`, tight) or `"stack-usage"` (`.su`, conservative).
    pub source: &'static str,
}

// ─── Parsing ────────────────────────────────────────────────────────────────

/// Parse a `-fstack-usage` `.su` file.  Lines are
/// `file:line:col:funcname\tbytes\tqualifier` (qualifier `static`/`dynamic`).
pub fn parse_su(text: &str) -> Vec<FuncFrame> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let mut cols = line.split('\t');
        let loc = cols.next().unwrap_or("");
        let bytes = cols.next().and_then(|s| s.trim().parse::<u64>().ok());
        let qual = cols.next().unwrap_or("").trim();
        // funcname is the segment after the last ':' (C identifiers have none).
        let name = loc.rsplit(':').next().unwrap_or("");
        if name.is_empty() {
            continue;
        }
        if let Some(bytes) = bytes {
            out.push(FuncFrame { name: name.to_string(), bytes, dynamic: qual != "static" });
        }
    }
    out
}

/// Parse a `-fcallgraph-info=su,da` `.ci` (VCG) file into a [`CallGraph`].
pub fn parse_ci(text: &str) -> CallGraph {
    let mut g = CallGraph::default();
    for line in text.lines() {
        let l = line.trim_start();
        if let Some(rest) = l.strip_prefix("node:") {
            if let Some(title) = field(rest, "title:") {
                let label = field(rest, "label:").unwrap_or_default();
                let (bytes, dynamic) = label_bytes(&label);
                g.nodes.insert(title.clone(), FuncFrame { name: title, bytes, dynamic });
            }
        } else if let Some(rest) = l.strip_prefix("edge:") {
            if let (Some(src), Some(dst)) = (field(rest, "sourcename:"), field(rest, "targetname:")) {
                g.edges.entry(src).or_default().push(dst);
            }
        }
    }
    g
}

/// Extract the first quoted value following `key` in a VCG record line.
fn field(s: &str, key: &str) -> Option<String> {
    let i = s.find(key)? + key.len();
    let rest = &s[i..];
    let start = rest.find('"')? + 1;
    let after = &rest[start..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

/// Pull the frame size out of a `.ci` node label.  Labels embed literal `\n`
/// sequences, e.g. `"fn\n…:5:6\n56 bytes (static)\n0 dynamic objects"`.
fn label_bytes(label: &str) -> (u64, bool) {
    for token in label.split("\\n") {
        let t = token.trim();
        if let Some(pos) = t.find(" bytes") {
            if let Ok(b) = t[..pos].trim().parse::<u64>() {
                // Anything not explicitly "(static)" is treated as dynamic.
                return (b, !t.contains("(static)"));
            }
        }
    }
    (0, false)
}

// ─── Worst-case computation ───────────────────────────────────────────────────

/// Compute the measured worst-case stack from a `.ci` call graph: thread-mode
/// chain + one `EXC_FRAME` per present ISR chain + headroom.  Errors if the
/// graph contains a cycle (recursion is banned, so this signals a bad parse or
/// an unexpected toolchain construct rather than a real program).
pub fn measure(graph: &CallGraph) -> Result<MeasuredStack, String> {
    let any_dynamic = graph.nodes.values().any(|f| f.dynamic);

    let base = chain_stack(graph, THREAD_ENTRY)?;
    let mut total = base.max(STACK_HEADROOM);
    for isr in ISR_ENTRIES {
        if graph.nodes.contains_key(*isr) {
            let cs = chain_stack(graph, isr)?;
            total = total.saturating_add(EXC_FRAME.saturating_add(cs));
        }
    }
    Ok(MeasuredStack { bytes: total, any_dynamic, source: "callgraph" })
}

/// Cumulative stack reachable from `entry`: its own frame plus the deepest
/// callee chain (max-cost path over the acyclic graph).
fn chain_stack(graph: &CallGraph, entry: &str) -> Result<u64, String> {
    fn walk(
        g: &CallGraph,
        n: &str,
        path: &mut Vec<String>,
        memo: &mut HashMap<String, u64>,
    ) -> Result<u64, String> {
        if path.iter().any(|p| p == n) {
            return Err(format!("call-graph cycle through '{}' — recursion is banned (§5.3)", n));
        }
        if let Some(&v) = memo.get(n) {
            return Ok(v);
        }
        let self_bytes = g.nodes.get(n).map(|f| f.bytes).unwrap_or(0);
        let mut best_child = 0u64;
        if let Some(callees) = g.edges.get(n) {
            path.push(n.to_string());
            for c in callees {
                best_child = best_child.max(walk(g, c, path, memo)?);
            }
            path.pop();
        }
        let total = self_bytes.saturating_add(best_child);
        memo.insert(n.to_string(), total);
        Ok(total)
    }
    walk(graph, entry, &mut Vec::new(), &mut HashMap::new())
}

/// Fallback worst-case from `.su` frames alone (no call edges).  Sums *every*
/// function's frame — i.e. assumes the whole program is one call chain — plus an
/// `EXC_FRAME` per ISR present and the thread headroom.  Loose, but a sound
/// upper bound; only used when `.ci` is unavailable.
pub fn measure_su(frames: &[FuncFrame]) -> MeasuredStack {
    let any_dynamic = frames.iter().any(|f| f.dynamic);
    let sum: u64 = frames.iter().map(|f| f.bytes).fold(0, u64::saturating_add);
    let n_isr = frames.iter().filter(|f| ISR_ENTRIES.contains(&f.name.as_str())).count() as u64;
    let bytes = STACK_HEADROOM
        .saturating_add(sum)
        .saturating_add(EXC_FRAME.saturating_mul(n_isr));
    MeasuredStack { bytes, any_dynamic, source: "stack-usage" }
}

/// Fold the measured worst-case stack into the RAM budget and enforce it
/// (§5.3, audit #35, P0-1b).  Returns the total RAM used on success, or a hard
/// error if the bound is unsound (a non-static alloca/VLA frame) or exceeds the
/// chip's RAM region.  This is the authoritative metal budget — the SIR
/// estimate is only a pre-compile fast-fail / host fallback.
pub fn enforce(measured: &MeasuredStack, statics: u64, ram_size: u64) -> Result<u64, String> {
    if measured.any_dynamic {
        return Err(format!(
            "worst-case stack is not bounded: the toolchain reported a non-static \
             (alloca/VLA) frame — recursion and VLAs are banned (§5.3), so this is \
             unexpected; refusing to emit an unsound RAM budget ({} source)",
            measured.source
        ));
    }
    let used = statics.saturating_add(measured.bytes);
    if used > ram_size {
        return Err(format!(
            "RAM budget exceeded (measured): {} B ({} statics + {} worst-case stack, \
             {} source) > {} B RAM region (§5.3)",
            used, statics, measured.bytes, measured.source, ram_size
        ));
    }
    Ok(used)
}

/// Read `<dir>/<base>.ci` (preferred) or `<dir>/<base>.su` (fallback) and
/// compute the measured worst-case stack.  Returns `None` if neither dump is
/// present/usable (e.g. a non-GCC `--cc`), so callers degrade to the estimate.
pub fn from_dump_dir(dir: &Path, base: &str) -> Option<MeasuredStack> {
    let ci = dir.join(format!("{base}.ci"));
    if let Ok(text) = std::fs::read_to_string(&ci) {
        let g = parse_ci(&text);
        if !g.nodes.is_empty() {
            if let Ok(m) = measure(&g) {
                return Some(m);
            }
            // A cycle/parse oddity in .ci — fall through to the .su fallback.
        }
    }
    let su = dir.join(format!("{base}.su"));
    if let Ok(text) = std::fs::read_to_string(&su) {
        let frames = parse_su(&text);
        if !frames.is_empty() {
            return Some(measure_su(&frames));
        }
    }
    None
}

/// Remove the dump files written for a build (best-effort).
pub fn cleanup_dump_dir(dir: &Path, base: &str) {
    let _ = std::fs::remove_file(dir.join(format!("{base}.ci")));
    let _ = std::fs::remove_file(dir.join(format!("{base}.su")));
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verbatim shapes from arm-none-eabi-gcc 15.2 (-O1 inlines single-use
    // statics, so leaf/__react_*_run fold into __reaction_0's frame).
    const SU: &str = "/tmp/x.c:5:6:__reaction_0\t56\tstatic\n\
                      /tmp/x.c:6:6:SysTick_Handler\t8\tstatic\n\
                      /tmp/x.c:7:6:Reset_Handler\t72\tstatic\n";

    const CI: &str = r#"graph: { title: "/tmp/x.c"
node: { title: "__reaction_0" label: "__reaction_0\n/tmp/x.c:5:6\n56 bytes (static)\n0 dynamic objects" }
node: { title: "SysTick_Handler" label: "SysTick_Handler\n/tmp/x.c:6:6\n8 bytes (static)\n0 dynamic objects" }
edge: { sourcename: "SysTick_Handler" targetname: "__reaction_0" label: "/tmp/x.c:6:30" }
node: { title: "Reset_Handler" label: "Reset_Handler\n/tmp/x.c:7:6\n72 bytes (static)\n0 dynamic objects" }
edge: { sourcename: "Reset_Handler" targetname: "__reaction_0" label: "/tmp/x.c:6:30" }
}"#;

    #[test]
    fn su_parses_names_bytes_and_qualifier() {
        let f = parse_su(SU);
        assert_eq!(f.len(), 3);
        assert_eq!(f[0], FuncFrame { name: "__reaction_0".into(), bytes: 56, dynamic: false });
        assert_eq!(f[1].name, "SysTick_Handler");
        assert_eq!(f[2].bytes, 72);
        assert!(!f.iter().any(|x| x.dynamic));
    }

    #[test]
    fn ci_parses_nodes_and_edges() {
        let g = parse_ci(CI);
        assert_eq!(g.nodes["__reaction_0"].bytes, 56);
        assert_eq!(g.nodes["SysTick_Handler"].bytes, 8);
        assert_eq!(g.nodes["Reset_Handler"].bytes, 72);
        assert_eq!(g.edges["SysTick_Handler"], vec!["__reaction_0".to_string()]);
    }

    #[test]
    fn measure_walks_chains_per_priority() {
        // base = max(Reset 72 + reaction 56 = 128, headroom 512) = 512
        // + SysTick chain (8 + 56 = 64) + EXC_FRAME 64 = 128
        // total = 640
        let m = measure(&parse_ci(CI)).expect("measure");
        assert_eq!(m.bytes, 640);
        assert_eq!(m.source, "callgraph");
        assert!(!m.any_dynamic);
    }

    #[test]
    fn dynamic_frame_is_flagged() {
        let ci = r#"node: { title: "Reset_Handler" label: "Reset_Handler\n16 bytes (dynamic)\n1 dynamic objects" }"#;
        let m = measure(&parse_ci(ci)).expect("measure");
        assert!(m.any_dynamic);
    }

    #[test]
    fn cycle_is_an_error() {
        let mut g = CallGraph::default();
        g.nodes.insert("SysTick_Handler".into(), FuncFrame { name: "SysTick_Handler".into(), bytes: 8, dynamic: false });
        g.nodes.insert("a".into(), FuncFrame { name: "a".into(), bytes: 8, dynamic: false });
        g.edges.insert("SysTick_Handler".into(), vec!["a".into()]);
        g.edges.insert("a".into(), vec!["SysTick_Handler".into()]);
        let err = measure(&g).unwrap_err();
        assert!(err.contains("cycle"), "got: {err}");
    }

    #[test]
    fn su_fallback_is_a_sound_sum() {
        // headroom 512 + (56+8+72)=136 + 1 ISR * EXC_FRAME 64 = 712
        let m = measure_su(&parse_su(SU));
        assert_eq!(m.bytes, 712);
        assert_eq!(m.source, "stack-usage");
    }

    fn measured(bytes: u64, any_dynamic: bool) -> MeasuredStack {
        MeasuredStack { bytes, any_dynamic, source: "callgraph" }
    }

    #[test]
    fn enforce_passes_within_budget() {
        assert_eq!(enforce(&measured(704, false), 1, 262_144).unwrap(), 705);
    }

    #[test]
    fn enforce_rejects_over_ram() {
        let err = enforce(&measured(2000, false), 100, 1024).unwrap_err();
        assert!(err.contains("RAM budget exceeded"), "got: {err}");
    }

    #[test]
    fn enforce_rejects_a_dynamic_frame() {
        // A non-static frame means the bound is unsound — fail even if it would
        // otherwise fit comfortably.
        let err = enforce(&measured(64, true), 0, 262_144).unwrap_err();
        assert!(err.contains("not bounded"), "got: {err}");
    }
}
