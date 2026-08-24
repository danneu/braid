use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::probe::Filesystem;
use crate::progress::Sleeper;
use crate::status_tag::{StatusTag, emit_status, status_line};
use crate::types::{DiskName, MapperName};
use std::time::Duration;

pub(crate) const CLOSE_RETRY_ATTEMPTS: u32 = 3;
pub(crate) const CLOSE_RETRY_DELAY: Duration = Duration::from_millis(500);

/// Pairs the operator label for cleanup rows with the runtime mapper handle
/// this command actually opened and must close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TrackedMapper {
    /// Operator identity used to render every `disk <name>: ...` cleanup row.
    pub(crate) name: DiskName,
    /// Observed dm handle closed during cleanup; never reconstructed from `name`.
    pub(crate) mapper: MapperName,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum CloseMapperError {
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Failed(String),
    #[error("device busy: {0}")]
    DeviceBusy(String),
}

/// Distinguishes a steady-state close from `add`'s pre-commit rollback close so
/// `emit_close_progress` is the single source of both the wait/ok row suffix and
/// the warn-row failure wording. Encoding the variants here -- rather than
/// letting each caller pass a free-form suffix -- keeps the two phrasings from
/// drifting apart across the close-row sites that share this core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloseContext {
    /// Mount unlock cleanup, post-commit maintenance, recover remount cycle: no
    /// row suffix.
    Normal,
    /// `add`'s rollback guard: rows carry a ` (cleanup)` suffix.
    Cleanup,
}

impl CloseContext {
    /// Suffix appended to the `locking`/`locked` wait/ok rows.
    fn row_suffix(self) -> &'static str {
        match self {
            CloseContext::Normal => "",
            CloseContext::Cleanup => " (cleanup)",
        }
    }

    /// Body of the `[warn]` failure row for warn-and-continue callers, embedding
    /// the underlying close error `e`.
    fn failure_detail(self, e: &CloseMapperError) -> String {
        match self {
            CloseContext::Normal => format!("lock failed ({e})"),
            CloseContext::Cleanup => format!("lock failed (cleanup, {e})"),
        }
    }
}

/// Best-effort pre-close release of btrfs kernel scan state for mapper paths
/// whose ownership the caller has already established. Filtering immediately
/// before execution handles disappearing mappers and, critically, suppresses
/// the empty no-argument form because that form is kernel-global rather than
/// scoped to braid's close work. Failures warn and continue so this stale-cache
/// mitigation never prevents callers from attempting their owned closes.
pub(crate) fn forget_existing_scanned_devices_best_effort<
    R: CommandRunner,
    F: Filesystem + ?Sized,
>(
    runner: &R,
    fs: &F,
    mut devices: Vec<String>,
    color_enabled: bool,
) {
    devices.retain(|path| fs.exists(path));
    if devices.is_empty() {
        return;
    }

    match runner.run(&CmdRequest::BtrfsDeviceScanForget { devices }) {
        Ok(result) if result.exit_status == 0 => {}
        Ok(result) => {
            emit_status(&status_line(
                StatusTag::Warn,
                color_enabled,
                &format!(
                    "btrfs device scan --forget failed (exit {}): {} (continuing)",
                    result.exit_status,
                    result.stderr.trim()
                ),
            ));
        }
        Err(error) => {
            emit_status(&status_line(
                StatusTag::Warn,
                color_enabled,
                &format!("btrfs device scan --forget failed: {error} (continuing)"),
            ));
        }
    }
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

/// Emit the `disk <label>: locking<suffix>...` wait row, close the mapper with
/// busy-retry, and on success emit `disk <label>: locked<suffix>` (suffix from
/// `context`). On failure returns the error WITHOUT a closing row: each caller
/// owns its own failure severity (warn-and-continue, fatal `[fail]`, or
/// hard-abort), so the only part shared across the close-row sites is the
/// wait + retry-close + ok. `disk_label` is the journaled operator name, never
/// derived from a mapper basename, so mapper drift cannot leak into the row.
pub(crate) fn emit_close_progress<R, S>(
    runner: &R,
    sleeper: &S,
    mapper: &MapperName,
    disk_label: &DiskName,
    context: CloseContext,
    color_enabled: bool,
) -> Result<(), CloseMapperError>
where
    R: CommandRunner,
    S: Sleeper + ?Sized,
{
    let suffix = context.row_suffix();
    emit_status(&status_line(
        StatusTag::Wait,
        color_enabled,
        &format!("disk {disk_label}: locking{suffix}..."),
    ));
    close_mapper_with_retry(runner, sleeper, mapper, color_enabled)?;
    emit_status(&status_line(
        StatusTag::Ok,
        color_enabled,
        &format!("disk {disk_label}: locked{suffix}"),
    ));
    Ok(())
}

/// Best-effort mapper close used by pool maintenance paths that must warn
/// instead of failing after btrfs has already committed the topology change.
/// Wraps `emit_close_progress` and, on failure, emits the `[warn]` row whose
/// wording is derived from the same `context`, returning whether the close
/// succeeded.
pub(crate) fn close_mapper_best_effort<R, S>(
    runner: &R,
    sleeper: &S,
    mapper: &MapperName,
    disk_label: &DiskName,
    context: CloseContext,
    color_enabled: bool,
) -> bool
where
    R: CommandRunner,
    S: Sleeper + ?Sized,
{
    match emit_close_progress(runner, sleeper, mapper, disk_label, context, color_enabled) {
        Ok(()) => true,
        Err(e) => {
            emit_status(&status_line(
                StatusTag::Warn,
                color_enabled,
                &format!("disk {disk_label}: {}", context.failure_detail(&e)),
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
    use crate::test_fixtures::MockFs;

    fn forget_request(devices: &[&str]) -> CmdRequest {
        CmdRequest::BtrfsDeviceScanForget {
            devices: devices.iter().map(|device| (*device).to_owned()).collect(),
        }
    }

    fn forget_output(exit_status: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "btrfs device scan --forget".into(),
            stdout: String::new(),
            stderr: stderr.into(),
            exit_status,
        }
    }

    // Intent: best-effort scan forget sends only still-existing scoped paths
    // and stays silent when the command succeeds.
    // Why it exists: mount and lock supply different ownership sets, but both
    // require the same last-moment disappearance guard before mapper close.
    // Scenario: one planned mapper still exists and one disappeared after
    // planning; btrfs receives only the surviving path.
    #[test]
    fn forget_existing_scanned_devices_best_effort_filters_and_succeeds_silently() {
        let request = forget_request(&["/dev/mapper/braid-disk1"]);
        let runner = MockRunner::default().with_output(request.clone(), forget_output(0, ""));
        let fs = MockFs::unmounted(vec!["/dev/mapper/braid-disk1".into()]);

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            forget_existing_scanned_devices_best_effort(
                &runner,
                &fs,
                vec![
                    "/dev/mapper/braid-disk1".into(),
                    "/dev/mapper/braid-disk2".into(),
                ],
                false,
            );
        });

        assert_eq!(runner.requests(), vec![request]);
        assert_eq!(captured, "");
    }

    // Intent: best-effort scan forget never emits the kernel-global empty
    // `btrfs device scan --forget` form.
    // Why it exists: filtering can remove every caller-owned mapper path, and
    // the no-argument command would affect unrelated btrfs filesystems.
    // Scenario: the only planned mapper disappeared before execution.
    #[test]
    fn forget_existing_scanned_devices_best_effort_skips_empty_request() {
        let runner = MockRunner::default();
        let fs = MockFs::unmounted(vec![]);

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            forget_existing_scanned_devices_best_effort(
                &runner,
                &fs,
                vec!["/dev/mapper/braid-disk1".into()],
                false,
            );
        });

        assert!(runner.requests().is_empty());
        assert_eq!(captured, "");
    }

    // Intent: a non-zero scoped scan-forget result uses the shared warning and
    // remains non-fatal.
    // Why it exists: mount and lock must not silently drift in their
    // operator-facing best-effort policy.
    // Scenario: btrfs rejects the surviving mapper path with exit 1.
    #[test]
    fn forget_existing_scanned_devices_best_effort_warns_on_nonzero_exit() {
        let request = forget_request(&["/dev/mapper/braid-disk1"]);
        let runner =
            MockRunner::default().with_output(request, forget_output(1, "forget failed\n"));
        let fs = MockFs::unmounted(vec!["/dev/mapper/braid-disk1".into()]);

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            forget_existing_scanned_devices_best_effort(
                &runner,
                &fs,
                vec!["/dev/mapper/braid-disk1".into()],
                false,
            );
        });

        assert_eq!(
            captured,
            "[warn] btrfs device scan --forget failed (exit 1): forget failed (continuing)\n"
        );
    }

    // Intent: a scoped scan-forget invocation error uses the shared warning
    // and remains non-fatal.
    // Why it exists: runner errors and non-zero tool exits are separate paths
    // that previously carried duplicated wording in both callers.
    // Scenario: the command runner cannot execute the forget request.
    #[test]
    fn forget_existing_scanned_devices_best_effort_warns_on_runner_error() {
        let runner = MockRunner::default();
        let fs = MockFs::unmounted(vec!["/dev/mapper/braid-disk1".into()]);

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            forget_existing_scanned_devices_best_effort(
                &runner,
                &fs,
                vec!["/dev/mapper/braid-disk1".into()],
                false,
            );
        });

        assert_eq!(
            captured,
            "[warn] btrfs device scan --forget failed: mock output missing for request (continuing)\n"
        );
    }

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
        let disk_label = DiskName::parse("disk2").unwrap();
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            closed = close_mapper_best_effort(
                runner,
                &NoopSleeper,
                &MapperName::from_basename("braid-disk2".into()),
                &disk_label,
                CloseContext::Normal,
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
