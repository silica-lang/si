# The Reactive Model

Silica programs do not have a `main` loop, tasks, or an RTOS to plumb. Instead, a
program declares **reactions**, and reactions are the unit of execution.
`on <event>` and `every <duration>` are the *entire* concurrency model — no
task-create, no semaphores, no manual scheduling.

## Two reaction forms

A reaction is a block of code that runs in response to a trigger. There are
exactly two triggers:

- **`every <duration> { … }`** — a *primitive temporal trigger* that fires on a
  fixed cadence.
- **`on <event> { … }`** — fires when a named event occurs, such as a GPIO edge
  or a device-emitted event.

```si
every 500ms {
  lit = not lit
  led.set(lit)
}

on button.falling {
  lit = not lit
  led.set(lit)
}
```

That is the whole surface. A reaction runs to completion (between yields), then
the program waits for the next trigger. How reactions are ordered and scheduled
is covered in [Scheduling](../execution/scheduling.md).

## A complete program: blink + button

Here is the canonical reactive slice. An LED toggles on a timer, and a button
press toggles the *same* state.

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
    lit = not lit          // keep the shared cell consistent — both reactions now touch `lit`
    led.set(lit)
  }
}
```

Two reactions, both touching `lit`. That is all it takes to blink an LED and let
a button flip it.

## Shared state with `cell`

`cell` marks state that more than one reaction may touch. In the example above,
both `every 500ms` and `on button.falling` read and write `lit`, so it is
declared as a `cell`.

Because two reactions share `lit`, the compiler computes the critical section
**automatically** from the static handler-to-cell graph — there is no
`disable_irq` anywhere in user code. You declare the shared state; the compiler
makes the accesses consistent. The details of how those critical sections are
derived and applied live in [Atomicity & shared cells](../execution/atomicity.md).

## Triggers are ordinary devices underneath

`every 500ms` is a primitive temporal trigger, but it is not magic: the compiler
implements it by allocating a timer/compare channel from the board's timer
devices. The timer is an ordinary `device`, not a privileged built-in — the same
is true of GPIO ports and the events they emit. There are no special-cased
peripherals in the language; the reactive model is built on top of the same
[devices](structure.md) you declare yourself.
