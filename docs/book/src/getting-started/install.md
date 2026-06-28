# Installation & Build

Silica's compiler, `silicac`, is written in Rust. To build it you need a **Rust toolchain**
(`cargo`).

## Build & run

```sh
# Build the compiler
cargo build

# Run the blink + button program in the deterministic simulator
cargo run -- --sim examples/blink_button.si

# Compile a host program to a native binary via the C backend
cargo run -- examples/hello.si -o /tmp/hello && /tmp/hello

# Build a bare-metal nRF52840 image via the C backend (needs arm-none-eabi-gcc)
cargo run -- --target metal-nrf52840 examples/blink_button_nrf52840.si -o blink.elf

# ...or via the LLVM backend — same metal target, independent codegen path (needs llc)
cargo run -- --target metal-nrf52840 --emit-llvm examples/blink_button_nrf52840.si -o blink.elf

# Run the test suite
cargo test

# End-to-end "sim ≡ metal" gate (needs arm-none-eabi-gcc + Renode); BUILD=llvm for the LLVM path
RENODE=/path/to/renode ./harness/metal_vs_sim.sh
```

## The `silicac` CLI

```
silicac <input.si> [-o <output>] [--emit-c] [--emit-llvm] [--sim] [--target host|metal-nrf52840] [--cc <compiler>] [--opt <level>] [--std <dir>]
```

- `-o <output>` — output path for the compiled binary or image.
- `--emit-c` — emit the generated C rather than a binary.
- `--emit-llvm` — use the [LLVM backend](../tooling/targets.md) (emit LLVM IR, or build the
  image through `llc`) instead of the C backend; orthogonal to `--target`.
- `--sim` — run the program in the deterministic [simulator](../tooling/simulator.md).
- `--target host|metal-nrf52840` — pick the [target](../tooling/targets.md); defaults to
  the host C backend.
- `--cc <compiler>` — choose the C compiler to invoke.
- `--opt <level>` — override the optimization level (metal defaults to `-Os`).
- `--std <dir>` — point at the standard-library directory (see below).

## Standard-library devices

The standard-library devices (for example `gpio`, `timer`) are authored in `.si` and loaded
from `crates/silicac/std` by default. Override the location with `--std <dir>`.

Once you have a build, head to [Your First Program](first-program.md).
