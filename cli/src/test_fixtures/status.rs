//! Status-scope fixtures for `cli/src/status.rs`'s `mod tests`.
//!
//! Status is read-only and its tests are dominated by per-test
//! `MockRunner` composition plus load-bearing missing-mock contracts, so
//! this module ships flat helpers instead of a topology installer or params
//! builder. A broad `with_handler` would silently resolve probes that some
//! status tests intentionally omit.
//!
//! Naming: every newly-exported helper carries a `status_` prefix. The
//! prefix keeps facade exports distinct from mount/shared helpers, and it
//! allowed the staged migration to import fixture helpers while same-purpose
//! local helpers still existed in `status.rs::tests`.

use super::shared::{disk_member, mock_ok};
use crate::alert::AlertCause;
use crate::cmd::{CmdRequest, LsblkFieldKind, MockRunner, RawCommandOutput};
use crate::config::Config;
use crate::membership::PoolMembership;
use crate::probe::Filesystem;
use crate::status::{DiskReport, DiskStatus, ScrubReport, StatusCode, StatusReport};
use crate::types::*;

// ---------------------------------------------------------------------------
// Filesystem
// ---------------------------------------------------------------------------

/// Status-scoped filesystem mock that only resolves `/proc/self/mountinfo`,
/// preserving status tests' guard against unexpected sysfs/preflight reads.
struct MockFs {
    paths: Vec<String>,
    mountinfo: String,
}

impl MockFs {
    fn new(paths: &[&str], mountinfo: &str) -> Self {
        Self {
            paths: paths.iter().map(|s| s.to_string()).collect(),
            mountinfo: mountinfo.to_owned(),
        }
    }
}

impl Filesystem for MockFs {
    fn exists(&self, path: &str) -> bool {
        self.paths.contains(&path.to_string())
    }

    fn is_block_device(&self, _path: &str) -> bool {
        false
    }

    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        if path == "/proc/self/mountinfo" {
            return Ok(self.mountinfo.clone());
        }
        Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
    }

    fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
        Ok(vec![])
    }
}

/// Mounted-btrfs mountinfo fixture with caller-supplied existing paths.
pub(crate) fn status_fs_mounted(paths: &[&str]) -> impl Filesystem {
    MockFs::new(
        paths,
        "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/disk1 rw\n",
    )
}

/// Offline mountinfo fixture for status tests that must short-circuit as not mounted.
pub(crate) fn status_fs_not_mounted(paths: &[&str]) -> impl Filesystem {
    MockFs::new(paths, "26 25 0:23 / / rw shared:1 - ext4 /dev/sda1 rw\n")
}

/// Non-btrfs mountinfo fixture for the ext4-at-mountpoint status branch.
pub(crate) fn status_fs_ext4(paths: &[&str]) -> impl Filesystem {
    MockFs::new(
        paths,
        "36 35 0:32 / /mnt/storage rw shared:1 - ext4 /dev/sda1 rw\n",
    )
}

/// Canonical three-disk mounted filesystem surface for status integration tests.
pub(crate) fn status_fs_three_disk() -> impl Filesystem {
    status_fs_mounted(&[
        "/dev/disk/by-id/disk1",
        "/dev/disk/by-id/disk2",
        "/dev/disk/by-id/disk3",
        "/dev/mapper/disk1",
        "/dev/mapper/disk2",
        "/dev/mapper/disk3",
    ])
}

/// Canonical one-disk mounted filesystem surface for status integration tests.
pub(crate) fn status_fs_one_disk() -> impl Filesystem {
    status_fs_mounted(&["/dev/disk/by-id/disk1", "/dev/mapper/disk1"])
}

// ---------------------------------------------------------------------------
// Identifier / config / membership primitives
// ---------------------------------------------------------------------------

/// Canonical status-test mount point used by all promoted command outputs.
pub(crate) fn status_mp() -> MountPoint {
    MountPoint("/mnt/storage".into())
}

/// Canonical status-test config; status config currently carries only mount point.
pub(crate) fn status_config() -> Config {
    Config::new(status_mp()).unwrap()
}

/// Single-disk membership fixture for compact-drive status rendering.
pub(crate) fn status_membership_1disk() -> PoolMembership {
    let mut membership = PoolMembership::empty();
    let (_, member) = disk_member(1, "disk1", "/dev/disk/by-id/disk1");
    membership
        .insert(
            LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
            member,
        )
        .expect("insert disk1");
    membership
}

// ---------------------------------------------------------------------------
// btrfs CLI output factories
// ---------------------------------------------------------------------------

/// One-device `btrfs filesystem show` output for single-disk status scenarios.
pub(crate) fn status_btrfs_show_1disk() -> RawCommandOutput {
    mock_ok(
        "btrfs filesystem show",
        "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
         \tTotal devices 1 FS bytes used 1.00GiB\n\
         \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/disk1\n",
    )
}

/// Healthy three-device `btrfs filesystem show` output for base status topology.
pub(crate) fn status_btrfs_show_3disk() -> RawCommandOutput {
    mock_ok(
        "btrfs filesystem show",
        "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
         \tTotal devices 3 FS bytes used 1.00GiB\n\
         \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/disk1\n\
         \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/disk2\n\
         \tdevid    3 size 10.00GiB used 2.00GiB path /dev/mapper/disk3\n",
    )
}

/// Degraded three-device output with one btrfs `MISSING` device.
pub(crate) fn status_btrfs_show_3disk_1missing() -> RawCommandOutput {
    mock_ok(
        "btrfs filesystem show",
        "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
         \tTotal devices 3 FS bytes used 1.00GiB\n\
         \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/disk1\n\
         \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/disk2\n\
         \t*** Some devices missing\n",
    )
}

/// Mixed missing output that drives the null-underlying plus btrfs-missing union test.
pub(crate) fn status_btrfs_show_3disk_1null_underlying_1missing() -> RawCommandOutput {
    mock_ok(
        "btrfs filesystem show",
        "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
         \tTotal devices 3 FS bytes used 1.00GiB\n\
         \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/disk1\n\
         \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/disk2\n\
         \tdevid    3 size 0 used 0 path MISSING\n\
         \t*** Some devices missing\n",
    )
}

/// Single-profile df JSON for one-disk status capacity tests.
pub(crate) fn status_btrfs_df_single() -> RawCommandOutput {
    mock_ok(
        "btrfs filesystem df",
        r#"{
  "filesystem-df": [
    { "bg-type": "Data", "bg-profile": "single", "total": 1073741824, "used": 536870912 },
    { "bg-type": "Metadata", "bg-profile": "single", "total": 268435456, "used": 65536 },
    { "bg-type": "System", "bg-profile": "single", "total": 4194304, "used": 16384 }
  ]
}"#,
    )
}

/// RAID1 df JSON used by healthy and degraded three-disk status tests.
pub(crate) fn status_btrfs_df_raid1() -> RawCommandOutput {
    mock_ok(
        "btrfs filesystem df",
        r#"{
  "filesystem-df": [
    { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
    { "bg-type": "System", "bg-profile": "RAID1", "total": 4194304, "used": 16384 },
    { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 65536 },
    { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3670016, "used": 0 }
  ]
}"#,
    )
}

/// Canonical `btrfs filesystem usage --raw` body for status capacity tests.
pub(crate) fn status_btrfs_usage_raw() -> RawCommandOutput {
    mock_ok(
        "btrfs filesystem usage",
        "Overall:\n\
         \tDevice size:\t\t\t1040187392\n\
         \tDevice allocated:\t\t503316480\n\
         \tDevice unallocated:\t\t536870912\n\
         \tUsed:\t\t\t\t33914880\n\
         \tFree (estimated):\t\t442957824\t(min: 442957824)\n\
         \tData ratio:\t\t\t2.00\n",
    )
}

/// Three-disk `btrfs device usage` body for RAID1 capacity estimation.
pub(crate) fn status_btrfs_device_usage_raw_3disk() -> RawCommandOutput {
    mock_ok(
        "btrfs device usage",
        "/dev/mapper/disk1, ID: 1\n\
         \x20  Device size:          346729130\n\
         \x20  Device slack:              0\n\
         \x20  Data,RAID1:           67108864\n\
         \x20  Metadata,RAID1:       33554432\n\
         \x20  System,RAID1:          4194304\n\
         \x20  Unallocated:         241871530\n\
         \n\
         /dev/mapper/disk2, ID: 2\n\
         \x20  Device size:          346729130\n\
         \x20  Device slack:              0\n\
         \x20  Data,RAID1:           67108864\n\
         \x20  Metadata,RAID1:       33554432\n\
         \x20  System,RAID1:          4194304\n\
         \x20  Unallocated:         241871530\n\
         \n\
         /dev/mapper/disk3, ID: 3\n\
         \x20  Device size:          346729130\n\
         \x20  Device slack:              0\n\
         \x20  Data,RAID1:           67108864\n\
         \x20  Metadata,RAID1:       33554432\n\
         \x20  System,RAID1:          4194304\n\
         \x20  Unallocated:         241871530\n",
    )
}

/// One-disk `btrfs device usage` body for single-profile status tests.
pub(crate) fn status_btrfs_device_usage_raw_1disk() -> RawCommandOutput {
    mock_ok(
        "btrfs device usage",
        "/dev/mapper/disk1, ID: 1\n\
         \x20  Device size:         1040187392\n\
         \x20  Device slack:              0\n\
         \x20  Data,single:         1073741824\n\
         \x20  Metadata,single:      268435456\n\
         \x20  System,single:          4194304\n\
         \x20  Unallocated:                 0\n",
    )
}

/// Scrub-status output for pools that have never been scrubbed.
pub(crate) fn status_btrfs_scrub_never() -> RawCommandOutput {
    mock_ok(
        "btrfs scrub status --raw",
        "UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\nScrub started:    no stats available\n",
    )
}

/// Scrub-status output for a completed scrub with no errors.
pub(crate) fn status_btrfs_scrub_finished() -> RawCommandOutput {
    mock_ok(
        "btrfs scrub status --raw",
        "UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
         Scrub started:    Mon Feb 23 10:00:00 2026\n\
         Status:           finished\n\
         Duration:         0:00:01\n\
         Total to scrub:   1073741824\n\
         Rate:             1073741824/s\n\
         Error summary:    no errors found\n",
    )
}

/// Scrub-status output for a completed scrub with csum errors.
pub(crate) fn status_btrfs_scrub_finished_with_errors() -> RawCommandOutput {
    mock_ok(
        "btrfs scrub status --raw",
        "UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
         Scrub started:    Mon Feb 23 10:00:00 2026\n\
         Status:           finished\n\
         Duration:         0:00:01\n\
         Total to scrub:   1073741824\n\
         Rate:             1073741824/s\n\
         Error summary:    csum=50\n",
    )
}

/// Scrub-status output for an aborted scrub.
pub(crate) fn status_btrfs_scrub_aborted() -> RawCommandOutput {
    mock_ok(
        "btrfs scrub status --raw",
        "UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
         Scrub started:    Mon Feb 23 10:00:00 2026\n\
         Status:           aborted\n\
         Duration:         0:00:01\n\
         Total to scrub:   1073741824\n\
         Rate:             1073741824/s\n\
         Error summary:    no errors found\n",
    )
}

/// Scrub-status output for an interrupted scrub.
pub(crate) fn status_btrfs_scrub_interrupted() -> RawCommandOutput {
    mock_ok(
        "btrfs scrub status --raw",
        "UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
         Scrub started:    Mon Feb 23 10:00:00 2026\n\
         Status:           interrupted\n\
         Duration:         0:00:01\n\
         Total to scrub:   1073741824\n\
         Rate:             1073741824/s\n\
         Error summary:    no errors found\n",
    )
}

/// Zero-error three-disk device-stats JSON for status report assembly.
pub(crate) fn status_btrfs_device_stats_3disk() -> RawCommandOutput {
    mock_ok(
        "btrfs device stats",
        r#"{"device-stats": [
            {"device": "/dev/mapper/disk1", "devid": 1, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
            {"device": "/dev/mapper/disk2", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
            {"device": "/dev/mapper/disk3", "devid": 3, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
        ]}"#,
    )
}

// ---------------------------------------------------------------------------
// cryptsetup output factories
// ---------------------------------------------------------------------------

/// Active-mapper `cryptsetup status` output with caller-supplied backing device.
pub(crate) fn status_cryptsetup_status_active(mapper: &str, device: &str) -> RawCommandOutput {
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

/// `cryptsetup luksUUID` output for status tests that build request keys inline.
pub(crate) fn status_cryptsetup_uuid_ok(device: &str, uuid: &str) -> RawCommandOutput {
    mock_ok(
        &format!("cryptsetup luksUUID {device}"),
        &format!("{uuid}\n"),
    )
}

/// Raw `cryptsetup isLuks` output for PresentNotLuks classification tests.
pub(crate) fn status_is_luks_raw(device: &str, exit: i32, stderr: &str) -> RawCommandOutput {
    RawCommandOutput {
        cmd: format!("cryptsetup isLuks {device}"),
        stdout: String::new(),
        stderr: stderr.to_owned(),
        exit_status: exit,
    }
}

/// Raw text-form `cryptsetup luksDump` output for header classification tests.
pub(crate) fn status_luks_dump_text_raw(
    device: &str,
    exit: i32,
    stdout: &str,
    stderr: &str,
) -> RawCommandOutput {
    RawCommandOutput {
        cmd: format!("cryptsetup luksDump {device}"),
        stdout: stdout.to_owned(),
        stderr: stderr.to_owned(),
        exit_status: exit,
    }
}

// ---------------------------------------------------------------------------
// lsblk output factory
// ---------------------------------------------------------------------------

/// Single-field `lsblk` success output with the trailing newline parsers expect.
pub(crate) fn status_lsblk_field_ok(cmd: &str, value: &str) -> RawCommandOutput {
    mock_ok(cmd, &format!("{value}\n"))
}

// ---------------------------------------------------------------------------
// Composite runners
// ---------------------------------------------------------------------------

/// Base healthy three-disk runner with only the probes every mounted status path needs.
///
/// Intentionally uses explicit `with_output` calls, not `with_handler`, so a
/// new production probe surfaces as `MissingMock` instead of being hidden by a
/// broad topology handler.
pub(crate) fn status_runner_healthy_3disk_base() -> MockRunner {
    MockRunner::default()
        .with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: status_mp(),
            },
            status_btrfs_show_3disk(),
        )
        .with_output(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName("disk1".into()),
            },
            status_cryptsetup_status_active("disk1", "/dev/vda"),
        )
        .with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/vda".into(),
            },
            status_cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
        )
        .with_output(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName("disk2".into()),
            },
            status_cryptsetup_status_active("disk2", "/dev/vdb"),
        )
        .with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/vdb".into(),
            },
            status_cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
        )
        .with_output(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName("disk3".into()),
            },
            status_cryptsetup_status_active("disk3", "/dev/vdc"),
        )
        .with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/vdc".into(),
            },
            status_cryptsetup_uuid_ok("/dev/vdc", "33333333-3333-3333-3333-333333333333"),
        )
        .with_output(
            CmdRequest::BtrfsFilesystemDfJson {
                mount_point: status_mp(),
            },
            status_btrfs_df_raid1(),
        )
        .with_output(
            CmdRequest::BtrfsFilesystemUsageRaw {
                mount_point: status_mp(),
            },
            status_btrfs_usage_raw(),
        )
        .with_output(
            CmdRequest::BtrfsDeviceUsageRaw {
                mount_point: status_mp(),
            },
            status_btrfs_device_usage_raw_3disk(),
        )
        .with_output(
            CmdRequest::BtrfsScrubStatus {
                mount_point: status_mp(),
            },
            status_btrfs_scrub_never(),
        )
        .with_output(
            CmdRequest::BtrfsDeviceStatsJson {
                mount_point: status_mp(),
            },
            status_btrfs_device_stats_3disk(),
        )
}

/// Adds verbose-mode per-config-disk probes to a base healthy three-disk runner.
pub(crate) fn status_runner_healthy_3disk_verbose(runner: MockRunner) -> MockRunner {
    runner
        .with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/disk/by-id/disk1".into(),
            },
            status_cryptsetup_uuid_ok(
                "/dev/disk/by-id/disk1",
                "11111111-1111-1111-1111-111111111111",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/disk/by-id/disk2".into(),
            },
            status_cryptsetup_uuid_ok(
                "/dev/disk/by-id/disk2",
                "22222222-2222-2222-2222-222222222222",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/disk/by-id/disk3".into(),
            },
            status_cryptsetup_uuid_ok(
                "/dev/disk/by-id/disk3",
                "33333333-3333-3333-3333-333333333333",
            ),
        )
        .with_output(
            CmdRequest::BtrfsDeviceStatsJson {
                mount_point: status_mp(),
            },
            status_btrfs_device_stats_3disk(),
        )
        .with_output(
            CmdRequest::LsblkField {
                device: "/dev/disk/by-id/disk1".into(),
                field: LsblkFieldKind::Model,
            },
            status_lsblk_field_ok("lsblk", "VBOX HARDDISK"),
        )
        .with_output(
            CmdRequest::LsblkField {
                device: "/dev/disk/by-id/disk1".into(),
                field: LsblkFieldKind::Serial,
            },
            status_lsblk_field_ok("lsblk", "disk1"),
        )
        .with_output(
            CmdRequest::LsblkField {
                device: "/dev/disk/by-id/disk2".into(),
                field: LsblkFieldKind::Model,
            },
            status_lsblk_field_ok("lsblk", "VBOX HARDDISK"),
        )
        .with_output(
            CmdRequest::LsblkField {
                device: "/dev/disk/by-id/disk2".into(),
                field: LsblkFieldKind::Serial,
            },
            status_lsblk_field_ok("lsblk", "disk2"),
        )
        .with_output(
            CmdRequest::LsblkField {
                device: "/dev/disk/by-id/disk3".into(),
                field: LsblkFieldKind::Model,
            },
            status_lsblk_field_ok("lsblk", "VBOX HARDDISK"),
        )
        .with_output(
            CmdRequest::LsblkField {
                device: "/dev/disk/by-id/disk3".into(),
                field: LsblkFieldKind::Serial,
            },
            status_lsblk_field_ok("lsblk", "disk3"),
        )
}

// ---------------------------------------------------------------------------
// Pool / config-disk / report data builders
// ---------------------------------------------------------------------------

/// Empty mounted pool state used by unpooled PresentNotLuks classification tests.
pub(crate) fn status_pool_empty() -> PoolState {
    PoolState {
        mounted: true,
        devices: vec![],
        missing_count: 0,
        missing_devids: vec![],
        total_devices: 0,
        fsid: None,
        null_underlying: vec![],
    }
}

/// One-element config-disk set in the `PresentNotLuks` state under test.
pub(crate) fn status_cfg_present_not_luks(name: &str, by_id: &str) -> Vec<ConfigDisk> {
    vec![ConfigDisk {
        name: DiskName::parse(name).expect("valid disk name in test fixture"),
        by_id_path: ByIdPath::parse(by_id).unwrap(),
        state: ConfigDiskState::PresentNotLuks,
    }]
}

/// Canonical three-disk RAID1 status report with caller-supplied scrub state.
pub(crate) fn status_report_with_scrub(last_scrub: ScrubReport) -> StatusReport {
    StatusReport {
        mount_point: status_mp(),
        status: StatusCode::Intact,
        total_devices: Some(3),
        present_count: Some(3),
        missing_count: Some(0),
        profile: Some("RAID1".to_owned()),
        capacity: None,
        last_scrub: Some(last_scrub),
        balance: None,
        allocation: None,
        disks: vec![],
        advisories: vec![],
        alert_active: false,
        alert_causes: vec![],
        missing_devids: vec![],
    }
}

/// Degraded alert-active report builder for alert rendering tests.
pub(crate) fn status_report_with_alerts(
    disks: Vec<DiskReport>,
    causes: Vec<AlertCause>,
) -> StatusReport {
    StatusReport {
        mount_point: status_mp(),
        status: StatusCode::Degraded,
        total_devices: Some(3),
        present_count: Some(2),
        missing_count: Some(1),
        profile: None,
        capacity: None,
        last_scrub: None,
        balance: None,
        allocation: None,
        disks,
        advisories: vec![],
        alert_active: true,
        alert_causes: causes,
        missing_devids: vec![],
    }
}

/// Present disk report keyed by name/devid for alert-name lookup tests.
pub(crate) fn status_disk_report_named(name: &str, devid: u64) -> DiskReport {
    DiskReport {
        name: name.into(),
        mapper: format!("braid-{name}"),
        by_id: format!("/dev/disk/by-id/{name}"),
        luks_uuid: "00000000-0000-0000-0000-000000000000".into(),
        devid: Some(devid.to_string()),
        underlying: None,
        status: DiskStatus::Present,
        errors: None,
    }
}
