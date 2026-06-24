use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::{ScrubState, parse_btrfs_scrub_status};
use crate::preflight::{ExclusiveOp, ExclusiveOpError, check_any_btrfs_exclusive_op};
use crate::probe::Filesystem;
use crate::progress::pct_from_bytes;
use crate::types::MountPoint;

/// Tri-state result of the `braid idle` autosuspend gate: `PoolOffline`
/// and `Idle` both allow suspend (exit 0), `Busy` blocks it (exit 1).
/// Fail-closed -- any unknowable probe maps to `Busy`, never to idle.
#[derive(Debug, PartialEq)]
pub enum IdleResult {
    /// Pool is idle -- no exclusive operations running.
    Idle,
    /// Pool not mounted -- nothing to protect -- allow suspend.
    PoolOffline,
    /// Pool is busy -- block suspend. Carries the reason for status output.
    Busy(BusyReason),
}

/// Why `braid idle` reports busy. Its `Display` is the idle-specific
/// status-line surface and intentionally diverges from
/// `ExclusiveOp::Display` (e.g. "balance paused" vs "balance (paused)").
#[derive(Debug, PartialEq)]
pub enum BusyReason {
    /// Probe failed, so the pool state is unknowable. Treat as busy so
    /// autosuspend blocks rather than assuming idle.
    Unknown(String),
    /// Scrub progress comes from `btrfs scrub status` because scrub is
    /// not in the kernel exclusive-operation set (`enum
    /// btrfs_exclusive_operation`, `reference/linux/fs/btrfs/fs.h`), so
    /// sysfs cannot detect or quantify it.
    ScrubRunning { pct: Option<u8> },
    /// Shared sysfs exclusive-op identity so idle and mutating-command
    /// preflight cannot drift on the set of operations that block suspend.
    Exclop(ExclusiveOp),
}

impl std::fmt::Display for BusyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BusyReason::Unknown(msg) => write!(f, "unknown ({msg})"),
            BusyReason::ScrubRunning { pct: Some(p) } => write!(f, "scrub running ({p}%)"),
            BusyReason::ScrubRunning { pct: None } => write!(f, "scrub running"),
            // `braid idle` renders standalone status-line labels for
            // balance variants; other ops fall through to `ExclusiveOp`'s
            // sentence-embedding noun phrase via the `{op} in progress` arm.
            BusyReason::Exclop(ExclusiveOp::Balance) => write!(f, "balance running"),
            BusyReason::Exclop(ExclusiveOp::BalancePaused) => write!(f, "balance paused"),
            BusyReason::Exclop(op) => write!(f, "{op} in progress"),
        }
    }
}

/// Autosuspend gate: probes mount, host-wide sysfs exclop, then pool scrub,
/// and maps every unknowable probe to `Busy` so suspend fails closed.
pub fn cmd_idle<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
) -> IdleResult {
    // 1. Pool offline -- nothing to protect.
    let mounted = match crate::mount_check::is_btrfs_mounted(fs, mount_point.as_str()) {
        Ok(mounted) => mounted,
        Err(e) => return busy_unknown("mountinfo", e),
    };
    if !mounted {
        return IdleResult::PoolOffline;
    }

    // 2. Kernel exclusive operations via sysfs. This is cheap and uses
    //    the same parser preflight.rs uses for mutating commands
    //    (ExclusiveOp::parse), so the two code paths cannot disagree
    //    about what counts as "busy." See
    //    docs/design/decisions/016-auto-suspend.md for the any-busy semantic.
    match check_any_btrfs_exclusive_op(fs) {
        Ok(()) => {}
        Err(ExclusiveOpError::Busy(op)) => return IdleResult::Busy(BusyReason::Exclop(op)),
        Err(e @ (ExclusiveOpError::Read(_) | ExclusiveOpError::Unrecognized(_))) => {
            return busy_unknown("sysfs", e);
        }
    }

    // 3. Scrub via subprocess. Scrub is outside the kernel exclop set, so
    //    sysfs cannot see or quantify it.
    let scrub_raw = match runner.run(&CmdRequest::BtrfsScrubStatus {
        mount_point: mount_point.clone(),
    }) {
        Ok(raw) => raw,
        Err(e) => return busy_unknown("scrub", e),
    };
    let scrub = match parse_btrfs_scrub_status(&scrub_raw) {
        Ok(scrub) => scrub,
        Err(e) => return busy_unknown("scrub", e),
    };
    match scrub.state {
        ScrubState::Running {
            bytes_scrubbed,
            total_bytes,
            ..
        } => {
            let pct = match (bytes_scrubbed, total_bytes) {
                (Some(scrubbed), Some(total)) => pct_from_bytes(scrubbed, total),
                _ => None,
            };
            IdleResult::Busy(BusyReason::ScrubRunning { pct })
        }
        ScrubState::Never
        | ScrubState::Finished { .. }
        | ScrubState::Aborted { .. }
        | ScrubState::Interrupted { .. } => IdleResult::Idle,
        ScrubState::Unknown => busy_unknown("scrub", "unrecognized scrub state"),
    }
}

fn busy_unknown(layer: &str, e: impl std::fmt::Display) -> IdleResult {
    IdleResult::Busy(BusyReason::Unknown(format!("{layer}: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::test_fixtures::{
        IDLE_FSID, IDLE_FSID_OTHER, IdleMockFs, assert_idle_busy_unknown_prefix, idle_mp,
        idle_ready_for_sysfs_check, idle_runner_with_scrub_finished, idle_scrub_running,
        idle_scrub_running_no_bytes, scrub_status_aborted, scrub_status_interrupted,
        scrub_status_never, scrub_status_unknown,
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

    // Intent: a non-btrfs filesystem mounted at the configured mount point
    //   yields PoolOffline (allow suspend), not Busy and not an error.
    // Why it exists: cmd_idle gates on is_btrfs_mounted, which collapses
    //   "nothing mounted" and "non-btrfs mounted" into one Ok(false) ->
    //   PoolOffline. This deliberately diverges from probe_pool / probe_fsid /
    //   probe_pool_alerts, which reject a non-btrfs mount at the same path with
    //   ProbeError::NotBtrfs. The divergence is correct (ext4 at /mnt/storage
    //   means the btrfs pool is not assembled, so suspend is safe) but was
    //   unguarded: a refactor swapping is_btrfs_mounted for fstype_at_mount_via_fs
    //   + a NotBtrfs-style error would compile, keep parser tests green, and
    //   silently flip this case to suspend-blocked. The sibling
    //   idle_when_pool_offline only covers the unmounted case (fstype None),
    //   which never exercises this branch.
    // Scenario: a misconfiguration mounts ext4 at /mnt/storage; autosuspend must
    //   still be allowed because the encrypted btrfs pool is offline.
    #[test]
    fn pool_offline_when_non_btrfs_at_mount_point() {
        let runner = MockRunner::default();
        let fs = IdleMockFs::non_btrfs_target();
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

    // Intent: ScrubState::Never through cmd_idle yields Idle.
    // Why it exists: Pins the cmd_idle wiring from the parser-side
    //   ScrubState variant to IdleResult::Idle. The match in cmd_idle
    //   groups Never/Finished/Aborted/Interrupted into a single arm;
    //   the parser tests only prove ScrubState classification, not
    //   this wiring. A refactor that moves a variant into the Unknown
    //   arm compiles cleanly and parser tests stay green, but
    //   autosuspend silently stops working.
    // Scenario: Freshly-created pool that has never been scrubbed.
    #[test]
    fn idle_when_scrub_never() {
        let (scrub_req, scrub_out) = scrub_status_never();
        let runner = MockRunner::default().with_output(scrub_req, scrub_out);
        let fs = IdleMockFs::with_exclop("none");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Idle);
    }

    // Intent: ScrubState::Aborted through cmd_idle yields Idle.
    // Why it exists: Pins the cmd_idle wiring from the parser-side
    //   ScrubState variant to IdleResult::Idle. The match in cmd_idle
    //   groups Never/Finished/Aborted/Interrupted into a single arm;
    //   the parser tests only prove ScrubState classification, not
    //   this wiring. A refactor that moves a variant into the Unknown
    //   arm compiles cleanly and parser tests stay green, but
    //   autosuspend silently stops working.
    // Scenario: `braid lock` cancelled a scrub, leaving resumable progress
    //   on disk, and sysfs is quiet.
    #[test]
    fn idle_when_scrub_aborted() {
        let (scrub_req, scrub_out) = scrub_status_aborted();
        let runner = MockRunner::default().with_output(scrub_req, scrub_out);
        let fs = IdleMockFs::with_exclop("none");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Idle);
    }

    // Intent: ScrubState::Interrupted through cmd_idle yields Idle.
    // Why it exists: Pins the cmd_idle wiring from the parser-side
    //   ScrubState variant to IdleResult::Idle. The match in cmd_idle
    //   groups Never/Finished/Aborted/Interrupted into a single arm;
    //   the parser tests only prove ScrubState classification, not
    //   this wiring. A refactor that moves a variant into the Unknown
    //   arm compiles cleanly and parser tests stay green, but
    //   autosuspend silently stops working.
    // Scenario: Userspace scrub process died before completing, and sysfs
    //   is quiet.
    #[test]
    fn idle_when_scrub_interrupted() {
        let (scrub_req, scrub_out) = scrub_status_interrupted();
        let runner = MockRunner::default().with_output(scrub_req, scrub_out);
        let fs = IdleMockFs::with_exclop("none");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Idle);
    }

    // Intent: Scrub running -> Busy with percentage from subprocess parser.
    // Why: Scrub is not in the kernel exclop set; only `btrfs scrub status`
    //   sees it. Suspending mid-scrub interrupts data integrity verification.
    // Scenario: Monthly auto-scrub is in progress when autosuspend checks.
    // Pre-condition: sysfs is seeded clean so the scrub probe is actually
    //   reached -- the sysfs-first order is exercised by
    //   `busy_exclop_short_circuits_scrub_probe`.
    #[test]
    fn busy_when_scrub_running() {
        let (scrub_req, scrub_out) = idle_scrub_running();
        let runner = MockRunner::default().with_output(scrub_req, scrub_out);
        let fs = IdleMockFs::with_exclop("none");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(
            result,
            IdleResult::Busy(BusyReason::ScrubRunning { pct: Some(45) })
        );
    }

    // Intent: a running scrub whose byte counters are absent maps to
    //   Busy(ScrubRunning { pct: None }), never Idle.
    // Why it exists: the sibling busy_when_scrub_running only pins the
    //   both-counters-present case (pct: Some). The Busy decision sits
    //   outside the (bytes_scrubbed, total_bytes) match, but no cmd_idle
    //   test pins that for pct: None. A refactor folding the Busy/Idle
    //   choice into the pct match -- returning Idle when the percentage
    //   cannot be computed -- would compile, keep parser tests green (they
    //   classify ScrubState, not IdleResult), keep busy_when_scrub_running
    //   green, and silently allow suspend whenever pct is unknowable. Same
    //   wiring-pin contract as idle_when_scrub_{never,aborted,interrupted}.
    // Scenario: btrfs-progs output drift (parser-compatibility risk) keeps
    //   `Status: running` but reshapes/omits the `Total to scrub` /
    //   `Bytes scrubbed` lines braid parses; the parser tolerates this
    //   sparse record (scrub_running_minimal), pct is unknowable, and the
    //   gate must still block suspend.
    #[test]
    fn busy_when_scrub_running_no_bytes() {
        let (scrub_req, scrub_out) = idle_scrub_running_no_bytes();
        let runner = MockRunner::default().with_output(scrub_req, scrub_out);
        let fs = IdleMockFs::with_exclop("none");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(
            result,
            IdleResult::Busy(BusyReason::ScrubRunning { pct: None })
        );
    }

    // Intent: BusyReason Display pins the exact stdout suffixes for `braid idle`.
    // Why: The refactor shares exclop identity with preflight, but idle has
    //   its own human-facing text for balance and paused balance.
    // Scenario: Autosuspend logs and shell scripts continue seeing the same
    //   `busy: ...` lines while the internal BusyReason representation changes.
    #[test]
    fn busy_reason_display_pins_cli_strings() {
        let cases = [
            (
                BusyReason::ScrubRunning { pct: Some(45) },
                "scrub running (45%)",
            ),
            (BusyReason::ScrubRunning { pct: None }, "scrub running"),
            (BusyReason::Exclop(ExclusiveOp::Balance), "balance running"),
            (
                BusyReason::Exclop(ExclusiveOp::BalancePaused),
                "balance paused",
            ),
            (
                BusyReason::Exclop(ExclusiveOp::DeviceAdd),
                "device add in progress",
            ),
            (
                BusyReason::Exclop(ExclusiveOp::DeviceRemove),
                "device remove in progress",
            ),
            (
                BusyReason::Exclop(ExclusiveOp::DeviceReplace),
                "device replace in progress",
            ),
            (
                BusyReason::Exclop(ExclusiveOp::Resize),
                "resize in progress",
            ),
            (
                BusyReason::Exclop(ExclusiveOp::SwapActivate),
                "swap activate in progress",
            ),
            (
                BusyReason::Unknown("sysfs: simulated failure".into()),
                "unknown (sysfs: simulated failure)",
            ),
        ];

        for (reason, expected) in cases {
            assert_eq!(reason.to_string(), expected);
        }
    }

    // Intent: every kernel exclop string is reported as the matching
    //   BusyReason::Exclop and short-circuits the scrub-status subprocess.
    // Why it exists: pins two contracts at once. (1) Coverage for the
    //   post-refactor exclop surface -- before the sysfs scan, only
    //   `balance` / `balance paused` were detected and the other five were
    //   silently reported as idle. (2) The sysfs-before-scrub ordering
    //   matters operationally: each spurious scrub spawn is a fork/exec
    //   on the autosuspend timer. Using MockRunner::default() (no scrub
    //   seed) makes any regression that pre-spawns the scrub probe fail
    //   loudly as Busy::Unknown via MissingMock, in addition to the
    //   explicit `runner.requests().is_empty()` check.
    // Scenario: Operator runs `btrfs device remove` directly on the pool;
    //   `braid idle` must report busy without spending a subprocess on
    //   `btrfs scrub status`.
    #[test]
    fn busy_exclop_short_circuits_scrub_probe() {
        let cases = [
            ("balance", ExclusiveOp::Balance),
            ("balance paused", ExclusiveOp::BalancePaused),
            ("device add", ExclusiveOp::DeviceAdd),
            ("device remove", ExclusiveOp::DeviceRemove),
            ("device replace", ExclusiveOp::DeviceReplace),
            ("resize", ExclusiveOp::Resize),
            ("swap activate", ExclusiveOp::SwapActivate),
        ];

        for (exclop, expected) in cases {
            let runner = MockRunner::default();
            let fs = IdleMockFs::with_exclop(exclop);

            let result = cmd_idle(&runner, &fs, &idle_mp());
            assert_eq!(
                result,
                IdleResult::Busy(BusyReason::Exclop(expected)),
                "exclop={exclop:?}",
            );
            assert!(
                runner.requests().is_empty(),
                "exclop={exclop:?}, requests={:?}",
                runner.requests(),
            );
        }
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
        assert_idle_busy_unknown_prefix(result, "sysfs:");
        assert!(runner.requests().is_empty(), "{:?}", runner.requests());
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
        assert_idle_busy_unknown_prefix(result, "sysfs:");
        assert!(runner.requests().is_empty(), "{:?}", runner.requests());
    }

    // Intent: `cmd_idle` must NOT call `BtrfsBalanceStatus`,
    //   `BtrfsReplaceStatus`, or `BtrfsFilesystemShow`.
    //   Those subprocess probes were removed in favor of the sysfs scan.
    // Why: Pins the contract that the refactor preserves by asserting the
    //   exact recorded request log -- the only `CmdRequest` `cmd_idle` may
    //   issue is `BtrfsScrubStatus`. Re-introducing any other subprocess
    //   probe fails this assertion directly, naming the offending request,
    //   independent of how the runner happens to handle unmocked calls.
    // Scenario: Future change accidentally re-introduces a subprocess
    //   probe; this test catches it before merge.
    #[test]
    fn no_balance_or_replace_subprocess_calls() {
        let (runner, fs) = idle_ready_for_sysfs_check("none");
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::BtrfsScrubStatus {
                mount_point: idle_mp()
            }]
        );
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
        assert_idle_busy_unknown_prefix(result, "scrub:");
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::BtrfsScrubStatus {
                mount_point: idle_mp()
            }]
        );
    }

    // Intent: A non-zero `btrfs scrub status` result returned by the runner
    //   is attributed to the scrub parser layer after a clean sysfs scan.
    // Why: Command invocation failures and parse-time command failures share
    //   the same fail-closed user-facing source label.
    // Scenario: `btrfs scrub status` exits non-zero and preserves stderr for
    //   the operator while `braid idle` blocks autosuspend.
    #[test]
    fn busy_unknown_on_scrub_parse_failure() {
        let request = CmdRequest::BtrfsScrubStatus {
            mount_point: idle_mp(),
        };
        let runner = MockRunner::default().with_output(
            request.clone(),
            RawCommandOutput {
                cmd: "btrfs scrub status --raw /mnt/storage".into(),
                stdout: String::new(),
                stderr: "simulated scrub status failure\n".into(),
                exit_status: 1,
            },
        );
        let fs = IdleMockFs::with_exclop("none");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_idle_busy_unknown_prefix(result, "scrub:");
        assert_eq!(runner.requests(), vec![request]);
    }

    // Intent: a parser result of `Ok(state: ScrubState::Unknown)` after a
    //   clean (zero-exit) scrub-status invocation must surface as
    //   Busy::Unknown, not Idle.
    // Why it exists: closes the last fail-open branch in the autosuspend
    //   gate. The parser-Err path is covered by
    //   busy_unknown_on_scrub_parse_failure; this test pins the
    //   parser-Ok-but-Unknown path that the previous non-exhaustive scrub
    //   state check silently treated as idle. Same fail-closed contract the
    //   sysfs branch and scrub_needs_resume.rs already obey.
    // Scenario: btrfs-progs upgrade reshapes the `Status:` line (or
    //   stdout is empty); parse_btrfs_scrub_status returns
    //   Ok(BtrfsScrubStatusOutput { state: ScrubState::Unknown }).
    #[test]
    fn busy_unknown_on_scrub_state_unknown() {
        let (scrub_req, scrub_out) = scrub_status_unknown();
        let runner = MockRunner::default().with_output(scrub_req.clone(), scrub_out);
        let fs = IdleMockFs::with_exclop("none");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_idle_busy_unknown_prefix(result, "scrub:");
        assert_eq!(runner.requests(), vec![scrub_req]);
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
        assert_idle_busy_unknown_prefix(result, "mountinfo:");
        assert!(runner.requests().is_empty(), "{:?}", runner.requests());
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
        assert_idle_busy_unknown_prefix(result, "mountinfo:");
        assert!(runner.requests().is_empty(), "{:?}", runner.requests());
    }

    // Intent: a `/proc/self/mountinfo` body with two entries for the
    //   configured target must propagate as Busy::Unknown, not silently
    //   become PoolOffline or be resolved by picking one entry.
    // Why it exists: ADR 016 and idle.md both name "ambiguous duplicate
    //   target entries" as a suspend-blocking mountinfo error.
    //   DuplicateTarget is the one mountinfo anomaly with a distinct code
    //   path (two parse-clean matches, not zero/garbage), so the Io and
    //   Malformed siblings above do not stand in for it. A refactor scoped
    //   to is_btrfs_mounted or this match arm that mapped DuplicateTarget
    //   to "not mounted" (e.g. "pick the first" / "an overmount means
    //   offline") would compile, keep every parser test and both sibling
    //   cmd_idle mountinfo tests green, and silently flip a documented
    //   block-suspend case to allow-suspend.
    // Scenario: an overmount or rebind landed a second mount at
    //   /mnt/storage alongside the pool; autosuspend must refuse to guess
    //   and block.
    #[test]
    fn mountinfo_duplicate_target_is_busy_unknown() {
        let runner = MockRunner::default();
        let fs = IdleMockFs::with_mountinfo(
            "36 35 0:32 / /mnt/storage rw,noatime shared:1 - btrfs /dev/mapper/braid-disk1 rw\n\
             37 35 0:33 / /mnt/storage rw,noatime shared:1 - btrfs /dev/mapper/braid-disk2 rw\n",
        );
        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_idle_busy_unknown_prefix(result, "mountinfo:");
        assert!(runner.requests().is_empty(), "{:?}", runner.requests());
    }

    /* Intent: `/sys/fs/btrfs/` entries named `features` or `debug` are
     *   skipped by name -- the helper never even attempts to read their
     *   `exclusive_operation` (which the kernel does not create for
     *   them; see `reference/linux/fs/btrfs/sysfs.c`, whose sysfs path
     *   table lists `features`/`debug` as the only non-`<uuid>` entries).
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
        assert_idle_busy_unknown_prefix(result, "sysfs:");
        assert!(runner.requests().is_empty(), "{:?}", runner.requests());
    }

    /* Intent: when the host has multiple btrfs filesystems, a busy
     *   non-pool fsid blocks suspend even when the pool fsid is idle.
     * Why: `cmd_idle` no longer does mount->fsid resolution. The trade
     *   is conservative-by-design: a busy non-pool btrfs (e.g. root)
     *   keeps the system awake. A future "scope to pool fsid" change
     *   would read only IDLE_FSID, see `none`, and silently return
     *   Idle -- defeating the host-wide rule. Pinning the busy state
     *   on IDLE_FSID_OTHER (and the idle state on IDLE_FSID) makes
     *   that regression fail this test.
     * Scenario: NixOS host with btrfs root and a braid pool; the pool
     *   is idle, root is mid-balance. autosuspend must see Busy and
     *   the `busy:` line names the non-pool op.
     */
    #[test]
    fn idle_any_busy_blocks_suspend_multi_btrfs() {
        let runner = idle_runner_with_scrub_finished();
        // Pool fsid (IDLE_FSID) is idle; non-pool fsid (IDLE_FSID_OTHER)
        // is balancing. List order puts the pool first so the loop must
        // continue past it to find Busy on the second entry. A future
        // change that scoped reads to only the pool fsid would read
        // IDLE_FSID, see `none`, and return Idle -- failing this test.
        let fs = IdleMockFs::mounted_btrfs_only()
            .seed_btrfs_listing(&[IDLE_FSID, IDLE_FSID_OTHER])
            .seed_exclop(IDLE_FSID, "none")
            .seed_exclop(IDLE_FSID_OTHER, "balance");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(
            result,
            IdleResult::Busy(BusyReason::Exclop(ExclusiveOp::Balance))
        );
    }

    // Intent: when the host exposes multiple btrfs filesystems that are all
    //   idle, `cmd_idle` issues exactly one `btrfs scrub status` probe, scoped
    //   to the configured pool mount point -- never one probe per fsid.
    // Why it exists: the scrub probe is deliberately pool-scoped, not host-wide
    //   (ADR 016, "Scrub probe is scoped to the pool mount point"): a scrub on a
    //   non-pool btrfs is not detected and does not block suspend. The sibling
    //   exclop rule (host-wide) is pinned by
    //   `idle_any_busy_blocks_suspend_multi_btrfs`; this is its scrub-side
    //   mirror. Every other test that reaches the scrub probe seeds a single
    //   fsid, and every existing multi-fsid test short-circuits at the sysfs
    //   scan before scrub is reached -- so a future change that made scrub
    //   host-wide (a probe per fsid), or scoped it to the wrong mount point,
    //   would compile and keep all current idle tests green while silently
    //   changing the documented suspend behavior. Asserting the exact request
    //   log -- one `BtrfsScrubStatus` keyed to `idle_mp()` -- fails closed on
    //   both regressions: MockRunner records every request before dispatch, so
    //   a second per-fsid probe lands in the log even when unmocked.
    // Scenario: NixOS host with a btrfs root alongside the braid pool; both are
    //   idle. autosuspend must conclude Idle after a single pool-scoped scrub
    //   probe, ignoring the non-pool filesystem entirely.
    #[test]
    fn idle_scrub_probe_stays_pool_scoped_multi_btrfs() {
        let runner = idle_runner_with_scrub_finished();
        let fs = IdleMockFs::mounted_btrfs_only()
            .seed_btrfs_listing(&[IDLE_FSID, IDLE_FSID_OTHER])
            .seed_exclop(IDLE_FSID, "none")
            .seed_exclop(IDLE_FSID_OTHER, "none");

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_eq!(result, IdleResult::Idle);
        assert_eq!(
            runner.requests(),
            vec![CmdRequest::BtrfsScrubStatus {
                mount_point: idle_mp()
            }]
        );
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
        assert_idle_busy_unknown_prefix(result, "sysfs:");
        assert!(runner.requests().is_empty(), "{:?}", runner.requests());
    }

    /* Intent: a `list_dir("/sys/fs/btrfs")` IO failure (e.g.
     *   PermissionDenied, EIO) must propagate as Busy::Unknown, not
     *   become Idle.
     * Why: without this, a future change could conflate "scan failed"
     *   with "no busy entries found" and reintroduce the fail-open seam
     *   the autosuspend gate exists to prevent. NotFound is excluded
     *   here because RealFilesystem::list_dir folds NotFound into
     *   Ok(vec![]), which the empty-listing test above covers separately.
     * Scenario: sysfs is mounted but `/sys/fs/btrfs` is unreadable
     *   under our credentials.
     */
    #[test]
    fn idle_list_dir_io_error_is_fail_closed() {
        let runner = idle_runner_with_scrub_finished();
        let fs =
            IdleMockFs::mounted_btrfs_only().seed_btrfs_listing_error(ErrorKind::PermissionDenied);

        let result = cmd_idle(&runner, &fs, &idle_mp());
        assert_idle_busy_unknown_prefix(result, "sysfs:");
        assert!(runner.requests().is_empty(), "{:?}", runner.requests());
    }
}
