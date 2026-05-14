# Fix: smartd_alert_active treats directories as active flags

## Context

`alert::smartd_alert_active` (`cli/src/alert.rs:277-279`) uses
`Path::exists()`, so any inode at `/var/lib/braid/smartd-alert` -- file,
directory, symlink-to-dir, FIFO -- counts as an active smartd alert.
The cleanup primitive `alert::remove_smartd_alert_flag`
(`cli/src/alert.rs:282-288`) uses `std::fs::remove_file`, which fails on
a directory with EISDIR (Linux) / EPERM (macOS).

The chain in `cleanup_alert_files_and_beeper` (`cli/src/ack.rs:178-190`)
runs `remove_smartd_alert_flag` first, so any non-file at the flag path
wedges `braid ack`: every retry re-runs `save_acked_stats`, hits the
same cleanup failure, and surfaces `AckError::CleanupFailed`. The error
message at `cli/src/ack.rs:255-259` directs the operator to "fix the
I/O error and re-run `braid ack`" -- which will not help, because the
root cause (a non-file at the flag path) is invisible to that
instruction.

The smartd hook is `touch /var/lib/braid/smartd-alert`
(`modules/braid/monitor.nix:24`), so this scenario only arises through
manual operator action, test scaffolding, or a future hook bug. But:

- Two existing unit tests
  (`cmd_ack_returns_cleanup_failed_when_remove_smartd_alert_errors_after_baseline_saved`,
  `cli/src/ack.rs:587-621`, and
  `ack_offline_cleanup_failure_after_missing_acked_returns_cleanup_failed`,
  `cli/src/ack.rs:840-870`) intentionally exploit this wedge by doing
  `std::fs::create_dir(paths.smartd_alert())` to inject a portable
  EISDIR/EPERM. The production surface and the test surface agree, but
  what they agree on is hostile to the operator.
- `monitor.rs:97` and `status.rs:524` also call
  `smartd_alert_active`, so a directory at the flag path produces a
  phantom `SmartdAlert` cause every monitor cycle and a phantom
  "smartd" line in `braid status`. Only `ack` actually breaks because
  it is the only call site that follows up with `remove_file`.

Outcome: `smartd_alert_active` should mean "the smartd hook fired"
(i.e. a regular file is present), not "any inode is present". With
that semantic, a stray directory matches the no-flag case across all
three call sites (ack, monitor, status) and `braid ack` is no longer
wedged. The cleanup-failed contract still needs coverage; the test
injector moves to a path later in the cleanup chain.

## Files to modify

- `cli/src/alert.rs` -- production fix (one function).
- `cli/src/ack.rs` -- two test injectors and their witnesses.

## Change 1: `cli/src/alert.rs:277-279`

Replace the `exists()`-based check with a regular-file check:

```rust
/// Check if the smartd alert flag file exists.
///
/// Treats only a regular file at the path as an active alert: a
/// directory or other non-file is ignored so a stray inode cannot
/// wedge `braid ack` (whose cleanup uses `remove_file`).
pub fn smartd_alert_active(paths: &StatePaths) -> bool {
    paths
        .smartd_alert()
        .metadata()
        .map(|m| m.is_file())
        .unwrap_or(false)
}
```

Notes:

- `Path::metadata()` follows symlinks, matching the prior `exists()`
  behavior for the legitimate symlink-to-regular-file case.
- A broken symlink: `metadata()` errors; we return `false`. Same as
  the prior `exists()` post Rust 1.63.
- Symlink to a directory: returns `false`, which is the new defensive
  outcome -- not an alert source.

## Change 2: swap test injector in two tests

The two cleanup-failed tests inject failure with
`std::fs::create_dir(paths.smartd_alert())`. With Change 1,
`smartd_active` becomes `false` and the cleanup chain never reaches
the failing `remove_file`. Move the injector to
`paths.alert_latch_corrupt()` -- the *last* `remove_*` in
`cleanup_alert_files_and_beeper` -- so all earlier cleanup steps run
and witnesses become unambiguous proofs of partial-apply.

### 2a. Mounted: `cli/src/ack.rs:587-621`

Replace the setup:

- Remove `std::fs::create_dir(paths.smartd_alert()).unwrap();`.
- Add `std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();`.

Update the witnesses:

- Keep `paths.acked_stats_json().exists()` (still durable before
  cleanup).
- Replace `paths.alert_latch_json().exists()` (true today, *false*
  after the swap) with:
  - `!paths.alert_latch_json().exists()` ("cleanup ran past
    `remove_alert_latch` before failing on the corrupt-sidecar
    `remove_file`").
  - `paths.alert_latch_corrupt().exists()` ("the directory that
    poisoned cleanup is still on disk -- proves where the chain
    failed").

Update the doc-comment scenario at `cli/src/ack.rs:580-586` to refer
to "a directory at the corrupt-latch sidecar path" instead of "the
smartd-alert path". Keep the EISDIR/EPERM portability note in the
inline comment.

### 2b. Offline: `cli/src/ack.rs:840-870`

Mirror the mounted change: swap the `create_dir` target and update
the same two witnesses + the scenario comment.

## Change 3: new regression test for `smartd_alert_active`

After Change 2 swaps both injectors off `paths.smartd_alert()`, no
test asserts the regular-file semantic directly. A regression that
reverts Change 1 back to `Path::exists()` would still leave
`just test-rust` green. Add a behavioral unit test in
`cli/src/alert.rs` (the `#[cfg(test)] mod tests` block at the bottom)
that pins all four boundary cases.

Gate the whole test with `#[cfg(unix)]`, matching the existing
symlink-adjacent test `quarantine_link_failure_surfaces_in_detail`
(`cli/src/alert.rs:738-773`). Use `std::os::unix::fs::symlink` for
the symlink case.

Follow the repo's `//` line-comment preamble form
(`docs/testing.md:11-22`); place it as a contiguous block directly
above the `#[cfg(unix)]` / `#[test]` attributes.

```rust
// Intent: smartd_alert_active treats only a regular file at the
//   flag path as an active alert source. Absent paths and
//   directories are false; a symlink resolving to a regular file is
//   true (matches the smartd hook's `touch` output, including
//   symlink-on-tmpfs deployments).
// Why it exists: prior behavior used Path::exists(), which counted
//   any inode -- including a directory -- as an active alert. The
//   subsequent cleanup (remove_smartd_alert_flag) calls remove_file,
//   which fails on a directory, so `braid ack` was permanently
//   wedged behind AckError::CleanupFailed any time a non-file ended
//   up at the flag path. This test fails loudly on a regression
//   back to Path::exists().
// Scenario: test scaffolding, a manual operator mistake, or a future
//   hook bug leaves a directory at /var/lib/braid/smartd-alert.
//   smartd_alert_active must report false so the ack cleanup chain
//   does not try to remove_file the directory and wedge subsequent
//   `braid ack` invocations.
#[cfg(unix)]
#[test]
fn smartd_alert_active_requires_regular_file() {
    let dir = tempfile::tempdir().unwrap();
    let paths = StatePaths::custom(dir.path().to_path_buf());

    assert!(
        !smartd_alert_active(&paths),
        "absent path must be false"
    );

    std::fs::write(paths.smartd_alert(), b"").unwrap();
    assert!(
        smartd_alert_active(&paths),
        "regular file must be true (matches smartd hook `touch` output)"
    );
    std::fs::remove_file(paths.smartd_alert()).unwrap();

    std::fs::create_dir(paths.smartd_alert()).unwrap();
    assert!(
        !smartd_alert_active(&paths),
        "directory must be false (regression guard for Path::exists revert)"
    );
    std::fs::remove_dir(paths.smartd_alert()).unwrap();

    let target = dir.path().join("real-flag");
    std::fs::write(&target, b"").unwrap();
    std::os::unix::fs::symlink(&target, paths.smartd_alert()).unwrap();
    assert!(
        smartd_alert_active(&paths),
        "symlink resolving to a regular file must be true"
    );
}
```

## Change 4: nothing else changes

- `monitor.rs:97` and `status.rs:524` get the corrected semantics for
  free; no monitor / status code edits needed. No new tests are
  warranted for these sites: a directory at the flag path is not a
  reachable state under any documented operator flow, the existing
  alert-state tests already cover the file / absent cases, and the
  fixed `smartd_alert_active` now collapses non-file inodes into the
  absent case at the only ingestion point.
- The test fixtures `OfflineFsThatTouchesSmartd` /
  `MountedFsThatTouchesSmartd` (`cli/src/test_fixtures/ack.rs:60-105`)
  already use `std::fs::write(..., b"")` to create a regular file, so
  they are unchanged.
- No NixOS module or VM-test change: the smartd hook
  (`modules/braid/monitor.nix:24`) already `touch`es a regular file,
  and all VM tests (`tests/cli/braid-smartd-alert.py`,
  `tests/module/smartd-hook.py`) use `touch` or `rm` on a regular
  file.
- No doc change: ADR 014 (`docs/decisions/014-alerts.md`) already
  describes smartd-alert as a "flag file"; the new `is_file()` check
  is consistent with that wording.

## Verification

1. `just test-rust` -- the required gate. It must cover, at minimum:
   - `smartd_alert_active_requires_regular_file` (new, Change 3) --
     the dedicated regression guard for the regular-file semantic. A
     revert of Change 1 back to `Path::exists()` must fail this test
     on the directory case.
   - `cmd_ack_returns_cleanup_failed_when_remove_smartd_alert_errors_after_baseline_saved`
     and `ack_offline_cleanup_failure_after_missing_acked_returns_cleanup_failed`
     (both rewritten by Change 2) -- the `CleanupFailed` contract
     must keep holding at the new cleanup step.
   - The remainder of the ack / alert / monitor suite must still
     pass with no other edits required.
2. No VM test rerun required: the smartd-alert path is exercised by
   `tests/cli/braid-smartd-alert.py` against a regular file (the only
   state the hook produces), and that behavior is unchanged.
