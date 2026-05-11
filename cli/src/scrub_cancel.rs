use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::types::MountPoint;

#[derive(Debug, PartialEq)]
pub enum ScrubCancelResult {
    /// `BTRFS_IOC_SCRUB_CANCEL` succeeded -- a kernel scrub was running and
    /// has been cancelled.
    Cancelled,
    /// `BTRFS_IOC_SCRUB_CANCEL` returned `ENOTCONN` (exit code 2 from
    /// btrfs-progs; rendered as `"not running"` in stderr). No scrub was
    /// running. Benign.
    NotRunning,
}

#[derive(Debug, thiserror::Error)]
pub enum ScrubCancelError {
    #[error("btrfs scrub cancel command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("btrfs scrub cancel failed: {stderr}")]
    CancelFailed { stderr: String },
}

/// Cancel any running btrfs scrub on `mount_point`.
///
/// Designed for the `braid-scrub.service` ExecStop hook. The cancel ioctl is
/// itself the kernel-authoritative test for whether a scrub is running, so
/// no userspace status probe precedes it. Skipping the probe makes the
/// shutdown path immune to:
///
/// - `btrfs scrub status` command failures (transient EIO, partial mount
///   degradation).
/// - parser drift (output format changes on a btrfs-progs version bump).
/// - userspace/kernel state divergence (kernel scrub running with no
///   on-disk progress checkpoint, e.g. when the foreground `btrfs scrub
///   start -B` died before its first write to `scrub.status.<fsid>`).
///
/// Result mapping (see `reference/btrfs-progs/cmds/scrub.c:1794-1812`):
///
/// - exit 0 -> `Cancelled` (kernel scrub was running).
/// - exit 2 -> `NotRunning` (`ENOTCONN`; idle filesystem; rendered as
///   stderr "not running" by btrfs-progs).
/// - other non-zero -> `CancelFailed`.
///
/// We dispatch on the numeric exit code rather than the stderr substring:
/// "not running" is btrfs-progs's human-readable ENOTCONN error text,
/// not the API braid should parse. Exit code 2 is the implementation
/// contract in the pinned btrfs-progs source, guarded by the live
/// scrub-lifecycle VM canary; btrfs-scrub(8) does not document the
/// cancel-idle exit code.
pub fn cmd_scrub_cancel<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<ScrubCancelResult, ScrubCancelError> {
    let raw = runner.run(&CmdRequest::BtrfsScrubCancel {
        mount_point: mount_point.clone(),
    })?;

    match raw.exit_status {
        0 => Ok(ScrubCancelResult::Cancelled),
        // ENOTCONN: kernel had no scrub running. btrfs-progs renders this as
        // exit code 2 with stderr "not running" (see
        // reference/btrfs-progs/cmds/scrub.c:1794-1812). Match on the numeric
        // code -- the stderr text is human-readable rendering of errno and
        // is not a stable contract.
        2 => Ok(ScrubCancelResult::NotRunning),
        _ => Err(ScrubCancelError::CancelFailed { stderr: raw.stderr }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::test_fixtures::{
        scrub_cancel_not_running, scrub_cancel_ok, scrub_cancel_real_failure, scrub_mp,
    };

    #[test]
    // Intent: cancel ioctl exit 0 -> Cancelled.
    // Why it exists: pins the kernel-authoritative success path. The cancel
    //   ioctl is the only signal we trust; this test asserts that exit 0
    //   maps to Cancelled with no other dependencies.
    // Scenario: braid-scrub.service ExecStop fires while a scrub is running;
    //   BTRFS_IOC_SCRUB_CANCEL returns 0 and btrfs prints "scrub cancelled".
    fn cancel_running_returns_cancelled() {
        let (req, out) = scrub_cancel_ok();
        let runner = MockRunner::default().with_output(req, out);

        let result = cmd_scrub_cancel(&runner, &scrub_mp()).unwrap();
        assert_eq!(result, ScrubCancelResult::Cancelled);
    }

    #[test]
    // Intent: cancel ioctl ENOTCONN (exit code 2) -> NotRunning, not an error.
    // Why it exists: pins the idle-cancel benign path. ExecStop must succeed
    //   when no scrub is running; this is the common case on every shutdown
    //   that did not coincide with a live scrub. Regression here would
    //   reintroduce the "false-fail in Never state" bug. Dispatch is exit
    //   code 2 (source-pinned btrfs-progs behavior guarded by the VM canary),
    //   not the "not running" stderr text.
    // Scenario: braid-scrub.service stop fires with no scrub active; cancel
    //   ioctl returns -ENOTCONN, btrfs prints "ERROR: ...: not running"
    //   and exits 2.
    fn cancel_idle_returns_not_running() {
        let (req, out) = scrub_cancel_not_running();
        let runner = MockRunner::default().with_output(req, out);

        let result = cmd_scrub_cancel(&runner, &scrub_mp()).unwrap();
        assert_eq!(result, ScrubCancelResult::NotRunning);
    }

    #[test]
    // Intent: cancel exits with a code other than 0 or 2 -> Err(CancelFailed).
    //   Real errors must propagate, not be swallowed.
    // Why it exists: pins the real-error propagation. Exit code 2 (ENOTCONN)
    //   is the only benign non-zero exit; every other non-zero exit must
    //   surface as ExecStop failure rather than silently leaking a busy or
    //   unknown-state filesystem past braid lock.
    // Scenario: cancel ioctl rejected due to permissions or a transient
    //   kernel error; btrfs exits 1 with a non-"not running" stderr.
    fn cancel_real_failure_propagates() {
        let (req, out) = scrub_cancel_real_failure();
        let runner = MockRunner::default().with_output(req, out);

        let result = cmd_scrub_cancel(&runner, &scrub_mp());
        assert!(
            matches!(result, Err(ScrubCancelError::CancelFailed { .. })),
            "expected Err(CancelFailed), got {result:?}"
        );
    }

    #[test]
    // Intent: exit code 2 alone -> NotRunning, regardless of stderr content.
    // Why it exists: pins the contract that exit code 2 is the dispatch.
    //   With empty stderr, any future regression that reintroduces a
    //   `stderr.contains("not running")` check would misclassify this case
    //   as CancelFailed, surfacing the regression at test time.
    // Scenario: a future btrfs-progs adopts a different "not running"
    //   wording (rephrasing, localization) but keeps exit code 2 -- braid
    //   continues to classify correctly.
    fn cancel_exit_two_with_empty_stderr_is_not_running() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubCancel {
                mount_point: scrub_mp(),
            },
            RawCommandOutput {
                cmd: "btrfs scrub cancel /mnt/storage".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 2,
            },
        );

        let result = cmd_scrub_cancel(&runner, &scrub_mp()).unwrap();
        assert_eq!(result, ScrubCancelResult::NotRunning);
    }

    #[test]
    // Intent: exit code 1 with "not running" in stderr -> Err(CancelFailed).
    // Why it exists: inverse pin to cancel_exit_two_with_empty_stderr_is_not_running.
    //   The stderr substring is NOT consulted: a "not running" stderr without
    //   exit code 2 is a real failure, not a benign idle. Together with that
    //   test, this fully characterizes the new contract -- exit code is the
    //   dispatch, stderr text is irrelevant.
    // Scenario: a hypothetical future btrfs-progs error path renders
    //   strerror(errno) text containing "not running" for some non-ENOTCONN
    //   errno -- braid does not silence it as a benign idle.
    fn cancel_not_running_stderr_with_exit_one_is_failure() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubCancel {
                mount_point: scrub_mp(),
            },
            RawCommandOutput {
                cmd: "btrfs scrub cancel /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: scrub cancel failed on /mnt/storage: not running\n".into(),
                exit_status: 1,
            },
        );

        let result = cmd_scrub_cancel(&runner, &scrub_mp());
        assert!(
            matches!(result, Err(ScrubCancelError::CancelFailed { .. })),
            "expected Err(CancelFailed), got {result:?}"
        );
    }

    struct FailingCancelRunner;

    impl CommandRunner for FailingCancelRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsScrubCancel { .. } => Err(CmdError::Failed(
                    "btrfs scrub cancel /mnt/storage: No such file or directory (os error 2)"
                        .into(),
                )),
                other => panic!("unexpected request: {other:?}"),
            }
        }

        fn run_with_stdin(
            &self,
            _request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            panic!("run_with_stdin not used by cmd_scrub_cancel")
        }
    }

    /*
     * Intent: a CommandRunner-layer failure (for example spawn error) remains
     *   Err(Cmd), but displays with scrub-cancel-specific framing.
     * Why it exists: pins the distinction between command-layer failure and
     *   btrfs cancel output failure while making the ExecStop journal line
     *   identify the failing operation. Reverting the display string to the
     *   generic "command error:" prefix must fail this test.
     * Scenario: braid-scrub.service ExecStop tries to cancel scrub, but the
     *   cancel subprocess cannot be spawned before any exit-status output exists.
     */
    #[test]
    fn cancel_command_failure_propagates() {
        let runner = FailingCancelRunner;
        let result = cmd_scrub_cancel(&runner, &scrub_mp());
        let err = result.unwrap_err();

        assert!(
            matches!(&err, ScrubCancelError::Cmd(_)),
            "expected Err(Cmd), got {err:?}"
        );
        assert!(
            err.to_string()
                .starts_with("btrfs scrub cancel command error:"),
            "expected cancel command error framing, got {err}"
        );
    }
}
