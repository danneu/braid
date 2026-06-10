//! Mount-scope fixtures: cross-test scaffolding for `cli/src/mount.rs`'s
//! `mod tests`.
//!
//! Mount is mutating-command oriented but its planner/executor entry points
//! (`plan_open_pool`, `execute_mount_only`, `execute_unlock_and_mount`,
//! `close_opened_mappers`) take positional args -- there is no `MountParams`
//! struct -- so this module is a flat collection of helpers like
//! `test_fixtures::doctor`, not the `*Pool` + `*ParamsBuilder` triad that
//! `add` / `remove` / `replace` ship.
//!
//! Two intentional omissions, both load-bearing:
//!
//!   * No `MountTopology` / `MountPool` handler installer. ProbeFailed-
//!     uncertainty tests in `mount.rs` deliberately omit
//!     `CryptsetupIsLuks` / `CryptsetupLuksDumpText` for a specific disk to
//!     assert `LuksHeaderState::ProbeFailed` wording. A broad `with_handler`
//!     would resolve those probes and silently break those tests.
//!   * `base_two_disk_runner` does NOT seed `CryptsetupIsLuks`. It seeds
//!     UUID, luksDumpText, mappers-closed, and verify-passphrase only --
//!     verify-callsite tests layer their own `is_luks_*` outputs on top.

use super::shared;
use crate::cmd::{CmdRequest, CommandRunner, MockRunner, RawCommandOutput};
use crate::config::Config;
use crate::credential::OpenCredential;
use crate::membership::PoolMembership;
use crate::mount::{
    MountError, OpenPlan, UnlockAndMountFailure, execute_mount_only, execute_unlock_and_mount,
    plan_open_pool,
};
use crate::probe::Filesystem;
use crate::progress::Sleeper;
use crate::secret::Passphrase;
use crate::types::{ByIdPath, DiskName, LuksUuid, MapperName, MountPoint};
use zeroize::Zeroizing;

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

/// Thin wrapper around `shared::MockFs::unmounted` that takes a `&[&str]`
/// for ergonomic per-test seeding. Centralising the `&str -> String`
/// conversion here keeps every test-mod call site to one line.
///
/// Safe to use the shared "unmounted" mountinfo body because
/// `plan_open_pool_inner` checks pool mountedness via
/// `runner.run(MountpointCheck)`, never via `fs.read_to_string`. The only
/// `Filesystem` method called on the mount-test call graph is
/// `fs.exists` through `probe_config_disk` and `close_opened_mappers`.
pub(crate) fn mount_fs(paths: &[&str]) -> shared::MockFs {
    shared::MockFs::unmounted(paths.iter().map(|p| (*p).to_string()).collect())
}

// ---------------------------------------------------------------------------
// Sleeper
// ---------------------------------------------------------------------------

/// No-op sleeper for `close_opened_mappers` cleanup tests so retry loops
/// don't burn wall time. Local to the mount fixture because only mount
/// tests consume it; promoted to `shared` if a future scope needs it.
pub(crate) struct NoopSleeper;

impl Sleeper for NoopSleeper {
    fn sleep(&self, _duration: std::time::Duration) {}
}

// ---------------------------------------------------------------------------
// RawCommandOutput primitives
// ---------------------------------------------------------------------------

pub(crate) fn ok_raw(cmd: &str) -> RawCommandOutput {
    RawCommandOutput {
        cmd: cmd.to_owned(),
        stdout: String::new(),
        stderr: String::new(),
        exit_status: 0,
    }
}

pub(crate) fn err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput {
    RawCommandOutput {
        cmd: cmd.to_owned(),
        stdout: String::new(),
        stderr: stderr.to_owned(),
        exit_status: exit_code,
    }
}

// ---------------------------------------------------------------------------
// (CmdRequest, RawCommandOutput) factories for chaining onto MockRunner
// ---------------------------------------------------------------------------

pub(crate) fn luks_uuid_ok(device: &str, uuid: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupLuksUuid {
            device: device.into(),
        },
        RawCommandOutput {
            cmd: "cryptsetup luksUUID".into(),
            stdout: format!("{uuid}\n"),
            stderr: String::new(),
            exit_status: 0,
        },
    )
}

pub(crate) fn test_passphrase_fail(device: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupTestPassphrase {
            device: device.into(),
        },
        err_raw(
            "cryptsetup open --test-passphrase",
            2,
            "No key available with this passphrase.",
        ),
    )
}

pub(crate) fn is_luks_ok(device: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupIsLuks {
            device: device.into(),
        },
        ok_raw("cryptsetup isLuks"),
    )
}

pub(crate) fn is_luks_fail(device: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupIsLuks {
            device: device.into(),
        },
        err_raw(
            "cryptsetup isLuks",
            1,
            &format!("Device {device} is not a valid LUKS device.\n"),
        ),
    )
}

pub(crate) fn luks_dump_text_ok(device: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupLuksDumpText {
            device: device.into(),
        },
        RawCommandOutput {
            cmd: "cryptsetup luksDump".into(),
            stdout: "LUKS header information\nVersion: 2\n".into(),
            stderr: String::new(),
            exit_status: 0,
        },
    )
}

pub(crate) fn luks_dump_text_fail(device: &str) -> (CmdRequest, RawCommandOutput) {
    (
        CmdRequest::CryptsetupLuksDumpText {
            device: device.into(),
        },
        err_raw(
            "cryptsetup luksDump",
            1,
            "Cannot read LUKS header metadata.",
        ),
    )
}

// ---------------------------------------------------------------------------
// Membership / config / credential constructors
// ---------------------------------------------------------------------------

/// Single source of truth for the passphrase bytes mount-test fixtures
/// expect on `with_output_stdin`. Intentionally `b"testpass"` (not
/// `shared::TEST_PASSPHRASE_BYTES = b"test-passphrase"`) because the 27
/// existing `with_output_stdin` calls in `mount.rs::tests` all encode
/// `b"testpass"` -- unifying would touch every chained override.
pub(crate) const MOUNT_TEST_PASSPHRASE_BYTES: &[u8] = b"testpass";

pub(crate) fn test_config() -> Config {
    Config::new(MountPoint::new("/mnt/storage".to_owned())).unwrap()
}

pub(crate) fn test_passphrase() -> OpenCredential {
    OpenCredential::Passphrase(Passphrase::from_zeroizing(Zeroizing::new(
        "testpass".to_owned(),
    )))
}

pub(crate) fn two_disk_membership() -> PoolMembership {
    let mut membership = PoolMembership::empty();
    for (uuid, name, path) in [
        (
            "11111111-1111-1111-1111-111111111111",
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
        ),
        (
            "22222222-2222-2222-2222-222222222222",
            "disk2",
            "/dev/disk/by-id/virtio-disk2",
        ),
    ] {
        let (_, member) = shared::disk_member(1, name, path);
        membership
            .insert(LuksUuid::parse(uuid).unwrap(), member)
            .expect("insert mount member");
    }
    membership
}

pub(crate) fn three_disk_membership() -> PoolMembership {
    let mut membership = PoolMembership::empty();
    for (uuid, name, path) in [
        (
            "11111111-1111-1111-1111-111111111111",
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
        ),
        (
            "22222222-2222-2222-2222-222222222222",
            "disk2",
            "/dev/disk/by-id/virtio-disk2",
        ),
        (
            "33333333-3333-3333-3333-333333333333",
            "disk3",
            "/dev/disk/by-id/virtio-disk3",
        ),
    ] {
        let (_, member) = shared::disk_member(1, name, path);
        membership
            .insert(LuksUuid::parse(uuid).unwrap(), member)
            .expect("insert mount member");
    }
    membership
}

/// Distinct sentinel `MountError` used by `explain_open_failure` tests to
/// prove which branches override the caller's fallback and which preserve
/// it verbatim. The literal text is asserted (positively or negatively)
/// in those tests so don't change it without updating the assertions.
pub(crate) fn arbitrary_fallback() -> MountError {
    MountError::Failed("ARBITRARY FALLBACK TEXT".into())
}

// ---------------------------------------------------------------------------
// Composite preflight runners
// ---------------------------------------------------------------------------

/// Canonical 2-disk-closed preflight `MockRunner`. Seeds the smallest set
/// of probes that resolve `plan_open_pool` to "two unlockable disks":
///
///   * `MountpointCheck("/mnt/storage")` -> not mounted
///   * `CryptsetupLuksUuid` for both `virtio-diskN` -> stable test UUIDs
///   * `CryptsetupLuksDumpText` (LUKS2) for both disks
///   * `CryptsetupStatus` for both `braid-diskN` mappers -> closed
///   * `CryptsetupTestPassphrase` for both disks with stdin
///     `MOUNT_TEST_PASSPHRASE_BYTES` -> ok
///
/// What this runner does NOT seed -- by design:
///
///   * `CryptsetupIsLuks`. ProbeFailed-uncertainty tests
///     (mount.rs `unlock_passphrase_open_exit2_probe_failed_does_not_blame_invariant`)
///     deliberately omit this for a specific disk so `MockRunner` returns
///     `MissingMock`, classifying as `LuksHeaderState::ProbeFailed`. If
///     this seed were added, those tests would silently invert.
///   * Open / scan / mount commands. Each test layers them per-scenario.
///
/// Per-test verify-outcome overrides chain on top via
/// `.with_output_stdin(req, bytes, output)` for the same `CmdRequest` --
/// `MockRunner::with_output_stdin` overwrites both `outputs` and
/// `stdin_expectations`, pinned by the regression test
/// `mock_runner_with_output_stdin_override_after_base_wins` in cmd.rs.
pub(crate) fn base_two_disk_runner() -> MockRunner {
    let (uuid1_req, uuid1_out) = luks_uuid_ok(
        "/dev/disk/by-id/virtio-disk1",
        "11111111-1111-1111-1111-111111111111",
    );
    let (uuid2_req, uuid2_out) = luks_uuid_ok(
        "/dev/disk/by-id/virtio-disk2",
        "22222222-2222-2222-2222-222222222222",
    );
    MockRunner::default()
        .with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()),
            },
            err_raw("mountpoint", 1, ""),
        )
        .with_output(uuid1_req, uuid1_out)
        .with_output(uuid2_req, uuid2_out)
        .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
        .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
        .with_mappers_closed(&["braid-disk1", "braid-disk2"])
        .with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/disk/by-id/virtio-disk1".into(),
            },
            MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
            ok_raw("cryptsetup open --test-passphrase"),
        )
        .with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/disk/by-id/virtio-disk2".into(),
            },
            MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
            ok_raw("cryptsetup open --test-passphrase"),
        )
}

// ---------------------------------------------------------------------------
// Direct execute_*-style fixtures (cleanup-ordering family)
// ---------------------------------------------------------------------------

/// `OpenPlan` for the canonical 2-disk closed-mappers scenario used by
/// the cleanup-ordering tests that call `execute_unlock_and_mount`
/// directly (bypassing `plan_open_pool`). `to_unlock` lists both members
/// in stable order; `mount_device` points at disk1's mapper.
pub(crate) fn direct_two_disk_plan() -> OpenPlan {
    OpenPlan {
        to_unlock: vec![
            (
                DiskName::parse("disk1").expect("test disk name"),
                ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
            ),
            (
                DiskName::parse("disk2").expect("test disk name"),
                ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
            ),
        ],
        any_open: false,
        any_missing_member: false,
        mount_device: "/dev/mapper/braid-disk1".to_owned(),
    }
}

/// `MockFs` seeded with both `virtio-diskN` device paths plus both
/// `braid-diskN` mapper paths. Cleanup-ordering tests need the mapper
/// paths visible so post-open `fs.exists` checks match.
pub(crate) fn direct_two_disk_fs_with_mappers() -> shared::MockFs {
    mount_fs(&[
        "/dev/disk/by-id/virtio-disk1",
        "/dev/disk/by-id/virtio-disk2",
        "/dev/mapper/braid-disk1",
        "/dev/mapper/braid-disk2",
    ])
}

/// `MockRunner` pre-seeded for direct `execute_unlock_and_mount` calls
/// against `direct_two_disk_plan`: verify-passphrase ok for both disks,
/// mappers closed, LUKS open ok for both. Tests layer their post-open
/// failure (mount, scan, etc.) on top.
pub(crate) fn direct_two_disk_open_runner() -> MockRunner {
    MockRunner::default()
        .with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/disk/by-id/virtio-disk1".into(),
            },
            MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
            ok_raw("cryptsetup open --test-passphrase"),
        )
        .with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/disk/by-id/virtio-disk2".into(),
            },
            MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
            ok_raw("cryptsetup open --test-passphrase"),
        )
        .with_mappers_closed(&["braid-disk1", "braid-disk2"])
        .with_output_stdin(
            CmdRequest::CryptsetupLuksOpen {
                device: "/dev/disk/by-id/virtio-disk1".into(),
                mapper: MapperName::from_basename("braid-disk1".into()),
            },
            MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
            ok_raw("cryptsetup open"),
        )
        .with_output_stdin(
            CmdRequest::CryptsetupLuksOpen {
                device: "/dev/disk/by-id/virtio-disk2".into(),
                mapper: MapperName::from_basename("braid-disk2".into()),
            },
            MOUNT_TEST_PASSPHRASE_BYTES.to_vec(),
            ok_raw("cryptsetup open"),
        )
}

// ---------------------------------------------------------------------------
// Test harness around plan + execute_*
// ---------------------------------------------------------------------------

/// Test-only helper that mirrors the legacy `open_and_mount_pool` flow
/// (plan + optional resolve + execute) so existing test bodies don't need
/// to spell out both phases. Production callers (`cmd_unlock`,
/// `cmd_recover`) compose the phases explicitly per the refactor's
/// design -- this helper exists ONLY for the test module.
///
/// Dispatches on `plan.to_unlock.is_empty()`: mount-only when empty,
/// unlock-and-mount otherwise. Tests that need to pin the per-entry-point
/// boundary checks must call the production functions directly (not
/// through this helper).
pub(crate) fn open_and_mount_for_test<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    credential: Option<OpenCredential>,
    allow_degraded: bool,
    command_hint: &str,
) -> Result<bool, MountError> {
    let backing_path_resolver = shared::mock_virtio_backing_path_resolver();
    let report = plan_open_pool(
        runner,
        fs,
        config,
        membership,
        backing_path_resolver,
        allow_degraded,
        command_hint,
    );
    let plan = match report.result? {
        Some(p) => p,
        None => return Ok(false),
    };
    if plan.to_unlock.is_empty() {
        execute_mount_only(runner, config, &plan)
    } else {
        let credential = credential
            .as_ref()
            .expect("test passed empty credential with non-empty plan");
        execute_unlock_and_mount(runner, config, &plan, backing_path_resolver, credential)
            .map_err(|failure: UnlockAndMountFailure| failure.error)
    }
}
