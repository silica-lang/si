# Programs, Boards, SoCs & Devices

A Silica project has three layers that fit together: a **device** describes a
peripheral, a **board** describes a concrete chip and how everything is wired,
and a **program** brings up reactions against a board. This page walks through
each one with real code.

## A leaf device: `uart`

A *leaf* device is backed directly by memory-mapped registers. It declares four
sections — `regs` (the memory-mapped truth), `config` (typed fields with
constraints), `needs` (typed wiring to other devices), and `ops` (its
capabilities, guarded by `when` state) — plus an optional state set and a
`safe_state`.

```si
device uart implements byte_sink, byte_source {
  regs {
    SR  : reg32 at 0x00 access ro { txe: bit[7], rxne: bit[5], busy: bit[3] }   // status: read-only
    DR  : reg32 at 0x04 access rw side_effect pop_on_read { data: field[7:0] }  // read consumes a byte
    BRR : reg32 at 0x08 access rw { div: field[15:0] }
    CR1 : reg32 at 0x0C access rw { enable: bit[13], rxneie: bit[5], txeie: bit[7] }
  }

  config {
    baud   : u32 where baud in 1_200 ..= 4_000_000
    parity : enum { none, even, odd } = none
    bits   : enum { seven, eight, nine } = eight
  }

  needs {
    clock : clock_source                 // typed reference, not a phandle
    irq   : irq_line
    pins  : pin_group                    // typed pad mux; one owner per physical pad
  }

  states { off, ready }
  safe_state = off

  ops {
    op enable() when off -> () {
      BRR.div  = comptime clock.hz / baud      // baud-rate divisor; computed at compile time
      CR1{ enable = 1, rxneie = 1 }            // single read-modify-write, volatile + ordered
      become ready                              // state transition
    }

    op write(b: u8) when ready -> () or fault {
      poll SR.txe == 1 within 2ms else fault timeout    // bounded busy-wait; does NOT yield
      DR.data = b
    }

    emits rx_ready : event when SR.rxne and CR1.rxneie   // RXNE pending AND its IRQ enabled
  }
}
```

A few things worth calling out:

- **Registers are volatile and correctly ordered automatically.**
  `CR1{ enable = 1, rxneie = 1 }` is one read-modify-write; `DR.data = b` is one
  volatile store. You never write `volatile` or memory barriers by hand — those
  are properties of the *register type*, not of the access site. The
  [registers & bit-fields](../types/registers.md) chapter covers `reg32`,
  `bit[..]`, `field[..]`, `access`, and side-effect flags like `pop_on_read`.
- **`config` fields are typed and constrained.** `baud : u32 where baud in
  1_200 ..= 4_000_000` is checked at every use and override site.
- **`needs` are typed references, not phandles.** `clock : clock_source` names
  exactly what kind of thing must be wired in, and the board supplies it by name.
- **`ops` are guarded by state.** An op runs only `when` the device is in a given
  state, and `become ready` is the only way to change that state. States are
  explicit and finite.
- **Two bounded-wait spellings.** `poll <cond> within <d> else fault` is a
  bounded *busy-wait* that does not yield the scheduler. Its sibling `await`
  *suspends* — see [The Reactive Model](reactive.md) and
  [Suspension & yields](../execution/suspension.md).
- **`implements byte_sink, byte_source`** declares the interfaces this device
  provides — the basis of composition. See
  [Devices & Interfaces](../types/devices.md).

## A board: a concrete SoC and its wiring

A **board** is a typed value describing a concrete SoC plus wiring: its memory
map, clock tree, and peripheral instances. It is the typed replacement for a
Devicetree board file — types in the language, named references instead of
phandles, grammar-level relations instead of cell arrays, and typed literals
instead of preprocessor macros.

```si
board nucleo_f401re {
  soc stm32f401re {
    memory {
      flash : region at 0x0800_0000 size 512K
      sram  : region at 0x2000_0000 size 96K
    }
    clocks {
      hse  : clock_source = 8MHz
      sysclk : clock_source = pll(hse, mul = 84, div = 8)   // 84MHz, computed
    }
    irqs { usart2_irq : irq_line = 38 }
  }

  // GPIO ports are ordinary device instances too — no privileged built-ins
  gpio_a : gpio at 0x4002_0000 { needs { clock = soc.sysclk } }
  gpio_c : gpio at 0x4002_0800 { needs { clock = soc.sysclk } }

  // Pad multiplexing is a typed, checked resource: every physical pad has exactly one owner.
  // Assigning the same pad twice, or an alt-function the pad cannot provide, is a compile error.
  pinctrl {
    usart2_pins : pinmux {
      tx = gpio_a.pin(2) as alt_fn(7) drive push_pull speed high
      rx = gpio_a.pin(3) as alt_fn(7) pulling up
    }
  }

  // peripheral instances are typed, checked against all four device sections
  usart2 : uart at 0x4000_4400 {
    config { baud = 115_200 }
    needs  { clock = soc.sysclk, irq = soc.usart2_irq, pins = pinctrl.usart2_pins }
  }

  led_user  : gpio.pin = gpio_a.pin(5)  as output
  btn_user  : gpio.pin = gpio_c.pin(13) as input pulling up

  // The hardware watchdog is a first-class device the scheduler feeds automatically.
  watchdog : wdt at 0x4000_2C00 { config { timeout = 100ms } }
}
```

What the board does:

- **Describes the SoC.** Memory regions, the clock tree, and IRQ numbers are all
  named, typed values.
- **Instantiates peripherals at addresses.** `usart2 : uart at 0x4000_4400`
  places a `uart` instance and supplies its `config` and `needs`. Each instance
  is checked against all four sections of its device type.
- **Wires by name.** `needs { clock = soc.sysclk, ... }` connects typed
  references; there are no positional cell arrays.
- **Treats pad muxing as a checked resource.** Every physical pad has exactly one
  owner. Assigning the same pad twice, or an alt-function the pad cannot provide,
  is a compile error.
- **Uses typed literals.** `512K`, `8MHz`, `115_200`, `100ms` carry units and are
  checked at their use sites — see [Literals & units](../types/literals.md) and
  [Time & durations](../types/time.md). Because `pll(hse, mul = 84, div = 8)` is
  evaluated at compile time, the resulting `clock_source` has a statically known
  `.hz`, which is what makes `clock.hz / baud` in the uart's `enable` op a
  constant.

## A program: using a board

A **program** binds to a board and declares reactions. It is where execution
actually lives.

```si
program blink {
  use board nucleo_f401re as board

  let led    = board.led_user
  let button = board.btn_user

  cell lit : bool = false           // shared state

  every 500ms {
    lit = not lit
    led.set(lit)
  }

  on button.falling {
    lit = not lit
    led.set(lit)
  }
}
```

A program `use`s a board, binds convenient `let` names to entities on it,
declares any shared `cell` state, and then lists its reactions. The two reaction
forms — `every <duration>` and `on <event>` — are the whole concurrency model.
The next chapter, [The Reactive Model](reactive.md), is all about them.
