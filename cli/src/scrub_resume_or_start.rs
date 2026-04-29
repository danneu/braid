use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::types::MountPoint;

#[derive(Debug, PartialEq, Eq)]
pub enum ScrubResumeOrStartResult {
    Resumed { uncorrectable_errors: bool },
    Started { uncorrectable_errors: bool },
}

#[derive(Debug, thiserror::Error)]
pub enum ScrubResumeOrStartError {
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("btrfs scrub resume failed: {stderr}")]
    ResumeFailed { stderr: String },
    #[error("btrfs scrub start failed: {stderr}")]
    StartFailed { stderr: String },
}

/// Resume saved scrub progress, or start a fresh scrub when nothing is saved.
///
/// This is the scheduled/manual scrub helper. Exit 2 from resume falls back to
/// `btrfs scrub start -B`; all other resume/start exit codes mirror btrfs.
pub fn cmd_scrub_resume_or_start<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<ScrubResumeOrStartResult, ScrubResumeOrStartError> {
    let resume_raw = runner.run(&CmdRequest::BtrfsScrubResume {
        mount_point: mount_point.clone(),
    })?;

    match resume_raw.exit_status {
        0 => Ok(ScrubResumeOrStartResult::Resumed {
            uncorrectable_errors: false,
        }),
        3 => Ok(ScrubResumeOrStartResult::Resumed {
            uncorrectable_errors: true,
        }),
        2 => start_scrub(runner, mount_point),
        _ => Err(ScrubResumeOrStartError::ResumeFailed {
            stderr: resume_raw.stderr,
        }),
    }
}

fn start_scrub<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<ScrubResumeOrStartResult, ScrubResumeOrStartError> {
    let start_raw = runner.run(&CmdRequest::BtrfsScrubStart {
        mount_point: mount_point.clone(),
    })?;

    match start_raw.exit_status {
        0 => Ok(ScrubResumeOrStartResult::Started {
            uncorrectable_errors: false,
        }),
        3 => Ok(ScrubResumeOrStartResult::Started {
            uncorrectable_errors: true,
        }),
        _ => Err(ScrubResumeOrStartError::StartFailed {
            stderr: start_raw.stderr,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
    }

    fn resume_output(exit_status: i32) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsScrubResume { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub resume -B /mnt/storage".into(),
                stdout: String::new(),
                stderr: if exit_status == 1 {
                    "ERROR: resume failed\n".into()
                } else {
                    String::new()
                },
                exit_status,
            },
        )
    }

    fn start_output(exit_status: i32) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::BtrfsScrubStart { mount_point: mp() },
            RawCommandOutput {
                cmd: "btrfs scrub start -B /mnt/storage".into(),
                stdout: String::new(),
                stderr: if exit_status == 1 {
                    "ERROR: start failed\n".into()
                } else {
                    String::new()
                },
                exit_status,
            },
        )
    }

    #[test]
    // Intent: resume exit 0 returns Resumed without falling back to start.
    // Why it exists: scheduled scrub should continue saved work before starting fresh.
    // Scenario: monthly timer fires while cancelled scrub progress exists.
    fn resume_succeeds_returns_resumed() {
        let (resume_req, resume_out) = resume_output(0);
        let runner = MockRunner::default().with_output(resume_req, resume_out);

        let result = cmd_scrub_resume_or_start(&runner, &mp()).unwrap();
        assert_eq!(
            result,
            ScrubResumeOrStartResult::Resumed {
                uncorrectable_errors: false
            }
        );
    }

    #[test]
    // Intent: resume exit 3 returns Resumed with uncorrectable_errors=true.
    // Why it exists: preserves btrfs scrub's exit-3 semantics.
    // Scenario: resumed scrub finishes but finds uncorrectable errors.
    fn resume_uncorrectable_propagates() {
        let (resume_req, resume_out) = resume_output(3);
        let runner = MockRunner::default().with_output(resume_req, resume_out);

        let result = cmd_scrub_resume_or_start(&runner, &mp()).unwrap();
        assert_eq!(
            result,
            ScrubResumeOrStartResult::Resumed {
                uncorrectable_errors: true
            }
        );
    }

    #[test]
    // Intent: resume exit 2 falls back to start exit 0.
    // Why it exists: timer/manual scrubs must always run a scrub when no
    // saved progress exists.
    // Scenario: monthly timer fires after all prior scrubs finished cleanly.
    fn resume_nothing_to_resume_falls_back_to_start() {
        let (resume_req, resume_out) = resume_output(2);
        let (start_req, start_out) = start_output(0);
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_output(start_req, start_out);

        let result = cmd_scrub_resume_or_start(&runner, &mp()).unwrap();
        assert_eq!(
            result,
            ScrubResumeOrStartResult::Started {
                uncorrectable_errors: false
            }
        );
    }

    #[test]
    // Intent: start exit 3 after fallback returns Started with errors.
    // Why it exists: a fresh scrub's uncorrectable errors must propagate too.
    // Scenario: scheduled scrub starts fresh and finds uncorrectable errors.
    fn start_uncorrectable_after_fallback() {
        let (resume_req, resume_out) = resume_output(2);
        let (start_req, start_out) = start_output(3);
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_output(start_req, start_out);

        let result = cmd_scrub_resume_or_start(&runner, &mp()).unwrap();
        assert_eq!(
            result,
            ScrubResumeOrStartResult::Started {
                uncorrectable_errors: true
            }
        );
    }

    #[test]
    // Intent: resume exit 1 propagates and does not fall back to start.
    // Why it exists: only "nothing to resume" is a fallback condition.
    // Scenario: btrfs cannot read the saved scrub state file.
    fn resume_real_failure_propagates() {
        let (resume_req, resume_out) = resume_output(1);
        let runner = MockRunner::default().with_output(resume_req, resume_out);

        let result = cmd_scrub_resume_or_start(&runner, &mp());
        assert!(
            matches!(result, Err(ScrubResumeOrStartError::ResumeFailed { .. })),
            "expected ResumeFailed, got {result:?}"
        );
    }

    #[test]
    // Intent: start exit 1 after fallback propagates as StartFailed.
    // Why it exists: real fresh-start failures must fail the scrub service.
    // Scenario: timer fires but btrfs cannot start a fresh scrub.
    fn start_real_failure_propagates() {
        let (resume_req, resume_out) = resume_output(2);
        let (start_req, start_out) = start_output(1);
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_output(start_req, start_out);

        let result = cmd_scrub_resume_or_start(&runner, &mp());
        assert!(
            matches!(result, Err(ScrubResumeOrStartError::StartFailed { .. })),
            "expected StartFailed, got {result:?}"
        );
    }
}
