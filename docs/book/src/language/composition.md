# Composed Devices on a Bus

This is the keystone of Silica's design. A `device` can be **implemented in terms
of another device** — a bus — and express its `ops` as **transactions on that
bus** rather than raw memory-mapped I/O. A device's ops are defined over a
*substrate* that is either a register file (a leaf device) or another device's op
surface (a composed device). The recursion bottoms out at a leaf whose ops touch
MMIO.

## Buses are interfaces

A bus is an **interface** — a named set of ops with semantics — that a concrete
controller *provides* and downstream devices `needs`. See
[Devices & Interfaces](../types/devices.md) for interfaces and capabilities in
general.

```si
interface i2c {
  type address = u7
  // Block transfer is the primitive: ONE yield wraps an entire DMA/FIFO transaction.
  op transfer(addr: address, tx: buffer, rx: buffer)   -> () or fault yields
  // The per-register ops are thin conveniences expressed over `transfer`.
  op write_reg(addr: address, reg: u8, val: u8)        -> () or fault yields
  op read_reg (addr: address, reg: u8)                 -> u8 or fault yields
  op read_reg24(addr: address, reg: u8)                -> u24 or fault yields
}
```

**Block transfer is the primitive; per-register ops are sugar.** If every byte
crossed the bus through its own `yields` op, a multi-byte read would lower to a
deep async state machine that suspends per byte. So the wire-level primitive is a
whole-transaction `transfer(tx, rx)` that **suspends once** and can wrap a
hardware FIFO loop or a DMA channel underneath; `read_reg`/`write_reg` are thin,
readable conveniences expressed over it. The examples below keep the per-register
spelling for clarity, but a driver moving a block — a display frame, a sensor
burst — reaches for `transfer` and pays one suspension, not N.

## A composed device: `bme280` over I²C

A concrete I²C controller is a *leaf* device that `implements i2c` (its ops
bottom out in MMIO). A sensor is a *composed* device that `needs` something
providing `i2c`. It has **no `regs`** of its own — the `REG_*` names are the
sensor's *remote* register addresses, passed as `reg` arguments over the bus, not
local MMIO. (`ctrl_bits()` and `compensate()` are pure compile-time / fixed-point
helpers.)

```si
device bme280 implements sensor {
  needs {
    bus  : i2c
    addr : i2c.address = 0x76
  }

  config {
    mode       : enum { sleep, forced, normal } = normal
    oversample : u8 where oversample in 1 ..= 16 = 1
  }

  states { uninit, ready, sleep }    // `sleep` is the device typestate, distinct from the `mode` config field
  safe_state = sleep                 // driven here on fault via a bounded safe op

  ops {
    op init() when uninit -> () or fault {
      bus.write_reg(addr, REG_CTRL_MEAS, ctrl_bits())?   // `?` = propagate fault to handler
      become ready
    }

    op read_temp() when ready -> fixed<16,16> or fault yields {
      let raw = bus.read_reg24(addr, REG_TEMP)?          // a yielding bus transaction
      return compensate(raw)                              // pure fixed-point math
    }
  }
}
```

Putting it on a board and using it looks just like any other device:

```si
board sensor_board {
  soc s {
    memory {
      flash : region at 0x0 size 1024K
      ram   : region at 0x2000_0000 size 256K
    }
    clocks { sysclk : clock_source = 64MHz }
  }

  i2c0 : i2c_controller at 0x4000_3000 { needs { clock = soc.sysclk } }
  env  : bme280 { needs { bus = i2c0 } }
}

program app {
  use board sensor_board as b

  let sensor = b.env
  cell samples : u32 = 0

  every 1000ms on fault retry(max = 3) {
    let t = sensor.read_temp()?
    samples += 1
  }
}
```

## Why this one mechanism matters

- **It unifies leaf and composed devices.** The *consumer* of an op never knows
  or cares whether the substrate is MMIO or another device. `sensor.read_temp()`
  and `uart.write(b)` are called identically.
- **It is the same shape as SD-over-SPI, NIC-over-anything, flash-over-QSPI.**
  Filesystems and networking are *instances* of this pattern, not new mechanisms.
- **It forces the concurrency decision.** A bus transaction takes real time, so
  the op is marked `yields`: the handler suspends and the scheduler runs other
  ready work until the transaction completes. Composition is exactly what makes
  run-to-completion-*between-yields* necessary — see
  [Suspension & yields](../execution/suspension.md).
- **It honours "no privileged built-ins."** `i2c` is a standard-library
  interface; the controller and the sensor are equal citizens.

> **Honest caveat.** "Compiles directly to MMIO" is precisely true only for leaf
> devices. A composed op compiles to *bus transactions*, which themselves compile
> down through the controller's leaf ops to MMIO. There is no C HAL anywhere — it
> is Silica ops all the way down — but you should not expect
> `bme280.read_temp()` to MMIO into the sensor directly. It can't; the sensor has
> no memory map on this core.

## Sharing a bus

A bus is a shared resource, so composition implies an arbitration model. When two
composed devices `needs` the same controller, that controller is contended, and
the design makes contention explicit rather than hoping handlers never overlap:

- **Transactions are exclusive.** A bus transaction (start to stop) is an
  indivisible unit; the controller serves one at a time. A second reaction's
  transaction does not interleave at the wire.
- **Waiting is bounded and queued.** A reaction needing a busy bus *yields* onto
  a statically-bounded per-bus queue; a full queue triggers the declared overflow
  policy, never unbounded waiting.
- **Arbitration is deterministic.** Order of service is by reaction priority with
  a stable tie-break — the same as the scheduler — so contention does not
  introduce nondeterminism.
- **Per-device speed/mode is type-checked.** Each device's required bus speed and
  mode is part of the interface's semantic contract; incompatible co-tenants on
  one bus are a compile error.
- **Bus faults and recovery are explicit.** Arbitration-lost, stuck-SDA /
  clock-stretch timeout, and the recovery sequence are declared fault codes with
  a defined recovery op, not silent retries.

This is still the same keystone — a controller is just a device whose op surface
several consumers share — but the resource discipline is named, because "two
drivers, one bus" is where naïve composition models tend to break.

> **Status (implemented).** Multi-consumer bus arbitration is built on the simulator and
> **both** metal backends, with the surface kept **implicit** — sharing one controller
> auto-serializes, no new syntax. A transaction on a busy bus joins a bounded per-bus wait
> queue; on completion the highest-priority waiter is granted (ties broken by lowest id) and
> retries its kick, without clobbering the in-flight owner. A single-consumer bus keeps the
> simpler single-owner path unchanged. See `examples/bus_contend_nrf52840.si`, validated
> `sim ≡ metal` in Renode on both backends. (Per-device speed/mode type-checking via interface
> properties is also enforced — see [Typed Overlays](overlays.md) and `examples/bus_speed.si`.)
