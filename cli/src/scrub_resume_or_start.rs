use crate::cmd::{CmdError, CmdRequest, CommandRunner, PendingCommand, RawCommandOutput};
use crate::filesystem::Filesystem;
use crate::parse::{ScrubState, parse_btrfs_scrub_status};
use crate::pool_lock::{PoolLockError, RealPoolLock, RealPoolLockGuard};
use crate::preflight::{
    ExclusiveOpError, check_any_btrfs_exclusive_op, check_no_pending_operation,
};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;
use std::time::{Duration, Instant};

/// How long the gate waits for the kernel to register the scrub it just
/// spawned, and how often it asks.
///
/// A value, not a constant, so tests can drive the poll deterministically. Any
/// deadline comfortably above scrub startup and well below the service's retry
/// interval leaves observable behavior unchanged -- it only bounds how long a
/// mutation can be kept waiting on the pool lock when `btrfs scrub` is slow to
/// reach its ioctl.
#[derive(Debug, Clone)]
pub struct ConfirmPoll {
    pub interval: Duration,
    pub deadline: Duration,
}

impl Default for ConfirmPoll {
    fn default() -> Self {
        Self {
            interval: Duration::from_millis(250),
            deadline: Duration::from_secs(60),
        }
    }
}

/// Everything the scheduled-scrub runner needs, grouped because the gate spans
/// four seams (subprocesses, sysfs, the state dir, and the pool lock) that all
/// have to be swappable together under test.
pub struct ScrubRunParams<'a, R: CommandRunner, F: Filesystem + ?Sized> {
    pub runner: &'a R,
    pub fs: &'a F,
    pub mount_point: &'a MountPoint,
    pub paths: &'a StatePaths,
    pub pool_lock: &'a RealPoolLock,
    pub confirm: ConfirmPoll,
}

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
    /// The pool was busy with braid's own work, so no scrub was started and
    /// nothing else was touched. Distinct from every failure variant: a skip is
    /// not a problem to alert on, it is a scrub that still owes a run, recorded
    /// durably by the deferred flag before this is returned.
    Skipped {
        reason: String,
    },
}

#[derive(Debug, thiserror::Error)]
pub enum ScrubResumeOrStartError {
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("btrfs scrub resume failed: {stderr}")]
    ResumeFailed { stderr: String },
    #[error("btrfs scrub start failed: {stderr}")]
    StartFailed { stderr: String },
    /// The pool lock could not be evaluated at all (an I/O error on
    /// `/run/braid-pool.lock`, not a lock held by a peer -- that is a skip).
    /// Passed through verbatim: `PoolLockError` already renders operator-ready
    /// wording.
    #[error("{0}")]
    PoolLock(#[from] PoolLockError),
    /// The sysfs exclusive-operation state could not be read or was
    /// unrecognized. Deliberately NOT a skip: mapping probe breakage to "busy,
    /// try later" would starve scrubs indefinitely with no alert, so an
    /// unreadable gate fails the service loudly instead.
    #[error(
        "cannot determine btrfs exclusive-operation state: {source}. \
         Refusing to run a scheduled scrub without it."
    )]
    ExclusiveOpUnknown { source: ExclusiveOpError },
    /// The durable deferred-scrub flag could not be written, cleared, or
    /// inspected. Fail-closed for the same reason as the cancel marker below: a
    /// best-effort write that silently fails strands the scrub until the next
    /// calendar firing, and a best-effort clear re-starts a fresh scrub on every
    /// later pool-online.
    #[error("could not {action} the deferred-scrub flag: {source}")]
    DeferredFlag {
        action: &'static str,
        source: std::io::Error,
    },
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
/// Before any of that it runs the busy gate (see [`gate`]): a scheduled scrub
/// must never pile onto a pool that braid is already mutating, and a scrub
/// started during a `btrfs replace` is kernel-rejected and would spuriously
/// alert. A tripped gate is a `Skipped`, not a failure -- systemd retries it.
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
pub fn cmd_scrub_resume_or_start<R: CommandRunner, F: Filesystem + ?Sized>(
    p: &ScrubRunParams<'_, R, F>,
) -> Result<ScrubResumeOrStartResult, ScrubResumeOrStartError> {
    // The gate runs before the marker cleanup so a skip leaves the cancel
    // marker and saved scrub state exactly as it found them.
    let guard = match gate(p)? {
        GateOutcome::Busy(reason) => {
            // Fail-closed ordering: the skip is only *reported* once the
            // deferral is durable, so an unwritable state dir alerts instead of
            // silently losing the scrub until the next calendar firing.
            record_deferral(p.paths)?;
            return Ok(ScrubResumeOrStartResult::Skipped { reason });
        }
        GateOutcome::Clear(guard) => guard,
    };

    // A real scrub run is beginning, so nothing is owed any more.
    clear_deferral(p.paths)?;

    // Remove any stale marker so only a cancel requested *during this run*
    // counts. The entry-remove runs when the scrub first starts (long before
    // any stop), so the marker is present at the post-exit check below iff a
    // teardown is in flight for *this* run.
    clear_stale_cancel_marker(p.paths)?;

    let (guard, resume_raw) = run_scrub_under_guard(
        p,
        Some(guard),
        CmdRequest::BtrfsScrubResume {
            mount_point: p.mount_point.clone(),
        },
    )?;

    match resume_raw.exit_status {
        0 => Ok(ScrubResumeOrStartResult::Resumed {
            uncorrectable_errors: false,
        }),
        3 => Ok(ScrubResumeOrStartResult::Resumed {
            uncorrectable_errors: true,
        }),
        // Exit 2 ends no run -- it is the fallback into `start -B` -- so the
        // guard is carried through rather than released and re-taken, which
        // would leave the fallback start ungated.
        2 => start_scrub(p, guard),
        _ => classify_btrfs_failure(
            p.paths,
            ScrubResumeOrStartError::ResumeFailed {
                stderr: resume_raw.stderr,
            },
        ),
    }
}

/// Why the scheduled scrub is not starting right now, or the pool lock proving
/// it may.
enum GateOutcome {
    Clear(RealPoolLockGuard),
    Busy(String),
}

/// Decide whether a scheduled scrub may start, and if so hand back the pool
/// lock that keeps it true.
///
/// Three conditions, all of them "braid is already working on this pool":
/// the pool lock held by another braid process, any btrfs exclusive operation
/// in flight (running *or* paused), and an interrupted-operation journal. The
/// lock is checked first and returned held, because it is the only one of the
/// three that also covers the LUKS work a mutator does *before* any btrfs
/// exclusive operation exists for sysfs to see.
///
/// Classification is asymmetric on purpose (ADR 018): busy is a skip, but an
/// unreadable gate is a hard error.
fn gate<R: CommandRunner, F: Filesystem + ?Sized>(
    p: &ScrubRunParams<'_, R, F>,
) -> Result<GateOutcome, ScrubResumeOrStartError> {
    let guard = match p.pool_lock.acquire() {
        Ok(guard) => guard,
        Err(PoolLockError::AlreadyHeld) => {
            return Ok(GateOutcome::Busy(
                "another braid operation holds the pool lock".to_owned(),
            ));
        }
        Err(e) => return Err(ScrubResumeOrStartError::PoolLock(e)),
    };

    match check_any_btrfs_exclusive_op(p.fs) {
        Ok(()) => {}
        Err(ExclusiveOpError::Busy(op)) => {
            return Ok(GateOutcome::Busy(format!("btrfs {op} is in progress")));
        }
        Err(source) => return Err(ScrubResumeOrStartError::ExclusiveOpUnknown { source }),
    }

    // An unreadable journal counts as present, exactly as it does for the
    // mutating commands: the remediation is `braid recover` either way.
    if let Err(msg) = check_no_pending_operation(p.paths) {
        let reason = msg
            .lines()
            .next()
            .unwrap_or("interrupted operation pending");
        return Ok(GateOutcome::Busy(reason.to_owned()));
    }

    Ok(GateOutcome::Clear(guard))
}

fn start_scrub<R: CommandRunner, F: Filesystem + ?Sized>(
    p: &ScrubRunParams<'_, R, F>,
    guard: Option<RealPoolLockGuard>,
) -> Result<ScrubResumeOrStartResult, ScrubResumeOrStartError> {
    let (_guard, start_raw) = run_scrub_under_guard(
        p,
        guard,
        CmdRequest::BtrfsScrubStart {
            mount_point: p.mount_point.clone(),
        },
    )?;

    match start_raw.exit_status {
        0 => Ok(ScrubResumeOrStartResult::Started {
            uncorrectable_errors: false,
        }),
        3 => Ok(ScrubResumeOrStartResult::Started {
            uncorrectable_errors: true,
        }),
        _ => classify_btrfs_failure(
            p.paths,
            ScrubResumeOrStartError::StartFailed {
                stderr: start_raw.stderr,
            },
        ),
    }
}

/// Spawn one btrfs scrub command, hold the gate's pool lock until the kernel
/// has accepted it, then reap that same child for its authoritative exit code.
///
/// The spawn/wait split exists because `btrfs scrub` is outside the kernel's
/// exclusive-operation set: between `fork` and `scrub_start`'s ioctl the child
/// opens the mount and queries the fs, and nothing braid can look at says a
/// scrub is coming. Releasing the lock at spawn would therefore let a mutation
/// slip in ahead of the scrub the gate just cleared.
///
/// Returns the guard iff it is still held, so the resume-exit-2 fallback can
/// carry the same guard into the fresh `start`.
fn run_scrub_under_guard<R: CommandRunner, F: Filesystem + ?Sized>(
    p: &ScrubRunParams<'_, R, F>,
    guard: Option<RealPoolLockGuard>,
    request: CmdRequest,
) -> Result<(Option<RealPoolLockGuard>, RawCommandOutput), ScrubResumeOrStartError> {
    let mut pending = p.runner.spawn(&request)?;
    let guard = match guard {
        Some(guard) => confirm_scrub_registered(p, guard, pending.as_mut()),
        // Already released on an earlier unconfirmable start (AR1/AR2); there
        // is nothing left to protect, so do not re-take it.
        None => None,
    };
    let raw = pending.wait()?;
    Ok((guard, raw))
}

/// Hold `guard` until the scrub is either registered with the kernel or can no
/// longer become registered under it, then drop it.
///
/// Terminal outcomes, in the order the loop tests them:
/// 1. scrub status reports `Running` -- the kernel has it; release so mutations
///    may overlap the run, exactly as they do for a scrub already in flight.
/// 2. the child exited -- keep the guard: its exit code decides the run, and a
///    resume that exits 2 still owes a `start` under this same lock.
/// 3. the status probe cannot classify, or the deadline expires -- release and
///    log (AR2). Blocking every mutation for the scrub's multi-hour run on a
///    parser break would be far worse than the residual overlap.
fn confirm_scrub_registered<R: CommandRunner, F: Filesystem + ?Sized>(
    p: &ScrubRunParams<'_, R, F>,
    guard: RealPoolLockGuard,
    pending: &mut dyn PendingCommand,
) -> Option<RealPoolLockGuard> {
    let start = Instant::now();
    loop {
        match observe_scrub_running(p) {
            Ok(true) => return None,
            Ok(false) => {}
            Err(reason) => {
                eprintln!(
                    "braid: scrub start not confirmed ({reason}); releasing the pool lock -- \
                     a braid operation may overlap this scrub"
                );
                return None;
            }
        }
        if pending.has_exited() {
            return Some(guard);
        }
        if start.elapsed() >= p.confirm.deadline {
            eprintln!(
                "braid: scrub start not confirmed within {:?}; releasing the pool lock -- \
                 a braid operation may overlap this scrub",
                p.confirm.deadline
            );
            return None;
        }
        std::thread::sleep(p.confirm.interval);
    }
}

/// One `btrfs scrub status` probe reduced to "is a scrub registered": `Err`
/// carries the reason the question could not be answered, never a guess.
fn observe_scrub_running<R: CommandRunner, F: Filesystem + ?Sized>(
    p: &ScrubRunParams<'_, R, F>,
) -> Result<bool, String> {
    let raw = p
        .runner
        .run(&CmdRequest::BtrfsScrubStatus {
            mount_point: p.mount_point.clone(),
        })
        .map_err(|e| e.to_string())?;
    let status = parse_btrfs_scrub_status(&raw).map_err(|e| e.to_string())?;
    match status.state {
        ScrubState::Running { .. } => Ok(true),
        ScrubState::Never
        | ScrubState::Finished { .. }
        | ScrubState::Aborted { .. }
        | ScrubState::Interrupted { .. } => Ok(false),
        ScrubState::Unknown => Err("unrecognized btrfs scrub status".to_owned()),
    }
}

/// Record that a scheduled scrub is still owed, durably enough to survive the
/// reboot that would otherwise discard systemd's pending restart.
fn record_deferral(paths: &StatePaths) -> Result<(), ScrubResumeOrStartError> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(paths.scrub_deferred())
        .map(|_| ())
        .map_err(|source| ScrubResumeOrStartError::DeferredFlag {
            action: "record",
            source,
        })
}

/// Drop the deferral because a real scrub run is starting now, tolerating only
/// `NotFound`: a clear that silently failed would re-start a fresh scrub on
/// every later pool-online.
fn clear_deferral(paths: &StatePaths) -> Result<(), ScrubResumeOrStartError> {
    match std::fs::remove_file(paths.scrub_deferred()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(source) => Err(ScrubResumeOrStartError::DeferredFlag {
            action: "clear",
            source,
        }),
    }
}

/// Is a scheduled scrub still owed? Shared with `braid scrub-needs-resume`, the
/// pool-online resume predicate, so the flag has exactly one reader.
///
/// Presence is the whole signal -- the contents are never read -- and only
/// `NotFound` means absent. Any other inspection error propagates, because
/// "cannot tell" must never be reported as "nothing pending".
pub fn scrub_deferral_pending(paths: &StatePaths) -> Result<bool, std::io::Error> {
    match std::fs::symlink_metadata(paths.scrub_deferred()) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
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
    use crate::test_fixtures::{
        IdleMockFs, isolated_paths, scrub_mp, scrub_resume_output, scrub_start_output,
        scrub_status_never, scrub_status_running, scrub_status_unknown,
    };
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;

    /// Sysfs surface with one btrfs filesystem and no exclusive operation --
    /// the gate-clear baseline for every non-gate test below.
    fn clear_fs() -> IdleMockFs {
        IdleMockFs::with_exclop("none")
    }

    /// Confirm poll fast enough that a test never actually waits: the mock
    /// child is always already exited, so one probe decides the outcome.
    fn fast_confirm() -> ConfirmPoll {
        ConfirmPoll {
            interval: Duration::from_millis(1),
            deadline: Duration::from_millis(50),
        }
    }

    /// Test rig owning the temp state dir, its pool-lock path, and the sysfs
    /// double, so each test only writes the parts it is asserting on.
    struct Rig {
        _dir: TempDir,
        paths: StatePaths,
        lock_path: PathBuf,
        lock: RealPoolLock,
        fs: IdleMockFs,
        mount_point: MountPoint,
    }

    impl Rig {
        fn new() -> Self {
            Self::with_fs(clear_fs())
        }

        fn with_fs(fs: IdleMockFs) -> Self {
            let (dir, paths) = isolated_paths();
            let lock_path = dir.path().join("pool.lock");
            Self {
                _dir: dir,
                paths,
                lock: RealPoolLock::new(lock_path.clone()),
                lock_path,
                fs,
                mount_point: scrub_mp(),
            }
        }

        fn params<'a>(
            &'a self,
            runner: &'a MockRunner,
        ) -> ScrubRunParams<'a, MockRunner, IdleMockFs> {
            ScrubRunParams {
                runner,
                fs: &self.fs,
                mount_point: &self.mount_point,
                paths: &self.paths,
                pool_lock: &self.lock,
                confirm: fast_confirm(),
            }
        }

        fn run(
            &self,
            runner: &MockRunner,
        ) -> Result<ScrubResumeOrStartResult, ScrubResumeOrStartError> {
            cmd_scrub_resume_or_start(&self.params(runner))
        }

        /// Can a *peer* braid process take the pool lock right now? flock is
        /// per-open-file-description, so a second handle in this process
        /// observes exactly what another process would.
        fn lock_is_free(&self) -> bool {
            RealPoolLock::new(self.lock_path.clone()).acquire().is_ok()
        }
    }

    /// A `with_handler` that records whether the pool lock was free at the
    /// moment btrfs was spawned, then returns the given output. This is how the
    /// tests observe the guard's lifetime from outside.
    fn observing_lock(
        observed: Arc<Mutex<Vec<bool>>>,
        lock_path: PathBuf,
        is_match: fn(&CmdRequest) -> bool,
        out: RawCommandOutput,
    ) -> impl Fn(&CmdRequest) -> Option<Result<RawCommandOutput, CmdError>> + Send + Sync + 'static
    {
        move |req: &CmdRequest| {
            if is_match(req) {
                let free = RealPoolLock::new(lock_path.clone()).acquire().is_ok();
                observed.lock().unwrap().push(free);
                Some(Ok(out.clone()))
            } else {
                None
            }
        }
    }

    fn is_start(req: &CmdRequest) -> bool {
        matches!(req, CmdRequest::BtrfsScrubStart { .. })
    }

    fn write_journal(paths: &StatePaths) {
        let journal = crate::journal::build_journal(
            crate::membership::PoolMembership::empty(),
            crate::membership::PoolMembership::empty(),
            crate::journal::OpKind::Add {
                phase: crate::journal::AddPhase::PoolMutation,
                targets: crate::membership::LuksUuidMap::new(),
            },
        );
        crate::journal::write_journal(paths, &journal).unwrap();
    }

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

    // -----------------------------------------------------------------------
    // Gate: busy pools skip, unreadable gates fail (I1, I4, I6)
    // -----------------------------------------------------------------------

    #[test]
    // Intent: a paused balance makes the scheduled scrub skip without running
    //   any btrfs command, without touching the cancel marker, and with the
    //   deferred flag recorded.
    // Why it exists: the caja incident -- a `braid add` convert balance was
    //   mid-flight with the monthly scrub due at midnight, and nothing stopped
    //   the scrub from piling onto the same spindles. A paused balance is the
    //   sharpest case: sysfs still reports it, so "paused" must count as busy.
    // Scenario: operator paused an add's convert balance overnight; the timer
    //   fires at 00:00.
    fn skips_when_balance_paused() {
        let rig = Rig::with_fs(IdleMockFs::with_exclop("balance paused"));
        std::fs::write(rig.paths.scrub_cancel_requested(), b"stale").unwrap();
        let runner = MockRunner::default();

        let result = rig.run(&runner).unwrap();
        assert!(
            matches!(result, ScrubResumeOrStartResult::Skipped { .. }),
            "paused balance must skip, got {result:?}"
        );
        assert!(
            runner.requests().is_empty(),
            "a skip must issue no btrfs command"
        );
        assert!(
            rig.paths.scrub_cancel_requested().exists(),
            "a skip must leave the cancel marker untouched"
        );
        assert!(
            scrub_deferral_pending(&rig.paths).unwrap(),
            "a skip must durably record that a scrub is still owed"
        );
        assert!(rig.lock_is_free(), "the gate must release the pool lock");
    }

    #[test]
    // Intent: a running device replace also skips.
    // Why it exists: a scheduled scrub firing during `btrfs replace` is
    //   kernel-rejected, exits 1, and spuriously fires the scrub-failed alert
    //   path -- the second half of the bug this gate closes.
    // Scenario: `braid replace` is rebuilding a disk when the monthly timer
    //   fires.
    fn skips_when_device_replace_running() {
        let rig = Rig::with_fs(IdleMockFs::with_exclop("device replace"));
        let runner = MockRunner::default();

        let result = rig.run(&runner).unwrap();
        assert!(
            matches!(result, ScrubResumeOrStartResult::Skipped { .. }),
            "device replace must skip, got {result:?}"
        );
        assert!(runner.requests().is_empty());
    }

    #[test]
    // Intent: an interrupted-operation journal skips the scrub.
    // Why it exists: pending-op.json means membership may be inconsistent; a
    //   scrub then competes with the `braid recover` the operator has to run.
    // Scenario: an add was interrupted by a power cut; the timer fires before
    //   anyone ran `braid recover`.
    fn skips_when_pending_operation_present() {
        let rig = Rig::new();
        write_journal(&rig.paths);
        let runner = MockRunner::default();

        let result = rig.run(&runner).unwrap();
        assert!(
            matches!(result, ScrubResumeOrStartResult::Skipped { .. }),
            "pending op must skip, got {result:?}"
        );
        assert!(runner.requests().is_empty());
        assert!(scrub_deferral_pending(&rig.paths).unwrap());
    }

    #[test]
    // Intent: an unreadable/malformed journal counts as present, so the scrub
    //   still skips rather than starting.
    // Why it exists: fail-closed -- "cannot tell whether an operation was
    //   interrupted" must never resolve to "go ahead and scrub", the same
    //   condition every mutating command already treats as blocking.
    // Scenario: pending-op.json was truncated by a crash mid-write.
    fn skips_when_pending_operation_unreadable() {
        let rig = Rig::new();
        std::fs::write(rig.paths.pending_op_json(), "not json").unwrap();
        let runner = MockRunner::default();

        let result = rig.run(&runner).unwrap();
        assert!(
            matches!(result, ScrubResumeOrStartResult::Skipped { .. }),
            "corrupt journal must skip, got {result:?}"
        );
        assert!(runner.requests().is_empty());
    }

    #[test]
    // Intent: a pool lock held by another braid process is itself a skip
    //   reason, with no btrfs command issued.
    // Why it exists: the lock is the only gate that covers the LUKS work a
    //   mutator does *before* any btrfs exclusive operation exists for sysfs to
    //   see -- without it the scrub can start inside that window.
    // Scenario: `braid add` is still formatting the new disk's LUKS header when
    //   the monthly timer fires.
    fn skips_when_pool_lock_held_by_peer() {
        let rig = Rig::new();
        let _peer = RealPoolLock::new(rig.lock_path.clone()).acquire().unwrap();
        let runner = MockRunner::default();

        let result = rig.run(&runner).unwrap();
        assert!(
            matches!(result, ScrubResumeOrStartResult::Skipped { .. }),
            "held pool lock must skip, got {result:?}"
        );
        assert!(runner.requests().is_empty());
        assert!(scrub_deferral_pending(&rig.paths).unwrap());
    }

    #[test]
    // Intent: an unreadable sysfs exclusive-operation state is a hard error,
    //   not a skip.
    // Why it exists: I4's deliberate asymmetry. Mapping probe breakage to
    //   "busy, retry later" would starve scrubs forever with no alert, which is
    //   exactly the silent-no-scrub failure mode braid exists to prevent.
    // Scenario: a kernel/sysfs change makes exclusive_operation unreadable.
    fn unreadable_exclusive_op_is_error_not_skip() {
        let rig = Rig::with_fs(IdleMockFs::with_exclop_read_error(
            std::io::ErrorKind::PermissionDenied,
        ));
        let runner = MockRunner::default();

        let result = rig.run(&runner);
        assert!(
            matches!(
                result,
                Err(ScrubResumeOrStartError::ExclusiveOpUnknown { .. })
            ),
            "unreadable sysfs must be an error, got {result:?}"
        );
        assert!(runner.requests().is_empty());
        assert!(
            !scrub_deferral_pending(&rig.paths).unwrap(),
            "a hard failure alerts; it must not masquerade as a deferral"
        );
    }

    #[test]
    // Intent: an unrecognized exclusive-operation value is a hard error too.
    // Why it exists: same asymmetry, other half -- parser drift against a newer
    //   kernel must alert rather than silently disable scrubbing.
    // Scenario: a future kernel adds an exclusive operation braid's parser does
    //   not know.
    fn unrecognized_exclusive_op_is_error_not_skip() {
        let rig = Rig::with_fs(IdleMockFs::with_exclop("quantum defrag"));
        let runner = MockRunner::default();

        let result = rig.run(&runner);
        assert!(
            matches!(
                result,
                Err(ScrubResumeOrStartError::ExclusiveOpUnknown { .. })
            ),
            "unrecognized sysfs state must be an error, got {result:?}"
        );
    }

    // -----------------------------------------------------------------------
    // Deferred flag is fail-closed (I3)
    // -----------------------------------------------------------------------

    #[test]
    // Intent: a deferred-flag write that fails turns the skip into a hard
    //   error (exit 1, alert) rather than a silent skip.
    // Why it exists: exit 4 promises "this scrub will be retried". Reporting it
    //   without the durable record would strand the scrub until the next
    //   calendar firing if the machine reboots.
    // Scenario: a directory sits at the flag path, so the write cannot succeed.
    fn deferral_write_failure_is_error_not_skip() {
        let rig = Rig::with_fs(IdleMockFs::with_exclop("balance"));
        std::fs::create_dir(rig.paths.scrub_deferred()).unwrap();
        let runner = MockRunner::default();

        let result = rig.run(&runner);
        assert!(
            matches!(
                result,
                Err(ScrubResumeOrStartError::DeferredFlag {
                    action: "record",
                    ..
                })
            ),
            "unwritable deferral must fail closed, got {result:?}"
        );
    }

    #[test]
    // Intent: a deferred-flag clear that fails stops the run before btrfs is
    //   spawned.
    // Why it exists: a best-effort clear that silently failed would re-start a
    //   fresh scrub on every later pool-online, forever.
    // Scenario: a directory sits at the flag path on an otherwise clear gate.
    fn deferral_clear_failure_blocks_the_scrub() {
        let rig = Rig::new();
        std::fs::create_dir(rig.paths.scrub_deferred()).unwrap();
        let runner = MockRunner::default();

        let result = rig.run(&runner);
        assert!(
            matches!(
                result,
                Err(ScrubResumeOrStartError::DeferredFlag {
                    action: "clear",
                    ..
                })
            ),
            "unclearable deferral must fail closed, got {result:?}"
        );
        assert!(
            runner.requests().is_empty(),
            "no btrfs command may run when the deferral cannot be cleared"
        );
    }

    #[test]
    // Intent: a flag-inspection error other than NotFound propagates instead of
    //   reporting "no deferral pending".
    // Why it exists: the resume predicate reads this. "Cannot tell" answered as
    //   "nothing owed" would silently drop the post-reboot retry -- the exact
    //   hole the flag exists to close.
    // Scenario: the state root is a regular file, so the lookup fails ENOTDIR.
    fn deferral_inspection_error_propagates() {
        let dir = TempDir::new().unwrap();
        let not_a_dir = dir.path().join("state");
        std::fs::write(&not_a_dir, b"").unwrap();
        let paths = StatePaths::custom(not_a_dir);

        let result = scrub_deferral_pending(&paths);
        assert!(
            result.is_err(),
            "an unclassifiable flag lookup must not report absent, got {result:?}"
        );
    }

    #[test]
    // Intent: a real scrub run clears a stale deferred flag.
    // Why it exists: without the clear, every later pool-online would see a
    //   deferral still owed and start another scrub.
    // Scenario: last night's skip left the flag; the balance finished and the
    //   retry now runs the scrub for real.
    fn real_run_clears_the_deferred_flag() {
        let rig = Rig::new();
        std::fs::write(rig.paths.scrub_deferred(), b"").unwrap();
        let (resume_req, resume_out) = scrub_resume_output(0);
        let (status_req, status_out) = scrub_status_running();
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_output(status_req, status_out);

        rig.run(&runner).unwrap();
        assert!(
            !scrub_deferral_pending(&rig.paths).unwrap(),
            "a real run must clear the deferral"
        );
    }

    // -----------------------------------------------------------------------
    // Pool lock lifetime across spawn/confirm/wait (I1, I5)
    // -----------------------------------------------------------------------

    #[test]
    // Intent: the pool lock is still held when the resume-exit-2 fallback
    //   spawns the fresh `btrfs scrub start`.
    // Why it exists: `resume -B` exit 2 ends no run, so releasing at child exit
    //   would leave the fallback start -- the scrub that actually happens --
    //   entirely ungated.
    // Scenario: the monthly timer fires with nothing to resume, on an idle pool
    //   whose scrub status still reads "never".
    fn lock_is_carried_through_the_resume_fallback() {
        let rig = Rig::new();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let (resume_req, resume_out) = scrub_resume_output(2);
        let (status_req, status_out) = scrub_status_never();
        let (_start_req, start_out) = scrub_start_output(0);
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_output(status_req, status_out)
            .with_handler(observing_lock(
                Arc::clone(&observed),
                rig.lock_path.clone(),
                is_start,
                start_out,
            ));

        let result = rig.run(&runner).unwrap();
        assert_eq!(
            result,
            ScrubResumeOrStartResult::Started {
                uncorrectable_errors: false
            }
        );
        assert_eq!(
            *observed.lock().unwrap(),
            vec![false],
            "the fallback start must be spawned while the gate's lock is still held"
        );
        assert!(
            rig.lock_is_free(),
            "the lock must be released once the run is over"
        );
    }

    #[test]
    // Intent: once the confirm poll sees a running scrub, the lock is released
    //   -- before the child is reaped.
    // Why it exists: holding it for the scrub's multi-hour run would block
    //   every mutation, contradicting ADR 018's position that a balance may
    //   overlap an already-running scrub.
    // Scenario: `btrfs scrub start` reached its ioctl, so scrub status reports
    //   running while the command is still executing.
    fn lock_is_released_once_the_scrub_is_confirmed_running() {
        let rig = Rig::new();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let (resume_req, resume_out) = scrub_resume_output(2);
        let (status_req, status_out) = scrub_status_running();
        let (_start_req, start_out) = scrub_start_output(0);
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_output(status_req, status_out)
            .with_handler(observing_lock(
                Arc::clone(&observed),
                rig.lock_path.clone(),
                is_start,
                start_out,
            ));

        rig.run(&runner).unwrap();
        assert_eq!(
            *observed.lock().unwrap(),
            vec![true],
            "a confirmed-running scrub must have released the lock already"
        );
    }

    #[test]
    // Intent: an unclassifiable confirm probe releases the lock and the run
    //   still classifies from the child's own exit code.
    // Why it exists: AR2 -- blocking every mutation for hours on a
    //   parser-compatibility break is worse than the residual overlap, but the
    //   authoritative exit code must not change (I5).
    // Scenario: `btrfs scrub status` output drifts into something braid's
    //   parser cannot classify.
    fn unconfirmable_start_releases_the_lock_and_still_classifies() {
        let rig = Rig::new();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let (resume_req, resume_out) = scrub_resume_output(2);
        let (status_req, status_out) = scrub_status_unknown();
        let (_start_req, start_out) = scrub_start_output(3);
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_output(status_req, status_out)
            .with_handler(observing_lock(
                Arc::clone(&observed),
                rig.lock_path.clone(),
                is_start,
                start_out,
            ));

        let result = rig.run(&runner).unwrap();
        assert_eq!(
            *observed.lock().unwrap(),
            vec![true],
            "an unconfirmable start must have released the lock"
        );
        assert_eq!(
            result,
            ScrubResumeOrStartResult::Started {
                uncorrectable_errors: true
            },
            "the child's exit code stays authoritative"
        );
    }

    #[test]
    // Intent: a scrub already in flight when this child starts releases the
    //   lock early, and the child's own non-zero exit still classifies as a
    //   failure.
    // Why it exists: the confirm poll cannot tell "my scrub registered" from "a
    //   scrub was already running". Releasing early is correct either way -- a
    //   scrub is in flight, which is exactly the state the lock is released for
    //   -- but the release must not soften the exit-code classification (I5).
    // Scenario: an operator started a manual scrub minutes ago; the timer fires
    //   and btrfs refuses the second scrub with exit 1.
    fn preexisting_running_scrub_releases_lock_but_exit_still_classifies() {
        let rig = Rig::new();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let (resume_req, resume_out) = scrub_resume_output(2);
        let (status_req, status_out) = scrub_status_running();
        let (_start_req, start_out) = scrub_start_output(1);
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_output(status_req, status_out)
            .with_handler(observing_lock(
                Arc::clone(&observed),
                rig.lock_path.clone(),
                is_start,
                start_out,
            ));

        let result = rig.run(&runner);
        assert_eq!(
            *observed.lock().unwrap(),
            vec![true],
            "an already-running scrub must have released the lock"
        );
        assert!(
            matches!(result, Err(ScrubResumeOrStartError::StartFailed { .. })),
            "the child's exit code stays authoritative, got {result:?}"
        );
    }

    #[test]
    // Intent: a terminal exit that starts no scrub still releases the lock.
    // Why it exists: the guard is dropped on the way out of every arm, not just
    //   the confirmed-running one. A leaked guard here would be invisible until
    //   the next braid mutation was refused for no reason.
    // Scenario: `btrfs scrub resume` fails outright with no teardown in flight.
    fn terminal_failure_releases_the_lock() {
        let rig = Rig::new();
        let (resume_req, resume_out) = scrub_resume_output(1);
        let (status_req, status_out) = scrub_status_never();
        let runner = MockRunner::default()
            .with_output(resume_req, resume_out)
            .with_output(status_req, status_out);

        let result = rig.run(&runner);
        assert!(matches!(
            result,
            Err(ScrubResumeOrStartError::ResumeFailed { .. })
        ));
        assert!(
            rig.lock_is_free(),
            "a run that starts no scrub must still release the pool lock"
        );
    }

    // -----------------------------------------------------------------------
    // Existing exit-code contract (I5, I6)
    // -----------------------------------------------------------------------

    /// Gate-clear runner whose confirm probe reports a running scrub, for the
    /// tests that only care about btrfs exit-code classification.
    fn runner_with_status_running() -> MockRunner {
        let (status_req, status_out) = scrub_status_running();
        MockRunner::default().with_output(status_req, status_out)
    }

    #[test]
    // Intent: resume exit 0 returns Resumed without falling back to start.
    // Why it exists: scheduled scrub should continue saved work before starting fresh.
    // Scenario: monthly timer fires while cancelled scrub progress exists.
    fn resume_succeeds_returns_resumed() {
        let rig = Rig::new();
        let (resume_req, resume_out) = scrub_resume_output(0);
        let runner = runner_with_status_running().with_output(resume_req, resume_out);

        let result = rig.run(&runner).unwrap();
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
        let rig = Rig::new();
        let (resume_req, resume_out) = scrub_resume_output(3);
        let runner = runner_with_status_running().with_output(resume_req, resume_out);

        let result = rig.run(&runner).unwrap();
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
        let rig = Rig::new();
        let (resume_req, resume_out) = scrub_resume_output(2);
        let (start_req, start_out) = scrub_start_output(0);
        let runner = runner_with_status_running()
            .with_output(resume_req, resume_out)
            .with_output(start_req, start_out);

        let result = rig.run(&runner).unwrap();
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
        let rig = Rig::new();
        let (resume_req, resume_out) = scrub_resume_output(2);
        let (start_req, start_out) = scrub_start_output(3);
        let runner = runner_with_status_running()
            .with_output(resume_req, resume_out)
            .with_output(start_req, start_out);

        let result = rig.run(&runner).unwrap();
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
        let rig = Rig::new();
        let (resume_req, resume_out) = scrub_resume_output(1);
        let runner = runner_with_status_running().with_output(resume_req, resume_out);

        let result = rig.run(&runner);
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
        let rig = Rig::new();
        let (resume_req, resume_out) = scrub_resume_output(2);
        let (start_req, start_out) = scrub_start_output(1);
        let runner = runner_with_status_running()
            .with_output(resume_req, resume_out)
            .with_output(start_req, start_out);

        let result = rig.run(&runner);
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
        let rig = Rig::new();
        let runner = runner_with_status_running().with_handler(cancel_during(
            rig.paths.scrub_cancel_requested(),
            |req| matches!(req, CmdRequest::BtrfsScrubResume { .. }),
            "btrfs scrub resume -B /mnt/storage",
        ));

        let result = rig.run(&runner).unwrap();
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
        let rig = Rig::new();
        let (resume_req, resume_out) = scrub_resume_output(2);
        let runner = runner_with_status_running()
            .with_output(resume_req, resume_out)
            .with_handler(cancel_during(
                rig.paths.scrub_cancel_requested(),
                is_start,
                "btrfs scrub start -B /mnt/storage",
            ));

        let result = rig.run(&runner).unwrap();
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
        let rig = Rig::new();
        let (resume_req, resume_out) = scrub_resume_output(1);
        let runner = runner_with_status_running().with_output(resume_req, resume_out);

        let result = rig.run(&runner);
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
        let rig = Rig::new();
        std::fs::write(rig.paths.scrub_cancel_requested(), b"").unwrap();
        let (resume_req, resume_out) = scrub_resume_output(1);
        let runner = runner_with_status_running().with_output(resume_req, resume_out);

        let result = rig.run(&runner);
        assert!(
            matches!(result, Err(ScrubResumeOrStartError::ResumeFailed { .. })),
            "stale marker must be cleared so a genuine failure still alerts, got {result:?}"
        );
        assert!(
            !rig.paths.scrub_cancel_requested().exists(),
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
        let rig = Rig::new();
        std::fs::create_dir(rig.paths.scrub_cancel_requested()).unwrap();
        // No registered output: any runner.run would surface as MissingMock, so
        // the returned MarkerCleanupFailed proves cleanup short-circuited.
        let runner = MockRunner::default();

        let result = rig.run(&runner);
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
