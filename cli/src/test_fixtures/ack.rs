//! Ack-scope fixtures for `cli/src/ack.rs`'s `mod tests`.
//!
//! Ack tests pin exact no-run, no-device-stats, race, and state-file
//! contracts. This module stays flat and uses explicit mock outputs so a new
//! production probe or side effect still surfaces through a missing mock or a
//! request-list assertion.

use super::shared::{DeviceUsageSpec, device_usage_raw_body, mock_ok};
use crate::alert::{AlertCause, AlertState, LatchedCause, save_alert_latch};
use crate::cmd::{CmdError, CmdRequest, CommandRunner, MockRunner, RawCommandOutput};
use crate::probe::Filesystem;
use crate::state_paths::StatePaths;
use crate::types::{MapperName, MountPoint};

/// FSID the ack 2-disk show reports; the `pool_key.fsid` that a mounted ack
/// of an EnospcRisk latch writes into `enospc-ack.json`.
pub(crate) const ACK_FSID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

/// Device size (100 GiB) for the ack ENOSPC usage fixtures, so the threshold caps
/// at 1 GiB. Also the `device_size` in the baseline `PoolKey`.
pub(crate) const ACK_DEVICE_SIZE: u64 = 100 * (1 << 30);

const MOUNTINFO_EXT4: &str = "36 35 0:32 / /mnt/storage rw,noatime shared:1 - ext4 /dev/sda1 rw\n";
const MOUNTINFO_BTRFS: &str =
    "36 35 0:32 / /mnt/storage rw,noatime shared:1 - btrfs /dev/mapper/braid-disk1 rw\n";

/// No-run sentinel for ack branches whose contract forbids command execution.
pub(crate) struct AckPanicRunner;

impl CommandRunner for AckPanicRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        panic!("offline ack must not invoke the runner; got: {request:?}");
    }

    fn run_with_stdin(
        &self,
        request: &CmdRequest,
        _stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        panic!("offline ack must not invoke run_with_stdin; got: {request:?}");
    }
}

/// No-filesystem-access sentinel for ack branches that must run before probe.
pub(crate) struct AckPanicFilesystem;

impl Filesystem for AckPanicFilesystem {
    fn exists(&self, path: &str) -> bool {
        panic!("sentinel-only retry must not touch the filesystem; got exists({path})");
    }

    fn is_block_device(&self, path: &str) -> bool {
        panic!("sentinel-only retry must not touch the filesystem; got is_block_device({path})");
    }

    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        panic!("sentinel-only retry must not touch the filesystem; got read_to_string({path})");
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
        panic!("sentinel-only retry must not touch the filesystem; got list_dir({path})");
    }

    fn create_dir_all(&self, path: &str) -> Result<(), std::io::Error> {
        panic!("sentinel-only retry must not touch the filesystem; got create_dir_all({path})");
    }
}

struct AckMountinfoFs {
    mountinfo: &'static str,
}

impl Filesystem for AckMountinfoFs {
    fn exists(&self, _path: &str) -> bool {
        false
    }

    fn is_block_device(&self, _path: &str) -> bool {
        false
    }

    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        assert_eq!(path, "/proc/self/mountinfo");
        Ok(self.mountinfo.to_owned())
    }

    fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
        Ok(vec![])
    }

    fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
        unreachable!("AckMountinfoFs: read-only fixture; create_dir_all must never be called")
    }
}

struct OfflineFsThatTouchesSmartd<'a> {
    paths: &'a StatePaths,
}

impl Filesystem for OfflineFsThatTouchesSmartd<'_> {
    fn exists(&self, _path: &str) -> bool {
        false
    }

    fn is_block_device(&self, _path: &str) -> bool {
        false
    }

    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        assert_eq!(path, "/proc/self/mountinfo");
        std::fs::write(self.paths.smartd_alert(), b"").unwrap();
        Ok(String::new())
    }

    fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
        Ok(vec![])
    }

    fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
        unreachable!(
            "OfflineFsThatTouchesSmartd: read-only fixture; create_dir_all must never be called"
        )
    }
}

struct MountedFsThatTouchesSmartd<'a> {
    paths: &'a StatePaths,
}

impl Filesystem for MountedFsThatTouchesSmartd<'_> {
    fn exists(&self, _path: &str) -> bool {
        false
    }

    fn is_block_device(&self, _path: &str) -> bool {
        false
    }

    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        assert_eq!(path, "/proc/self/mountinfo");
        std::fs::write(self.paths.smartd_alert(), b"").unwrap();
        Ok(MOUNTINFO_BTRFS.to_owned())
    }

    fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
        Ok(vec![])
    }

    fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
        unreachable!(
            "MountedFsThatTouchesSmartd: read-only fixture; create_dir_all must never be called"
        )
    }
}

/// Offline mountinfo probe that writes `scrub-failed` mid-probe -- the
/// scrub-failed analog of `OfflineFsThatTouchesSmartd`, since
/// `braid-scrub-failed.service` (onFailure) is not under the pool lock and can
/// fire while ack is probing the mount point.
struct OfflineFsThatTouchesScrubFailed<'a> {
    paths: &'a StatePaths,
}

impl Filesystem for OfflineFsThatTouchesScrubFailed<'_> {
    fn exists(&self, _path: &str) -> bool {
        false
    }

    fn is_block_device(&self, _path: &str) -> bool {
        false
    }

    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        assert_eq!(path, "/proc/self/mountinfo");
        std::fs::write(self.paths.scrub_failed(), b"").unwrap();
        Ok(String::new())
    }

    fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
        Ok(vec![])
    }

    fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
        unreachable!(
            "OfflineFsThatTouchesScrubFailed: read-only fixture; create_dir_all must never be called"
        )
    }
}

/// Offline mountinfo probe that writes `scrub-failed` mid-probe.
pub(crate) fn ack_offline_fs_that_touches_scrub_failed<'a>(
    paths: &'a StatePaths,
) -> impl Filesystem + 'a {
    OfflineFsThatTouchesScrubFailed { paths }
}

/// Canonical ack-test mount point shared by the ack runner outputs.
pub(crate) fn ack_mp() -> MountPoint {
    MountPoint::new("/mnt/storage".to_owned())
}

/// Explicit no-op beeper hook for ack tests that only care about ack logic.
pub(crate) fn ack_noop_beeper() {}

/// Ack-specific alert latch writer for tests that compose alert causes. Each
/// cause is stamped with a fixed first-detection time; ack tests assert on cause
/// kinds and lifecycle, not on the timestamp.
pub(crate) fn ack_write_latch(paths: &StatePaths, causes: Vec<AlertCause>) {
    let state = AlertState {
        causes: causes
            .into_iter()
            .map(|cause| LatchedCause::new(cause, "2023-11-14T22:13:20Z".to_owned()))
            .collect(),
    };
    save_alert_latch(&state, paths).unwrap();
}

/// Mounted btrfs filesystem surface that only allows the mountinfo read.
pub(crate) fn ack_fs_btrfs() -> impl Filesystem {
    AckMountinfoFs {
        mountinfo: MOUNTINFO_BTRFS,
    }
}

/// Offline filesystem surface that only allows the mountinfo read.
pub(crate) fn ack_fs_not_mounted() -> impl Filesystem {
    AckMountinfoFs { mountinfo: "" }
}

/// Mounted non-btrfs filesystem surface for NotBtrfs ack boundaries.
pub(crate) fn ack_fs_ext4() -> impl Filesystem {
    AckMountinfoFs {
        mountinfo: MOUNTINFO_EXT4,
    }
}

/// Offline mountinfo probe that writes `smartd-alert` mid-probe.
pub(crate) fn ack_offline_fs_that_touches_smartd<'a>(
    paths: &'a StatePaths,
) -> impl Filesystem + 'a {
    OfflineFsThatTouchesSmartd { paths }
}

/// Mounted btrfs mountinfo probe that writes `smartd-alert` mid-probe.
pub(crate) fn ack_mounted_fs_that_touches_smartd<'a>(
    paths: &'a StatePaths,
) -> impl Filesystem + 'a {
    MountedFsThatTouchesSmartd { paths }
}

fn btrfs_show_2disk() -> RawCommandOutput {
    mock_ok(
        "btrfs filesystem show /mnt/storage",
        "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
         \tTotal devices 2 FS bytes used 1.00GiB\n\
         \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
         \tdevid    3 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk3\n",
    )
}

fn cryptsetup_status_active(mapper: &str, device: &str) -> RawCommandOutput {
    mock_ok(
        &format!("cryptsetup status {mapper}"),
        &format!(
            "/dev/mapper/{mapper} is active and is in use.\n\
             \ttype:    LUKS2\n\
             \tcipher:  aes-xts-plain64\n\
             \tdevice:  {device}\n\
             \tsector size:  512\n"
        ),
    )
}

fn btrfs_device_stats_healthy() -> RawCommandOutput {
    mock_ok(
        "btrfs --format json device stats /mnt/storage",
        r#"{
            "device-stats": [
                {
                    "device": "/dev/mapper/braid-disk1",
                    "devid": 1,
                    "write_io_errs": 0,
                    "read_io_errs": 0,
                    "flush_io_errs": 0,
                    "corruption_errs": 0,
                    "generation_errs": 0
                },
                {
                    "device": "/dev/mapper/braid-disk3",
                    "devid": 3,
                    "write_io_errs": 0,
                    "read_io_errs": 0,
                    "flush_io_errs": 0,
                    "corruption_errs": 0,
                    "generation_errs": 0
                }
            ]
        }"#,
    )
}

fn btrfs_device_stats_with_stale_devid() -> RawCommandOutput {
    mock_ok(
        "btrfs --format json device stats /mnt/storage",
        r#"{
            "device-stats": [
                {
                    "device": "/dev/mapper/braid-disk1",
                    "devid": 1,
                    "write_io_errs": 0,
                    "read_io_errs": 0,
                    "flush_io_errs": 0,
                    "corruption_errs": 0,
                    "generation_errs": 0
                },
                {
                    "device": "/dev/mapper/braid-disk3",
                    "devid": 3,
                    "write_io_errs": 0,
                    "read_io_errs": 0,
                    "flush_io_errs": 0,
                    "corruption_errs": 0,
                    "generation_errs": 0
                },
                {
                    "device": "/dev/mapper/braid-stale",
                    "devid": 99,
                    "write_io_errs": 0,
                    "read_io_errs": 3,
                    "flush_io_errs": 0,
                    "corruption_errs": 1,
                    "generation_errs": 0
                }
            ]
        }"#,
    )
}

/// Mounted probe runner that intentionally omits `BtrfsDeviceStatsJson`.
pub(crate) fn ack_mounted_probe_runner() -> MockRunner {
    MockRunner::default()
        .with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: ack_mp(),
            },
            btrfs_show_2disk(),
        )
        .with_output(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName::from_basename("braid-disk1".into()),
            },
            cryptsetup_status_active("braid-disk1", "/dev/vda"),
        )
        .with_output(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName::from_basename("braid-disk3".into()),
            },
            cryptsetup_status_active("braid-disk3", "/dev/vdc"),
        )
}

/// Mounted probe runner plus the healthy stats response for full ack paths.
pub(crate) fn ack_mounted_probe_runner_with_device_stats() -> MockRunner {
    ack_mounted_probe_runner().with_output(
        CmdRequest::BtrfsDeviceStatsJson {
            mount_point: ack_mp(),
        },
        btrfs_device_stats_healthy(),
    )
}

/// Mounted probe runner plus stats containing an unrecognized stale devid.
pub(crate) fn ack_mounted_probe_runner_with_stale_devid_stats() -> MockRunner {
    ack_mounted_probe_runner().with_output(
        CmdRequest::BtrfsDeviceStatsJson {
            mount_point: ack_mp(),
        },
        btrfs_device_stats_with_stale_devid(),
    )
}

/// At-risk `btrfs device usage --raw` for the ack pool (devids 1 and 3, matching
/// the show): device 1 down to 100 MiB unallocated (below the 1 GiB threshold),
/// device 3 roomy -- a clearly negative predicate margin.
fn ack_btrfs_device_usage_atrisk() -> RawCommandOutput {
    mock_ok(
        "btrfs device usage",
        &device_usage_raw_body(&[
            DeviceUsageSpec::live(
                "/dev/mapper/braid-disk1",
                1,
                ACK_DEVICE_SIZE,
                &[("Data", "RAID1", ACK_DEVICE_SIZE - 100 * (1 << 20))],
                100 * (1 << 20),
            ),
            DeviceUsageSpec::live(
                "/dev/mapper/braid-disk3",
                3,
                ACK_DEVICE_SIZE,
                &[("Data", "RAID1", ACK_DEVICE_SIZE - 50 * (1 << 30))],
                50 * (1 << 30),
            ),
        ]),
    )
}

/// At-risk usage whose second entry is btrfs's missing-device marker, while the
/// paired show fixture still reports both devices present.
fn ack_btrfs_device_usage_atrisk_one_missing() -> RawCommandOutput {
    mock_ok(
        "btrfs device usage",
        &device_usage_raw_body(&[
            DeviceUsageSpec::live(
                "/dev/mapper/braid-disk1",
                1,
                ACK_DEVICE_SIZE,
                &[("Data", "RAID1", ACK_DEVICE_SIZE - 100 * (1 << 20))],
                100 * (1 << 20),
            ),
            DeviceUsageSpec::missing(3, &[("Data", "RAID1", ACK_DEVICE_SIZE / 2)], 0),
        ]),
    )
}

/// At-risk usage whose second entry carries a **real** device path but `device_size == 0` --
/// the btrfs-progs size-probe failure on a present device, not the `<missing disk>` marker.
/// ADR 014's ENOSPC baseline guard keys on size, not on the marker, so ack must still write no
/// baseline here.
fn ack_btrfs_device_usage_atrisk_one_zero_size_real_path() -> RawCommandOutput {
    mock_ok(
        "btrfs device usage",
        &device_usage_raw_body(&[
            DeviceUsageSpec::live(
                "/dev/mapper/braid-disk1",
                1,
                ACK_DEVICE_SIZE,
                &[("Data", "RAID1", ACK_DEVICE_SIZE - 100 * (1 << 20))],
                100 * (1 << 20),
            ),
            DeviceUsageSpec::live(
                "/dev/mapper/braid-disk3",
                3,
                0,
                &[("Data", "RAID1", ACK_DEVICE_SIZE / 2)],
                0,
            ),
        ]),
    )
}

/// Healthy (dead-band) `btrfs device usage --raw` for the ack pool: device 1 has
/// 1.5 GiB unallocated against the 1 GiB threshold (predicate margin 0.5 GiB, in
/// `[0, REARM)`), so the fresh ack-time probe is not at risk and a mounted ack
/// writes no snooze marker. Devids 1 and 3 match the show.
fn ack_btrfs_device_usage_healthy() -> RawCommandOutput {
    mock_ok(
        "btrfs device usage",
        &device_usage_raw_body(&[
            DeviceUsageSpec::live(
                "/dev/mapper/braid-disk1",
                1,
                ACK_DEVICE_SIZE,
                &[("Data", "RAID1", ACK_DEVICE_SIZE - 1536 * (1 << 20))],
                1536 * (1 << 20),
            ),
            DeviceUsageSpec::live(
                "/dev/mapper/braid-disk3",
                3,
                ACK_DEVICE_SIZE,
                &[("Data", "RAID1", ACK_DEVICE_SIZE - 50 * (1 << 30))],
                50 * (1 << 30),
            ),
        ]),
    )
}

/// 2-disk btrfs show with no `uuid:` line, so `probe_pool_alerts` yields
/// `fsid: None` -- the "ack cannot key a baseline" path.
fn btrfs_show_2disk_no_uuid() -> RawCommandOutput {
    mock_ok(
        "btrfs filesystem show /mnt/storage",
        "Label: none\n\
         \tTotal devices 2 FS bytes used 1.00GiB\n\
         \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
         \tdevid    3 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk3\n",
    )
}

/// Mounted probe + device-stats runner plus an at-risk usage stub, so a mounted
/// ack of an EnospcRisk latch can re-probe and write a keyed baseline.
pub(crate) fn ack_mounted_probe_runner_with_enospc_usage() -> MockRunner {
    ack_mounted_probe_runner_with_device_stats().with_output(
        CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: ack_mp(),
        },
        ack_btrfs_device_usage_atrisk(),
    )
}

/// Mounted probe runner plus an at-risk usage snapshot that already carries a
/// missing-device marker, so ack must clear the latch without writing a baseline.
pub(crate) fn ack_mounted_probe_runner_with_missing_enospc_usage() -> MockRunner {
    ack_mounted_probe_runner_with_device_stats().with_output(
        CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: ack_mp(),
        },
        ack_btrfs_device_usage_atrisk_one_missing(),
    )
}

/// Mounted probe runner plus an at-risk usage snapshot whose second device has a real path but
/// `device_size == 0` (a present device whose size probe failed), so ack must clear the latch
/// without writing a baseline -- the ADR 014 guard keys on size, not on the missing marker.
pub(crate) fn ack_mounted_probe_runner_with_zero_size_real_path_enospc_usage() -> MockRunner {
    ack_mounted_probe_runner_with_device_stats().with_output(
        CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: ack_mp(),
        },
        ack_btrfs_device_usage_atrisk_one_zero_size_real_path(),
    )
}

/// Mounted probe + device-stats runner plus a healthy (dead-band) usage stub, so a
/// mounted ack of an EnospcRisk latch whose pool recovered writes no snooze marker.
pub(crate) fn ack_mounted_probe_runner_with_healthy_enospc_usage() -> MockRunner {
    ack_mounted_probe_runner_with_device_stats().with_output(
        CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: ack_mp(),
        },
        ack_btrfs_device_usage_healthy(),
    )
}

/// Mounted runner whose show carries no FS UUID (plus the at-risk usage stub), so
/// the ack baseline probe parses usage but builds no `PoolKey` and writes nothing.
pub(crate) fn ack_mounted_probe_runner_no_uuid_with_enospc_usage() -> MockRunner {
    MockRunner::default()
        .with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: ack_mp(),
            },
            btrfs_show_2disk_no_uuid(),
        )
        .with_output(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName::from_basename("braid-disk1".into()),
            },
            cryptsetup_status_active("braid-disk1", "/dev/vda"),
        )
        .with_output(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName::from_basename("braid-disk3".into()),
            },
            cryptsetup_status_active("braid-disk3", "/dev/vdc"),
        )
        .with_output(
            CmdRequest::BtrfsDeviceStatsJson {
                mount_point: ack_mp(),
            },
            btrfs_device_stats_healthy(),
        )
        .with_output(
            CmdRequest::BtrfsDeviceUsageRaw {
                mount_point: ack_mp(),
            },
            ack_btrfs_device_usage_atrisk(),
        )
}
