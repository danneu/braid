# Plan: trim `cmd_lock` preflight from `probe_pool` to `probe_fsid`

## Context

`cmd_lock` (cli/src/lock.rs:128-135) currently calls `probe_pool` purely to
extract `pool.fsid`, which is then passed to `preflight::require_lock_preflight`
to read `/sys/fs/btrfs/<fsid>/exclusive_operation`. Every other field of the
`PoolState` is immediately discarded.

`probe_pool` costs `2N+2` subprocesses for an N-device pool:
`findmnt --json`, `btrfs filesystem show`, and per-device
`cryptsetup status` + `cryptsetup luksUUID`. On a 3-drive pool that is 8
subprocesses whose output is thrown away.

Two things `probe_pool` actually contributes to `cmd_lock` beyond the FSID:

1. An explicit `ProbeError::NotBtrfs` on "mounted here, but wrong fstype"
   (cli/src/probe.rs:244-249). This has to be preserved or `cmd_lock`
   starts reporting that case as a generic btrfs-show parse failure.
2. An implicit check that per-device cryptsetup state is sane. This one
   is genuinely redundant for `cmd_lock` -- the mapper close loop
   re-derives existence via `fs.exists` and reports errors via the
   cryptsetup-close exit code.

The goal is to keep (1) and drop the rest, shrinking the preflight from
`2N+2` subprocesses to 2 (`findmnt --json` + `btrfs filesystem show`).

No new parser is introduced. The existing `parse_btrfs_filesystem_show`
already returns a `uuid: Option<String>` field; `probe_fsid` takes that
and converts `None` into a typed error. Real btrfs-progs always emits
`Total devices` on a successful `btrfs filesystem show`, so the
existing parser's dependency on that line is not a practical failure
mode.

## Approach

### Step 1 -- add `probe_fsid` in `cli/src/probe.rs`

Place `probe_fsid` next to `probe_pool`. Two subprocesses: `findmnt`
(for the fstype guard) and `btrfs filesystem show` (for the FSID, via
the existing full parser).

```rust
/// Resolve a mounted pool's FSID for preflight checks against
/// `/sys/fs/btrfs/<fsid>/exclusive_operation`, without probing the
/// per-device cryptsetup state.
///
/// Preserves the `NotBtrfs` contract from `probe_pool`: if the mount
/// point is held by a non-btrfs filesystem, returns `ProbeError::NotBtrfs`
/// rather than a generic parse failure from `btrfs filesystem show`.
///
/// Caller must have already confirmed the mount point is active (e.g.
/// via `CmdRequest::MountpointCheck`).
pub fn probe_fsid<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<String, ProbeError> {
    let findmnt_raw = runner.run(&CmdRequest::FindmntJson {
        mount_point: mount_point.clone(),
    })?;
    let findmnt = crate::parse::parse_findmnt_json(&findmnt_raw)?;

    let entry = findmnt
        .filesystems
        .iter()
        .find(|e| e.target == mount_point.as_str())
        .ok_or_else(|| ProbeError::PoolDevice {
            mapper: mount_point.0.clone(),
            detail: "mount point not present in findmnt output".into(),
        })?;

    if entry.fstype != "btrfs" {
        return Err(ProbeError::NotBtrfs {
            mount_point: mount_point.0.clone(),
            fstype: entry.fstype.clone(),
        });
    }

    let show_raw = runner.run(&CmdRequest::BtrfsFilesystemShow {
        mount_point: mount_point.clone(),
    })?;
    let show = parse_btrfs_filesystem_show(&show_raw)?;
    show.uuid.ok_or_else(|| ProbeError::PoolDevice {
        mapper: mount_point.0.clone(),
        detail: "mounted pool has no FSID in btrfs filesystem show output".into(),
    })
}
```

### Step 2 -- swap the call in `cli/src/lock.rs`

Replace the existing `probe_pool` block in `cmd_lock` with:

```rust
if pool_was_mounted {
    let fsid = probe_fsid(runner, mount_point)
        .map_err(|e| LockError::Failed(format!("cannot probe pool: {e}")))?;
    preflight::require_lock_preflight(fs, &fsid).map_err(LockError::Failed)?;
}
```

Update the import at cli/src/lock.rs:8 from
`use crate::probe::{probe_pool, Filesystem};` to
`use crate::probe::{probe_fsid, Filesystem};`.

### Out of scope

`ack.rs`, `add.rs`, `remove.rs`, `remove_missing.rs`, `replace.rs`,
`status.rs`, `monitor.rs`, `unlock.rs`, `pool.rs` all consume more of
`PoolState` than just `fsid`. They keep using `probe_pool`.

## Files to modify

- cli/src/probe.rs -- add `probe_fsid` (reuses `parse_btrfs_filesystem_show`
  and `parse_findmnt_json`).
- cli/src/lock.rs -- swap the probe call; update import; shrink test
  mocks.

## Test coverage

### `probe_fsid` unit tests (cli/src/probe.rs)

1. `probe_fsid_happy` -- `FindmntJson` returns btrfs entry,
   `BtrfsFilesystemShow` returns the canonical 2-device output. Assert
   the FSID string. `MockRunner` panics on any unregistered
   `CmdRequest`, so only FindmntJson + BtrfsFilesystemShow being
   registered is the regression guard that `probe_fsid` does not
   silently grow into per-device cryptsetup probing.
2. `probe_fsid_rejects_non_btrfs` -- `FindmntJson` reports
   `fstype: "ext4"`. Assert `ProbeError::NotBtrfs`. Explicit
   regression test for the `NotBtrfs` contract.
3. `probe_fsid_mount_not_in_findmnt` -- `FindmntJson` returns no
   matching entry. Assert `ProbeError::PoolDevice` with a "not
   present in findmnt" detail.

Reuse the existing `findmnt_btrfs()`, `findmnt_ext4()`, `findmnt_empty()`,
and `btrfs_show_2disk()` helpers already in `probe::tests`.

### Lock tests (cli/src/lock.rs)

The existing `mounted_runner` helper and `with_probe_pool_mocks` helper
register `FindmntJson`, `BtrfsFilesystemShow`, two `CryptsetupStatus`,
and two `CryptsetupLuksUuid` mocks. After this change the two
`CryptsetupStatus` and two `CryptsetupLuksUuid` mocks are unreachable.

Actions:

- Rename `with_probe_pool_mocks` -> `with_fsid_probe_mocks` and delete
  its four per-device cryptsetup `.with_output(...)` calls. Keep
  `FindmntJson` + `BtrfsFilesystemShow` (the BtrfsFilesystemShow mock
  must contain a `Total devices` line and at least one `devid`, because
  `probe_fsid` reuses the full `parse_btrfs_filesystem_show`).
- Shrink `mounted_runner` to compose via `with_fsid_probe_mocks` plus
  `MountpointCheck`, `Umount`, and `BtrfsDeviceScanForget`.
- `sed`-rename every `with_probe_pool_mocks(...)` call site to
  `with_fsid_probe_mocks(...)`.

Because `MockRunner` returns `MissingMock` for unregistered requests,
the surviving lock tests running against the shrunken mock surface
mechanically prove `cmd_lock` no longer issues the per-device cryptsetup
requests. Tests' behavioral assertions do not change.

Add one new lock test:

- `lock_rejects_mounted_but_not_btrfs` -- `MountpointCheck` says mounted,
  `FindmntJson` reports `fstype: "ext4"` for the mount point. Assert
  `cmd_lock` returns `LockError::Failed` with a message mentioning
  `not btrfs` and `ext4`. Regression guard that the `NotBtrfs` contract
  fires via `cmd_lock`, not just via `probe_fsid` in isolation.

### Coverage audit vs. the two axes

Plan's substantive changes:
- New probe (`probe_fsid`) -- pinned by the three probe unit tests.
- `cmd_lock` swap -- pinned by the surviving lock tests on the
  shrunken mock surface plus the new `lock_rejects_mounted_but_not_btrfs`.

Plan's claims about existing behavior:
- "Mapper close loop re-verifies the `/dev/mapper` existence invariant."
  Pinned by `lock_happy_path_unmounts_and_closes`, `lock_partial_state`,
  `lock_orphan_close_failure_is_fatal`.
- "Cryptsetup close exit code re-reports busy/not-active."
  Pinned by `lock_retries_busy_close_then_succeeds`,
  `lock_umount_fails_busy_mapper_is_warning`,
  `lock_umount_fails_unexpected_mapper_error_is_fatal`,
  `lock_mapper_close_fatal_when_umount_succeeded`.
- "Lock still refuses on active exclusive op via `require_lock_preflight`."
  Pinned by `lock_refuses_when_exclusive_op_active` and
  `lock_refuses_when_balance_paused`. These tests are load-bearing --
  they only pass if the FSID returned by `probe_fsid` matches the one
  under `/sys/fs/btrfs/<fsid>/exclusive_operation` in `MockFs`.

## Verification

1. `just test-rust` -- runs the three new probe tests, one new lock
   test, and every existing lock test against the shrunken mock
   surface.
2. Confirm no residual callers: `rg '\bprobe_pool\b' cli/src/lock.rs`
   returns no matches (comment references are fine).
3. No VM test change required. Existing lock-path VM coverage
   (`scrub-lifecycle.py`, `systemd-lifecycle.py`,
   `pool-lock-contention.py`, `ups-lb-*.py`) continues to exercise
   `braid lock` end-to-end against real btrfs/cryptsetup and will
   surface any behavioral regression on a real pool.
