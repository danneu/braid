# Plan: harden `probe_pool` mount detection by reading `/proc/self/mountinfo` directly

## Context

The `cmd_idle` mountinfo plan (`plans/impl/2026-04-28-cmd-idle-mountinfo-probe.md`) closed a fail-open seam in autosuspend by routing `cmd_idle`'s mount-presence check through the new `cli/src/mount_check.rs` module. That plan deliberately left an identical fail-open seam in `probe_pool` (`cli/src/probe.rs:214-241`) for a separate decision -- this is that plan.

Bug shape, identical to the `cmd_idle` case:

- `parse_findmnt_json` (`cli/src/parse/findmnt.rs:30-36`) maps `exit_status != 0 && stderr.is_empty()` to `Ok(empty filesystems)` -- the "mount point not found" folklore branch.
- `probe_pool` (`cli/src/probe.rs:223-237`) maps an empty filesystems list to `Ok(PoolState { mounted: false, ... })`.
- `cmd_monitor` (`cli/src/monitor.rs:46-78`) maps `mounted: false` to `MonitorResult::PoolOffline`, which `cli/src/main.rs:582-601` exits 0. ADR 014 (`docs/decisions/014-alerts.md:58-65`) declares the monitor fail-closed: any indeterminate state must latch `ComputationError` and exit 1 so the systemd wrapper beeps.

Result: a non-zero `findmnt` exit with empty stderr that is **not** the legitimate "not a mount point" case -- a future findmnt regression, an unexpected upstream output change, a cgroup/PID namespace anomaly -- silently silences alerting on a genuinely mounted pool. A degraded-array event that should beep stays silent.

Signal-killed `findmnt` is already converted to `CmdError::Failed` in `cli/src/cmd.rs:841-867` before the parser runs, so SIGKILL/OOM are not in scope. The gap is purely the lenient "non-zero + empty stderr" branch, exactly as in the `cmd_idle` case.

Goal: remove the subprocess from `probe_pool`'s mount-presence check. Use `mount_check::fstype_at_mount` -- already on disk and validated -- as the single source of truth. Preserve the existing semantics for the other findmnt consumers (`probe_fsid`, `check_not_read_only`); they are fail-closed-at-callsite or fail-open-by-design and not the subject of this plan.

## Why route `probe_pool` through `mount_check` (option a) over the alternatives

Per the follow-up note in the `cmd_idle` plan:

- **Option (a) -- route `probe_pool` through `mount_check`.** Eliminates the folklore branch from the safety-critical path entirely. The `mount_check` module already exists, is unit-tested, and answers exactly the question `probe_pool` is asking ("is `mount_point` present and what fstype?"). Threads `&Filesystem` through every `probe_pool` caller, but the change is mechanical and matches the pattern already established for `cmd_idle`. **Chosen.**
- **Option (b) -- `lsblk` or `btrfs filesystem show` for device enumeration plus mountinfo for presence.** `probe_pool` already uses `BtrfsFilesystemShow` for device enumeration (`probe.rs:241-248`); the only thing findmnt does there is mount-presence + fstype detection. So (b) collapses into (a) for `probe_pool`. Rejected as a distinct option.
- **Option (c) -- tighten `parse_findmnt_json` to return `Err` on non-zero exit + empty stderr.** Smallest code delta, but `findmnt --mountpoint X` exits 1 with empty stderr as its standard "not a mount point" signal (modelled in real test mocks like `unlock.rs:1341-1344`). Tightening the parser without an alternative source of truth means callers have **no** way to distinguish "not mounted" from "tooling broke" -- every caller would have to fall back to a non-findmnt detector, which is exactly option (a) plus extra steps. Rejected.

Coverage of the existing `mount_check` module is in `cli/src/mount_check.rs:138-419` (15 unit tests covering happy path, octal-escape decoding, non-ASCII UTF-8 paths, malformed lines, duplicate targets, empty source fields, IO failure). No new parser logic required for this plan.

## Files to modify

- `cli/src/mount_check.rs` -- add a thin helper `fstype_at_mount_via_fs<F: Filesystem + ?Sized>(fs, target) -> Result<Option<String>, MountInfoError>` that reads `MOUNTINFO_PATH` through the `Filesystem` trait and delegates to the existing `fstype_at_mount`. Mirrors the existing `is_btrfs_mounted` helper but returns the fstype so `probe_pool` can distinguish "absent" from "wrong fstype".
- `cli/src/probe.rs` --
  - Add `MountInfo(#[from] crate::mount_check::MountInfoError)` variant to `ProbeError` (preserve the existing `Cmd`, `Parse`, `PoolDevice`, `NotBtrfs`, `UnsupportedLuksVersion`, `MapperConflict` variants).
  - Change `probe_pool` signature to `pub fn probe_pool<R: CommandRunner, F: Filesystem + ?Sized>(runner: &R, fs: &F, mount_point: &MountPoint) -> Result<PoolState, ProbeError>`.
  - Replace the `runner.run(FindmntJson) + parse_findmnt_json + .find(|e| e.target == ...)` block (`probe.rs:217-237`) with a single `mount_check::fstype_at_mount_via_fs(fs, mount_point.as_str())?` call that maps:
    - `None` -> early-return `Ok(PoolState { mounted: false, devices: vec![], missing_count: 0, total_devices: 0, fsid: None, missing_devids: vec![], null_underlying: vec![] })` (unchanged shape).
    - `Some(fstype)` where `fstype != "btrfs"` -> `Err(ProbeError::NotBtrfs { mount_point: mount_point.0.clone(), fstype })` (unchanged shape).
    - `Some("btrfs")` -> fall through to the existing `BtrfsFilesystemShow` + cryptsetup logic, unchanged.
  - Update `probe.rs` tests to construct an `&fs` argument (the existing `MockFs` in `probe.rs:398-450` already implements `Filesystem` and is used by the per-device tests; extend it with a `mountinfo: Option<String>` field served from `read_to_string("/proc/self/mountinfo")`).
- `cli/src/monitor.rs` -- `cmd_monitor` signature change to take `&F: Filesystem + ?Sized`, threaded to the `probe_pool` call. Add `ProbeError::MountInfo(_)` to the existing fail-closed match arm at `monitor.rs:62-69` so a mountinfo IO/parse error latches `ComputationError` (the whole point of this plan). The `?Sized` bound matches the `cmd_idle` pattern in `idle.rs`.
- `cli/src/ack.rs`, `cli/src/add.rs`, `cli/src/remove.rs`, `cli/src/replace.rs`, `cli/src/remove_missing.rs`, `cli/src/status.rs`, `cli/src/recover.rs`, `cli/src/unlock.rs`, `cli/src/pool.rs` -- thread `&fs` through every `probe_pool` caller. Each `cmd_*` function takes a new `&F: Filesystem + ?Sized` parameter; `pool.rs::pool_restore_raid1_if_degraded` and `pool.rs::evict_present_device` likewise. No behavior change in these callers -- they continue to handle `Ok(PoolState { mounted: false })` exactly as today. The new `ProbeError::MountInfo` variant is propagated by `?` through the same `Probe(#[from] ProbeError)` channel each command already uses (see `add.rs::AddError::Probe`, `remove.rs::RemoveError::Probe`, etc.); no per-caller error-mapping changes needed.
- `cli/src/tui/probe.rs` -- `probe_pool_for_tui` already takes `&F: Filesystem + ?Sized` (`tui/probe.rs:23-30`), but its inner call is `probe_pool(runner, mount_point)` (`tui/probe.rs:30`); change to `probe_pool(runner, fs, mount_point)`. No new parameter needed at the TUI boundary -- `fs` is already in scope. The eight TUI tests in this file that mock `CmdRequest::FindmntJson` (`tui/probe.rs:825, 987, 1126, 1271, 1351, 1418, 1496, 1564`) must be retargeted to seed mountinfo via `MockFs` instead, same shape as the `probe.rs` test retargets below.
- `cli/src/main.rs` -- single change: pass `&fs` (the existing `RealFilesystem` constructed for `cmd_idle` at `main.rs:548-549`) to every `cmd_*` dispatch arm that calls `probe_pool` transitively. No new construction needed; `RealFilesystem` is zero-sized.
- `docs/decisions/014-alerts.md` -- add a one-paragraph note under "Mount detection" (or extend the existing fail-closed section) recording that mount-presence is read from `/proc/self/mountinfo` via `mount_check`, not from `findmnt`, and that any IO/parse error on mountinfo latches `ComputationError`.

**Out of scope:**

- `probe_fsid` (`probe.rs:354-387`). Still uses `parse_findmnt_json` for "is mounted as btrfs". `probe_fsid` is fail-closed at its callsite -- "target not in findmnt output" maps to `Err(ProbeError::PoolDevice)`, which the only safety-critical caller (`cmd_idle`) propagates as `IdleError::Probe` -> exit 2 -> blocks suspend. Hardening `probe_fsid` is a separate plan.
- `check_not_read_only` (`preflight.rs:198-231`). Fail-open by design -- preflight failures map to advisory warnings (`Ok(Some(text))`), not hard errors. Not affected by the lenient parser branch in any safety-critical sense.
- `parse_findmnt_json` itself stays unchanged. Tightening it is the rejected option (c); we pull its sole safety-critical caller off it instead.
- `FindmntJson` `CmdRequest` variant stays. `probe_fsid` and `check_not_read_only` still need it.

## Implementation sketch

### 1. `cli/src/mount_check.rs` -- new helper

```rust
/// IO-shimmed variant of `fstype_at_mount` that reads `/proc/self/mountinfo`
/// through the existing `Filesystem` trait. Returns the fstype mounted at
/// `target` (Some("btrfs"), Some("ext4"), ...), Ok(None) if the well-formed
/// mountinfo content has no entry for `target`, or Err for IO failure /
/// malformed line / duplicate target.
pub fn fstype_at_mount_via_fs<F: Filesystem + ?Sized>(
    fs: &F,
    target: &str,
) -> Result<Option<String>, MountInfoError> {
    let content = fs.read_to_string(MOUNTINFO_PATH)?;
    fstype_at_mount(&content, target)
}
```

`is_btrfs_mounted` (already public) stays as-is and is used by `cmd_idle`. The new helper is used by `probe_pool`.

### 2. `cli/src/probe.rs` -- `ProbeError` and `probe_pool`

```rust
#[derive(Debug, thiserror::Error)]
pub enum ProbeError {
    #[error(transparent)]
    Cmd(#[from] CmdError),
    #[error(transparent)]
    Parse(#[from] ParseError),
    #[error("pool device error at {mapper}: {detail}")]
    PoolDevice { mapper: String, detail: String },
    #[error("mount point {mount_point} holds non-btrfs filesystem ({fstype})")]
    NotBtrfs { mount_point: String, fstype: String },
    #[error("unsupported LUKS version on {name}: {version}")]
    UnsupportedLuksVersion { name: String, version: String },
    #[error("mapper {name} conflicts: expected {expected}, found {found}")]
    MapperConflict { name: String, expected: String, found: String },
    #[error("mountinfo error: {0}")]
    MountInfo(#[from] crate::mount_check::MountInfoError),  // NEW
}
```

`probe_pool` -- replace lines 217-237 (the findmnt-driven mount-presence branch) with:

```rust
pub fn probe_pool<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
) -> Result<PoolState, ProbeError> {
    match crate::mount_check::fstype_at_mount_via_fs(fs, mount_point.as_str())? {
        None => {
            return Ok(PoolState {
                mounted: false,
                devices: vec![],
                missing_count: 0,
                total_devices: 0,
                fsid: None,
                missing_devids: vec![],
                null_underlying: vec![],
            });
        }
        Some(fstype) if fstype != "btrfs" => {
            return Err(ProbeError::NotBtrfs {
                mount_point: mount_point.0.clone(),
                fstype,
            });
        }
        Some(_) => { /* btrfs -- fall through to BtrfsFilesystemShow */ }
    }

    let show_raw = runner.run(&CmdRequest::BtrfsFilesystemShow {
        mount_point: mount_point.clone(),
    })?;
    // ... rest of existing logic unchanged from probe.rs:241 onward
}
```

Note: this drops the `runner.run(FindmntJson)` call and the `parse_findmnt_json` call. `parse_findmnt_json` import in `probe.rs` is now used only by `probe_fsid` (still in scope of that function, unchanged).

### 3. `cli/src/monitor.rs` -- threaded `&fs` and the new fail-closed arm

```rust
pub fn cmd_monitor<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> MonitorResult {
    let pool = match probe_pool(runner, fs, mount_point) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => return MonitorResult::PoolOffline,
        Err(
            e @ (ProbeError::Cmd(_)
            | ProbeError::Parse(_)
            | ProbeError::PoolDevice { .. }
            | ProbeError::UnsupportedLuksVersion { .. }
            | ProbeError::MapperConflict { .. }
            | ProbeError::MountInfo(_)),  // NEW: fail-closed on mountinfo IO/parse
        ) => return latch_computation_error(e.to_string(), paths),
    };
    // rest unchanged
}
```

The `|` exhaustive match guarantees that adding `ProbeError::MountInfo` without updating this arm is a compile error. That is the structural property that pins fail-closed behavior.

### 4. Wide thread of `&fs` through other `probe_pool` callers

Every caller currently has the shape `cmd_X<R: CommandRunner>(runner, ..., mount_point, ...)`. After this change: `cmd_X<R: CommandRunner, F: Filesystem + ?Sized>(runner, fs, ..., mount_point, ...)`. Each command's body passes `fs` to `probe_pool` (and to any internal helper that itself calls `probe_pool`, e.g. `pool.rs::pool_restore_raid1_if_degraded`).

Concrete sites (per Explore inventory):

| File | Function / line | Role |
|---|---|---|
| `monitor.rs:58` | `cmd_monitor` | safety-critical (the bug) |
| `ack.rs:26` | `cmd_ack` | benign read |
| `add.rs:601, 616, 638, 778` | `cmd_add` | mutation; `mounted: false` blocks |
| `remove.rs:243` | `cmd_remove` | mutation; `mounted: false` blocks |
| `replace.rs:365, 537` | `cmd_replace` | mutation; `mounted: false` blocks |
| `remove_missing.rs:262` | `cmd_remove_missing` | mutation; `mounted: false` blocks |
| `status.rs:302, 402` | `cmd_status` | benign read |
| `recover.rs:348, 490` | `cmd_recover` | validation; errors propagate |
| `unlock.rs:130` | `cmd_unlock` | post-mount enrichment; `if let Ok(...)` -- tolerates errors |
| `pool.rs:170, 300` | `pool_restore_raid1_if_degraded`, `evict_present_device` | helpers; errors propagate |
| `tui/probe.rs:30` | `probe_pool_for_tui` | TUI; already has `&fs`, just pass through |

For all non-monitor callers, the only behavioral change is: if mountinfo IO/parse fails (e.g. `/proc/self/mountinfo` unreadable, malformed line, duplicate target), the caller now receives `Err(ProbeError::MountInfo(...))` instead of the previously silent `Ok(PoolState { mounted: false })`. Mutation commands surface this as a hard error -- desirable, since "we can't tell if the pool is mounted" is the wrong condition under which to attempt a mutation. Read commands (status, ack) surface it with a clear error message instead of silently displaying "offline" -- also desirable.

### 5. `cli/src/main.rs`

`main.rs:548-549` already constructs `let fs = braid_cli::probe::RealFilesystem;` for `cmd_idle`. Reuse that same `&fs` in every command-dispatch arm that calls a `cmd_*` function affected by the signature change. No new construction.

### 6. `docs/decisions/014-alerts.md`

Add a paragraph (under the existing fail-closed exit-code section, around line 58-65):

> Mount-presence is read from `/proc/self/mountinfo` via `mount_check::fstype_at_mount_via_fs`, not from `findmnt`. Any mountinfo IO failure, malformed line, or duplicate target entry surfaces as `ProbeError::MountInfo` -> latches `ComputationError` -> exit 1 -> beeper. "We can't tell if the pool is mounted" is fail-closed; "the pool is not mounted" is `MonitorResult::PoolOffline` (exit 0). The two are now distinguishable, where previously a silent `findmnt` non-zero exit could collapse the first into the second.

## Tests

Each test gets a `/* Intent / Why / Scenario */` block per project convention.

### `cli/src/mount_check.rs` -- new tests for the new helper

- `fstype_at_mount_via_fs_returns_btrfs_when_mounted` -- MockFs serves a mountinfo body with target as btrfs; helper returns `Ok(Some("btrfs"))`. (Behavioral happy path through the IO shim; the underlying `fstype_at_mount` already has 15 tests.)
- `fstype_at_mount_via_fs_returns_none_when_target_absent` -- MockFs serves mountinfo without target; helper returns `Ok(None)`. (Pins the "absent != error" distinction.)
- `fstype_at_mount_via_fs_propagates_io_failure` -- MockFs `read_to_string("/proc/self/mountinfo")` returns `Err(PermissionDenied)`; helper returns `Err(MountInfoError::Io)`. (Regression guard for the IO branch -- this is the critical one.)

(No new tests for malformed-line / duplicate-target paths in `fstype_at_mount_via_fs` -- those are already pinned in the existing `fstype_at_mount` tests at `mount_check.rs:255-313`. The IO shim is a one-liner; we only need to pin the IO branch it adds.)

### `cli/src/probe.rs` -- retargeted + new tests

Existing `probe_pool` tests in `probe.rs:1018+` mock `CmdRequest::FindmntJson` outputs. Retarget them to seed mountinfo via the existing `MockFs`:

- Extend `MockFs` (`probe.rs:398-450`) with a `mountinfo: Option<String>` field. `read_to_string("/proc/self/mountinfo")` returns the seeded body or `NotFound`. Other paths fall through to the existing exists/list_dir behavior.
- `probe_pool_unmounted` (existing, retargeted) -- MockFs serves mountinfo without target; `probe_pool` returns `Ok(mounted: false)`. Drop the `FindmntJson` mock from this test.
- `probe_pool_mounted_btrfs` (existing, retargeted) -- MockFs serves mountinfo with target as btrfs; `probe_pool` proceeds to `BtrfsFilesystemShow` (mocked). Drop the `FindmntJson` mock from this test, keep `BtrfsFilesystemShow` and per-device mocks.
- `probe_pool_returns_not_btrfs_for_other_fstype` (existing, retargeted) -- MockFs serves mountinfo with target as ext4; `probe_pool` returns `Err(ProbeError::NotBtrfs { fstype: "ext4", .. })`.

New regression tests. **Critical: each must seed BOTH the new path (mountinfo error) AND the old fail-open path (`FindmntJson` mock with `exit_status = 1`, empty stderr, empty stdout).** Otherwise reverting the fix would make the test fail with `CmdError::MissingMock`, which is a different error from the actual bug shape -- and a future regression that re-introduces a findmnt fallback would slip past the gate. Seeding both sides means:
- Pre-fix code path: ignores mountinfo, calls findmnt, gets the lenient empty-list, returns `Ok(mounted: false)` -- the exact silent fail-open the plan exists to prevent.
- Post-fix code path: ignores findmnt, reads mountinfo, gets `Err(MountInfoError::*)`, returns `Err(ProbeError::MountInfo(_))`.

Both arms of the regression check are exercised by the same fixture, so the test pins the *behavioral diff* that the fix produces, not just the new behavior in isolation.

- `probe_pool_propagates_mountinfo_io_error` -- `MockFs.read_to_string("/proc/self/mountinfo")` returns `Err(PermissionDenied)`; runner additionally has a stale `CmdRequest::FindmntJson` mock with `RawCommandOutput { exit_status: 1, stderr: "", stdout: "", cmd: "findmnt ..." }`. Assert `probe_pool` returns `Err(ProbeError::MountInfo(MountInfoError::Io(_)))`. **Regression guard for the bug** -- pre-fix this fixture produces `Ok(mounted: false)`; post-fix it produces `Err`. Must fail before the fix, pass after.
- `probe_pool_propagates_mountinfo_malformed_line` -- MockFs serves a mountinfo body where the target line is missing the `-` separator; runner has the same stale `FindmntJson` mock as above (exit 1, empty stderr/stdout). Assert `probe_pool` returns `Err(ProbeError::MountInfo(MountInfoError::Malformed { .. }))`. Pins malformed-line propagation; same dual-seed rationale as above.

### `cli/src/monitor.rs` -- new fail-closed test

- `cmd_monitor_latches_computation_error_on_mountinfo_io_failure` -- `MockFs.read_to_string("/proc/self/mountinfo")` returns `Err(PermissionDenied)`; runner additionally has a stale `CmdRequest::FindmntJson` mock with `RawCommandOutput { exit_status: 1, stderr: "", stdout: "", cmd: "findmnt ..." }` (the *exact* shape that previously silenced the alert). Assert `cmd_monitor` returns `MonitorResult::Alert(_)` with a `ComputationError` cause and that the alert latch is persisted. **Regression guard for the bug at the safety-critical callsite** -- pre-fix this fixture produces `MonitorResult::PoolOffline` (exit 0, silent); post-fix it produces `Alert(ComputationError)`. The dual seed pins the exact behavioral diff this plan exists to produce.

The existing `monitor.rs` test that asserts `PoolOffline` for the unmounted case (per Explore: `monitor_classifies_non_btrfs_mount_as_offline` and similar) should be retargeted to seed mountinfo (well-formed, target absent) instead of the `findmnt_empty()` mock. Behavior asserted is unchanged; the seed mechanism shifts.

### `cli/src/{ack,add,remove,replace,remove_missing,status,recover,unlock,pool}.rs` and `cli/src/tui/probe.rs` -- mechanical retarget

For every existing test that mocks `CmdRequest::FindmntJson` to drive `probe_pool`'s mount detection: replace the `FindmntJson` mock with a `MockFs` mountinfo seed. Where the test mocks the *unmounted* case (`err_raw("findmnt", 1, "")`), seed mountinfo with the target absent. Where the test mocks the *mounted* case, seed mountinfo with the target as btrfs. Keep all other mocks (`BtrfsFilesystemShow`, `CryptsetupStatus`, etc.) unchanged. The TUI test file `tui/probe.rs` has eight such call sites (lines 825, 987, 1126, 1271, 1351, 1418, 1496, 1564 in the current tree); each gets the same retarget.

No new behavioral tests in these files -- the new behavior (fail-closed on mountinfo error) is pinned in `probe.rs` and `monitor.rs` above. The retargets here just keep the existing test set green under the signature change.

### VM tests

No new VM tests. The bug is reachable only via a misbehaving findmnt or an unreadable `/proc/self/mountinfo`, neither of which can be staged in a NixOS VM without injecting kernel-level faults. The unit-test regression contract above is the durable guarantee. Run the existing VM suite to confirm no regression:

- `just test-vm braid-monitor monitor-hot-unplug braid-smartd-alert` -- existing alert-path tests (per Explore inventory of `tests/cli/braid-monitor.nix`, `tests/cli/monitor-hot-unplug.nix`, `tests/cli/braid-smartd-alert.nix`).
- `just test-vm` -- full sweep, to catch any unanticipated regression in mutation paths or status display.

## Verification

1. `just test-rust` -- new `mount_check::fstype_at_mount_via_fs_*` tests, retargeted `probe_pool` tests, new `probe_pool_propagates_mountinfo_*` regression tests, new `cmd_monitor_latches_computation_error_on_mountinfo_io_failure` regression test, and all retargeted `cmd_*` tests pass.
2. `just test-vm braid-monitor monitor-hot-unplug braid-smartd-alert braid-doctor braid-doctor-beep` -- targeted alert-path tests pass.
3. `just test-vm` -- full suite passes (no regression in mutation flows, status, recover, unlock).
4. Manual sanity in a VM with mounted pool:
   - `braid monitor; echo $?` -> 0, no latch written.
   - `umount /mnt/storage; braid monitor; echo $?` -> 0, no latch written (well-formed mountinfo, target absent = legitimate offline).
   - There is no production way to make `/proc/self/mountinfo` unreadable; the unit test is the contract for that path.

## Follow-ups (out of scope, must be tracked)

- Harden `probe_fsid` (`probe.rs:354-387`). Currently fail-closed at its callsite via `Err(ProbeError::PoolDevice)` on absent target, so safety is preserved; but it's the last `parse_findmnt_json` consumer that depends on the lenient empty-stderr branch. Routing it through `mount_check` (or a new mountinfo-driven `probe_fsid`) would let us delete the lenient branch entirely and remove the entire `FindmntJson` `CmdRequest` variant, since `check_not_read_only` could read mount options from mountinfo as well.
- After both `probe_pool` (this plan) and `probe_fsid` (above follow-up) are converted: tighten `parse_findmnt_json` -- or remove it. At that point only `check_not_read_only` would still use it, and it can move to mountinfo (mountinfo line 6 contains the per-mount options field already).
- Mount-point validation in `cli/src/config.rs` / `modules/braid/options.nix` to reject paths with whitespace. The mountinfo octal-escape decoder already handles this correctly, so this is a UX-improvement plan, not a correctness plan. Same status as in the `cmd_idle` plan.
