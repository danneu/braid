# Add Credential Verification Wait Lines

## Summary

Add one durable stderr status line after a passphrase or keyfile is submitted
and before braid verifies it with cryptsetup:

```text
[wait] passphrase: checking against toshiba-pro-00ff...
[wait] keyfile: checking against toshiba-pro-00ff...
```

This fills the silent delay without adding spinners, async progress, prompt
changes, or TTY-specific behavior.

## Key Changes

- Add `StatusTag::Wait` to the shared status-line API.
- Render it as `[wait]` in plain output.
- Render it in colored output as `\x1b[90m[wait]\x1b[0m`, matching `[skip]`.
- Pad it like `[warn]`, `[fail]`, and `[skip]`, so message text still starts at
  column 8.
- Add a shared helper for credential wait rows, using `StatusTag::Wait`:
  - passphrase body: `passphrase: checking against {name}...`
  - keyfile body: `keyfile: checking against {name}...`
  - output: stderr via existing `status_line(...)` and the caller's existing
    `color_enabled` value.
- Call the helper immediately before every real `luks::verify_passphrase(...)`
  and `luks::verify_key_file(...)` call.
- Covered call sites:
  - unlock passphrase verification in `open_disks_with_passphrase`, using
    `first_name` from `to_unlock[0]`.
  - unlock keyfile verification in `execute_unlock_and_mount`, using
    `first_name` from `plan.to_unlock[0]`.
  - recover credential verification through the same mount execution paths.
  - enroll passphrase verification in `verify_first_candidate_passphrase`,
    using the first enrollment candidate name.
  - enroll existing-keyfile probe in `plan_enrollment`, using the current
    candidate name for each `verify_key_file` probe.
  - add passphrase preflight against an existing pool member, using
    `name_from_mapper(existing.mapper.0).unwrap_or(existing.mapper.0)`.
  - replace passphrase preflight against an existing pool member, using the
    same mapper-to-name display rule as add.
  - replace passphrase preflight against a preformatted closed new disk, using
    `new_name`.
- Leave success and error wording unchanged.

## Public Interface Impact

- Human CLI stderr output gains one new line before every real passphrase or
  keyfile verification.
- `StatusTag` gains one enum variant: `Wait`.
- No CLI flags, JSON output, dry-run output, config schema, or README examples
  need to change.

## Test Plan

- Update `status_tag` unit tests to include `StatusTag::Wait` in:
  - visible-column alignment test
  - plain tag pinning test
  - colored tag pinning test
  - ANSI stripping parity test
- Add focused behavioral coverage for each credential verification surface:
  - unlock passphrase: stderr contains
    `[wait] passphrase: checking against disk1...` before unlocked output.
  - unlock keyfile: stderr contains
    `[wait] keyfile: checking against disk1...` before unlocked output.
  - recover passphrase: stderr contains the passphrase wait row before recover
    proceeds through unlock.
  - add passphrase preflight: stderr contains the passphrase wait row before
    add accepts or rejects the pool-member passphrase.
  - replace passphrase preflight: stderr contains the passphrase wait row for
    the existing-pool-member preflight and for the preformatted-new-disk
    preflight when that path is exercised.
  - enroll passphrase preflight: stderr contains the passphrase wait row before
    enrollment planning accepts or rejects the passphrase.
  - enroll existing-keyfile probe: stderr contains a keyfile wait row before
    each real existing-keyfile probe.
- Run `just test-rust`.
- Run targeted VM tests for the command paths whose assertions are added.

## Assumptions

- The wait line should appear for all passphrase and keyfile credentials,
  because the slow operation is verification, not credential reading.
- The line should not be cleared or overwritten; logs should preserve it.
- The trailing `...` is part of the exact intended human output.
