# Plan: enrich post-enroll LUKS header-backup failure messages

## Context

When `luks::backup_luks_header_to` (or its `backup_luks_header` wrapper) fails after a LUKS slot has just been mutated -- `luks::enroll_key_file` for `enroll`, `luks_format` (with optional keyfile enroll) for `add` and `replace` -- today's error gives the user no signal that:

1. The slot mutation step completed before the backup attempt. The user need not re-enroll or re-format on retry.
2. The local backup is documented (`docs/luks-unlock.md:118-137`) as a transient byproduct the user is expected to copy off-system and delete. The user can capture an off-system backup directly with `cryptsetup luksHeaderBackup` and skip the local file entirely.

The error today is a bare `LuksError::Validation("LUKS header backup failed (exit N): <stderr>")` propagated via `?` (`cli/src/luks.rs:470-474`). The user sees the cryptsetup stderr and nothing else.

The pattern recurs at three callsites with identical post-slot-mutation framing:

- `cli/src/enroll_key_file.rs:307` -- `apply_enrollment`, after `luks::enroll_key_file` succeeds.
- `cli/src/add.rs:905` -- fresh-add path, after `luks_format` and optional keyfile enroll.
- `cli/src/replace.rs:488` -- replace path, after the new disk's `luks_format` and optional keyfile enroll.

Surfaced during a `verify-issue` review of a code-review finding that misframed the local backup as a recovery artifact and proposed re-creating it on `AlreadyEnrolled` re-runs. Filed issue [#62](https://github.com/danneu/braid/issues/62) tracks the larger off-system refactor that would obviate this code path; this plan is the small adjacent improvement that ships now.

## Approach

Add one private helper and one public wrapper in `cli/src/luks.rs`. Route the three callsites through the wrapper. No callsite logic changes; no error-type changes.

### Helper: `header_backup_failure_message`

Private to `cli/src/luks.rs`. Single-source-of-truth for the wording so all three commands produce identical output.

```rust
/// Compose the post-mutation header-backup failure message.
/// Single source of truth for the wording so the three commands that
/// can hit this failure mode (enroll, add, replace) emit identical output.
fn header_backup_failure_message(device: &str, underlying: &LuksError) -> String {
    format!(
        "LUKS header backup failed after the LUKS mutation completed: {underlying}\n\
         To capture an off-system header backup directly, run:\n  \
         cryptsetup luksHeaderBackup --header-backup-file <off-system path> {device}"
    )
}
```

The wording claims only the proven sequence (mutation step ran before the backup step failed). It does not claim the disk is currently healthy: `backup_luks_header_to` can fail because the device disappeared, the header became unreadable, or local I/O failed, and we cannot distinguish those from where we sit.

### Wrapper: `backup_luks_header_post_mutation`

`pub` in `cli/src/luks.rs`, parallel to the existing `backup_luks_header` (`cli/src/luks.rs:484-491`). Replaces the three direct calls.

```rust
/// Back up the LUKS header at a callsite where a LUKS slot has just been
/// mutated (luksFormat or luksAddKey). Wraps any error with the proven
/// sequence ("the LUKS mutation completed before the backup failed") and
/// a direct off-system remediation. Does not claim the disk is currently
/// healthy. Invariant: only call after a successful slot mutation, so the
/// "after the LUKS mutation completed" framing is accurate.
pub fn backup_luks_header_post_mutation<R: CommandRunner>(
    runner: &R,
    device: &str,
    mapper: &str,
    paths: &StatePaths,
) -> Result<PathBuf, LuksError> {
    backup_luks_header(runner, device, mapper, paths).map_err(|e| {
        LuksError::Validation(header_backup_failure_message(device, &e))
    })
}
```

Returning `LuksError::Validation` lets each command's existing `#[from] LuksError` impl carry it unchanged. The user-facing message surfaces as:

```
luks error: LUKS header backup failed after the LUKS mutation completed: <underlying>
To capture an off-system header backup directly, run:
  cryptsetup luksHeaderBackup --header-backup-file <off-system path> <device>
```

### Callsite changes

1. **`cli/src/enroll_key_file.rs`** (`apply_enrollment` at line 269)
   - Drop the `let backup_dir = paths.luks_headers_dir();` at line 277 (no other reader after the swap).
   - Lines 306-307: replace `luks::backup_luks_header_to(runner, &by_id.0, &mn.0, &backup_dir)` with `luks::backup_luks_header_post_mutation(runner, &by_id.0, &mn.0, paths)`.

2. **`cli/src/add.rs`**
   - Line 11 `use` list: swap `backup_luks_header` for `backup_luks_header_post_mutation`.
   - Line 905: replace `backup_luks_header(runner, &target.by_id.0, &target.mapper_name, params.paths)` with `backup_luks_header_post_mutation(...)` (same args).

3. **`cli/src/replace.rs`**
   - Line 10 `use` list: swap `backup_luks_header` for `backup_luks_header_post_mutation`.
   - Line 488: replace `backup_luks_header(runner, &new_by_id.0, &new_mn.0, params.paths)` with `backup_luks_header_post_mutation(...)` (same args).

After the swap, the existing `backup_luks_header` public wrapper is still used by `cli/src/recover.rs:2194` and `cli/src/recover.rs:2487`; do not delete it.

## Out of scope

- **`cli/src/recover.rs:2194` and `cli/src/recover.rs:2487`.** Recovery already supplies its own framing ("resuming a journaled add/replace; backup step failed"), and `RecoverError` lacks a free-form `Validation(String)` variant matching the other commands, so wrapping there would touch broader error infrastructure. Re-evaluate only if recovery wording is observed to confuse users in practice.
- **Modifying `backup_luks_header_to` at the source.** Rejected because the "post-mutation" framing is callsite-context. The recovery callsites intentionally let `LuksError` flow through unchanged.
- **Changing `LuksError::Validation` body in `backup_luks_header_to`.** Not needed -- the wrapper renders all `LuksError` variants (`Validation`, `Io`, etc.) uniformly via `Display`.
- **Issue #62's larger off-system refactor.** Tracked separately. Until that ships, this plan delivers the actionable improvement; once it ships, this code path may be removed entirely.

## Tests

### `cli/src/luks.rs` `#[cfg(test)] mod tests`

1. **`header_backup_failure_message_includes_device_and_remediation`** -- composition unit test. Build a `LuksError::Validation("LUKS header backup failed (exit 1): No space left on device")`, call the helper, assert the returned string contains all of:
   - The underlying error's Display output verbatim.
   - The exact device path argument.
   - The literal `cryptsetup luksHeaderBackup --header-backup-file` substring.
   - The literal `after the LUKS mutation completed` substring.
   - Negative assertion: does **not** contain the substring `intact` (regression guard against re-introducing the over-strong "disk is intact" wording).

2. **`backup_luks_header_post_mutation_wraps_error_on_failure`** -- integration test against the wrapper. Use `MockRunner::default().with_output(...)` to make `CryptsetupLuksHeaderBackup` return `RawCommandOutput { exit_status: 1, stderr: "No space left on device".into(), ... }`. Call `backup_luks_header_post_mutation` with a tempdir-backed `StatePaths`. Assert the returned `Err(LuksError::Validation(msg))` body contains both the underlying stderr and the cryptsetup remediation.

3. **`backup_luks_header_post_mutation_passes_through_success`** -- happy-path regression: stub `CryptsetupLuksHeaderBackup` with `exit_status: 0`, assert `Ok(path)` with the same path the underlying `backup_luks_header` would have returned. Guards against accidental wrapping that hides the success value.

### `cli/src/enroll_key_file.rs` test module

4. **`apply_enrollment_returns_enriched_error_when_backup_fails`** -- mirror the `apply_enrolls_needs_enroll_items` fixture at `cli/src/enroll_key_file.rs:2428` for one disk: stub the keyfile-enroll happy-path commands, then stub `CryptsetupLuksHeaderBackup` with `exit_status: 1, stderr: "No space left on device"`. Note that `apply_enrollment` now takes `passphrase: &Passphrase` (`cli/src/enroll_key_file.rs:272`, with `Passphrase` imported from `crate::secret` at line 11); the new test must follow the current fixture's `Passphrase` construction, not the older `&str` shape. Assert the returned `EnrollKeyFileError` Display contains:
   - `cryptsetup luksHeaderBackup --header-backup-file` (the remediation).
   - The disk's by-id path.
   - `after the LUKS mutation completed` (proves the wrapper, not the raw `backup_luks_header_to`, is in the call path).

### `cli/src/add.rs` and `cli/src/replace.rs` test modules (required, no fallback)

5. **`add_returns_enriched_error_when_post_format_backup_fails`** in `add.rs`.
6. **`replace_returns_enriched_error_when_post_format_backup_fails`** in `replace.rs`.

These are required regression tests: the helper / wrapper tests cannot detect a callsite that reverts to `backup_luks_header` (or `backup_luks_header_to`), so each callsite needs its own assertion that the enriched message reaches the command's returned error.

Each: smallest fresh-add / replace fixture, MockRunner stubs the full `luksFormat` -> (optional) `luksAddKey` happy path, then fails `CryptsetupLuksHeaderBackup` with `exit_status: 1, stderr: "No space left on device"`. Assert the returned error Display contains:
- `cryptsetup luksHeaderBackup --header-backup-file` (the remediation).
- `after the LUKS mutation completed` (proves the wrapper, not the raw `backup_luks_header`, is in the call path).

If a fixture needs scaffolding to reach the post-format point, scaffold it; do not skip these tests.

All preambles follow the project's three-section convention (`docs/testing.md`).

## Verification

- `just test-rust` -- runs the new unit/integration tests.
- Optional manual stderr spot-check: in any VM test that runs `braid enroll`, inject a fault by filling `/var/lib/braid` to capacity (`dd if=/dev/zero of=/var/lib/braid/.fill bs=1M` until ENOSPC), then run `braid enroll <dir>` and confirm the new wording on stderr. Skip unless the message wording feels off after the test pass.

No new VM tests required: the failure path is fully covered at the unit level, and message wording can be iterated cheaply without VM cycles.

## Files modified

- `cli/src/luks.rs` -- add helper + wrapper + 3 unit tests.
- `cli/src/enroll_key_file.rs` -- swap callsite, drop unused local, add 1 test.
- `cli/src/add.rs` -- swap `use` and callsite, add 1 test.
- `cli/src/replace.rs` -- swap `use` and callsite, add 1 test.
