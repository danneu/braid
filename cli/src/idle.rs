use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::parse::{ParseError, ScrubState, parse_btrfs_scrub_status, parse_findmnt_json};
use crate::preflight::{ExclusiveOp, ExclusiveOpError, check_no_exclusive_op};
use crate::probe::{Filesystem, ProbeError, probe_fsid};
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

#[derive(Debug, thiserror::Error)]
pub enum IdleError {
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    /// Wraps the non-`Busy` variants of `ExclusiveOpError` (sysfs read
    /// failure, unrecognized value). `ExclusiveOpError::Busy` is the
    /// success signal for a running exclusive op and never reaches here.
    #[error("exclusive-op check error: {0}")]
    Exclop(String),
}

pub fn cmd_idle<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
) -> Result<IdleResult, IdleError> {
    // 1. Pool offline -- nothing to protect.
    if !is_btrfs_mounted(runner, mount_point)? {
        return Ok(IdleResult::PoolOffline);
    }

    // 2. Scrub via subprocess (scrub is not in the kernel exclop set, so
    //    sysfs cannot see it). Done before fsid lookup so the common
    //    "scrub in progress" case short-circuits the extra probes.
    let scrub_raw = runner.run(&CmdRequest::BtrfsScrubStatus {
        mount_point: mount_point.clone(),
    })?;
    let scrub = parse_btrfs_scrub_status(&scrub_raw)?;
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
        return Ok(IdleResult::Busy(BusyReason::ScrubRunning { pct }));
    }

    // 3. Every other exclusive operation comes from one sysfs read.
    //    Same source preflight.rs uses for mutating commands, so the two
    //    code paths cannot disagree about what counts as "busy."
    let fsid = probe_fsid(runner, mount_point)?;
    match check_no_exclusive_op(fs, &fsid) {
        Ok(()) => Ok(IdleResult::Idle),
        Err(ExclusiveOpError::Busy(op)) => Ok(IdleResult::Busy(busy_from_exclop(op))),
        Err(e @ (ExclusiveOpError::Read(_) | ExclusiveOpError::Unrecognized(_))) => {
            Err(IdleError::Exclop(e.to_string()))
        }
    }
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

/// Check whether the mount point is a mounted btrfs filesystem.
/// Returns false if not mounted or not btrfs.
fn is_btrfs_mounted<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<bool, IdleError> {
    let findmnt_raw = runner.run(&CmdRequest::FindmntJson {
        mount_point: mount_point.clone(),
    })?;
    let findmnt = parse_findmnt_json(&findmnt_raw)?;
    let entry = findmnt
        .filesystems
        .iter()
        .find(|e| e.target == mount_point.as_str());
    match entry {
        None => Ok(false),
        Some(e) => Ok(e.fstype == "btrfs"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};

    const FSID: &str = "12345678-1234-1234-1234-123456789abc";

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
    }

    fn exclop_path() -> String {
        format!("/sys/fs/btrfs/{FSID}/exclusive_operation")
    }

    /// MockFs that serves *only* the exact exclop path derived from the
    /// seeded fsid. Any other read returns NotFound, so a test fails if
    /// `cmd_idle` reads the wrong fsid or the wrong file -- the whole
    /// point of this refactor.
    struct MockFs {
        expected_path: String,
        body: Option<String>,
    }

    impl MockFs {
        fn with_exclop(body: &str) -> Self {
            Self {
                expected_path: exclop_path(),
                body: Some(format!("{body}\n")),
            }
        }

        /// Configure the sysfs read to fail (simulates a missing/locked
        /// file or kernel that doesn't expose the attribute).
        fn with_read_error() -> Self {
            Self {
                expected_path: exclop_path(),
                body: None,
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
            if path == self.expected_path {
                match &self.body {
                    Some(b) => Ok(b.clone()),
                    None => Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "mock read error",
                    )),
                }
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("MockFs: unexpected path {path}"),
                ))
            }
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    fn findmnt_json(mounted: bool) -> RawCommandOutput {
        let stdout = if mounted {
            r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs","options":"rw,noatime"}]}"#
        } else {
            r#"{"filesystems":[]}"#
        };
        RawCommandOutput {
            cmd: "findmnt --json --output TARGET,SOURCE,FSTYPE,OPTIONS --mountpoint /mnt/storage"
                .into(),
            stdout: stdout.into(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn findmnt_mounted() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::FindmntJson { mount_point: mp() },
            findmnt_json(true),
        )
    }

    fn findmnt_not_mounted() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::FindmntJson { mount_point: mp() },
            findmnt_json(false),
        )
    }

    fn btrfs_show() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsFilesystemShow { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs filesystem show /mnt/storage".into(),
                stdout: format!(
                    "Label: none  uuid: {FSID}\n\
                     \tTotal devices 2 FS bytes used 16.00MiB\n\
                     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-aaa\n\
                     \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-bbb\n"
                ),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn scrub_completed() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsScrubStatus { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub status --raw /mnt/storage".into(),
                stdout: "UUID:             12345678-1234-1234-1234-123456789abc\n\
                         Scrub started:    Mon Jan  1 00:00:00 2024\n\
                         Status:           finished\n\
                         Duration:         0:00:01\n\
                         Total to scrub:   1073741824\n\
                         Rate:             1073741824/s\n\
                         Error summary:    no errors found\n"
                    .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn scrub_running(pct: u8) -> (CmdRequest, RawCommandOutput) {
        let total: u64 = 30408704000;
        let scrubbed = total * u64::from(pct) / 100;
        (
            CmdRequest::BtrfsScrubStatus { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub status --raw /mnt/storage".into(),
                stdout: format!(
                    "UUID:             12345678-1234-1234-1234-123456789abc\n\
                     Scrub started:    Mon Jan  1 00:00:00 2024\n\
                     Status:           running\n\
                     Duration:         0:00:05\n\
                     Total to scrub:   {total}\n\
                     Bytes scrubbed:   {scrubbed}  ({pct}.00%)\n\
                     Rate:             2952790016/s\n\
                     Error summary:    no errors found\n"
                ),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    /// Seed everything `cmd_idle` needs after the scrub probe: the second
    /// findmnt for `probe_fsid` and the `btrfs filesystem show` that
    /// returns `FSID`.
    fn seed_fsid_probe(runner: MockRunner) -> MockRunner {
        let (show_req, show_out) = btrfs_show();
        // probe_fsid runs FindmntJson again. The MockRunner serves the
        // same cached entry we already registered for is_btrfs_mounted,
        // so we just add the BtrfsFilesystemShow mock.
        runner.with_output(show_req, show_out)
    }

    // Intent: Pool not mounted -> PoolOffline (idle).
    // Why: If the pool is offline, there's nothing to protect -- allow suspend.
    // Scenario: NAS has not been unlocked yet; autosuspend checks idle state.
    #[test]
    fn idle_when_pool_offline() {
        let (req, out) = findmnt_not_mounted();
        let runner = MockRunner::default().with_output(req, out);
        let fs = MockFs::with_exclop("none");
        let result = cmd_idle(&runner, &fs, &mp()).unwrap();
        assert_eq!(result, IdleResult::PoolOffline);
    }

    // Intent: Pool mounted, sysfs reports `none`, scrub idle -> Idle.
    // Why: The normal idle state -- system should be allowed to suspend.
    // Scenario: NAS pool is online but no user activity or maintenance in progress.
    #[test]
    fn idle_when_all_ops_quiet() {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        let (scrub_req, scrub_out) = scrub_completed();

        let runner = seed_fsid_probe(
            MockRunner::default()
                .with_output(fmnt_req, fmnt_out)
                .with_output(scrub_req, scrub_out),
        );
        let fs = MockFs::with_exclop("none");

        let result = cmd_idle(&runner, &fs, &mp()).unwrap();
        assert_eq!(result, IdleResult::Idle);
    }

    // Intent: Scrub running -> Busy with percentage from subprocess parser.
    // Why: Scrub is not in the kernel exclop set; only `btrfs scrub status`
    //   sees it. Suspending mid-scrub interrupts data integrity verification.
    // Scenario: Monthly auto-scrub is in progress when autosuspend checks.
    #[test]
    fn busy_when_scrub_running() {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        let (scrub_req, scrub_out) = scrub_running(45);

        let runner = MockRunner::default()
            .with_output(fmnt_req, fmnt_out)
            .with_output(scrub_req, scrub_out);
        // Deliberately NOT seeding the fsid probe -- a passing test
        // proves we short-circuit before sysfs.
        let fs = MockFs::with_exclop("none");

        let result = cmd_idle(&runner, &fs, &mp()).unwrap();
        assert_eq!(
            result,
            IdleResult::Busy(BusyReason::ScrubRunning { pct: Some(45) })
        );
    }

    /// Build a runner+fs ready to drive cmd_idle to the sysfs read step
    /// (mount + scrub-clean already seeded).
    fn ready_for_sysfs_check(exclop: &str) -> (MockRunner, MockFs) {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        let (scrub_req, scrub_out) = scrub_completed();
        let runner = seed_fsid_probe(
            MockRunner::default()
                .with_output(fmnt_req, fmnt_out)
                .with_output(scrub_req, scrub_out),
        );
        let fs = MockFs::with_exclop(exclop);
        (runner, fs)
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
        let (runner, fs) = ready_for_sysfs_check("balance");
        let result = cmd_idle(&runner, &fs, &mp()).unwrap();
        assert_eq!(result, IdleResult::Busy(BusyReason::Balance));
    }

    #[test]
    fn busy_when_balance_paused() {
        let (runner, fs) = ready_for_sysfs_check("balance paused");
        let result = cmd_idle(&runner, &fs, &mp()).unwrap();
        assert_eq!(result, IdleResult::Busy(BusyReason::BalancePaused));
    }

    #[test]
    fn busy_when_device_add() {
        let (runner, fs) = ready_for_sysfs_check("device add");
        let result = cmd_idle(&runner, &fs, &mp()).unwrap();
        assert_eq!(result, IdleResult::Busy(BusyReason::DeviceAdd));
    }

    #[test]
    fn busy_when_device_remove() {
        let (runner, fs) = ready_for_sysfs_check("device remove");
        let result = cmd_idle(&runner, &fs, &mp()).unwrap();
        assert_eq!(result, IdleResult::Busy(BusyReason::DeviceRemove));
    }

    #[test]
    fn busy_when_device_replace() {
        let (runner, fs) = ready_for_sysfs_check("device replace");
        let result = cmd_idle(&runner, &fs, &mp()).unwrap();
        assert_eq!(result, IdleResult::Busy(BusyReason::DeviceReplace));
    }

    #[test]
    fn busy_when_resize() {
        let (runner, fs) = ready_for_sysfs_check("resize");
        let result = cmd_idle(&runner, &fs, &mp()).unwrap();
        assert_eq!(result, IdleResult::Busy(BusyReason::Resize));
    }

    #[test]
    fn busy_when_swap_activate() {
        let (runner, fs) = ready_for_sysfs_check("swap activate");
        let result = cmd_idle(&runner, &fs, &mp()).unwrap();
        assert_eq!(result, IdleResult::Busy(BusyReason::SwapActivate));
    }

    // Intent: Unrecognized exclop value -> IdleError::Exclop (fail-closed).
    // Why: A kernel that adds a new exclop name we have not yet mapped
    //   must not be silently treated as idle. Better to error and let
    //   autosuspend block suspend than to suspend mid-unknown-operation.
    // Scenario: New btrfs version writes a new state we do not yet handle.
    #[test]
    fn error_on_unrecognized_exclop() {
        let (runner, fs) = ready_for_sysfs_check("brand new op");
        let err = cmd_idle(&runner, &fs, &mp()).unwrap_err();
        assert!(matches!(err, IdleError::Exclop(_)), "got {err:?}");
    }

    // Intent: Sysfs read error -> IdleError::Exclop (fail-closed).
    // Why: If we cannot read the exclop file (permissions, unmount race,
    //   kernel without sysfs btrfs attrs), we must not assume idle.
    // Scenario: race between idle check and `btrfs unmount`.
    #[test]
    fn error_on_sysfs_read_failure() {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        let (scrub_req, scrub_out) = scrub_completed();
        let runner = seed_fsid_probe(
            MockRunner::default()
                .with_output(fmnt_req, fmnt_out)
                .with_output(scrub_req, scrub_out),
        );
        let fs = MockFs::with_read_error();

        let err = cmd_idle(&runner, &fs, &mp()).unwrap_err();
        assert!(matches!(err, IdleError::Exclop(_)), "got {err:?}");
    }

    // Intent: `cmd_idle` must NOT call `BtrfsBalanceStatus` or
    //   `BtrfsReplaceStatus`. Those subprocess probes were removed in
    //   favor of the sysfs read.
    // Why: Pins the contract that the refactor preserves -- a
    //   `MockRunner` with no balance/replace mocks must still let
    //   cmd_idle return successfully. Adding a new caller of those
    //   CmdRequests inside cmd_idle would surface as MissingMock here.
    // Scenario: Future change accidentally re-introduces a subprocess
    //   probe; this test catches it before merge.
    #[test]
    fn no_balance_or_replace_subprocess_calls() {
        let (runner, fs) = ready_for_sysfs_check("none");
        let result = cmd_idle(&runner, &fs, &mp()).unwrap();
        assert_eq!(result, IdleResult::Idle);
    }

    // Intent: If the scrub probe itself fails, return an error
    //   (fail-closed). Same shape as the legacy test it replaces.
    // Why: If we cannot determine whether a scrub is running, we must
    //   not allow suspend.
    // Scenario: btrfs scrub status command fails due to kernel bug or
    //   permissions.
    #[test]
    fn error_on_scrub_probe_failure() {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        // No scrub mock -> MissingMock when scrub is queried
        let runner = MockRunner::default().with_output(fmnt_req, fmnt_out);
        let fs = MockFs::with_exclop("none");

        let result = cmd_idle(&runner, &fs, &mp());
        assert!(result.is_err());
    }
}
