use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::parse::{ParseError, ScrubState, parse_btrfs_scrub_status};
use crate::scrub_resume_or_start::scrub_deferral_pending;
use crate::state_paths::StatePaths;
use crate::types::MountPoint;

#[derive(Debug, PartialEq, Eq)]
pub enum ScrubNeedsResumeResult {
    Yes,
    No,
}

#[derive(Debug, thiserror::Error)]
pub enum ScrubNeedsResumeError {
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error(
        "btrfs scrub status returned an unclassifiable result; refusing to silently skip a \
         pool-online resume. Investigate parser drift."
    )]
    StatusUnknown,
    /// The deferred-scrub flag could not be inspected. Not degraded to "no
    /// deferral pending": that would silently drop the retry a skipped scrub is
    /// owed. The trigger unit has no `onFailure`, so this surfaces as a
    /// journalled exit-2 failed unit rather than an alert.
    #[error("cannot inspect the deferred-scrub flag: {source}. Investigate /var/lib/braid.")]
    DeferredFlagUnreadable { source: std::io::Error },
}

/// Decides whether the pool-online trigger should start the scrub service.
///
/// Two independent reasons to say `Yes`, checked in this order:
///
/// 1. A scheduled scrub was skipped as busy and never ran. systemd's pending
///    restart does not survive a reboot, so the durable deferred flag is what
///    makes "at most until the next pool-online" true instead of "until the
///    next calendar firing".
/// 2. Saved scrub progress exists, decided from the terminal `Status:` word; a
///    missing or unparseable start timestamp is not a reason to strand
///    resumable progress.
///
/// The flag is checked first because it is cheap, local, and true even when the
/// pool has no saved progress at all -- a skipped scrub usually never started.
pub fn cmd_scrub_needs_resume<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> Result<ScrubNeedsResumeResult, ScrubNeedsResumeError> {
    if scrub_deferral_pending(paths)
        .map_err(|source| ScrubNeedsResumeError::DeferredFlagUnreadable { source })?
    {
        return Ok(ScrubNeedsResumeResult::Yes);
    }

    let raw = runner.run(&CmdRequest::BtrfsScrubStatus {
        mount_point: mount_point.clone(),
    })?;

    match parse_btrfs_scrub_status(&raw)?.state {
        ScrubState::Aborted { .. } | ScrubState::Interrupted { .. } => {
            Ok(ScrubNeedsResumeResult::Yes)
        }
        ScrubState::Never | ScrubState::Finished { .. } | ScrubState::Running { .. } => {
            Ok(ScrubNeedsResumeResult::No)
        }
        ScrubState::Unknown => Err(ScrubNeedsResumeError::StatusUnknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::test_fixtures::{
        isolated_paths, scrub_mp, scrub_status_aborted, scrub_status_finished,
        scrub_status_interrupted, scrub_status_never, scrub_status_running, scrub_status_unknown,
    };

    #[test]
    // Intent: status==Aborted means the pool-online trigger should start the scrub service.
    // Why it exists: cancelled scrub progress must be resumed on the next unlock.
    // Scenario: `braid lock` cancelled a running scrub and persisted btrfs progress.
    fn aborted_needs_resume() {
        let (_dir, paths) = isolated_paths();
        let (status_req, status_out) = scrub_status_aborted();
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = cmd_scrub_needs_resume(&runner, &scrub_mp(), &paths).unwrap();
        assert_eq!(result, ScrubNeedsResumeResult::Yes);
    }

    #[test]
    // Intent: an aborted scrub with no parseable start time still reports
    // needs-resume.
    // Why it exists: the pool-online resume trigger must not hard-fail and
    // strand resumable progress over a missing display timestamp.
    // Scenario: pool-online probes a scrub that `braid lock` aborted, whose
    // status block lacks a parseable start line.
    fn aborted_without_started_at_still_needs_resume() {
        let (_dir, paths) = isolated_paths();
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus {
                mount_point: scrub_mp(),
            },
            RawCommandOutput {
                cmd: "btrfs scrub status --raw /mnt/storage".into(),
                stdout: "\
UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1
Status:           aborted
Duration:         0:00:10
Total to scrub:   1073741824
Rate:             104857600/s
Error summary:    no errors found
"
                .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );

        let result = cmd_scrub_needs_resume(&runner, &scrub_mp(), &paths).unwrap();
        assert_eq!(result, ScrubNeedsResumeResult::Yes);
    }

    #[test]
    // Intent: status==Interrupted means the pool-online trigger should start the scrub service.
    // Why it exists: interrupted btrfs scrub progress is resumable.
    // Scenario: userspace scrub process died before completing.
    fn interrupted_needs_resume() {
        let (_dir, paths) = isolated_paths();
        let (status_req, status_out) = scrub_status_interrupted();
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = cmd_scrub_needs_resume(&runner, &scrub_mp(), &paths).unwrap();
        assert_eq!(result, ScrubNeedsResumeResult::Yes);
    }

    #[test]
    // Intent: status==Never means the trigger exits as a clean no-op.
    // Why it exists: pool-online activation must not start unscheduled scrubs.
    // Scenario: a newly created pool unlocks before any scrub has run.
    fn never_does_not_need_resume() {
        let (_dir, paths) = isolated_paths();
        let (status_req, status_out) = scrub_status_never();
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = cmd_scrub_needs_resume(&runner, &scrub_mp(), &paths).unwrap();
        assert_eq!(result, ScrubNeedsResumeResult::No);
    }

    #[test]
    // Intent: status==Finished means the trigger exits as a clean no-op.
    // Why it exists: cleanly completed scrubs must not be resumed or restarted by pool-online.
    // Scenario: pool unlocks after the previous scheduled scrub finished normally.
    fn finished_does_not_need_resume() {
        let (_dir, paths) = isolated_paths();
        let (status_req, status_out) = scrub_status_finished();
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = cmd_scrub_needs_resume(&runner, &scrub_mp(), &paths).unwrap();
        assert_eq!(result, ScrubNeedsResumeResult::No);
    }

    #[test]
    // Intent: status==Running means the trigger exits as a clean no-op.
    // Why it exists: a live scrub does not need a separate resume kick.
    // Scenario: a manual scrub is already running when the trigger is invoked.
    fn running_does_not_need_resume() {
        let (_dir, paths) = isolated_paths();
        let (status_req, status_out) = scrub_status_running();
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = cmd_scrub_needs_resume(&runner, &scrub_mp(), &paths).unwrap();
        assert_eq!(result, ScrubNeedsResumeResult::No);
    }

    #[test]
    // Intent: status==Unknown returns Err(StatusUnknown).
    // Why it exists: failure-layer guard against silently masking parser drift.
    // Scenario: btrfs-progs changes scrub-status output in a way the parser can't classify.
    fn unknown_is_hard_error() {
        let (_dir, paths) = isolated_paths();
        let (status_req, status_out) = scrub_status_unknown();
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = cmd_scrub_needs_resume(&runner, &scrub_mp(), &paths);
        assert!(
            matches!(result, Err(ScrubNeedsResumeError::StatusUnknown)),
            "expected Err(StatusUnknown), got {result:?}"
        );
    }

    #[test]
    // Intent: btrfs scrub status command failure propagates as Parse(CommandFailed).
    // Why it exists: failed status probes must not be mistaken for "no resume needed."
    // Scenario: typo in mount path or filesystem error causes scrub status to exit non-zero.
    fn status_command_failure_propagates() {
        let (_dir, paths) = isolated_paths();
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus {
                mount_point: scrub_mp(),
            },
            RawCommandOutput {
                cmd: "btrfs scrub status --raw /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: not a btrfs filesystem".into(),
                exit_status: 1,
            },
        );

        let result = cmd_scrub_needs_resume(&runner, &scrub_mp(), &paths);
        let err = result.unwrap_err();
        assert!(
            matches!(
                err,
                ScrubNeedsResumeError::Parse(ParseError::CommandFailed { exit_code: 1, .. })
            ),
            "expected ScrubNeedsResumeError::Parse(CommandFailed {{ exit_code: 1 }}), got {err:?}"
        );
    }

    #[test]
    // Intent: a deferred-scrub flag reports Yes without probing btrfs at all.
    // Why it exists: systemd's pending restart does not survive a reboot, so
    //   the pool-online trigger is the only thing that re-pokes a scrub skipped
    //   before the machine went down. A skipped scrub usually never started, so
    //   btrfs scrub status has nothing resumable to report -- the flag has to be
    //   an independent Yes.
    // Scenario: the monthly scrub skipped for a running balance at 00:00; the
    //   NAS was rebooted at 00:30 and the pool comes back online.
    fn deferred_flag_needs_resume_without_probing_btrfs() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.scrub_deferred(), b"").unwrap();
        // No registered output: any btrfs probe would surface as MissingMock.
        let runner = MockRunner::default();

        let result = cmd_scrub_needs_resume(&runner, &scrub_mp(), &paths).unwrap();
        assert_eq!(result, ScrubNeedsResumeResult::Yes);
        assert!(
            runner.requests().is_empty(),
            "the flag alone answers the question"
        );
    }

    #[test]
    // Intent: an unclassifiable deferred-flag lookup is an error, not "No".
    // Why it exists: reporting "nothing pending" when the flag cannot be read
    //   silently drops the retry the skipped scrub is owed. The trigger unit has
    //   no onFailure, so this surfaces as a journalled failed unit (exit 2).
    // Scenario: the braid state root is unreadable, so the lookup fails with
    //   something other than NotFound.
    fn deferred_flag_inspection_error_is_hard_error() {
        let dir = tempfile::TempDir::new().unwrap();
        let not_a_dir = dir.path().join("state");
        std::fs::write(&not_a_dir, b"").unwrap();
        let paths = crate::state_paths::StatePaths::custom(not_a_dir);
        let runner = MockRunner::default();

        let result = cmd_scrub_needs_resume(&runner, &scrub_mp(), &paths);
        assert!(
            matches!(
                result,
                Err(ScrubNeedsResumeError::DeferredFlagUnreadable { .. })
            ),
            "expected DeferredFlagUnreadable, got {result:?}"
        );
    }
}
