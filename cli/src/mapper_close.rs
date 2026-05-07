use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::progress::Sleeper;
use crate::status_tag::{StatusTag, emit_status, status_line};
use std::time::Duration;

pub(crate) const CLOSE_RETRY_ATTEMPTS: u32 = 3;
pub(crate) const CLOSE_RETRY_DELAY: Duration = Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub(crate) enum CloseMapperError {
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Failed(String),
    #[error("device busy: {0}")]
    DeviceBusy(String),
}

/// Close a LUKS mapper, retrying up to 3 times if the error indicates the
/// device is busy. Non-busy errors fail immediately.
pub(crate) fn close_mapper_with_retry<R: CommandRunner, S: Sleeper>(
    runner: &R,
    sleeper: &S,
    mapper: &str,
    color_enabled: bool,
) -> Result<(), CloseMapperError> {
    for attempt in 1..=CLOSE_RETRY_ATTEMPTS {
        let result = runner.run(&CmdRequest::CryptsetupClose {
            mapper: mapper.to_owned(),
        })?;
        if result.exit_status == 0 {
            return Ok(());
        }
        let msg = format!(
            "cryptsetup close {} failed (exit {}): {}",
            mapper,
            result.exit_status,
            result.stderr.trim()
        );
        // cryptsetup close (lib/setup.c:5763-5811) returns -EBUSY for a held
        // mapper, translated to exit 5 by src/utils_tools.c translate_errno.
        // On the close path exit 5 is EBUSY-exclusive (no -EEXIST branch),
        // so matching exit status is wording- and locale-agnostic and
        // survives upstream phrasing drift. The canonical non-busy distractor
        // (an already-closed mapper -> ENODEV -> exit 4) is runtime-locked by
        // tests/repro/cryptsetup-close-mounted.py; a regression that routed
        // that path through exit 5 would fail that assertion.
        let is_busy = result.exit_status == 5;
        if !is_busy {
            return Err(CloseMapperError::Failed(msg));
        }
        if attempt == CLOSE_RETRY_ATTEMPTS {
            return Err(CloseMapperError::DeviceBusy(msg));
        }
        emit_status(&status_line(
            StatusTag::Warn,
            color_enabled,
            &format!(
                "cryptsetup close {mapper} busy, retrying ({attempt}/{CLOSE_RETRY_ATTEMPTS})..."
            ),
        ));
        sleeper.sleep(CLOSE_RETRY_DELAY);
    }
    unreachable!()
}
