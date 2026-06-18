# Plan: harden both `cmd_unlock` absent-passphrase sentinel tests

## Context

`cmd_unlock` (`cli/src/unlock.rs#cmd_unlock`) has two unit tests that each prove a
branch correctly **skips credential resolution**, and each uses its `Ok(())` return
as the load-bearing witness that the passphrase file was never read:

1. `cmd_unlock_dry_run_skips_credential_resolution_with_disks_to_unlock` -- guards
   the dry-run gate. `cmd_unlock` returns at `if params.dry_run` before
   `UnlockPlan::execute`, the only path that calls
   `credential::resolve_credential` (`cli/src/credential.rs#resolve_credential`).
   This is ADR 022's side-effect-free preview contract
   (`docs/design/decisions/022-dry-run-preview-model.md`).
2. `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock` -- guards the
   empty-`to_unlock` branch. When every mapper is already open the pool needs no
   credentials, so `UnlockPlan::execute` (`cli/src/unlock.rs#UnlockPlan::execute`)
   takes the `if plan.to_unlock.is_empty()` mount-only path and never reaches
   `resolve_credential`. Runs with `dry_run: false`.

Both prove the skip the same way: pass a `passphrase_file` that does not exist and
assert `Ok(())`. If the guarded branch regressed and `resolve_credential` ran,
reading the absent passphrase file would error before `Ok(())` --
`resolve_credential` reads the passphrase **eagerly** via `luks::read_passphrase`
-> `read_file_into_zeroizing` (`cli/src/luks.rs`).

**The shared flaw.** Both derive the bogus path from the same hardcoded absolute
literal:

```rust
let (_state_dir, sp) = isolated_paths();
// ...
let bogus = std::path::PathBuf::from("/definitely/not/a/real/path/passphrase");
```

The "guaranteed absent" property depends on global host filesystem state. If that
literal ever existed as a readable file, a regressed branch would read it
successfully and the `Ok(())` witness would be **silently defeated** -- the test
would pass green while the credential-skip it protects was actually broken. Two
adjacent tests carry the identical weakness.

**The fix has a proven shape.** The keyfile sibling
`cmd_unlock_dry_run_skips_keyfile_validation_with_disks_to_unlock` (same file, added
in the prior change) anchors its bogus path under the per-test `isolated_paths()`
tempdir -- a child we never create inside a freshly-made empty directory is absent
independent of global FS state:

```rust
let (state_dir, sp) = isolated_paths();
// ...
let bogus = state_dir.path().join("missing-braid.key");
```

This plan brings both passphrase sentinels to that parity. No production change.

**Correction of record.** An earlier follow-up note (in the committed keyfile plan,
`plans/impl/2026-06-17-pin-unlock-dry-run-keyfile-gate.md`) called passphrase
resolution a lazy / lower-risk read. That is wrong: `resolve_credential` reads the
passphrase file eagerly (`luks::read_passphrase` -> `read_file_into_zeroizing`),
exactly as it stats the keyfile eagerly via `validate_user_keyfile_path`. Both
passphrase sentinels rely on that eager read for their witness.

## The change

Two tests edited in `cli/src/unlock.rs`, both surgical -- only the tempdir binding,
the bogus-path construction, and the explanatory comment change. Each test's
distinct setup (runner mocks, fs body, membership, params, assertions) stays
untouched. No production code changes.

For **each** test apply the same three edits:

1. **Bind the tempdir guard:** `let (_state_dir, sp) = isolated_paths();` ->
   `let (state_dir, sp) = isolated_paths();` so `.path()` is reachable.
   (`isolated_paths()` returns `(tempfile::TempDir, StatePaths)`,
   `cli/src/test_fixtures/doctor.rs#isolated_paths`; the `TempDir` exposes `.path()`.)
2. **Anchor the bogus path:**
   `let bogus = std::path::PathBuf::from("/definitely/not/a/real/path/passphrase");`
   -> `let bogus = state_dir.path().join("missing-passphrase");` (any descriptive
   never-created child name; reuse `missing-passphrase` in both for consistency).
3. **Refresh the inline comment** to explain the tempdir anchoring and name the
   test's own guarded branch. Keep `Ok(())` as the named witness; note the eager
   passphrase read.

### Test 1 -- `cmd_unlock_dry_run_skips_credential_resolution_with_disks_to_unlock`

Suggested comment:

```rust
// Nonexistent path anchored under the freshly-created, empty isolated_paths()
// tempdir: a child we never write is guaranteed absent independent of global
// filesystem state. resolve_credential reads the passphrase file eagerly
// (luks::read_passphrase), so if dry-run regresses and resolves the credential,
// the read of this absent path fails before Ok(()).
let bogus = state_dir.path().join("missing-passphrase");
```

Leave unchanged: the `result.expect(...)` message ("...without reading the
(nonexistent) passphrase file"), the zero-execute-commands denylist assertion plus
its comment, and the `Intent / Why it exists / Scenario` preamble. All already
accurate.

### Test 2 -- `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock`

Suggested comment:

```rust
// Nonexistent path anchored under the freshly-created, empty isolated_paths()
// tempdir: a child we never write is guaranteed absent independent of global
// filesystem state. resolve_credential reads the passphrase file eagerly
// (luks::read_passphrase), so if dispatch regresses and hoists resolve_credential
// above the empty-to_unlock check, the read of this absent path fails before Ok(()).
let bogus = state_dir.path().join("missing-passphrase");
```

Leave unchanged: the elaborate `MockRunner` / `mount_fs` setup, the
`result.expect(...)` message ("...take the mount-only branch and never attempt to
read the (nonexistent) passphrase file"), and the preamble.

## Explicitly out of scope (considered, rejected)

- **No production change.** `resolve_credential`, the dry-run gate, the
  empty-`to_unlock` branch, and the read path are untouched. Test-robustness fix only.
- **No shared helper / parameterized test.** The codebase keeps these tests as
  one-scenario-per-`#[test]` copies with distinct preambles and distinct setups; a
  surgical in-place edit to each matches project style.
- **No change to the keyfile sibling** -- already hardened; it is the reference.
- **No edit to the stale follow-up note** in the committed keyfile plan. It is a
  committed historical artifact; correcting it is separate, low-value docs churn. The
  accurate mechanic is recorded in this plan's Context instead.

## Files

- `cli/src/unlock.rs` -- edit the two tests named above. Reference sibling in the
  same file: `cmd_unlock_dry_run_skips_keyfile_validation_with_disks_to_unlock`. All
  fixtures and imports are already in scope (`isolated_paths`, `base_two_disk_runner`,
  `mount_fs`, `MockRunner`, `two_disk_membership`, `unlock_three_disk_membership`,
  `test_config`); the edits use only std (`state_dir.path().join(...)`). No new
  fixtures or imports.

## Verification

- Run each edited test by its exact name. `cargo test` accepts only one positional
  `[TESTNAME]` filter before `--`, so use two separate invocations (a single
  two-filter command errors with `unexpected argument`):
  - `cargo test -p braid-cli cmd_unlock_skips_credential_resolution_when_nothing_to_unlock`
  - `cargo test -p braid-cli cmd_unlock_dry_run_skips_credential_resolution_with_disks_to_unlock`

  Each full name matches only its own test. Do **not** use the bare
  `skips_credential_resolution` substring: it also catches
  `remove_missing_pool_mutation_recovery_skips_credential_resolution_when_all_mappers_open`
  in `cli/src/recover.rs`. Both edited tests pass after the edits.
- **Right-reason check, Test 1 (dry-run gate).** Temporarily insert
  `crate::credential::resolve_credential(params.passphrase_stdin, params.passphrase_file, params.key_file).map_err(MountError::from)?;`
  just before the `if params.dry_run` return in `cmd_unlock`
  (`cli/src/unlock.rs#cmd_unlock`); re-run Test 1 -- it MUST fail at
  `result.expect(...)` with a passphrase read error (the absent tempdir child cannot
  be read), proving `Ok(())` is the real witness. Revert.
- **Right-reason check, Test 2 (empty-`to_unlock` branch).** Temporarily insert the
  same `resolve_credential(...).map_err(MountError::from)?;` above the
  `if plan.to_unlock.is_empty()` check in `UnlockPlan::execute`
  (`cli/src/unlock.rs#UnlockPlan::execute`); re-run Test 2 (it has `dry_run: false`,
  so it reaches `execute`) -- it MUST fail at `result.expect(...)` with a passphrase
  read error. Revert. (Run only the targeted test for this check; the temporary hoist
  perturbs other execute-path tests until reverted.)
- `cargo test -p braid-cli` (or `just test-rust`) -- full crate suite stays green.
- ASCII-only: the edits touch only comments and the unchanged assertion messages
  (both exempt), so `scripts/docs/check-output-ascii.py` is a formality.
