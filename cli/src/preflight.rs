use std::fmt;

use crate::cmd::{CmdRequest, CommandRunner};
use crate::journal;
use crate::parse::types::BtrfsDeviceUsageEntry;
use crate::parse::{parse_btrfs_device_usage, parse_findmnt_json};
use crate::probe::Filesystem;
use crate::state_paths::StatePaths;
use crate::status::format_bytes;
use crate::types::MountPoint;

/// Refuse if a pending-operation journal exists.
/// When the journal is present, pool.json may be inconsistent — only
/// `status`, `recover`, and `lock` are safe to run.
pub fn check_no_pending_operation(paths: &StatePaths) -> Result<(), String> {
    match journal::load_journal(paths) {
        Ok(Some(j)) => Err(format!(
            "interrupted operation detected (pending-op.json exists, started {}).\n\
             Pool membership may be inconsistent. Run 'braid recover' to reconcile \
             from live pool state, or 'braid status' to inspect.",
            j.started_at
        )),
        Ok(None) => Ok(()),
        Err(e) => Err(format!(
            "cannot read pending-op.json: {e}. Remove it manually or run 'braid recover'."
        )),
    }
}

// ---------------------------------------------------------------------------
// Exclusive operation check (sysfs-based)
// ---------------------------------------------------------------------------

/// Kernel exclusive operation state, read from
/// `/sys/fs/btrfs/{fsid}/exclusive_operation`.
///
/// String values follow `exclop_def[]` in btrfs-progs
/// `common/utils.c:1186-1194` (vendored in `reference/btrfs-progs/`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum ExclusiveOp {
    None,
    Balance,
    BalancePaused,
    DeviceAdd,
    /// The kernel writes "device remove" — not "device delete" as
    /// btrfs-man5.rst sometimes says.  Follows `exclop_def[]` in
    /// `reference/btrfs-progs/common/utils.c:1191`.
    DeviceRemove,
    DeviceReplace,
    Resize,
    SwapActivate,
}

impl ExclusiveOp {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "none" => Some(Self::None),
            "balance" => Some(Self::Balance),
            "balance paused" => Some(Self::BalancePaused),
            "device add" => Some(Self::DeviceAdd),
            "device remove" => Some(Self::DeviceRemove),
            "device replace" => Some(Self::DeviceReplace),
            "resize" => Some(Self::Resize),
            "swap activate" => Some(Self::SwapActivate),
            _ => Option::None,
        }
    }
}

impl fmt::Display for ExclusiveOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::None => write!(f, "none"),
            Self::Balance => write!(f, "balance"),
            Self::BalancePaused => write!(f, "balance (paused)"),
            Self::DeviceAdd => write!(f, "device add"),
            Self::DeviceRemove => write!(f, "device remove"),
            Self::DeviceReplace => write!(f, "device replace"),
            Self::Resize => write!(f, "resize"),
            Self::SwapActivate => write!(f, "swap activate"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
enum ExclusiveOpError {
    #[error("an exclusive operation is already running: {0}")]
    Busy(ExclusiveOp),
    #[error("cannot read exclusive operation status: {0}")]
    Read(std::io::Error),
    #[error("unrecognized exclusive operation: {0:?}")]
    Unrecognized(String),
}

/// How to handle `/sys/fs/btrfs/<fsid>/exclusive_operation` when it is not
/// `none`.
enum ExclusiveOpPolicy {
    /// `braid lock` behavior:
    /// hard-fail on any active exclusive op.
    ///
    /// Why: lock is teardown (unmount + close). It must not proceed while btrfs
    /// is mid balance/device-add/device-remove/device-replace/resize.
    RejectAnyBusy,

    /// Mutating command behavior (`add`, `remove`, `remove-missing`, `replace`):
    /// - `balance paused` => hard error (operator must resume/cancel)
    /// - any other busy state => warn and proceed
    ///
    /// Why: these commands invoke btrfs with `--enqueue`, so kernel serialization
    /// is the correctness mechanism and avoids TOCTOU-style preflight busy failures.
    /// Paused balance is the exception because it can block indefinitely.
    RejectPausedBalanceElseEnqueue,
}

/// Apply `policy` to the current exclusive-op state read from sysfs.
///
/// Returns `Err(String)` on rejection, `Ok(())` when the caller may proceed.
fn check_exclusive_op_with_policy<F: Filesystem + ?Sized>(
    fs: &F,
    fsid: &str,
    policy: ExclusiveOpPolicy,
) -> Result<(), String> {
    match check_no_exclusive_op(fs, fsid) {
        Ok(()) => Ok(()),
        Err(ExclusiveOpError::Busy(op)) => match policy {
            ExclusiveOpPolicy::RejectAnyBusy => {
                Err(format!("cannot lock: {op} is in progress. Wait for it to finish first."))
            }
            ExclusiveOpPolicy::RejectPausedBalanceElseEnqueue => match op {
                ExclusiveOp::BalancePaused => {
                    Err("a btrfs balance is paused. Resume or cancel it before proceeding.".into())
                }
                _ => {
                    eprintln!("  waiting for in-flight {op} to finish...");
                    Ok(())
                }
            },
        },
        Err(e) => Err(e.to_string()),
    }
}

/// Check `/sys/fs/btrfs/{fsid}/exclusive_operation` for an active exclusive op.
///
/// Returns `Ok(())` if the sysfs file reads `"none"`.
/// Returns `Err(Busy(op))` for any other recognized state.
/// Fail-closed: unrecognized values and read errors are errors.
fn check_no_exclusive_op<F: Filesystem + ?Sized>(
    fs: &F,
    fsid: &str,
) -> Result<(), ExclusiveOpError> {
    let path = format!("/sys/fs/btrfs/{fsid}/exclusive_operation");
    let contents = fs.read_to_string(&path).map_err(ExclusiveOpError::Read)?;
    let op = ExclusiveOp::parse(contents.trim())
        .ok_or_else(|| ExclusiveOpError::Unrecognized(contents.trim().to_owned()))?;
    match op {
        ExclusiveOp::None => Ok(()),
        _ => Err(ExclusiveOpError::Busy(op)),
    }
}

/// Refuse if the pool is mounted read-only.
/// Runs its own findmnt probe — avoids adding mount_options to PoolState
/// and touching all 7+ PoolState construction sites.
fn check_not_read_only<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<(), String> {
    let raw = match runner.run(&CmdRequest::FindmntJson {
        mount_point: mount_point.clone(),
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("warning: read-only pre-flight failed: {e}; proceeding anyway");
            return Ok(());
        }
    };

    let findmnt = match parse_findmnt_json(&raw) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("warning: read-only pre-flight failed: {e}; proceeding anyway");
            return Ok(());
        }
    };

    let entry = findmnt
        .filesystems
        .iter()
        .find(|e| e.target == mount_point.as_str());
    if let Some(entry) = entry
        && entry.options.split(',').any(|opt| opt.trim() == "ro") {
            return Err(format!(
                "pool is mounted read-only. Remount read-write first:\n  \
                 mount -o remount,rw {mount_point}"
            ));
        }
    Ok(())
}

/// Refuse if the pool has missing devices.
pub fn check_no_missing_devices(missing_count: u64, action: &str) -> Result<(), String> {
    if missing_count > 0 {
        Err(format!(
            "pool has {missing_count} missing device{}. \
             Resolve the missing device{} first -- repair with \
             `braid replace --missing-id <devid>`, or forget the entry with \
             `braid remove-missing` -- then {action}. \
             Use `braid status` to see device IDs.",
            if missing_count == 1 { "" } else { "s" },
            if missing_count == 1 { "" } else { "s" },
        ))
    } else {
        Ok(())
    }
}

/// Return the set of devids that are missing (device_size == 0 in btrfs
/// device usage output). Used to validate --missing-id arguments.
pub fn probe_missing_devids<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<Vec<u64>, String> {
    let raw = runner
        .run(&CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| format!("failed to probe device usage: {e}"))?;

    let usage =
        parse_btrfs_device_usage(&raw).map_err(|e| format!("failed to parse device usage: {e}"))?;

    Ok(usage
        .devices
        .iter()
        .filter(|d| d.device_size == 0)
        .map(|d| d.devid)
        .collect())
}

/// Check that remaining devices have enough RAID1-aware space to absorb the
/// allocations from the target device(s) being removed or relocated.
///
/// Checks per allocation type (Data, Metadata, System) independently, because
/// the kernel allocates chunks per type and cannot use Data space for Metadata.
///
/// For RAID1, two constraints must hold:
///   1. At least 2 remaining devices must have unallocated space (RAID1 requires
///      two devices with capacity to write a new chunk).
///   2. Effective RAID1 capacity = min(largest, rest) where largest is the
///      biggest device's unallocated space and rest is the sum of all others.
///      Each RAID1 chunk needs space on 2 devices, so a device with more
///      free space than all others combined is bottlenecked by what those
///      others can provide.
pub fn check_raid1_relocation_space(
    target_devs: &[&BtrfsDeviceUsageEntry],
    remaining_devs: &[&BtrfsDeviceUsageEntry],
) -> Result<(), String> {
    for alloc_type in &["Data", "Metadata", "System"] {
        let bytes_on_target: u64 = target_devs
            .iter()
            .map(|d| d.allocated_by_type(alloc_type))
            .sum();

        if bytes_on_target == 0 {
            continue;
        }

        let mut remaining_unalloc: Vec<u64> =
            remaining_devs.iter().map(|d| d.unallocated).collect();
        remaining_unalloc.sort_unstable_by(|a, b| b.cmp(a));

        let devices_with_space = remaining_unalloc.iter().filter(|&&s| s > 0).count();
        if devices_with_space < 2 {
            return Err(format!(
                "cannot relocate {} chunks: fewer than 2 remaining devices \
                 have unallocated space (need space on 2 devices for RAID1)",
                alloc_type
            ));
        }

        let total: u64 = remaining_unalloc.iter().sum();
        let largest = remaining_unalloc[0];
        let rest: u64 = remaining_unalloc[1..].iter().sum();

        let raid1_capacity = if largest > rest { rest } else { total / 2 };

        if raid1_capacity < bytes_on_target {
            return Err(format!(
                "not enough space to relocate {} chunks.\n\n  \
                 {} allocated on device(s) being removed: {}\n  \
                 RAID1 capacity on remaining devices: {}\n\n\
                 Each RAID1 chunk requires space on 2 devices simultaneously.",
                alloc_type,
                alloc_type,
                format_bytes(bytes_on_target),
                format_bytes(raid1_capacity),
            ));
        }
    }
    Ok(())
}

/// Guard for mutating pool commands (add, remove, remove-missing, replace).
///
/// Checks three preconditions before the caller touches the pool:
///   1. No paused balance is blocking the exclusive-op lock (hard error).
///   2. No other in-flight exclusive op — prints a wait message and proceeds
///      on the assumption the kernel will serialize access.
///   3. The filesystem is not mounted read-only.
///
/// Returns `Err(String)` suitable for wrapping in a command's
/// `Validation` error variant.
pub fn require_mutation_preflight<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    fsid: &str,
    mount_point: &MountPoint,
) -> Result<(), String> {
    check_exclusive_op_with_policy(fs, fsid, ExclusiveOpPolicy::RejectPausedBalanceElseEnqueue)?;
    check_not_read_only(runner, mount_point)
}

/// Guard for `braid lock` (teardown: unmount + close LUKS).
///
/// Hard-fails on any active exclusive op. Lock must not proceed while btrfs
/// is mid balance/device-add/device-remove/device-replace/resize.
///
/// Returns `Err(String)` suitable for wrapping in `LockError::Failed`.
pub fn require_lock_preflight<F: Filesystem + ?Sized>(
    fs: &F,
    fsid: &str,
) -> Result<(), String> {
    check_exclusive_op_with_policy(fs, fsid, ExclusiveOpPolicy::RejectAnyBusy)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::probe::Filesystem;

    struct MockFs {
        files: std::collections::HashMap<String, String>,
    }

    impl MockFs {
        fn with_sysfs(fsid: &str, content: &str) -> Self {
            let mut files = std::collections::HashMap::new();
            files.insert(
                format!("/sys/fs/btrfs/{fsid}/exclusive_operation"),
                content.to_owned(),
            );
            Self { files }
        }

        fn empty() -> Self {
            Self {
                files: std::collections::HashMap::new(),
            }
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, _path: &str) -> bool {
            false
        }
        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }
        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    const FSID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    // --- ExclusiveOp::parse tests ---

    #[test]
    // Intent: ExclusiveOp::parse recognizes all sysfs strings from exclop_def[].
    // Why: Ensures the parser covers every value the kernel can write.
    // Scenario: Kernel writes each possible exclusive_operation value.
    fn exclusive_op_parse_all_variants() {
        assert_eq!(ExclusiveOp::parse("none"), Some(ExclusiveOp::None));
        assert_eq!(ExclusiveOp::parse("balance"), Some(ExclusiveOp::Balance));
        assert_eq!(
            ExclusiveOp::parse("balance paused"),
            Some(ExclusiveOp::BalancePaused)
        );
        assert_eq!(
            ExclusiveOp::parse("device add"),
            Some(ExclusiveOp::DeviceAdd)
        );
        assert_eq!(
            ExclusiveOp::parse("device remove"),
            Some(ExclusiveOp::DeviceRemove)
        );
        assert_eq!(
            ExclusiveOp::parse("device replace"),
            Some(ExclusiveOp::DeviceReplace)
        );
        assert_eq!(ExclusiveOp::parse("resize"), Some(ExclusiveOp::Resize));
        assert_eq!(
            ExclusiveOp::parse("swap activate"),
            Some(ExclusiveOp::SwapActivate)
        );
    }

    #[test]
    // Intent: ExclusiveOp::parse returns None for unrecognized values.
    // Why: Future kernel versions may add new op types; fail-closed is safer.
    // Scenario: Kernel writes a value not in exclop_def[].
    fn exclusive_op_parse_unrecognized() {
        assert_eq!(ExclusiveOp::parse("something new"), Option::None);
        assert_eq!(ExclusiveOp::parse(""), Option::None);
    }

    #[test]
    // Intent: ExclusiveOp Display produces human-readable strings.
    // Why: These strings appear in user-facing "waiting for..." messages.
    // Scenario: Each op variant is formatted for display.
    fn exclusive_op_display() {
        assert_eq!(format!("{}", ExclusiveOp::Balance), "balance");
        assert_eq!(
            format!("{}", ExclusiveOp::BalancePaused),
            "balance (paused)"
        );
        assert_eq!(format!("{}", ExclusiveOp::DeviceRemove), "device remove");
    }

    // --- check_no_exclusive_op tests ---

    #[test]
    // Intent: check_no_exclusive_op passes when sysfs reports "none".
    // Why: Confirms the happy path doesn't block valid operations.
    // Scenario: Operator runs a command while the pool is idle.
    fn exclusive_op_passes_when_none() {
        let fs = MockFs::with_sysfs(FSID, "none\n");
        assert!(check_no_exclusive_op(&fs, FSID).is_ok());
    }

    #[test]
    // Intent: check_no_exclusive_op returns Busy when a balance is running.
    // Why: Callers need to distinguish active ops to decide wait-vs-error.
    // Scenario: Operator tries a command while a RAID1 balance is in progress.
    fn exclusive_op_busy_when_balance_running() {
        let fs = MockFs::with_sysfs(FSID, "balance\n");
        match check_no_exclusive_op(&fs, FSID) {
            Err(ExclusiveOpError::Busy(ExclusiveOp::Balance)) => {}
            other => panic!("expected Busy(Balance), got: {other:?}"),
        }
    }

    #[test]
    // Intent: check_no_exclusive_op returns Busy(BalancePaused) for paused balance.
    // Why: A paused balance never clears on its own — callers must hard-error,
    //   not enqueue (which would hang forever).
    // Scenario: Operator paused a balance and forgot, then tries `braid add`.
    fn exclusive_op_busy_when_balance_paused() {
        let fs = MockFs::with_sysfs(FSID, "balance paused\n");
        match check_no_exclusive_op(&fs, FSID) {
            Err(ExclusiveOpError::Busy(ExclusiveOp::BalancePaused)) => {}
            other => panic!("expected Busy(BalancePaused), got: {other:?}"),
        }
    }

    #[test]
    // Intent: check_no_exclusive_op returns Busy for device operations.
    // Why: Covers the non-balance exclusive ops that the old balance-only
    //   check could not detect.
    // Scenario: A device remove is in progress when operator tries `braid add`.
    fn exclusive_op_busy_when_device_remove() {
        let fs = MockFs::with_sysfs(FSID, "device remove\n");
        match check_no_exclusive_op(&fs, FSID) {
            Err(ExclusiveOpError::Busy(ExclusiveOp::DeviceRemove)) => {}
            other => panic!("expected Busy(DeviceRemove), got: {other:?}"),
        }
    }

    #[test]
    // Intent: check_no_exclusive_op errors on unrecognized sysfs value.
    // Why: Fail-closed — unknown state is treated as an error.
    // Scenario: Kernel version introduces a new exclusive op type.
    fn exclusive_op_unrecognized_value() {
        let fs = MockFs::with_sysfs(FSID, "something new\n");
        match check_no_exclusive_op(&fs, FSID) {
            Err(ExclusiveOpError::Unrecognized(s)) => {
                assert_eq!(s, "something new");
            }
            other => panic!("expected Unrecognized, got: {other:?}"),
        }
    }

    #[test]
    // Intent: check_no_exclusive_op errors when sysfs file can't be read.
    // Why: Fail-closed — if we can't determine state, refusing is safer than
    //   proceeding and potentially starting a conflicting exclusive op.
    // Scenario: sysfs not available (container, broken mount).
    fn exclusive_op_read_failure() {
        let fs = MockFs::empty();
        match check_no_exclusive_op(&fs, FSID) {
            Err(ExclusiveOpError::Read(_)) => {}
            other => panic!("expected Read error, got: {other:?}"),
        }
    }

    #[test]
    // Intent: check_not_read_only passes when pool is rw.
    // Why: Confirms rw mounts are not falsely rejected.
    // Scenario: Normal pool mount with default options.
    fn read_only_passes_when_rw() {
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson { mount_point: mp() },
            RawCommandOutput {
                cmd: "findmnt --json --output TARGET,SOURCE,FSTYPE,OPTIONS --mountpoint /mnt/storage".into(),
                stdout: r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-vdb","fstype":"btrfs","options":"rw,relatime,ssd,space_cache=v2,subvolid=5,subvol=/"}]}"#.into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        assert!(check_not_read_only(&runner, &mp()).is_ok());
    }

    #[test]
    // Intent: check_not_read_only refuses when pool is ro.
    // Why: After a crash, btrfs remounts ro; writes fail with cryptic errors.
    // Scenario: Pool crashed, operator tries `braid remove` on the ro mount.
    fn read_only_refuses_when_ro() {
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson { mount_point: mp() },
            RawCommandOutput {
                cmd: "findmnt --json --output TARGET,SOURCE,FSTYPE,OPTIONS --mountpoint /mnt/storage".into(),
                stdout: r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-vdb","fstype":"btrfs","options":"ro,relatime,ssd,space_cache=v2,subvolid=5,subvol=/"}]}"#.into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let err = check_not_read_only(&runner, &mp()).unwrap_err();
        assert!(err.contains("read-only"), "expected 'read-only' in: {err}");
        assert!(
            err.contains("remount"),
            "expected remount guidance in: {err}"
        );
    }

    #[test]
    // Intent: check_not_read_only proceeds when findmnt probe fails.
    // Why: A bug in the safety check shouldn't block valid operations.
    // Scenario: findmnt not found or permissions issue.
    fn read_only_proceeds_on_probe_failure() {
        let runner = MockRunner::default(); // no mock → MissingMock
        assert!(check_not_read_only(&runner, &mp()).is_ok());
    }

    #[test]
    // Intent: check_no_missing_devices passes when no devices are missing.
    // Why: Confirms healthy pools are not rejected.
    // Scenario: Normal 3-disk pool, all present.
    fn missing_devices_passes_when_none() {
        assert!(check_no_missing_devices(0, "remove a disk").is_ok());
    }

    #[test]
    // Intent: check_no_missing_devices refuses when devices are missing.
    // Why: Removing a live disk from a degraded pool is dangerous.
    // Scenario: One disk has died, operator tries to remove a different live disk.
    fn missing_devices_refuses_when_degraded() {
        let err = check_no_missing_devices(2, "remove a disk").unwrap_err();
        assert!(
            err.contains("2 missing devices"),
            "expected count in: {err}"
        );
        assert!(
            err.contains("replace --missing-id"),
            "expected repair guidance in: {err}"
        );
        assert!(
            err.contains("remove-missing"),
            "expected cleanup guidance in: {err}"
        );
    }

    #[test]
    // Intent: check_no_missing_devices uses singular for 1 device.
    // Why: Grammar correctness in user-facing messages.
    // Scenario: Pool has exactly 1 missing device.
    fn missing_devices_singular_grammar() {
        let err = check_no_missing_devices(1, "remove a disk").unwrap_err();
        assert!(
            err.contains("1 missing device."),
            "expected singular in: {err}"
        );
    }

    #[test]
    // Intent: probe_missing_devids returns devids of missing devices.
    // Why: Used to validate --missing-id arguments against actual missing devids.
    // Scenario: 3-disk pool with one missing device (devid 3).
    fn probe_missing_devids_returns_missing() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsDeviceUsageRaw { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs device usage --raw /mnt/storage".into(),
                stdout: "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

/dev/mapper/braid-disk2, ID: 2
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

<missing disk>, ID: 3
   Device size:                  0
   Device slack:                  0
   Data,RAID1:           2147483648
   Unallocated:                  0

"
                .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let missing = probe_missing_devids(&runner, &mp()).unwrap();
        assert_eq!(missing, vec![3]);
    }

    #[test]
    // Intent: probe_missing_devids returns empty when no devices are missing.
    // Why: Confirms healthy pools report no missing devids.
    // Scenario: Normal 2-disk pool, all present.
    fn probe_missing_devids_returns_empty_when_healthy() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsDeviceUsageRaw { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs device usage --raw /mnt/storage".into(),
                stdout: "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

/dev/mapper/braid-disk2, ID: 2
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

"
                .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let missing = probe_missing_devids(&runner, &mp()).unwrap();
        assert!(missing.is_empty());
    }

    // --- check_raid1_relocation_space tests ---

    use crate::parse::types::DeviceAllocation;

    fn make_dev(devid: u64, unallocated: u64, allocs: &[(&str, u64)]) -> BtrfsDeviceUsageEntry {
        BtrfsDeviceUsageEntry {
            path: format!("/dev/mapper/braid-disk{}", devid),
            devid,
            device_size: 1_000_000_000,
            device_slack: 0,
            allocations: allocs
                .iter()
                .map(|(t, b)| DeviceAllocation {
                    alloc_type: t.to_string(),
                    profile: "RAID1".to_string(),
                    bytes: *b,
                })
                .collect(),
            unallocated,
        }
    }

    #[test]
    // Intent: check_raid1_relocation_space passes when 3 remaining devices have
    //   enough space for target's Data and Metadata allocations.
    // Why: Confirms valid operations are not blocked.
    // Scenario: 4-disk pool removing one disk; remaining three each have 200MB
    //   unallocated; target has 100MB Data + 50MB Metadata.
    fn raid1_space_passes_sufficient_space() {
        let target = make_dev(1, 0, &[("Data", 100_000_000), ("Metadata", 50_000_000)]);
        let rem1 = make_dev(2, 200_000_000, &[]);
        let rem2 = make_dev(3, 200_000_000, &[]);
        let rem3 = make_dev(4, 200_000_000, &[]);
        let result = check_raid1_relocation_space(&[&target], &[&rem1, &rem2, &rem3]);
        assert!(result.is_ok(), "should pass: {result:?}");
    }

    #[test]
    // Intent: check_raid1_relocation_space fails when RAID1 chunk-level capacity
    //   is insufficient despite large total unallocated.
    // Why: The naive sum/2 can be misleading when one device dominates —
    //   each RAID1 chunk needs 2 devices, so the dominant device is
    //   bottlenecked by what others can provide.
    // Scenario: 3 remaining devices with [200MB, 10MB, 10MB] unallocated.
    //   RAID1 capacity = rest = 20MB (not 110MB). Target has 500MB Data.
    fn raid1_space_fails_chunk_capacity_constraint() {
        let target = make_dev(1, 0, &[("Data", 500_000_000)]);
        let rem1 = make_dev(2, 200_000_000, &[]);
        let rem2 = make_dev(3, 10_000_000, &[]);
        let rem3 = make_dev(4, 10_000_000, &[]);
        let result = check_raid1_relocation_space(&[&target], &[&rem1, &rem2, &rem3]);
        let err = result.expect_err("should fail: chunk capacity constraint");
        assert!(err.contains("Data"), "expected 'Data' in error: {err}");
    }

    #[test]
    // Intent: check_raid1_relocation_space fails when fewer than 2 remaining
    //   devices have unallocated space.
    // Why: RAID1 requires 2 devices with capacity; 1 device cannot form a RAID1 chunk.
    // Scenario: Target has 100MB Data; remaining has 200MB + 0MB unallocated.
    fn raid1_space_fails_fewer_than_two_devices_with_space() {
        let target = make_dev(1, 0, &[("Data", 100_000_000)]);
        let rem1 = make_dev(2, 200_000_000, &[]);
        let rem2 = make_dev(3, 0, &[]);
        let result = check_raid1_relocation_space(&[&target], &[&rem1, &rem2]);
        let err = result.expect_err("should fail: fewer than 2 devices with space");
        assert!(
            err.contains("fewer than 2"),
            "expected 'fewer than 2' in error: {err}"
        );
    }

    #[test]
    // Intent: check_raid1_relocation_space skips types with zero allocations on target.
    // Why: Types not present on target don't need relocation; checking them would
    //   cause false negatives against an empty remaining device list.
    // Scenario: Target has 0 Data but 40MB Metadata; remaining have 50MB each.
    //   Data is skipped (0 allocated). Metadata RAID1 capacity = 50MB > 40MB → pass.
    fn raid1_space_skips_zero_allocation_type() {
        let target = make_dev(1, 0, &[("Data", 0), ("Metadata", 40_000_000)]);
        let rem1 = make_dev(2, 50_000_000, &[]);
        let rem2 = make_dev(3, 50_000_000, &[]);
        let result = check_raid1_relocation_space(&[&target], &[&rem1, &rem2]);
        assert!(
            result.is_ok(),
            "should pass (Data skipped, Metadata fits): {result:?}"
        );
    }

    #[test]
    // Intent: check_raid1_relocation_space fails on the per-type that is tight,
    //   even when other types have plenty of space.
    // Why: DATA and METADATA are independent allocation pools in the kernel.
    //   Surplus Data space cannot cover Metadata relocation.
    // Scenario: Target has 0 Data but 100MB Metadata; remaining have 40MB each.
    //   Metadata RAID1 capacity = 40MB < 100MB → fail.
    fn raid1_space_fails_tight_metadata_despite_data_ok() {
        let target = make_dev(1, 0, &[("Metadata", 100_000_000)]);
        let rem1 = make_dev(2, 40_000_000, &[]);
        let rem2 = make_dev(3, 40_000_000, &[]);
        let result = check_raid1_relocation_space(&[&target], &[&rem1, &rem2]);
        let err = result.expect_err("should fail: Metadata tight");
        assert!(
            err.contains("Metadata"),
            "expected 'Metadata' in error: {err}"
        );
    }

    #[test]
    // Intent: check_raid1_relocation_space handles 4 remaining devices with
    //   RAID1 capacity correctly using total/2 (no dominant device).
    // Why: When no single device dominates, capacity = total/2 is the correct formula.
    // Scenario: 5-disk pool, target has 1GB Data; remaining [500MB, 400MB, 300MB] unallocated.
    //   total=1200MB, largest=500MB, rest=700MB → 500 <= 700 → capacity=600MB < 1000MB → fail.
    fn raid1_space_fails_4devs_insufficient_total() {
        let target = make_dev(1, 0, &[("Data", 1_000_000_000)]);
        let rem1 = make_dev(2, 500_000_000, &[]);
        let rem2 = make_dev(3, 400_000_000, &[]);
        let rem3 = make_dev(4, 300_000_000, &[]);
        let result = check_raid1_relocation_space(&[&target], &[&rem1, &rem2, &rem3]);
        let err = result.expect_err("should fail: total/2 < bytes_on_target");
        assert!(err.contains("Data"), "expected 'Data' in error: {err}");
    }

    #[test]
    // Intent: check_no_pending_operation passes when no journal exists.
    // Why: Normal operations should not be blocked when there's no interrupted op.
    // Scenario: Fresh state dir, no pending-op.json.
    fn pending_op_passes_when_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        assert!(check_no_pending_operation(&paths).is_ok());
    }

    #[test]
    // Intent: check_no_pending_operation refuses when a journal exists.
    // Why: Operations on suspect membership risk mounting the wrong disks.
    // Scenario: An add was interrupted; pending-op.json exists.
    fn pending_op_refuses_when_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let journal = crate::journal::build_journal(
            crate::membership::PoolMembership::empty(),
            crate::membership::PoolMembership::empty(),
            crate::journal::OpKind::Add {
                disks: std::collections::BTreeMap::new(),
            },
        );
        crate::journal::write_journal(&paths, &journal).unwrap();
        let err = check_no_pending_operation(&paths).unwrap_err();
        assert!(
            err.contains("interrupted operation"),
            "expected 'interrupted operation' in: {err}"
        );
    }

    #[test]
    // Intent: check_no_pending_operation refuses on corrupt journal (fail-closed).
    // Why: A corrupt journal is ambiguous — safer to block than proceed.
    // Scenario: pending-op.json exists but contains garbage.
    fn pending_op_refuses_on_corrupt_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        std::fs::write(paths.pending_op_json(), "not json").unwrap();
        let err = check_no_pending_operation(&paths).unwrap_err();
        assert!(
            err.contains("cannot read"),
            "expected 'cannot read' in: {err}"
        );
    }

    // --- require_lock_preflight tests ---

    #[test]
    // Intent: require_lock_preflight passes when sysfs says "none".
    // Why: Lock teardown should proceed when nothing is running.
    // Scenario: No active exclusive op.
    fn lock_preflight_passes_when_none() {
        let fs = MockFs::with_sysfs(FSID, "none\n");
        assert!(require_lock_preflight(&fs, FSID).is_ok());
    }

    #[test]
    // Intent: require_lock_preflight rejects on any busy op, including non-paused ones.
    // Why: Lock is teardown — must not proceed while btrfs is mid-operation.
    // Scenario: sysfs says "device add".
    fn lock_preflight_rejects_busy_op() {
        let fs = MockFs::with_sysfs(FSID, "device add\n");
        let err = require_lock_preflight(&fs, FSID).unwrap_err();
        assert!(
            err.contains("device add") && err.contains("in progress"),
            "expected 'device add' + 'in progress' in: {err}"
        );
    }

    #[test]
    // Intent: require_lock_preflight also rejects paused balance (not just running ops).
    // Why: A paused balance is still an active exclusive-op holder.
    // Scenario: sysfs says "balance paused".
    fn lock_preflight_rejects_balance_paused() {
        let fs = MockFs::with_sysfs(FSID, "balance paused\n");
        let err = require_lock_preflight(&fs, FSID).unwrap_err();
        assert!(
            err.contains("balance (paused)") && err.contains("in progress"),
            "expected 'balance (paused)' + 'in progress' in: {err}"
        );
    }

    // --- require_mutation_preflight tests ---

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
    }

    #[test]
    // Intent: require_mutation_preflight passes when no exclusive op is running
    //   and pool is writable.
    // Why: Baseline happy path — mutating commands should proceed on a healthy pool.
    // Scenario: sysfs says "none", findmnt mock absent (swallowed as warning).
    fn mutation_preflight_passes_when_none() {
        let fs = MockFs::with_sysfs(FSID, "none\n");
        let runner = MockRunner::default();
        assert!(require_mutation_preflight(&runner, &fs, FSID, &mp()).is_ok());
    }

    #[test]
    // Intent: require_mutation_preflight rejects when a balance is paused.
    // Why: A paused balance holds the exclusive-op lock indefinitely; proceeding
    //   would deadlock.
    // Scenario: sysfs says "balance paused".
    fn mutation_preflight_rejects_balance_paused() {
        let fs = MockFs::with_sysfs(FSID, "balance paused\n");
        let runner = MockRunner::default();
        let err = require_mutation_preflight(&runner, &fs, FSID, &mp()).unwrap_err();
        assert!(
            err.contains("balance is paused"),
            "expected 'balance is paused' in: {err}"
        );
    }

    #[test]
    // Intent: require_mutation_preflight proceeds (with stderr warning) when
    //   another exclusive op is in-flight.
    // Why: The kernel serializes exclusive ops, so waiting is safe. Blocking
    //   would prevent queuing.
    // Scenario: sysfs says "device add" — function returns Ok and prints a
    //   wait message to stderr (stderr side-effect not captured in unit tests).
    fn mutation_preflight_proceeds_on_busy_op() {
        let fs = MockFs::with_sysfs(FSID, "device add\n");
        let runner = MockRunner::default();
        assert!(require_mutation_preflight(&runner, &fs, FSID, &mp()).is_ok());
    }

    #[test]
    // Intent: require_mutation_preflight rejects when the pool is mounted read-only.
    // Why: Mutating commands will fail at the filesystem layer; better to fail
    //   early with a clear message.
    // Scenario: sysfs says "none", findmnt returns ro mount option.
    fn mutation_preflight_rejects_read_only() {
        let fs = MockFs::with_sysfs(FSID, "none\n");
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson { mount_point: mp() },
            RawCommandOutput {
                cmd: "findmnt".into(),
                stdout: format!(
                    r#"{{"filesystems": [{{"target": "{mount}", "source": "/dev/mapper/braid-a", "fstype": "btrfs", "options": "ro,space_cache=v2"}}]}}"#,
                    mount = mp(),
                ),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let err = require_mutation_preflight(&runner, &fs, FSID, &mp()).unwrap_err();
        assert!(
            err.contains("read-only"),
            "expected 'read-only' in: {err}"
        );
    }
}
