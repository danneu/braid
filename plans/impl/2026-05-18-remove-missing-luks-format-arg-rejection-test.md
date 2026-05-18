# Plan: Add `remove-missing` `--luks-format-arg` Rejection Test

## Context

The structural refactor that motivated the original version of this plan is
already at HEAD. `cli/src/main.rs` defines a separate `PassphraseInputArgs`
struct (line 151) that is flattened only into the five passphrase-consuming
commands (`AddArgs:188`, `ReplaceArgs:228`, `RecoverArgs:112`,
`UnlockArgs:252`, `EnrollKeyFileArgs:272`); the narrowed `CommonArgs` (line
136) carries only `dry_run` / `yes` / `progress`; every dispatch site reads
`args.passphrase.passphrase_*`; and `passphrase_file` carries
`conflicts_with = "passphrase_stdin"` (line 156) so stdin and file inputs
cannot silently combine. Two regression tests already pin this:

- `passphrase_input_conflicts_are_rejected` (`main.rs:1037`) -- asserts
  `ErrorKind::ArgumentConflict` when both passphrase inputs are passed,
  across every passphrase-consuming command plus the `unlock --key-file`
  variants.
- `remove_commands_reject_passphrase_flags` (`main.rs:1115`) -- asserts
  `ErrorKind::UnknownArgument` for `remove` and `remove-missing` against
  both passphrase flags.

The only remaining gap is symmetry on the LUKS-format-arg surface: the
existing `remove_does_not_accept_luks_format_arg` test (`main.rs:1009`)
pins `remove`, but not `remove-missing`. `RemoveMissingArgs` already does
not flatten `LuksFormatArgs`, so `braid remove-missing --luks-format-arg=...`
is rejected today -- this is purely a regression-pin to prevent a future
flatten from silently re-accepting it.

Outcome: one new test, no struct / dispatch / behavior changes.

## Files Touched

- `cli/src/main.rs` -- test module only.

## Change

Append after `remove_does_not_accept_luks_format_arg` (after line 1014),
inside the existing `#[cfg(test)] mod tests` block:

```rust
// Intent: remove-missing rejects --luks-format-arg because it never
// formats a fresh LUKS volume.
// Why it exists: RemoveMissingArgs structurally does not flatten
// LuksFormatArgs, but only `remove` had a parse-rejection regression test
// before. A future reflexive flatten would regress this silently.
// Scenario: an operator copy-pastes a --luks-format-arg=... flag from add
// or replace into a remove-missing invocation.
#[test]
fn remove_missing_does_not_accept_luks_format_arg() {
    let err = Cli::try_parse_from([
        "braid",
        "remove-missing",
        "--missing-id",
        "1",
        "--luks-format-arg=--pbkdf",
    ])
    .expect_err("remove-missing must not expose LUKS format options");

    assert!(err.to_string().contains("unexpected argument"));
}
```

Assertion style matches the adjacent `remove_does_not_accept_luks_format_arg`
exactly (`expect_err` + substring match), not the newer
`ErrorKind::UnknownArgument` style of `remove_commands_reject_passphrase_flags`
-- the goal is symmetry with the test it shadows, not consolidation. The
three-line `//` preamble follows the AGENTS.md Test Conventions.

## Verification

1. `just test-rust` -- compiles; the new test passes; existing tests are
   unaffected.
2. Conventional commit: `test(cli): pin remove-missing --luks-format-arg rejection`.

## Out of Scope -- Already at HEAD

The earlier `/plan` ran through Phase 1/2 against a snapshot of `main.rs`
predating this work. These items are now done and must not be re-attempted:

- Splitting `PassphraseInputArgs` out of `CommonArgs` (`main.rs:151`).
- Flatten into `Add` / `Replace` / `Recover` / `Unlock` / `EnrollKeyFile`
  (`main.rs:188`, `228`, `112`, `252`, `272`).
- Narrowing `CommonArgs` to `dry_run` / `yes` / `progress`
  (`main.rs:136`).
- Dispatch-site rewrites to `args.passphrase.passphrase_*`
  (`main.rs:406-407`, `496-497`, `572-573`, `612-613`, `873-874`).
- `--passphrase-stdin` / `--passphrase-file` parse-rejection regression
  tests on `remove` and `remove-missing` (covered by
  `remove_commands_reject_passphrase_flags`, `main.rs:1115`).
- `--passphrase-stdin` / `--passphrase-file` mutual conflict (covered by
  `passphrase_input_conflicts_are_rejected`, `main.rs:1037`).

## What the Earlier Plan Got Wrong

- Stale against HEAD: targeted a pre-refactor `main.rs` that no longer
  exists. Implementing it literally would have duplicated `PassphraseArgs`
  alongside the existing `PassphraseInputArgs`.
- Would have dropped the `conflicts_with = "passphrase_stdin"` attribute
  on `passphrase_file` (`main.rs:156`), regressing the stdin-vs-file
  safety invariant currently pinned by
  `passphrase_input_conflicts_are_rejected`. In the read path
  (`cli/src/luks.rs:327`), the file branch is checked first, so a silent
  fallback would have ignored stdin bytes the operator expected to be
  consumed.
- Used the wrong struct name (`PassphraseArgs`) -- the existing extraction
  is named `PassphraseInputArgs`.
