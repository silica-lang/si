# The Number / Data Model

Silica's number model is built around one idea: a number's width and signedness
are *always* explicit, and conversions are never silent. There is no `int` and no
pointer-width default, so the kinds of bug that come from an implicit `int`
promotion or a quiet truncation simply cannot be written without saying so.

## Fixed-width integers

The integer types are `u8 u16 u32 u64` and signed `s8 s16 s32 s64`. Width is
always explicit, and a register field can also have an odd width such as `u7` or
`u24`.

| Concern | Rule |
| --- | --- |
| Overflow | **Traps by default** — it is a fault. Wrapping is a *distinct* operator (`+%`, `-%`, `*%`); saturating is a third (`+|`, `-|`, `*|`). |
| Widening | Implicit only when **lossless** (`u8 → u16`). |
| Narrowing | **Never implicit.** Use an explicit, fallible or truncating cast. |
| Mixed sign | **No** implicit signed/unsigned mixing. |
| Booleans | A distinct type, **not** an integer. |
| Bytes | A thin, **bounded** `buffer<N>` / `bytes` type tied to the pool/arena model. |
| Text | Minimal byte-strings only. **No Unicode / text machinery** on device. |
| Endianness | **Explicit** at the byte/buffer boundary: `u32.le`, `u32.be` when (de)serializing. |

> **Status.** The widening, narrowing, and mixed-sign rules are enforced: a
> declared-typed value assigned to a narrower type, or signed/unsigned operands
> mixed in one operation, is a compile error, and an out-of-range integer literal
> for its target type is rejected. Integer *literals* and device-op / register
> results stay width-flexible so ordinary code needs no annotation. Not yet built:
> `.le`/`.be` endianness, odd-width fields (`u7`/`u24`) in expressions, and a
> *checked* (fallible) narrowing cast — only the truncating `as` exists today.

## Casts: the single, visible escape hatch

Because narrowing and sign-mixing are never implicit, the explicit `as` cast is
the one place where you spell out a lossy conversion. It is greppable, so a
reviewer (or an agent) can find every place a value is deliberately truncated.

In `examples/casts.si`, a wide running sum is deliberately narrowed to a byte
before being stored:

```si
cell total : u32 = 0   // wide running sum
cell low8  : u8  = 0   // its low byte, via an explicit narrowing cast

every 100ms {
  total = total + 100
  low8  = total as u8   // narrowing u32 → u8: explicit, truncates at 256
}
```

The truncation is *spelled out*, not implied: `total as u8` truncates at the
target width, and without the cast this would be a compile error.

## Overflow policy

Plain `+`, `-`, and `*` **trap on overflow by default**, in the simulator *and*
on metal — there is no "silent in release" carve-out. The width that's checked is
the assignment target's type, so the same `+ 100` is safe in a `u32` cell and a
trap risk in a `u8` cell. When wraparound or clamping is genuinely what you want,
you say so:

```si
cell wrapped   : u8  = 200   // +% 100 → 44 (mod 256)
cell saturated : u8  = 200   // +| 100 → clamps to 255 and stays
cell safe32    : u32 = 0     // plain + is checked but never overflows here

every 100ms {
  wrapped   = wrapped   +% 100   // wrap (two's-complement, at target width)
  saturated = saturated +| 100   // saturate (clamp to the type's min/max)
  safe32    = safe32    +  1000
}
```

That example is `examples/overflow.si`. On an overflow, the trap is the
hardware-level fault path: in the simulator it drives the system to its
[safe state](../execution/safe-state.md) (an `OVERFLOW TRAP` trace); on metal the
generated helper calls the overflow trap and halts. How a trap relates to
fallibility is covered in [Faults & Fallibility](faults.md); the broader fault
machinery is in [Execution-level faults](../execution/faults.md).

> A scoped directive `@overflow(saturate | wrap | trap)` is planned to set the
> default arithmetic mode over a block or op — useful because the *correct*
> behaviour for a real-time control loop is usually saturation, and writing `+|`
> on every line is noisy. Trap stays the global default everywhere it is not
> explicitly overridden. The per-operator opt-out covers the same ground today;
> the block directive is not yet implemented.

Because event sources interact with overflow as well — a re-firing reaction whose
pending slot is full — see how the event-source overflow *policy* differs from
arithmetic overflow under [Atomicity & re-entrancy](../execution/atomicity.md).

## Fixed-point

Fixed-point is first-class, and the binary point lives in the type:
`fixed<16,16>` is 16 integer bits and 16 fractional bits. The compiler handles
scaling on multiply and add — a `fixed<16,16>` multiply computes in a wider
intermediate, then rescales, and the rescale obeys the same overflow rule. This
is the default way to do fractional math on the many parts that have no FPU, and
it needs no FPU because it is integer math underneath.

## Float, gated on an FPU capability

Float is not in the core. It is opt-in and allowed *only* if the target SoC type
**declares an FPU** — an `fpu` capability (see
[Devices, Interfaces & Capabilities](devices.md)). Using `float` on an FPU-less
part is a **compile error**, not a silent soft-float fallback: the design refuses
rather than emit slow soft-float. (This is not foreclosed — soft-float could
later be a std-lib capability that satisfies the same `fpu` requirement.)

In `examples/fpu.si`, the SoC declares the capability, so a `float` cell is
allowed:

```si
soc s {
  // ...
  clocks { sysclk : clock_source = 64MHz }
  fpu                       // Cortex-M4F: hardware single-precision FPU
}
// ...
cell reading : float = 0    // allowed: the SoC declares an FPU
```

Deleting the `fpu` line turns the `reading` cell into a compile error.

> **Status.** The FPU gate is implemented: `float`/`f32`/`f64`/`double` resolve
> to single/double types, and a `float` cell or `let` on a board whose SoC does
> not declare `fpu` is a compile error. Float *arithmetic* at runtime (sim ops,
> float literals) is a follow-up — today a `float` value is carried and stored but
> not computed on.
