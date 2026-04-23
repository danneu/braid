# Plan: live-path resize failure leaves old LUKS mapper open

## Context

In `cli/src/replace.rs` the Live arm of `cmd_replace` runs, in order:

```rust
pool_replace_device(runner, *devid, &new_mapper_path, mount_point, progress)?;  // line 320
eprintln!("Replace complete.");
pool_resize_device(runner, *devid, config.mount_point())?;                       // line 329  <-- `?`
// Best-effort LUKS close of old mapper.                                         // lines 331-344
let close_result = runner.run(&CmdRequest::CryptsetupClose { mapper: mapper.0.clone() });
...
eprintln!("Old device closed. If repurposing the physical disk, wipe it separately.");
```

If `pool_resize_device` returns `Err`, the `?` propagates and the best-effort
`CryptsetupClose` at line 332 never runs. The old mapper stays open, still
bound to the backing disk -- `btrfs replace` has already decoupled that disk
from the pool, so **data integrity is not at risk**, but the dm slot keeps
the physical disk busy until `braid lock` (which closes it as a pool member
or as an orphan) or a reboot.

A related cosmetic bug is next door: the `"Old device closed..."` message at
line 345 prints unconditionally, even if the close returned an error or a
non-zero exit. After a close failure the operator currently sees both a
warning AND a success line.

## Fix

In the Live arm only, reorder cleanup so the best-effort close runs BEFORE
`pool_resize_device`. The two operations are independent:

- Close targets the **old mapper name** (no longer referenced by btrfs).
- Resize targets the devid preserved through `btrfs replace`, addressed via
  the mount point and devid -- not via the old mapper path.

Reordering means a resize failure still releases the old dm slot before
returning `Err`. Also move the `"Old device closed..."` print into the
success arm of the close match, so a failed close no longer produces a
contradictory success line.

The Missing arm at `cli/src/replace.rs:347-363` is untouched (no old mapper
to close; see the existing comment).

## Changes

### 1. `cli/src/replace.rs` -- execution ordering (Live arm)

Rewrite the Live arm body at `cli/src/replace.rs:319-345` (below the
`"Replace complete."` print) as:

```rust
// Best-effort LUKS close of old mapper. Runs BEFORE resize: a resize
// failure would `?` out and skip the close, leaving the old dm slot
// bound to the backing disk until `braid lock` or reboot.
let close_result = runner.run(&CmdRequest::CryptsetupClose {
    mapper: mapper.0.clone(),
});
match close_result {
    Ok(r) if r.exit_status == 0 => {
        eprintln!(
            "Old device closed. If repurposing the physical disk, wipe it separately."
        );
    }
    Ok(r) => {
        eprintln!(
            "Warning: failed to close LUKS mapper {} (exit {})",
            mapper, r.exit_status
        );
    }
    Err(e) => {
        eprintln!("Warning: failed to close LUKS mapper {}: {}", mapper, e);
    }
}

pool_resize_device(runner, *devid, config.mount_point())?;
```

No helper extraction: testing is done against `cmd_replace` directly (see
change 4) so there is no test-only refactor pressure on the production
code.

### 2. `cli/src/replace.rs` -- dry-run step order (`compile_replace_steps`)

In the Live branch at `cli/src/replace.rs:600-617`, swap the order of the
resize step and the cryptsetup-close step so the dry-run rendering mirrors
execution. Replace the two `steps.push` calls so the final order is:

1. `btrfs replace start` (existing, unchanged)
2. `cryptsetup close <old_mapper>` (moved up from below)
3. `btrfs filesystem resize <devid>:max` (moved down)

### 3. `cli/src/replace.rs` -- dry-run test assertion line numbers

`dry_run_render_fresh_disk_live_replace_with_keyfile` at
`cli/src/replace.rs:1662-1720` asserts absolute line positions against the
rendered output. Update assertions so the final layout is:

- lines 8-9: `btrfs replace start` (unchanged)
- lines 10-11: `cryptsetup close braid-disk2` (moved up; include an
  `assert_eq!` on the `$ cryptsetup close braid-disk2` command line to pin
  the exact position)
- lines 12-13: `btrfs filesystem resize` (moved down)

### 4. `cli/src/replace.rs` -- new regression test against `cmd_replace`

Add a unit test that drives `cmd_replace` end-to-end on the Live path so
the test binds to the user-facing behavior, not to an internal helper.
Model it on the existing `journal_survives_replace_failure` test at
`cli/src/replace.rs:1653-1729`: same `tempfile::TempDir` + `StatePaths`
setup, same `ReplaceMockFs` (`cli/src/replace.rs:1562-1580`) with
`/dev/disk/by-id/virtio-disk3` and `/dev/mapper/braid-disk3` present, same
`RecordingInhibitor::new()` (`cli/src/inhibit.rs:76-106`) for
`sleep_inhibitor`, same `ReplaceParams` shape with
`passphrase_file: Some(pass_path.as_path())` and
`progress: ProgressOutput::Off`.

```rust
// Intent: close of old mapper must run even when post-replace resize fails.
// Why: a resize failure returning `?` previously skipped the best-effort
//   close, leaving the old dm slot bound to its backing disk until the
//   next `braid lock` or reboot.
// Scenario: `btrfs fi resize <devid>:max` fails (exit != 0) after a
//   successful `btrfs replace start`; cmd_replace must still have emitted
//   the CryptsetupClose on the old mapper before returning the resize
//   error.
```

Runner construction: copy the existing `FailingReplaceRunner`
(`cli/src/replace.rs:1584-1651`) into a new logging variant -- call it
`ResizeFailingLoggingRunner` -- that:

1. Adds an `Arc<Mutex<Vec<CmdRequest>>>` log field and pushes every
   incoming request at the top of `run` (and `run_with_stdin`) before
   dispatching.
2. Keeps every probe/preflight match arm identical to `FailingReplaceRunner`
   (Findmnt, BtrfsFilesystemShow, CryptsetupStatus, CryptsetupLuksUuid,
   CryptsetupLuksDumpText, BtrfsBalanceStatus, BtrfsDeviceStatsJson).
3. Changes the `BtrfsReplaceStart` arm from failure to success:
   `Ok(mock_ok("btrfs replace start", "", 0))` (use the existing `mock_ok`
   helper already used elsewhere in this module).
4. Adds a `CryptsetupClose { .. }` arm returning `Ok(mock_ok("cryptsetup close", "", 0))`.
5. Adds a `BtrfsFilesystemResize { .. }` arm returning a failure
   `RawCommandOutput { exit_status: 1, stderr: "ERROR: unable to resize".into(), .. }`.
6. Runs with `ProgressOutput::Off`, so `run_replace_with_progress`
   (`cli/src/progress.rs:222-223`) dispatches through the plain `runner.run`
   path and the log captures every call.

Assertions:
- Return value matches `Err(ReplaceError::Pool(PoolError::Failed(msg)))`
  where `msg` contains `"btrfs filesystem resize failed"` (the typed
  payload from `cli/src/pool.rs:261-265`). Use `matches!` or a `match`
  arm to bind the typed variant, not just the `to_string()` text.
- The recorded log (locked and cloned for inspection) contains a
  `CryptsetupClose { mapper: "braid-disk2" }` entry.
- The `CryptsetupClose { mapper: "braid-disk2" }` index is strictly less
  than the `BtrfsFilesystemResize { devid: 2, .. }` index (closure over
  `log.iter().position(...)`). This pin is what a future "move close back
  after resize" regression trips on -- the close would be missing from
  the log entirely (resize's `?` returns before the close runs).
- Pending-op journal survives the error return:
  `journal::load_journal(&paths).unwrap().is_some()` (same guardrail as
  `cli/src/replace.rs:1720-1723`).

### 5. `cli/src/recover.rs` -- doc comment drift

`cli/src/recover.rs:445-447` currently says the original command issues the
resize "at `cli/src/replace.rs:327` (Live) and `:359` (Missing) immediately
after `pool_replace_device`." The line numbers are already stale (actual
lines are 329 and 361), and after this change the Live resize is no longer
"immediately after" the replace -- a close sits in between. Rewrite to:

```
/// 1. **Replace-only**: replay `pool_resize_device` on the new disk's devid.
///    The original command issues the resize in both Live and Missing arms
///    after `pool_replace_device` succeeds. If shutdown lands between the
///    kernel-resumed dev_replace and the resize, the new disk reports the
///    source disk's old size instead of its full capacity. Resize-to-max is
///    idempotent at the btrfs layer.
```

## Out of scope

- **Orphan cleanup in recovery.** After the fix the residual leak window
  shrinks to "shutdown between `pool_replace_device` and the close", which
  is tiny. If it still fires, `braid lock`'s orphan sweep in
  `cli/src/lock.rs:149-161` closes the dangling mapper. Changing
  `relock_and_remount` / `replay_post_mutation` to proactively close an
  old-mapper orphan is a separate concern with its own
  `union_memberships`/reopen-then-close dance to design.
- **Missing arm.** `cli/src/replace.rs:347-363` has no old mapper to close;
  the comment already documents that.

## Critical files

- `cli/src/replace.rs:319-345` (Live arm body), `:600-617` (compile steps),
  `:1662-1720` (dry-run test), `:769` (`test_paths` helper for the new
  regression test)
- `cli/src/pool.rs:251-268` (`pool_resize_device` contract -- unchanged,
  reference only)
- `cli/src/recover.rs:443-450` (doc comment to update)
- `cli/src/replace.rs:1562-1580` (`ReplaceMockFs`),
  `cli/src/replace.rs:1584-1651` (`FailingReplaceRunner` -- template for
  the new logging runner),
  `cli/src/replace.rs:1653-1729` (`journal_survives_replace_failure` --
  end-to-end `cmd_replace` driving template)
- `cli/src/inhibit.rs:76-106` (`RecordingInhibitor` seam)

## Verification

1. `cargo test -p braid-cli replace::` -- new regression test passes;
   updated `dry_run_render_fresh_disk_live_replace_with_keyfile` still
   passes.
2. `just test-vm replace-live-disk` -- existing happy-path VM test already
   asserts `test -e /dev/mapper/braid-disk2` fails after live replace
   (`tests/cli/replace-live-disk.py:103-104`); the reorder does not change
   that outcome.
3. Manual revert check: temporarily swap the two operations (resize before
   close) in the Live arm and confirm the new unit test fails because the
   `CryptsetupClose` entry is missing from the recorded request log.
   Revert.
