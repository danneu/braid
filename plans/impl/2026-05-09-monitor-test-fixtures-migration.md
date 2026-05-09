# Plan: Migrate `cli/src/monitor.rs` test scaffolding to `test_fixtures::monitor`

**Status: Draft**

## Goals

Simplify `cli/src/monitor.rs::tests` by moving repeated monitor-shaped setup
into a focused `cli/src/test_fixtures/monitor.rs` module while preserving the
contracts that make the current tests useful:

- `cmd_monitor` reconciles `acked-stats.json` across present,
  null-underlying, btrfs `MISSING`, and orphan devids.
- A stale zero-counter mapper row in `btrfs device stats` remains benign and
  does not latch a `ComputationError`.
- Every indeterminate probe or stats failure latches exactly one
  `ComputationError`, writes `alert-latch.json`, and returns
  `MonitorResult::Alert`.
- `/proc/self/mountinfo` read failures fail closed as an active alert.
- Mounted non-btrfs filesystems remain the single offline-classified probe
  branch and must not write an alert latch.
- State-file side effects stay visible through real disk reads after
  `cmd_monitor`, not only through returned values.

This is a test-side refactor only. Do not change `cmd_monitor`,
`latch_computation_error`, `probe_pool`, alert merging, or acked-stats
reconciliation behavior.

## Current-State Inventory

`cli/src/monitor.rs` is 753 lines. The `#[cfg(test)] mod tests` block starts at
line 158 and contains 8 tests plus roughly 275 lines of local scaffolding.

### Local Helpers

| Helper | Lines | Role | Plan |
|---|---:|---|---|
| `MOUNTINFO_BTRFS` / `MOUNTINFO_EXT4` | 163-165 | Monitor-specific mountinfo bodies. The btrfs source is `/dev/mapper/braid-vdb`, matching the monitor topology. | Move private to `test_fixtures::monitor`, exposed through strict FS helpers. Do not reuse `status_fs_*`, `ack_fs_*`, or `shared::MockFs`. |
| `BTRFS_SHOW_2DISK` | 168-171 | Healthy 2-disk monitor pool with `braid-vdb` and `braid-vdc`. | Move private to the monitor fixture runner. |
| `CRYPTSETUP_STATUS_VDB` / `CRYPTSETUP_STATUS_VDC` | 173-193 | Active LUKS2 status outputs for the healthy 2-disk topology. | Move private to the monitor fixture runner. |
| `LUKS_UUID` | 195 | Shared UUID body used by both healthy monitor mappers. | Move private to the monitor fixture runner. |
| `STATS_2DISK_HEALTHY` | 197-203 | Healthy zero-counter stats for devids 1 and 2. | Move private to the default monitor runner. |
| `STATS_WITH_STALE_MAPPER` | 205-212 | Zero-counter stats with extra unknown devid 99. | Move private behind a clearly named stale-stats runner constructor. |
| `BTRFS_SHOW_PRESENT_NULL_MISSING` | 214-218 | Reconciliation topology: devid 1 present, devid 2 null-underlying, devid 3 btrfs `MISSING`. | Move private to `MonitorReconcileRunner`. Do not reuse status's mixed missing fixture; monitor's mapper names and semantics differ. |
| `CRYPTSETUP_STATUS_VDC_NULL` | 220-229 | Active mapper with `(null)` backing device for null-underlying reconciliation. | Move private to `MonitorReconcileRunner`. |
| `ok_output` | 231-238 | Local `RawCommandOutput` success builder with a dummy `cmd`. | Do not facade-export. Use `shared::mock_ok` inside the fixture module, or keep a private `ok_output` wrapper there. |
| `Override` | 242-246 | One-shot failure/payload override for the healthy runner. | Promote as `MonitorOverride` or equivalent. Keep one-shot semantics so a test cannot accidentally reuse an override. |
| `MonitorTestRunner` | 248-334 | Strict healthy runner with optional single override; panics on unexpected requests. | Promote mostly as-is. This is the main monitor runner fixture. Do not replace with a broad topology handler. |
| `MonitorReconcileRunner` | 337-362 | Strict runner for the present/null/missing acked-stats reconciliation topology. | Promote mostly as-is. Keep distinct from the healthy runner so the reconciliation shape is explicit. |
| `MonitorFs` | 365-408 | Filesystem mock whose `read_to_string` asserts `/proc/self/mountinfo`, but whose `exists`, `is_block_device`, and `list_dir` currently return broad defaults. | Move to the monitor fixture module as a private type with exported constructors, and tighten it so `exists`, `is_block_device`, and `list_dir` panic. |
| `mp` | 410-412 | Canonical `/mnt/storage` mount point. | Promote as `monitor_mp()`. Do not import `status_mp` or `ack_mp`; the name should stay monitor-scoped. |
| `fresh_paths` | 414-418 | Bare tempdir plus `StatePaths::custom`. | Reuse the existing facade export `isolated_paths()`. Do not add another wrapper. |
| `acked_disk` | 420-428 | Builds exact `AckedDisk` baselines for the reconciliation test. | Keep local in `monitor.rs::tests`. It is one-test assertion setup, not shared monitor topology. |
| `assert_single_computation_error` | 430-449 | Repeated monitor-specific assertion over `MonitorResult::Alert`. | Promote as `assert_monitor_single_computation_error()`. It is a fixture-level assertion helper like lock's dry-run helpers. |

### Behavior Families

| Family | Tests | Migration concern |
|---|---|---|
| Acked-stats reconciliation | `cmd_monitor_reconciles_acked_stats_across_pool_axes` | Preserve the real `save_acked_stats` seed, real reload from disk, and exact key/value assertions. |
| Benign stale stats row | `stale_mapper_row_no_longer_latches_computation_error` | Preserve the stale devid 99 stats row and the "no latch file exists" assertion. |
| Probe fail-closed paths | `probe_error_returns_alert_with_latched_computation_error`, `probe_parse_failure_returns_alert_with_latched_computation_error`, `probe_pool_device_failure_returns_alert_with_latched_computation_error` | Preserve distinct `ProbeError::Cmd`, `ProbeError::Parse`, and `ProbeError::PoolDevice` coverage. |
| Stats fail-closed paths | `stats_path_failures_return_alert_with_latched_computation_error` | Preserve both runner-level `CmdError` and malformed-JSON parser failure cases. |
| Mountinfo / fstype classification | `cmd_monitor_latches_computation_error_on_mountinfo_io_failure`, `monitor_classifies_non_btrfs_mount_as_offline` | Preserve fail-closed mountinfo IO behavior and the single non-alerting non-btrfs branch. |

## Existing Fixture Modules

- `status` has superficially similar btrfs-show, stats, cryptsetup, and
  mountinfo helpers, but they are status-shaped: 1-disk or 3-disk topologies,
  `disk1` mapper names, verbose-status probes, and status-specific JSON
  surface. Do not reuse them for monitor.
- `ack` has strict mountinfo filesystem helpers and ack-shaped two-disk
  outputs, but its topology uses `braid-disk1` and `braid-disk3` with devids
  1 and 3. Monitor needs `braid-vdb` / `braid-vdc` with null-underlying and
  stale-mapper variants. Do not couple monitor tests to ack helper names.
- `mount`, `unlock`, and `lock` demonstrate the pattern monitor should follow:
  flat helpers, scoped names, strict request surfaces, and no broad handler
  when a test depends on a missing mock or exact enum/request shape.
- `replace` is the opposite shape: a broad topology installer is useful for a
  large mutating workflow with state flips. Monitor has only eight tests and
  several fail-closed branch assertions, so that pattern would be too broad.
- `shared::MockFs` can technically model custom mountinfo, but it also answers
  sysfs exclusive-operation reads and `/dev/mapper` listings. Monitor's
  promoted FS should be stricter than today's local `MonitorFs`: only
  `read_to_string("/proc/self/mountinfo")` is supported; `exists`,
  `is_block_device`, and `list_dir` should panic because `cmd_monitor`'s
  current `probe_pool` path reaches only the mountinfo read through
  `mount_check::fstype_at_mount_via_fs`.
- `doctor::isolated_paths` is already used as a cross-scope path fixture by
  other migrated tests. Reuse it through the facade. Moving it to `shared` can
  be a later cleanup, but it is not required for this migration.

## Proposed Fixture Shape

Create `cli/src/test_fixtures/monitor.rs` as a flat monitor-scoped module.
Register it in `cli/src/test_fixtures.rs` with `mod monitor;` and facade
re-exports.

Do not create a `MonitorPool`, params builder, or catch-all
`MockRunner::with_handler` topology. The existing local runners are already
small and strict. Promoting them keeps the unexpected-request panic behavior
that protects fail-closed tests from silently accepting new command probes.

### Public Fixture Surface

```rust
pub(crate) enum MonitorOverride {
    BtrfsShowResult(Result<RawCommandOutput, CmdError>),
    BtrfsShowPayload(String),
    StatsResult(Result<RawCommandOutput, CmdError>),
}

pub(crate) struct MonitorTestRunner;

impl MonitorTestRunner {
    pub(crate) fn with_stale_mapper_stats() -> Self;
    pub(crate) fn with_override(override_op: MonitorOverride) -> Self;
}

pub(crate) struct MonitorReconcileRunner;

pub(crate) fn monitor_mp() -> MountPoint;

pub(crate) fn monitor_fs_btrfs() -> impl Filesystem;
pub(crate) fn monitor_fs_ext4() -> impl Filesystem;
pub(crate) fn monitor_fs_mountinfo_error(kind: std::io::ErrorKind) -> impl Filesystem;

pub(crate) fn assert_monitor_single_computation_error(result: &MonitorResult) -> &str;
```

Implementation notes:

- `MonitorTestRunner` should preserve the current one-shot override behavior.
  If an override is consumed once, later matching requests should fall back to
  the healthy default. This keeps failure injection explicit.
- `MonitorTestRunner` and `MonitorReconcileRunner` should continue to panic on
  unexpected `CmdRequest` variants and unexpected mappers.
- `run_with_stdin` should keep returning `CmdError::MissingMock` or delegate
  to `run` only where today's local runner already does. Monitor should not
  start accepting stdin-bearing requests.
- The strict filesystem type should stay private. Export constructors rather
  than the concrete type, matching the status and ack fixture pattern. Unlike
  today's local `MonitorFs`, the promoted type should panic on `exists`,
  `is_block_device`, and `list_dir`; only
  `read_to_string("/proc/self/mountinfo")` should return a value or the
  configured mountinfo error.
- The raw output bodies should stay private unless a test needs to vary them
  directly. The only public variation point should be the override enum.

### Facade Exports

Add a small block to `cli/src/test_fixtures.rs`:

```rust
mod monitor;

#[allow(unused_imports)]
pub(crate) use monitor::{
    MonitorOverride, MonitorReconcileRunner, MonitorTestRunner,
    assert_monitor_single_computation_error, monitor_fs_btrfs, monitor_fs_ext4,
    monitor_fs_mountinfo_error, monitor_mp,
};
```

Update the module-level comment in `cli/src/test_fixtures.rs` with one
monitor bullet: flat monitor helpers, strict probe runners, fail-closed and
state-file contracts.

### Staging Import Rule

During sub-commits 2-5, `monitor.rs::tests` still contains local
`MonitorTestRunner` and `MonitorReconcileRunner` types. Migrated tests must
import the promoted runners under temporary aliases so imports compile and
unqualified names cannot keep resolving to the old local scaffolding:

```rust
use crate::test_fixtures::{
    MonitorReconcileRunner as FixtureMonitorReconcileRunner,
    MonitorTestRunner as FixtureMonitorTestRunner,
};
```

Use `FixtureMonitorReconcileRunner` and `FixtureMonitorTestRunner` at every
migrated call site until commit 6 deletes the local types. The cleanup commit
can either keep those aliases or simplify them after the collision is gone;
do not use bare `MonitorTestRunner` / `MonitorReconcileRunner` during the
staged migration.

### What Stays Local

- The `acked_disk(missing_acked, read_io_errs)` helper stays in
  `monitor.rs::tests`. It is used by one reconciliation test to make the
  exact on-disk baseline and post-monitor values readable. Promoting it would
  add fixture surface without reducing repeated topology.
- The `BTreeMap` construction, `save_acked_stats`, reload, key-order
  assertion, and per-devid assertions stay inline in
  `cmd_monitor_reconciles_acked_stats_across_pool_axes`. Those are the state
  side effects under test.
- The raw non-zero `RawCommandOutput` in
  `probe_parse_failure_returns_alert_with_latched_computation_error` stays
  inline because the exit status plus stderr shape is the parser contract.
- The non-`/dev/mapper` btrfs-show payload in
  `probe_pool_device_failure_returns_alert_with_latched_computation_error`
  stays inline because the malformed pool-device path is the assertion setup.
- Each test keeps its latch-file existence or non-existence assertion inline.
  The fixture may assert the returned `MonitorResult`, but it should not hide
  the disk side effect.

### What Does Not Go In `shared`

No new `shared` helper is required for this migration.

- The monitor FS is intentionally stricter than `shared::MockFs`, and stricter
  than the current local default-method behavior for `exists`,
  `is_block_device`, and `list_dir`.
- The monitor outputs are tied to `braid-vdb` / `braid-vdc`, stale mapper, and
  null-underlying semantics.
- `assert_monitor_single_computation_error` depends on `MonitorResult`, so it
  belongs to monitor, not shared.
- `isolated_paths` is already available through the facade. Moving it from
  `doctor` to `shared` would be reasonable someday, but this plan keeps the
  migration focused.

## Staged Migration

Each sub-commit should keep
`cargo test --manifest-path cli/Cargo.toml --lib monitor::tests` and
`just test-rust` green.

| # | Commit subject | Scope | Focused verification |
|---:|---|---|---|
| 1 | `test(monitor): add monitor fixture module` | Add `cli/src/test_fixtures/monitor.rs`, register facade exports, and update the fixture facade doc comment. No `monitor.rs` call sites change yet. | `cargo check --manifest-path cli/Cargo.toml --tests`; `cargo test --manifest-path cli/Cargo.toml --lib monitor::tests`; `just test-rust` |
| 2 | `test(monitor): migrate acked-stats reconciliation topology` | Migrate `cmd_monitor_reconciles_acked_stats_across_pool_axes` to `isolated_paths`, `monitor_mp`, `monitor_fs_btrfs`, and `FixtureMonitorReconcileRunner` (alias for the promoted `MonitorReconcileRunner`). Keep `acked_disk` and all state-file assertions local. | Run the one test by name, then `cargo test --manifest-path cli/Cargo.toml --lib monitor::tests`. |
| 3 | `test(monitor): migrate stats-path fixtures` | Migrate `stale_mapper_row_no_longer_latches_computation_error` and `stats_path_failures_return_alert_with_latched_computation_error` to `FixtureMonitorTestRunner` (alias for the promoted `MonitorTestRunner`), `MonitorOverride`, `monitor_fs_btrfs`, `monitor_mp`, `isolated_paths`, and `assert_monitor_single_computation_error`. Preserve the benign stale row and both stats failure cases. | Run the two tests by name, then `cargo test --manifest-path cli/Cargo.toml --lib monitor::tests`. |
| 4 | `test(monitor): migrate fail-closed probe fixtures` | Migrate `probe_error_returns_alert_with_latched_computation_error`, `cmd_monitor_latches_computation_error_on_mountinfo_io_failure`, `probe_parse_failure_returns_alert_with_latched_computation_error`, and `probe_pool_device_failure_returns_alert_with_latched_computation_error` using the aliased fixture runners. Keep the non-zero btrfs-show output and non-mapper payload inline. | Run the four tests by name, then `cargo test --manifest-path cli/Cargo.toml --lib monitor::tests`. |
| 5 | `test(monitor): migrate non-btrfs offline fixture` | Migrate `monitor_classifies_non_btrfs_mount_as_offline` to `monitor_fs_ext4`, `monitor_mp`, `isolated_paths`, and `FixtureMonitorTestRunner`. Preserve the no-latch assertion. | Run the one test by name, then `cargo test --manifest-path cli/Cargo.toml --lib monitor::tests`. |
| 6 | `refactor(monitor): delete local test scaffolding` | Delete local mountinfo constants, btrfs/cryptsetup/stats constants, `ok_output`, `Override`, `MonitorTestRunner`, `MonitorReconcileRunner`, `MonitorFs`, `mp`, `fresh_paths`, and `assert_single_computation_error` from `monitor.rs::tests`. Leave `acked_disk` local. Clean unused imports. | `cargo check --manifest-path cli/Cargo.toml --tests`; `cargo test --manifest-path cli/Cargo.toml --lib monitor::tests`; `just test-rust` |

## Risks

- **Fail-closed masking from a broad runner.** If the fixture answers too many
  request shapes, a future production probe could be silently accepted.
  Mitigation: promote the strict local runners mostly as-is and keep panics for
  unexpected requests and mappers.
- **Runner-name collisions during staging.** Bare imports of promoted runner
  names collide with the still-local `MonitorTestRunner` and
  `MonitorReconcileRunner` during sub-commits 2-5, or unqualified uses keep
  calling the local scaffolding. Mitigation: import promoted runners as
  `FixtureMonitorTestRunner` and `FixtureMonitorReconcileRunner` until the
  cleanup commit removes the locals.
- **Filesystem broad-mock hole.** Today's local `MonitorFs` returns defaults
  from `exists`, `is_block_device`, and `list_dir`, which could hide a future
  filesystem probe. Mitigation: the promoted private FS panics on those
  methods and supports only `read_to_string("/proc/self/mountinfo")`.
- **Cross-command topology drift.** Status and ack have similar-looking
  fixtures, but their mapper names, device counts, and missing-device semantics
  do not match monitor. Mitigation: duplicate the small monitor-specific
  outputs in `test_fixtures::monitor`.
- **State-file assertions get hidden.** The acked-stats and alert-latch tests
  are valuable because they check files after `cmd_monitor`. Mitigation: keep
  writes, reloads, and file-existence assertions in the tests.
- **`NotBtrfs` accidentally becomes fail-closed.** The non-btrfs mounted
  filesystem is intentionally classified as `PoolOffline`. Mitigation: keep a
  dedicated migrated test using `monitor_fs_ext4` and asserting no latch file.
- **One-shot overrides accidentally become persistent.** Reusing a failure
  override across multiple requests could make a test exercise a different
  path than intended. Mitigation: preserve the current `Option<Override>` plus
  `take_*` behavior.
- **Null-underlying reconciliation gets generalized away.** The reconciliation
  test depends on present devid 1, null-underlying devid 2, btrfs `MISSING`
  devid 3, and orphan devid 99. Mitigation: keep `MonitorReconcileRunner` as a
  named topology and keep the `AckedStats` map local and explicit.

## Verification

Use filtered Rust tests during each sub-commit:

```sh
cargo test --manifest-path cli/Cargo.toml --lib monitor::tests::<test_name>
cargo test --manifest-path cli/Cargo.toml --lib monitor::tests
```

At every sub-commit boundary, run:

```sh
just test-rust
```

Use `cargo check --manifest-path cli/Cargo.toml --tests` after adding the
fixture module and after deleting local scaffolding to catch facade wiring,
unused imports, and dangling references.

No VM tests or parser fixture refresh are required. This migration moves Rust
test scaffolding only and should not alter parser fixtures, NixOS modules, or
runtime behavior.
