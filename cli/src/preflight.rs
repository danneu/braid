use std::fmt;

use crate::cmd::{CmdRequest, CommandRunner};
use crate::journal;
use crate::parse::parse_upsc;
use crate::parse::types::{BtrfsBgType, BtrfsDeviceUsageEntry, BtrfsDfOutput};
use crate::parse::{parse_btrfs_device_usage, parse_findmnt_json};
use crate::preview::PreviewNote;
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
    /// - any other busy state => `Ok(Some(op))` for the caller to surface
    ///   as a `PreviewNote::Info`
    ///
    /// Why: these commands invoke btrfs with `--enqueue`, so kernel serialization
    /// is the correctness mechanism and avoids TOCTOU-style preflight busy failures.
    /// Paused balance is the exception because it can block indefinitely.
    RejectPausedBalanceElseEnqueue,
}

/// Apply `policy` to the current exclusive-op state read from sysfs.
///
/// `Ok(None)` means the pool is idle. `Ok(Some(op))` is reachable only under
/// `RejectPausedBalanceElseEnqueue` when a non-paused exclusive op is in
/// flight -- the caller surfaces it as a `PreviewNote::Info`. `Err(msg)`
/// means the policy rejected the state (paused balance, any busy under
/// `RejectAnyBusy`, unrecognized value, or sysfs read failure).
fn check_exclusive_op_with_policy<F: Filesystem + ?Sized>(
    fs: &F,
    fsid: &str,
    policy: ExclusiveOpPolicy,
) -> Result<Option<ExclusiveOp>, String> {
    match check_no_exclusive_op(fs, fsid) {
        Ok(()) => Ok(None),
        Err(ExclusiveOpError::Busy(op)) => match policy {
            ExclusiveOpPolicy::RejectAnyBusy => Err(format!(
                "cannot lock: {op} is in progress. Wait for it to finish first."
            )),
            ExclusiveOpPolicy::RejectPausedBalanceElseEnqueue => match op {
                ExclusiveOp::BalancePaused => {
                    Err("a btrfs balance is paused. Resume or cancel it before proceeding.".into())
                }
                _ => Ok(Some(op)),
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
///
/// `Ok(None)` = writable mount. `Ok(Some(body))` = the probe itself
/// failed (spawn error or unparseable JSON); the caller wraps `body` in
/// a `PreviewNote::Warn` so operators know the ro guard did not run.
/// `Err(msg)` = pool is mounted read-only.
fn check_not_read_only<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<Option<String>, String> {
    let raw = match runner.run(&CmdRequest::FindmntJson {
        mount_point: mount_point.clone(),
    }) {
        Ok(r) => r,
        Err(e) => {
            return Ok(Some(e.to_string()));
        }
    };

    let findmnt = match parse_findmnt_json(&raw) {
        Ok(f) => f,
        Err(e) => {
            return Ok(Some(e.to_string()));
        }
    };

    let entry = findmnt
        .filesystems
        .iter()
        .find(|e| e.target == mount_point.as_str());
    if let Some(entry) = entry
        && entry.options.split(',').any(|opt| opt.trim() == "ro")
    {
        return Err(format!(
            "pool is mounted read-only. Remount read-write first:\n  \
                 mount -o remount,rw {mount_point}"
        ));
    }
    Ok(None)
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

/// Check that the surviving device can hold all live data after a 2->1
/// eviction (RAID1 data -> single, RAID1 metadata/system -> DUP).
///
/// Uses logical usage from `btrfs filesystem df` rather than per-device
/// allocations, so it is correct regardless of current profile mix
/// (RAID1, single, DUP, or leftover chunks from an interrupted balance).
///
/// Post-balance + post-remove demand on the survivor:
///   Data (single):     Data.used
///   Metadata (DUP):    2 * Metadata.used
///   System (DUP):      2 * System.used
/// Usable survivor capacity = device_size - device_slack.
///
/// GlobalReserve is excluded -- it is an internal emergency reservation
/// carved out of Metadata, not additional on-disk data.
pub fn check_single_survivor_capacity(
    df: &BtrfsDfOutput,
    survivor: &BtrfsDeviceUsageEntry,
) -> Result<(), String> {
    let sum_bg = |t: BtrfsBgType| -> u64 {
        df.entries
            .iter()
            .filter(|e| e.bg_type == t)
            .map(|e| e.bg_used)
            .sum()
    };
    let data = sum_bg(BtrfsBgType::Data);
    let metadata = sum_bg(BtrfsBgType::Metadata);
    let system = sum_bg(BtrfsBgType::System);
    let needed = data + 2 * metadata + 2 * system;
    let usable = survivor.device_size.saturating_sub(survivor.device_slack);
    if needed > usable {
        return Err(format!(
            "not enough space on surviving device after RAID1 -> single conversion.\n  \
             data + 2 * metadata + 2 * system: {}\n  \
             surviving device usable capacity:  {}\n\n\
             Free up space by deleting files first, or `braid add` a larger disk.",
            format_bytes(needed),
            format_bytes(usable),
        ));
    }
    Ok(())
}

/// Refuse if the configured UPS is on battery, in any critical state,
/// or unreachable.
///
/// Fail-closed: daemon-down, malformed output, and an empty `ups.status`
/// all produce the same refusal. One wording covers all three so the
/// message stays honest when the real cause is comms-failure rather than
/// an on-battery condition. Caller passes `None` when no UPS is
/// configured, which makes this a no-op.
///
/// Critical-state classification is shared with the TUI via
/// `UpsStatusFlag::is_critical` so the two surfaces stay in sync: any
/// token the UI paints red (LB, TESTFAIL, COMMBAD, FSD) also blocks
/// mutations here. `OB` alone is yellow in the UI but still refused
/// here -- starting a long mutation while the pool is on battery
/// widens the mid-mutation recovery surface.
///
/// Wire into `add`, `remove`, `remove-missing`, `replace` before journal
/// write. See docs/decisions/020-ups-integration.md for the safety
/// rationale.
pub fn check_ups_not_on_battery<R: CommandRunner>(
    runner: &R,
    ups_name: Option<&str>,
    op: &str,
) -> Result<(), String> {
    let Some(name) = ups_name else {
        return Ok(());
    };
    let refuse = |context: &str| {
        Err(format!(
            "cannot verify UPS is on utility power ({context}) -- refusing to start {op}. \
             Check 'braid ups status', restore utility power, then retry."
        ))
    };
    let raw = match runner.run(&CmdRequest::UpscQuery {
        name: name.to_owned(),
    }) {
        Ok(r) => r,
        Err(_) => return refuse("upsc command failed"),
    };
    let parsed = match parse_upsc(&raw) {
        Ok(p) => p,
        Err(_) => return refuse("upsc output unparseable or upsd unreachable"),
    };
    if parsed.status_flags.is_empty() {
        return refuse("ups.status is empty or missing");
    }
    if parsed.is_critical() {
        return refuse("UPS reports a critical state (LB / TESTFAIL / COMMBAD / FSD)");
    }
    if parsed.is_on_battery() {
        return refuse("UPS reports on-battery");
    }
    Ok(())
}

/// Guard for mutating pool commands (add, remove, remove-missing, replace).
///
/// Returns accumulated soft-success notes the caller surfaces as
/// `PreviewNote` entries (dry-run stdout via `Preview::render`, real-run
/// stderr via `preview::render_notes_for_stderr`, failure-path stderr
/// via `cmd_*`'s `report.notes` rendering). Never writes to stderr
/// itself. Hard failures (paused balance, mounted read-only) return
/// `Err(String)` suitable for wrapping in a command's `Validation`
/// error variant.
///
/// `Ok(notes)`: the vec may be empty (clean preflight) or carry one
/// `Info` (busy-op enqueued) and/or one `Warn` (read-only probe
/// degraded), in that insertion order.
pub fn require_mutation_preflight<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    fsid: &str,
    mount_point: &MountPoint,
) -> Result<Vec<PreviewNote>, String> {
    let mut notes: Vec<PreviewNote> = Vec::new();
    if let Some(op) =
        check_exclusive_op_with_policy(fs, fsid, ExclusiveOpPolicy::RejectPausedBalanceElseEnqueue)?
    {
        notes.push(PreviewNote::Info(format!(
            "waiting for in-flight {op} to finish..."
        )));
    }
    if let Some(probe_err) = check_not_read_only(runner, mount_point)? {
        notes.push(PreviewNote::Warn(format!(
            "read-only pre-flight failed: {probe_err}; proceeding anyway"
        )));
    }
    Ok(notes)
}

/// Guard for `braid lock` (teardown: unmount + close LUKS).
///
/// Hard-fails on any active exclusive op. Lock must not proceed while btrfs
/// is mid balance/device-add/device-remove/device-replace/resize.
///
/// Returns `Err(String)` suitable for wrapping in `LockError::Failed`.
pub fn require_lock_preflight<F: Filesystem + ?Sized>(fs: &F, fsid: &str) -> Result<(), String> {
    check_exclusive_op_with_policy(fs, fsid, ExclusiveOpPolicy::RejectAnyBusy).map(|_| ())
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

    /// MockRunner pre-populated with a `FindmntJson` response reporting
    /// a writable (`rw,...`) mount. Mirrors the mock shape used by
    /// `read_only_passes_when_rw`; used for every preflight test that
    /// expects the findmnt probe to succeed.
    fn rw_runner() -> MockRunner {
        MockRunner::default().with_output(
            CmdRequest::FindmntJson { mount_point: mp() },
            RawCommandOutput {
                cmd: "findmnt --json --output TARGET,SOURCE,FSTYPE,OPTIONS --mountpoint /mnt/storage".into(),
                stdout: r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-vdb","fstype":"btrfs","options":"rw,relatime,ssd,space_cache=v2,subvolid=5,subvol=/"}]}"#.into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    #[test]
    // Intent: check_not_read_only returns Ok(None) when pool is rw.
    // Why: Confirms rw mounts are not falsely rejected.
    // Scenario: Normal pool mount with default options.
    fn read_only_passes_when_rw() {
        let runner = rw_runner();
        let out = check_not_read_only(&runner, &mp()).unwrap();
        assert!(out.is_none(), "expected Ok(None) on rw mount, got {out:?}");
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
    // Intent: check_not_read_only surfaces probe-failure body via Ok(Some(_)).
    // Why: A bug in the safety check shouldn't block valid operations, but the
    //   caller must still surface a Warn note so operators know the guard did
    //   not run.
    // Scenario: findmnt not found or permissions issue.
    fn read_only_returns_probe_error_body() {
        let runner = MockRunner::default(); // no mock → MissingMock
        let body = check_not_read_only(&runner, &mp())
            .unwrap()
            .expect("expected Ok(Some(_)) with probe-failure body");
        assert!(!body.is_empty(), "probe-failure body must not be empty");
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

    // --- check_single_survivor_capacity tests ---

    use crate::parse::types::{BtrfsDfEntry, BtrfsProfile};

    fn make_df(entries: &[(BtrfsBgType, u64)]) -> BtrfsDfOutput {
        BtrfsDfOutput {
            entries: entries
                .iter()
                .map(|(t, used)| BtrfsDfEntry {
                    bg_type: *t,
                    bg_profile: BtrfsProfile::Raid1,
                    bg_used: *used,
                    bg_total: *used,
                })
                .collect(),
        }
    }

    fn make_survivor(device_size: u64, device_slack: u64) -> BtrfsDeviceUsageEntry {
        BtrfsDeviceUsageEntry {
            path: "/dev/mapper/braid-disk2".to_string(),
            devid: 2,
            device_size,
            device_slack,
            allocations: vec![],
            unallocated: 0,
        }
    }

    #[test]
    // Intent: check_single_survivor_capacity passes when data + 2*meta + 2*sys
    //   fits comfortably within the survivor's device_size - device_slack.
    // Why: Common healthy pool: a 1 GiB survivor can absorb a lightly-used pool.
    // Scenario: 1 GiB survivor (no slack); 200 MiB Data, 10 MiB Metadata,
    //   4 KiB System. needed = 200 + 20 + ~0 = 220 MiB << 1024 MiB.
    fn survivor_fits_passes() {
        let df = make_df(&[
            (BtrfsBgType::Data, 200 * 1024 * 1024),
            (BtrfsBgType::Metadata, 10 * 1024 * 1024),
            (BtrfsBgType::System, 4 * 1024),
        ]);
        let survivor = make_survivor(1024 * 1024 * 1024, 0);
        assert!(check_single_survivor_capacity(&df, &survivor).is_ok());
    }

    #[test]
    // Intent: check_single_survivor_capacity fails when the data alone already
    //   exceeds the survivor's usable capacity.
    // Why: This is the obvious sad path — the balance would ENOSPC on Data.
    // Scenario: 512 MiB survivor; Data.used = 600 MiB.
    fn survivor_undersized_fails() {
        let df = make_df(&[(BtrfsBgType::Data, 600 * 1024 * 1024)]);
        let survivor = make_survivor(512 * 1024 * 1024, 0);
        let err = check_single_survivor_capacity(&df, &survivor)
            .expect_err("should fail: data > survivor");
        assert!(
            err.contains("not enough space on surviving device"),
            "wrong error: {err}"
        );
    }

    #[test]
    // Intent: check_single_survivor_capacity fails when Data alone fits but
    //   2 * Metadata tips the demand past usable.
    // Why: This is the exact bug the 2->1 preflight exists to catch —
    //   post-balance metadata is DUP (2x physical) even when pre-balance
    //   RAID1 hid the overhead.
    // Scenario: 1000 MiB survivor; Data = 700 MiB, Metadata = 200 MiB.
    //   Data alone fits. needed = 700 + 400 = 1100 MiB > 1000 MiB.
    fn metadata_doubling_tips_over() {
        let df = make_df(&[
            (BtrfsBgType::Data, 700 * 1024 * 1024),
            (BtrfsBgType::Metadata, 200 * 1024 * 1024),
        ]);
        let survivor = make_survivor(1000 * 1024 * 1024, 0);
        let err = check_single_survivor_capacity(&df, &survivor)
            .expect_err("should fail: 2 * meta tips over");
        assert!(err.contains("data + 2 * metadata"), "wrong error: {err}");
    }

    #[test]
    // Intent: check_single_survivor_capacity passes on an empty pool.
    // Why: No entries must not crash or false-fail; the helper is called on
    //   every 2->1 remove including against a pool mounted for the first time.
    // Scenario: Empty df, 1 GiB survivor. needed = 0.
    fn empty_pool_passes() {
        let df = make_df(&[]);
        let survivor = make_survivor(1024 * 1024 * 1024, 0);
        assert!(check_single_survivor_capacity(&df, &survivor).is_ok());
    }

    #[test]
    // Intent: check_single_survivor_capacity passes when only metadata/system
    //   is present and 2x fits.
    // Why: Exercises the boundary where Data.used == 0 but Metadata/System
    //   still incur the 2x multiplier -- confirms metadata/system are
    //   counted correctly when data is absent.
    // Scenario: 1 GiB survivor; 200 MiB Metadata, 16 MiB System, 0 Data.
    //   needed = 2 * 200 + 2 * 16 = 432 MiB << 1024 MiB.
    fn metadata_only_passes() {
        let df = make_df(&[
            (BtrfsBgType::Metadata, 200 * 1024 * 1024),
            (BtrfsBgType::System, 16 * 1024 * 1024),
        ]);
        let survivor = make_survivor(1024 * 1024 * 1024, 0);
        assert!(check_single_survivor_capacity(&df, &survivor).is_ok());
    }

    #[test]
    // Intent: check_single_survivor_capacity excludes GlobalReserve from the
    //   demand calculation.
    // Why: GlobalReserve is an internal emergency reservation carved out of
    //   Metadata, not on-disk data that needs to migrate; counting it would
    //   false-fail healthy pools.
    // Scenario: 100 MiB survivor; real Data = 30 MiB, real Metadata = 5 MiB,
    //   GlobalReserve.used = 999 MiB (impossibly big, a forgotten filter
    //   would double it into needed and refuse). Expected: pass.
    fn global_reserve_excluded() {
        let df = make_df(&[
            (BtrfsBgType::Data, 30 * 1024 * 1024),
            (BtrfsBgType::Metadata, 5 * 1024 * 1024),
            (BtrfsBgType::GlobalReserve, 999 * 1024 * 1024),
        ]);
        let survivor = make_survivor(100 * 1024 * 1024, 0);
        assert!(check_single_survivor_capacity(&df, &survivor).is_ok());
    }

    #[test]
    // Intent: check_single_survivor_capacity subtracts device_slack from
    //   device_size when computing usable capacity.
    // Why: device_slack is space the kernel cannot address (alignment
    //   gaps, reserved boundary regions); ignoring it would false-pass on
    //   a pool whose real usable capacity is smaller than device_size.
    // Scenario: 1 GiB device_size + 100 MiB device_slack = 924 MiB usable;
    //   demand = 950 MiB. Expected: fail (950 > 924).
    fn device_slack_reduces_usable() {
        let df = make_df(&[(BtrfsBgType::Data, 950 * 1024 * 1024)]);
        let survivor = make_survivor(1024 * 1024 * 1024, 100 * 1024 * 1024);
        assert!(check_single_survivor_capacity(&df, &survivor).is_err());
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
    // Intent: require_mutation_preflight returns an empty notes vec on the
    //   clean path (no busy op, rw probe).
    // Why: Baseline happy path -- mutating commands should proceed on a healthy
    //   pool without emitting any PreviewNote.
    // Scenario: sysfs says "none", findmnt reports rw.
    fn mutation_preflight_passes_when_none() {
        let fs = MockFs::with_sysfs(FSID, "none\n");
        let runner = rw_runner();
        let notes = require_mutation_preflight(&runner, &fs, FSID, &mp()).unwrap();
        assert!(notes.is_empty(), "expected empty notes, got {notes:?}");
    }

    #[test]
    // Intent: require_mutation_preflight rejects when a balance is paused.
    // Why: A paused balance holds the exclusive-op lock indefinitely; proceeding
    //   would deadlock.
    // Scenario: sysfs says "balance paused".
    fn mutation_preflight_rejects_balance_paused() {
        let fs = MockFs::with_sysfs(FSID, "balance paused\n");
        let runner = rw_runner();
        let err = require_mutation_preflight(&runner, &fs, FSID, &mp()).unwrap_err();
        assert!(
            err.contains("balance is paused"),
            "expected 'balance is paused' in: {err}"
        );
    }

    #[test]
    // Intent: require_mutation_preflight surfaces a busy exclusive op as a
    //   single Info note.
    // Why: The kernel serializes exclusive ops, so waiting is safe; the
    //   operator still needs to know the mutation is about to enqueue behind
    //   the in-flight op.
    // Scenario: sysfs says "device add", findmnt reports rw.
    fn mutation_preflight_busy_op_returns_info_note() {
        let fs = MockFs::with_sysfs(FSID, "device add\n");
        let runner = rw_runner();
        let notes = require_mutation_preflight(&runner, &fs, FSID, &mp()).unwrap();
        assert_eq!(notes.len(), 1, "expected one Info note, got {notes:?}");
        match &notes[0] {
            PreviewNote::Info(body) => {
                assert!(body.contains("waiting for in-flight"), "body={body:?}");
                assert!(body.contains("device add"), "body={body:?}");
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    // Intent: require_mutation_preflight surfaces a findmnt probe failure as
    //   a single Warn note.
    // Why: The read-only guard is a best-effort safety net; if the probe
    //   itself fails, the caller must not silently proceed -- operators
    //   should know the ro mount guard did not run.
    // Scenario: sysfs says "none"; the runner has no FindmntJson mock, so the
    //   probe returns MissingMock.
    fn mutation_preflight_readonly_probe_failure_returns_warn_note() {
        let fs = MockFs::with_sysfs(FSID, "none\n");
        let runner = MockRunner::default();
        let notes = require_mutation_preflight(&runner, &fs, FSID, &mp()).unwrap();
        assert_eq!(notes.len(), 1, "expected one Warn note, got {notes:?}");
        match &notes[0] {
            PreviewNote::Warn(body) => {
                assert!(
                    body.starts_with("read-only pre-flight failed:"),
                    "body={body:?}"
                );
                assert!(body.ends_with("; proceeding anyway"), "body={body:?}");
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    // Intent: require_mutation_preflight stacks [Info, Warn] when both an
    //   in-flight exclusive op AND a probe failure happen.
    // Why: insertion order is load-bearing for the renderer (busy-op Info
    //   before probe-failure Warn) so dry-run stdout and failure-path stderr
    //   agree on how the two diagnostics present.
    // Scenario: sysfs says "device add", runner has no FindmntJson mock.
    fn mutation_preflight_busy_and_probe_failure_returns_info_then_warn() {
        let fs = MockFs::with_sysfs(FSID, "device add\n");
        let runner = MockRunner::default();
        let notes = require_mutation_preflight(&runner, &fs, FSID, &mp()).unwrap();
        assert_eq!(notes.len(), 2, "expected two notes, got {notes:?}");
        assert!(
            matches!(
                &notes[0],
                PreviewNote::Info(b) if b.contains("waiting for in-flight") && b.contains("device add")
            ),
            "notes[0]={:?}",
            notes[0]
        );
        assert!(
            matches!(
                &notes[1],
                PreviewNote::Warn(b) if b.starts_with("read-only pre-flight failed:")
                    && b.ends_with("; proceeding anyway")
            ),
            "notes[1]={:?}",
            notes[1]
        );
    }

    // --- check_ups_not_on_battery tests ---

    fn upsc_mock(name: &str, stdout: &str, exit: i32) -> MockRunner {
        MockRunner::default().with_output(
            CmdRequest::UpscQuery {
                name: name.to_owned(),
            },
            RawCommandOutput {
                cmd: format!("upsc {name}"),
                stdout: stdout.to_owned(),
                stderr: if exit == 0 { "" } else { "daemon unreachable" }.to_owned(),
                exit_status: exit,
            },
        )
    }

    #[test]
    // Intent: check_ups_not_on_battery passes when ups_name is None.
    // Why: users who have not enabled braid.ups should not see a preflight
    // change at all. The no-op guard is load-bearing for config compat.
    // Scenario: braid.ups.enable = false (default), operator runs `braid add`.
    fn ups_no_config_is_noop() {
        let runner = MockRunner::default();
        assert!(check_ups_not_on_battery(&runner, None, "add").is_ok());
    }

    #[test]
    // Intent: check_ups_not_on_battery passes when ups.status = OL.
    // Why: preflight must not refuse the healthy case; doing so would make
    // `braid.ups.enable = true` refuse every mutation and silently regress.
    // Scenario: operator runs `braid add` against a UPS on utility power.
    fn ups_online_passes() {
        let runner = upsc_mock("ups", "ups.status: OL\n", 0);
        assert!(check_ups_not_on_battery(&runner, Some("ups"), "add").is_ok());
    }

    #[test]
    // Intent: OB in the status set triggers refusal.
    // Why: primary safety feature -- narrow the mid-mutation recovery surface
    // by rejecting avoidable starts on battery.
    // Scenario: operator runs `braid remove` while the UPS is on battery.
    fn ups_on_battery_refuses() {
        let runner = upsc_mock("ups", "ups.status: OB\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "remove").unwrap_err();
        assert!(err.contains("utility power"), "got: {err}");
        assert!(err.contains("remove"), "op name should appear in: {err}");
    }

    #[test]
    // Intent: LB alone (without OB) still triggers refusal.
    // Why: upsmon's critical-state check requires OB+LB together, but a
    // battery self-test can transiently show LB+OL. braid refuses either
    // way because starting a long mutation while LB is reported is risky.
    // Scenario: UPS reports LB during a self-test or flaky USB HID state.
    fn ups_low_battery_refuses() {
        let runner = upsc_mock("ups", "ups.status: OL LB\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(
            err.contains("critical") || err.contains("on-battery"),
            "got: {err}"
        );
    }

    #[test]
    // Intent: TESTFAIL in ups.status triggers refusal, even when OL is
    // also present.
    // Why: the TUI shows TESTFAIL in red as a critical state; preflight
    // must agree. A driver that surfaces TESTFAIL while OL is lit must
    // not be a "green light" for mutation starts. Shares the predicate
    // with the UI so the two surfaces cannot drift.
    // Scenario: some drivers append TESTFAIL to ups.status on a
    // failed self-test.
    fn ups_test_fail_refuses() {
        let runner = upsc_mock("ups", "ups.status: OL TESTFAIL\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(err.contains("critical"), "got: {err}");
    }

    #[test]
    // Intent: COMMBAD triggers refusal.
    // Why: comms loss is fail-closed by definition -- we cannot trust
    // what the UPS reports next. The TUI paints this red; preflight
    // refuses.
    // Scenario: USB cable unplugged mid-session; driver reports
    // COMMBAD in ups.status before declaring the UPS lost.
    fn ups_comm_bad_refuses() {
        let runner = upsc_mock("ups", "ups.status: OL COMMBAD\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(err.contains("critical"), "got: {err}");
    }

    #[test]
    // Intent: FSD triggers refusal.
    // Why: Forced-Shutdown-Delay means the UPS has decided shutdown is
    // imminent. Starting a mutation here is always wrong.
    // Scenario: network UPS has been issued a scheduled shutdown.
    fn ups_fsd_refuses() {
        let runner = upsc_mock("ups", "ups.status: OL FSD\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(err.contains("critical"), "got: {err}");
    }

    #[test]
    // Intent: daemon-down (non-zero upsc exit) refuses the mutation.
    // Why: fail-closed -- if braid cannot determine UPS state, it must not
    // start work it can't guarantee a clean shutdown from.
    // Scenario: upsd.service has crashed or hasn't started yet.
    fn ups_daemon_down_refuses() {
        let runner = upsc_mock("ups", "", 1);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "replace").unwrap_err();
        assert!(err.contains("utility power"), "got: {err}");
    }

    #[test]
    // Intent: empty status set (no ups.status line) refuses.
    // Why: an absent ups.status is indistinguishable from a stuck driver;
    // treating empty as OL would undermine the whole preflight contract.
    // Scenario: dummy-ups driver hasn't filled in ups.status yet.
    fn ups_empty_status_refuses() {
        let runner = upsc_mock("ups", "battery.charge: 100\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "remove-missing").unwrap_err();
        assert!(err.contains("utility power"), "got: {err}");
    }

    #[test]
    // Intent: missing mock output is treated as daemon-down (fail-closed).
    // Why: MockRunner::default() produces MissingMock, which mirrors a
    // subprocess spawn failure at runtime; both must refuse.
    // Scenario: a future refactor forgets to wire the upsc mock in a test.
    fn ups_missing_mock_refuses() {
        let runner = MockRunner::default();
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(err.contains("utility power"), "got: {err}");
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
        assert!(err.contains("read-only"), "expected 'read-only' in: {err}");
    }
}
