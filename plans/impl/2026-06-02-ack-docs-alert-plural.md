# Fix stale `alert(s)` sample output in `braid ack` docs

## Context

`docs/commands/ack.md:22` shows the sample output of `braid ack` as:

```
acknowledged 3 alert(s)
```

The CLI never emits that `(s)` parenthetical form. The emitter at
`cli/src/ack.rs#cmd_ack_impl` (the `println!` block, currently lines 107-115)
pluralizes inline:

```rust
println!(
    "acknowledged {} alert{}",
    causes.len(),
    if causes.len() == 1 { "" } else { "s" }
);
```

So 3 causes prints `acknowledged 3 alerts` and 1 cause prints
`acknowledged 1 alert`. A reader who greps logs or scripts for the literal
`alert(s)` string will never match.

This is stale-after-refactor, not wrong-from-birth. The doc was written against
older code that did print `alert(s)`; commit `ff8235a8`
(*"refactor(cli): remove literal plural markers from output"*, implementing
`plans/impl/2026-05-14-cli-drop-literal-s-pluralization.md`) deliberately
replaced the `(s)` literal with the pluralizing ternary, and the doc line was
never updated -- it was later carried forward verbatim into the unified docs
tree by `403d1b07`.

Intended outcome: the documented sample output matches what the CLI actually
prints, completing the 2026-05-14 literal-plural-marker cleanup across the
user-facing surface (docs as well as code).

## Change

Single-line edit in `docs/commands/ack.md` (line 22):

- From: `acknowledged 3 alert(s)`
- To:   `acknowledged 3 alerts`

Keep the count `3` -- it is a realistic multi-alert example, and the plural
form is the one most users with active alerts will see. No other text in the
file changes.

## Scope boundary (deliberately not touched)

A sibling sweep confirmed this is the only stale instance; do not expand the
edit beyond line 22.

- `docs/guides/recovery-scenarios.md:232` ("surviving disk(s)") is explanatory
  prose where the count genuinely varies, not a pinned CLI-output example.
  Correct English; leave it.
- The rest of `ack.md`'s output examples are already accurate: `no active alerts`
  (line 28) matches `cli/src/ack.rs:81`, and `acknowledged current alerts`
  (prose, line 45) matches the smartd-only / cleanup-retry branches.
- No test asserts the count form, so there is nothing to update on the test
  side. `tests/cli/braid-smartd-alert.py:51` pins only the
  `acknowledged current alerts` smartd-branch string; line 75 is a loose
  `"acknowledged" in stdout` substring check. Neither references `alert(s)` or
  the `acknowledged N alerts` count output.

## Verification

- Confirm `docs/commands/ack.md:22` now reads `acknowledged 3 alerts` and the
  literal `alert(s)` no longer appears in the file:
  `rg -n 'alert\(s\)' docs/commands/ack.md` returns nothing.
- Cross-check the wording against the live format string at
  `cli/src/ack.rs#cmd_ack_impl` (`"acknowledged {} alert{}"`).
- Optional: `mdbook build docs` still succeeds. The edit touches plain output
  text -- no links change -- so `mdbook-linkcheck2` is unaffected; this is just
  a sanity build.
- No Rust or VM tests need to run for this change.
