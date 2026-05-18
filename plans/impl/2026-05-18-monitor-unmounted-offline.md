# Pin the "well-formed mountinfo, no entry" arm of cmd_monitor

## Context

`cmd_monitor` (`cli/src/monitor.rs:51-76`) routes pool-state outcomes
through two non-error arms and one fail-closed arm:

1. `Err(ProbeError::NotBtrfs { .. })` -> `MonitorResult::PoolOffline`
   (exit 0, no latch).
2. `Ok(p)` with `p.mounted == false` -> `MonitorResult::PoolOffline`
   (exit 0, no latch). Reached when `/proc/self/mountinfo` is
   well-formed and simply has no entry for the configured mount point.
3. Every other `Err(...)` variant -> latch `ComputationError`,
   `MonitorResult::Alert` (exit 1, beeper fires).

ADR 014:78 explicitly carves out arm 2 as legitimate offline (exit 0)
and distinguishes it from any mountinfo IO/malformed/duplicate failure
(arm 3). The boundary matters: the wrong default produces "false-alarm
beep on every timer cycle when the pool is legitimately offline" --
exactly the regression that motivated the existing arm-1 test
`monitor_classifies_non_btrfs_mount_as_offline`
(`cli/src/monitor.rs:589-601`).

Arm 1 and arm 3 each have a dedicated `cmd_monitor` unit test. Arm 2
does not. The underlying probe behavior is pinned at
`probe_pool_alerts_unmounted` (`cli/src/probe.rs:1826-1837`), but no
test pins the integration at the `cmd_monitor` boundary -- so a future
over-eager refactor (e.g. removing the `if !pool.mounted` early return
at `cli/src/monitor.rs:74-76`, or flipping the `None` arm of
`probe_pool_alerts` to `Err(ProbeError::MountInfo(..))`) would compile
clean and silently start beeping on every offline cycle.

The sister `cmd_ack` surface already has the parallel fixture
(`ack_fs_not_mounted`, `cli/src/test_fixtures/ack.rs:126-128`) used by
8 ack tests. The monitor fixture family has `monitor_fs_btrfs`,
`monitor_fs_ext4`, and `monitor_fs_mountinfo_error` but lacks the
not-mounted variant -- pure asymmetry, not a deliberate omission.

## Change

### 1. Add the missing fixture constructor

**File:** `cli/src/test_fixtures/monitor.rs`

Add directly after `monitor_fs_btrfs()` (`cli/src/test_fixtures/monitor.rs:240-244`),
mirroring the body shape of the existing constructors and the name of
the sister `ack_fs_not_mounted()`:

```rust
/// Mountinfo with no entry for the configured target -- monitor's
/// legitimate-offline branch (well-formed body, target simply absent).
pub(crate) fn monitor_fs_not_mounted() -> impl Filesystem {
    MonitorFs {
        mountinfo: Ok(""),
    }
}
```

Empty string is sufficient because `fstype_at_mount`
(`cli/src/mount_check.rs:62-79`) iterates lines and returns `Ok(None)`
when nothing matches the target. `MonitorFs::read_to_string`
(`cli/src/test_fixtures/monitor.rs:222-227`) asserts the read path is
`/proc/self/mountinfo`, so no other surface is touched.

### 2. Wire the re-export

**File:** `cli/src/test_fixtures.rs`

Add `monitor_fs_not_mounted` to the existing `pub(crate) use monitor::{..}`
block at `cli/src/test_fixtures.rs:176-180`, in the alphabetically-correct
slot:

```rust
pub(crate) use monitor::{
    MonitorOverride, MonitorReconcileRunner, MonitorTestRunner,
    assert_monitor_single_computation_error, monitor_fs_btrfs, monitor_fs_ext4,
    monitor_fs_mountinfo_error, monitor_fs_not_mounted, monitor_mp,
};
```

### 3. Add the missing test

**File:** `cli/src/monitor.rs`

Add to the `#[cfg(test)] mod tests` block, placed directly next to the
sister `monitor_classifies_non_btrfs_mount_as_offline`
(`cli/src/monitor.rs:589-601`). The test mirrors the sibling exactly --
same runner, same assertion pair, same preamble shape -- so the two
arms read together as a complete coverage map of the "exit 0, no
latch" surface:

```rust
/*
 * Intent: When `/proc/self/mountinfo` is well-formed but has no entry
 * for the configured mount point, cmd_monitor must return
 * MonitorResult::PoolOffline and leave no alert latch behind.
 *
 * Why it exists: pins the only other non-fail-closed arm besides
 * NotBtrfs. ADR 014:78 distinguishes this case (legitimate offline,
 * exit 0) from any mountinfo IO/malformed/duplicate failure
 * (ProbeError::MountInfo, fail-closed, exit 1). The probe layer is
 * already pinned by probe_pool_alerts_unmounted, but no integration
 * test pins how cmd_monitor classifies the Ok(p) + pool.mounted=false
 * branch -- an over-eager refactor that drops the `if !pool.mounted`
 * early return or flips the probe's None arm to Err would compile
 * clean and start the beeper on every offline timer cycle.
 *
 * Scenario: the NAS has booted but the encrypted pool has not been
 * unlocked or mounted yet, so mountinfo has no /mnt/storage entry.
 */
#[test]
fn monitor_classifies_unmounted_as_offline() {
    let (_dir, paths) = isolated_paths();
    let runner = MonitorTestRunner::with_stale_mapper_stats();

    let result = cmd_monitor(&runner, &monitor_fs_not_mounted(), &monitor_mp(), &paths);

    assert_eq!(result, MonitorResult::PoolOffline);
    assert!(
        !paths.alert_latch_json().exists(),
        "PoolOffline must not write an alert latch"
    );
}
```

Also add `monitor_fs_not_mounted` to the `use crate::test_fixtures::{..};`
import block at `cli/src/monitor.rs:162-166`.

## Files to modify

- `cli/src/test_fixtures/monitor.rs` -- new `monitor_fs_not_mounted()`
  constructor (4 lines + doc).
- `cli/src/test_fixtures.rs` -- add `monitor_fs_not_mounted` to the
  re-export at line 176-180.
- `cli/src/monitor.rs` -- new test + add the constructor to the local
  import block.

Total: ~25 lines including doc comment and test preamble. No changes
to non-test code.

## Existing functions / fixtures reused

- `MonitorFs` -- struct at `cli/src/test_fixtures/monitor.rs:209-232`.
  Single `mountinfo: Result<&'static str, std::io::ErrorKind>` field;
  the new constructor reuses it as-is.
- `MonitorTestRunner::with_stale_mapper_stats()` -- runner at
  `cli/src/test_fixtures/monitor.rs:99-104`. Mirrors what the sister
  `monitor_classifies_non_btrfs_mount_as_offline` test uses.
- `monitor_mp()`, `isolated_paths()` -- shared helpers already in
  scope via the existing import block.
- `MonitorResult::PoolOffline`, `paths.alert_latch_json()` -- assertion
  surfaces already used by the sister test.
- Sister-surface precedent: `ack_fs_not_mounted()` at
  `cli/src/test_fixtures/ack.rs:126-128` for the naming convention
  and fixture shape.

## Naming note

The verify-issue finding proposed `monitor_fs_unmounted()`. This plan
uses `monitor_fs_not_mounted()` instead to match the established
sister `ack_fs_not_mounted()`. The two fixture families then share one
naming rule (`<surface>_fs_{btrfs,ext4,not_mounted,mountinfo_error,...}`)
and a future reader can find the parallel by mechanical substitution.

## Verification

- `just test-rust -- monitor::tests::monitor_classifies_unmounted_as_offline`
  -- the new test must pass.
- `just test-rust -- monitor::tests::monitor_classifies_non_btrfs_mount_as_offline`
  -- the sister test must still pass (regression gate for the import
  block edit).
- `just test-rust` -- full unit test suite must remain green (no
  collateral damage from the re-export edit).
- Local regression check: temporarily delete the `if !pool.mounted {
  return Ok(None); }` block at `cli/src/monitor.rs:74-76` in a scratch
  branch and confirm `monitor_classifies_unmounted_as_offline` fails
  (it should fail by returning `MonitorResult::Ok` instead of
  `PoolOffline`). Revert before committing.

No VM-test changes are needed: the gap was that `tests/cli/braid-monitor.py`
never asserts a `braid monitor` exit code in the unmount-without-teardown
window, but adding one there would force the existing test sequence to
re-issue cryptsetup-open after an exit-code probe, which is incidental
to this finding. The Rust unit test pins the boundary at the cheapest
layer.
