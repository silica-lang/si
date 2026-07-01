# Debug: implicit narrowing is rejected

The program in `before.si` does not compile: it assigns a `u32` accumulator into
a `u8` cell, which Silica rejects as an implicit narrowing (§4.3). Fix it so it
compiles, keeping the truncating behaviour explicit.

The idiomatic fix is the single visible escape hatch for this: an explicit
`as u8` cast at the narrowing site — not widening the cell or changing the math.
