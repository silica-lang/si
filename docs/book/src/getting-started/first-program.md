# Your First Program

This page walks through the two smallest Silica programs: a hello-world host program, and
the canonical blink + button reactive program.

## Hello, Silica

The smallest example lives in [`examples/hello.si`](https://github.com/silica-lang/si/blob/main/examples/hello.si):

```si
program hello {
    on sys.start {
        host_io.print("Hello from Silica!\n")
        host_io.flush()
    }
}
```

Compile it through the host C backend and run it:

```sh
silicac hello.si -o hello
./hello
```

A `program` is the top-level unit of execution. Everything a Silica program does is a
**reaction** to an event — here, `on sys.start { … }` runs once when the program starts.
There is no `main` and no implicit control flow; you describe *what reacts to what*.

## Blink + button

The canonical reactive-core program toggles an LED on a timer **and** on a button press,
sharing a single piece of state. See
[`examples/blink_button.si`](https://github.com/silica-lang/si/blob/main/examples/blink_button.si)
for the full program, board, and simulation script. The heart of it is:

```si
program blink {
  use board nucleo_f401re as nucleo

  let led    = nucleo.led_user
  let button = nucleo.btn_user

  cell lit : bool = false

  every 500ms       { lit = not lit; led.set(lit) }
  on button.falling { lit = not lit; led.set(lit) }   // shares `lit` — critical section auto-computed
}
```

Run it deterministically in the simulator:

```sh
silicac --sim examples/blink_button.si
```

### What each piece means

- **`program blink { … }`** — the top-level program named `blink`.
- **`use board nucleo_f401re as nucleo`** — binds a [board](../language/structure.md)
  definition (its SoC, memory, clocks, pins) under the local alias `nucleo`. The board is
  defined elsewhere in the same file.
- **`let led = nucleo.led_user`** — an immutable binding to one of the board's typed pins.
- **`cell lit : bool = false`** — a `cell` is mutable reactive state. Unlike a hidden
  global, a cell is named, typed, and the compiler tracks exactly which reactions touch it.
  See [the reactive model](../language/reactive.md).
- **`every 500ms { … }`** — a periodic reaction; `500ms` is a typed [duration](../types/time.md).
  Here it runs every half second.
- **`on button.falling { … }`** — an event reaction that fires on the falling edge of the
  button pin.

### The auto-computed critical section

Both reactions read and write `lit`. Because two reactions touch the *same* cell, the
compiler computes a **critical section automatically** — a priority-ceiling region, with no
`disable_irq` anywhere in the source. On metal it lowers to real BASEPRI masking; a cell
touched by only one reaction is *proven* section-free and needs no protection at all. The
details live in [atomicity](../execution/atomicity.md).

From here, dig into [the reactive model](../language/reactive.md) or browse the
[Examples Tour](../examples.md).
