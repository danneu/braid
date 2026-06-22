use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;

#[derive(Debug, PartialEq, Eq)]
pub enum ScrubResumeOrStartResult {
    Resumed {
        uncorrectable_errors: bool,
    },
    Started {
        uncorrectable_errors: bool,
    },
    /// btrfs exited outside `{0,2,3}` while a deliberate teardown was in flight
    /// (the cancel-request marker was present). `braid lock`/suspend/shutdown
    /// cancels the running scrub via the cancel ioctl, which makes btrfs exit 1
    /// -- the *same* code a genuine fatal scrub error uses. The marker (written
    /// by the ExecStop teardown) is the sole authoritative "this was
    /// intentional" signal, so this maps to a clean service exit 0 and never
    /// fires `onFailure`.
    Cancelled,
}

#[derive(Debug, thiserror::Error)]
pub enum ScrubResumeOrStartError {
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("btrfs scrub resume failed: {stderr}")]
    ResumeFailed { stderr: String },
    #[error("btrfs scrub start failed: {stderr}")]
    StartFailed { stderr: String },
    /// Entry cleanup of a stale cancel-request marker failed with something
    /// other than `NotFound` (the path is a directory, `EACCES`, `EIO`, ...).
    /// The scrub does not start: if entry cleanup cannot *guarantee* a clean
    /// slate, a surviving stale marker would later read a genuine exit 1 as
    /// `Cancelled` and silently swallow the very failure this feature exists to
    /// alert on -- so cleanup is fail-closed (the downstream failure mode makes
    /// every cleanup uncertainty a hard error). Split from the btrfs
    /// resume/start failures because the remediation differs: inspect the
    /// poisoned `scrub-cancel-requested` path, not the pool.
    #[error("could not clear stale scrub-cancel marker: {source}")]
    MarkerCleanupFailed { source: std::io::Error },
}

/// Resume saved scrub progress, or start a fresh scrub when nothing is saved.
///
/// This is the scheduled/manual scrub helper. Exit 2 from resume falls back to
/// `btrfs scrub start -B`; exit 3 (uncorrectable errors found, scrub completed)
/// stays a service success -- corruption alerts via ADR 014's device-stats
/// poll, not this exit code, so `onFailure` covers execution failure only.
///
/// A btrfs exit outside `{0,2,3}` is ambiguous: btrfs returns 1 for *both* a
/// deliberate cancel (lock/suspend/shutdown) and a genuine fatal scrub error,
/// and `scrub_one_dev` sets `canceled = !!ret` so even scrub *status* renders
/// both as `aborted`. The only authoritative discriminator is braid's own
/// teardown intent: the ExecStop script touches a cancel-request marker, so
/// the runner removes any stale marker at entry (fail-closed -- a surviving
/// marker would later mask a real failure) and, on an ambiguous exit, returns
/// `Cancelled` iff the marker is present and the failure otherwise. The marker
/// is the sole discriminator; scrub status is never consulted.
pub fn cmd_scrub_resume_or_start<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> Result<ScrubResumeOrStartResult, ScrubResumeOrStartError> {
    // Remove any stale marker so only a cancel requested *during this run*
    // counts. The entry-remove runs when the scrub first starts (long before
    // any stop), so the marker is present at the post-exit check below iff a
    // teardown is in flight for *this* run.
    clear_stale_cancel_marker(paths)?;

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
        2 => start_scrub(runner, mount_point, paths),
        _ => classify_btrfs_failure(
            paths,
            ScrubResumeOrStartError::ResumeFailed {
                stderr: resume_raw.stderr,
            },
        ),
    }
}

fn start_scrub<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    paths: &StatePaths,
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
        _ => classify_btrfs_failure(
            paths,
            ScrubResumeOrStartError::StartFailed {
                stderr: start_raw.stderr,
            },
        ),
    }
}

/// Remove a stale cancel-request marker at entry, tolerating *only* `NotFound`.
///
/// Fail-closed per [safety-heuristics.md](../../docs/dev/safety-heuristics.md):
/// the "no marker" sibling proceeds, but any other removal error is a hard
/// error before btrfs runs, because a marker this run could not clear would
/// later turn a genuine exit 1 into `Cancelled` and swallow the failure.
fn clear_stale_cancel_marker(paths: &StatePaths) -> Result<(), ScrubResumeOrStartError> {
    match std::fs::remove_file(paths.scrub_cancel_requested()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ScrubResumeOrStartError::MarkerCleanupFailed { source }),
    }
}

/// Classify an ambiguous btrfs exit (outside `{0,2,3}`) into a clean cancel or
/// a genuine failure, keyed solely on the cancel-request marker.
///
/// `Path::exists()` coerces any I/O error to `false`, so the only route to
/// `Cancelled` is an unambiguously present marker; absence *or* any read
/// ambiguity falls through to the failure error -> alert (fail-closed here
/// too). Shared by the resume and start arms so both classify identically.
fn classify_btrfs_failure(
    paths: &StatePaths,
    failure: ScrubResumeOrStartError,
) -> Result<ScrubResumeOrStartResult, ScrubResumeOrStartError> {
    if paths.scrub_cancel_requested().exists() {
        Ok(ScrubResumeOrStartResult::Cancelled)
    } else {
        Err(failure)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::test_fixtures::{isolated_paths, scrub_mp, scrub_resume_output, scrub_start_output};

    /// A `with_handler` that models the ExecStop teardown: when btrfs runs the
    /// given scrub command, write the cancel-request marker (as `touch` does)
    /// and return exit 1 -- the marker appears *during* the run, after the
    /// entry-clear, exactly as a real lock/suspend/shutdown produces it.
    fn cancel_during(
        marker: std::path::PathBuf,
        is_match: fn(&CmdRequest) -> bool,
        cmd: &'static str,
    ) -> impl Fn(&CmdRequest) -> Option<Result<RawCommandOutput, CmdError>> + Send + Sync + 'static
    {
        move |req: &CmdRequest| {
            if is_match(req) {
                std::fs::write(&marker, b"").unwrap();
                Some(Ok(RawCommandOutput {
                    cmd: cmd.to_owned(),
                    stdout: String::new(),
                    stderr: "ERROR: scrub cancelled\n".to_owned(),
                    exit_status: 1,
                }))
            } else {
                None
            }
        }
    }

    #[test]
    // Intent: resume exit 0 returns Resumed without falling back to start.
    // Why it exists: scheduled scrub should continue saved work before starting fresh.
    // Scenario: monthly timer fires while cancelled scrub progress exists.
    fn resume_succeeds_returns_resumed() {
        let (_dir, paths) = isolated_paths();
        let (resume_req, resume_out) = scrub_resume_output(0);
        let runner = MockRunner::default().with_output(resume_req, resume_out);

        let result = cmd_scrub_resume_or_start(&runner, &scrub_mp(), &paths).unwrap();
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
        let (_dir, paths) = isolated_paths();
        let (resume_req, resume_out) = scrub_resume_output(3);
        let runner = MockRunner::default().with_output(resume_req, resume_out);

        let result = cmd_scrub_resume_or_start(&runner, &scrub_mp(), &paths).unwrap();
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
        let (_dir, paths) = isolated_paths();
        let (resume_req, resume_out) = scrub_resume_output(2);
        let (start_req, start_out) = scrub_start_output(0);
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_output(start_req, start_out);

        let result = cmd_scrub_resume_or_start(&runner, &scrub_mp(), &paths).unwrap();
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
        let (_dir, paths) = isolated_paths();
        let (resume_req, resume_out) = scrub_resume_output(2);
        let (start_req, start_out) = scrub_start_output(3);
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_output(start_req, start_out);

        let result = cmd_scrub_resume_or_start(&runner, &scrub_mp(), &paths).unwrap();
        assert_eq!(
            result,
            ScrubResumeOrStartResult::Started {
                uncorrectable_errors: true
            }
        );
    }

    #[test]
    // Intent: resume exit 1 with NO cancel-request marker propagates as
    //   ResumeFailed (no scrub-status probe involved).
    // Why it exists: only "nothing to resume" is a fallback condition, and a
    //   genuine fatal scrub error -- which btrfs also reports as exit 1 -- must
    //   alert, not be mistaken for a cancel.
    // Scenario: btrfs cannot read the saved scrub state file; no teardown is in
    //   flight, so no marker exists.
    fn resume_real_failure_propagates() {
        let (_dir, paths) = isolated_paths();
        let (resume_req, resume_out) = scrub_resume_output(1);
        let runner = MockRunner::default().with_output(resume_req, resume_out);

        let result = cmd_scrub_resume_or_start(&runner, &scrub_mp(), &paths);
        assert!(
            matches!(result, Err(ScrubResumeOrStartError::ResumeFailed { .. })),
            "expected ResumeFailed, got {result:?}"
        );
    }

    #[test]
    // Intent: start exit 1 after fallback with NO marker propagates as
    //   StartFailed.
    // Why it exists: real fresh-start failures must fail the scrub service so
    //   onFailure fires; a marker-absent exit 1 is never a cancel.
    // Scenario: timer fires but btrfs cannot start a fresh scrub; no teardown.
    fn start_real_failure_propagates() {
        let (_dir, paths) = isolated_paths();
        let (resume_req, resume_out) = scrub_resume_output(2);
        let (start_req, start_out) = scrub_start_output(1);
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_output(start_req, start_out);

        let result = cmd_scrub_resume_or_start(&runner, &scrub_mp(), &paths);
        assert!(
            matches!(result, Err(ScrubResumeOrStartError::StartFailed { .. })),
            "expected StartFailed, got {result:?}"
        );
    }

    #[test]
    // Intent: btrfs exit 1 with the cancel-request marker present (written
    //   during the run, as ExecStop does) returns Ok(Cancelled) on the resume
    //   arm.
    // Why it exists: btrfs exits 1 for BOTH a real cancel and a genuine
    //   failure, so onFailure would beep on every lock/suspend/shutdown without
    //   the marker discriminator. The marker is the only authoritative cancel
    //   signal.
    // Scenario: `braid lock` mid-scrub; ExecStop wrote the marker just before
    //   the cancel ioctl made `btrfs scrub resume` exit 1.
    fn cancelled_when_marker_present_resume() {
        let (_dir, paths) = isolated_paths();
        let runner = MockRunner::default().with_handler(cancel_during(
            paths.scrub_cancel_requested(),
            |req| matches!(req, CmdRequest::BtrfsScrubResume { .. }),
            "btrfs scrub resume -B /mnt/storage",
        ));

        let result = cmd_scrub_resume_or_start(&runner, &scrub_mp(), &paths).unwrap();
        assert_eq!(result, ScrubResumeOrStartResult::Cancelled);
    }

    #[test]
    // Intent: btrfs exit 1 with the marker present returns Ok(Cancelled) on the
    //   start-after-fallback arm too.
    // Why it exists: the start arm shares the marker discrimination with the
    //   resume arm; a teardown during a fresh scrub (resume returned 2) must be
    //   classified the same way.
    // Scenario: timer fires with nothing to resume; the fresh `btrfs scrub
    //   start` is cancelled mid-run by suspend, exiting 1 with the marker set.
    fn cancelled_when_marker_present_start() {
        let (_dir, paths) = isolated_paths();
        let (resume_req, resume_out) = scrub_resume_output(2);
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_handler(cancel_during(
                paths.scrub_cancel_requested(),
                |req| matches!(req, CmdRequest::BtrfsScrubStart { .. }),
                "btrfs scrub start -B /mnt/storage",
            ));

        let result = cmd_scrub_resume_or_start(&runner, &scrub_mp(), &paths).unwrap();
        assert_eq!(result, ScrubResumeOrStartResult::Cancelled);
    }

    #[test]
    // Intent: btrfs exit 1 with NO marker is a genuine failure -> Err.
    // Why it exists: the F2 regression -- a genuine fatal scrub error also sets
    //   btrfs `canceled=1` (so the old `Aborted`-based rule would have swallowed
    //   it), yet it must still alert. The marker, not scrub status, is the sole
    //   discriminator.
    // Scenario: a real btrfs internal error aborts the scrub with exit 1 while
    //   no teardown is in flight, so no marker is written.
    fn failure_when_marker_absent() {
        let (_dir, paths) = isolated_paths();
        let (resume_req, resume_out) = scrub_resume_output(1);
        let runner = MockRunner::default().with_output(resume_req, resume_out);

        let result = cmd_scrub_resume_or_start(&runner, &scrub_mp(), &paths);
        assert!(
            matches!(result, Err(ScrubResumeOrStartError::ResumeFailed { .. })),
            "marker-absent exit 1 must be a genuine failure, got {result:?}"
        );
    }

    #[test]
    // Intent: a marker left from a PRIOR run is cleared at entry, so a genuine
    //   exit-1 failure this run (no ExecStop re-write) still alerts.
    // Why it exists: without the fail-closed entry-clear, a stale marker would
    //   turn this run's real failure into Ok(Cancelled) and silently swallow
    //   the very failure the feature exists to alert on.
    // Scenario: a previous lock/suspend left the marker on disk; this run hits a
    //   genuine btrfs error with no teardown in flight.
    fn stale_marker_removed_at_entry() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.scrub_cancel_requested(), b"").unwrap();
        let (resume_req, resume_out) = scrub_resume_output(1);
        let runner = MockRunner::default().with_output(resume_req, resume_out);

        let result = cmd_scrub_resume_or_start(&runner, &scrub_mp(), &paths);
        assert!(
            matches!(result, Err(ScrubResumeOrStartError::ResumeFailed { .. })),
            "stale marker must be cleared so a genuine failure still alerts, got {result:?}"
        );
        assert!(
            !paths.scrub_cancel_requested().exists(),
            "entry-clear must have removed the stale marker"
        );
    }

    #[test]
    // Intent: an un-removable cancel-request marker fails closed with
    //   MarkerCleanupFailed *before* btrfs runs.
    // Why it exists: if entry cleanup cannot guarantee a clean slate, a
    //   surviving stale marker could later mask a real exit 1 as Cancelled. The
    //   command must therefore refuse to start the scrub. Regression for the
    //   fail-closed entry-cleanup policy.
    // Scenario: a directory sits at the marker path (test scaffolding or
    //   operator error), so remove_file returns EISDIR/EPERM, not NotFound.
    fn fails_closed_when_marker_unremovable() {
        let (_dir, paths) = isolated_paths();
        std::fs::create_dir(paths.scrub_cancel_requested()).unwrap();
        // No registered output: any runner.run would surface as MissingMock, so
        // the returned MarkerCleanupFailed proves cleanup short-circuited.
        let runner = MockRunner::default();

        let result = cmd_scrub_resume_or_start(&runner, &scrub_mp(), &paths);
        assert!(
            matches!(
                result,
                Err(ScrubResumeOrStartError::MarkerCleanupFailed { .. })
            ),
            "unremovable marker must fail closed, got {result:?}"
        );
        assert!(
            runner.requests().is_empty(),
            "no btrfs command may run when entry cleanup fails"
        );
    }
}
