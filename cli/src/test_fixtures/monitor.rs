//! Monitor-scope fixtures for `cli/src/monitor.rs`'s `mod tests`.
//!
//! Monitor tests pin strict fail-closed probe behavior and state-file side
//! effects, so this module promotes the existing narrow runners and
//! mountinfo-only filesystem helpers without adding a broad topology handler.

use super::shared::{DeviceUsageSpec, device_usage_raw_body, mock_ok};
use crate::alert::AlertCause;
use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
use crate::monitor::MonitorResult;
use crate::probe::Filesystem;
use crate::types::MountPoint;
use std::sync::Mutex;

const MOUNTINFO_BTRFS: &str =
    "36 35 0:32 / /mnt/storage rw,noatime shared:1 - btrfs /dev/mapper/braid-vdb rw\n";
const MOUNTINFO_EXT4: &str = "36 35 0:32 / /mnt/storage rw,noatime shared:1 - ext4 /dev/sda1 rw\n";

/// Canonical FS UUID for the monitor 2-disk show, also the `pool_key.fs_uuid`
/// the keying-baseline tests seed and compare against.
pub(crate) const MONITOR_FS_UUID: &str = "de2b8517-f972-45fc-b121-3e160c8ea432";

/// Device size (bytes) for the monitor usage fixtures' large-pool model, chosen
/// so the ENOSPC threshold caps at 1 GiB and the at-risk margin has room to vary
/// across the worsen step. Also the `device_size` in the seeded `PoolKey`.
pub(crate) const USAGE_DEVICE_SIZE: u64 = 100 * (1 << 30); // 100 GiB

const BTRFS_SHOW_2DISK: &str = "Label: none  uuid: de2b8517-f972-45fc-b121-3e160c8ea432\n\
    \tTotal devices 2 FS bytes used 16.17MiB\n\
    \tdevid    1 size 1008.00MiB used 209.50MiB path /dev/mapper/braid-vdb\n\
    \tdevid    2 size 1008.00MiB used 209.50MiB path /dev/mapper/braid-vdc\n";

/// Same two-disk pool as `BTRFS_SHOW_2DISK` but with no `uuid:` line, so
/// `probe_pool_alerts` yields `fs_uuid: None` and `live_pool_key` returns `None`.
/// Drives the identity-gap monitor test (live key cannot be built, but the pool
/// is at risk).
pub(crate) const BTRFS_SHOW_2DISK_NO_UUID: &str = "Label: none\n\
    \tTotal devices 2 FS bytes used 16.17MiB\n\
    \tdevid    1 size 1008.00MiB used 209.50MiB path /dev/mapper/braid-vdb\n\
    \tdevid    2 size 1008.00MiB used 209.50MiB path /dev/mapper/braid-vdc\n";

/// Two present members plus a btrfs-MISSING devid: a degraded pool
/// (`missing_count > 0`) so the monitor must skip ENOSPC evaluation entirely.
pub(crate) const BTRFS_SHOW_2DISK_1MISSING: &str = "Label: none  uuid: de2b8517-f972-45fc-b121-3e160c8ea432\n\
    \tTotal devices 3 FS bytes used 16.17MiB\n\
    \tdevid    1 size 1008.00MiB used 209.50MiB path /dev/mapper/braid-vdb\n\
    \tdevid    2 size 1008.00MiB used 209.50MiB path /dev/mapper/braid-vdc\n\
    \tdevid    3 size 0 used 0 path MISSING\n";

const CRYPTSETUP_STATUS_VDB: &str = "/dev/mapper/braid-vdb is active and is in use.\n\
      type:    LUKS2\n\
      cipher:  aes-xts-plain64\n\
      keysize: 512 [bits]\n\
      key location: keyring\n\
      device:  /dev/vdb\n\
      sector size:  512 [bytes]\n\
      offset:  32768 [512-byte units] (16777216 [bytes])\n\
      size:    2064384 [512-byte units] (1056964608 [bytes])\n\
      mode:    read/write\n";

const CRYPTSETUP_STATUS_VDC: &str = "/dev/mapper/braid-vdc is active and is in use.\n\
      type:    LUKS2\n\
      cipher:  aes-xts-plain64\n\
      keysize: 512 [bits]\n\
      key location: keyring\n\
      device:  /dev/vdc\n\
      sector size:  512 [bytes]\n\
      offset:  32768 [512-byte units] (16777216 [bytes])\n\
      size:    2064384 [512-byte units] (1056964608 [bytes])\n\
      mode:    read/write\n";

const STATS_2DISK_HEALTHY: &str = r#"{
    "__header": {"version": "1"},
    "device-stats": [
        {"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
        {"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
    ]
}"#;

const STATS_WITH_STALE_MAPPER: &str = r#"{
    "__header": {"version": "1"},
    "device-stats": [
        {"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
        {"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
        {"device": "/dev/mapper/braid-stale", "devid": 99, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
    ]
}"#;

const STATS_WITH_STALE_MAPPER_ERRORS: &str = r#"{
    "__header": {"version": "1"},
    "device-stats": [
        {"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
        {"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
        {"device": "/dev/mapper/braid-stale", "devid": 99, "write_io_errs": 0, "read_io_errs": 3, "flush_io_errs": 0, "corruption_errs": 1, "generation_errs": 0}
    ]
}"#;

const BTRFS_SHOW_PRESENT_NULL_MISSING: &str = "Label: none  uuid: de2b8517-f972-45fc-b121-3e160c8ea432\n\
    \tTotal devices 3 FS bytes used 16.17MiB\n\
    \tdevid    1 size 1008.00MiB used 209.50MiB path /dev/mapper/braid-vdb\n\
    \tdevid    2 size 1008.00MiB used 209.50MiB path /dev/mapper/braid-vdc\n\
    \tdevid    3 size 0 used 0 path MISSING\n";

const CRYPTSETUP_STATUS_VDC_NULL: &str = "/dev/mapper/braid-vdc is active and is in use.\n\
      type:    LUKS2\n\
      cipher:  aes-xts-plain64\n\
      keysize: 512 [bits]\n\
      key location: keyring\n\
      device:  (null)\n\
      sector size:  512 [bytes]\n\
      offset:  32768 [512-byte units] (16777216 [bytes])\n\
      size:    2064384 [512-byte units] (1056964608 [bytes])\n\
      mode:    read/write\n";

fn ok_output(stdout: &str) -> RawCommandOutput {
    mock_ok("test", stdout)
}

/// Build a two-device `btrfs device usage --raw` body for the monitor fixtures,
/// varying `device_size` and each device's `unallocated` independently so the
/// keying tests can model a resize (changed `device_size`, same devids) and the
/// risk tests can model a fill (changed `unallocated`, same `device_size`).
pub(crate) fn usage_2disk(device_size: u64, unalloc1: u64, unalloc2: u64) -> String {
    device_usage_raw_body(&[
        DeviceUsageSpec::live(
            "/dev/mapper/braid-vdb",
            1,
            device_size,
            &[("Data", "RAID1", device_size.saturating_sub(unalloc1))],
            unalloc1,
        ),
        DeviceUsageSpec::live(
            "/dev/mapper/braid-vdc",
            2,
            device_size,
            &[("Data", "RAID1", device_size.saturating_sub(unalloc2))],
            unalloc2,
        ),
    ])
}

/// Roomy two-disk usage: both devices have 50 GiB unallocated, so the predicate
/// margin is a large positive surplus (well past the re-arm gate). The default
/// usage for every monitor fixture -- existing tests stay healthy.
pub(crate) fn usage_2disk_healthy() -> String {
    usage_2disk(USAGE_DEVICE_SIZE, 50 * (1 << 30), 50 * (1 << 30))
}

/// Four-disk, one-low *predicate-healthy* usage: three roomy devices plus one at
/// 50 MiB. The pool survives any single loss (large positive margin) even though
/// one device is far below the raw threshold -- the F2 re-arm guard. The monitor
/// 2-disk show only supplies fs_uuid + missing_count(0) here; the helper consumes
/// the four usage devices.
pub(crate) fn usage_4disk_one_low() -> String {
    device_usage_raw_body(&[
        DeviceUsageSpec::live(
            "/dev/mapper/braid-vdb",
            1,
            USAGE_DEVICE_SIZE,
            &[("Data", "RAID1", 0)],
            10 * (1 << 30),
        ),
        DeviceUsageSpec::live(
            "/dev/mapper/braid-vdc",
            2,
            USAGE_DEVICE_SIZE,
            &[("Data", "RAID1", 0)],
            10 * (1 << 30),
        ),
        DeviceUsageSpec::live(
            "/dev/mapper/braid-vdd",
            3,
            USAGE_DEVICE_SIZE,
            &[("Data", "RAID1", 0)],
            10 * (1 << 30),
        ),
        DeviceUsageSpec::live(
            "/dev/mapper/braid-vde",
            4,
            USAGE_DEVICE_SIZE,
            &[("Data", "RAID1", 0)],
            50 * (1 << 20),
        ),
    ])
}

/// One-shot response override for the healthy monitor runner.
pub(crate) enum MonitorOverride {
    BtrfsShowResult(Result<RawCommandOutput, CmdError>),
    BtrfsShowPayload(String),
    StatsResult(Result<RawCommandOutput, CmdError>),
    UsageResult(Result<RawCommandOutput, CmdError>),
}

/// Strict healthy two-disk monitor runner with one optional failure injection.
///
/// `stats_payload` and `usage_payload` are always-present fields (default
/// healthy), so a test can customize both without contending for the single
/// `override_op` slot. The override slot carries one-shot `Result`/payload
/// injections (a show, stats, or usage failure) -- which is why the
/// "at-risk usage + custom show" tests can set usage via the field and the show
/// via the slot.
pub(crate) struct MonitorTestRunner {
    stats_payload: String,
    usage_payload: String,
    override_op: Mutex<Option<MonitorOverride>>,
}

impl MonitorTestRunner {
    /// Healthy runner variant whose stats include a benign stale zero row.
    pub(crate) fn with_stale_mapper_stats() -> Self {
        Self {
            stats_payload: STATS_WITH_STALE_MAPPER.to_owned(),
            usage_payload: usage_2disk_healthy(),
            override_op: Mutex::new(None),
        }
    }

    /// Healthy runner variant whose stats include a stale non-zero row.
    pub(crate) fn with_stale_mapper_errors() -> Self {
        Self {
            stats_payload: STATS_WITH_STALE_MAPPER_ERRORS.to_owned(),
            usage_payload: usage_2disk_healthy(),
            override_op: Mutex::new(None),
        }
    }

    /// Healthy runner plus a one-shot override for a single request family.
    pub(crate) fn with_override(override_op: MonitorOverride) -> Self {
        Self {
            stats_payload: STATS_2DISK_HEALTHY.to_owned(),
            usage_payload: usage_2disk_healthy(),
            override_op: Mutex::new(Some(override_op)),
        }
    }

    /// Build a runner with a caller-supplied `btrfs device stats` payload so a
    /// test can place non-zero counters on a *recognized* devid (the existing
    /// stale/healthy constants only zero recognized devids).
    pub(crate) fn with_stats_payload(payload: impl Into<String>) -> Self {
        Self {
            stats_payload: payload.into(),
            usage_payload: usage_2disk_healthy(),
            override_op: Mutex::new(None),
        }
    }

    /// Build a runner with a caller-supplied `btrfs device usage --raw` payload
    /// (healthy stats), so ENOSPC tests can drive an at-risk or re-arm-eligible
    /// pool through the usage probe. The payload is served from the
    /// `usage_payload` field, leaving the override slot free.
    pub(crate) fn with_usage_payload(usage: impl Into<String>) -> Self {
        Self {
            stats_payload: STATS_2DISK_HEALTHY.to_owned(),
            usage_payload: usage.into(),
            override_op: Mutex::new(None),
        }
    }

    /// Build a runner with a custom usage payload *and* a one-shot override
    /// (typically a `BtrfsShowPayload` to control fs_uuid/membership), so the
    /// identity-gap test can run an at-risk pool whose show carries no FS UUID.
    pub(crate) fn with_usage_and_override(
        usage: impl Into<String>,
        override_op: MonitorOverride,
    ) -> Self {
        Self {
            stats_payload: STATS_2DISK_HEALTHY.to_owned(),
            usage_payload: usage.into(),
            override_op: Mutex::new(Some(override_op)),
        }
    }

    /// Build a runner with a caller-supplied stats payload *and* a one-shot
    /// override (e.g. `UsageResult(Err)`), expressing "device-error stats AND a
    /// usage-probe failure at once" -- the probe-failure-isolation precondition.
    pub(crate) fn with_stats_payload_and_usage(
        stats: impl Into<String>,
        override_op: MonitorOverride,
    ) -> Self {
        Self {
            stats_payload: stats.into(),
            usage_payload: usage_2disk_healthy(),
            override_op: Mutex::new(Some(override_op)),
        }
    }

    fn take_btrfs_show_payload(&self) -> Option<String> {
        let mut guard = self.override_op.lock().unwrap();
        if matches!(guard.as_ref(), Some(MonitorOverride::BtrfsShowPayload(_)))
            && let Some(MonitorOverride::BtrfsShowPayload(s)) = guard.take()
        {
            return Some(s);
        }
        None
    }

    fn take_btrfs_show_result(&self) -> Option<Result<RawCommandOutput, CmdError>> {
        let mut guard = self.override_op.lock().unwrap();
        if matches!(guard.as_ref(), Some(MonitorOverride::BtrfsShowResult(_)))
            && let Some(MonitorOverride::BtrfsShowResult(r)) = guard.take()
        {
            return Some(r);
        }
        None
    }

    fn take_stats_result(&self) -> Option<Result<RawCommandOutput, CmdError>> {
        let mut guard = self.override_op.lock().unwrap();
        if matches!(guard.as_ref(), Some(MonitorOverride::StatsResult(_)))
            && let Some(MonitorOverride::StatsResult(r)) = guard.take()
        {
            return Some(r);
        }
        None
    }

    fn take_usage_result(&self) -> Option<Result<RawCommandOutput, CmdError>> {
        let mut guard = self.override_op.lock().unwrap();
        if matches!(guard.as_ref(), Some(MonitorOverride::UsageResult(_)))
            && let Some(MonitorOverride::UsageResult(r)) = guard.take()
        {
            return Some(r);
        }
        None
    }
}

impl CommandRunner for MonitorTestRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        match request {
            CmdRequest::BtrfsFilesystemShow { .. } => {
                if let Some(r) = self.take_btrfs_show_result() {
                    return r;
                }
                if let Some(payload) = self.take_btrfs_show_payload() {
                    return Ok(ok_output(&payload));
                }
                Ok(ok_output(BTRFS_SHOW_2DISK))
            }
            CmdRequest::CryptsetupStatus { mapper } => match mapper.as_str() {
                "braid-vdb" => Ok(ok_output(CRYPTSETUP_STATUS_VDB)),
                "braid-vdc" => Ok(ok_output(CRYPTSETUP_STATUS_VDC)),
                other => panic!("unexpected CryptsetupStatus mapper: {other}"),
            },
            CmdRequest::BtrfsDeviceStatsJson { .. } => {
                if let Some(r) = self.take_stats_result() {
                    return r;
                }
                Ok(ok_output(&self.stats_payload))
            }
            // ENOSPC probe: a one-shot UsageResult override wins (failure
            // injection); otherwise serve the usage_payload field (default
            // healthy, so existing monitor tests stay green).
            CmdRequest::BtrfsDeviceUsageRaw { .. } => {
                if let Some(r) = self.take_usage_result() {
                    return r;
                }
                Ok(ok_output(&self.usage_payload))
            }
            other => panic!("unexpected CmdRequest in monitor test: {other:?}"),
        }
    }

    fn run_with_stdin(
        &self,
        _request: &CmdRequest,
        _stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        Err(CmdError::MissingMock)
    }
}

/// Strict present/null-underlying/MISSING topology for acked-stats reconciliation.
pub(crate) struct MonitorReconcileRunner;

impl CommandRunner for MonitorReconcileRunner {
    fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
        match request {
            CmdRequest::BtrfsFilesystemShow { .. } => {
                Ok(ok_output(BTRFS_SHOW_PRESENT_NULL_MISSING))
            }
            CmdRequest::CryptsetupStatus { mapper } => match mapper.as_str() {
                "braid-vdb" => Ok(ok_output(CRYPTSETUP_STATUS_VDB)),
                "braid-vdc" => Ok(ok_output(CRYPTSETUP_STATUS_VDC_NULL)),
                other => panic!("unexpected CryptsetupStatus mapper: {other}"),
            },
            CmdRequest::BtrfsDeviceStatsJson { .. } => Ok(ok_output(STATS_2DISK_HEALTHY)),
            // Healthy usage payload. This runner's topology is degraded (devid 3
            // MISSING), so the monitor skips ENOSPC before probing usage and this
            // arm is unreached -- present defensively so a future non-degraded
            // reconcile fixture cannot panic here.
            CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(ok_output(&usage_2disk_healthy())),
            other => panic!("unexpected CmdRequest in monitor reconcile test: {other:?}"),
        }
    }

    fn run_with_stdin(
        &self,
        request: &CmdRequest,
        _stdin: &[u8],
    ) -> Result<RawCommandOutput, CmdError> {
        self.run(request)
    }
}

struct MonitorFs {
    mountinfo: Result<&'static str, std::io::ErrorKind>,
}

impl Filesystem for MonitorFs {
    fn exists(&self, path: &str) -> bool {
        panic!("unexpected monitor fs exists probe: {path}");
    }

    fn is_block_device(&self, path: &str) -> bool {
        panic!("unexpected monitor fs block-device probe: {path}");
    }

    fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
        assert_eq!(path, "/proc/self/mountinfo");
        self.mountinfo
            .map(str::to_owned)
            .map_err(|kind| std::io::Error::new(kind, "mock mountinfo error"))
    }

    fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
        panic!("unexpected monitor fs list_dir probe: {path}");
    }

    fn create_dir_all(&self, path: &str) -> Result<(), std::io::Error> {
        panic!("unexpected monitor fs create_dir_all probe: {path}");
    }
}

/// Canonical monitor-test mount point shared by the promoted runners.
pub(crate) fn monitor_mp() -> MountPoint {
    MountPoint::new("/mnt/storage".to_owned())
}

/// Mounted btrfs filesystem surface that only allows the mountinfo read.
pub(crate) fn monitor_fs_btrfs() -> impl Filesystem {
    MonitorFs {
        mountinfo: Ok(MOUNTINFO_BTRFS),
    }
}

/// Mountinfo with no entry for the configured target -- monitor's legitimate-offline branch.
pub(crate) fn monitor_fs_not_mounted() -> impl Filesystem {
    MonitorFs { mountinfo: Ok("") }
}

/// Mounted non-btrfs filesystem surface for the NotBtrfs monitor branch.
pub(crate) fn monitor_fs_ext4() -> impl Filesystem {
    MonitorFs {
        mountinfo: Ok(MOUNTINFO_EXT4),
    }
}

/// Mountinfo read failure surface for monitor's fail-closed IO branch.
pub(crate) fn monitor_fs_mountinfo_error(kind: std::io::ErrorKind) -> impl Filesystem {
    MonitorFs {
        mountinfo: Err(kind),
    }
}

/// Assert monitor returned one active ComputationError and expose its detail.
pub(crate) fn assert_monitor_single_computation_error(result: &MonitorResult) -> &str {
    match result {
        MonitorResult::Alert(state) => {
            assert!(state.active(), "AlertState must be active");
            assert_eq!(
                state.causes.len(),
                1,
                "expected exactly one cause, got {:?}",
                state.causes
            );
            match &state.causes[0] {
                AlertCause::ComputationError { detail } => detail.as_str(),
                other => panic!("expected ComputationError, got {other:?}"),
            }
        }
        other => panic!("expected MonitorResult::Alert, got {other:?}"),
    }
}
