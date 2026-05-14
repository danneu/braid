# Plan: list enroll UUID-mismatch tests in decision-024 enforcement list

## Context

Commit `af7d140` ("fix(enroll): reject luks uuid mismatches before
enrollment") added both a VM test (`tests/cli/enroll-uuid-mismatch.py`)
and a Rust unit test
(`cli/src/enroll_key_file.rs::discover_rejects_luks_uuid_mismatch_before_slot_inventory`)
that enforce the same swap/reformat invariant as the existing
`unlock-uuid-mismatch.py` and `recover-replace-existing-luks-uuid-mismatch.py`
tests. But the commit did not update
`docs/decisions/024-luks-uuid-identity.md`'s "Tests That Enforce This"
section, so that list still omits enroll coverage entirely.

A reviewer reading just decision-024 cannot tell the enroll guard
exists, and the gap already produced one stale "missing coverage"
finding in this session's verify-issue. Closing the loop in the
canonical enforcement list prevents the same false positive from
recurring.

Per the verify-issue exploration, decision-024 is the **sole** doc that
enumerates the UUID-mismatch enforcement tests as a structured list --
no other decision, README, or design note maintains a parallel
inventory. Scope of this change is one file.

## Scope

One file: `docs/decisions/024-luks-uuid-identity.md`. No code, no test,
no plan-file, no other doc.

## Edits

### 1. Add a new Rust-test bullet for `cli/src/enroll_key_file.rs`

Insert immediately after the `cli/src/recover.rs` bullet (currently the
last Rust-test bullet, lines 159-161 in the current file). Keep the
existing wording style ("`cli/src/<file>` unit tests verify ...").

New bullet:

```markdown
- `cli/src/enroll_key_file.rs` unit tests verify standalone enroll
  rejects a member whose live LUKS UUID does not match the pool.json
  membership key before any slot inventory or keyfile mutation runs.
```

This describes what the new
`discover_rejects_luks_uuid_mismatch_before_slot_inventory` test pins:
the guard fires in discovery, before `CryptsetupLuksDump` (slot
inventory) and `CryptsetupLuksAddKeyFile` (mutation) -- which is
strictly earlier than the `CryptsetupTestPassphrase` bound the
original review finding asked for.

### 2. Expand the combined VM-test bullet on lines 164-166

The current combined bullet:

```markdown
- `tests/cli/unlock-uuid-mismatch.py` and
  `tests/cli/recover-replace-existing-luks-uuid-mismatch.py` verify swapped or
  reformatted disks fail UUID re-checks before unsafe replay or mount.
```

Becomes:

```markdown
- `tests/cli/unlock-uuid-mismatch.py`,
  `tests/cli/enroll-uuid-mismatch.py`, and
  `tests/cli/recover-replace-existing-luks-uuid-mismatch.py` verify
  swapped or reformatted disks fail UUID re-checks before unsafe
  replay, slot enrollment, or mount.
```

Keeps the existing "fail UUID re-checks before X, Y, or Z" structure
and groups the new test with the other swap/reformat siblings rather
than introducing a stranded one-test bullet.

## Why not a separate bullet for the enroll VM test

The three swap/reformat VM tests are one family: same scenario shape
(reformat one disk behind pool.json), same canonical `"LUKS UUID
mismatch"` wording, same enforcement direction (refuse before
mutation). Splitting `enroll-uuid-mismatch.py` into its own bullet
would imply it's a different invariant than `unlock` and `recover`.
It is not.

## Why not also touch the "Benefits" section

`docs/decisions/024-luks-uuid-identity.md` lines 61-63 already credit
the migration with "Earlier clone and swap detection ... UUID
mismatches catch disks that were swapped, cloned, or reformatted after
the original plan was made." That sentence is correct as written
post-`af7d140` -- enroll is now in scope. No edit needed.

## Critical files

- `docs/decisions/024-luks-uuid-identity.md` (only file edited)
  - After current line 161 (`cli/src/recover.rs` bullet): insert new
    Rust-test bullet for `cli/src/enroll_key_file.rs`.
  - Replace current lines 164-166 with the expanded combined VM-test
    bullet.

## Verification

This is a doc-only change. No code, test, or build artifact is
affected. Verification is two reads:

1. `git diff docs/decisions/024-luks-uuid-identity.md` -- all changes
   confined to the "Tests That Enforce This" section. The two edits
   (new Rust-test bullet and the expanded combined VM-test bullet) sit
   close enough together that default `git diff` context will likely
   coalesce them into a single hunk; that is fine. The check is on the
   section, not on the hunk count.
2. After the edit, `grep -n "enroll" docs/decisions/024-luks-uuid-identity.md`
   should return matches only inside the "Tests That Enforce This"
   section -- one hit in the new `cli/src/enroll_key_file.rs` bullet
   ("standalone enroll") and two hits in the expanded combined VM-test
   bullet (the filename `enroll-uuid-mismatch.py` and the prose "slot
   enrollment"). No hits in any other section of the doc.

No `just` or `cargo` invocation is required for a Markdown text edit
to a decision record.
