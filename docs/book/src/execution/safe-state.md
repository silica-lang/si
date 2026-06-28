# Safe State

When something goes wrong, what should the hardware *do*? Silica's answer is that each device
**declares its own safe state**, and the fault path can drive every device into it before
deciding what happens next.

## Devices declare their safe state

`safe_state = off` for a motor, `= open` for a relief valve, `= sleep` for a sensor — what is
safe is device knowledge. A motor off is safe; a valve *open* may be the safe one. A generic
fault handler cannot infer that, so safe-state is a first-class part of the device type.

```si
device motor {
  regs {
    CTRL : reg32 at 0x00 access rw { enable: bit[0] }
  }
  states { running, off }
  safe_state = off
  ops {
    op run()  -> () { CTRL.enable = 1 }
    op safe() -> () { CTRL.enable = 0 }   // de-energize (non-yielding, bounded)
  }
}
```

On an unrecovered fault, the [Layer-3 handler](faults.md) can drive all devices to their safe
states before deciding what to do next. The post-safe-state policy is itself declarable —
`panic-and-reset` versus `transition-to-safe-state-and-hold` — per program and overridable per
device.

```si
on sys.start {
  m.run()           // energize
}

every 1000ms on fault safe {
  let t = sensor.read_temp()?   // a fault here drives everything safe
}
```

Here a motor is energized at boot; when the control loop's sensor read NAKs with no recovery, the
`safe` disposition drives every device to its declared safe state, and the motor's own bounded,
non-yielding `safe` op writes the de-energize bit. (Full example: `examples/safe_state.si`.)

## Safe ops run in a degraded world

The fault path may face a wedged bus, a clock that is off, or already-corrupt RAM — so driving
everything to safe state cannot rely on the normal machinery. A `safe` op is therefore required to
be:

- **bounded** — a hard time/step cap;
- **idempotent** — running it twice is harmless;
- **non-allocating**;
- **preferably non-yielding** — it should not depend on the scheduler it may be escaping.

A safe op **may itself fail**, and the device declares the fallback when it does — assert a
hardware fail-safe line, or fall through to reset. Crucially, software safe-state is the *second*
line of defence. The design assumes **hardware fail-safe** (pull-downs and biasing so the
de-energized state is the safe one) and an independent **watchdog** that forces a reset if the
fault path itself hangs. Silica models and sequences the software part; it does not pretend
software alone makes a system safe.

## The watchdog is part of the runtime

A software-only fault decoder cannot recover a CPU stuck in an interrupt storm, a livelock, or a
wedged bus — so the watchdog is not left to the programmer to remember. A board declares one as an
ordinary device, and the **scheduler owns feeding it**: the generated event loop emits the feed
only on a clean return to the idle/dispatch point.

```si
board rig {
  // ...
  wdt0 : wdt at 0x4001_0000 { config { timeout = 100ms } }
}

program app {
  use board rig as r
  let sensor = r.env

  every 1000ms {
    let t = sensor.read_temp()?   // if the bus wedges here, the watchdog saves us
  }
}
```

If a reaction hangs — here the I²C bus wedges mid-transaction and never completes — it never
returns to idle, starves the watchdog, and the hardware resets. The consequence is deliberate: **a
reaction that overruns its declared `within` budget starves the watchdog and triggers a hardware
master reset** rather than hanging silently. The feed is never sprinkled through user code — that
is how watchdogs get defeated, fed by the very loop that is stuck — it is a property of the
scheduler, like the critical sections of [atomicity](atomicity.md). (Full example:
`examples/watchdog.si`.)

> **Status (implemented — `within` on metal).** Beyond the watchdog catching a handler that
> *never* returns to idle, a per-reaction `within <d>` deadline is now enforced on metal for
> yielding reactions when a watchdog is declared: a `__deadline_N` countdown (in the **TIMER2**
> 1 ms tick) is armed at trigger entry, decremented on each tick, and disarmed when the frame
> returns to idle; an overrun latches a flag that gates off the idle-loop watchdog feed, forcing a
> reset. This catches a handler that is merely *too slow* (would eventually complete), a tighter
> bound than "never idle." Proven on nRF52840 in Renode on **both** the C and LLVM backends.
> **Remaining:** it requires a declared watchdog (the reset path); non-yielding reactions are
> bounded by ISR run-to-completion (no mid-handler check); resolution is the 1 ms TIMER2 tick.
> Windowed and multi-stage watchdogs are deferred-not-foreclosed.
