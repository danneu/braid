//! Remove-missing fixtures: `RemoveMissingPool` topology installer,
//! `RemoveMissingParamsBuilder`, and remove-missing-only `PoolFixture`
//! constructors with pinned devids.
//!
//! Pinned devids are scope-local because `--missing-id N` resolves
//! through the pool.json membership map; the shared `two_disk_healthy`
//! and `one_live_one_missing` constructors only pin disk2.

use super::shared::{DeviceUsageSpec, PoolFixture, device_usage_raw_body, disk_member_with, mock_ok};
use crate::cmd::{CmdRequest, MockRunner};
use crate::config::Config;
use crate::confirm::RecordingConfirm;
use crate::inhibit::RecordingInhibitor;
use crate::membership::{self, PoolMembership};
use crate::progress::{self, ProgressOutput};
use crate::remove_missing::RemoveMissingParams;
use crate::state_paths::StatePaths;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const THREE_DISK_PRE_SHOW: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
     \tTotal devices 3 FS bytes used 16.17MiB\n\
     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
     \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\
     \tdevid    3 size 0 used 0 path MISSING\n";

const THREE_DISK_POST_SHOW: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
     \tTotal devices 2 FS bytes used 16.17MiB\n\
     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
     \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n";

const TWO_DISK_PRE_SHOW: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
     \tTotal devices 2 FS bytes used 16.17MiB\n\
     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
     \tdevid    2 size 0 used 0 path MISSING\n";

const TWO_DISK_POST_SHOW: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
     \tTotal devices 1 FS bytes used 16.17MiB\n\
     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n";

fn usage_raw_three_disk_one_missing() -> String {
    device_usage_raw_body(&[
        remove_missing_usage_live_device(1),
        remove_missing_usage_live_device(2),
        DeviceUsageSpec::missing(3, &[("Data", "RAID1", 67_108_864)], 0),
    ])
}

fn usage_raw_two_disk_one_missing() -> String {
    device_usage_raw_body(&[
        remove_missing_usage_live_device(1),
        DeviceUsageSpec::missing(2, &[("Data", "RAID1", 67_108_864)], 0),
    ])
}

fn remove_missing_usage_live_device(devid: u64) -> DeviceUsageSpec {
    DeviceUsageSpec::live(
        &format!("/dev/mapper/braid-disk{devid}"),
        devid,
        520_093_696,
        &[("Data", "RAID1", 67_108_864)],
        452_984_832,
    )
}

/// Canonical pool topology installer for `remove-missing` tests.
///
/// State flips on `remove_done: AtomicBool` after the broad handler
/// observes a successful `BtrfsDeviceRemove`, so subsequent
/// `BtrfsFilesystemShow` probes return the post-remove body. Per-test
/// handlers registered after `install` win via reverse-order dispatch;
/// if such a handler shadows `BtrfsDeviceRemove`, it must call
/// `remove_done.store(true, SeqCst)` itself or the topology will keep
/// reporting the missing devid post-remove.
pub(crate) struct RemoveMissingPool {
    pre_show: &'static str,
    post_show: &'static str,
    usage_raw: String,
    still_degraded_after: bool,
}

impl RemoveMissingPool {
    /// 3-disk pool with devid 3 reported MISSING; post-remove flips to
    /// 2 healthy survivors. The default for command-level success tests
    /// and the `>= 2` survivor branch of `check_relocation_space`.
    pub(crate) fn three_disk_one_missing() -> Self {
        Self {
            pre_show: THREE_DISK_PRE_SHOW,
            post_show: THREE_DISK_POST_SHOW,
            usage_raw: usage_raw_three_disk_one_missing(),
            still_degraded_after: false,
        }
    }

    /// 2-disk pool with devid 2 reported MISSING; post-remove leaves
    /// one healthy survivor. Drives the single-survivor preflight
    /// skip; tests assert `BtrfsDeviceUsageRaw` is never invoked.
    pub(crate) fn two_disk_one_missing() -> Self {
        Self {
            pre_show: TWO_DISK_PRE_SHOW,
            post_show: TWO_DISK_POST_SHOW,
            usage_raw: usage_raw_two_disk_one_missing(),
            still_degraded_after: false,
        }
    }

    /// Keep the pre-remove SHOW body for post-remove probes too. Used
    /// by tests that model "still degraded after remove" (e.g. a
    /// 4-disk pool with 2 missing, removing only one) where the
    /// post-remove probe must still report at least one missing devid.
    pub(crate) fn still_degraded_after(mut self, b: bool) -> Self {
        self.still_degraded_after = b;
        self
    }

    /// Register the broad pool-topology handler on `runner`. Returns
    /// the runner plus the shared `remove_done` flag so per-test
    /// handlers that shadow `BtrfsDeviceRemove` can flip the flag
    /// themselves (otherwise post-remove SHOW probes will keep
    /// reporting the pre-remove body).
    pub(crate) fn install(self, runner: MockRunner) -> (MockRunner, Arc<AtomicBool>) {
        let remove_done = Arc::new(AtomicBool::new(false));
        let pre_show = self.pre_show;
        let post_show = self.post_show;
        let usage_raw = self.usage_raw;
        let still_degraded_after = self.still_degraded_after;
        let remove_done_handler = Arc::clone(&remove_done);

        let runner = runner.with_handler(move |req| match req {
            CmdRequest::BtrfsFilesystemShow { mount_point } => {
                let body = if remove_done_handler.load(Ordering::Relaxed)
                    && !still_degraded_after
                {
                    post_show
                } else {
                    pre_show
                };
                Some(Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    body,
                )))
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
            CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(mock_ok(
                "btrfs device usage --raw /mnt/storage",
                &usage_raw,
            ))),
            CmdRequest::BtrfsDeviceRemove { .. } => {
                remove_done_handler.store(true, Ordering::SeqCst);
                Some(Ok(mock_ok("btrfs device remove", "")))
            }
            CmdRequest::BtrfsBalanceRaid1Soft { .. } => Some(Ok(mock_ok(
                "btrfs balance start -dconvert=raid1,soft",
                "",
            ))),
            _ => None,
        });
        (runner, remove_done)
    }
}

impl PoolFixture {
    /// pool.json: disk1 (devid=1) + disk2 (devid=2) + disk3 (devid=3).
    /// All devids pinned because `--missing-id N` resolves through the
    /// membership map via `by_devid`; without pinning, the lookup
    /// cannot match the requested id to a pool.json entry. UUID seeds
    /// mirror disk numbers for readability.
    pub(crate) fn three_disk_devids_pinned() -> Self {
        let base = Self::empty_inner();
        let mut m = PoolMembership::empty();
        for (seed, name, devid) in [(1u64, "disk1", 1u64), (2, "disk2", 2), (3, "disk3", 3)] {
            let (uuid, member) = disk_member_with(
                seed,
                name,
                &format!("/dev/disk/by-id/virtio-{name}"),
                Some(devid),
                None,
            );
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

    /// pool.json: disk1 (devid=1) + disk2 (devid=2). Both devids pinned
    /// so validation-precedence tests can pass `--missing-id 1` (live
    /// device branch reachable in principle) without `by_devid` losing
    /// the disk1 row.
    pub(crate) fn two_disk_devids_pinned() -> Self {
        let base = Self::empty_inner();
        let mut m = PoolMembership::empty();
        for (seed, name, devid) in [(1u64, "disk1", 1u64), (2, "disk2", 2)] {
            let (uuid, member) = disk_member_with(
                seed,
                name,
                &format!("/dev/disk/by-id/virtio-{name}"),
                Some(devid),
                None,
            );
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

    /// Start a `RemoveMissingParamsBuilder` whose defaults match the
    /// most common 3-disk-1-missing test shape: missing_id=3, yes=true,
    /// dry_run=false, progress=Off, sleeper=NoopSleeper.
    pub(crate) fn remove_missing_params(&self) -> RemoveMissingParamsBuilder<'_> {
        RemoveMissingParamsBuilder {
            config: &self.config,
            missing_id: 3,
            dry_run: false,
            yes: true,
            progress: ProgressOutput::Off,
            sleeper: &progress::NoopSleeper,
            paths: &self.paths,
            inhibitor: &self.inhibitor,
            confirm: &self.confirm,
        }
    }
}

/// Per-test `RemoveMissingParams` builder over the remove-missing
/// command defaults. The fixture owns the temp config/state paths and
/// inhibitor; tests only override the command intent (`missing_id`,
/// dry-run, yes, progress, sleeper).
pub(crate) struct RemoveMissingParamsBuilder<'a> {
    config: &'a Config,
    missing_id: u64,
    dry_run: bool,
    yes: bool,
    progress: ProgressOutput,
    sleeper: &'a dyn progress::Sleeper,
    paths: &'a StatePaths,
    inhibitor: &'a RecordingInhibitor,
    confirm: &'a RecordingConfirm,
}

impl<'a> RemoveMissingParamsBuilder<'a> {
    pub(crate) fn missing_id(mut self, id: u64) -> Self {
        self.missing_id = id;
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

    pub(crate) fn progress(mut self, p: ProgressOutput) -> Self {
        self.progress = p;
        self
    }

    pub(crate) fn sleeper(mut self, s: &'a dyn progress::Sleeper) -> Self {
        self.sleeper = s;
        self
    }

    pub(crate) fn build(self) -> RemoveMissingParams<'a> {
        RemoveMissingParams {
            config: self.config,
            missing_id: self.missing_id,
            dry_run: self.dry_run,
            yes: self.yes,
            progress: self.progress,
            paths: self.paths,
            sleep_inhibitor: self.inhibitor,
            confirm: self.confirm,
            sleeper: self.sleeper,
        }
    }
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
