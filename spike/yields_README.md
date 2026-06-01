# Spike: `yields` state-machine lowering (Phase 1 de-risk)

`yields_prototype.rs` validates the central new mechanism of Phase 1 (§5.2):
**run-to-completion *between* yields**. It's a throwaway, self-contained model (no
deps, not part of the compiler) of the lowering *target* and its execution.

```sh
rustc -O spike/yields_prototype.rs -o /tmp/yp && /tmp/yp
```

## The risk

The device-composition keystone (§3.5) makes a bus transaction a first-class,
*slow*, `yields` op: the handler must suspend, the scheduler must run other work,
and the handler must later resume with its locals intact. That's the Embassy-style
state-machine transform — the one genuinely novel compiler capability in Phase 1,
and where the phase could go sideways. This spike answers: **is the model
expressible as explicit state, and does it execute correctly?**

## Result: yes ✅

The prototype models a reaction as the lowering would emit it — a list of
straight-line **segments** split at suspension points, plus a statically-sized
**frame** for locals that live across a yield — and runs two reactions on a
virtual-time scheduler:

- `sensor` (`every 1ms`): seg0 starts a bus read and **yields** (suspends 2µs);
  seg1 resumes when the transaction completes.
- `button` (`on falling`, higher priority): a fast, non-yielding `counter += 1`.

Trace (button injected *during* the sensor's suspension):

```
[1us] sensor seg0: start bus read (counter sampled = 0)
[1us] sensor yield -> resume at 3us, seg1
[1us] button counter -> 1
[2us] sensor re-fire dropped (still in-flight) — coalesced (§5.1)
[3us] sensor seg1: bus done raw=0xABC; counter now = 1 (was 0 before yield)
```

Checks that pass:
- **sensor suspends** on the bus transaction;
- the **button runs during the suspension** (the scheduler interleaves);
- the post-yield **re-read sees the concurrent write** (`counter now = 1, was 0`)
  — the §5.5/D03 rule that a cell borrow may not span a yield, enforced by reading
  through the frame, not a held reference;
- a periodic **re-fire while in-flight is coalesced** (§5.1, single live
  activation) — a bonus the model gets for free.

## Implications for the real implementation

- **SIR:** `SirReaction` gains a state-machine form — an ordered list of segments
  (each a `Vec<SirStmt>` ending in `Done` or a `Yield{ wake, next }`) plus a
  per-reaction frame layout (the live set across each yield). Frames of
  disjoint-lifetime reactions union into shared storage (§5.3/SIL-005), so the
  static RAM budget (already computed for Phase 0) extends naturally.
- **Suspension wake condition:** here it's a virtual-time delay; in reality it's a
  *completion event* (bus/DMA done → an IRQ). That slots into the existing event
  queue exactly like the `Resume` events here.
- **Simulator:** add `Resume` events + a per-reaction frame/segment cursor +
  `in_flight` flag to the existing discrete-event loop (`sim/mod.rs`) — a small,
  localized extension, not a rewrite.
- **Lowering (resolver/compiler):** split an op/handler body at each `yields`
  call into segments; compute the locals live across each split → the frame; the
  cell-across-yield check (§5.5/D03) becomes "reject a cell borrow whose live
  range crosses a segment boundary."
- **Metal codegen:** a reaction lowers to `switch (state) { case N: ... }` over an
  explicit state variable + the frame struct (§6.2) — no new runtime, fits the
  freestanding-C subset already in place.

## Not covered here (the actual Phase-1 work)

The mechanical body-splitting transform; the live-set/frame computation; composed
ops as transactions over a substrate device (§3.5); bus arbitration + the bounded
per-bus wait queue (§3.5/D06); and the Layer-1/2 fault model (`?`/dispositions)
that yielding bus ops return through. The spike de-risks the *execution model*;
those are the build.

**Verdict:** the run-to-completion-between-yields model is sound and cheaply
expressible as segments + frame + resume-events. Phase 1 can proceed on this basis.
