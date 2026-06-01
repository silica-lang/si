//! Spike: de-risk the `yields` state-machine lowering for Phase 1 (§5.2).
//!
//! Throwaway, self-contained (no deps, not part of the compiler).  Compile+run:
//!     rustc -O spike/yields_prototype.rs -o /tmp/yp && /tmp/yp
//!
//! The risk it answers: can "run-to-completion *between* yields" be represented
//! as an explicit state machine and executed so that, while a handler is
//! suspended on a slow bus transaction, the scheduler runs *other* reactions and
//! later resumes the first one with its locals intact?  This is the Embassy-style
//! transform the composed-device keystone (§3.5) forces.
//!
//! It models the lowering target generically (not hand-coded coroutines):
//!   - a reaction is lowered to a list of straight-line **segments**;
//!   - each segment ends in `Done` or `Yield { resume_at, next }` (a suspension
//!     point with a wake condition — here a bus-transaction completion time);
//!   - locals that live across a yield are saved in a statically-sized **frame**
//!     (§5.2/§5.3) instead of on the stack.
//!
//! It also exercises the §5.5/D03 rule: a `cell` value read before a yield must
//! be **re-read** after resume (no borrow spans the suspension), so a writer that
//! runs during the suspension is visible.

use std::cmp::Ordering;
use std::collections::BinaryHeap;

// ─── The "world": shared state + trace, mutated by reaction segments ───────────

struct World {
    now_ns: u64,
    counter: u32, // a shared `cell`, touched by both reactions
    trace: Vec<String>,
}

impl World {
    fn log(&mut self, who: &str, msg: &str) {
        self.trace.push(format!("[{:>5}us] {:<8} {}", self.now_ns / 1000, who, msg));
    }
}

// ─── A reaction lowered to segments + a frame ──────────────────────────────────

/// A statically-sized frame: the locals that survive across a yield (§5.2/§5.3).
/// In the real compiler this is sized per reaction and union-ed across reactions
/// with disjoint lifetimes (SIL-005); here it is just a struct.
#[derive(Default, Clone)]
struct Frame {
    raw: u32,           // bus-read result, produced after the yield
    counter_before: u32, // cell value sampled before the yield (to show re-read)
}

/// What a segment returns when it finishes running.
enum Step {
    Done,
    /// Suspend until `resume_at`, then continue at segment `next`.
    Yield { resume_at: u64, next: usize },
}

type SegFn = fn(seg: usize, frame: &mut Frame, w: &mut World) -> Step;

struct Reaction {
    name: &'static str,
    priority: u8,     // higher = more urgent
    period_ns: Option<u64>, // periodic (`every`) if Some
    body: SegFn,
    // runtime state
    frame: Frame,
    in_flight: bool,  // single live activation (§5.1): suspended counts as in-flight
}

// ─── Reaction bodies (what the lowering would generate) ────────────────────────

/// `every 1µs { let before = counter; let raw = bus.read()? /* yields */;
///              temp = compensate(raw); log }`  (the prototype uses tiny ns
/// periods — 1µs here, 2µs bus latency — so the trace is quick to read.)
/// Two segments split by the bus transaction's suspension point.
fn sensor_body(seg: usize, frame: &mut Frame, w: &mut World) -> Step {
    const BUS_LATENCY_NS: u64 = 2_000; // a slow I2C transaction
    match seg {
        0 => {
            // straight-line: write CTRL reg, sample the shared cell, kick off the
            // bus read, then suspend until it completes.
            frame.counter_before = w.counter;
            w.log("sensor", &format!("seg0: start bus read (counter sampled = {})", frame.counter_before));
            Step::Yield { resume_at: w.now_ns + BUS_LATENCY_NS, next: 1 }
        }
        1 => {
            // resumed: the bus result is now available; re-read the cell (must NOT
            // reuse the pre-yield sample — §5.5/D03).
            frame.raw = 0x0ABC;
            let fresh = w.counter;
            w.log(
                "sensor",
                &format!(
                    "seg1: bus done raw=0x{:03X}; counter now = {} (was {} before yield)",
                    frame.raw, fresh, frame.counter_before
                ),
            );
            Step::Done
        }
        _ => Step::Done,
    }
}

/// `on button.falling { counter += 1 }` — fast, non-yielding, higher priority.
fn button_body(_seg: usize, _frame: &mut Frame, w: &mut World) -> Step {
    w.counter += 1;
    w.log("button", &format!("counter -> {}", w.counter));
    Step::Done
}

// ─── Deterministic discrete-event scheduler ────────────────────────────────────

#[derive(PartialEq, Eq)]
enum Kind {
    Fire { reaction: usize, seg: usize }, // start (seg 0) or one-shot fire
    Resume { reaction: usize, seg: usize },
}

struct Event {
    at: u64,
    priority: u8,
    seq: u64,
    kind: Kind,
}
impl PartialEq for Event {
    fn eq(&self, o: &Self) -> bool {
        self.at == o.at && self.priority == o.priority && self.seq == o.seq
    }
}
impl Eq for Event {}
impl Ord for Event {
    // Min-heap by (time asc, priority desc, seq asc): earliest first, then more
    // urgent, then stable — the §5.1 deterministic order.
    fn cmp(&self, o: &Self) -> Ordering {
        o.at.cmp(&self.at)
            .then(self.priority.cmp(&o.priority))
            .then(o.seq.cmp(&self.seq))
    }
}
impl PartialOrd for Event {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}

fn main() {
    let mut reactions = vec![
        Reaction { name: "sensor", priority: 1, period_ns: Some(1_000), body: sensor_body, frame: Frame::default(), in_flight: false },
        Reaction { name: "button", priority: 2, period_ns: None, body: button_body, frame: Frame::default(), in_flight: false },
    ];
    let mut w = World { now_ns: 0, counter: 0, trace: Vec::new() };

    let mut q: BinaryHeap<Event> = BinaryHeap::new();
    let mut seq = 0u64;
    let push = |q: &mut BinaryHeap<Event>, seq: &mut u64, at, priority, kind| {
        q.push(Event { at, priority, seq: *seq, kind });
        *seq += 1;
    };

    // Seed: the periodic sensor at its first period (t=1µs); a scripted button
    // press at 1.5µs — i.e. *during* the sensor's bus suspension (1µs..3µs).
    push(&mut q, &mut seq, 1_000, reactions[0].priority, Kind::Fire { reaction: 0, seg: 0 });
    push(&mut q, &mut seq, 1_500, reactions[1].priority, Kind::Fire { reaction: 1, seg: 0 });

    const HORIZON_NS: u64 = 4_000;

    while let Some(ev) = q.pop() {
        if ev.at >= HORIZON_NS {
            break;
        }
        w.now_ns = ev.at;
        let (reaction, seg, is_fire) = match ev.kind {
            Kind::Fire { reaction, seg } => (reaction, seg, true),
            Kind::Resume { reaction, seg } => (reaction, seg, false),
        };

        // Single live activation (§5.1): a periodic re-fire while still in-flight
        // (suspended) is coalesced/dropped.
        if is_fire && reactions[reaction].in_flight {
            w.log(reactions[reaction].name, "re-fire dropped (still in-flight) — coalesced (§5.1)");
        } else {
            reactions[reaction].in_flight = true;
            let mut frame = reactions[reaction].frame.clone();
            let step = (reactions[reaction].body)(seg, &mut frame, &mut w);
            reactions[reaction].frame = frame;
            match step {
                Step::Done => {
                    reactions[reaction].in_flight = false;
                }
                Step::Yield { resume_at, next } => {
                    // Suspend: the scheduler is now free to run other events
                    // (e.g. the button) before this resume fires.
                    w.log(reactions[reaction].name, &format!("yield -> resume at {}us, seg{}", resume_at / 1000, next));
                    push(&mut q, &mut seq, resume_at, reactions[reaction].priority, Kind::Resume { reaction, seg: next });
                }
            }
        }

        // Reschedule periodic reactions (fixed-rate from the scheduled time).
        if is_fire {
            if let Some(p) = reactions[reaction].period_ns {
                push(&mut q, &mut seq, ev.at + p, reactions[reaction].priority, Kind::Fire { reaction, seg: 0 });
            }
        }
    }

    // ─── Output + checks ───────────────────────────────────────────────────────
    println!("--- trace ---");
    for line in &w.trace {
        println!("{}", line);
    }

    let joined = w.trace.join("\n");
    let sensor_yielded = joined.contains("yield -> resume");
    let button_ran_during = {
        // button line appears between the sensor's seg0 and its seg1.
        let s0 = w.trace.iter().position(|l| l.contains("seg0: start bus read")).unwrap();
        let s1 = w.trace.iter().position(|l| l.contains("seg1: bus done")).unwrap();
        let b = w.trace.iter().position(|l| l.contains("button") && l.contains("counter -> 1")).unwrap();
        s0 < b && b < s1
    };
    let reread_after_yield = joined.contains("counter now = 1 (was 0 before yield)");

    println!("\n--- checks ---");
    println!("sensor suspended on the bus transaction      : {}", sensor_yielded);
    println!("button reaction ran DURING the suspension     : {}", button_ran_during);
    println!("cell re-read after resume sees the writer     : {}", reread_after_yield);

    assert!(sensor_yielded, "sensor must yield on the bus read");
    assert!(button_ran_during, "scheduler must interleave the button during suspension");
    assert!(reread_after_yield, "post-yield re-read must see the concurrent write (§5.5/D03)");
    println!("\nPASS: run-to-completion-between-yields is expressible and executes correctly.");
}
