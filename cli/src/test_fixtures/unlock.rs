//! Unlock-scope fixtures: cross-test scaffolding for `cli/src/unlock.rs`'s
//! `mod tests`.
//!
//! Unlock stays as a flat collection of leaf helpers, not a topology
//! installer or params builder. Several tests rely on exact missing-mock
//! behavior, especially the distinction between `Mount` and
//! `MountWithOptions`, so broad handlers would hide the regressions those
//! tests exist to catch.
//!
//! Naming: every newly-exported helper carries an `unlock_` prefix. This
//! keeps the facade distinct from `mount`'s `ok_raw` / `err_raw` exports
//! and avoids duplicate names while the staged migration coexists with old
//! locals in `unlock.rs::tests`.

use super::mount::{MOUNT_TEST_PASSPHRASE_BYTES, err_raw, ok_raw};
use super::shared;
use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
use crate::types::{MapperName, MountPoint};
use std::io::Write;

/// Unlock tests mostly model a mounted `/mnt/storage` post-mount probe;
/// this wrapper keeps per-test path seeding compact while reusing the
/// shared mountinfo body that matches the old local fixture.
pub(crate) fn unlock_storage_fs(paths: &[&str]) -> shared::MockFs {
    shared::MockFs::storage(paths.iter().map(|p| (*p).to_string()).collect())
}

/// Bricked-header LUKS UUID probe used by unlock degraded-mount tests.
/// Kept scope-local because enroll uses a different command shape and
/// exit-code convention for its not-LUKS helper.
pub(crate) fn unlock_luks_uuid_not_luks(device: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupLuksUuid {
            device: device.to_owned(),
        },
        err_raw(
            "cryptsetup luksUUID",
            1,
            "Device is not a valid LUKS device.",
        ),
    )
}

/// Canonical successful full-pool scan after unlock opens mappers. This
/// helper removes repeated one-line boilerplate without hiding the request.
pub(crate) fn unlock_btrfs_device_scan_ok() -> (CmdRequest, RawCommandOutput) {
    (CmdRequest::BtrfsDeviceScanAll, ok_raw("btrfs device scan"))
}

/// Canonical idle balance-status response used by successful unlock paths
/// whose post-mount warning probe should emit nothing.
pub(crate) fn unlock_btrfs_balance_status_idle(mp: &MountPoint) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsBalanceStatus {
            mount_point: mp.clone(),
        },
        RawCommandOutput {
            cmd: "btrfs balance status".to_owned(),
            stdout: "No balance found on '/mnt/storage'\n".to_owned(),
            stderr: String::new(),
            exit_status: 0,
        },
    )
}

/// Paused balance-status body copied from the unlock success-path test.
/// A focused unlock test pipes it through the production parser and warning
/// emitter so fixture-body drift cannot silently remove the warning.
pub(crate) fn unlock_btrfs_balance_status_paused(
    mp: &MountPoint,
) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::BtrfsBalanceStatus {
            mount_point: mp.clone(),
        },
        RawCommandOutput {
            cmd: "btrfs balance status".to_owned(),
            stdout: "Balance on '/mnt/storage' is paused\n\
                     3 out of about 10 chunks balanced (7 considered), 70% left\n"
                .to_owned(),
            stderr: String::new(),
            exit_status: 0,
        },
    )
}

/// Adds a successful passphrase verification using the unlock/mount test
/// passphrase bytes. Keeping this as a single chained request preserves
/// each test's explicit per-disk runner composition.
pub(crate) fn unlock_with_test_passphrase_ok(runner: MockRunner, device: &str) -> MockRunner {
    runner.with_output_stdin(
        CmdRequest::CryptsetupTestPassphrase {
            device: device.to_owned(),
        },
        MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
        ok_raw("cryptsetup open --test-passphrase"),
    )
}

/// Adds a successful passphrase-fed mapper open for one disk. Tests chain
/// it per disk so failures still reveal the exact missing request.
pub(crate) fn unlock_with_open_mapper_ok(
    runner: MockRunner,
    device: &str,
    mapper: &str,
) -> MockRunner {
    runner.with_output_stdin(
        CmdRequest::CryptsetupLuksOpen {
            device: device.to_owned(),
            mapper: MapperName(mapper.to_owned()),
        },
        MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
        ok_raw("cryptsetup open"),
    )
}

/// Adds the plain mount variant used by healthy unlock paths. This is
/// intentionally separate from `unlock_with_mount_degraded_ok` because the
/// enum variant is the assertion in the degraded-mount test.
pub(crate) fn unlock_with_mount_ok(
    runner: MockRunner,
    device: &str,
    mp: &MountPoint,
) -> MockRunner {
    runner.with_output(
        CmdRequest::Mount {
            device: device.to_owned(),
            mount_point: mp.clone(),
        },
        ok_raw("mount -o noatime,skip_balance"),
    )
}

/// Adds the degraded mount variant used only when a missing member was
/// explicitly accepted. Seeding `MountWithOptions`, not `Mount`, is the
/// load-bearing behavior this helper preserves.
pub(crate) fn unlock_with_mount_degraded_ok(
    runner: MockRunner,
    device: &str,
    mp: &MountPoint,
) -> MockRunner {
    runner.with_output(
        CmdRequest::MountWithOptions {
            device: device.to_owned(),
            mount_point: mp.clone(),
            options: vec!["degraded".to_owned()],
        },
        ok_raw("mount -o noatime,skip_balance,degraded"),
    )
}

/// Seeds the canonical three already-open mappers used by unlock's
/// mount-only branch tests. The backing devices and UUIDs must stay aligned
/// so probe-time mapper ownership classification round-trips.
pub(crate) fn unlock_with_three_mappers_open(runner: MockRunner) -> MockRunner {
    runner
        .with_mapper_open(
            "braid-disk1",
            "/dev/vda",
            "11111111-1111-1111-1111-111111111111",
        )
        .with_mapper_open(
            "braid-disk2",
            "/dev/vdb",
            "22222222-2222-2222-2222-222222222222",
        )
        .with_mapper_open(
            "braid-disk3",
            "/dev/vdc",
            "33333333-3333-3333-3333-333333333333",
        )
}

/// Creates the real passphrase file used by unlock tests that exercise
/// credential resolution. The bogus-path test deliberately does not use
/// this helper because the missing path is the assertion.
pub(crate) fn unlock_passphrase_file() -> tempfile::NamedTempFile {
    let tmp = tempfile::NamedTempFile::new().expect("passphrase tempfile");
    tmp.as_file()
        .write_all(MOUNT_TEST_PASSPHRASE_BYTES)
        .expect("write test passphrase");
    tmp
}
