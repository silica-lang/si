# Memory & Allocation

Silica's memory is **bounded, not absent**. There is no general heap and no dynamic allocation.
Instead, memory comes in statically-sized forms that the compiler sums at build time, so that
total RAM use is a compile-time constant.

## Bounded containers

Storage is declared in shapes whose size the compiler can compute:

- `pool<T, N>` — N slots of `T`; allocation returns `handle or fault`, where `fault` means
  exhausted.
- `arena` — a region you carve bounded sub-allocations from, with a reset point.
- `ring<T, N>` — a bounded ring buffer, the canonical producer/consumer queue between an `on`
  handler and an `every` handler.
- `buffer<N>` / `bytes` — bounded byte storage for DMA and protocol framing.

The `ring<T, N>` is the one that is built today. It is N slots of `T` plus head/tail/count
indices, with `push`/`pop`/`len`/`is_empty`/`is_full` ops, and the compiler sums it into the
static RAM budget — no heap. On a full ring, `push` overwrites the oldest element (a defined,
bounded overflow policy).

```si
cell q        : ring<u32, 4> = 0   // bounded queue (init ignored)
cell produced : u32 = 0
cell consumed : u32 = 0

// Producer: one sample every 100ms.
every 100ms {
  produced = produced + 1
  q.push(produced)
}

// Consumer: drains one every 300ms — slower, so the ring fills and saturates.
every 300ms {
  let v = q.pop()
  consumed = consumed + v
}
```

A fast producer fills the ring while a slower consumer drains it, so the 4-slot ring saturates
and the oldest samples are dropped — visibly bounded. (Full example: `examples/ring.si`.) Because
the ring is shared across two reactions, its ops are protected by the automatic priority-ceiling
critical section described under [atomicity](atomicity.md).

> **Status (partly deferred).** `ring<T, N>` is implemented on the simulator and **both** metal
> backends (for example, `ring<u32,16>` sums to 76 bytes, verified by the RAM-budget gate; see
> `examples/ring_nrf52840.si`). `pool<T,N>`, `arena`, and `buffer<N>`/`bytes` are not yet built;
> `T` must currently be an integer scalar (see [numbers](../types/numbers.md)); and a
> fault-on-full/empty variant, as an alternative to overwrite-oldest, is a follow-up.

## A bounded stack

"Total RAM is a compile-time constant" is only honest if the **stack** is bounded too, so Silica
bounds it explicitly:

- **Recursion is banned by default.** It is the one easy way to make stack depth unknowable. A
  bounded, annotated form may be allowed later, but the default keeps depth statically computable.
- **Local storage is bounded.** No variable-length arrays or unbounded locals; large buffers live
  in pools or arenas, not on the stack.
- **ISR nesting is accounted.** Because priorities are static (see
  [atomicity](atomicity.md)), the worst-case interrupt nesting depth is computable, and the stack
  budget includes it.
- **Suspended-handler frames are counted** as static allocations — one per reaction, sized to its
  largest live set across a yield — not as live stack.
- **Backend-generated frames count.** The C/LLVM lowering must not introduce dynamic stack
  (`alloca`, large temporaries); the lowering contract bounds call-frame size so the summed stack
  high-water mark is itself a compile-time number.

The honest claim, then, is that **statics + pools + handler frames + a bounded worst-case stack**
are summed at build time. Stack overflow becomes a *budget* failure caught at link time — plus an
MPU guard page on parts that have one — not a runtime mystery.

```si
program app {
  use board demo as b
  cell boots : u32 = 0
  cell ticks : u32 = 0

  on sys.start {
    boots = boots + 1
  }

  every 100ms {
    ticks = ticks + 1
  }
}
```

Building this for `--target metal-nrf52840` prints a RAM-budget line whose stack term is computed
from *this* program, not a fixed reserve. The two reaction priority levels here — the boot
one-shot and the periodic timer — can nest, so the stack term reflects both. (Full example:
`examples/stack_budget.si`.)

> **Status (over-approximation).** The worst-case-stack estimate is a sound over-approximation: it
> uses conservative fixed frame overheads and counts a yielding reaction's static temporaries as
> if on-stack, rather than the toolchain's exact `-fstack-usage` numbers. The **frame-union**
> optimization — overlapping frames with disjoint lifetimes, the static analogue of stack reuse —
> is not yet applied. Both can only make the summed budget *smaller*, never unknowable.
