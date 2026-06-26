# Atomicity & Interrupt-safety

This is the headline feature of Silica's execution model. State shared between an
interrupt-driven `on` handler and an `every` handler — or between any two
[reactions](../language/reactive.md), now including a yielded handler and the reaction that runs
while it is suspended — needs protection. Silica makes that protection a **language construct, not
a manual `disable_irq`**.

## Compiler-computed critical sections

Shared mutable state is declared as a `cell`. From the program, the compiler builds the static
**reaction↔cell access graph**: which reactions read or write which cells, and at what priority
each reaction runs. From that graph it computes, per cell, the minimal critical section using a
**priority-ceiling protocol**: access to a cell raises priority to the ceiling of all reactions
touching it, for the shortest possible span, masking exactly the interrupts that could race and
no others.

```si
cell counter : u32 = 0

on tick.elapsed     { counter += 1 }        // compiler: this access needs the ceiling section
every 1s            { let c = counter; counter = 0; report(c) }
```

You write neither locks nor interrupt masks. Because the access set is fully static, the analysis
is exact — no over-broad "disable all interrupts" hammer, and no missed race. On metal the
section lowers to a `BASEPRI` adjustment, masking precisely the racing priorities.

A cell touched by only one reaction needs **no** critical section at all, and the compiler proves
it section-free. That is the reactive-model-native answer to the classic
shared-state-with-an-ISR problem, and it becomes *more* necessary under
[suspension](suspension.md), where a yielded handler and its successor can interleave.

## Yield-aware ownership rules

Suspension makes the priority-ceiling section necessary but **not sufficient**: a section that
spanned a yield would stall the scheduler, defeating the whole point. So the rules are:

- **A cell borrow cannot cross a yield.** The ceiling section is confined to a single
  run-to-completion segment; the compiler rejects code that holds a cell reference across an
  `await`. Read the cell, yield, re-read after resume — the intervening writer is visible, by
  design.
- **Multi-statement atomic updates use an explicit construct** — `atomic { ... }` over one or more
  cells — checked to be yield-free and bounded, so "read-modify-write a pair of cells together"
  has one spelling instead of accidental tearing.
- **DMA-shared buffers are not cells.** A buffer handed to DMA needs typed ownership transfer plus
  cache-coherency/barrier handling, not a priority-ceiling section — masking interrupts does not
  stop a DMA engine.
- **NMI and hard-fault contexts are outside cell protection.** Priority ceiling masks only
  maskable interrupts; an NMI or the Layer-3 fault path can run regardless, so any state they
  share must be lock-free/single-word and is called out as such.

## The `atomic { }` block

The compiler already inserts a ceiling section around each *individual* shared-cell access. The
`atomic { }` block groups several updates into **one** indivisible section, so another reaction
can never observe a partial update:

```si
cell lo : u32 = 0
cell hi : u32 = 0

// Advance both halves as one indivisible step.
every 1000ms {
  atomic {
    lo = lo + 1
    hi = hi + 1
  }
}

// A second reaction shares the cells, so they need a ceiling — and the
// `atomic` block above is what keeps the pair consistent against it.
every 700ms {
  lo = lo + 10
  hi = hi + 10
}
```

Because the second reaction also touches `lo` and `hi`, those cells need a ceiling; the `atomic`
block is what keeps the pair advancing together, so no observer ever sees `lo` updated without
`hi`. An `atomic` block may **not** contain a yielding op — a held ceiling cannot survive a
suspension. (Full example: `examples/atomic.si`.)
