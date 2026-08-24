//! Test-only shared fixtures for `replace`, `add`, `remove`,
//! `remove_missing`, `recover`, `doctor`, `mount`, `enroll_key_file`,
//! `unlock`, `status`, `lock`, `ack`, `monitor`, `idle`, `scrub`,
//! `discover`, `ups`, and parser modules.
//!
//! These fixtures consolidate the per-test scaffolding that previously
//! lived as one-off `*Runner` structs and inline `tempdir + config + pass +
//! membership` setups. The split is:
//!
//!   * Runtime fixture readers -- resolve required files from the fixture root
//!     or authoritative stable lane and fail closed with the resolved path.
//!   * `MockFs` -- generic `Filesystem` mock with the canonical
//!     `/proc/self/mountinfo` body and an optional sysfs override.
//!   * `ReplacementPool` -- canonical pool-topology mock-handler
//!     installer for `replace` (mapper -> dev, dev -> uuid, btrfs
//!     filesystem show / usage with state flipping on `replace_done`,
//!     plus the boring preflight surface).
//!   * `RemovalPool` -- canonical pool-topology mock-handler installer
//!     for `remove` tests that exercise 2->1 and 3->2 success paths.
//!   * `RemountHarness` -- promoted stateful-FS + mapper-closing-runner
//!     pair for `recover` tests that exercise the relock / remount cycle.
//!   * `doctor` -- flat collection of doctor-shaped helpers:
//!     `*_ctx` builders, mock-output factories, DF JSON corpora, the
//!     three named runners (`DfQueryFailureRunner`,
//!     `PoolMissingDevicesRunner`, `UpscSpawnFailureRunner`), and the
//!     `cls` summarizer-input builder.
//!   * `mount` -- flat collection of mount-shaped helpers
//!     (`base_two_disk_runner`, `direct_two_disk_*`,
//!     `open_and_mount_for_test`, `mount_fs`, `NoopSleeper`, leaf
//!     output factories). Ships flat (no topology installer, no params
//!     builder) because mount's planner/executor entry points take
//!     positional args and ProbeFailed-uncertainty tests deliberately
//!     omit specific probes -- a broad handler would silently break them.
//!   * `enroll_key_file` -- flat collection of `enroll_`-prefixed
//!     leaf helpers (`enroll_fs`, `enroll_by_id`, `enroll_passphrase`,
//!     `enroll_luks_uuid_ok`, `enroll_test_passphrase_*`,
//!     `enroll_add_keyfile_*`, `enroll_with_mountpoint_*`,
//!     `enroll_make_membership`, `enroll_make_existing_keyfile`,
//!     `enroll_discovery_two_disks`). Ships flat for the same reason
//!     mount does -- per-test request-set composition and
//!     load-bearing missing-mock contracts. The `enroll_` prefix is
//!     load-bearing: it sidesteps facade collisions with `doctor`'s
//!     `mountpoint_*` and `mount`'s `err_raw` / `luks_uuid_ok` /
//!     `test_passphrase_fail` / `ok_raw`, and it lets the staged
//!     migration import a fixture helper while the same-purpose local
//!     still exists for unmigrated tests. The `err_raw` reuse is
//!     re-imported under the alias `enroll_err_raw` for the same
//!     reason; the alias stays after the locals are deleted.
//!   * `status` -- flat collection of `status_`-prefixed leaf helpers
//!     and the two healthy three-disk runner composers for `status` tests.
//!     Ships flat because status is read-only and many tests deliberately
//!     compose exact request sets or omit probes to preserve `MissingMock`
//!     contracts. The `status_` prefix keeps the facade distinct from
//!     `mount`'s `err_raw` / `ok_raw` and `shared`'s `MockFs`; the
//!     status-specific filesystem mock stays private so it can preserve the
//!     stricter mountinfo-only read contract.
//!   * `unlock` -- flat collection of `unlock_`-prefixed leaf helpers for
//!     unlock tests. Ships flat because several tests rely on exact
//!     `MissingMock` behavior for request variants, especially
//!     `Mount` vs `MountWithOptions`; the prefix avoids facade collisions
//!     with mount's raw-output helpers during the staged migration.
//!   * `lock` -- flat collection of lock-shaped helpers that preserve
//!     missing-mock contracts, close/forget recording, and exact dry-run
//!     step assertions while moving repeated scaffolding out of
//!     `lock.rs::tests`.
//!   * `ack` -- flat collection of ack-shaped helpers that preserve exact
//!     no-run, no-device-stats, mid-probe smartd race, and state-file
//!     side-effect contracts. It intentionally avoids a broad topology
//!     installer so new probes surface as missing mocks or request-list
//!     failures.
//!   * `monitor` -- flat collection of monitor-shaped helpers with strict
//!     probe runners and mountinfo-only filesystem fixtures so fail-closed
//!     branches and state-file side effects remain visible in the tests.
//!   * `idle` -- flat idle helpers with a strict mountinfo/sysfs filesystem
//!     mock and scrub output factories. It deliberately avoids a broad runner
//!     because missing mocks and missing sysfs seeds are load-bearing.
//!   * `scrub` -- flat scrub-shaped helpers for `cmd_scrub_cancel`,
//!     `cmd_scrub_needs_resume`, and `cmd_scrub_resume_or_start`. Ships
//!     exit-code-shaped factories for cancel and resume/start, plus
//!     per-state scrub-status factories. Names document kernel state, not
//!     stderr text, so the numeric-exit-code dispatch contract for
//!     `scrub cancel` stays visible. No broad scrub runner: cross-command
//!     probes still surface as `MissingMock`.
//!   * `discover` -- flat discover-shaped helpers: `DiscoverLabelMap`
//!     preserves cryptsetup-like `Ok(exit=1)` fall-through for unknown
//!     devices and records calls for the non-LUKS gate test, while
//!     `discover_create_target` and `discover_create_by_id_symlink` keep
//!     real tempdir and Unix-symlink coverage at each call site. The prefix
//!     avoids facade collisions with other fixture families.
//!   * `ups` -- flat ups-shaped helpers for `cli/src/ups.rs::tests`:
//!     `ups_write_config` (on-disk `config.json` writer for `cmd_ups_status`
//!     tests) and three `(CmdRequest, RawCommandOutput)` pair factories
//!     (healthy OL+100%, plus two daemon-down variants -- with and without
//!     a trailing stderr newline -- that match the current local bodies
//!     byte-identically so the trim-proof and command-layer Display tests
//!     each keep the body they have today). Ships flat because the test
//!     surface is small and tightly scoped to the runner / config boundary.
//!     No broad runner helper: the missing-mock test deliberately uses
//!     `MockRunner::default()` to trigger `CmdError::MissingMock`, and a
//!     multi-test runner would mask that proof. The `ups_` prefix avoids
//!     facade collisions with the `doctor::config_with_ups_*` family.
//!   * `PoolFixture` -- bundled tempdirs + `StatePaths` + config +
//!     passphrase + `RecordingInhibitor`.
//!   * `ReplaceParamsBuilder` / `RemoveParamsBuilder`
//!     / `RemoveMissingParamsBuilder` / `RecoverParamsBuilder` -- per-test
//!     builders over command defaults.
//!
//! Layout: this file is a facade. `shared` holds cross-scope items;
//! `replace`, `remove`, `remove_missing`, `recover`, `doctor`,
//! `mount`, `enroll_key_file`, `unlock`, `status`, `lock`, `ack`, `monitor`,
//! `idle`, `scrub`, `discover`, and `ups` hold their per-scope topologies,
//! builders, and helpers.

mod ack;
mod discover;
mod doctor;
mod enroll_key_file;
mod idle;
mod lock;
mod monitor;
mod mount;
mod recover;
mod remove;
mod remove_missing;
mod replace;
mod scrub;
mod shared;
mod status;
mod unlock;
mod ups;

pub(crate) use ack::{
    ACK_DEVICE_SIZE, ACK_FSID, AckPanicFilesystem, AckPanicRunner, ack_fs_btrfs, ack_fs_ext4,
    ack_fs_not_mounted, ack_mounted_fs_that_touches_smartd, ack_mounted_probe_runner,
    ack_mounted_probe_runner_no_uuid_with_enospc_usage, ack_mounted_probe_runner_with_device_stats,
    ack_mounted_probe_runner_with_enospc_usage, ack_mounted_probe_runner_with_healthy_enospc_usage,
    ack_mounted_probe_runner_with_missing_enospc_usage,
    ack_mounted_probe_runner_with_stale_devid_stats,
    ack_mounted_probe_runner_with_zero_size_real_path_enospc_usage, ack_mp, ack_noop_beeper,
    ack_offline_fs_that_touches_scrub_failed, ack_offline_fs_that_touches_smartd, ack_write_latch,
};
pub(crate) use discover::{
    DiscoverLabelMap, discover_create_by_id_symlink, discover_create_target,
};
pub(crate) use doctor::{
    DF_METADATA_20_USED, DF_METADATA_78_USED, DF_MIXED, DF_MIXED_METADATA, DF_RAID1_CLEAN,
    DfQueryFailureRunner, DoctorMockFs, PoolMissingDevicesRunner, UpscSpawnFailureRunner, beep_ctx,
    cls, config_with_ups_enabled, config_without_ups, device_usage_raw,
    device_usage_three_one_tight, device_usage_three_two_tight, device_usage_two_healthy,
    device_usage_two_tight, df_json, df_json_fail, doctor_btrfs_show,
    doctor_cryptsetup_status_active, doctor_cryptsetup_uuid_ok, human_options, isolated_paths,
    mountpoint_fail, mountpoint_ok, parsed_doctor_ctx, pool_state_runner,
    smart_selftest_runner_for, smartctl_selftest_json, systemctl_show_active_state_output, ups_ctx,
    valid_config_json, write_temp,
};
pub(crate) use enroll_key_file::{
    enroll_add_keyfile_fail, enroll_add_keyfile_ok, enroll_by_id, enroll_discovery_two_disks,
    enroll_fs, enroll_luks_dump_slot1_empty, enroll_luks_dump_slot1_occupied,
    enroll_luks_uuid_not_luks, enroll_luks_uuid_ok, enroll_make_existing_keyfile,
    enroll_make_membership, enroll_mountpoint_fail, enroll_mountpoint_ok, enroll_passphrase,
    enroll_test_keyfile_fail, enroll_test_keyfile_ok, enroll_test_passphrase_fail,
    enroll_test_passphrase_ok, enroll_with_mountpoint_fail, enroll_with_mountpoint_ok,
};
pub(crate) use idle::{
    IDLE_FSID, IDLE_FSID_OTHER, IdleMockFs, assert_idle_busy_unknown_prefix, idle_mp,
    idle_ready_for_sysfs_check, idle_runner_with_scrub_finished, idle_scrub_running,
    idle_scrub_running_no_bytes,
};
pub(crate) use lock::{
    LockNoopSleeper, RecordingRunner as LockRecordingRunner, lock_count_forget_steps, lock_err_raw,
    lock_forget_step_devices, lock_fs, lock_mounted_runner, lock_ok_raw, lock_test_config,
    lock_test_membership, lock_umount_failed_runner, lock_with_fsid_probe_mocks,
};
pub(crate) use monitor::{
    BTRFS_SHOW_2DISK_1MISSING, BTRFS_SHOW_2DISK_NO_UUID, MONITOR_FSID, MonitorOverride,
    MonitorReconcileRunner, MonitorTestRunner, USAGE_DEVICE_SIZE,
    assert_monitor_single_computation_error, missing_pool_key, monitor_fs_btrfs, monitor_fs_ext4,
    monitor_fs_mountinfo_error, monitor_fs_not_mounted, monitor_mp, usage_2disk,
    usage_2disk_one_missing, usage_4disk_one_low,
};
pub(crate) use mount::{
    MOUNT_TEST_PASSPHRASE_BYTES, NoopSleeper, arbitrary_fallback, base_two_disk_runner,
    direct_two_disk_fs_with_mappers, direct_two_disk_open_runner, direct_two_disk_plan, err_raw,
    is_luks_fail, is_luks_ok, luks_dump_text_fail, luks_dump_text_ok, luks_uuid_ok, mount_fs,
    ok_raw, open_and_mount_for_test, test_config, test_passphrase, test_passphrase_fail,
    three_disk_membership, two_disk_membership,
};
pub(crate) use recover::RemountHarness;
pub(crate) use remove::{
    RemovalPool, overcommitted_survivor_df_json, overcommitted_survivor_usage_stdout,
    target_device, valid_three_disk_df_json, valid_three_disk_usage_stdout, valid_two_disk_df_json,
    valid_two_disk_usage_stdout,
};
pub(crate) use remove_missing::RemoveMissingPool;
pub(crate) use replace::{ReplacementPool, replace_dev_info_sufficient};
pub(crate) use scrub::{
    scrub_cancel_not_running, scrub_cancel_ok, scrub_cancel_real_failure, scrub_mp,
    scrub_resume_output, scrub_start_output, scrub_status_aborted, scrub_status_finished,
    scrub_status_interrupted, scrub_status_never, scrub_status_running, scrub_status_unknown,
};
pub(crate) use shared::{
    DeviceUsageSpec, MockBackingPathResolver, MockFs, PoolFixture, RecordingSleeper,
    TEST_PASSPHRASE_BYTES, assert_exact_lines_in_order, assert_lines_in_order,
    btrfs_remove_devid_error, btrfs_remove_path_error, canonical_luks_uuid, device_usage_raw_body,
    disk_member, disk_member_with, line_index, mock_ok, mock_virtio_backing_path_resolver,
    mock_virtio_offset_backing_path_resolver, read_fixture, read_stable_fixture, test_uuid,
    with_lsblk_hw_info,
};
pub(crate) use status::{
    status_btrfs_device_usage_raw_1disk, status_btrfs_df_single, status_btrfs_scrub_aborted,
    status_btrfs_scrub_aborted_no_start, status_btrfs_scrub_finished,
    status_btrfs_scrub_finished_with_errors, status_btrfs_scrub_interrupted,
    status_btrfs_scrub_never, status_btrfs_scrub_running, status_btrfs_show_1disk,
    status_btrfs_show_3disk_1missing, status_btrfs_show_3disk_1null_underlying_1missing,
    status_btrfs_show_3disk_missing_devid3, status_btrfs_usage_raw, status_cfg_absent,
    status_cfg_present_not_luks, status_config, status_cryptsetup_status_active,
    status_cryptsetup_uuid_ok, status_disk_report_missing, status_disk_report_named,
    status_fs_ext4, status_fs_mounted, status_fs_not_mounted, status_fs_one_disk,
    status_fs_three_disk, status_is_luks_raw, status_lsblk_field_ok, status_membership_1disk,
    status_membership_3disk, status_mp, status_pool_empty, status_report_with_alerts,
    status_report_with_scrub, status_runner_healthy_3disk_base,
    status_runner_healthy_3disk_verbose,
};
pub(crate) use unlock::{
    unlock_btrfs_balance_status_idle, unlock_btrfs_balance_status_paused,
    unlock_btrfs_balance_status_paused_skip_balance, unlock_btrfs_device_scan_ok,
    unlock_luks_uuid_not_luks, unlock_passphrase_file, unlock_storage_fs,
    unlock_with_mount_degraded_ok, unlock_with_mount_ok, unlock_with_open_mapper_ok,
    unlock_with_test_passphrase_ok, unlock_with_three_mappers_open,
};
pub(crate) use ups::{
    ups_query_connection_refused_no_newline, ups_query_connection_refused_with_newline,
    ups_query_empty_stderr_exit_1, ups_query_healthy_minimal, ups_write_config,
    ups_write_config_without_ups,
};
