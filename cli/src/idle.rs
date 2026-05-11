use crate::cmd::{CmdRequest, CommandRunner};
use crate::mount_check::MountInfoError;
use crate::parse::{ScrubState, parse_btrfs_scrub_status};
use crate::preflight::{ExclusiveOp, ExclusiveOpError, check_any_btrfs_exclusive_op};
use crate::probe::Filesystem;
use crate::progress::pct_from_bytes;
use crate::types::MountPoint;

#[derive(Debug, PartialEq)]
pub enum IdleResult {
    /// Pool is idle -- no exclusive operations running.
    Idle,
    /// Pool not mounted -- nothing to protect -- allow suspend.
    PoolOffline,
    Busy(BusyReason),
}

#[derive(Debug, PartialEq)]
pub enum BusyReason {
    /// Probe failed, so the pool state is unknowable. Treat as busy so
    /// autosuspend blocks rather than assuming idle.
    Unknown(String),
    /// Scrub progress comes from `btrfs scrub status` because scrub is
    /// not in the kernel exclusive-operation set (see
    /// `reference/btrfs-progs/common/utils.c:1188-1197`), so sysfs cannot
    /// detect or quantify it.
    ScrubRunning {
        pct: Option<u8>,
    },
    Balance,
    BalancePaused,
    DeviceAdd,
    DeviceRemove,
    DeviceReplace,
    Resize,
    SwapActivate,
}

impl std::fmt::Display for BusyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BusyReason::Unknown(msg) => write!(f, "unknown ({msg})"),
            BusyReason::ScrubRunning { pct: Some(p) } => write!(f, "scrub running ({p}%)"),
            BusyReason::ScrubRunning { pct: None } => write!(f, "scrub running"),
            BusyReason::Balance => write!(f, "balance running"),
            BusyReason::BalancePaused => write!(f, "balance paused"),
            BusyReason::DeviceAdd => write!(f, "device add in progress"),
            BusyReason::DeviceRemove => write!(f, "device remove in progress"),
            BusyReason::DeviceReplace => write!(f, "device replace in progress"),
            BusyReason::Resize => write!(f, "resize in progress"),
            BusyReason::SwapActivate => write!(f, "swap activate in progress"),
        }
    }
}

pub fn cmd_idle<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
) -> IdleResult {
    // 1. Pool offline -- nothing to protect.
    let mounted = match is_btrfs_mounted(fs, mount_point) {
        Ok(mounted) => mounted,
        Err(e) => return busy_unknown(e),
    };
    if !mounted {
        return IdleResult::PoolOffline;
    }

    // 2. Scrub via subprocess (scrub is not in the kernel exclop set, so
    //    sysfs cannot see it). Done before fsid lookup so the common
    //    "scrub in progress" case short-circuits the extra probes.
    let scrub_raw = match runner.run(&CmdRequest::BtrfsScrubStatus {
        mount_point: mount_point.clone(),
    }) {
        Ok(raw) => raw,
        Err(e) => return busy_unknown(e),
    };
    let scrub = match parse_btrfs_scrub_status(&scrub_raw) {
        Ok(scrub) => scrub,
        Err(e) => return busy_unknown(e),
    };
    if let ScrubState::Running {
        bytes_scrubbed,
        total_bytes,
        ..
    } = scrub.state
    {
        let pct = match (bytes_scrubbed, total_bytes) {
            (Some(scrubbed), Some(total)) => pct_from_bytes(scrubbed, total),
            _ => None,
        };
        return IdleResult::Busy(BusyReason::ScrubRunning { pct });
    }

    // 3. Every other exclusive operation comes from a single sysfs scan
    //    of /sys/fs/btrfs/*. Same parser preflight.rs uses for mutating
    //    commands (ExclusiveOp::parse), so the two code paths cannot
    //    disagree about what counts as "busy." See
    //    docs/decisions/016-auto-suspend.md for the any-busy semantic.
    match check_any_btrfs_exclusive_op(fs) {
        Ok(()) => IdleResult::Idle,
        Err(ExclusiveOpError::Busy(op)) => IdleResult::Busy(busy_from_exclop(op)),
        Err(e @ (ExclusiveOpError::Read(_) | ExclusiveOpError::Unrecognized(_))) => busy_unknown(e),
    }
}

fn busy_unknown(e: impl std::fmt::Display) -> IdleResult {
    IdleResult::Busy(BusyReason::Unknown(e.to_string()))
}

fn busy_from_exclop(op: ExclusiveOp) -> BusyReason {
    match op {
        // Should never reach here -- check_no_exclusive_op returns Ok(()) for None.
        // Map to Balance as a safe fail-busy default rather than panicking.
        ExclusiveOp::None => BusyReason::Balance,
        ExclusiveOp::Balance => BusyReason::Balance,
        ExclusiveOp::BalancePaused => BusyReason::BalancePaused,
        ExclusiveOp::DeviceAdd => BusyReason::DeviceAdd,
        ExclusiveOp::DeviceRemove => BusyReason::DeviceRemove,
        ExclusiveOp::DeviceReplace => BusyReason::DeviceReplace,
        ExclusiveOp::Resize => BusyReason::Resize,
        ExclusiveOp::SwapActivate => BusyReason::SwapActivate,
    }
}

/// Check whether `mount_point` is a mounted btrfs filesystem.
///
/// Reads `/proc/self/mountinfo` directly via the `Filesystem` abstraction
/// rather than shelling out to `findmnt`. The mount probe is a fail-closed
/// safety gate (autosuspend uses the exit code to decide whether to suspend);
/// any subprocess fallback path that maps "non-zero exit + empty stderr" to
/// "no mount" reintroduces a fail-open seam. IO errors, malformed mountinfo
/// lines, and ambiguous duplicate target entries all surface as
/// `BusyReason::Unknown`, which `main.rs` maps to exit 1 and autosuspend
/// then treats as activity. See docs/decisions/016-auto-suspend.md.
fn is_btrfs_mounted<F: Filesystem + ?Sized>(
    fs: &F,
    mount_point: &MountPoint,
) -> Result<bool, MountInfoError> {
    crate::mount_check::is_btrfs_mounted(fs, mount_point.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::MockRunner;
    use crate::test_fixtures::{
        IDLE_FSID, IDLE_FSID_OTHER, IdleMockFs, assert_idle_busy_unknown, idle_mp,
        idle_ready_for_sysfs_check, idle_runner_with_scrub_finished, idle_scrub_running,
    };
    use std::io::ErrorKind;

    // Intent: Pool not mounted -> PoolOffline (idle).
    // Why: If the pool is offline, there's nothing to protect -- allow suspend.
    // Scenario: NAS has not been unlocked yet; autosuspend checks idle state.
    #[test]
    fn idle_when_pool_offline() {
        let runner = MockRunner::default();
        let fs = IdleMockFs::offline_mountinfo();
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::PoolOffline);
    }

    // Intent: Pool mounted, sysfs reports `none`, scrub idle -> Idle.
    // Why: The normal idle state -- system should be allowed to suspend.
    // Scenario: NAS pool is online but no user activity or maintenance in progress.
    #[test]
    fn idle_when_all_ops_quiet() {
        let runner = idle_runner_with_scrub_finished();
        let fs = IdleMockFs::with_exclop("none");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Idle);
    }

    // Intent: Scrub running -> Busy with percentage from subprocess parser.
    // Why: Scrub is not in the kernel exclop set; only `btrfs scrub status`
    //   sees it. Suspending mid-scrub interrupts data integrity verification.
    // Scenario: Monthly auto-scrub is in progress when autosuspend checks.
    #[test]
    fn busy_when_scrub_running() {
        let (scrub_req, scrub_out) = idle_scrub_running(45);
        let runner = MockRunner::default().with_output(scrub_req, scrub_out);
        // Deliberately seed mountinfo only (no /sys/fs/btrfs listing) --
        // a passing test proves we short-circuit on the scrub probe
        // before the sysfs scan would error on unseeded list_dir.
        let fs = IdleMockFs::mounted_btrfs_only();

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(
            result,
            IdleResult::Busy(BusyReason::ScrubRunning { pct: Some(45) })
        );
    }

    // Intent: Each kernel exclop string maps to the matching BusyReason.
    // Why: Coverage for the new behavior -- before this refactor, only
    //   `balance` / `balance paused` were detected (and only via
    //   `btrfs balance status`); `device add`, `device remove`, `resize`,
    //   and `swap activate` were silently reported as idle.
    // Scenario: Operator runs `btrfs device remove` directly on the pool;
    //   `braid idle` must report busy so autosuspend does not suspend.
    #[test]
    fn busy_when_balance() {
        let (runner, fs) = idle_ready_for_sysfs_check("balance");
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Busy(BusyReason::Balance));
    }

    #[test]
    fn busy_when_balance_paused() {
        let (runner, fs) = idle_ready_for_sysfs_check("balance paused");
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Busy(BusyReason::BalancePaused));
    }

    #[test]
    fn busy_when_device_add() {
        let (runner, fs) = idle_ready_for_sysfs_check("device add");
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Busy(BusyReason::DeviceAdd));
    }

    #[test]
    fn busy_when_device_remove() {
        let (runner, fs) = idle_ready_for_sysfs_check("device remove");
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Busy(BusyReason::DeviceRemove));
    }

    #[test]
    fn busy_when_device_replace() {
        let (runner, fs) = idle_ready_for_sysfs_check("device replace");
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Busy(BusyReason::DeviceReplace));
    }

    #[test]
    fn busy_when_resize() {
        let (runner, fs) = idle_ready_for_sysfs_check("resize");
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Busy(BusyReason::Resize));
    }

    #[test]
    fn busy_when_swap_activate() {
        let (runner, fs) = idle_ready_for_sysfs_check("swap activate");
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Busy(BusyReason::SwapActivate));
    }

    // Intent: Unrecognized exclop value -> Busy::Unknown (fail-closed).
    // Why: A kernel that adds a new exclop name we have not yet mapped
    //   must not be silently treated as idle. Better to report unknown
    //   and let autosuspend block suspend than to suspend mid-unknown-operation.
    // Scenario: New btrfs version writes a new state we do not yet handle.
    #[test]
    fn busy_unknown_on_unrecognized_exclop() {
        let (runner, fs) = idle_ready_for_sysfs_check("brand new op");
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_idle_busy_unknown(result);
    }

    // Intent: Sysfs read error on a real fsid dir -> Busy::Unknown
    //   (fail-closed).
    // Why: If we cannot read the exclop file (permissions, unmount race,
    //   kernel without sysfs btrfs attrs), we must not assume idle.
    //   PermissionDenied is used here intentionally -- the helper skips
    //   NotFound on purpose (features/debug pseudo-dirs), so a NotFound
    //   here would be misread as an "all skipped" idle.
    // Scenario: race between idle check and `btrfs unmount` that leaves
    //   the fsid dir present but inaccessible.
    #[test]
    fn busy_unknown_on_sysfs_read_failure() {
        let runner = idle_runner_with_scrub_finished();
        let fs = IdleMockFs::with_exclop_read_error(ErrorKind::PermissionDenied);

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_idle_busy_unknown(result);
    }

    // Intent: `cmd_idle` must NOT call `BtrfsBalanceStatus`,
    //   `BtrfsReplaceStatus`, or `BtrfsFilesystemShow`.
    //   Those subprocess probes were removed in favor of the sysfs scan.
    // Why: Pins the contract that the refactor preserves -- a
    //   `MockRunner` with only `BtrfsScrubStatus` seeded must still let
    //   `cmd_idle` return successfully. Adding a new caller of any of
    //   those CmdRequests inside `cmd_idle` would surface as MissingMock
    //   here.
    // Scenario: Future change accidentally re-introduces a subprocess
    //   probe; this test catches it before merge.
    #[test]
    fn no_balance_or_replace_subprocess_calls() {
        let (runner, fs) = idle_ready_for_sysfs_check("none");
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Idle);
    }

    // Intent: If the scrub probe itself fails, return Busy::Unknown
    //   (fail-closed). Same shape as the legacy test it replaces.
    // Why: If we cannot determine whether a scrub is running, we must
    //   not allow suspend.
    // Scenario: btrfs scrub status command fails due to kernel bug or
    //   permissions.
    #[test]
    fn busy_unknown_on_scrub_probe_failure() {
        // No scrub mock -> MissingMock when scrub is queried.
        let runner = MockRunner::default();
        let fs = IdleMockFs::with_exclop("none");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_idle_busy_unknown(result);
    }

    /* Intent: a `/proc/self/mountinfo` IO failure must propagate as
     *   Busy::Unknown, not silently become PoolOffline.
     * Why: the original bug shape was "lenient parser branch returned no
     *   entry on a non-zero+empty-stderr findmnt exit -> cmd_idle
     *   concluded PoolOffline -> autosuspend allowed suspend". The
     *   replacement reads /proc/self/mountinfo via the Filesystem
     *   abstraction; the equivalent failure mode is the file being
     *   unreadable and must surface as Busy::Unknown for the
     *   fail-closed contract to hold.
     * Scenario: IdleMockFs.read_to_string("/proc/self/mountinfo") returns
     *   NotFound.
     */
    #[test]
    fn mountinfo_read_failure_is_busy_unknown() {
        let runner = MockRunner::default();
        let fs = IdleMockFs::empty();
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_idle_busy_unknown(result);
    }

    /* Intent: malformed mountinfo content for the target line must
     *   propagate as Busy::Unknown, not silently become
     *   PoolOffline.
     * Why: same fail-closed contract as above. A lenient parser branch
     *   that swallows malformed content reintroduces the "we don't know
     *   -> allow suspend" gap this fix exists to close.
     * Scenario: mountinfo body well-formed except the target line is
     *   missing the "- fstype" tail.
     */
    #[test]
    fn mountinfo_malformed_target_line_is_busy_unknown() {
        let runner = MockRunner::default();
        let fs = IdleMockFs::with_mountinfo(
            "36 35 0:32 / /mnt/storage rw,noatime shared:1 garbage_no_dash_separator\n",
        );
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_idle_busy_unknown(result);
    }

    /* Intent: `/sys/fs/btrfs/` entries named `features` or `debug` are
     *   skipped by name -- the helper never even attempts to read their
     *   `exclusive_operation` (which the kernel does not create for
     *   them; see reference/linux/fs/btrfs/sysfs.c:29-47).
     * Why: skipping by name -- not by "absorb any NotFound on read" --
     *   keeps the fail-closed contract. The next test pins the other
     *   half of that contract: a real fsid dir whose exclop disappears
     *   must surface as Busy::Unknown, not as "skipped pseudo-dir."
     * Scenario: typical NixOS host; sysfs scan walks features, debug,
     *   and a single fsid dir; only the fsid dir's exclop is read.
     */
    #[test]
    fn idle_skips_features_and_debug_pseudo_dirs() {
        let runner = idle_runner_with_scrub_finished();
        // Deliberately do NOT seed `features` or `debug` exclop reads.
        // If the helper ever stopped skipping them by name, IdleMockFs's
        // unseeded-path fallback would return NotFound, which under the
        // current allowlist-only skip would now produce Busy::Unknown.
        // So either direction of regression in the skip rule is
        // observable here.
        let fs = IdleMockFs::mounted_btrfs_only()
            .seed_btrfs_listing(&["features", "debug", IDLE_FSID])
            .seed_exclop(IDLE_FSID, "none");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Idle);
    }

    /* Intent: a `NotFound` on a listed entry that is NOT in the
     *   features/debug allowlist must surface as `Busy::Unknown`,
     *   not be silently absorbed as "probably a pseudo-dir."
     * Why: closes a fail-open seam where, under a concurrent unmount
     *   race, a real fsid dir's `exclusive_operation` could disappear
     *   between the listing and the read -- if the helper treated that
     *   NotFound as a skip, a busy state on that fs would never be
     *   observed and autosuspend would proceed despite incomplete
     *   coverage of the listed btrfs filesystems.
     * Scenario: list returns [IDLE_FSID, IDLE_FSID_OTHER]; the IDLE_FSID dir
     *   reports `none`; IDLE_FSID_OTHER's exclop file is gone (unmount race).
     *   Must return Busy::Unknown rather than concluding Idle from IDLE_FSID
     *   alone.
     */
    #[test]
    fn idle_unknown_entry_notfound_is_fail_closed() {
        let runner = idle_runner_with_scrub_finished();
        let fs = IdleMockFs::mounted_btrfs_only()
            .seed_btrfs_listing(&[IDLE_FSID, IDLE_FSID_OTHER])
            .seed_exclop(IDLE_FSID, "none");
        // IDLE_FSID_OTHER intentionally has no exclop seeded -> NotFound.

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_idle_busy_unknown(result);
    }

    /* Intent: when the host has multiple btrfs filesystems, ANY busy
     *   fsid blocks suspend -- not just the one the pool maps to.
     * Why: `cmd_idle` no longer does mount->fsid resolution. The trade
     *   is conservative-by-design: a busy non-pool btrfs (e.g. root)
     *   keeps the system awake. A future "scope to pool fsid" change
     *   would defeat this protection silently; this test pins it.
     * Scenario: NixOS host with btrfs root and a braid pool; root is
     *   idle, pool is mid-balance. autosuspend must see Busy.
     */
    #[test]
    fn idle_any_busy_blocks_suspend_multi_btrfs() {
        let runner = idle_runner_with_scrub_finished();
        // First entry is `none`, second is `balance`. The helper iterates
        // entries in returned order, so the loop must continue past the
        // first and report Busy on the second.
        let fs = IdleMockFs::mounted_btrfs_only()
            .seed_btrfs_listing(&[IDLE_FSID_OTHER, IDLE_FSID])
            .seed_exclop(IDLE_FSID_OTHER, "none")
            .seed_exclop(IDLE_FSID, "balance");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Busy(BusyReason::Balance));
    }

    /* Intent: `is_btrfs_mounted` returned true but `/sys/fs/btrfs/` is
     *   empty -> Busy::Unknown (fail-closed).
     * Why: this is an invariant violation -- a mounted btrfs always has
     *   a sysfs entry. Treating an empty listing as Idle would silently
     *   suppress every busy state. Better to report unknown and let
     *   autosuspend block.
     * Scenario: defensive coverage. No real-world btrfs gets here, but
     *   a kernel bug, sandbox, or namespace shenanigan could.
     */
    #[test]
    fn idle_zero_fsid_dirs_after_mount_check_is_busy_unknown() {
        let runner = idle_runner_with_scrub_finished();
        let fs = IdleMockFs::mounted_btrfs_only().seed_btrfs_listing(&[]);

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_idle_busy_unknown(result);
    }

    /* Intent: a `list_dir("/sys/fs/btrfs")` IO failure (e.g.
     *   PermissionDenied, EIO) must propagate as Busy::Unknown, not
     *   become Idle.
     * Why: without this, a future change could conflate "scan failed"
     *   with "no busy entries found" and reintroduce the fail-open seam
     *   the autosuspend gate exists to prevent. NotFound is excluded
     *   here because RealFilesystem::list_dir folds NotFound into
     *   Ok(vec![]) (probe.rs:47), which the empty-listing test above
     *   covers separately.
     * Scenario: sysfs is mounted but `/sys/fs/btrfs` is unreadable
     *   under our credentials.
     */
    #[test]
    fn idle_list_dir_io_error_is_fail_closed() {
        let runner = idle_runner_with_scrub_finished();
        let fs =
            IdleMockFs::mounted_btrfs_only().seed_btrfs_listing_error(ErrorKind::PermissionDenied);

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_idle_busy_unknown(result);
    }
}
