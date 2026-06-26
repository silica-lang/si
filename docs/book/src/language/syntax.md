# Syntax & Grammar

Silica's grammar is deliberately small and predictable. The whole point is that a
program should be easy for both a person and a machine to read, index, and
analyze without running anything. This page describes the design philosophy
behind the grammar and the general shape of the language; the chapters that
follow build real programs on top of it, starting with
[Programs, Boards, SoCs & Devices](structure.md).

## Design philosophy

A few rules shape everything else:

- **One spelling per construct.** Each kind of declaration is written one way.
  There is no "two ways to say the same thing," which keeps the language regular
  and easy to index.
- **Everything is named; nothing is referenced by position.** You never refer to
  a *thing* by its index in an array. The Devicetree `<&phandle 0 2>` cell-array
  pattern — an anonymous, positional reference to a named entity — is explicitly
  banned. (See [Composed Devices on a Bus](composition.md) for why this matters
  in practice.)
- **Typed scalar arguments are fine.** The ban is on referring to *entities* by
  position, not on passing values. `gpio_a.pin(5)` passes a pad index, and
  `pll(hse, mul = 84, div = 8)` passes one named entity plus named scalars. The
  rule is "you never refer to a *thing* by its position in an array," not
  "functions take no arguments."
- **No hidden state, no preprocessor.** There is no preprocessor and no textual
  include-order semantics. Whitespace is not significant beyond separating
  tokens, and blocks are brace-delimited. What you read is what the compiler
  sees.
- **Statically analyzable.** Because every entity is named and every relation is
  expressed at the grammar level, a tool can build the full graph of a program by
  reading it — no macro expansion, no link-order surprises.

The grammar is **regular and indexable**: the recurring shape is
`subject verb args`.

## The shape of a module

A module is a sequence of top-level items. Each item is one of a handful of
forms:

```ebnf
module      = { item } ;
item        = interface | device | board | program | overlay | const | comptime_fn ;
```

The major forms map onto the chapters in this section:

- `device` — a peripheral: its registers, configuration, wiring needs, and
  operations. See [Programs, Boards, SoCs & Devices](structure.md) and, for the
  type-level detail, [Devices & Interfaces](../types/devices.md).
- `board` — a concrete SoC plus wiring: memory map, clock tree, and peripheral
  instances. See [Programs, Boards, SoCs & Devices](structure.md).
- `program` — the reactive entry point that uses a board and declares reactions.
  See [The Reactive Model](reactive.md).
- `interface` — a named set of ops with semantics that controllers *provide* and
  downstream devices `needs`. See [Composed Devices on a Bus](composition.md).
- `overlay` — typed structured edits over a named target. See
  [Typed Overlays](overlays.md).
- `const` / `comptime_fn` — compile-time values and helpers.

## A sketch of the grammar

The following EBNF-ish sketch is illustrative, not final, but it shows the
overall structure. Every snippet elsewhere in these docs is meant to parse under
it.

```ebnf
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

You will recognize each of these forms in the chapters ahead. The next one starts
where most programs do — with a [device, a board, and a program](structure.md).
