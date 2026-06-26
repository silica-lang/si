# Literals & Compile-time Evaluation

Two related features keep Silica's constants honest. **Typed literals** carry
their units so a wrong unit is a type error, and a bounded **`comptime`**
sublanguage computes values at compile time without ever turning into unbounded
computation.

## Typed literals

Literals carry units and are checked at use:

- **Sizes** — `4K`, `512K`, `64M`.
- **Rates** — `115_200`, `16MHz`.
- **Signal polarities / edges** — `level-high`, `falling`.
- **Voltages** — `3v3`, `1v8`.
- **Durations** — `500ms`, `2us`.

These replace the C preprocessor's stringly-typed constants. Because the unit is
part of the literal, using one where another is expected is a type error: `16MHz`
is a `clock_source`-compatible frequency, and assigning it where a `duration` is
expected is rejected. (Durations are covered in more depth under
[Time as a Type](time.md).)

## Compile-time evaluation

A bounded `comptime` sublanguage computes values at compile time:

- register divisors — `comptime clock.hz / baud`;
- lookup tables — sine, gamma, or CRC LUTs as `comptime` array initializers;
- computed addresses;
- **pool sizes**.

`comptime` is *total and bounded* — bounded loops and recursion, no unbounded
computation. That property is exactly what keeps the memory model statically
sized: a pool's size must be a `comptime` value, so it is known before the program
runs. The same evaluator produces the linker script, the vector table, and the
`.data`/`.bss` layout from the board type — so the constants you write and the
artifacts the toolchain emits come from one source of truth.
