# Collapse Shape A stderr-emission boilerplate

## Context

Seven Shape A commands (`add`, `remove`, `remove-missing`, `recover`,
`unlock`, `enroll`, `replace`) each render plan-derived notes to stderr
via the same six-line block: resolve `color_enabled_for_stderr()`, call
`preview::render_notes_for_stderr_with(notes, Self::STDERR_STYLE,
color_enabled)`, and `eprint!` the result. The block appears at two
sites per command (`XxxPlan::execute` plus the `cmd_xxx` preserved-
context Err arm) and at additional sites inside `replace.rs`'s capture-
aware wrapper.

A code-review finding proposed dropping `STDERR_STYLE` and defaulting
the renderer to `Bracketed`. That's wrong: `enroll_key_file::EnrollPlan`
deliberately uses `PerDiskStyle::Plain` to preserve byte-identical
`skip: <name> not present` wording (pinned by an ADR at
`plans/impl/2026-04-24-dry-run-preview-refactor.md:139` and a
byte-exact assertion at `enroll_key_file.rs:1198`). Defaulting would
silently regress that contract.

The correct simplification is to extract a small `emit_notes_to_stderr`
helper from the shared `preview` module and route the 12 boilerplate
sites through it. The per-command `STDERR_STYLE` consts stay --
they're the contract anchor for enroll's divergence and they pin
"success and failure paths MUST use the same per-command style".
`replace.rs` keeps its capture-aware emit wrapper because it serves a
different concern (test interception of `eprint!`), and lifting that
into `preview.rs` for a single caller is overreach.

Outcome: ~60 lines of repeated scaffolding collapse into one-line
helper calls. Zero behavioral change. The contract that motivated the
const stays visible at the same locations.

## Change

### 1. Add the helper

`cli/src/preview.rs` -- add a new public function alongside the
existing `render_notes_for_stderr` / `render_notes_for_stderr_with`
(currently at lines 152-187):

```rust
/// Render `notes` and emit the result to stderr with the per-command
/// `style`. Wraps `render_notes_for_stderr_with` plus the standard
/// `color_enabled_for_stderr()` resolution so Shape A commands
/// (`XxxPlan::execute` and `cmd_xxx` Err arms) collapse to a single
/// call. `replace` does not use this helper -- its capture-aware
/// wrapper owns the write side.
pub fn emit_notes_to_stderr(notes: &[PreviewNote], style: PerDiskStyle) {
    let color_enabled = crate::status_tag::color_enabled_for_stderr();
    eprint!("{}", render_notes_for_stderr_with(notes, style, color_enabled));
}
```

Doc comment is justified per AGENTS.md "Doc Comments" rule: it pins the
intent (collapse Shape A scaffolding) and the boundary (replace stays
out).

### 2. Route Shape A call sites through it

Replace the six-line `eprint!(... render_notes_for_stderr_with ...)`
block at each site with a one-liner:

```rust
preview::emit_notes_to_stderr(&self.notes, Self::STDERR_STYLE);
// or, in cmd_xxx Err arms:
preview::emit_notes_to_stderr(&report.notes, XxxPlan::STDERR_STYLE);
```

Sites (12 total):

| File | `XxxPlan::execute` site | `cmd_xxx` Err arm site |
| --- | --- | --- |
| `cli/src/add.rs` | ~707-710 | ~1410-1414 |
| `cli/src/remove.rs` | 188-195 | 439-446 |
| `cli/src/remove_missing.rs` | 162-169 | ~456-463 |
| `cli/src/recover.rs` | 1035-1042 | ~1357-1365 |
| `cli/src/unlock.rs` | 86-93 | ~230-238 |
| `cli/src/enroll_key_file.rs` | 443-450 | ~679-687 |

(Line numbers are at-time-of-planning; locate the
`render_notes_for_stderr_with` calls during implementation.)

### 3. Leave the rest alone

- All seven `pub const STDERR_STYLE: PerDiskStyle = ...` declarations
  (`add.rs:683`, `remove.rs:168`, `remove_missing.rs:142`,
  `recover.rs:1014`, `unlock.rs:66`, `enroll_key_file.rs:425`,
  `replace.rs:280`) and their doc comments stay verbatim.
- `cli/src/replace.rs` -- no changes. `emit_replace_notes_to_stderr`,
  `render_replace_notes_for_stderr`, `emit_replace_stderr`, and the
  `replace_stderr_capture` module (lines ~789-846) all stay.
- The four byte-exact rendering tests at `add.rs:6415`,
  `remove.rs:2449`, `remove_missing.rs:2267`, and
  `enroll_key_file.rs:1197` continue to call
  `preview::render_notes_for_stderr(&notes, XxxPlan::STDERR_STYLE)`
  unchanged -- they exercise the pure render function, not the new
  emit helper.

## Reuse

- `preview::render_notes_for_stderr_with` at `cli/src/preview.rs:156`
  -- the new helper is a thin wrapper over this existing function.
- `crate::status_tag::color_enabled_for_stderr` -- already used at
  every current call site; the helper just centralizes the call.

## Verification

End-to-end checks after the refactor:

1. `just test-rust` -- exercises the four byte-exact rendering tests
   that pin per-command stderr wording. They use the pure
   `render_notes_for_stderr` (unchanged), so they should pass without
   modification. Failure here means the helper accidentally changed
   render output.
2. `just test-vm` -- exercises real-run stderr for every Shape A
   command in NixOS VM tests. Particularly:
   - `enroll-skip-notes` (or equivalent) -- verifies enroll's
     `skip: <name>` Plain wording survives.
   - `unlock-degraded-refused` / `recover-*` -- verifies probe-event
     notes render in Bracketed style on failure.
   - `add-noop` / `remove-warn-notes` -- verifies plan-derived warns
     render as `[warn] ...`.
3. `git diff` review: confirm exactly 12 call sites changed, each
   from a six-line `eprint!` block to a one-line
   `preview::emit_notes_to_stderr(...)` call; confirm `replace.rs`
   has zero diffs; confirm all seven `STDERR_STYLE` consts still
   exist and are unchanged.

If all three pass, the refactor is byte-equivalent to today's output.

## Out of scope

- Lifting `replace_stderr_capture` into `preview.rs`. The single test
  that uses it (`cmd_replace_old_equals_new_aborts_before_any_probe`,
  `replace.rs:5662`) relies on end-to-end `eprint!` interception; no
  other command tests against capture today.
- Inlining `STDERR_STYLE` literals or merging the const across
  commands. The named anchor pins enroll's `Plain` divergence and the
  "two call sites per command must agree" contract.
- Adding a `Plan` trait to host `emit_notes_to_stderr` as a method.
  The seven plan types are independent structs by design; a free
  function in `preview.rs` matches the existing `render_*` helpers'
  shape.
