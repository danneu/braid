# Dedup the generated-keyfile no-overwrite check (enroll --generate)

## Context

`braid enroll --generate DIR` creates `DIR/braid.key`. The no-overwrite
guarantee is currently enforced three times in one run:

1. **Plan-time** `key_file_path.exists()` check -- a friendly early error
   (`braid.key already exists ... drop --generate`) before discovery/passphrase.
2. **Mutation-boundary** `exists()` re-check -- redundant.
3. **Atomic** `OpenOptions::create_new(true)` in `generate_key_file` -- the real
   fail-closed guard.

Layers 1 and 2 are the *same* call: `validate_generated_keyfile_target(.., phase)`
ends with an unconditional `validate_key_file_path(key_file_path, true)`
(`cli/src/enroll_key_file.rs#validate_generated_keyfile_target`, final line), reached
at plan time via `plan_enroll` (`KeyfileTargetPhase::Plan`) and again at the
boundary via `EnrollPlan::execute` (`KeyfileTargetPhase::Recheck`), immediately
before `generate_key_file`.

The boundary re-check sits microseconds before `create_new(true)`, so it adds **no**
TOCTOU protection -- only a friendlier error string. The documented invariant it
shares the function with (`docs/internals/luks-unlock.md#keyfile-creation-target-invariant`)
covers only the **mountpoint/directory gate**, which legitimately must run at both
phases. The doubled no-overwrite check forces a reader to trace both `KeyfileTargetPhase`
arms to learn the existence check is load-bearing only once.

**Outcome:** the no-overwrite check runs once at plan time (friendly early error)
and is enforced at the boundary by the atomic `create_new(true)` -- whose
`AlreadyExists` is mapped to the same friendly message, so behavior (including the
rare mid-run race) is fully preserved while the redundancy is gone.

## The fix (A + C)

All edits are in `cli/src/enroll_key_file.rs`.

### Change 1 -- extract the message helper

Pull the inline `braid.key already exists at {} ... drop --generate ...` string out
of `validate_key_file_path`'s `generate` branch into a private helper, reusing the
existing `key_file_directory`:

```rust
/// Friendly no-overwrite message for `enroll --generate`. Shared by the
/// plan-time existence check and the mutation-boundary `create_new(true)`
/// failure so both render byte-identically and both surface the
/// "drop --generate and re-run" recovery hint.
fn format_keyfile_already_exists(key_file_path: &Path) -> String { ... }
```

`validate_key_file_path` (generate branch) calls it. Models naming/doc style on the
existing `partial_generate_recovery_message`. (Justified independently by Change C,
which needs the same string -- avoids duplicating it.)

### Change A -- gate the existence check to plan time

In `validate_generated_keyfile_target`, replace the trailing unconditional
`validate_key_file_path(key_file_path, true)` with a phase-gated check plus an
explanatory comment, leaving the mountpoint/directory gate untouched at both phases:

```rust
// No-overwrite is a plan-time friendly early error. At the mutation boundary
// generate_key_file's create_new(true) is the atomic guard; re-checking
// exists() here adds no protection (it races create_new) and only duplicates
// this message.
if let KeyfileTargetPhase::Plan = phase {
    validate_key_file_path(key_file_path, true)?;
}
Ok(())
```

Update the function's doc comment to state it is the mount-target gate (both phases)
plus the plan-only no-overwrite check. Keeping the check **inside** the helper
preserves the mountpoint-before-existence ordering structurally -- the mount gate (the
root-fs-leak guard) always wins when a dir is both unmounted and already holds a stale
`braid.key`. (This is why we do *not* hoist the check out to `plan_enroll`; that would
make the ordering reorderable.)

### Change C -- preserve the friendly message at the boundary

In `EnrollPlan::execute`, map `generate_key_file`'s error instead of propagating it raw
(mirrors the established `luks::validate_user_keyfile_path` NotFound mapping):

```rust
generate_key_file(params.key_file_path).map_err(|e| match e.kind() {
    std::io::ErrorKind::AlreadyExists => {
        EnrollKeyFileError::Validation(format_keyfile_already_exists(params.key_file_path))
    }
    _ => EnrollKeyFileError::Io(e),
})?;
```

**Do not** change `generate_key_file`'s signature (it stays `-> io::Error`) so its
direct unit tests still assert `ErrorKind::AlreadyExists`.

## Explicitly not doing

- **Not** pulling the existence check out to `plan_enroll` (the "B" variant):
  splitting it into a separate sequential guard introduces a mount-before-existence
  reordering hazard that Change A avoids, for no benefit. More refactor than a
  Low-severity simplicity finding warrants.
- **Not** touching the `generate: bool` parameter shape of `validate_key_file_path`
  (used by `add`/`replace`/non-generate enroll) -- out of scope.

## Tests

Existing tests that must stay green (no edits expected):

- `generate_rejects_existing_keyfile_after_mountpoint_check` -- plan-time existence
  error + `requests() == [MountpointCheck]`. Preserved (mount gate then plan-only
  existence; message bytes unchanged via Change 1).
- `cmd_generate_mountpoint_revoked_between_plan_and_write` -- exactly 2 MountpointCheck
  calls + ordering. Preserved (Change A drops only the *existence* sub-check, not the
  mountpoint probe).
- `generate_rejects_existing_keyfile`, `generate_key_file_create_new_rejects_existing`
  -- assert `ErrorKind::AlreadyExists` from `generate_key_file` directly. Unaffected
  (signature unchanged).
- VM `tests/cli/braid-enroll-generate.py` Test 3 -- file pre-exists, asserts
  "braid.key already exists" + "drop --generate". Caught at plan time; preserved.

One new unit test (pins Change C -- a currently-untested live path):

- **`execute_generate_existing_keyfile_at_boundary_reports_friendly_error`** (or
  similar). Build an `EnrollPlan` directly -- clone the skeleton of
  `execute_generate_partial_failure_reports_recovery_hint` (single disk, plan built by
  hand, no discovery mocks). Pre-create `braid.key` *before* calling `execute` so it is
  absent during plan construction but present when `generate_key_file` runs. Mocks:
  `luksUuid` (reprobe), `test-passphrase` ok, `luksDump` slot-1-empty, mountpoint OK
  (Recheck). Assert the result is `EnrollKeyFileError::Validation(msg)` -- match the
  variant (not just a substring) to pin the remap away from `Io` -- and assert the
  **full** friendly message, not just its opening clause. Mirror the substrings the
  plan-time test `generate_rejects_existing_keyfile_after_mountpoint_check` already
  pins: `msg.contains("braid.key already exists")`, ``msg.contains("drop `--generate`")``,
  and `msg.contains(&format!("braid enroll {}", dir.display()))` (the retry command),
  plus `!msg.contains("I/O error")`. Rationale: asserting only `braid.key already exists`
  would stay green even if the boundary `map_err` produced a truncated `Validation`
  message missing the `drop --generate` recovery hint -- exactly the regression Change C
  exists to prevent. (Equivalently, `assert_eq!(msg, format_keyfile_already_exists(&kf))`
  is an acceptable stronger pin now that Change 1 makes the helper callable from the test.)

No mount-before-existence test is needed: Change A keeps that ordering inside the
helper, so it cannot regress.

## Docs

No doc edits required:

- `docs/commands/enroll.md` describes the no-overwrite as plan-time user-facing behavior
  ("does not already contain braid.key", "drop --generate") -- still accurate (plan-time
  check preserved; boundary still fails closed with the same message).
- `docs/internals/luks-unlock.md#keyfile-creation-target-invariant` is mount-gate-only --
  Change A makes `validate_generated_keyfile_target` match that framing more closely;
  still accurate.

## Verification

```
just test-rust                       # unit tests (new + existing enroll tests)
just test-vm braid-enroll-generate   # end-to-end: Test 3 no-overwrite path
```

ASCII-output check (`scripts/docs/check-output-ascii.py`) is satisfied -- the message is
unchanged ASCII. `just docs-build` is unaffected (no doc edits) but harmless to run.
