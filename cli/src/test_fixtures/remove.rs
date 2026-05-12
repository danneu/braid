//! Remove-scope fixtures: `RemovalPool`, `RemoveParamsBuilder`, and
//! remove-only `PoolFixture` constructors.

use super::shared::{PoolFixture, disk_member_with, mock_ok};
use crate::cmd::{CmdRequest, MockRunner};
use crate::inhibit::RecordingInhibitor;
use crate::membership::{self, PoolMembership};
use crate::progress::{self, ProgressOutput};
use crate::remove::RemoveParams;
use crate::state_paths::StatePaths;
use crate::types::{LuksUuid, MapperName, PoolDevice};
use std::path::Path;

const TWO_DISK_SHOW: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
     \tTotal devices 2 FS bytes used 16.17MiB\n\
     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
     \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n";

const THREE_DISK_SHOW: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
     \tTotal devices 3 FS bytes used 16.17MiB\n\
     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
     \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\
     \tdevid    3 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n";

const TWO_DISK_USAGE_RAW: &str = "/dev/mapper/braid-disk1, ID: 1\n\
     \x20  Device size:         1073741824\n\
     \x20  Device slack:                 0\n\
     \x20  Data,RAID1:            52428800\n\
     \x20  Metadata,RAID1:        10485760\n\
     \x20  System,RAID1:             32768\n\
     \x20  Unallocated:         1010794496\n\n\
     /dev/mapper/braid-disk2, ID: 2\n\
     \x20  Device size:         1073741824\n\
     \x20  Device slack:                 0\n\
     \x20  Data,RAID1:            52428800\n\
     \x20  Metadata,RAID1:        10485760\n\
     \x20  System,RAID1:             32768\n\
     \x20  Unallocated:         1010794496\n";

const THREE_DISK_USAGE_RAW: &str = "/dev/mapper/braid-disk1, ID: 1\n\
     \x20  Device size:         1073741824\n\
     \x20  Device slack:                 0\n\
     \x20  Data,RAID1:            52428800\n\
     \x20  Metadata,RAID1:        10485760\n\
     \x20  System,RAID1:             32768\n\
     \x20  Unallocated:         1010794496\n\n\
     /dev/mapper/braid-disk2, ID: 2\n\
     \x20  Device size:         1073741824\n\
     \x20  Device slack:                 0\n\
     \x20  Data,RAID1:            52428800\n\
     \x20  Metadata,RAID1:        10485760\n\
     \x20  System,RAID1:             32768\n\
     \x20  Unallocated:         1010794496\n\n\
     /dev/mapper/braid-disk3, ID: 3\n\
     \x20  Device size:         1073741824\n\
     \x20  Device slack:                 0\n\
     \x20  Data,RAID1:            52428800\n\
     \x20  Metadata,RAID1:        10485760\n\
     \x20  System,RAID1:             32768\n\
     \x20  Unallocated:         1010794496\n";

const TWO_DISK_DF_JSON: &str = r#"{
  "filesystem-df": [
    { "bg-type": "Data", "bg-profile": "RAID1", "total": 52428800, "used": 52428800 },
    { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 10485760, "used": 10485760 },
    { "bg-type": "System", "bg-profile": "RAID1", "total": 32768, "used": 32768 }
  ]
}"#;

const THREE_DISK_DF_JSON: &str = TWO_DISK_DF_JSON;

impl PoolFixture {
    /// pool.json: disk1 + disk2 + disk3. Kept remove-scoped until another
    /// command needs the same three-member steady-state topology. Each
    /// disk's UUID seed encodes its disk number (`disk1` -> seed 1, etc.)
    /// so fixture UUIDs read at a glance and stay in disk-number order.
    pub(crate) fn three_disk_healthy() -> Self {
        let base = Self::empty_inner();
        let mut m = PoolMembership::empty();
        for (seed, name) in [(1u64, "disk1"), (2, "disk2"), (3, "disk3")] {
            let (uuid, member) = disk_member_with(
                seed,
                name,
                &format!("/dev/disk/by-id/virtio-{name}"),
                None,
                None,
            );
            m.insert(uuid, member).expect("fixture insert");
        }
        membership::save_membership(&m, &base.paths).expect("save_membership");
        Self {
            _state_tmp: base.state_tmp,
            paths: base.paths,
            _config_tmp: base.config_tmp,
            config_path: base.config_path,
            config: base.config,
            pass_path: base.pass_path,
            inhibitor: RecordingInhibitor::new(),
        }
    }

    /// Start a `RemoveParamsBuilder` whose defaults match command-level
    /// migrated tests: remove disk2, yes=true, dry_run=false, progress=Off.
    pub(crate) fn remove_params(&self) -> RemoveParamsBuilder<'_> {
        RemoveParamsBuilder {
            config_path: &self.config_path,
            name: "disk2",
            dry_run: false,
            yes: true,
            progress: ProgressOutput::Off,
            paths: &self.paths,
            inhibitor: &self.inhibitor,
        }
    }
}

/// Canonical remove pool topology installer for command and planner tests.
///
/// The topology is success-only by design. Tests inject failures with a
/// later `MockRunner::with_handler`, which shadows this broad handler.
pub(crate) struct RemovalPool {
    show: &'static str,
    usage_raw: &'static str,
    df_json: &'static str,
}

impl RemovalPool {
    /// Live disk1 + disk2, where removing disk2 leaves one survivor and
    /// therefore runs the RAID1 -> single balance path.
    pub(crate) fn two_disk() -> Self {
        Self {
            show: TWO_DISK_SHOW,
            usage_raw: valid_two_disk_usage_stdout(),
            df_json: valid_two_disk_df_json(),
        }
    }

    /// Live disk1 + disk2 + disk3, where removing one disk leaves two
    /// survivors and exercises the soft-warn eviction preflight branch.
    pub(crate) fn three_disk() -> Self {
        Self {
            show: THREE_DISK_SHOW,
            usage_raw: valid_three_disk_usage_stdout(),
            df_json: valid_three_disk_df_json(),
        }
    }

    /// Register the complete steady-state remove command surface on
    /// `runner`; per-test handlers registered afterwards override it.
    pub(crate) fn install(self, runner: MockRunner) -> MockRunner {
        let show = self.show;
        let usage_raw = self.usage_raw;
        let df_json = self.df_json;

        runner.with_handler(move |req| match req {
            CmdRequest::BtrfsFilesystemShow { mount_point } => {
                Some(Ok(mock_ok(&format!("btrfs filesystem show {mount_point}"), show)))
            }
            CmdRequest::CryptsetupStatus { mapper } => {
                let dev = mapper_underlying(mapper)?;
                Some(Ok(mock_ok(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"
                    ),
                )))
            }
            CmdRequest::CryptsetupLuksUuid { device } => {
                let uuid = luks_uuid_for_device(device)?;
                Some(Ok(mock_ok(
                    &format!("cryptsetup luksUUID {device}"),
                    &format!("{uuid}\n"),
                )))
            }
            CmdRequest::BtrfsBalanceStatus { .. } => Some(Ok(mock_ok(
                "btrfs balance status",
                "No balance found on '/mnt/storage'\n",
            ))),
            CmdRequest::BtrfsDeviceUsageRaw { .. } => {
                Some(Ok(mock_ok("btrfs device usage --raw /mnt/storage", usage_raw)))
            }
            CmdRequest::BtrfsFilesystemDfJson { .. } => Some(Ok(mock_ok(
                "btrfs --format json filesystem df /mnt/storage",
                df_json,
            ))),
            CmdRequest::BtrfsBalanceSingle { .. } => {
                Some(Ok(mock_ok("btrfs balance start", "")))
            }
            CmdRequest::BtrfsDeviceRemove { .. } => {
                Some(Ok(mock_ok("btrfs device remove", "")))
            }
            CmdRequest::CryptsetupClose { .. } => Some(Ok(mock_ok("cryptsetup close", ""))),
            _ => None,
        })
    }
}

/// Per-test `RemoveParams` builder over the remove command defaults.
///
/// The fixture owns the temp config/state paths and inhibitor; tests only
/// override the command intent (`name`, dry-run, yes, progress).
pub(crate) struct RemoveParamsBuilder<'a> {
    config_path: &'a Path,
    name: &'a str,
    dry_run: bool,
    yes: bool,
    progress: ProgressOutput,
    paths: &'a StatePaths,
    inhibitor: &'a RecordingInhibitor,
}

impl<'a> RemoveParamsBuilder<'a> {
    pub(crate) fn name(mut self, name: &'a str) -> Self {
        self.name = name;
        self
    }

    pub(crate) fn dry_run(mut self, dry_run: bool) -> Self {
        self.dry_run = dry_run;
        self
    }

    #[allow(dead_code)]
    pub(crate) fn yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
    }

    #[allow(dead_code)]
    pub(crate) fn progress(mut self, progress: ProgressOutput) -> Self {
        self.progress = progress;
        self
    }

    pub(crate) fn build(self) -> RemoveParams<'a> {
        RemoveParams {
            config_path: self.config_path,
            name: self.name,
            dry_run: self.dry_run,
            yes: self.yes,
            progress: self.progress,
            paths: self.paths,
            sleep_inhibitor: self.inhibitor,
            sleeper: &progress::NoopSleeper,
        }
    }
}

/// Canonical target device used by direct `check_eviction_space` tests.
pub(crate) fn target_device(name: &str) -> PoolDevice {
    let disk = name.strip_prefix("disk").unwrap_or(name);
    let devid = disk.parse().unwrap_or(1);
    let uuid_raw = luks_uuid_for_disk_name(name).unwrap_or("00000000-0000-0000-0000-000000000000");
    PoolDevice {
        devid,
        mapper: MapperName(format!("braid-{name}")),
        luks_uuid: LuksUuid::parse(uuid_raw).expect("valid fixture UUID"),
        underlying: mapper_underlying(&format!("braid-{name}"))
            .unwrap_or("/dev/vda")
            .to_owned(),
    }
}

/// Valid two-disk `btrfs device usage --raw` stdout for override tests.
pub(crate) fn valid_two_disk_usage_stdout() -> &'static str {
    TWO_DISK_USAGE_RAW
}

/// Valid three-disk `btrfs device usage --raw` stdout for override tests.
pub(crate) fn valid_three_disk_usage_stdout() -> &'static str {
    THREE_DISK_USAGE_RAW
}

/// Valid two-disk `btrfs --format json filesystem df` stdout for overrides.
pub(crate) fn valid_two_disk_df_json() -> &'static str {
    TWO_DISK_DF_JSON
}

/// Valid three-disk `btrfs --format json filesystem df` stdout for overrides.
pub(crate) fn valid_three_disk_df_json() -> &'static str {
    THREE_DISK_DF_JSON
}

fn mapper_underlying(mapper: &str) -> Option<&'static str> {
    match mapper {
        "braid-disk1" => Some("/dev/vdb"),
        "braid-disk2" => Some("/dev/vdc"),
        "braid-disk3" => Some("/dev/vdd"),
        _ => None,
    }
}

fn luks_uuid_for_device(device: &str) -> Option<&'static str> {
    match device {
        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => Some("11111111-1111-1111-1111-111111111111"),
        "/dev/vdc" | "/dev/disk/by-id/virtio-disk2" => Some("22222222-2222-2222-2222-222222222222"),
        "/dev/vdd" | "/dev/disk/by-id/virtio-disk3" => Some("33333333-3333-3333-3333-333333333333"),
        _ => None,
    }
}

fn luks_uuid_for_disk_name(name: &str) -> Option<&'static str> {
    match name {
        "disk1" => Some("11111111-1111-1111-1111-111111111111"),
        "disk2" => Some("22222222-2222-2222-2222-222222222222"),
        "disk3" => Some("33333333-3333-3333-3333-333333333333"),
        _ => None,
    }
}
