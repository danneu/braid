use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::parse::{parse_btrfs_scrub_status, ParseError, ScrubState};
use crate::types::MountPoint;

#[derive(Debug, PartialEq)]
pub enum ScrubCancelResult {
    /// Scrub was running and we cancelled it.
    Cancelled,
    /// Status said Running but cancel raced with completion ("not running"). Benign.
    RacedCompletion,
    /// No scrub running (Never / Completed). Nothing to do.
    NotRunning,
}

#[derive(Debug, thiserror::Error)]
pub enum ScrubCancelError {
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("btrfs scrub cancel failed: {stderr}")]
    CancelFailed { stderr: String },
    #[error(
        "btrfs scrub status returned an unclassifiable result; refusing to silently no-op a \
         shutdown-path cancel. Investigate parser drift or partial output."
    )]
    StatusUnknown,
}

/// Probe scrub state and cancel only when actually running.
///
/// Designed for the `braid-scrub.service` ExecStop hook. Replaces a brittle
/// `grep finished || btrfs scrub cancel` shell pipeline that misclassified
/// every non-`finished` state and turned the benign "not running" cancel
/// failure into an ExecStop failure.
///
/// - `Running`  → invoke `btrfs scrub cancel`. Success → `Cancelled`.
/// - `Running` → cancel exits with stderr containing `"not running"` →
///   `RacedCompletion` (the scrub completed between probe and cancel).
/// - `Never` / `Completed` → `NotRunning` (silent no-op success).
/// - `Unknown` → `StatusUnknown` error. Unknown is the parser's "couldn't
///   classify" bucket — silently succeeding here would mask parser drift
///   and leave a busy mount uncancelled. Fail loud instead.
pub fn cmd_scrub_cancel<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<ScrubCancelResult, ScrubCancelError> {
    let status_raw = runner.run(&CmdRequest::BtrfsScrubStatus {
        mount_point: mount_point.clone(),
    })?;
    let status = parse_btrfs_scrub_status(&status_raw)?;

    match status.state {
        ScrubState::Running { .. } => {
            let cancel_raw = runner.run(&CmdRequest::BtrfsScrubCancel {
                mount_point: mount_point.clone(),
            })?;
            if cancel_raw.exit_status == 0 {
                Ok(ScrubCancelResult::Cancelled)
            } else if cancel_raw.stderr.contains("not running") {
                // Race: scrub completed between probe and cancel. Treat as success.
                Ok(ScrubCancelResult::RacedCompletion)
            } else {
                Err(ScrubCancelError::CancelFailed {
                    stderr: cancel_raw.stderr,
                })
            }
        }
        ScrubState::Never | ScrubState::Completed { .. } => Ok(ScrubCancelResult::NotRunning),
        ScrubState::Unknown => {
            // Unknown is the parser's "couldn't classify" bucket — NOT evidence
            // that no scrub is running. Failing loud here surfaces parser drift
            // instead of letting it silently break the cancel path.
            Err(ScrubCancelError::StatusUnknown)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
    }

    fn scrub_status_running() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsScrubStatus { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub status --raw /mnt/storage".into(),
                stdout: "UUID:             12345678-1234-1234-1234-123456789abc\n\
                         Scrub started:    Mon Jan  1 00:00:00 2024\n\
                         Status:           running\n\
                         Duration:         0:00:05\n\
                         Total to scrub:   30408704000\n\
                         Rate:             2952790016/s\n\
                         10.00% done\n\
                         Error summary:    no errors found\n"
                    .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn scrub_status_never() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsScrubStatus { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub status --raw /mnt/storage".into(),
                stdout: "UUID:             12345678-1234-1234-1234-123456789abc\n\
                         no stats available\n"
                    .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn scrub_status_completed() -> (CmdRequest, RawCommandOutput) {
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

    fn scrub_status_unknown() -> (CmdRequest, RawCommandOutput) {
        // Empty stdout — no "no stats available", no "Status:" line. The parser
        // returns ScrubState::Unknown rather than guessing.
        (
            CmdRequest::BtrfsScrubStatus { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub status --raw /mnt/storage".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn scrub_cancel_ok() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsScrubCancel { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub cancel /mnt/storage".into(),
                stdout: "scrub cancelled\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn scrub_cancel_not_running() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsScrubCancel { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub cancel /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: scrub cancel failed on /mnt/storage: not running\n".into(),
                exit_status: 1,
            },
        )
    }

    fn scrub_cancel_real_failure() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsScrubCancel { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub cancel /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: permission denied\n".into(),
                exit_status: 1,
            },
        )
    }

    #[test]
    // Intent: status==Running → BtrfsScrubCancel is invoked and Cancelled is
    //   returned.
    // Why it exists: Failure-layer guard. If a future refactor stops issuing the
    //   cancel request when scrub is running, MockRunner returns MissingMock and
    //   this test fails — proving the cancel call site is exercised.
    // Scenario: braid-scrub.service ExecStop fires while a scrub is mid-run.
    fn running_invokes_cancel() {
        let (status_req, status_out) = scrub_status_running();
        let (cancel_req, cancel_out) = scrub_cancel_ok();
        let runner = MockRunner::default()
            .with_output(status_req, status_out)
            .with_output(cancel_req, cancel_out);

        let result = cmd_scrub_cancel(&runner, &mp()).unwrap();
        assert_eq!(result, ScrubCancelResult::Cancelled);
    }

    #[test]
    // Intent: status==Never → no cancel issued, returns NotRunning.
    // Why it exists: This is the bug the new handler exists to fix. The old
    //   `grep finished || btrfs scrub cancel` shell hook would call cancel here,
    //   which exits non-zero with "not running" and marks ExecStop as failed.
    //   No cancel mock is seeded; if cmd_scrub_cancel calls cancel,
    //   MockRunner panics with MissingMock.
    // Scenario: braid-scrub.service is stopped before any scrub has ever run on
    //   the pool (Never state).
    fn never_does_not_invoke_cancel() {
        let (status_req, status_out) = scrub_status_never();
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = cmd_scrub_cancel(&runner, &mp()).unwrap();
        assert_eq!(result, ScrubCancelResult::NotRunning);
    }

    #[test]
    // Intent: status==Completed → no cancel issued, returns NotRunning.
    // Why it exists: Same shape as the Never case — Completed must also not
    //   invoke cancel (cancel would exit "not running" and fail ExecStop).
    // Scenario: braid-scrub.service is stopped after the scrub has finished
    //   normally; the unit lingers in "active (exited)" until the next stop.
    fn completed_does_not_invoke_cancel() {
        let (status_req, status_out) = scrub_status_completed();
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = cmd_scrub_cancel(&runner, &mp()).unwrap();
        assert_eq!(result, ScrubCancelResult::NotRunning);
    }

    #[test]
    // Intent: status==Unknown → returns Err(StatusUnknown). No cancel invoked.
    // Why it exists: Failure-layer guard against silently masking parser drift.
    //   `Unknown` is the parser's "couldn't classify" bucket, NOT evidence that
    //   no scrub is running. Treating it as a no-op would (a) hide a parser
    //   regression and (b) leave a real running scrub uncancelled, which would
    //   then block the unmount path. Hard-fail surfaces drift loudly.
    //   No cancel mock is seeded; if anything tried to cancel, MissingMock fires.
    // Scenario: nixpkgs bumps btrfs-progs and the scrub-status output format
    //   changes in a way the parser can't classify. ExecStop hits a fresh case.
    fn unknown_is_hard_error() {
        let (status_req, status_out) = scrub_status_unknown();
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = cmd_scrub_cancel(&runner, &mp());
        assert!(
            matches!(result, Err(ScrubCancelError::StatusUnknown)),
            "expected Err(StatusUnknown), got {result:?}"
        );
    }

    #[test]
    // Intent: status==Running but cancel races with completion (cancel exits
    //   non-zero with "not running" stderr) → RacedCompletion, treated as
    //   success.
    // Why it exists: There is an inherent race: the scrub can finish between
    //   our status probe and the cancel call. We must not turn this benign race
    //   into an ExecStop failure.
    // Scenario: scrub is at 99% during status probe; finishes naturally before
    //   our cancel reaches the kernel.
    fn cancel_race_with_completion_is_success() {
        let (status_req, status_out) = scrub_status_running();
        let (cancel_req, cancel_out) = scrub_cancel_not_running();
        let runner = MockRunner::default()
            .with_output(status_req, status_out)
            .with_output(cancel_req, cancel_out);

        let result = cmd_scrub_cancel(&runner, &mp()).unwrap();
        assert_eq!(result, ScrubCancelResult::RacedCompletion);
    }

    #[test]
    // Intent: status==Running and cancel fails for a real reason (not the
    //   "not running" race) → Err(CancelFailed). Genuine failure must propagate.
    // Why it exists: We must not swallow real cancel failures. The "not running"
    //   stderr substring is the only benign case; everything else is an error.
    // Scenario: cancel hits a permission or kernel error during shutdown.
    fn cancel_real_failure_propagates() {
        let (status_req, status_out) = scrub_status_running();
        let (cancel_req, cancel_out) = scrub_cancel_real_failure();
        let runner = MockRunner::default()
            .with_output(status_req, status_out)
            .with_output(cancel_req, cancel_out);

        let result = cmd_scrub_cancel(&runner, &mp());
        assert!(
            matches!(result, Err(ScrubCancelError::CancelFailed { .. })),
            "expected Err(CancelFailed), got {result:?}"
        );
    }

    #[test]
    // Intent: btrfs scrub status command fails → returns
    //   Err(Parse(CommandFailed)), not silently no-op.
    // Why it exists: A failed status probe must not be mistaken for "no scrub
    //   running" — that could leave a busy mount uncancelled. Mirrors the
    //   `replace_status_failure_is_not_idle` shape from `idle.rs`.
    // Scenario: typo in mount path or filesystem in error state causes
    //   `btrfs scrub status` to exit non-zero.
    fn status_command_failure_propagates() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub status --raw /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: not a btrfs filesystem".into(),
                exit_status: 1,
            },
        );

        let result = cmd_scrub_cancel(&runner, &mp());
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                ScrubCancelError::Parse(ParseError::CommandFailed { exit_code: 1, .. })
            ),
            "expected ScrubCancelError::Parse(CommandFailed {{ exit_code: 1 }}), got {err:?}"
        );
    }
}
