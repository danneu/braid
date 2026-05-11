# plan-the-fix-calm-prism

## Context

`RelocationCheck` and `check_relocation_space` in `cli/src/remove_missing.rs`
are declared `pub(crate)` even though they have no callers outside the
file. Verification (`grep -rn` across `cli/src/` and `cli/tests/`) found
only:

- The production call site at `cli/src/remove_missing.rs:438-440` (same
  file).
- In-module test usages in `mod tests` starting at line 634 (same file;
  child module sees parent-private items without any visibility
  modifier).
- A doc-comment mention of the function name at
  `cli/src/test_fixtures/remove_missing.rs:64` -- a string in a comment,
  not a real reference.

The `pub(crate)` visibility is misleading: it implies "callable from
anywhere in the crate" when in fact nothing outside this file calls
either symbol. Tightening to module-private removes that false signal
and lets the compiler reject any future reach-around at the moment it
is introduced.

This is a hygiene change. No behavior changes. No test changes.

## Change

`cli/src/remove_missing.rs`:

- Line 502: `pub(crate) enum RelocationCheck {` -> `enum RelocationCheck {`
- Line 520: `pub(crate) fn check_relocation_space<R: CommandRunner>(`
  -> `fn check_relocation_space<R: CommandRunner>(`

Leave the doc comments on both items unchanged.

No other `pub(crate)` items in this file (`grep -c "pub(crate)"
cli/src/remove_missing.rs` returns 2 -- both are the targets). No
sibling cleanup in scope for this plan; a wider audit of
unused-`pub(crate)` across the crate is a separate task.

## Verification

1. `just test-rust` -- exercises the in-module tests at lines 890, 939,
   978, 1027 (`check_relocation_space_*`), which still resolve the
   symbols via parent-module privacy. Must pass.
2. `cargo check -p braid-cli` (or rely on `just test-rust` for the full
   compile) -- confirms no out-of-file caller breaks. The verification
   grep already established there isn't one, so this is belt-and-braces.

No VM tests, fixture refresh, or documentation updates are required:
the change is internal to one Rust file and does not affect any tool
output, parser, or CLI surface.
