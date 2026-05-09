# Plan: Migrate `cli/src/ack.rs` test scaffolding to `test_fixtures::ack`

**Status: Draft**

## Goals

Simplify `cli/src/ack.rs::tests` by moving repeated setup into a focused
`cli/src/test_fixtures/ack.rs` module while preserving the behavior contracts
that make these tests valuable:

- offline ack must not invoke the command runner
- mounted no-op ack must not query `BtrfsDeviceStatsJson` or write
  `acked-stats.json`
- corrupt latch on a mounted pool must still run the full ack path
- mounted smartd-only state must run the full ack path when appropriate
- mid-probe smartd races must preserve or remove the flag according to the
  entry snapshot contract
- mounted btrfs-error ack must update `acked-stats.json` while preserving
  late smartd flags when required
- foreign filesystem paths must surface probe errors and preserve state
- foreign filesystem paths must not invoke the beeper cleanup hook
- offline missing-device ack must persist `missing_acked`
- offline mixed missing-device plus btrfs-error latch must be refused
- offline smartd-only and computation-error-only latches must not load
  corrupt `acked-stats.json`
- cleanup failure after a baseline or state write must surface
  `AckError::CleanupFailed`
- corrupt acked-stats and corrupt latch cases must preserve their current
  error/cleanup behavior

This is a test-side refactor only. Do not change `cmd_ack`, `ack_offline`, or
`cleanup_alert_files_and_beeper` behavior.

## Current-State Inventory

`cli/src/ack.rs` has 24 tests in the `#[cfg(test)] mod tests` block:
22 ack behavior tests plus 2 focused `format_systemctl_stop_failure` tests.

### Local helpers

| Helper | Lines | Role | Plan |
|---|---:|---|---|
| `MOUNTINFO_EXT4` / `MOUNTINFO_BTRFS` | 277-280 | Ack-specific mountinfo bodies. The ext4 body includes `rw,noatime`; the btrfs body uses `/dev/mapper/braid-disk1`. | Move private to `test_fixtures::ack`. |
| `PanicRunner` | 282-294 | Sharp no-run sentinel for offline paths. Panics on both `run` and `run_with_stdin`. | Promote as `AckPanicRunner`; preserve panic text or equivalent clarity. |
| `Ext4Fs` | 297-313 | Strict mounted non-btrfs filesystem. Asserts only `/proc/self/mountinfo` is read. | Promote as `ack_fs_ext4()` or an `AckExt4Fs` type. |
| `NotMountedFs` | 315-331 | Strict offline filesystem. Asserts only `/proc/self/mountinfo` is read. | Promote as `ack_fs_not_mounted()`. |
| `BtrfsFs` | 333-349 | Strict mounted-btrfs filesystem. Asserts only `/proc/self/mountinfo` is read. | Promote as `ack_fs_btrfs()`. |
| `OfflineFsThatTouchesSmartd` | 351-370 | Race simulator: writes `smartd-alert` while offline mountinfo is read. | Promote with an explicit name, e.g. `ack_offline_fs_that_touches_smartd(paths)`. Do not hide the race behind a generic knob. |
| `MountedFsThatTouchesSmartd` | 372-391 | Race simulator: writes `smartd-alert` while mounted btrfs mountinfo is read. | Promote with an explicit name, e.g. `ack_mounted_fs_that_touches_smartd(paths)`. |
| `mp` | 393-395 | Canonical `/mnt/storage` mount point. | Promote as `ack_mp()`. Do not reuse `status_mp`; the name would couple ack tests to status. |
| `fresh_paths` | 397-401 | Bare tempdir plus `StatePaths::custom`. | Replace call sites with existing facade `isolated_paths()`. No new helper needed. |
| `write_latch` | 403-406 | Writes `AlertState { causes }` to the alert latch. | Promote as `ack_write_latch(paths, causes)`. Ack-specific because callers use alert causes heavily. |
| `ok_raw` | 408-415 | `RawCommandOutput` builder with stdout. | Do not export. Use `shared::mock_ok` inside `test_fixtures::ack`. |
| `btrfs_show_2disk` | 417-425 | Ack-specific 2-row btrfs show body with devids 1 and 3. | Move private to `test_fixtures::ack`. |
| `cryptsetup_status_active` | 427-438 | Active LUKS2 status body. | Move private to `test_fixtures::ack`. Do not reuse status helper. |
| `cryptsetup_uuid_ok` | 440-445 | `cryptsetup luksUUID` body. | Move private to `test_fixtures::ack`. Do not reuse status helper. |
| `btrfs_device_stats_healthy` | 447-473 | Ack-specific device stats JSON for devids 1 and 3. | Move private to `test_fixtures::ack`. Do not reuse status's 3-disk stats. |
| `mounted_probe_runner` | 475-504 | Mounted probe runner without device stats. This absence is load-bearing for no-op tests. | Promote as `ack_mounted_probe_runner()`. |
| `mounted_probe_runner_with_device_stats` | 507-514 | Mounted probe runner plus healthy device stats. | Promote as `ack_mounted_probe_runner_with_device_stats()`. |

### Existing fixture modules relevant to ack

- `shared::mock_ok` matches ack's local `ok_raw(cmd, stdout)` shape. Use it
  internally in `test_fixtures::ack`; no facade export needed.
- `doctor::isolated_paths` is already re-exported by the facade and is used by
  migrated `status`, `unlock`, and `enroll_key_file` tests. Use it in ack
  tests instead of adding another `fresh_paths` wrapper.
- `status` has superficially similar helpers (`status_fs_ext4`,
  `status_cryptsetup_status_active`, `status_cryptsetup_uuid_ok`,
  `status_btrfs_device_stats_3disk`), but their names, mapper spelling,
  mountinfo bodies, and stats rows are status-shaped. Do not reuse them.
- `mount::ok_raw` has a different signature (`cmd` only, empty stdout), so it
  is not a replacement for ack's local `ok_raw`.
- `shared::MockFs` can model mountinfo, but it also answers
  `*/exclusive_operation` and `/dev/mapper` reads. Ack's current filesystem
  mocks are stricter and assert that only `/proc/self/mountinfo` is read.
  Keep an ack-scoped strict mock instead of reusing `shared::MockFs`.

## Proposed Fixture Shape

Create `cli/src/test_fixtures/ack.rs` as a flat, ack-scoped module. Register it
in `cli/src/test_fixtures.rs` with `mod ack;` and facade re-exports.

Do not create an `AckPool`, topology installer, params builder, or broad
`MockRunner::with_handler`. Ack tests are about exact branch gates, exact state
file side effects, exact command absence, and cleanup hooks. A broad topology
would make it too easy for a future production command to be silently answered
instead of surfacing through a missing mock or request-list assertion.

### Public fixture surface

```rust
pub(crate) struct AckPanicRunner;

pub(crate) fn ack_mp() -> MountPoint;

pub(crate) fn ack_write_latch(paths: &StatePaths, causes: Vec<AlertCause>);

pub(crate) fn ack_fs_btrfs() -> impl Filesystem;
pub(crate) fn ack_fs_not_mounted() -> impl Filesystem;
pub(crate) fn ack_fs_ext4() -> impl Filesystem;

pub(crate) fn ack_offline_fs_that_touches_smartd<'a>(
    paths: &'a StatePaths,
) -> impl Filesystem + 'a;

pub(crate) fn ack_mounted_fs_that_touches_smartd<'a>(
    paths: &'a StatePaths,
) -> impl Filesystem + 'a;

pub(crate) fn ack_mounted_probe_runner() -> MockRunner;

pub(crate) fn ack_mounted_probe_runner_with_device_stats() -> MockRunner;
```

Implementation notes:

- The filesystem helpers should use a private strict struct whose
  `read_to_string` accepts only `/proc/self/mountinfo`, `is_block_device`
  returns false, and `list_dir` returns `Ok(vec![])`. This keeps the existing
  "unexpected filesystem read" guard.
- The smartd race helpers should be named exactly for what they do. Do not
  collapse them into `ack_fs_btrfs_with_side_effect(...)` or a closure hook
  unless the call sites remain just as obvious.
- `ack_mounted_probe_runner()` must not seed `BtrfsDeviceStatsJson`. The
  no-op tests prove that command is absent.
- `ack_mounted_probe_runner_with_device_stats()` should be a thin extension
  over `ack_mounted_probe_runner()` that adds only the healthy stats response.
- The raw output factories (`btrfs_show_2disk`,
  `cryptsetup_status_active`, `cryptsetup_uuid_ok`,
  `btrfs_device_stats_healthy`) can stay private to the fixture module until
  a test needs to vary one directly.

### Facade exports

Add a small ack block to `cli/src/test_fixtures.rs`:

```rust
mod ack;

#[allow(unused_imports)]
pub(crate) use ack::{
    AckPanicRunner, ack_fs_btrfs, ack_fs_ext4, ack_fs_not_mounted,
    ack_mounted_fs_that_touches_smartd, ack_mounted_probe_runner,
    ack_mounted_probe_runner_with_device_stats, ack_mp,
    ack_offline_fs_that_touches_smartd, ack_write_latch,
};
```

Update the module-level comment in `cli/src/test_fixtures.rs` to mention the
new ack scope and why it is flat: exact no-run, no-device-stats, race, and
state-file contracts.

### What stays local in `cli/src/ack.rs::tests`

- The two `format_systemctl_stop_failure` tests stay local. They do not use
  ack fixtures, and the Unix `ExitStatusExt` setup is clearer next to the
  formatter under test.
- Inline corrupt-file writes stay inline:
  `std::fs::write(paths.alert_latch_json(), b"not json")` and
  `std::fs::write(paths.acked_stats_json(), b"not json")` are the behavior
  being tested.
- The one-test `AckedStats` baseline construction in
  `ack_offline_preserves_existing_device_stats_baseline` stays inline. The
  exact `read_io_errs = 7` baseline is the assertion setup, not generic
  scaffolding.
- The beeper `Cell` closures stay local in tests that assert hook invocation
  or non-invocation.
- The cleanup-failure setup using a directory at `smartd_alert()` stays inline.
  The platform note about `EISDIR` / `EPERM` is part of the test's intent.

## Staged Migration

Each sub-commit should keep `cargo test --manifest-path cli/Cargo.toml --lib
ack::tests` and `just test-rust` green.

| # | Commit subject | Scope | Focused verification |
|---:|---|---|---|
| 1 | `test(ack): add ack fixture module` | Add `cli/src/test_fixtures/ack.rs`, register facade exports, update `test_fixtures.rs` module doc comment. No `ack.rs` call sites change yet. | `cargo check --manifest-path cli/Cargo.toml --tests`; `cargo test --manifest-path cli/Cargo.toml --lib ack::tests`; `just test-rust` |
| 2 | `test(ack): migrate mounted no-op and full-path fixtures` | Migrate `cmd_ack_noop_when_no_alerts_does_not_query_btrfs_or_write_acked_stats`, `cmd_ack_with_mounted_pool_and_corrupt_latch_runs_full_ack_path`, and `cmd_ack_with_mounted_pool_and_smartd_flag_no_latch_runs_full_ack_path` to `isolated_paths`, `ack_fs_btrfs`, `ack_mp`, and the two mounted runner helpers. Preserve the no-op test's runner without device stats. | Run the three tests by name, then `cargo test --manifest-path cli/Cargo.toml --lib ack::tests`. |
| 3 | `test(ack): migrate smartd race filesystem fixtures` | Migrate the six mid-probe smartd race tests to `ack_offline_fs_that_touches_smartd`, `ack_mounted_fs_that_touches_smartd`, `AckPanicRunner`, and the mounted runner helpers. Preserve every assertion about whether `smartd-alert` remains or is removed. | `cargo test --manifest-path cli/Cargo.toml --lib ack::tests::smartd` is not a valid exact module filter for every test, so run the six tests by name or run `cargo test --manifest-path cli/Cargo.toml --lib ack::tests`. |
| 4 | `test(ack): migrate foreign filesystem probe boundaries` | Migrate `cmd_ack_with_foreign_fstype_and_alerts_returns_probe_error_and_preserves_state`, `cmd_ack_with_foreign_fstype_and_no_alerts_returns_probe_error`, `cmd_ack_with_foreign_fstype_and_corrupt_latch_preserves_latch_bytes`, and `cmd_ack_impl_with_foreign_fstype_does_not_invoke_beeper` to `ack_fs_ext4`, `AckPanicRunner`, `ack_write_latch`, and `ack_mp`. Keep the beeper `Cell` local. | Run the four tests by name, then `cargo test --manifest-path cli/Cargo.toml --lib ack::tests`. |
| 5 | `test(ack): migrate offline missing-device ack contracts` | Migrate `ack_offline_with_missing_device_cause_marks_missing_acked`, `ack_offline_refuses_when_btrfs_errors_mixed_with_missing`, and `ack_offline_preserves_existing_device_stats_baseline` to `ack_fs_not_mounted`, `AckPanicRunner`, `ack_write_latch`, and `ack_mp`. Keep the baseline `BTreeMap` inline. | Run the three tests by name, then `cargo test --manifest-path cli/Cargo.toml --lib ack::tests`. |
| 6 | `test(ack): migrate cleanup-failure contracts` | Migrate `cmd_ack_returns_cleanup_failed_when_remove_smartd_alert_errors_after_baseline_saved` and `ack_offline_cleanup_failure_after_missing_acked_returns_cleanup_failed`. Keep the directory-at-`smartd_alert` setup and partial-state assertions inline. | Run both tests by name, then `cargo test --manifest-path cli/Cargo.toml --lib ack::tests`. |
| 7 | `test(ack): migrate corrupt and skip-load offline gates` | Migrate `ack_offline_corrupt_latch_still_clears_files`, `ack_offline_corrupt_acked_stats_propagates_io_error_when_missing_cause`, `ack_offline_smartd_only_latch_does_not_load_acked_stats`, and `ack_offline_computation_error_only_latch_does_not_load_acked_stats`. Preserve inline corrupt bytes and byte-equality assertions. | Run the four tests by name, then `cargo test --manifest-path cli/Cargo.toml --lib ack::tests`. |
| 8 | `refactor(ack): delete local test scaffolding` | Delete the local mountinfo constants, `PanicRunner`, filesystem mocks, `mp`, `fresh_paths`, `write_latch`, raw output helpers, and mounted runner helpers from `ack.rs::tests`. Leave `format_systemctl_stop_failure` tests and their Unix imports local. Clean up unused imports. | `cargo check --manifest-path cli/Cargo.toml --tests`; `cargo test --manifest-path cli/Cargo.toml --lib ack::tests`; `just test-rust` |

## Risks

- **Hidden device-stats query.** The no-op test depends on `BtrfsDeviceStatsJson`
  being absent. Mitigation: keep separate `ack_mounted_probe_runner()` and
  `ack_mounted_probe_runner_with_device_stats()` helpers; never seed device
  stats in the base mounted probe runner.
- **Weakening no-run coverage.** Replacing `PanicRunner` with
  `MockRunner::default()` would turn "must not run" into a weaker missing-mock
  failure only if a command happens to execute. Mitigation: keep
  `AckPanicRunner` and use it for all offline no-probe assertions.
- **Accidental cross-command coupling.** Status and mount have similar helper
  names, but their bodies are not ack-shaped. Mitigation: duplicate the small
  ack-specific bodies in `test_fixtures::ack` and use only `shared::mock_ok`
  plus facade `isolated_paths`.
- **Race intent becomes opaque.** The `*FsThatTouchesSmartd` helpers model a
  precise race. Mitigation: promote them under explicit names and keep the test
  assertions local.
- **State-file side effects get abstracted away.** The tests rely on exact
  alert latch, corrupt latch, smartd flag, and acked-stats outcomes.
  Mitigation: fixture helpers should prepare only common shape; file writes
  that encode a test's scenario stay inline.
- **Cleanup hook behavior gets hidden.** The foreign-fstype beeper test and
  offline success hook test use local closures to count calls. Mitigation:
  keep those closures in the tests, not in the fixture module.

## Verification

Use filtered Rust tests during each sub-commit:

```sh
cargo test --manifest-path cli/Cargo.toml --lib ack::tests::<test_name>
cargo test --manifest-path cli/Cargo.toml --lib ack::tests
```

At every sub-commit boundary, run:

```sh
just test-rust
```

Use `cargo check --manifest-path cli/Cargo.toml --tests` after adding the
fixture module and after deleting local scaffolding to catch unused imports,
dead references, and facade wiring errors.

No VM tests or fixture refresh are required. This migration touches only
Rust test scaffolding and should not alter parser fixtures, NixOS modules, or
runtime behavior.
