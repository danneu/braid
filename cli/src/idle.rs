use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::parse::{
    parse_btrfs_balance_status, parse_btrfs_replace_status, parse_btrfs_scrub_status,
    parse_findmnt_json, BalanceState, ParseError, ReplaceState, ScrubState,
};
use crate::progress::pct_from_bytes;
use crate::types::MountPoint;

#[derive(Debug, PartialEq)]
pub enum IdleResult {
    /// Pool is idle — no exclusive operations running.
    Idle,
    /// Pool not mounted — nothing to protect — allow suspend.
    PoolOffline,
    Busy(BusyReason),
}

#[derive(Debug, PartialEq)]
pub enum BusyReason {
    ScrubRunning { pct: Option<u8> },
    BalanceRunning { pct_left: u8 },
    BalancePaused { pct_left: u8 },
    ReplaceRunning { pct: f64 },
}

impl std::fmt::Display for BusyReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BusyReason::ScrubRunning { pct: Some(p) } => write!(f, "scrub running ({p}%)"),
            BusyReason::ScrubRunning { pct: None } => write!(f, "scrub running"),
            BusyReason::BalanceRunning { pct_left } => {
                write!(f, "balance running ({pct_left}% left)")
            }
            BusyReason::BalancePaused { pct_left } => {
                write!(f, "balance paused ({pct_left}% left)")
            }
            BusyReason::ReplaceRunning { pct } => write!(f, "replace running ({pct:.1}%)"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum IdleError {
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
}

pub fn cmd_idle<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<IdleResult, IdleError> {
    // 1. Check if pool is mounted
    if !is_btrfs_mounted(runner, mount_point)? {
        return Ok(IdleResult::PoolOffline);
    }

    // 2. Check scrub
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

    // 3. Check balance / device-remove
    let balance_raw = runner.run(&CmdRequest::BtrfsBalanceStatus {
        mount_point: mount_point.clone(),
    })?;
    let balance = parse_btrfs_balance_status(&balance_raw)?;
    match balance.state {
        BalanceState::Running { pct_left, .. } => {
            return Ok(IdleResult::Busy(BusyReason::BalanceRunning { pct_left }));
        }
        BalanceState::Paused { pct_left, .. } => {
            return Ok(IdleResult::Busy(BusyReason::BalancePaused { pct_left }));
        }
        BalanceState::None => {}
    }

    // 4. Check replace
    let replace_raw = runner.run(&CmdRequest::BtrfsReplaceStatus {
        mount_point: mount_point.clone(),
    })?;
    let replace = parse_btrfs_replace_status(&replace_raw)?;
    if let ReplaceState::Running { pct } = replace.state {
        return Ok(IdleResult::Busy(BusyReason::ReplaceRunning { pct }));
    }

    Ok(IdleResult::Idle)
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

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
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

    fn balance_none() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsBalanceStatus { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs balance status /mnt/storage".into(),
                stdout: "No balance found on '/mnt/storage'\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn balance_running(pct_left: u8) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsBalanceStatus { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs balance status /mnt/storage".into(),
                stdout: format!(
                    "Balance on '/mnt/storage' is running\n\
                     3 out of about 10 chunks balanced (7 considered), {pct_left}% left\n"
                ),
                stderr: String::new(),
                exit_status: 1,
            },
        )
    }

    fn balance_paused(pct_left: u8) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsBalanceStatus { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs balance status /mnt/storage".into(),
                stdout: format!(
                    "Balance on '/mnt/storage' is paused\n\
                     5 out of about 12 chunks balanced (8 considered), {pct_left}% left\n"
                ),
                stderr: String::new(),
                exit_status: 1,
            },
        )
    }

    fn replace_none() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsReplaceStatus { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs replace status /mnt/storage".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn replace_running(pct: f64) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsReplaceStatus {
                mount_point: mp(),
            },
            RawCommandOutput {
                cmd: "btrfs replace status /mnt/storage".into(),
                stdout: format!(
                    "Started on  1.Jan 00:00:00, pid = 1234, {pct:.1}% done, 0 write errs, 0 uncorr. read errs\n"
                ),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    #[test]
    // Intent: Pool not mounted → PoolOffline (idle).
    // Why: If the pool is offline, there's nothing to protect — allow suspend.
    // Scenario: NAS has not been unlocked yet; autosuspend checks idle state.
    fn idle_when_pool_offline() {
        let (req, out) = findmnt_not_mounted();
        let runner = MockRunner::default().with_output(req, out);
        let result = cmd_idle(&runner, &mp()).unwrap();
        assert_eq!(result, IdleResult::PoolOffline);
    }

    #[test]
    // Intent: Pool mounted, no ops running → Idle.
    // Why: The normal idle state — system should be allowed to suspend.
    // Scenario: NAS pool is online but no user activity or maintenance in progress.
    fn idle_when_all_ops_quiet() {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        let (scrub_req, scrub_out) = scrub_completed();
        let (bal_req, bal_out) = balance_none();
        let (rep_req, rep_out) = replace_none();

        let runner = MockRunner::default()
            .with_output(fmnt_req, fmnt_out)
            .with_output(scrub_req, scrub_out)
            .with_output(bal_req, bal_out)
            .with_output(rep_req, rep_out);

        let result = cmd_idle(&runner, &mp()).unwrap();
        assert_eq!(result, IdleResult::Idle);
    }

    #[test]
    // Intent: Scrub running → Busy.
    // Why: Suspending during a scrub would interrupt data integrity verification.
    // Scenario: Monthly auto-scrub is in progress when autosuspend checks idle state.
    fn busy_when_scrub_running() {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        let (scrub_req, scrub_out) = scrub_running(45);

        let runner = MockRunner::default()
            .with_output(fmnt_req, fmnt_out)
            .with_output(scrub_req, scrub_out);

        let result = cmd_idle(&runner, &mp()).unwrap();
        assert_eq!(
            result,
            IdleResult::Busy(BusyReason::ScrubRunning { pct: Some(45) })
        );
    }

    #[test]
    // Intent: Balance running → Busy.
    // Why: Suspending during a balance risks inconsistent chunk allocation.
    // Scenario: User ran `braid add` which triggers a RAID1 balance; system tries to sleep.
    fn busy_when_balance_running() {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        let (scrub_req, scrub_out) = scrub_completed();
        let (bal_req, bal_out) = balance_running(70);

        let runner = MockRunner::default()
            .with_output(fmnt_req, fmnt_out)
            .with_output(scrub_req, scrub_out)
            .with_output(bal_req, bal_out);

        let result = cmd_idle(&runner, &mp()).unwrap();
        assert_eq!(
            result,
            IdleResult::Busy(BusyReason::BalanceRunning { pct_left: 70 })
        );
    }

    #[test]
    // Intent: Balance paused → Busy.
    // Why: A paused balance holds the btrfs exclusive op lock. Suspending
    //   mid-pause leaves the pool in an intermediate state that needs human action.
    // Scenario: User paused a balance and forgot; system should not sleep.
    fn busy_when_balance_paused() {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        let (scrub_req, scrub_out) = scrub_completed();
        let (bal_req, bal_out) = balance_paused(58);

        let runner = MockRunner::default()
            .with_output(fmnt_req, fmnt_out)
            .with_output(scrub_req, scrub_out)
            .with_output(bal_req, bal_out);

        let result = cmd_idle(&runner, &mp()).unwrap();
        assert_eq!(
            result,
            IdleResult::Busy(BusyReason::BalancePaused { pct_left: 58 })
        );
    }

    #[test]
    // Intent: Replace running → Busy.
    // Why: Suspending during a disk replacement risks incomplete data migration.
    // Scenario: User ran `braid replace` and the operation is in progress.
    fn busy_when_replace_running() {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        let (scrub_req, scrub_out) = scrub_completed();
        let (bal_req, bal_out) = balance_none();
        let (rep_req, rep_out) = replace_running(45.3);

        let runner = MockRunner::default()
            .with_output(fmnt_req, fmnt_out)
            .with_output(scrub_req, scrub_out)
            .with_output(bal_req, bal_out)
            .with_output(rep_req, rep_out);

        let result = cmd_idle(&runner, &mp()).unwrap();
        assert_eq!(
            result,
            IdleResult::Busy(BusyReason::ReplaceRunning { pct: 45.3 })
        );
    }

    #[test]
    // Intent: If any status probe fails, return an error (fail-closed).
    // Why: If we can't determine whether an op is running, we must not allow
    //   suspend — the safe default is to block it.
    // Scenario: btrfs scrub status command fails due to kernel bug or permissions.
    fn error_on_probe_failure() {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        // No scrub mock → MissingMock error when scrub is queried
        let runner = MockRunner::default().with_output(fmnt_req, fmnt_out);

        let result = cmd_idle(&runner, &mp());
        assert!(result.is_err());
    }

    #[test]
    // Intent: Short-circuit on first busy — don't query balance/replace if scrub
    //   is already running.
    // Why: Validates that the check order is scrub→balance→replace and that
    //   detection of the first busy condition skips unnecessary probes.
    // Scenario: Scrub is running; no mocks for balance or replace are seeded.
    //   If cmd_idle queries them, MockRunner will panic with MissingMock.
    fn short_circuits_on_first_busy() {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        let (scrub_req, scrub_out) = scrub_running(10);
        // No balance or replace mocks — proves short-circuit
        let runner = MockRunner::default()
            .with_output(fmnt_req, fmnt_out)
            .with_output(scrub_req, scrub_out);

        let result = cmd_idle(&runner, &mp()).unwrap();
        assert_eq!(
            result,
            IdleResult::Busy(BusyReason::ScrubRunning { pct: Some(10) })
        );
    }

    #[test]
    // Intent: replace status command fails → cmd_idle returns
    //   IdleError::Parse(CommandFailed), not Idle.
    // Why: a failed status check must not be mistaken for "no replace running" —
    //   that would allow autosuspend during an active replace.
    // Scenario: typo in mount path causes btrfs replace status to exit non-zero.
    fn replace_status_failure_is_not_idle() {
        let (fmnt_req, fmnt_out) = findmnt_mounted();
        let (scrub_req, scrub_out) = scrub_completed();
        let (bal_req, bal_out) = balance_none();

        let runner = MockRunner::default()
            .with_output(fmnt_req, fmnt_out)
            .with_output(scrub_req, scrub_out)
            .with_output(bal_req, bal_out)
            .with_output(
                CmdRequest::BtrfsReplaceStatus { mount_point: mp() },
                RawCommandOutput {
                    cmd: "btrfs replace status /mnt/storage".into(),
                    stdout: String::new(),
                    stderr: "ERROR: not a btrfs filesystem".into(),
                    exit_status: 1,
                },
            );

        let result = cmd_idle(&runner, &mp());
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                IdleError::Parse(ParseError::CommandFailed { exit_code: 1, .. })
            ),
            "expected IdleError::Parse(CommandFailed {{ exit_code: 1 }}), got {err:?}"
        );
    }
}
