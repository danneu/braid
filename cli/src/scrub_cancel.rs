use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::types::MountPoint;

#[derive(Debug, PartialEq)]
pub enum ScrubCancelResult {
    /// `BTRFS_IOC_SCRUB_CANCEL` succeeded -- a kernel scrub was running and
    /// has been cancelled.
    Cancelled,
    /// `BTRFS_IOC_SCRUB_CANCEL` returned `ENOTCONN` (mapped to `"not running"`
    /// stderr by btrfs-progs). No scrub was running. Benign.
    NotRunning,
}

#[derive(Debug, thiserror::Error)]
pub enum ScrubCancelError {
    #[error("command error: {0}")]
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
/// Result mapping (see `reference/btrfs-progs/cmds/scrub.c:1794-1808`):
///
/// - exit 0 -> `Cancelled` (kernel scrub was running).
/// - exit non-zero with `"not running"` in stderr -> `NotRunning`
///   (`ENOTCONN`; idle filesystem). btrfs-progs uses exit code 2 for this
///   case but we match on the stderr substring to stay aligned with the
///   pre-existing convention in this file.
/// - other non-zero -> `CancelFailed`.
pub fn cmd_scrub_cancel<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<ScrubCancelResult, ScrubCancelError> {
    let raw = runner.run(&CmdRequest::BtrfsScrubCancel {
        mount_point: mount_point.clone(),
    })?;

    if raw.exit_status == 0 {
        Ok(ScrubCancelResult::Cancelled)
    } else if raw.stderr.contains("not running") {
        Ok(ScrubCancelResult::NotRunning)
    } else {
        Err(ScrubCancelError::CancelFailed { stderr: raw.stderr })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
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
        // btrfs-progs maps ENOTCONN to exit code 2 (scrub.c:1799-1800), not 1.
        (
            CmdRequest::BtrfsScrubCancel { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub cancel /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: scrub cancel failed on /mnt/storage: not running\n".into(),
                exit_status: 2,
            },
        )
    }

    fn scrub_cancel_real_failure() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsScrubCancel { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub cancel /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: scrub cancel failed on /mnt/storage: Permission denied\n".into(),
                exit_status: 1,
            },
        )
    }

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

        let result = cmd_scrub_cancel(&runner, &mp()).unwrap();
        assert_eq!(result, ScrubCancelResult::Cancelled);
    }

    #[test]
    // Intent: cancel ioctl ENOTCONN -> NotRunning, not an error.
    // Why it exists: pins the idle-cancel benign path. ExecStop must succeed
    //   when no scrub is running; this is the common case on every shutdown
    //   that did not coincide with a live scrub. Regression here would
    //   reintroduce the "false-fail in Never state" bug. Stderr substring
    //   match stays decoupled from the exact exit code (currently 2).
    // Scenario: braid-scrub.service stop fires with no scrub active; cancel
    //   ioctl returns -ENOTCONN, btrfs prints "ERROR: ...: not running"
    //   and exits 2.
    fn cancel_idle_returns_not_running() {
        let (req, out) = scrub_cancel_not_running();
        let runner = MockRunner::default().with_output(req, out);

        let result = cmd_scrub_cancel(&runner, &mp()).unwrap();
        assert_eq!(result, ScrubCancelResult::NotRunning);
    }

    #[test]
    // Intent: cancel non-zero with stderr that does not contain "not running"
    //   -> Err(CancelFailed). Real errors must propagate, not be swallowed.
    // Why it exists: pins the real-error propagation. We must not classify
    //   permission/IO/unknown errors as "no scrub running"; doing so would
    //   silently leak a kernel scrub past ExecStop and break braid lock.
    // Scenario: cancel ioctl rejected due to permissions or a transient
    //   kernel error; btrfs exits 1 with a non-"not running" stderr.
    fn cancel_real_failure_propagates() {
        let (req, out) = scrub_cancel_real_failure();
        let runner = MockRunner::default().with_output(req, out);

        let result = cmd_scrub_cancel(&runner, &mp());
        assert!(
            matches!(result, Err(ScrubCancelError::CancelFailed { .. })),
            "expected Err(CancelFailed), got {result:?}"
        );
    }

    #[test]
    // Intent: a CommandRunner-layer failure (e.g. spawn error) -> Err(Cmd),
    //   not silently treated as success.
    // Why it exists: pins the command-layer error propagation. If the
    //   subprocess never executed, we cannot claim the scrub is cancelled;
    //   ExecStop must surface that as a stop failure rather than masking it.
    // Scenario: btrfs binary missing on PATH or the runner cannot fork.
    fn cancel_command_failure_propagates() {
        let runner = MockRunner::default(); // no mocks seeded -> MissingMock
        let result = cmd_scrub_cancel(&runner, &mp());
        assert!(
            matches!(result, Err(ScrubCancelError::Cmd(_))),
            "expected Err(Cmd), got {result:?}"
        );
    }
}
