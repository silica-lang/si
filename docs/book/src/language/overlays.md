# Typed Overlays

An **overlay** is a typed, structured edit over named entities — `set`, `extend`,
and `remove` against a target like a board. Crucially, an overlay is *not* text
or fragment merging: every edit addresses a **named path** and is checked against
the target's schema, the same way ordinary code is checked. This is
simultaneously the Devicetree-overlay replacement and the surface an agent would
use to edit a program's graph; the two goals collapse into one mechanism.

## The shape of an overlay

```si
overlay tune_uart for board.nucleo_f401re {
  set    usart2.config.baud = 9_600
  extend usart2.needs { dma_tx = soc.dma1.stream6 }
  remove led_user
}
```

Every edit is type-checked against the target's schema:

- `set usart2.config.baud = 9_600` checks that `baud` is a `config` field and
  that `9_600` satisfies its `where` constraint.
- `extend usart2.needs { dma_tx = ... }` checks that `dma_tx` is a declared (or
  extendable) need with a matching type.
- `remove led_user` checks that the entity exists and that nothing still
  references it.

A malformed overlay fails to compile, the same way malformed code does. There is
no "the merge applied but the result is nonsense" failure mode that text-based
Devicetree overlays have.

Because edits address **named paths** and never textual positions, an agent can
emit them deterministically, and they are a natural unit for a future
content-addressed store.

## A worked example

This overlay retunes a watchdog window and drops a spare sensor the final image
does not need. Building the program then uses the *patched* board.

```si
board base {
  soc s {
    memory {
      flash : region at 0x0 size 1024K
      ram   : region at 0x2000_0000 size 256K
    }
    clocks { sysclk : clock_source = 64MHz }
  }
  i2c0  : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env   : bme280 { needs { bus = i2c0 } }
  spare : bme280 { needs { bus = i2c0 } }            // dropped by the overlay
  wdt0  : wdt at 0x4001_0000 { config { timeout = 50ms } }
}

overlay tune for board.base {
  set wdt0.config.timeout = 200ms   // retune (checked: 200ms satisfies `where`)
  remove spare                      // drop the unused sensor
}

program app {
  use board base as b
  let sensor = b.env
  cell samples : u32 = 0

  every 100ms {
    let t = sensor.read_temp()?
    samples = samples + 1
  }
}
```

The overlay is applied to `base` *before* the board is built, so the patched
value still goes through the normal `config` constraint check — `200ms` has to
satisfy the watchdog's `where` constraint just like any literal written inline.

## What is implemented today

Silica is early and experimental, so it is worth being precise about what works
right now:

- **`set` and `remove` are implemented at compile time.** `overlay <name> for
  board.<b> { … }` is parsed and applied to the target board before it is built,
  so the existing config `where`-check validates the patched value.
  `set <inst>.config.<field> = <value>` checks that the instance and config field
  exist and overrides the value (an out-of-range value fails its `where`
  constraint). `remove <name>` deletes an instance or pin binding and errors if
  it doesn't exist. An overlay targeting an unknown board is rejected.
- **`extend <inst>.needs { … }` is not yet applied.** It currently parses but is
  rejected — a noted follow-up.
- **The `remove` dangling-reference check is not yet enforced.** The intent is
  that `remove` errors if something still references the entity; that check is
  still to come.
- **The agent overlay-edit *API* is out of scope for now.** The overlay grammar
  exists; programmatic emission by an agent is future work.
