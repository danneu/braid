# Fail Closed After Failed Unlock

## Summary

Make `braid unlock` and recovery mount paths fail closed when braid
opens LUKS mappers but does not successfully mount the pool. On failure,
braid closes only the mappers it **newly opened in this command** (not
ones it found already-owned at execute time), preserves the original
error as the primary error, and reports cleanup status.

`execute_mount_only` remains unchanged.

## Key Changes

### Ownership boundary at the LUKS helper

- Change `luks::ensure_luks_open` and `ensure_luks_open_with_key_file`
  from `Result<(), LuksError>` to `Result<OpenOutcome, LuksError>` with
  `enum OpenOutcome { Opened, AlreadyOwned }`.
- This is the only authority for the cleanup set. A mapper that was
  manually opened between plan and execute returns `AlreadyOwned` and is
  never closed by braid.
- Classify each existing call site and update it accordingly:
  - **Cleanup-aware (consume the variant):**
    - `mount.rs` `open_disks_with_passphrase` / `open_disks_with_key_file`
      -- push only `OpenOutcome::Opened` into the new `opened` out-param
      (see next section).
    - `add.rs` `ensure_luks_open` ([add.rs:769](cli/src/add.rs:769),
      [add.rs:927](cli/src/add.rs:927)) -- `LuksCleanupGuard::track` must
      run only when the call returns `OpenOutcome::Opened`. Today it
      tracks unconditionally after `Ok(())`, so an `AlreadyOwned` mapper
      gets closed by the guard's drop. The `OpenOutcome` change closes
      that pre-existing gap as a side effect; it is in scope here.
  - **Non-cleanup (discard the variant):** `replace.rs` and `recover.rs`
    `ensure_luks_open` sites have no cleanup guard around the call --
    they may use `let _ = ensure_luks_open(...)?;`.

### Track opened mappers and surface them on failure

- Inside `open_disks_with_passphrase` / `open_disks_with_key_file`, take
  an out-param `opened: &mut Vec<MapperName>` and push only
  `OpenOutcome::Opened` results.
- Change `execute_unlock_and_mount`'s failure return so callers can
  scope cleanup:

  ```rust
  pub struct UnlockAndMountFailure {
      pub error: MountError,
      pub opened_mappers: Vec<MapperName>,
  }
  pub fn execute_unlock_and_mount(...) -> Result<bool, UnlockAndMountFailure>;
  ```

  `Ok(true)` is unchanged. The function does NOT run cleanup itself --
  callers do, so recover can interleave the bootstrap btrfs probe.

### Shared cleanup primitives

- Move `close_mapper_with_retry` plus its retry-count and retry-delay
  constants out of `lock.rs` into a shared module. `lock.rs`,
  `mount::close_opened_mappers`, and `add.rs`'s `LuksCleanupGuard` all
  use this same close/retry body.
- Update `add.rs`'s `LuksCleanupGuard` to use the shared retrying close
  primitive while preserving add's existing policy: best-effort cleanup
  on drop, warning status rows only, no unlock-style trailing summary,
  and no replacement of the primary error.
- Add `mount::close_opened_mappers(runner, sleeper, fs, opened, color)`
  as the unlock/recover cleanup policy helper. It is built on the shared
  close/retry primitive and:
  1. Returns success silently when `opened` is empty. Pre-open failures
     (wrong passphrase, verification rejection) reach this helper with
     nothing to close, and the trailing summary line must not fire in
     that case.
  2. Runs scoped `btrfs device scan --forget <paths>` for the subset of
     `opened` whose `/dev/mapper/X` still exists. Same scoping rule and
     warn-on-failure behavior as `cmd_lock`'s post-umount forget step.
     Skip if the subset is empty.
  3. Closes each mapper via the shared retry-on-exit-5 helper.
  4. Attempts every mapper before reporting -- a busy/failed close on
     disk1 does not skip disk2.

### Per-caller failure policy

- `cmd_unlock`: on any `Err(UnlockAndMountFailure)`, invoke
  `close_opened_mappers`, then return the original error.
- `execute_recover_initial_open`:
  - For the bootstrap `MountFailed` arm (no pre-membership, Add op),
    run the existing `BtrfsFilesystemShowTarget` probe against
    `/dev/mapper/{name}` FIRST (the mappers must still be open for the
    probe to see LUKS contents), THEN invoke `close_opened_mappers`,
    THEN return the bootstrap-or-mount error.
  - For all other failure arms, invoke `close_opened_mappers`
    immediately.
- `recover_remount_cycle`: on any post-open failure, invoke
  `close_opened_mappers` for the mappers reopened by the cycle.

### Output shape

- Cleanup emits the same per-mapper `Wait` / `Ok` / `Fail` status lines
  as `cmd_lock`.
- After the close loop, print one trailing stderr line, independent of
  the returned `MountError`:
  - On success: `cleanup: closed LUKS mappers opened by this command.`
  - On failure: `cleanup failed: one or more LUKS mappers opened by this
    command could not be closed; run 'braid lock' after resolving the
    issue. First cleanup error: ...`
- The original error is what propagates as the command's exit message;
  cleanup output never replaces it.

### Docs

- Add a bullet to `docs/principles.md` under "3. Safe-by-construction
  operations" stating that failed `unlock` and recovery mount paths
  close only mappers braid newly opened during that invocation and
  never close pre-existing operator-owned mappers. This is the
  authoritative location for the invariant per `AGENTS.md` ("Design
  principles and invariants live in docs/principles.md").
- Update `docs/luks-unlock.md` with the operational detail: the
  fail-closed cleanup sequence (forget, then close), the trailing
  `cleanup:` summary lines, and the empty-`opened` no-op.

## Test Plan

### Rust unit tests in `mount.rs`

- Mount fails after two successful opens: both opened mappers closed,
  scoped `BtrfsDeviceScanForget` issued before close, error starts with
  `mount failed`.
- `BtrfsDeviceScanAll` fails after two successful opens: both opened
  mappers closed, error starts with `btrfs device scan failed`.
- One mapper is `AlreadyOwned` at execute time, one is `Opened`: only
  the `Opened` mapper is in the cleanup set. Pins the ownership filter,
  not the `to_unlock` plan list.
- disk1 opens, disk2 open fails: disk1 is closed and the disk2 open
  error remains primary.
- Cleanup close returns exit 5 through all retries on disk1 but disk2
  closes cleanly: cleanup attempts both, original failure is preserved,
  cleanup-failed guidance is appended.
- Wrong-passphrase rejection at credential verification (zero opens):
  no `BtrfsDeviceScanForget`, no `CryptsetupClose`, no trailing
  `cleanup:` line on stderr. Pins the empty-`opened` no-op.
- Keyfile path post-open failure: with `OpenCredential::KeyFile` and
  two successful keyfile opens, mount (or `BtrfsDeviceScanAll`) fails
  and cleanup issues scoped `BtrfsDeviceScanForget` followed by
  `CryptsetupClose` for both opened mappers. Pins that the keyfile arm
  uses the same opened-mapper tracking and cleanup path as passphrase
  -- a passphrase-only fix would still pass the other tests.
- `BtrfsDeviceScanForget` returns nonzero during cleanup: warning is
  emitted, every `CryptsetupClose` still runs, original mount/open
  error is preserved as primary. Pins the warn-and-continue policy
  inherited from `cmd_lock` against an accidental `?` short-circuit.

### Rust unit tests in `recover.rs`

- Bootstrap `MountFailed` runs the btrfs probe BEFORE cleanup, then
  closes opened mappers before returning bootstrap instructions. Pin
  probe-then-close ordering with a MockRunner sequence assertion.
- Bootstrap mount failure with existing btrfs superblock returns the
  original mount error and closes opened mappers.
- Recover remount-cycle mount failure closes mappers reopened by the
  cycle.

### Rust unit tests in `add.rs`

- `LuksCleanupGuard` does NOT track an `AlreadyOwned` mapper: with one
  target whose disk was already-owned at execute time, an injected
  later-step failure must not produce a `CryptsetupClose` for that
  mapper. Pins the cleanup-aware reclassification.
- `LuksCleanupGuard` uses the shared retry-on-exit-5 close primitive:
  when cleanup close for an actually opened mapper returns exit 5 once
  and then succeeds, the guard retries before emitting the final cleanup
  status row. Pins that add and unlock share close mechanics even though
  their caller-level policies differ.

### VM tests

- Update `recover-bootstrap-crash` so the mapper is expected to be
  closed after the recover error.
- Extend `braid-unlock.py` with a destructive final subtest: corrupt or
  wipe the btrfs signature inside a manually opened mapper, close it,
  run `braid unlock`, assert nonzero exit, `mount failed` in stderr,
  cleanup status in stderr, and no `/dev/mapper/braid-*` remains open.

### Verification commands

- `just test-rust`
- `just test-vm braid-unlock recover-bootstrap-crash`

## Out of Scope

`replace` and `recover` are non-cleanup `ensure_luks_open` callers and
get only the mechanical `OpenOutcome` discard. No new fail-closed mount
cleanup is added to those flows in this plan beyond the recover-initial
and remount-cycle policies already specified above.

`add` is reclassified as cleanup-aware (above) -- not out of scope.

## Assumptions

- Cleanup is best-effort but mandatory to attempt after braid opens any
  mapper and then fails before a successful mount.
- Cleanup failures do not replace the original error; they are appended
  as secondary stderr guidance.
- No new CLI flags are added.
- No compatibility path is needed.
