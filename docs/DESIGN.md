# Silica — Design Document

> An experimental, embedded-native and agentic-native programming language.
> **Name:** Silica (after the silicon substrate). **File extension:** `.si`.
> Status: design draft, spec-level. This document describes intent and structure; it is not an
> implementation. It is meant to be versioned alongside the eventual repository and evolve with it.

---

## Pre-publish checklist (not yet performed)

These are recorded here as a gate, not done in this draft (they require live network checks and a
publishing decision):

- [ ] Confirm the `silica` GitHub org is free; fall back to `silica-lang`, or the alternates
  `Ingot` / `Etch`.
- [ ] Confirm `silica` on crates.io / PyPI / npm; same fallbacks.
- [ ] Re-confirm `.si` has no live collision in any toolchain a contributor is likely to have on
  `$PATH` (known dead/irrelevant uses only: Autodesk Softimage, a 1997 game asset format).

---

## 1. Vision & the two-goals-converge thesis

Silica is built around a wager: that the language an **AI agent** most wants to author, edit, and
debug is the same language a **compiler** most wants to analyze and a **hardware engineer** most
wants to read — and that *embedded systems* are where that alignment is sharpest, because embedded
work is already about explicit resources, explicit time, and a hardware truth that does not lie.

Two design goals drive everything, and they reinforce rather than compete:

- **Embedded-native.** Semantics are built from hardware concepts — *devices, registers,
  interrupts, time, resources, capabilities* — rather than from the C-and-UNIX vocabulary
  (files, heap, stdio, errno, flat untyped memory) that most embedded toolchains smuggle in. A
  program's mental model should be the *board*, not a stripped-down PC.
- **Agentic-native.** The language is engineered to be an excellent target for AI authoring,
  editing, and debugging — not by adding "AI features," but by removing the things that make code
  hard for a machine to reason about: hidden state, ambiguous grammar, and untyped text that only
  becomes meaningful after a build.

These converge on **three shared properties**. A good compiler and a good agent want the same
things, and so does a careful human:

1. **Explicitness.** Effects, time, resources, and capabilities are visible *in types*. Nothing
   that matters to correctness is implicit. If a function touches a device, takes time, can fail,
   or needs a capability, its signature says so.
2. **Regularity.** A boring, indexable `subject verb args` grammar — *not* natural language.
   Every construct has one spelling. Entities are named, never positionally referenced. The
   grammar is something you can pattern-match, not parse-by-vibes. (Regularity is a feature for the
   agent and a constraint on the designer: resist "convenient" special cases.)
3. **Structured truth.** The canonical artifact is *validated structure*, not text that happens to
   parse. Edits are structured graph operations over named entities. The same mechanism that
   replaces Devicetree overlays is the mechanism an agent uses to edit code.

The thesis in one line: **no hidden *software* state, and nothing about the program's resources,
topology, or effects is statically unknowable.** This is deliberately a claim about software, not
physics: dynamic hardware behaviour — input levels, interrupt timing, analog values, clock
tolerance, bus faults, metastability, the environment — is real and is modelled *explicitly* as
events, faults, and bounded waits, never assumed away. What the language removes is *hidden*
state (implicit globals, untyped effects, unknowable resource use), not *uncertainty about the
world*. Wherever the embedded goal and the agentic goal seem to pull apart, that is a signal the
design is wrong, not a tradeoff to split.

**Scope.** Silica is deliberately a "toy" — an intellectual exercise — but with an aspirational
long-term ceiling: *potentially replacing an RTOS like Zephyr for personal projects.* That ceiling
is not a v1 deliverable; it is a **foreclosure constraint**. Every decision below is checked
against it. Target hardware is **fully open / documented / open-source hardware**, which sidesteps
vendor NDAs and keeps the typed peripheral library tractable.

---

## 2. Guiding principles & tenets

These are the rules the rest of the document must obey. They are load-bearing.

**Scope, not foreclosure.** Deciding *not to build something yet* is fine and expected. Designing
something that *bars a capability permanently* is not. Foreclosure lives in two places only — the
**type system** and the **memory model** — so those two are designed conservatively and reviewed
hardest. The feature list is allowed to be short; the type system is not allowed to be a dead end.
Section 10 audits the deferred list against this rule.

**No privileged built-ins.** `gpio`, `uart`, the interrupt controller, the system timer — these
are ordinary `device` types from the standard library, privileged in no way. The compiler *core*
knows only a small fixed vocabulary: `regs`, `ops`, `when`, `on`, `every`, and the type system. A
user-defined exotic peripheral is identical in status to a "built-in." There is no two-tier system
where blessed code can do what user code cannot. (This is what makes the long tail of weird
hardware expressible at all — see §3.5 and §4.1.)

**Bounded allocation, not absent allocation.** Silica has no free-for-all `malloc`. It *does* have
statically-sized pools and arenas the compiler can reason about. "A buffer pool exists" is allowed
and encouraged; "an unbounded heap exists" is not. This single tenet is what keeps networking and
filesystems structurally on the table (both need buffers) while preserving static analyzability
(total RAM use is a compile-time sum). See §5.3.

**Port the facts, not the framework.** Decades of hardware knowledge — base addresses, IRQ
numbers, clock trees — live in Zephyr's Devicetree corpus. Silica *harvests the facts* via a
mechanical transpiler (§8) but **does not** port the framework (the driver model, init levels,
Kconfig conditional compilation, function-pointer dispatch). The framework re-imports the C/UNIX
mental model and would quietly rewrite Silica into a skin over Zephyr. Drivers are designed from
**datasheets**, the real source of truth for `regs`/`ops`.

**Structured truth over text.** The source-of-record is a typed graph of named entities. Text is a
serialization. Overlays, patches, and agent edits are validated transforms on the graph, not text
merges (§3.6, §7.3).

**One mechanism where two goals meet.** Wherever the "declarative hardware" goal and the "agentic
edit" goal both apply (notably overlays), there is exactly **one** mechanism, not two. Collapsing
them is a design win, not a coincidence to be tolerated.

---

## 3. Lexical & syntactic design

### 3.1 Design constraints on the grammar

The grammar is **regular and indexable**: `subject verb args`, one spelling per construct, every
entity named, no positional/anonymous references (the Devicetree `<&phandle 0 2>` cell-array
pattern is explicitly banned — see §3.5). The banned thing is anonymous reference to *named
entities* (the phandle-by-position pattern); **typed scalar arguments are not banned** — `gpio_a.pin(5)`
passes a pad index, and `pll(hse, mul = 84, div = 8)` passes one named entity plus named scalars.
The rule is "you never refer to a *thing* by its position in an array," not "functions take no
arguments." Whitespace is not significant beyond token separation;
blocks are brace-delimited. There is **no preprocessor** and **no textual include-order
semantics** — a hard rule, because it is also what keeps content-addressed storage un-foreclosed
(§9.5).

A sketch in EBNF-ish form (illustrative, not final):

```ebnf
module      = { item } ;
item        = interface | device | board | program | overlay | const | comptime_fn ;

device      = "device" ident [ "implements" ident { "," ident } ] "{"
                [ regs_sec ] [ config_sec ] [ needs_sec ] ops_sec [ states_sec ] [ safe_sec ]
              "}" ;
regs_sec    = "regs" "{" { reg_decl } "}" ;
reg_decl    = ident ":" reg_type "at" int_lit "{" { field_decl } "}" ;
field_decl  = ident ":" ( "bit" "[" int "]" | "field" "[" int ":" int "]" )
                [ "=" ident ] [ "as" enum_ref ] ;

ops_sec     = "ops" "{" { op_decl | emit_decl } "}" ;
op_decl     = "op" ident "(" [ params ] ")" [ "when" state_expr ]
                "->" return_type [ "yields" ] block ;
emit_decl   = "emits" ident ":" "event" [ "when" cond ] ;

return_type = type | type "or" "fault" ;

program     = "program" ident "{" { use_decl | let_decl | state_decl | reaction } "}" ;
reaction    = ( "on" event_ref | "every" duration_lit ) [ "within" duration_lit ]
                [ fault_disp ] block ;
fault_disp  = "on" "fault" disposition ;

overlay     = "overlay" ident "for" path "{" { edit } "}" ;
edit        = "set" path "=" expr
            | "extend" path block
            | "remove" path ;
```

The rest of this section shows real `.si` against this grammar. The same grammar is used
throughout the document; every snippet is meant to parse under it.

### 3.2 A leaf device: `uart`

A *leaf* device is backed directly by memory-mapped registers. It declares four sections —
`regs` (the memory-mapped truth), `config` (typed fields + constraints), `needs` (typed wiring to
other devices), and `ops` (capabilities = verbs, guarded by `when` state) — plus an optional state
set and a `safe_state`.

```si
device uart implements byte_sink, byte_source {
  regs {
    SR  : reg32 at 0x00 access ro { txe: bit[7], rxne: bit[5], busy: bit[3] }   // status: read-only
    DR  : reg32 at 0x04 access rw side_effect pop_on_read { data: field[7:0] }  // read consumes a byte (§4.2)
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
    pins  : pin_group                    // typed pad mux (§3.3 pinctrl); one owner per physical pad
  }

  states { off, ready }
  safe_state = off

  ops {
    op enable() when off -> () {
      BRR.div  = comptime clock.hz / baud      // baud-rate divisor lives in BRR; computed at compile time (§4.7)
      CR1{ enable = 1, rxneie = 1 }            // single read-modify-write, volatile + ordered
      become ready                              // state transition
    }

    op write(b: u8) when ready -> () or fault {
      poll SR.txe == 1 within 2ms else fault timeout    // bounded busy-wait; does NOT yield (§5.2)
      DR.data = b
    }

    emits rx_ready : event when SR.rxne and CR1.rxneie   // RXNE pending AND its IRQ enabled; wired to `irq` (§4.1)
  }
}
```

Notes that matter:

- **Register access is always volatile and correctly ordered automatically.** `CR1{ enable = 1,
  rxneie = 1 }` is one read-modify-write; `DR.data = b` is one volatile store. The programmer never
  writes `volatile` or memory barriers — those are properties of the *register type*, not of the
  access site (§4.2).
- **Two bounded-wait spellings, one visible distinction.** `poll <cond> within <d> else fault` is a
  bounded *busy-wait* that does **not** yield the scheduler (used here because `TXE` clears in
  sub-microseconds). Its sibling `await <cond> within <d>` *suspends*, and **any op that can reach an
  `await` must be marked `yields`** (§5.2). That is why `write` is not `yields` but `read_temp` (§3.5)
  is — busy-wait vs. suspend is a type-level choice, never hidden.

  > **Status (implemented).** `await <cond> within <d> else fault <code>` parses with the same shape
  > as `poll` and lowers to `SirStmt::Await`. **Resume model (the chosen default):** on reaching the
  > `await` the handler *yields* to the scheduler; the condition is re-checked on a cadence (a
  > fraction of the budget) until it holds — resume the handler — or the `within` budget elapses —
  > raise `fault` to the reaction's Layer-2 disposition. In the sim this is a true suspension via the
  > event queue (an `AwaitRecheck` is a peer of the bus `Resume`), so other reactions run meanwhile
  > and a *different* reaction can make the condition true (modelled + tested). An `await` is a *poll*
  > of its condition, so it is exempt from the §5.5 auto-critical and is rejected inside an `atomic`
  > (a critical cannot span a suspension). **Metal:** currently lowered as a bounded re-check loop
  > (`wfi` between checks) that respects `within` — the condition can be set by an ISR between checks;
  > a full D2-style suspend/resume of the handler *frame* across the await is the noted follow-up.
- `op write(...) -> () or fault` is **fallible**: the caller must discharge the fault path or it is
  a compile error (§4.4).
- `become ready` is the only way to change `when`-state; states are explicit and finite, which is
  what makes the Layer-3 fault decoder possible (§5.4).
- `implements byte_sink, byte_source` declares the *interfaces* this device provides — the basis of
  composition (§3.5, §4.1).

### 3.3 An instance / board declaration

A **board** is a typed value describing a concrete SoC + wiring: its memory map, clock tree, and
peripheral instances. This is the typed replacement for a Devicetree board file — *types in the
language*, named references instead of phandles, grammar-level relations instead of cell arrays,
typed literals instead of preprocessor macros.

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

  // GPIO ports are ordinary device instances too — no privileged built-ins (§2)
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
  usart2 : uart at 0x4000_4400 {           // 0x4000_4400 is USART2 on this SoC — instance named for the part
    config { baud = 115_200 }
    needs  { clock = soc.sysclk, irq = soc.usart2_irq, pins = pinctrl.usart2_pins }
  }

  led_user  : gpio.pin = gpio_a.pin(5)  as output
  btn_user  : gpio.pin = gpio_c.pin(13) as input pulling up

  // The hardware watchdog is a first-class device the scheduler feeds automatically (§5.6).
  watchdog : wdt at 0x4000_2C00 { config { timeout = 100ms } }
}
```

Typed literals (`512K`, `8MHz`, `115_200`, `3v3`, `level-high`) carry units and are checked at
their use sites (§4.6). `pll(hse, mul = 84, div = 8)` is evaluated at compile time and produces a
`clock_source` whose `.hz` is known statically — which is what makes `clock.hz / baud` in the uart
`enable` op a constant.

### 3.4 A reactive program: blink + button

`on <event>` and `every <duration>` are the **entire** concurrency model. No task-create, no
semaphores, no manual RTOS plumbing.

```si
program blink {
  use board nucleo_f401re as board

  let led    = board.led_user
  let button = board.btn_user

  cell lit : bool = false           // shared state, see §5.5 for atomicity

  every 500ms {
    lit = not lit
    led.set(lit)
  }

  on button.falling {
    lit = not lit          // keep the shared cell consistent — both reactions now touch `lit`
    led.set(lit)
  }
}
```

`cell` marks state that more than one reaction may touch. The compiler computes the critical
section automatically from the static handler↔cell graph (§5.5); there is no `disable_irq` in user
code. `every 500ms` is a *primitive temporal trigger*; the compiler implements it by allocating a
timer/compare channel from the board's timer devices — the timer is an ordinary `device`, not a
privileged built-in (§4.1).

### 3.5 A composed device on a bus (the keystone)

The keystone of the whole design: a `device` can declare it is **implemented in terms of another
device** (a bus) and express its `ops` as **transactions on that bus**, not raw MMIO. A device's
ops are defined over a *substrate* that is **either a register file (leaf) or another device's op
surface (composed)**. The recursion bottoms out at a leaf whose ops touch MMIO.

Buses are **interfaces** — a named set of ops with semantics — that concrete controllers
*provide* and downstream devices `needs`:

```si
interface i2c {
  type address = u7
  // Block transfer is the primitive: ONE yield wraps an entire DMA/FIFO transaction (SIL-003).
  op transfer(addr: address, tx: buffer, rx: buffer)   -> () or fault yields
  // The per-register ops are thin conveniences expressed over `transfer`.
  op write_reg(addr: address, reg: u8, val: u8)        -> () or fault yields
  op read_reg (addr: address, reg: u8)                 -> u8 or fault yields
  op read_reg24(addr: address, reg: u8)                -> u24 or fault yields
}

**Block transfer is the primitive; per-register ops are sugar (Gemini SIL-003).** If every byte
crossed the bus through its own `yields` op, a multi-byte read would lower to a deep async state
machine that suspends per byte — large frames (§5.3) and heavy scheduler churn. So the wire-level
primitive is a whole-transaction `transfer(tx, rx)` that **suspends once** and can wrap a hardware
FIFO loop or a DMA channel underneath; `read_reg`/`write_reg` are thin, readable conveniences
expressed over it. The composed-device examples below keep the per-register spelling for clarity, but
a driver that moves a block (a display frame, a sensor burst) reaches for `transfer` and pays one
suspension, not N.
```

A concrete I²C controller is a *leaf* device that `implements i2c` (its ops bottom out in MMIO).
A sensor is a *composed* device that `needs` something providing `i2c` — it has **no `regs`** (the
`REG_*` names below are the sensor's *remote* register addresses passed as `reg` arguments over the
bus, **not** local MMIO; `ctrl_bits()`/`compensate()` are pure `comptime`/fixed-point helpers, §4.7):

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
  safe_state = sleep                 // driven here on fault via a bounded safe op (§5.6)

  ops {
    op init() when uninit -> () or fault {
      bus.write_reg(addr, REG_CTRL_MEAS, ctrl_bits())?   // `?` = propagate fault to handler
      become ready
    }

    op read_temp() when ready -> fixed<16,16> or fault yields {
      let raw = bus.read_reg24(addr, REG_TEMP)?          // a yielding bus transaction
      return compensate(raw)                              // pure fixed-point math (§4.3)
    }
  }
}
```

This one mechanism does an enormous amount of work, and is why it is designed first:

- **It unifies leaf and composed devices.** The *consumer* of an op never knows or cares whether
  the substrate is MMIO or another device. `sensor.read_temp()` and `uart.write(b)` are called
  identically.
- **It is the same shape as SD-over-SPI, NIC-over-anything, flash-over-QSPI** — i.e. the filesystem
  and networking deferrals (§10) are *instances* of this pattern, not new mechanisms.
- **It forces the concurrency decision.** A bus transaction takes real time. The op is marked
  `yields` (§5.2): the handler suspends and the scheduler runs other ready work until the
  transaction completes. Composition is what makes strict run-to-completion untenable and
  run-to-completion-*between-yields* necessary.
- **It honours "no privileged built-ins."** `i2c` is a std-lib interface; the controller and the
  sensor are equal citizens.

> **Caveat (flagged honestly): "compiles directly to MMIO" is precisely true only for leaf
> devices.** A composed op compiles to *bus transactions*, which themselves compile down through
> the controller's leaf ops to MMIO. There is **no C HAL anywhere** — it is Silica ops all the way
> down — but the reader should not expect `bme280.read_temp()` to MMIO into the sensor directly. It
> can't; the sensor has no memory map on this core. See §6.6.

**A bus is a shared resource, so composition implies an arbitration model (D06).** When two composed
devices (`bme280`, a second sensor) `needs` the same `i2c` controller, the controller is contended,
and the design makes that explicit rather than hoping handlers never overlap:

- **Transactions are exclusive.** A bus transaction (start→…→stop) is an indivisible unit; the
  controller serves one at a time. A second reaction's transaction does not interleave at the wire.
- **Waiting is bounded and queued.** A reaction needing a busy bus *yields* (§5.2) onto a
  **statically-bounded** per-bus queue; full queue ⇒ the declared overflow policy (§5.1, D02), not
  unbounded waiting.
- **Arbitration is deterministic.** Order of service is by reaction priority with a stable tie-break
  — same as the scheduler — so contention does not introduce nondeterminism.
- **Per-device speed/mode is type-checked.** Each device's required bus speed/mode is part of the
  interface's semantic contract (§4.1, D18); incompatible co-tenants on one bus are a compile error.
- **Bus faults and recovery are explicit.** Arbitration-lost, stuck-SDA/clock-stretch timeout, and
  the recovery sequence are declared fault codes (§4.4, D14) and a defined recovery op, not silent
  retries.

This is still the *same* keystone — a controller is just a device whose op surface several consumers
share — but the resource discipline is named, because "two drivers, one bus" is where naïve
composition models break in practice (§12).

### 3.6 A typed overlay / patch

Overlays are **typed structured edits** (`set`, `extend`, `remove`) over named entities — *not*
text or fragment merges. This is simultaneously the Devicetree-overlay replacement **and** the
agentic graph-edit surface. The two goals collapse into one mechanism.

```si
overlay tune_uart for board.nucleo_f401re {
  set    usart2.config.baud = 9_600
  extend usart2.needs { dma_tx = soc.dma1.stream6 }
  remove led_user
}
```

Every edit is **type-checked against the target's schema**: `set usart2.config.baud = 9_600` checks
that `baud` is a `config` field and that `9_600` satisfies its `where` constraint; `extend
usart2.needs { dma_tx = ... }` checks that `dma_tx` is a declared (or extendable) need with a
matching type; `remove led_user` checks the entity exists and that nothing still references it. A
malformed overlay fails to compile, the same way malformed code does — there is no "the merge
applied but the result is nonsense" failure mode that text-based Devicetree overlays have.

Because edits address **named paths** and never textual positions, an agent can emit them
deterministically, and they are the natural unit for a future content-addressed store (§9.5).

> **Status (implemented — compile-time `set`/`remove`).** `overlay <name> for board.<b> { … }` is
> parsed (`Item::Overlay`) and applied to the target board *before* it is built, so the existing §4.1
> config `where`-check validates the patched value. `set <inst>.config.<field> = <value>` checks the
> instance and config field exist and overrides the value (an out-of-range value fails its `where`
> constraint); `remove <name>` deletes an instance/pin binding and errors if it doesn't exist; an
> overlay targeting an unknown board is rejected. **Remaining:** `extend <inst>.needs { … }` is
> parsed-rejected (a noted follow-up), the `remove` dangling-reference check (nothing still uses it)
> is not yet enforced, and the agent overlay-edit *API* stays out of scope (Phase 2).

---

## 4. Type system

The type system is one of the two places foreclosure can happen, so it is designed to be
expressive at the *boundaries* (devices, faults, capabilities, time) even where v1 leaves the
interior simple.

### 4.1 Devices, interfaces, and capabilities

A `device` type is an **interface-with-behavior over a register-backed (or device-backed)
resource** — *not* a Devicetree node with a schema. Its four sections each contribute to checking:

- `regs` — the memory-mapped layout (leaf devices only). Types here are register/bit-field types
  (§4.2).
- `config` — typed fields with `where` constraints, checked at instantiation.
- `needs` — typed *relations* to other devices: `clock`, `reset`, `power_domain`, `irq`, a `bus`, a
  `dma` channel. These replace phandles. A `needs` is satisfied by a named reference whose type
  matches. **Clock/reset/power are first-class relations, not a flat `clock_source` scalar** (§4.5,
  D17): a peripheral commonly needs its clock *enabled*, its reset *deasserted*, and its power domain
  *up* before any op is legal — so these are devices/relations the compiler can order in generated
  startup (§6.4), not assumed-on globals. v1 *freezes the clock tree after init*; typed dynamic
  frequency changes are deferred-not-foreclosed (§10).
- `ops` — verbs, each optionally guarded by `when <state>` and typed for fallibility (§4.4),
  latency (`yields`, §5.2), and capability.

**`when`-state is static where provable, runtime-checked otherwise (D07).** When the compiler can
prove from straight-line control flow that a device is in the required state at a call site (e.g. a
`become ready` dominates the call), the `when` guard is a **static typestate** check with zero
runtime cost. Where the state cannot be statically established — across an event boundary, after a
`yields`, or through a dynamic reference — the guard lowers to a **runtime precondition** whose
violation is a Layer-3 fault (§5.4), not undefined behaviour. The design goal is to maximise the
statically-proven fraction; the runtime check is the sound fallback, never silently skipped.

> **Status (implemented — static half).** Devices declare `states { … }`; an op may be guarded
> `when <state>` and transitions with `become <state>`. The resolver tracks each device's provable
> state through a reaction's straight-line flow (reset at every event boundary, since typestate is
> not carried across one): a `when S` op call when a dominating `become S` has not run is a compile
> error; a `when`/`become` naming an undeclared state is rejected at the device. **Remaining:** the
> *runtime-precondition* lowering for the unprovable cases (across a `yields` / dynamic ref →
> Layer-3 fault) and the Layer-3 **site map** (per-call-site debug info so the decoder can name
> "handler X touched device Y outside its valid state") are follow-ups; today the unprovable case is
> conservatively a compile error rather than a runtime check, and op-internal transitions are read
> from the op's own top-level `become` (not through nested sub-op inlining).

An **interface** is the contract a device provides (`implements i2c`) or requires
(`needs bus: i2c`). Interfaces are how composition is typed: any device providing `i2c` can satisfy
any `needs bus: i2c` — a controller does not need to know about the sensors that will use it.

**Interfaces are nominal with structural conformance (D18).** Pure structural matching would let a
bus with the *same op shapes but different semantics* be silently accepted, so an interface is named
(`implements i2c` is a declared claim, not an accident of matching signatures) **and** carries
*semantic properties* the compiler and tools can check and version. For `i2c` those properties
include addressing mode (7- vs 10-bit), maximum bus speed, transaction atomicity (start→stop is one
indivisible unit), clock-stretching support, and bus-recovery behaviour. A device declares the
properties it requires; the controller declares what it provides; a mismatch (a 400 kHz-only sensor
on a 100 kHz-capped controller) is a **compile error**, not a runtime surprise.

> **Status (implemented).** An interface declares `property <name> [= default]`; a controller adds a
> `provides <iface> { <name> = <value> }` block; a device constrains a need with
> `needs { bus : i2c where <expr> }`. At board-bind the resolver const-evaluates the requirement
> against the provider's values (overlaid on interface defaults, reusing the §4.1 `where` evaluator),
> and a false result — or a reference to a property the provider doesn't declare — is a compile
> error. `std/i2c.si` now declares `max_speed`/`addressing`; `std/i2c_controller.si` provides
> `400_000`/`7`. **Remaining:** the richer property set (atomicity, clock-stretch, bus-recovery
> behaviour) is expressible but not yet declared on the std interface; property values are
> integer/bool constants only.

**Capabilities** are unforgeable typed values that gate access. A handler can only touch a device
it has been *granted* (passed a typed reference to). Floating-point requires an `fpu` capability the
board only provides if the SoC declares an FPU (§4.3). A secure-enclave boundary is "a core with a
capability boundary." Capabilities are the through-line that keeps the confidential-computing and
coprocessor deferrals open (§10): they are already in the type model, so adding a boundary later is
"introduce a new capability," not "retrofit the type system."

> **Threat model — what capabilities do and do not buy (D20).** Capabilities are a *source-level
> discipline*: inside safe Silica they prevent a handler from touching a device it was not granted,
> the way a borrow checker prevents aliasing. On bare metal with no MPU/TrustZone/MMU, a compiled
> capability is **not** a hardware isolation boundary — `raw` (§4.2), an FFI edge (§10), or a
> hardware fault can step outside it. The honest claim is: *capabilities prevent accidental misuse
> within well-typed Silica; real security isolation requires hardware support (MPU regions,
> TrustZone, an enclave) that a capability can be made to **drive** but does not itself provide.*
> Keeping capabilities clean in the type model is what lets such a hardware boundary later be
> *attached* to a capability rather than retrofitted.

**How `on`/`every` stay primitive while devices stay un-privileged.** The compiler core knows the
*binding/trigger* concepts `on` and `every`. It does **not** know what a UART or an NVIC is. A
device declares `emits <name> : event`; `on usart2.rx_ready { ... }` binds a handler to that event
source. The compiler resolves the binding to a concrete IRQ by following the device's `needs irq`
relation into the (ordinary, std-lib) interrupt-controller device, and generates the vector-table
entry. `every` is implemented over an ordinary timer device the same way. **The primitives are
control-flow constructs; the devices remain equal citizens.** Nothing about `gpio`, `uart`, or the
NVIC is special to the compiler.

### 4.2 Registers and bit-fields

Bit and register fields are **first-class**, not bit-twiddling on a `u32`:

- Named single bits: `SR.busy`.
- Named multi-bit ranges: `CR.mode[2:0]` (declared `mode: field[2:0]`).
- Enums mapped to field values: declare `mode: field[2:0] as Mode` then write `CR.mode = fast`.
- Single-op read-modify-write over several fields: `CR1{ enable = 1, rxneie = 1 }`.
- Raw bitwise ops still available when you want them: `CR1.raw |= 0x2000`.

The register *type* carries the access semantics: **every access is volatile and correctly
ordered, automatically.** Ordering between accesses to the *same* peripheral is preserved; ordering
across peripherals uses the minimal barrier the target requires. The programmer never reasons about
`volatile` or fences — that is exactly the kind of hidden, easy-to-get-wrong detail the language
removes. A `raw` escape hatch (§3.5 mention; here at the field level `CR1.raw`) exists for the
exotic ~5%, and it is *opt-in and visible* — you can grep for `.raw`.

**"Volatile and ordered" is necessary but not sufficient — registers need a real access model
(D04).** Hardware registers are not just memory that must not be cached; their *semantics* differ
per field, and getting them wrong silently corrupts state. So a `reg`/`field` declaration carries
explicit access qualifiers, and the compiler enforces them:

| Qualifier | Meaning | What the compiler does |
| --- | --- | --- |
| `ro` / `wo` / `rw` | read-only / write-only / read-write | rejects an illegal direction at compile time |
| `w1c` | write-1-to-clear | a "clear" lowers to writing `1` to that bit, **never** read-modify-write |
| `rc` | read-to-clear / read-has-side-effects | the read is treated as an effect; never elided, reordered, or duplicated |
| `side_effect pop_on_read` | reading a data/FIFO register **consumes** data | a destructive read; the simulator and debug "watch" views must not peek it (watching a FIFO would drain it) — see the `DR` register in §3.2 |
| `reserved` | reserved bits | preserved across any read-modify-write; never written with arbitrary values |
| `reset = <v>` | power-on reset value | known statically; feeds the generated startup and the simulator (§7.1) |
| `width = 8\|16\|32` | required access width | byte/half/word access enforced; no illegal narrowing/widening of the bus access |

Qualifiers attach at the **register or the field** level (`SR : reg32 ... access ro { ... }`, or a
per-field `txe: bit[7] access ro`), so a status register that mixes read-only flags with a `w1c` bit
is described exactly. This matters most exactly where the simple "RMW everything" model is *wrong*: writing a multi-field
update to a register that contains a `w1c` status bit would inadvertently clear it; an `rc` data
register read must not be duplicated by the optimizer; reserved bits must survive. The model also
states ordering obligations the bare "volatile" claim glosses: a **barrier is required** before
enabling an interrupt source and around DMA buffer hand-off (the store that arms DMA must not be
reordered before the buffer is written). The **C backend must not emit C bitfields** for any of this
(their layout is implementation-defined); register access lowers to explicit masked loads/stores on
fixed-width volatile pointers (§6.2, D09).

### 4.3 The number / data model

Fixed-width integers are first-class and width is **always explicit**: `u8 u16 u32 u64` and signed
`s8 s16 s32 s64`. **There is no `int`** and no pointer-width default; a register field can also have
an odd width like `u7` or `u24`.

| Concern | Rule |
| --- | --- |
| Overflow | **Traps by default** (it is a fault — see the flagged inconsistency below). Wrapping is a *distinct* operator `+%`, `-%`, `*%`. Saturating is a third: `+|`, `-|`, `*|`. |
| Widening | Implicit only when **lossless** (`u8 → u16`). |
| Narrowing | **Never implicit.** Use an explicit, fallible or truncating cast. |
| Mixed sign | **No** implicit signed/unsigned mixing. |
| Booleans | A distinct type, **not** an integer. |
| Bytes | A thin, **bounded** `buffer<N>` / `bytes` type tied to the pool/arena model (§5.3). |
| Text | Minimal byte-strings only. **No Unicode / text machinery** on device. |
| Endianness | **Explicit** at the byte/buffer boundary: `u32.le`, `u32.be` when (de)serializing. |

> **Status (implemented — conversions).** The Widening / Narrowing / Mixed-sign rules are enforced.
> The resolver runs a `value_type` pass over each statement: a declared-typed value assigned to a
> narrower type, or signed/unsigned operands mixed in one operation, is a compile error; an
> out-of-range integer literal for its target type is rejected. The single escape hatch is the
> explicit cast `<expr> as <type>` (`AST::Cast` → `SirExpr::Cast`), which the sim truncates to the
> target width and the C backend emits as a fixed-width C cast. Integer *literals* and device-op /
> register results stay width-flexible so ordinary code needs no annotation. **Remaining:** `.le`/`.be`
> endianness, odd-width fields (`u7`/`u24`) in expressions, and a *checked* (fallible) narrowing cast
> are not yet built — only the truncating `as`. (Overflow trap-by-default is tracked separately as B1.)

> **Flagged inconsistency in the source decisions.** The settled list says overflow "traps by
> default" but parenthesizes "(fault in debug/sim)", which quietly implies *no* trap in
> release/on-metal — re-introducing exactly the silent-wraparound footgun the design exists to
> kill. **Recommendation: trap by default everywhere, including release.** On metal a trap is just
> the Layer-3 fault path (§5.4); it is not free, but neither is a silent wrong answer in a motor
> controller. Provide an explicit, *visible* opt-out at two grains: `+%`/`+|` operators for a single
> wrap/saturate site, and a **scoped directive `@overflow(saturate | wrap | trap)`** on a block or op
> that sets the default arithmetic mode inside it (Gemini SIL-004). The directive earns its place
> because the *correct* behaviour for a real-time control loop is usually **saturation**, not a trap
> (clamping a PID term beats faulting a live motor controller) and not silent wrap — and writing `+|`
> on every line of the loop is exactly the kind of noise an agent gets wrong. `@overflow(saturate)`
> makes "this whole loop clamps" one greppable declaration; **trap stays the global default**
> everywhere it is not explicitly overridden. The default must be safe; the opt-out must be loud.
> (This is a place where the embedded goal and the agentic goal agree: an agent reasons far better
> about a language whose `+` means one thing within a clearly-marked scope.)
>
> **Status (implemented).** Trap-by-default is now the resolved behaviour, sim *and* metal
> (SIL-004 inconsistency closed — no "silent in release" carve-out). Plain `+`/`-`/`*` lower to
> width-checked ops at the assignment-target type: in the sim an overflow drives the system to its
> safe state (`OVERFLOW TRAP` trace); on metal the generated `__si_*` helper uses
> `__builtin_*_overflow` and calls `__silica_overflow_trap` → `__drive_safe()` + halt. The explicit
> `+%`/`-%`/`*%` (wrap) and `+|`/`-|`/`*|` (saturate) operators lex/parse/lower end-to-end. Proven on
> nRF52840 in Renode by `harness/overflow_trap.sh` (a u8 counter halts at the overflow tick under
> `+`, keeps running under `+%`). **Remaining:** the scoped `@overflow(saturate | wrap | trap)`
> block directive (needs attribute syntax) — the per-operator opt-out covers the same ground for now.

**Fixed-point is first-class.** The binary point is in the type: `fixed<16,16>` is 16 integer bits
and 16 fractional bits. The compiler handles scaling on multiply/add (a `fixed<16,16>` multiply
computes in a wider intermediate then rescales; the rescale obeys the same overflow rule). This is
the default way to do fractional math on the many parts without an FPU — and it needs no FPU
because it is integer math underneath.

**Float is not in the core.** It is opt-in and allowed *only* if the target SoC type **declares an
FPU** (an `fpu` capability, §4.1). Using `float` on an FPU-less part is a **compile error**, not a
silent soft-float fallback. In the toy we *refuse* rather than emit slow soft-float — but this is
**not foreclosed**: soft-float can later be a std-lib-provided capability that satisfies the same
`fpu` requirement.

> **Status (implemented — FPU gate).** A SoC declares the capability with an `fpu` line in its `soc`
> block (`SocDef.fpu`). `float`/`f32`/`f64`/`double` resolve to `SirType::F32`/`F64`, and a `float`
> cell or `let` on a board whose SoC does *not* declare `fpu` is a compile error (no silent
> soft-float). On an FPU board it is allowed and lowers to a C `float`/`double`. **Remaining:** the
> broader capability system — unforgeable typed device grants and the "a handler touches only granted
> devices" check — is not yet built (the FPU capability is the first, concrete instance); float
> *arithmetic* at runtime (sim ops, float literals) is a follow-up, so today a `float` value is
> carried/stored but not computed on.

### 4.4 Fallibility and faults

Three distinct layers, kept distinct.

**Layer 1 — expected operational failures (NAK, timeout, out-of-range): fallibility in the type.**
A fallible op returns `T or fault`. You cannot obtain the `T` without discharging the fault path —
ignoring it is a **compile error**. This kills the errno / unchecked-return-code footgun at the
type level. Discharge is by pattern match or by the propagation operator `?`, which forwards the
fault outward:

```si
op read_temp() when ready -> fixed<16,16> or fault yields {
  let raw = bus.read_reg24(addr, REG_TEMP)?   // on fault, return it from read_temp
  return compensate(raw)
}
```

**Faults are opaque.** There is **one** `fault` type with a queryable `code` inside, not a typed
error zoo:

```si
match usart2.write(b) {
  ok                -> { }
  fault f if f.code == timeout -> retry_later()
  fault f           -> escalate(f)
}
```

This is chosen for *learnability* and *agentic regularity* over fine-grained expressiveness: an
agent (and a human) only ever handles one error shape. The cost — you cannot get the compiler to
prove you handled every distinct error *variant* — is accepted deliberately.

**But each op must *declare* its possible fault codes (D14).** The runtime type stays the single
`fault`, yet an op's signature lists the codes it can raise — `op read_reg(...) -> u8 or
fault{nak, timeout, arblost}` — so the set is *known statically* even though the value is opaque at
runtime. This buys back what matters without a typed-error zoo: tooling and the agent see exactly
which failures to expect; the simulator can fault-inject the right set (§7.1); and a fault
disposition or `match` can be checked for **completeness against the declared codes** and flagged
when it ignores one a device documents (e.g. silently dropping `crc` on a sensor read). Recovery in
embedded systems routinely depends on *which* failure occurred — `timeout` vs `nak` vs `exhausted`
vs `overflow` vs a `when`-precondition violation — so the codes are first-class to docs, tools, and
disposition validation, while the value stays one regular shape to handle.

> **Status (implemented — `match` + totality).** The `match` statement is built as the surface
> conditional (`match <expr> { <lit> => …, _ => … }`, lowered to a guarded if-chain over existing
> `SirStmt::If`). Matching is enforced **total**: a `_` wildcard arm is required (else a compile
> error) and duplicate literal arms are rejected — the "completeness, no silent fall-through"
> discipline D14 asks for, applied to value matching. **Remaining:** patterns are integer/bool
> literals only; `ok`/`fault f` op-result patterns and exhaustiveness against an op's *declared fault
> codes* (the `match usart2.write(b)` form above) build on this and are not yet wired.

**Layer 2 — propagation through the reactive model: fault disposition.** Fallibility composes
*within* a handler (via `?`), but a handler has **no caller to unwind to** — it was invoked by an
event, not a function call. So each reaction declares a **fault disposition**, the reactive-model
equivalent of a catch block, attached to the *event source* rather than a stack frame:

```si
on sensor.sample_ready on fault retry(max = 3) {   // disposition: retry up to 3×
  let t = bme280.read_temp()?
  log_sample(t)
}

every 1s on fault skip {                            // disposition: drop this tick, keep scheduling
  housekeeping()?
}
```

Dispositions are a small, fixed set with sane defaults: **`retry`**, **`skip`** (drop this event,
keep running), **`safe`** (drive devices to safe state — §5.6), **`escalate`** (raise to the
Layer-3 handler). The default if unstated is conservative (`escalate`) so that an unhandled fault is
never silently dropped.

**Layer 3 — hardware faults (HardFault / bus fault / mem fault, and `when`-precondition
violations).** A **language-level fault decoder** maps a hardware trap back to language-level truth
— "handler *X* touched device *Y* outside its valid `when` state," or "MMIO to an address no device
claims" — using the same graph-aware information that debug info carries (§7.2). See §5.4 for how the
decoder is built and §5.6 for safe-state. The decoder is possible *because* states are explicit and
every address range has a declared owning device.

### 4.5 Time as a type

`duration` and `instant` are **distinct types**, so unit errors are type errors:

```si
let deadline : instant  = now() + 500ms   // ok: instant + duration -> instant
let bad                 = now() + 5       // compile error: instant + (untyped) int
let elapsed : duration  = now() - start   // ok: instant - instant -> duration
```

A `duration` is represented as a count of ticks in a **known monotonic tick domain** whose rate is
*derived from the board's clock topology* — i.e. the typed hardware model already knows the timer
clock, so `500ms` lowers to an exact tick count for *this* board. `instant` is a reading of the
monotonic clock; arithmetic is defined so only sensible combinations type-check.

> **Status (implemented).** `instant`/`duration` are distinct `SirType`s (both `uint64_t` ns at
> runtime) and `now()` reads the clock — the sim's virtual time, a host `clock_gettime` monotonic
> read, or a SysTick-driven uptime counter on metal. The resolver's `time_kind` pass enforces the
> rules above: `instant - instant → duration`, `instant ± duration → instant`, and rejects
> `instant + instant`, `now() + <bare int>`, scaling an instant, comparing an instant to a
> non-instant, and assigning an instant to a non-instant cell. A duration literal (`500ms`) is kept
> type-distinct from a bare integer (`5`) via a dedicated `DurationLit` AST node, so the canonical
> `now() + 500ms` (ok) vs `now() + 5` (error) distinction holds. **Remaining:** the exact-or-error
> tick-rate conversion and `rounded` modes below (D15) are not yet enforced; metal `now()` is at
> 1 ms (SysTick base) resolution.

**Depth.** v1 ships *unit-safety* (the above). The representation is chosen so that **deadline /
WCET annotations attach later without redesign**: a reaction may already be written `on x within
2ms { ... }` (§3.4), which today is a runtime-checked bound and tomorrow can feed a static
schedulability analysis. The run-to-completion-between-yields model (§5.2) keeps this amenable: the
unit of timing analysis is a *handler segment between yields*, each of which is straight-line +
bounded-loop code (§9.2). Time-as-a-type is therefore not just ergonomics; it is the on-ramp to
WCET reasoning, and it is foreclosure-checked against it.

**Conversion and jitter are specified, not hand-waved (D15).** Lowering `500ms` to ticks has rules:
the value is converted at compile time against the board's tick rate; **conversion must be exact, or
it is a compile error** unless an explicit rounding mode is given (`500ms rounded nearest`), so a
period the hardware cannot represent fails loudly rather than silently drifting. A periodic `every`
is **fixed-rate** (the next deadline is computed from the *scheduled* time, not from handler
completion), so handler execution time does not accumulate as drift. If a periodic handler **overruns**
its own period, the default is **coalesce** (the missed tick is dropped, not queued — a backlog of
stale ticks is worse than skipping), and the overrun is observable via the trace ring (§7.1, D13);
`every ... on fault` and a future `within` deadline (§9.2) let a program choose otherwise. Clock
*drift* (crystal tolerance, temperature) is a physical fact the type system does not pretend to
remove (§1, D12); it surfaces as the bound on `instant` accuracy, not as a guarantee.

### 4.6 Typed literals

Literals carry units and are checked at use: `4K`/`512K`/`64M` (sizes), `115_200`/`16MHz` (rates),
`level-high`/`falling` (signal polarities/edges), `3v3`/`1v8` (voltages), `500ms`/`2us`
(durations). They replace the C preprocessor's stringly-typed constants. `16MHz` is a
`clock_source`-compatible frequency; assigning it where a `duration` is expected is a type error.

### 4.7 Compile-time evaluation

A bounded `comptime` sublanguage (§9.4) computes values at compile time: register divisors
(`comptime clock.hz / baud`), lookup tables (sine/gamma/CRC LUTs as `comptime` array initializers),
computed addresses, and **pool sizes**. It is *total and bounded* — bounded loops and recursion, no
unbounded computation — which is precisely what keeps the memory model statically sized (a pool's
size must be a `comptime` value). The same evaluator produces the linker script, vector table, and
`.data`/`.bss` layout from the board type (§6.4).

---

## 5. Execution model

### 5.1 Reactive scheduling

The whole concurrency model is `on <event>` and `every <duration>`. There are no threads, tasks,
semaphores, or mutexes in the surface language. A **reaction** is bound to an event source (a device
`emits`, or a timer for `every`). The runtime is an event-driven scheduler: when an event fires, its
reaction(s) run. Priorities derive from the event source (an IRQ priority; a timer tick), and the
compiler knows the full static set of reactions and the cells each touches.

**Scheduler contract (D02).** "Event-driven" is not a specification, so the model fixes the
following, all statically sized to keep RAM a compile-time constant (§5.3):

- **Bounded queues.** Each event source has a statically-sized pending capacity (often 1). Capacity
  is part of the program's RAM budget, not a dynamic queue.
- **Overflow is explicit.** When an event arrives and its pending slot is full, the policy is
  declared per source — **coalesce** (collapse to one pending — the default for level/periodic
  sources), **drop-newest**, or **fault** (raise to the Layer-3/overflow handler). There is no
  silent unbounded growth and no silent loss.

  > **Status (implemented).** A reaction declares its policy with an `on overflow <coalesce |
  > drop_newest | fault>` clause (`SirReaction.overflow`, default `Coalesce`). When an event
  > re-fires while an activation is in flight, the sim's `fire()` applies it: coalesce (the prior
  > behaviour) collapses the re-fire; drop-newest discards it (distinct trace); `fault` drives the
  > device to its safe state and stops — a system-integrity fault. On metal the yielding-reaction
  > trigger entry branches the same way (coalesce/drop → return; `fault` → `__drive_safe()` + halt).
  > **Remaining:** a pending capacity > 1 (today the slot is exactly one, the common case) and a
  > per-event-*source* declaration (vs per-reaction) are follow-ups; multi-consumer bus arbitration /
  > a bounded per-bus wait queue beyond the §5.2 single-yield model is not yet built.
- **Single live activation per reaction.** A reaction has at most one in-flight activation. If its
  event re-fires while it is running *or yielded*, the re-fire follows the overflow policy above
  (default: coalesce). Reactions are **not** re-entrant; there is no stack of suspended activations
  of the same handler.
- **Deterministic order.** When several reactions are runnable at once, they run in a deterministic
  order: by source priority, ties broken by a stable compile-time order. Same inputs ⇒ same order,
  on metal and in sim (§7.1, D19).
- **Missed timer ticks** follow the `every` overrun rule (§4.5, D15): coalesced by default, observable
  via trace (§7.1).

### 5.2 Run-to-completion vs. suspension — *run-to-completion between yields*

This is the central execution decision, and the keystone (§3.5) forces it. The options:

- **Strict run-to-completion (RTC).** Each handler runs to its end before any other runs. Simple,
  no per-handler stack, trivially analyzable — but a handler that needs a slow bus transaction must
  **busy-block**, starving everything else. With device composition over I²C/SPI this is untenable.
- **Suspendable handlers (`await`-style).** A handler may suspend at explicit points and resume
  later, requiring a **compiler state-machine transform** (à la Embassy in Rust): each handler
  becomes a state machine whose locals across a suspension point are captured in a statically-sized
  frame. More compiler work; introduces reentrancy concerns (§5.5).

**Recommendation: run-to-completion *between explicit yield points*.** A handler never blocks the
scheduler; it either completes or **yields**. Yields are explicit and typed — an op that suspends is
marked `yields` in its signature (`op read_temp() ... yields`), so suspension is visible in the type
exactly like fallibility is. Between two yields, a handler is strict RTC (straight-line + bounded
loops). This:

- makes device composition over slow buses natural (the bus op yields; other reactions run);
- keeps each *segment* analyzable for WCET (§4.5, §9.2);
- preserves the "no hidden state" promise — every suspension is spelled `yields`/`await`, never
  implicit.

The cost is the state-machine lowering (§6) and the reentrancy it creates: while one handler is
yielded, another can run and touch shared `cells` (§5.5). That cost is paid once in the compiler and
the atomicity construct, and it buys the entire composed-device and future-networking story.

### 5.3 Memory & allocation — bounded, not absent

No general heap. Memory comes in **statically-sized** forms the compiler sums at build time:

- `pool<T, N>` — N slots of `T`; allocation returns `handle or fault` (`fault` = exhausted).
- `arena` — a region you carve bounded sub-allocations from with a reset point.
- `ring<T, N>` — a bounded ring buffer (the canonical producer/consumer between an `on` and an
  `every` handler).
- `buffer<N>` / `bytes` — bounded byte storage for DMA and protocol framing.

> **Status (implemented — `ring<T, N>`).** The canonical producer/consumer queue is built: a
> `ring<T, N>` cell with `push`/`pop`/`len`/`is_empty`/`is_full`, modelled in the sim and lowered on
> metal to a backing array + head/tail/count indices, all summed into the static RAM budget
> (`ring<u32,16>` → 76 B; verified by `c::ram_budget`). On a full ring `push` overwrites the oldest
> (a defined, bounded overflow policy); cross-reaction sharing is priority-ceiling protected by the
> existing §5.5 auto-critical (ring ops register as cell touches). **Remaining:** `pool<T,N>`,
> `arena`, and `buffer<N>`/`bytes` are not yet built (ring proves the bounded-container + static-
> accounting pattern); `T` must be an integer scalar; a fault-on-full/empty variant (vs
> overwrite-oldest) is a follow-up.

Handler frames for suspendable handlers (§5.2) are also statically sized and counted. The result:
**total RAM use is a compile-time constant** — but that claim is only true if the *stack* is bounded
too, so the model bounds it explicitly (D08):

- **Recursion is banned by default** (it is the one easy way to make stack depth unknowable); a
  bounded, annotated form may be allowed later, but the default keeps depth statically computable.
- **Local storage is bounded** — no variable-length arrays or unbounded locals; large buffers live
  in pools/arenas (above), not on the stack.
- **ISR nesting is accounted.** Because priorities are static (§5.5), the worst-case interrupt
  nesting depth is computable, and the stack budget includes it.
- **Suspended-handler frames are counted** as static allocations (one per reaction, sized to its
  largest live set across a yield), not as live stack.
- **Backend-generated frames count.** The C/LLVM lowering must not introduce dynamic stack
  (`alloca`, large temporaries); the SIR→backend contract (§6) bounds call-frame size so the summed
  stack high-water-mark is itself a compile-time number.

So the honest claim is: **statics + pools + handler frames + a bounded worst-case stack** are summed
at build time. Stack overflow is then a *budget* failure caught at link time, plus an MPU guard-page
on parts that have one — not a runtime mystery.

> **Status (implemented).** The RAM-budget gate's stack term is now **computed from the program**, not
> a flat 2048-byte stub: `worst_case_stack` sizes each reaction's frame from its SIR frame-locals
> (× a word + a fixed per-frame overhead) and sums, over distinct static priority levels, the largest
> frame at each level (+ a Cortex-M exception frame per level) plus a base context — the worst-case
> ISR nest, since a reaction cannot preempt one at its own level (non-reentrant, run-to-completion).
> **Recursion is banned**: the resolver rejects an op that re-enters itself on the active inline path
> (`§5.3/SIL-005`), which also keeps the inliner — and the bound — finite. **Remaining:** the estimate
> is a sound *over-approximation* (it uses conservative fixed frame overheads and counts a yielding
> reaction's static `__rf` temps as if on-stack) rather than the toolchain's exact `-fstack-usage`
> numbers; and the **frame-union** optimisation (overlapping disjoint-lifetime frames) is not yet
> applied — both can only make the summed budget *smaller*, never unknowable.

**Frame *union* keeps the static cost affordable (Gemini SIL-005).** Allocating a separate frame for
every async handler would exhaust RAM on an 8–32 KB part, so the compiler does the opposite of
wasteful: from the static call-graph and priority map it computes which handler frames have
**disjoint lifetimes** — handlers that can never be live at the same time (a one-shot `boot` sequence
vs. the steady-state loop; two handlers at the same single-threaded priority that cannot preempt each
other) — and **overlaps their frames in a shared union** in the generated layout. This is the static
analogue of stack reuse: predictable *and* compact. It is an optimization over the accounting above,
not a change to it — the summed budget can only get smaller, never unknowable.

### 5.4 The three-layer fault model (execution view)

§4.4 defined the layers in type terms; here is how they execute:

- **Layer 1** discharges or propagates within a handler via `?`/`match`.
- **Layer 2** catches at the reaction boundary via the declared **disposition** (`retry`/`skip`/
  `safe`/`escalate`).
- **Layer 3** is the hardware trap path. The compiler emits, alongside debug info, two tables: an
  **address-ownership map** (which device claims each MMIO range; flash/RAM regions) and a
  **site map** (each code site → its enclosing handler, the device/op it touches, and the `when`
  state expected there). On a HardFault/bus/mem fault, the decoder reads the faulting address and PC
  and produces a language-level diagnosis: *"handler `pump_ctrl` wrote `valve.regs.CR` while valve
  was in state `closed`, which forbids it,"* or *"store to 0x4002_0000 — no device claims this
  address."* This is the same graph-aware information the agent uses to debug (§7.2), reused at fault
  time.

### 5.5 Atomicity / interrupt-safety as a language construct

State shared between an interrupt-driven `on` handler and an `every` handler — or between any two
reactions, now including a yielded handler and the reaction that runs while it is suspended — needs
protection. Silica makes this a **language construct, not manual `disable_irq`**.

Shared mutable state is declared as a `cell` (§3.4). The compiler builds the static **reaction↔cell
access graph**: which reactions read/write which cells, and at what priority each reaction runs. From
this it computes, per cell, the minimal critical section using a **priority-ceiling protocol**
(the same idea RTIC uses): access to a cell raises priority to the ceiling of all reactions touching
it, for the shortest possible span, masking exactly the interrupts that could race and no others.

```si
cell counter : u32 = 0

on tick.elapsed     { counter += 1 }        // compiler: this access needs the ceiling section
every 1s            { let c = counter; counter = 0; report(c) }
```

The programmer writes neither locks nor interrupt masks. Because the access set is fully static,
the analysis is exact (no over-broad "disable all interrupts" hammer, no missed race). A cell only
touched by one reaction needs **no** critical section at all, and the compiler proves it. This is the
reactive-model-native answer to the classic shared-state-with-an-ISR problem, and it is *more*
necessary under suspension (§5.2), which is why Open Questions 1 and 3 are linked.

**Yield-aware ownership rules (D03).** Suspension makes the priority-ceiling section necessary but
*not sufficient*: a critical section that spans a `yields` would stall the scheduler, defeating the
whole point. So:

- **A cell borrow cannot cross a yield.** The ceiling section is confined to a single
  RTC segment (§5.2); the compiler rejects code that holds a cell reference across an `await`. Read
  the cell, yield, re-read after resume — the intervening writer is visible, by design.
- **Multi-statement atomic updates use an explicit construct** — `atomic { ... }` over one or more
  cells — which is checked to be yield-free and bounded, so "read-modify-write a pair of cells
  together" has one spelling instead of accidental tearing.
- **DMA-shared buffers are not cells.** A buffer handed to DMA needs *typed ownership transfer*
  (the handler gives up access until completion) plus cache-coherency/barrier handling (§4.2, D04),
  not a priority-ceiling section — masking interrupts does not stop a DMA engine.
- **NMI and hard-fault contexts are outside cell protection.** Priority ceiling masks maskable
  interrupts; an NMI or the Layer-3 fault path (§5.4) can run regardless, so state they share must
  be lock-free/single-word and is called out as such.

### 5.6 Safe state

Each device **declares its own safe state** (`safe_state = off` for a motor, `= open` for a relief
valve, `= sleep` for a sensor). On an unrecovered fault, the Layer-3 handler can **drive all devices
to their safe states before deciding what to do next**. The post-safe-state policy is **declarable**:
`panic-and-reset` vs. `transition-to-safe-state-and-hold`, per program and overridable per device.
Safe-state is a first-class part of a device type precisely because "what is safe" is device
knowledge (a motor off is safe; a valve *open* may be the safe one), not something a generic fault
handler can infer.

**Safe ops run in a degraded world, so they are constrained (D05).** The fault path may face a wedged
bus, a clock that is off, or already-corrupt RAM — so "drive everything to safe state" cannot rely on
the normal machinery. A `safe` op is therefore required to be **bounded** (a hard time/step cap),
**idempotent** (running it twice is harmless), **non-allocating**, and **preferably non-yielding**
(it should not depend on the scheduler it may be escaping). A safe op **may itself fail**, and the
device declares the fallback when it does (e.g. assert a hardware fail-safe line, or fall through to
reset). Crucially, software safe-state is the *second* line of defence: the design assumes
**hardware fail-safe** (pull-downs/biasing so the de-energized state is safe) and an independent
**watchdog** that forces a reset if the fault path itself hangs. Silica models and sequences the
software part; it does not pretend software alone makes a system safe.

**The hardware watchdog is a first-class part of the runtime (Gemini SIL-006).** A software-only
Layer-3 decoder cannot recover a CPU stuck in an interrupt storm, a livelock, or a wedged bus — so
the watchdog is not left to the programmer to remember. A board declares one as an ordinary device
(`watchdog : wdt at … { config { timeout = 100ms } }`, §3.3), and the **scheduler owns feeding it**:
the generated event loop emits the feed only on a clean return to the idle/dispatch point. The
consequence is deliberate — **a reaction that overruns its declared budget (`within …`, §4.5) starves
the watchdog and triggers a hardware master reset** rather than hanging silently. The feed is never
sprinkled through user code (that is how watchdogs get defeated — fed by the very loop that is stuck);
it is a property of the scheduler, like the critical sections of §5.5. v1 wires a single
system watchdog; windowed/multi-stage watchdogs are deferred-not-foreclosed (§10).

> **Status (implemented — `within` on metal).** Beyond the watchdog catching a handler that *never*
> returns to idle, a per-reaction `within <d>` deadline is now enforced on metal for yielding
> reactions when a watchdog is declared: the backend emits a `__deadline_N` countdown (in 1ms SysTick
> ticks) armed at the trigger entry, decremented by SysTick, and disarmed when the frame returns to
> idle; an overrun latches `__deadline_missed`, which gates off the idle-loop watchdog feed → reset.
> This catches a handler that is merely *too slow* (would eventually complete), a tighter bound than
> "never idle". Proven on nRF52840 in Renode (`harness/deadline_reset.sh`: a `within 30ms` read over
> a ~50ms bus latches the flag; `within 80ms` does not). **Remaining:** it requires a declared
> watchdog (the reset path); non-yielding reactions are bounded by ISR run-to-completion (no
> mid-handler check); resolution is the 1ms SysTick base tick.

---

## 6. Compilation & backend

### 6.1 The IR boundary

The compiler emits a narrow **intermediate form, Silica IR (SIR)**, and a *backend consumes it*.
This boundary exists from day one so that the C→LLVM transition is "swap one consumer," not "rewrite
the compiler." SIR is deliberately **below** any source-level sugar and **above** any target detail:

- handlers lowered to **explicit state machines** (suspension points resolved, frames sized);
- register accesses as **typed volatile loads/stores with explicit ordering** (no `volatile`
  keyword — it is a property of the op);
- the **device graph resolved** to concrete addresses; the **schedule and vector table** computed;
- the **memory layout** (pools, arenas, statics, frames) as concrete sizes/sections;
- faults as explicit control edges; `comptime` values already folded.

SIR is the contract. Everything below is "just" a printer/lowering from SIR.

### 6.2 C backend (first)

The first backend emits C — the fast path to real hardware and to design feedback. Each reaction
becomes a C function; the state-machine transform becomes a `switch` on an explicit state variable;
register accesses become `volatile` pointer writes with the required barriers; pools become static
arrays; the scheduler becomes a generated event loop plus NVIC configuration. The C is an
*implementation detail of a backend*, not a HAL the language sits on — there is no hand-written C
driver layer underneath Silica devices.

> **Risk flagged (see §12): the C backend's "purity" can be cosmetic.** If SIR quietly encodes
> C-isms (host `int`, libc assumptions, UNIX-y I/O), then "emit C first, LLVM later" becomes a trap
> rather than a stepping stone. **Guard:** SIR must be expressible in **LLVM with no libc and no C
> runtime**; C is merely one printer of SIR. Concretely, SIR uses only fixed-width types, explicit
> memory ops, and explicit control flow — nothing that *needs* a C semantic to be meaningful. The
> LLVM path (below) is the proof obligation that keeps the C backend honest.

The flip side of "C is just a printer" is that the printer must **dodge C's undefined behaviour**,
or the language's guarantees (checked overflow, fixed widths, ordered MMIO) leak away in codegen
(D09). The backend emits a strict freestanding subset:

- **Fixed-width types only** (`uint32_t` &c.) — never `int`/`long`; no reliance on host word size or
  integer-promotion rules.
- **Explicit checked arithmetic** — overflow checks are emitted as explicit compares; the backend
  never *relies on* signed-overflow wraparound (which is UB) and never leaves a trap-on-overflow op
  as a bare `+`.
- **No C bitfields** — register and bit-field access lowers to explicit mask/shift on volatile
  fixed-width pointers (§4.2, D04), because C bitfield layout and access width are
  implementation-defined.
- **Explicit barriers** — compiler barriers and hardware fences are emitted where the ordering model
  (§4.2) requires them, rather than trusting `volatile` alone (which orders volatile-vs-volatile but
  not volatile-vs-ordinary, and implies no hardware fence).
- **No libc, no dynamic initialization, no hidden runtime** — startup is generated (§6.4); there is
  no `__attribute__((constructor))`, no `malloc`, no static initializers that run before the
  generated reset path.

Each of these is also exactly what the LLVM backend (§6.3) does natively, which is why holding the C
backend to them keeps SIR honest instead of letting C-isms calcify.

### 6.3 LLVM path (then)

The LLVM backend is what makes "replace Zephyr" structurally real: full control of startup, no libc,
custom section placement, and direct lowering of SIR's typed memory ops to LLVM IR. Because SIR is
already target-neutral and below source sugar, the LLVM backend is a second *consumer* of the same
IR, validating §6.2's guard. Nothing in the language design above assumes C semantics; this is
checked in §10's foreclosure audit (LLVM, FFI, multicore all remain reachable).

### 6.4 Generated linker script, vector table, startup, `.data`/`.bss`

The typed hardware model already knows the memory map (flash/RAM origin+size), the IRQ table (from
`needs irq` relations and the interrupt-controller device), and the full static memory budget (§5.3).
Therefore these artifacts are **generated, not hand-written**:

- **Linker script** from `board.soc.memory` regions + computed section sizes.
- **Vector table** from the reset vector + the resolved `on <irq>` bindings.
- **Reset/startup** — set SP, copy `.data` from flash to RAM, zero `.bss`, run device init ops in
  dependency order, enter the scheduler.
- **`.data`/`.bss` init** from the typed statics and pool declarations.

All of these are `comptime` computations (§4.7) over the board type. Hand-editing a linker script is
not a supported workflow; changing the memory map means changing the board type, which re-derives
everything consistently.

### 6.5 Executable register access → MMIO lowering

A leaf device's `regs` lower to MMIO: a field write becomes read-modify-write of the owning register
at `base + offset`, as a volatile access with the register type's ordering. `CR1{ enable = 1,
rxneie = 1 }` lowers to a single load, mask/set, store. Multi-field writes coalesce; single-field
writes to a write-1-to-clear register lower correctly because the *register type* declares its
access semantics. The base address comes from the instance (`usart2 : uart at 0x4000_4400`), so the
same `uart` device type is reused at every instance address with no per-instance code.

### 6.6 Composed-device lowering

A composed op lowers to a **sequence of calls to the substrate device's ops**, which themselves
lower (recursively) until a leaf reaches MMIO. `bme280.read_temp()` → `i2c.read_reg24(...)` →
(controller leaf) register sequence → MMIO, with the `yields` points becoming state-machine
suspension/resume edges in SIR. This is the lowering-level statement of the §3.5 keystone and the
§3.5 caveat: composition is real and zero-HAL, but it is *transactions lowered to MMIO*, not direct
MMIO from the sensor.

---

## 7. Tooling

### 7.1 Host simulation as a first-class mode (incl. macOS & Windows)

Because a device is just `regs` + `ops`, on the host it is a **mock object implementing the same
`ops`** — no MMIO, no OS dependency. The simulator runs the *same* SIR-level program with device
ops dispatched to host models instead of memory-mapped registers. This is feasible **and portable to
macOS and Windows** *only if the runtime contains no UNIX-isms* — which is a first-class constraint
on the runtime, not an afterthought (it is the same constraint that keeps the language
embedded-native). Sim is where you develop blink+button before touching metal, and where CI runs.

The simulator is also the path to learning **graph-aware debug info**: the sim *is* a runtime that
knows full language-level state (which reactions exist, each device's `when` state, every cell's
value), so it is the natural place to prototype the debug model that the on-metal decoder (§5.4) and
the agent (§7.2) consume.

**The simulator is deterministic by default (D19).** "Same SIR-level program" only helps if a run is
reproducible, so the sim defines: a **virtual clock** that advances only when the program would wait
(no wall-clock dependence); **explicit event injection** (inputs, IRQs, bus responses arrive at
scripted virtual-time points, not whenever the host happens to schedule); a fixed, documented order
for simultaneous IRQs matching the scheduler's deterministic order (§5.1, D02); modelled **register
side-effects** (`w1c`/`rc`/reset values from §4.2, D04); and a first-class **fault-injection** API
keyed to each op's declared fault codes (§4.4, D14). Nondeterminism (jitter, random inputs, race
exploration) is available but **opt-in and seeded**, so a failing run is always replayable — which is
what makes the sim usable as CI and as the agent's debug loop rather than a flaky approximation.

### 7.2 Graph-aware debug info

The aspirational debugging goal: debug info carries the **reactive graph + device state**, so an
agent (or a human) debugs at the *language's* abstraction level — "reaction `pump_ctrl` is yielded
awaiting `i2c` while `valve` is `closed`" — rather than at register/PC level. The same tables that
power the Layer-3 fault decoder (§5.4) power this. Where useful, Silica leverages existing **MCP
servers** for GDB, serial, and logic-analyzer (Saleae) access, plus Cortex-M fault-register
knowledge, so the agent can correlate language-level state with bus traces and trap registers.

### 7.3 MCP / agentic integration

The structured-truth design (§2, §3.6) means an agent edits via **typed structured edits** (`set`/
`extend`/`remove`) validated the same way code is, not by emitting text diffs that may merge into
nonsense. The overlay mechanism is the agent's edit API. Combined with graph-aware debug info, the
agent's author→edit→debug loop runs entirely at the language's abstraction level.

### 7.4 Standard library as the agent's idiom corpus

The std lib is *also* the agent's worked-examples corpus — designing it **is** designing the agent's
idioms. Minimal v1: pool/arena allocator, ring buffer, fixed-capacity collections, fixed-point math,
and the canonical device types `uart`, `gpio`, `i2c`, `spi`, `timer` (and the interfaces they
implement). Every std-lib device is built from datasheets (§8), is un-privileged (§2), and
demonstrates one pattern cleanly, because the agent will learn the language *from these files*.

### 7.5 Self-versioning

The spec, the std lib, and the agent-facing guidance **version together** (cf. version-matched
skills), so an agent never generates against a language version that no longer exists. A program
declares the language version it targets; the toolchain and the agent guidance for that version are
retrievable as a matched set. This is a correctness mechanism for agentic use, not just hygiene: it
removes "the model is writing valid v0.3 against a v0.5 compiler" as a failure mode.

---

## 8. Zephyr interop — port the facts, not the framework

**Goal (facts):** harvest Zephyr's DTS/bindings as a hardware-validated database of base addresses,
IRQ numbers, and clock topologies — free breadth, a bounded problem, and a good agentic task. A
mechanical **DTS→Silica transpiler** reads `.dts`/`.dtsi` + bindings and emits `board`/`soc` types
(§3.3): nodes with `reg`/`interrupts`/`clocks` become typed instances with `at`/`needs irq`/`needs
clock`; `compatible` strings map to Silica device types where one exists, and to a `raw`-backed
stub where one does not yet — **and every stub emits a diagnostic, never a silent pass-through**
(D10). The transpiler validates against the Silica type system, so a fact that does not type-check
(an IRQ with no controller, a clock with no source) surfaces as a diagnostic too.

**The transpiler runs the target's C preprocessor as phase 1 — and this does *not* violate the
no-preprocessor rule (Gemini SIL-007).** Real `.dts`/`.dtsi` lean on `#include`, `#define`, and
macro arithmetic, so the DTS *ingestion pipeline* is two-phase: `[raw .dts/.dtsi] → cpp -nostdinc
-undef -x assembler-with-cpp → [flat preprocessed DTS] → Silica DTS parser → [board .si AST]`. The
key distinction: §3.1 bans a preprocessor in **Silica source**; here `cpp` is run over **foreign
input** by an import tool, exactly the way one runs a parser over any other external format. Nothing
preprocessed survives into `.si` — the output is plain typed `board`/`soc` AST. This keeps the rule
("no include-order or macro semantics in the language") intact while acknowledging the reality that
the corpus we are harvesting *is* macro-laden.

**The DTS facts are much more than reg/irq/clock, so the supported subset is defined explicitly
(D10).** DTS also encodes `pinctrl` (pad mux — needed even for blink, §16/D16), `resets`, power
domains, GPIO flags, `interrupt-parent` chains, `#address-cells`/`#size-cells`, `aliases`, `chosen`
nodes, flash partitions, DMA channels, and bus-specific properties. The Phase-2 transpiler declares,
per property, whether it is **ingested** (mapped to a typed relation — `reg`, `interrupts`,
`clocks`, `pinctrl`, `resets`, power domains, GPIO flags, `interrupt-parent`, DMA channels, flash
partitions, `aliases`/`chosen`), **interpreted-then-discarded** (the cell-size meta-properties), or
**explicitly rejected with a diagnostic** (anything not yet modelled). The rule mirrors §2: *DTS is
data we ingest* — but ingestion is a typed, diagnosed mapping with a known coverage list, not a
best-effort scrape that silently drops what it does not understand.

**Non-goals (framework), explicitly.** Silica does **not** port: `DEVICE_DT_DEFINE` and the device
init-object model; init levels/priorities; function-pointer driver dispatch; Kconfig conditional
compilation. These encode the C/UNIX driver mental model and would quietly turn Silica into a skin
over Zephyr. Drivers are designed from **datasheets** — the real `regs`/`ops` source of truth — which
dovetails with a future datasheet-extraction pipeline. The line is sharp: *DTS is data we ingest;
the driver framework is a model we reject.*

---

## 9. Open questions — recommendations & tradeoffs

### 9.1 Suspendable handlers?

**Recommendation: yes, but as run-to-completion *between explicit yield points* (§5.2).** A handler
never blocks the scheduler; it completes or `yields`, and suspension is visible in the op type.
**Why:** the device-composition keystone (§3.5) makes slow bus transactions first-class, and
busy-blocking on them under strict RTC starves the system. **Tradeoff:** requires the Embassy-style
state-machine transform in the compiler (§6.1) and creates reentrancy that the atomicity construct
(§5.5) must cover. Strict RTC would be simpler to compile and analyze, but would either forbid
composed slow-bus devices or force busy-waiting — both unacceptable given the long-term ceiling.
The chosen middle keeps each *segment* RTC-simple while making suspension explicit and typed.

### 9.2 Time as a type?

**Recommendation: yes — distinct `duration`/`instant`, unit-safe, with the representation chosen for
later WCET/deadline reasoning (§4.5).** **Why:** unit errors (`now() + 5`) become type errors at
zero runtime cost, and the tick domain falls naturally out of the typed clock topology. **How deep:**
ship unit-safety now; do **not** ship full WCET now, but **foreclosure-check** it — the
RTC-between-yields model makes the analyzable unit a handler segment (straight-line + bounded loops),
so WCET annotations can attach later without redesign. **Tradeoff:** distinct time types add a little
ceremony and some conversion friction at boundaries (you must say `.le`/`.be`, you must construct
durations from typed literals); the payoff is that an entire class of timing bugs is unrepresentable
and the door to schedulability analysis stays open.

### 9.3 Atomicity / interrupt-safety?

**Recommendation: a language construct — typed `cell`s with compiler-computed critical sections via
a priority-ceiling protocol (§5.5).** **Why:** the static reaction↔cell graph lets the compiler mask
*exactly* the racing interrupts for the *shortest* span — strictly better than a hand-written
`disable_irq` hammer, and it cannot be forgotten. The reactive model has room for this as a language
feature precisely because the full set of reactions and their priorities is static. **Tradeoff:**
the compiler must do the access-graph + ceiling analysis (modest), and shared state must be declared
as `cell` rather than an ordinary variable (a small, deliberate friction that makes sharing
visible). Manual critical sections are rejected as the default footgun; a `raw` escape exists for
the exotic case but is opt-in and greppable.

### 9.4 Compile-time evaluation?

**Recommendation: a bounded, total `comptime` sublanguage (§4.7).** **Why:** computed register
divisors, generated LUTs/tables, computed addresses, and — critically — **pool sizes** all need
compile-time computation; and the linker script/vector table/`.data`/`.bss` are themselves comptime
derivations of the board type (§6.4). **Interaction with the memory model:** `comptime`-ness is
exactly what keeps the model statically sized — a pool's `N` must be a comptime value, so allocation
remains analyzable. **Tradeoff:** keeping `comptime` *bounded* (no Turing-complete compile-time
computation) costs some expressiveness versus Zig-style unrestricted `comptime`, but unbounded
compile-time evaluation undermines the "statically knowable" promise and complicates the agentic
analysis. Bounded is the right call for the toy and does not foreclose loosening later.

### 9.5 Content-addressed code?

**Recommendation: confirm the lean — boring, regular *text* source now; content-addressing NOT
foreclosed.** **Why now:** text is what agents, humans, diff tools, and editors already handle; the
grammar is deliberately structured-edit-friendly (named entities, no positional refs, no
preprocessor) so the *benefits* of content-addressing (precise edits, stable identity) are largely
available without paying its tooling cost. **What keeps it un-foreclosed (the hard rule):**
**semantic identity must never depend on textual position or file layout.** Every entity is named;
overlays address named paths (§3.6); there is no include-order or preprocessor semantics. Given that
invariant, moving to a content-addressed store later is "change the storage/identity layer," not
"change the language." **Tradeoff:** we forgo, for now, the Unison/Zero benefits (no broken
references, trivial renaming, perfect caching); we accept ordinary text tooling's weaknesses in
exchange for ubiquity and simplicity. The invariant above is the entire cost of keeping the option
open, and it is cheap, so we pay it.

### 9.6 First prototype slice?

**Recommendation: build the `on`/`every` reactive core first**, in sim then on metal. **Why:** of
{effect/platform boundary, temporal reactive core, agent edit surface}, the reactive core is the
most *novel and instructive* — the declarative-hardware half is largely solved by the Devicetree
rework (§3.3/§3.6), and the agent edit surface (overlays) rides on the type system the core forces
us to build. Building the core first also surfaces the §5.2/§5.5 decisions against real code early,
where they are cheapest to revise. **Concrete minimal milestone:** *blink + button on one open
board, in sim then on metal, via the C backend* — `every 500ms` toggling an LED and `on
button.falling` toggling it too, with one shared `cell` exercising the atomicity construct. This
touches `gpio` and `timer` devices, the scheduler, the C backend, and generated startup/linker — a
true end-to-end vertical slice. **Tradeoff:** leading with the core defers the agent-edit and
FFI/platform-boundary stories, but those are additive over the core rather than foundational, so the
ordering minimizes rework.

---

## 10. Deferred — not foreclosed (register)

For each: it is *safe to defer* and *kept structurally possible* by an existing decision. None is
barred by the type system or memory model.

| Deferred capability | Safe to defer because… | Kept possible by… |
| --- | --- | --- |
| **Filesystem** | A toy needs no persistence; large surface, low novelty now. | It is a state machine over a block `device` — the same composition as SD-over-SPI (§3.5); buffers are bounded pools (§5.3). |
| **Networking / TCP-IP** | Full IP is huge; near-term we only need a protocol state machine (MQTT-SN/BLE-GATT), Golioth's layer. | Bounded-pool allocation (§5.3) covers reassembly/retransmit buffers; a NIC is an external `device`; protocol = a reactive state machine (§5.1). |
| **REPL / shell** | Not needed to prove the core; additive. | It is additive over the live object model; the external-DSL + simulator (§7.1) lineage already implies an interactive driver. |
| **FFI / calling C** | No vendor blobs in the toy's open-hardware scope. | Capabilities (§4.1) give a *clean typed boundary*; an `extern` device/op is a capability-gated edge, not a contaminating hole. SIR (§6.1) already separates language from target. |
| **Bootloader / DFU / OTA** | Out of scope for blink-class goals. | It is "a device that rewrites flash" — a flash `device` with `ops` (§4.1); generated startup (§6.4) already owns the memory map. |
| **Richer observability (metrics/trace UI)** | Manual printf is rejected; we want it *derived*, which needs the graph first. *Note (D13): a **minimal** structured trace ring is now Phase 0/1 (§11), not deferred — only the richer tooling is.* | It is **derived** from the reactive graph + graph-aware debug info (§7.2): events are structured (device+op+code), rendered host-side (no on-device text, §4.3). |
| **Multicore (AMP/SMP), DMA, cache coherency** | Single-core blink needs none of it. | DMA = "a device that does work asynchronously" → the same async/`yields` device shape as a completion event (§5.2); cores are capability boundaries (§4.1); SIR/LLVM (§6.3) does not assume single-core. |
| **MPU/NPU/DSP coprocessors** | Exotic; not needed to prove the model. | A coprocessor = hand-off + completion event → async-device shape (§5.2); access is capability-gated (§4.1). |
| **Secure enclaves / confidential computing** | No security model in the toy. | "A core with a capability boundary" — motivates keeping capabilities-in-the-type clean (§4.1); adding a boundary is a new capability, not a type-system retrofit. |

The recurring pattern is intentional: **almost every deferred item is an instance of the
device-composition keystone (§3.5), the bounded-pool memory model (§5.3), or the capability model
(§4.1)** — the three things designed conservatively up front precisely so the feature list can stay
short without foreclosing the ceiling.

---

## 11. Roadmap

**Phase 0 — first slice (the reactive core).** Implement enough to run **blink + button on one open
board, in sim then on metal, via the C backend** (§9.6). Deliverables: minimal grammar + parser;
`device`/`board`/`program`/`on`/`every`/`cell`; the leaf `gpio` and `timer` device types; the
atomicity construct (§5.5); SIR + C backend (§6.2); generated startup/linker (§6.4); host simulator
(§7.1). **Two items the review (D16/D13) correctly pulls forward into Phase 0, because blink-on-metal
genuinely needs them:** (a) a **minimal pin/pad model** — pad mux, pull, direction — with
**duplicate pin ownership a compile error** (you cannot wire `led_user` and an alternate function to
the same pad); and (b) a **structured trace ring buffer** (fault code, handler id, device id, event
id, tick timestamp; text rendered host-side, no on-device strings, §4.3) so first bring-up is
debuggable instead of dark. Target board: a well-documented open part (e.g. RP2040, or an STM32
Nucleo / iCE40-class target with full datasheets). Success = identical program runs in sim and on
metal, LED blinks, the button reaction shares one `cell` with the timer reaction without a manual
critical section, and a forced fault shows up as a decoded trace record.

*Phase-0 validation matrix (Gemini), the concrete machine-checkable acceptance gates:* (1) **no
dynamic allocation** — `.bss` + `.data` + computed pool/frame/stack sizes equal the program's total
RAM footprint exactly (§5.3); (2) **deterministic pin muxing** — compiling two bindings to one
physical pad is a *static error* (§3.3); (3) **barrier insertion** — the emitted C contains the
required `__DSB()`/`__DMB()`/compiler-volatile fences around each register-write block and before
IRQ enable (§4.2, §6.2). These are CI assertions, not prose claims.

**Phase 1 — composition + faults.** Add interfaces, the `i2c`/`spi` controller leaf devices, one
composed sensor (e.g. BME280, §3.5), `yields`/suspension lowering (§5.2), and the three-layer fault
model incl. safe-state and the **scheduler-fed hardware watchdog** (§5.4–§5.6). This is where the
keystone is proven against real silicon — and the right place to validate the bus model (§3.5) and
register access model (§4.2) against a genuinely awkward part (clock-stretching, `w1c`/`pop_on_read`
registers), per risk #7 (§12).

**Phase 2 — agent edit surface + facts.** Typed overlays (§3.6) as the agent edit API; the
DTS→Silica transpiler (§8) to harvest board facts; graph-aware debug info v1 from the simulator
(§7.2); self-versioning (§7.5).

**Phase 3 — LLVM backend.** Second consumer of SIR (§6.3), validating the C-purity guard and making
the "replace Zephyr" path structurally real. No language changes expected — this is the proof that
none were needed.

**Phase 4+ — deferred items, demand-ordered.** Pull from §10 as real projects need them (protocol
state machine → flash/DFU → filesystem → richer observability), each as an *instance* of an existing
mechanism rather than a new one.

---

## 12. Biggest risks / where this could go wrong

Honest failure modes, roughly in order of how much they would hurt:

1. **The device-composition model proves inexpressive.** This is the keystone (§3.5); if real
   stacked devices (a sensor whose driver needs interleaved reads/writes with timing constraints, a
   bus with clock-stretching, an SD card's command/data state machine) don't fit "ops as transactions
   over a substrate," much of the design unwinds. *Mitigation:* prove it early (Phase 1) against
   genuinely awkward parts, not just a clean I²C temperature sensor; keep the `raw` escape and the
   `yields` model flexible. This is the single highest-leverage risk and the reason composition is
   designed first.
2. **The C backend's purity is cosmetic (§6.2).** If SIR absorbs C/UNIX semantics, "C now, LLVM
   later" becomes a tar pit and the embedded-native promise leaks. *Mitigation:* hold SIR to "must
   lower to LLVM with no libc"; treat the Phase-3 LLVM backend as a *standing proof obligation*, and
   ideally stub a thin LLVM lowering early to catch leakage before it calcifies.
3. **Scope creep toward becoming an RTOS too early.** The "replace Zephyr" ceiling is a foreclosure
   constraint, not a v1 target; chasing networking/filesystem/multicore before the core is solid
   would collapse the toy under its own weight. *Mitigation:* the §2 scope-vs-foreclosure rule and
   the §10 register exist precisely to let us say "deferred, and here's why it's still possible"
   instead of building it now.
4. **The overflow/units/no-text strictness becomes friction that pushes users to escape hatches.**
   If `raw`, `+%`, and explicit endianness are needed constantly, people route around the type
   system and the guarantees evaporate. *Mitigation:* make the *common* path ergonomic (good
   defaults, typed literals, fixed-point that "just works"), and measure how often escape hatches
   appear in the std lib — if the corpus is full of `.raw`, the defaults are wrong.
5. **Agentic-native is asserted, not validated.** "Good for agents" is a hypothesis until an agent
   actually authors/edits/debugs non-trivial Silica and we measure it. *Mitigation:* treat the std
   lib as the idiom corpus (§7.4) and the overlay API as the edit surface (§7.3) from Phase 2, and
   run real agentic loops against them as an evaluation, not a vibe.
6. **The "no privileged built-ins" purity costs more than it's worth.** Forcing `gpio`/`uart`/NVIC
   through the same `device`/`ops` machinery as exotic parts could make common things verbose.
   *Mitigation:* let the std lib carry the verbosity once so user code stays terse; revisit only if
   the common path is genuinely painful — but do not introduce a two-tier system, which would
   reintroduce exactly the "built-ins can do what your code can't" problem the design rejects.
7. **The embedded-correctness surface is large, and a clean abstraction can hide a wrong one.** The
   parts that make this a real embedded language — the register access model (§4.2), bus arbitration
   (§3.5), scheduler/queue semantics (§5.1), cell-across-yield rules (§5.5), time conversion and
   overrun (§4.5), stack accounting (§5.3), and the C-UB-free backend subset (§6.2) — are each a
   place where an elegant surface can quietly encode incorrect hardware behaviour (a clobbered
   `w1c` bit, a reordered DMA arm, a torn cell, drifting periods). This whole class is what an
   external review (the GPT-5.5 pass that prompted §§4.2/5.1/5.5/6.2 revisions) flagged as the
   biggest gap. *Mitigation:* treat these as **normative** parts of the spec, not prose; validate
   each against a genuinely awkward real part in Phase 0/1 (clock-stretching bus, w1c/rc status
   registers, an overrunning periodic) rather than only the clean happy path; and keep the simulator
   (§7.1) modelling the nasty side-effects so they are exercised in CI, not discovered on metal.

---

*End of design draft. This document versions with the language: spec, std lib, and agent-facing
guidance move together (§7.5).*
