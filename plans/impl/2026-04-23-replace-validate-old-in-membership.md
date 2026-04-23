# Fix: cmd_replace does not verify --old exists in pool.json

## Context

`cmd_replace`'s Missing path can silently write an orphan `pool.json` when the
operator typos `--old`. `resolve_replace_source` validates against btrfs state
only -- it never consults `pool.json`. Then `build_replacement_membership`
calls `next.disks.remove(old_name)`, which is a silent no-op for an absent
key, and inserts the new name. The next `braid unlock` iterates
`membership.disks` (`mount::plan_open_pool`), finds the stale old entry as an
`Absent` member, and trips `DegradedRefused`. The operator is now stuck in
recovery until `pool.json` is hand-edited.

`remove_missing` already solves the analogous case: `resolve_removal_target`
(cli/src/remove_missing.rs:44-55) errors with
`"devid {devid} not found in pool.json membership -- ..."`. `cmd_replace`
should fail with the same shape, but earlier -- before the inhibitor and
before any journal write.

## Fix

The unsafe primitive is the silent `.remove` inside
`build_replacement_membership` (cli/src/replace.rs:753). Fold the validation
into that transform so the function that owns the silent no-op is the function
that rejects it. Then move the call earlier in `cmd_replace` -- the transform
is pure, so running it before the inhibitor is free, and an Err return aborts
cleanly with no journal and no inhibitor held.

### Critical files

- `cli/src/replace.rs`
  - **`build_replacement_membership`** (cli/src/replace.rs:746-761):
    - New signature: take `replace_source: &ReplaceSource` in addition to the
      existing args, so the Missing-path devid cross-check can happen inside.
    - New body, in order:
      1. Look up `old_name` in `existing.disks`. If absent ->
         `Err(ReplaceError::Validation(...))` naming the missing name.
      2. If `replace_source` is `ReplaceSource::Missing { devid }`, require
         `existing.disks[old_name].devid == Some(*devid)`. On mismatch ->
         `Err(ReplaceError::Validation(...))` naming both devids.
      3. Existing body: clone, `.remove(old_name)` (now guaranteed to hit),
         `validate_no_conflicts`, insert new member.
  - **`cmd_replace`** (cli/src/replace.rs:56-254):
    - Move the `pre_membership` load + `build_replacement_membership` call
      **up**: insert them immediately after `resolve_replace_source`
      (cli/src/replace.rs:102-109) and **before** the sleep inhibitor block
      at cli/src/replace.rs:230-237. This matches the existing inline rule at
      cli/src/replace.rs:224-229 ("reversible preflight before inhibitor").
    - In the current journal block (cli/src/replace.rs:240-254): drop the
      local `load_membership` and `build_replacement_membership` calls and
      reuse the values computed above. `journal::build_journal` +
      `journal::write_journal` stay where they are, still inside the
      inhibitor scope.

### Error messages

Both messages use `--` (per `CLAUDE.md` CLI output style), not em-dash. Both
return `ReplaceError::Validation(String)`, matching
`RemoveMissingError::Validation` at cli/src/remove_missing.rs:50-53.

**Missing-key (both paths):**
```
'{old_name}' not found in pool.json membership -- no disk entry has this
name. Pool membership may need manual repair.
```

**Devid mismatch (Missing path only):**
```
--old '{old_name}' records devid {pool_devid:?} in pool.json, but btrfs
reports missing devid {resolved_devid}. --old and --missing-id disagree about
which member is being replaced.
```

`{pool_devid:?}` handles `None` without a branch.

### Revised sequencing in `cmd_replace`

1. Preflight, `probe_pool`, parse new spec, `--old == --new` guard
   (cli/src/replace.rs:61-98, unchanged).
2. `resolve_replace_source` (cli/src/replace.rs:102-109, unchanged).
3. **MOVED:** `membership::load_membership` + `build_replacement_membership`
   (hoisted from cli/src/replace.rs:240-243). Err returns here leave zero
   state: no journal, no inhibitor, no disk mutation.
4. Probe `--new`, compile steps, dry-run branch, confirm, read passphrase,
   reversible new-disk checks (cli/src/replace.rs:111-215, unchanged).
5. Sleep inhibitor acquisition (cli/src/replace.rs:230-237, unchanged).
6. `journal::build_journal` + `journal::write_journal` reuse `pre_membership`
   and `target_membership` from step 3 (cli/src/replace.rs:244-254 minus the
   deleted lines).
7. Irreversible disk ops (cli/src/replace.rs:256+, unchanged).

## Verification

### Regression test: `cmd_replace` command-level (required)

The primary regression test must exercise the real `cmd_replace` path so that
deleting or bypassing the guard at the call site trips it. Model after
`journal_survives_replace_failure` at cli/src/replace.rs:1473-1553, which
already uses `ReplaceMockFs`, `ReplaceParams`, and
`crate::inhibit::RecordingInhibitor`.

Test: `cmd_replace_missing_path_rejects_old_name_absent_from_membership`.

Fixture:
- `PoolMembership` saved with only `disk1` present (no `disk2`). This is the
  typo scenario: operator types `--old disk2` but pool.json does not know
  about `disk2`.
- Runner that drives `resolve_replace_source` down the Missing branch:
  `probe_pool` reports `missing_count > 0` with a missing devid, and
  `probe_missing_devids` returns that devid. If `FailingReplaceRunner` does
  not already supply these mocks, add a new minimal runner alongside it --
  do not extend `FailingReplaceRunner`, keep it scoped to its existing test.
- `missing_id: Some(<that devid>)`, `old_name: "disk2"`, `dry_run: false`,
  `yes: true`.

Assertions:
- `matches!(result, Err(ReplaceError::Validation(_)))`. Typed variant only
  (per `feedback_assert_typed_error_shape_not_substrings.md`).
- `inhibitor.acquire_count() == 0`. Pins that validation happens before the
  inhibitor seam; matches the positive-direction assertion used in
  `journal_survives_replace_failure` at cli/src/replace.rs:1548-1552.
- `journal::load_journal(&paths).unwrap().is_none()`. Pins that no journal is
  written; matches `dry_run_does_not_acquire_inhibitor` at
  cli/src/replace.rs:1624-1627.

**This test fails when the fix is reverted:** whether the revert strips the
guard, moves it after the inhibitor, or drops the call from `cmd_replace`,
at least one of the three asserts above flips.

### Unit tests: `build_replacement_membership` (supporting)

Pin the transform's rejection branches directly -- cheap and fast:

- Missing-path, `old_name` absent in membership -> `Err(Validation(_))`.
- Missing-path, `old_name` present but `devid` mismatches resolved devid
  -> `Err(Validation(_))`.
- Live-path, `old_name` absent in membership -> `Err(Validation(_))`.
- Happy: Missing-path, `old_name` present with matching devid -> `Ok(_)`.
- Happy: Live-path, `old_name` present -> `Ok(_)`.

### Manual check

- `just test-rust` -- runs the new tests and the existing
  `resolve_replace_source` suite at cli/src/replace.rs:1204-1295.
- `cargo build` clean.

### Out of scope

- No VM / repro test. The fix is an in-process precondition check with no
  system-call surface; the command-level Rust test above is proportionate and
  fails on revert.
- No change to `ReplaceError` enum shape. `Validation(String)` stays,
  matching `RemoveMissingError::Validation` usage. A dedicated variant would
  add surface area without improving the typed-variant assertion in the
  regression test.
- No change to `build_replacement_membership`'s existing
  `validate_no_conflicts` call -- it must stay on the post-remove,
  pre-insert membership, matching current code at cli/src/replace.rs:753-757.
  Validating after the insert would let `insert(new_name, ...)` overwrite an
  existing member before conflict detection. The new preconditions run ahead
  of the `.remove`, so this ordering is preserved.
