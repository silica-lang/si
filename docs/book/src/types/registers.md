# Registers & Bit-fields

In Silica, a hardware register is a *type*, not a raw pointer. Reading a field
isn't bit-twiddling on a `u32` — it is a typed access whose offset, mask, shift,
and access semantics are known to the compiler. That means the easy-to-get-wrong
details — volatility, ordering, read-modify-write hazards — are handled for you,
and the few places you still want raw access are *visible*.

## Registers and bit-fields are first-class

Bit and register fields are first-class:

- **Named single bits**: `SR.busy`.
- **Named multi-bit ranges**: `CR.mode[2:0]` (declared `mode: field[2:0]`).
- **Enums mapped to field values**: declare `mode: field[2:0] as Mode`, then
  write `CR.mode = fast`.
- **Single-op read-modify-write over several fields**:
  `CR1{ enable = 1, rxneie = 1 }`.
- **Raw bitwise ops** when you want them: `CR1.raw |= 0x2000`.

## A typed access, not a raw pointer

The register *type* carries the access semantics, so **every access is volatile
and correctly ordered, automatically**. Ordering between accesses to the *same*
peripheral is preserved; ordering *across* peripherals uses the minimal barrier
the target requires. The programmer never reasons about `volatile` or fences —
that is exactly the kind of hidden, easy-to-get-wrong detail the language
removes.

A `raw` escape hatch exists at the field level (`CR1.raw`) for the exotic ~5% of
cases. It is *opt-in and visible* — you can grep for `.raw`.

## Access qualifiers

"Volatile and ordered" is necessary but not sufficient. Hardware registers are
not just memory that must not be cached; their *semantics* differ per field, and
getting them wrong silently corrupts state. So a `reg`/`field` declaration
carries explicit access qualifiers, and the compiler enforces them:

| Qualifier | Meaning | What the compiler does |
| --- | --- | --- |
| `ro` / `wo` / `rw` | read-only / write-only / read-write | rejects an illegal direction at compile time |
| `w1c` | write-1-to-clear | a "clear" lowers to writing `1` to that bit, **never** read-modify-write |
| `rc` | read-to-clear / read-has-side-effects | the read is treated as an effect; never elided, reordered, or duplicated |
| `side_effect pop_on_read` | reading a data/FIFO register **consumes** data | a destructive read; the simulator and debug "watch" views must not peek it (watching a FIFO would drain it) |
| `reserved` | reserved bits | preserved across any read-modify-write; never written with arbitrary values |
| `reset = <v>` | power-on reset value | known statically; feeds the generated startup and the simulator |
| `width = 8\|16\|32` | required access width | byte/half/word access enforced; no illegal narrowing/widening of the bus access |

Qualifiers attach at the **register or the field** level
(`SR : reg32 ... access ro { ... }`, or a per-field `txe: bit[7] access ro`), so
a status register that mixes read-only flags with a `w1c` bit is described
exactly. This matters most precisely where the simple "RMW everything" model is
*wrong*: writing a multi-field update to a register that contains a `w1c` status
bit would inadvertently clear it; an `rc` data-register read must not be
duplicated by the optimizer; reserved bits must survive.

The access model also states ordering obligations that the bare "volatile" claim
glosses over: a **barrier is required** before enabling an interrupt source and
around DMA buffer hand-off — the store that arms DMA must not be reordered before
the buffer is written.

## How registers lower to MMIO

The C backend deliberately **does not emit C bitfields** for any of this — their
layout is implementation-defined. Instead, register access lowers to explicit
masked loads and stores on fixed-width volatile pointers: a typed access is, at
the end, a concrete `{offset, mask, shift, access}` against the device's claimed
address range. How that lowering targets a specific board's memory map is covered
under [Targets & MMIO lowering](../tooling/targets.md).
