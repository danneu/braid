use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::progress::Sleeper;
use crate::status_tag::{StatusTag, emit_status, status_line};
use crate::types::MapperName;
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
pub(crate) fn close_mapper_with_retry<R: CommandRunner, S: Sleeper + ?Sized>(
    runner: &R,
    sleeper: &S,
    mapper: &MapperName,
    color_enabled: bool,
) -> Result<(), CloseMapperError> {
    for attempt in 1..=CLOSE_RETRY_ATTEMPTS {
        let result = runner.run(&CmdRequest::CryptsetupClose {
            mapper: mapper.clone(),
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

/// Best-effort mapper close used by pool maintenance paths that must warn
/// instead of failing after btrfs has already committed the topology change.
pub(crate) fn close_mapper_best_effort<R, S>(
    runner: &R,
    sleeper: &S,
    mapper: &MapperName,
    disk_label: &str,
    color_enabled: bool,
) -> bool
where
    R: CommandRunner,
    S: Sleeper + ?Sized,
{
    emit_status(&status_line(
        StatusTag::Wait,
        color_enabled,
        &format!("disk {disk_label}: locking..."),
    ));
    match close_mapper_with_retry(runner, sleeper, mapper, color_enabled) {
        Ok(()) => {
            emit_status(&status_line(
                StatusTag::Ok,
                color_enabled,
                &format!("disk {disk_label}: locked"),
            ));
            true
        }
        Err(e) => {
            emit_status(&status_line(
                StatusTag::Warn,
                color_enabled,
                &format!("disk {disk_label}: lock failed ({e})"),
            ));
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::progress::NoopSleeper;

    fn close_output(exit_status: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "cryptsetup close".into(),
            stdout: String::new(),
            stderr: stderr.into(),
            exit_status,
        }
    }

    fn close_request() -> CmdRequest {
        CmdRequest::CryptsetupClose {
            mapper: MapperName::from_basename("braid-disk2".into()),
        }
    }

    fn close_request_count(runner: &MockRunner) -> usize {
        runner
            .requests()
            .iter()
            .filter(|request| matches!(request, CmdRequest::CryptsetupClose { .. }))
            .count()
    }

    fn run_best_effort(runner: &MockRunner) -> (bool, String) {
        let mut closed = false;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            closed = close_mapper_best_effort(
                runner,
                &NoopSleeper,
                &MapperName::from_basename("braid-disk2".into()),
                "disk2",
                false,
            );
        });
        (closed, captured)
    }

    // Intent: best-effort mapper close reports success after one successful
    // cryptsetup close request.
    // Why it exists: callers use the returned bool to decide whether to print
    // a post-success trailer, so the direct success path must stay true.
    // Scenario: btrfs has already removed/replaced a disk and cryptsetup close
    // releases the old mapper immediately.
    #[test]
    fn close_mapper_best_effort_returns_true_on_success() {
        let runner = MockRunner::default().with_output(close_request(), close_output(0, ""));

        let (closed, _) = run_best_effort(&runner);

        assert!(closed);
        assert_eq!(close_request_count(&runner), 1);
    }

    // Intent: best-effort mapper close retries a busy close and returns true
    // when a later attempt succeeds.
    // Why it exists: this is the transient EBUSY race the shared helper is
    // meant to dissolve across remove, replace, and recover.
    // Scenario: a short-lived holder keeps the mapper busy for the first close
    // attempt, then releases it before the second attempt.
    #[test]
    fn close_mapper_best_effort_retries_busy_then_succeeds() {
        let runner = MockRunner::default().with_output_sequence(
            close_request(),
            vec![close_output(5, "device is busy"), close_output(0, "")],
        );

        let (closed, captured) = run_best_effort(&runner);

        assert!(closed);
        assert_eq!(close_request_count(&runner), 2);
        let wait = "[wait] disk disk2: locking...";
        let ok = "[ok]   disk disk2: locked";
        assert!(captured.contains(wait), "missing wait row: {captured:?}");
        assert!(captured.contains(ok), "missing ok row: {captured:?}");
        assert!(
            captured.find(wait) < captured.find(ok),
            "wait must precede ok, got: {captured:?}"
        );
    }

    // Intent: best-effort mapper close exhausts the busy retry budget before
    // returning false.
    // Why it exists: callers must not treat a persistently busy mapper as
    // closed or print a post-success trailer.
    // Scenario: an external process keeps holding the mapper through all retry
    // attempts.
    #[test]
    fn close_mapper_best_effort_returns_false_after_persistent_busy() {
        let runner = MockRunner::default().with_output_sequence(
            close_request(),
            vec![
                close_output(5, "device is busy"),
                close_output(5, "device is busy"),
                close_output(5, "device is busy"),
            ],
        );

        let (closed, _) = run_best_effort(&runner);

        assert!(!closed);
        assert_eq!(close_request_count(&runner), 3);
    }

    // Intent: best-effort mapper close fails non-busy errors immediately.
    // Why it exists: retrying ENODEV-style failures would mask a different
    // close contract from the EBUSY race.
    // Scenario: the mapper is already absent by the time cleanup runs.
    #[test]
    fn close_mapper_best_effort_returns_false_without_retry_on_non_busy() {
        let runner =
            MockRunner::default().with_output(close_request(), close_output(4, "device not found"));

        let (closed, captured) = run_best_effort(&runner);

        assert!(!closed);
        assert_eq!(close_request_count(&runner), 1);
        let wait = "[wait] disk disk2: locking...";
        let warn = "[warn] disk disk2: lock failed (cryptsetup close braid-disk2 failed (exit 4): device not found)";
        assert!(captured.contains(wait), "missing wait row: {captured:?}");
        assert!(captured.contains(warn), "missing warn row: {captured:?}");
        assert!(
            captured.find(wait) < captured.find(warn),
            "wait must precede warn, got: {captured:?}"
        );
    }
}
