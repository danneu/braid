//! Remove-scope fixtures: `RemovalPool`, `RemoveParamsBuilder`, and
//! remove-only `PoolFixture` constructors.

use super::shared::{
    DeviceUsageSpec, PoolFixture, canonical_luks_uuid, device_usage_raw_body, disk_member_with,
    mock_ok,
};
use crate::cmd::{CmdRequest, MockRunner};
use crate::config::{Config, mapper_name};
use crate::confirm::RecordingConfirm;
use crate::inhibit::RecordingInhibitor;
use crate::membership::{self, PoolMembership};
use crate::progress::{self, ProgressOutput};
use crate::remove::RemoveParams;
use crate::state_paths::StatePaths;
use crate::types::{Devid, DiskName, LuksUuid, PoolDevice};

const TWO_DISK_SHOW: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
     \tTotal devices 2 FS bytes used 16.17MiB\n\
     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
     \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n";

const THREE_DISK_SHOW: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
     \tTotal devices 3 FS bytes used 16.17MiB\n\
     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
     \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\
     \tdevid    3 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n";

const TWO_DISK_DF_JSON: &str = r#"{
  "filesystem-df": [
    { "bg-type": "Data", "bg-profile": "RAID1", "total": 52428800, "used": 52428800 },
    { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 10485760, "used": 10485760 },
    { "bg-type": "System", "bg-profile": "RAID1", "total": 32768, "used": 32768 }
  ]
}"#;

const THREE_DISK_DF_JSON: &str = TWO_DISK_DF_JSON;

// `data + 2*metadata + 2*system` = 60 + 2*30 + 2*~0 MiB = ~120 MiB demand,
// which exceeds the 100 MiB survivor `device_size` in
// `overcommitted_survivor_usage_stdout`. Pairs with that usage body to drive
// the single-survivor capacity check into a clean refusal.
const OVERCOMMITTED_SURVIVOR_DF_JSON: &str = r#"{
  "filesystem-df": [
    { "bg-type": "Data", "bg-profile": "RAID1", "total": 62914560, "used": 62914560 },
    { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 31457280, "used": 31457280 },
    { "bg-type": "System", "bg-profile": "RAID1", "total": 32768, "used": 32768 }
  ]
}"#;

impl PoolFixture {
    /// pool.json: disk1 + disk2 + disk3. Kept remove-scoped until another
    /// command needs the same three-member steady-state topology. Each disk
    /// is keyed by `canonical_luks_uuid(n)` (via `luks_uuid_for_disk_name`) in
    /// disk-number order, so the membership key equals the UUID the live
    /// `RemovalPool` probe reports for `/dev/vd{b,c,d}`. The seed passed to
    /// `disk_member_with` feeds only its now-discarded UUID -- the member's
    /// fields come from name/by_id/devid -- so it is vestigial here.
    pub(crate) fn three_disk_healthy() -> Self {
        let base = Self::empty_inner();
        let mut m = PoolMembership::empty();
        for (seed, name) in [(1u64, "disk1"), (2, "disk2"), (3, "disk3")] {
            let (_, member) = disk_member_with(
                seed,
                name,
                &format!("/dev/disk/by-id/virtio-{name}"),
                None,
                None,
            );
            let uuid = luks_uuid_for_disk_name(name).expect("fixture disk name");
            m.insert(uuid, member).expect("fixture insert");
        }
        membership::save_membership(&m, &base.paths).expect("save_membership");
        Self {
            _state_tmp: base.state_tmp,
            paths: base.paths,
            _config_tmp: base.config_tmp,
            config: base.config,
            pass_path: base.pass_path,
            inhibitor: RecordingInhibitor::new(),
            confirm: RecordingConfirm::new(),
        }
    }

    /// Start a `RemoveParamsBuilder` whose defaults match command-level
    /// migrated tests: remove disk2, yes=true, dry_run=false, progress=Off.
    pub(crate) fn remove_params(&self) -> RemoveParamsBuilder<'_> {
        RemoveParamsBuilder {
            config: &self.config,
            name: "disk2",
            dry_run: false,
            yes: true,
            progress: ProgressOutput::Off,
            paths: &self.paths,
            inhibitor: &self.inhibitor,
            confirm: &self.confirm,
        }
    }
}

/// Canonical remove pool topology installer for command and planner tests.
///
/// The topology is success-only by design. Tests inject failures with a
/// later `MockRunner::with_handler`, which shadows this broad handler.
pub(crate) struct RemovalPool {
    show: &'static str,
    usage_raw: String,
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
                let dev = mapper_underlying(mapper.as_str())?;
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
                Some(Ok(mock_ok(
                    "btrfs device usage --raw /mnt/storage",
                    &usage_raw,
                )))
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
    config: &'a Config,
    name: &'a str,
    dry_run: bool,
    yes: bool,
    progress: ProgressOutput,
    paths: &'a StatePaths,
    inhibitor: &'a RecordingInhibitor,
    confirm: &'a RecordingConfirm,
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

    pub(crate) fn yes(mut self, yes: bool) -> Self {
        self.yes = yes;
        self
    }

    pub(crate) fn build(self) -> RemoveParams<'a> {
        RemoveParams {
            config: self.config,
            name: self.name,
            dry_run: self.dry_run,
            yes: self.yes,
            progress: self.progress,
            paths: self.paths,
            sleep_inhibitor: self.inhibitor,
            confirm: self.confirm,
            sleeper: &progress::NoopSleeper,
        }
    }
}

/// Canonical target device used by direct `check_eviction_space` tests.
pub(crate) fn target_device(name: &str) -> PoolDevice {
    let disk = name.strip_prefix("disk").unwrap_or(name);
    let raw_devid: u64 = disk.parse().unwrap_or(1);
    let luks_uuid = luks_uuid_for_disk_name(name).unwrap_or_else(|| {
        LuksUuid::parse("00000000-0000-0000-0000-000000000000").expect("valid fixture UUID")
    });
    let disk_name = DiskName::parse(name).expect("valid fixture disk name");
    let mapper = mapper_name(&disk_name);
    PoolDevice {
        devid: Devid::new(raw_devid),
        mapper: mapper.clone(),
        luks_uuid,
        underlying: mapper_underlying(mapper.as_str())
            .unwrap_or("/dev/vda")
            .to_owned(),
    }
}

/// Valid two-disk `btrfs device usage --raw` stdout for override tests.
pub(crate) fn valid_two_disk_usage_stdout() -> String {
    device_usage_raw_body(&[remove_usage_live_device(1), remove_usage_live_device(2)])
}

/// Valid three-disk `btrfs device usage --raw` stdout for override tests.
pub(crate) fn valid_three_disk_usage_stdout() -> String {
    device_usage_raw_body(&[
        remove_usage_live_device(1),
        remove_usage_live_device(2),
        remove_usage_live_device(3),
    ])
}

/// Valid two-disk `btrfs --format json filesystem df` stdout for overrides.
pub(crate) fn valid_two_disk_df_json() -> &'static str {
    TWO_DISK_DF_JSON
}

/// Valid three-disk `btrfs --format json filesystem df` stdout for overrides.
pub(crate) fn valid_three_disk_df_json() -> &'static str {
    THREE_DISK_DF_JSON
}

/// `btrfs device usage --raw` stdout where the survivor (devid 1, the
/// non-target when removing disk2/devid 2) has a `device_size` too small to
/// absorb the post-balance single + DUP demand. The target stanza keeps its
/// normal size -- only the survivor's `device_size`/`device_slack` feed the
/// 2->1 capacity check. Pairs with `overcommitted_survivor_df_json` so the
/// execute-time re-check refuses a survivor that drifted over capacity after
/// a healthy plan.
pub(crate) fn overcommitted_survivor_usage_stdout() -> String {
    device_usage_raw_body(&[
        // Survivor (devid 1): 100 MiB device, far smaller than the ~120 MiB
        // demand in OVERCOMMITTED_SURVIVOR_DF_JSON.
        DeviceUsageSpec::live(
            "/dev/mapper/braid-disk1",
            1,
            104_857_600,
            &[
                ("Data", "RAID1", 62_914_560),
                ("Metadata", "RAID1", 31_457_280),
                ("System", "RAID1", 32_768),
            ],
            10_452_992,
        ),
        // Target (devid 2, being removed): its size is irrelevant to the check.
        remove_usage_live_device(2),
    ])
}

/// `btrfs --format json filesystem df` stdout whose `data + 2*metadata +
/// 2*system` demand (~120 MiB) exceeds the 100 MiB survivor in
/// `overcommitted_survivor_usage_stdout`, forcing `check_single_survivor`
/// into a "not enough space on surviving device" refusal.
pub(crate) fn overcommitted_survivor_df_json() -> &'static str {
    OVERCOMMITTED_SURVIVOR_DF_JSON
}

fn remove_usage_live_device(devid: u64) -> DeviceUsageSpec {
    DeviceUsageSpec::live(
        &format!("/dev/mapper/braid-disk{devid}"),
        devid,
        1_073_741_824,
        &[
            ("Data", "RAID1", 52_428_800),
            ("Metadata", "RAID1", 10_485_760),
            ("System", "RAID1", 32_768),
        ],
        1_010_794_496,
    )
}

fn mapper_underlying(mapper: &str) -> Option<&'static str> {
    match mapper {
        "braid-disk1" => Some("/dev/vdb"),
        "braid-disk2" => Some("/dev/vdc"),
        "braid-disk3" => Some("/dev/vdd"),
        _ => None,
    }
}

fn luks_uuid_for_device(device: &str) -> Option<LuksUuid> {
    let n = match device {
        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => 1,
        "/dev/vdc" | "/dev/disk/by-id/virtio-disk2" => 2,
        "/dev/vdd" | "/dev/disk/by-id/virtio-disk3" => 3,
        _ => return None,
    };
    Some(canonical_luks_uuid(n))
}

fn luks_uuid_for_disk_name(name: &str) -> Option<LuksUuid> {
    let n = match name {
        "disk1" => 1,
        "disk2" => 2,
        "disk3" => 3,
        _ => return None,
    };
    Some(canonical_luks_uuid(n))
}
