//! Ack-scope fixtures for `cli/src/ack.rs`'s `mod tests`.
//!
//! Ack tests pin exact no-run, no-device-stats, race, and state-file
//! contracts. This module stays flat and uses explicit mock outputs so a new
//! production probe or side effect still surfaces through a missing mock or a
//! request-list assertion.

use super::shared::mock_ok;
use crate::alert::{AlertCause, AlertState, save_alert_latch};
use crate::cmd::{CmdError, CmdRequest, CommandRunner, MockRunner, RawCommandOutput};
use crate::probe::Filesystem;
use crate::state_paths::StatePaths;
use crate::types::{MapperName, MountPoint};

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
}

/// Canonical ack-test mount point shared by the ack runner outputs.
pub(crate) fn ack_mp() -> MountPoint {
    MountPoint("/mnt/storage".to_owned())
}

/// Ack-specific alert latch writer for tests that compose alert causes.
pub(crate) fn ack_write_latch(paths: &StatePaths, causes: Vec<AlertCause>) {
    let state = AlertState { causes };
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
                mapper: MapperName("braid-disk1".into()),
            },
            cryptsetup_status_active("braid-disk1", "/dev/vda"),
        )
        .with_output(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName("braid-disk3".into()),
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
