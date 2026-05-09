//! Test-only shared fixtures for `replace`, `add`, `remove`,
//! `remove_missing`, `recover`, `doctor`, `mount`, `enroll_key_file`,
//! `unlock`, and `status`.
//!
//! These fixtures consolidate the per-test scaffolding that previously
//! lived as one-off `*Runner` structs and inline `tempdir + config + pass +
//! membership` setups. The split is:
//!
//!   * `MockFs` -- generic `Filesystem` mock with the canonical
//!     `/proc/self/mountinfo` body and an optional sysfs override.
//!   * `ReplacementPool` -- canonical pool-topology mock-handler
//!     installer for `replace` (mapper -> dev, dev -> uuid, btrfs
//!     filesystem show / usage with state flipping on `replace_done`,
//!     plus the boring preflight surface).
//!   * `AddTopology` -- canonical static one-disk pool topology installer
//!     for `add` tests that exercise the live-pool returning-disk surface.
//!   * `AddStatefulPool` + `AddPoolHandle` + `AddDynFs` -- stateful
//!     bootstrap+live mutation lifecycle installer for `add` tests that
//!     observe mount/device-add commits and per-mapper opens.
//!   * `AddPlanTopology` -- `plan_add` boundary topology with
//!     parameterised keyfile-probe responses and missing-device count.
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
//!     `enroll_add_keyfile_ok`, `enroll_with_mountpoint_*`,
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
//!   * `PoolFixture` -- bundled tempdirs + `StatePaths` + config +
//!     passphrase + `RecordingInhibitor`.
//!   * `ReplaceParamsBuilder` / `AddParamsBuilder` / `RemoveParamsBuilder`
//!     / `RemoveMissingParamsBuilder` / `RecoverParamsBuilder` -- per-test
//!     builders over command defaults.
//!
//! Layout: this file is a facade. `shared` holds cross-scope items;
//! `replace`, `add`, `remove`, `remove_missing`, `recover`, `doctor`,
//! `mount`, `enroll_key_file`, `unlock`, and `status` hold their per-scope
//! topologies, builders, and helpers.

mod add;
mod doctor;
mod enroll_key_file;
mod mount;
mod recover;
mod remove;
mod remove_missing;
mod replace;
mod shared;
mod status;
mod unlock;

#[allow(unused_imports)]
pub(crate) use doctor::{
    DF_MIXED, DF_MIXED_METADATA, DF_RAID1_CLEAN, DfQueryFailureRunner, PoolMissingDevicesRunner,
    UpscSpawnFailureRunner, beep_check_options, beep_ctx, cls, config_with_ups_disabled,
    config_with_ups_enabled, config_without_ups, device_usage_healthy, device_usage_with_missing,
    df_json, df_json_fail, human_options, isolated_paths, mountpoint_fail, mountpoint_ok,
    parsed_doctor_ctx, systemctl_is_active_output, ups_ctx, valid_config_json, write_temp,
};
pub(crate) use enroll_key_file::{
    enroll_add_keyfile_ok, enroll_by_id, enroll_discovery_two_disks, enroll_fs,
    enroll_luks_dump_slot1_empty, enroll_luks_dump_slot1_occupied, enroll_luks_uuid_not_luks,
    enroll_luks_uuid_ok, enroll_make_existing_keyfile, enroll_make_membership, enroll_passphrase,
    enroll_test_keyfile_fail, enroll_test_keyfile_ok, enroll_test_passphrase_fail,
    enroll_test_passphrase_ok, enroll_with_mountpoint_fail, enroll_with_mountpoint_ok,
};
pub(crate) use mount::{
    MOUNT_TEST_PASSPHRASE_BYTES, NoopSleeper, arbitrary_fallback, base_two_disk_runner,
    direct_two_disk_fs_with_mappers, direct_two_disk_open_runner, direct_two_disk_plan, err_raw,
    is_luks_fail, is_luks_ok, luks_dump_text_fail, luks_dump_text_ok, luks_uuid_ok, mount_fs,
    ok_raw, open_and_mount_for_test, test_config, test_passphrase, test_passphrase_fail,
    three_disk_membership, two_disk_membership,
};
#[allow(unused_imports)]
pub(crate) use recover::{RecoverParamsBuilder, RemountHarness};
#[allow(unused_imports)]
pub(crate) use remove::{
    RemovalPool, RemoveParamsBuilder, target_device, valid_three_disk_df_json,
    valid_three_disk_usage_stdout, valid_two_disk_df_json, valid_two_disk_usage_stdout,
};
#[allow(unused_imports)]
pub(crate) use remove_missing::{RemoveMissingParamsBuilder, RemoveMissingPool};
pub(crate) use replace::ReplacementPool;
#[allow(unused_imports)]
pub(crate) use shared::{MockFs, PoolFixture, TEST_PASSPHRASE_BYTES, mock_ok};
#[allow(unused_imports)]
pub(crate) use status::{
    status_btrfs_device_stats_3disk, status_btrfs_device_usage_raw_1disk,
    status_btrfs_device_usage_raw_3disk, status_btrfs_df_raid1, status_btrfs_df_single,
    status_btrfs_scrub_aborted, status_btrfs_scrub_finished,
    status_btrfs_scrub_finished_with_errors, status_btrfs_scrub_interrupted,
    status_btrfs_scrub_never, status_btrfs_show_1disk, status_btrfs_show_3disk,
    status_btrfs_show_3disk_1missing, status_btrfs_show_3disk_1null_underlying_1missing,
    status_btrfs_usage_raw, status_cfg_present_not_luks, status_config,
    status_cryptsetup_status_active, status_cryptsetup_uuid_ok, status_disk_report_named,
    status_fs_ext4, status_fs_mounted, status_fs_not_mounted, status_fs_one_disk,
    status_fs_three_disk, status_is_luks_raw, status_lsblk_field_ok, status_luks_dump_text_raw,
    status_membership_1disk, status_mp, status_pool_empty, status_report_with_alerts,
    status_report_with_scrub, status_runner_healthy_3disk_base,
    status_runner_healthy_3disk_verbose,
};
#[allow(unused_imports)]
pub(crate) use unlock::{
    unlock_btrfs_balance_status_idle, unlock_btrfs_balance_status_paused,
    unlock_btrfs_device_scan_ok, unlock_luks_uuid_not_luks, unlock_passphrase_file,
    unlock_storage_fs, unlock_with_mount_degraded_ok, unlock_with_mount_ok,
    unlock_with_open_mapper_ok, unlock_with_test_passphrase_ok, unlock_with_three_mappers_open,
};
