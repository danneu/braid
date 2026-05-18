# Plan: fix the `extra` example in the `ups status` manual

## Context

The JSON shape example in `manual/commands/ups-status.md:67` shows
`"extra": { "driver.name": "usbhid-ups" }`. That single-key cherry-pick
misrepresents what `braid ups status --json` actually emits.

Tracing the captured fixture
(`cli/tests/fixtures/nixos-25.11/upsc/upsc-online.txt`, 32 lines)
through `parse_upsc` (`cli/src/parse/upsc.rs:45-125`) produces **12**
untyped keys in `extra`:

- `battery.charge.low`
- `driver.debug`
- `driver.flag.allow_killpower`
- `driver.name`
- `driver.parameter.{mode,pollinterval,port,synchronous}`
- `driver.state`
- `driver.version`
- `driver.version.internal`
- `input.voltage.nominal`

`ups.mfr` and `ups.model` are *not* in `extra` -- the parser routes them
into typed `device.*` fields (or discards them when `device.*` is
already populated). The current one-key example also miscommunicates
that distinction.

The parser type's own doc comment already says it correctly:

> `cli/src/parse/types.rs:644-647`
> "Typed `upsc <name>` output. `extra` keeps every `key: value` line
> that did not land in a typed field..."

Only the user-facing manual contradicts this. A user copy-pasting from
the manual into a `jq` filter could key off the wrong shape and miss
the bulk of driver-emitted data.

## Scope

Single-file documentation fix. Severity: low. No code change, no
behavior change, no other doc to update -- `manual/guides/ups.md:78-86`
references `UpscOutput` by name but does not duplicate the JSON shape,
and the manual is not embedded in `--help` output or tests.

## Change

**File:** `manual/commands/ups-status.md`

Rewrite line 67 from:

```json
  "extra": { "driver.name": "usbhid-ups" }
```

to a shape that uses only real representative keys -- no literal
ellipsis entry inside the JSON object, since `extra` never contains a
`"...": "..."` pair. The elision (i.e. that more keys may be present)
lives entirely in the prose note added *after* the closing fence of
the JSON block (between current lines 69 and 71).

Proposed JSON line:

```json
  "extra": { "driver.name": "usbhid-ups", "battery.charge.low": "10" }
```

Proposed prose insert (one line, between the existing fence and the
"Distinct sentinels cover..." line):

> `extra` is a string-keyed map of every `upsc` line that did not land
> in a typed field above. Its contents vary with the NUT driver and
> version (typically `driver.*` debug keys plus other untyped fields
> like `battery.charge.low` or `input.voltage.nominal`), and values
> are kept verbatim as strings.

The two example keys are deliberately mixed (one `driver.*`, one
`battery.*`) so readers don't assume `extra` is only driver metadata.
Values are quoted strings because `extra` is `BTreeMap<String,
String>` (`cli/src/parse/types.rs:663`) -- everything serializes as a
string, even numbers like `"10"`. That detail matters for `jq` users
and is currently invisible in the manual.

## Critical files

- `manual/commands/ups-status.md` -- only file to edit (lines 67 and
  a one-line prose insert after line 69).

## Verification

- Visually re-read the rendered JSON block in
  `manual/commands/ups-status.md` and confirm:
  - `extra` shows two real keys from distinct categories (one
    `driver.*`, one non-driver).
  - The JSON object contains no literal `"...": "..."` entry -- the
    elision lives in the prose insert below the block.
  - The string values are quoted (matches the parser's `BTreeMap<String,
    String>` typing).
- Visually confirm the prose insert sits between the JSON code fence
  and the "Distinct sentinels..." paragraph and explicitly conveys
  that `extra` contents vary by driver and version.
- Run `just test-rust` to confirm no Rust unit tests reference this
  manual snippet (none should -- the manual is not consumed by code).
- No VM tests needed -- this is documentation only.

## Out of scope

- Tooling to auto-derive the manual JSON example from the Rust type.
  Worth considering separately if more `--json` examples drift, but
  not justified for a single inaccurate example.
- Adding more typed fields to `UpscOutput` (e.g. promoting
  `battery.charge.low` or `input.voltage.nominal` out of `extra`).
  Separate design question -- the manual fix here just documents the
  current behavior accurately.
