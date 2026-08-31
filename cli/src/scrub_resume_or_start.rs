use crate::cmd::{CmdError, CmdRequest, CommandRunner, PendingCommand, RawCommandOutput};
use crate::filesystem::Filesystem;
use crate::parse::types::ScrubTimestamp;
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
/// deadline comfortably above scrub startup and well below the timer's poll
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
    /// Naive-local wall clock the freshness age is measured against, injected
    /// from `main.rs` (the `_at` convention) via `crate::util::local_now`. Naive
    /// local because btrfs renders scrub timestamps as local ctime; a UTC basis
    /// would skew every comparison by the host offset.
    pub now: time::PrimitiveDateTime,
    /// How long a recorded scrub keeps the pool fresh. Required -- there is no
    /// unwindowed scheduled scrub, because a missing window would silently mean
    /// "scrub on every poll".
    pub fresh_for: time::Duration,
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
    /// braid is already working on this pool (its own pool lock, a btrfs
    /// exclusive operation, or an interrupted-operation journal), so no scrub
    /// was started and nothing else was touched. Purely informational: the
    /// hourly poll is the retry, so this owes no durable record.
    Skipped {
        reason: String,
    },
    /// The recorded scrub is younger than the freshness window, so nothing is
    /// owed. The overwhelmingly common outcome -- it is reached before any pool
    /// lock and mutates nothing.
    NotDue {
        detail: String,
    },
    /// A scrub is already in flight on this pool -- seen either by the entry
    /// probe or by btrfs's own refusal a moment later. A scrub running means
    /// nothing is owed, so this is a clean success, not a skip.
    AlreadyRunning,
}

/// The one operator-visible phrasing for "someone else is scrubbing this pool",
/// shared by the entry classifier and the invocation-time collision below.
///
/// Both are the same fact observed at different moments -- the probe sees it
/// before the spawn, the collision sees it in btrfs's own refusal a moment
/// later -- so they must read the same way in the journal.
pub const SCRUB_ALREADY_RUNNING: &str = "a btrfs scrub is already running";

/// btrfs's refusal to start or resume over a scrub that is already running,
/// pinned to `scrub_start`'s `is_scrub_running_on_fs` wording in
/// `reference/btrfs-progs/cmds/scrub.c` (the resume path shares that guard).
///
/// This substring is the *sole* discriminator for a collision. A post-failure
/// `btrfs scrub status` re-probe would be racy in both directions: an external
/// scrub that finished first would turn a real collision into a false failure,
/// and one that started after a genuine braid failure would suppress an alert
/// braid must raise. Behavior-locked live by
/// `tests/repro/btrfs-scrub-start-rejected-during-scrub.py`
/// ([live-tool behavior locks](../../docs/dev/testing.md#live-tool-behavior-locks)).
const ALREADY_RUNNING_REJECTION: &str = "Scrub is already running.";

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
    /// `btrfs scrub status` could not be classified. Hard error, not a skip and
    /// not "due", by the same asymmetry as `ExclusiveOpUnknown` (ADR 018): a
    /// scheduler that cannot read the pool's own scrub record must not decide
    /// anything from it, in either direction.
    #[error(
        "parse error: could not read the pool's scrub record: {reason}. \
         Refusing to schedule a scrub without it."
    )]
    ScrubStatusUnreadable { reason: String },
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

/// What the pool's own scrub record says about whether a scrub is owed.
///
/// The single classification of the single entry probe (ADR 035). Kept as a
/// value rather than folded into control flow so the whole decision is one
/// pure, table-testable function.
#[derive(Debug, PartialEq, Eq)]
pub enum ScrubFreshness {
    /// A scrub finished inside the window: nothing is owed.
    Fresh {
        last: ScrubTimestamp,
        age: time::Duration,
    },
    /// A scrub is in flight right now: nothing is owed either.
    AlreadyRunning,
    /// Anything else. `clock_skew` marks the one due reason that is a symptom
    /// rather than a schedule: a record dated in the future.
    Due { clock_skew: bool },
}

/// Decide, from btrfs's own scrub record, whether the pool is due for a scrub.
///
/// The scheduling anchor is the *latest start-or-resume* btrfs reports -- after
/// a resumed scrub it prints only `Scrub resumed:`, and the parser folds both
/// lines into `started_at`. btrfs records no finish time, and deriving
/// `start + duration` is skewed on resumed scrubs, so the anchor approximates
/// completion from below: it errs toward scrubbing slightly early, and it is
/// the very timestamp `braid status` shows as "Last scrub", so operator and
/// scheduler do the same arithmetic.
///
/// The fail direction is the inverse of the busy gate's: nothing ambiguous may
/// read as fresh. `Never`, a terminal-but-not-`Finished` state, a missing
/// anchor and a future-dated anchor all fall to `Due`, because an unnecessary
/// scrub is visible and self-limiting whereas silent starvation is not. Only
/// `Unknown` -- the parser could not classify the output at all -- is neither,
/// and becomes the caller's hard error.
pub fn classify_freshness(
    state: &ScrubState,
    now: time::PrimitiveDateTime,
    window: time::Duration,
) -> Result<ScrubFreshness, String> {
    match state {
        ScrubState::Running { .. } => Ok(ScrubFreshness::AlreadyRunning),
        ScrubState::Unknown => Err("unrecognized btrfs scrub status".to_owned()),
        ScrubState::Finished {
            started_at: Some(last),
            ..
        } => {
            let age = now - last.0;
            if age.is_negative() {
                Ok(ScrubFreshness::Due { clock_skew: true })
            } else if age < window {
                Ok(ScrubFreshness::Fresh {
                    last: last.clone(),
                    age,
                })
            } else {
                Ok(ScrubFreshness::Due { clock_skew: false })
            }
        }
        ScrubState::Never
        | ScrubState::Finished { .. }
        | ScrubState::Aborted { .. }
        | ScrubState::Interrupted { .. } => Ok(ScrubFreshness::Due { clock_skew: false }),
    }
}

/// The journal line a fresh poll leaves behind, so "why didn't my scrub run?"
/// is answerable from `journalctl -u braid-scrub.service` alone.
///
/// Says "started/resumed", never "started": on a resumed scrub the anchor is
/// btrfs's `Scrub resumed:` line, and calling that a start would send an
/// operator looking for a scrub that began days earlier.
fn not_due_line(last: &ScrubTimestamp, age: time::Duration, window: time::Duration) -> String {
    let ago = crate::util::humanize_ago(age).unwrap_or_else(|| "just now".to_owned());
    format!(
        "last scrub started/resumed {} ({}); next due in {}",
        crate::util::format_scrub_timestamp(last),
        ago,
        format_days_until(window - age)
    )
}

/// Round the remaining window *up* to whole days so a scrub still half a day
/// out never reads as "next due in 0 days".
fn format_days_until(remaining: time::Duration) -> String {
    const SECS_PER_DAY: i64 = 86_400;
    let days = remaining
        .whole_seconds()
        .saturating_add(SECS_PER_DAY - 1)
        .div_euclid(SECS_PER_DAY);
    if days == 1 {
        "1 day".to_owned()
    } else {
        format!("{days} days")
    }
}

/// Resume saved scrub progress, or start a fresh scrub when nothing is saved.
///
/// This is the scheduled/manual scrub helper, driven by a cheap hourly poll.
/// It opens by reading btrfs's own scrub record exactly once, before any pool
/// lock, and classifying it exactly once (see [`classify_freshness`]): a fresh
/// or already-running pool owes nothing and returns without touching anything,
/// which is the outcome of almost every poll and must not contend for the lock.
///
/// Only a `Due` pool reaches the busy gate (see [`gate`]): a scheduled scrub
/// must never pile onto a pool that braid is already mutating, and a scrub
/// started during a `btrfs replace` is kernel-rejected and would spuriously
/// alert. A tripped gate is a `Skipped`, not a failure -- the next poll retries.
///
/// Exit 2 from resume falls back to `btrfs scrub start -B`; exit 3
/// (uncorrectable errors found, scrub completed) stays a service success --
/// corruption alerts via ADR 014's device-stats poll, not this exit code, so
/// `onFailure` covers execution failure only.
///
/// A btrfs exit outside `{0,2,3}` is ambiguous: btrfs returns 1 for *both* a
/// deliberate cancel (lock/suspend/shutdown) and a genuine fatal scrub error,
/// and `scrub_one_dev` sets `canceled = !!ret` so even scrub *status* renders
/// both as `aborted`. The only authoritative discriminator is braid's own
/// teardown intent: the ExecStop script touches a cancel-request marker, so
/// the runner removes any stale marker at entry (fail-closed -- a surviving
/// marker would later mask a real failure) and, on an ambiguous exit, returns
/// `Cancelled` iff the marker is present. The marker is the sole discriminator
/// for a cancel; scrub status is never consulted.
///
/// One other exit 1 is not a failure: btrfs refusing to resume or start over a
/// scrub that began after the entry probe cleared. That refusal is recognized
/// from its own stderr and lands on the same `AlreadyRunning` the entry probe
/// would have produced -- see [`classify_btrfs_failure`]. Everything else is
/// the failure.
pub fn cmd_scrub_resume_or_start<R: CommandRunner, F: Filesystem + ?Sized>(
    p: &ScrubRunParams<'_, R, F>,
) -> Result<ScrubResumeOrStartResult, ScrubResumeOrStartError> {
    // Invariant 1: exactly one status probe, before any pool-lock acquisition,
    // classified exactly once. Nothing below re-probes to revise this.
    let raw = p.runner.run(&CmdRequest::BtrfsScrubStatus {
        mount_point: p.mount_point.clone(),
    })?;
    let status = parse_btrfs_scrub_status(&raw).map_err(|e| {
        ScrubResumeOrStartError::ScrubStatusUnreadable {
            reason: e.to_string(),
        }
    })?;
    match classify_freshness(&status.state, p.now, p.fresh_for)
        .map_err(|reason| ScrubResumeOrStartError::ScrubStatusUnreadable { reason })?
    {
        ScrubFreshness::Fresh { last, age } => {
            return Ok(ScrubResumeOrStartResult::NotDue {
                detail: not_due_line(&last, age, p.fresh_for),
            });
        }
        ScrubFreshness::AlreadyRunning => return Ok(ScrubResumeOrStartResult::AlreadyRunning),
        ScrubFreshness::Due { clock_skew } => {
            if clock_skew {
                // Named rather than silently rolled into "due": a future-dated
                // record means the host clock moved, and the operator wants to
                // know that, not just see an unexplained extra scrub.
                eprintln!(
                    "braid: the pool's last scrub is dated in the future (clock skew); \
                     treating the pool as due for a scrub"
                );
            }
        }
    }

    // The gate runs before the marker cleanup so a skip leaves the cancel
    // marker and saved scrub state exactly as it found them.
    let guard = match gate(p)? {
        GateOutcome::Busy(reason) => return Ok(ScrubResumeOrStartResult::Skipped { reason }),
        GateOutcome::Clear(guard) => guard,
    };

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
        _ => classify_btrfs_failure(p.paths, resume_raw.stderr, |stderr| {
            ScrubResumeOrStartError::ResumeFailed { stderr }
        }),
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
/// Three conditions, all of them "braid is already working on this pool": the
/// pool lock held by another braid process, any btrfs exclusive operation in
/// flight (running *or* paused), and an interrupted-operation journal. The
/// lock is checked first and returned held, because it is the only one that
/// also covers the LUKS work a mutator does *before* any btrfs exclusive
/// operation exists for sysfs to see.
///
/// "Someone else is already scrubbing" is deliberately not here: it is not a
/// reason to *defer* a scrub, it is a reason the pool owes none, and the entry
/// classifier already answered it from the same probe this gate would have had
/// to repeat (ADR 035, invariant 1).
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
        _ => classify_btrfs_failure(p.paths, start_raw.stderr, |stderr| {
            ScrubResumeOrStartError::StartFailed { stderr }
        }),
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
///
/// Only the post-spawn confirmation calls this. It answers a different question
/// from the entry classifier ("did *my* scrub register", not "is a scrub owed"),
/// which is why it does not count against the one-probe invariant.
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

/// Classify an ambiguous btrfs exit (outside `{0,2,3}`) into a clean cancel, an
/// invocation-time collision with someone else's scrub, or a genuine failure.
///
/// Cancel is decided first and solely on the cancel-request marker:
/// `Path::exists()` coerces any I/O error to `false`, so the only route to
/// `Cancelled` is an unambiguously present marker; absence *or* any read
/// ambiguity falls through (fail-closed here too).
///
/// A collision is decided solely on this invocation's own stderr carrying
/// [`ALREADY_RUNNING_REJECTION`] -- btrfs refused before touching the pool
/// because an external scrub started after the entry probe classified the pool
/// as due. That is the entry probe's `AlreadyRunning` arriving a moment late,
/// so it produces the same outcome rather than the alert the shape used to
/// raise. Every other stderr keeps its failure classification.
///
/// Takes the stderr and a constructor rather than a built error so the
/// collision test can read the same string the failure would have carried.
/// Shared by the resume and start arms so both classify identically.
fn classify_btrfs_failure(
    paths: &StatePaths,
    stderr: String,
    failure: fn(String) -> ScrubResumeOrStartError,
) -> Result<ScrubResumeOrStartResult, ScrubResumeOrStartError> {
    if paths.scrub_cancel_requested().exists() {
        return Ok(ScrubResumeOrStartResult::Cancelled);
    }
    if stderr.contains(ALREADY_RUNNING_REJECTION) {
        return Ok(ScrubResumeOrStartResult::AlreadyRunning);
    }
    Err(failure(stderr))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::parse::types::ScrubState;
    use crate::test_fixtures::{
        IdleMockFs, isolated_paths, scrub_already_running_rejection, scrub_mp, scrub_resume_output,
        scrub_start_output, scrub_status_finished, scrub_status_finished_at, scrub_status_never,
        scrub_status_running, scrub_status_unknown,
    };
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use tempfile::TempDir;
    use time::macros::datetime;

    /// The injected `now` every runner-level test decides against. Fixed so no
    /// test depends on the host clock or on when a fixture was written.
    const NOW: time::PrimitiveDateTime = datetime!(2026-03-01 12:00:00);

    /// The freshness window every runner-level test runs with, matching the
    /// module option's 30-day default.
    const WINDOW: time::Duration = time::Duration::days(30);

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
                now: NOW,
                fresh_for: WINDOW,
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
    // Entry classifier: the freshness decision (I1, I2, I3, I4)
    // -----------------------------------------------------------------------

    /// A `Finished` record anchored at the given ctime, for the pure matrix.
    fn finished_at(started_at: time::PrimitiveDateTime) -> ScrubState {
        ScrubState::Finished {
            started_at: Some(ScrubTimestamp(started_at)),
            error_count: 0,
            duration_secs: Some(1),
            total_bytes: Some(1_073_741_824),
            rate_bytes_per_sec: Some(1_073_741_824),
        }
    }

    /// A terminal-but-not-finished record anchored at the given ctime.
    fn aborted_at(started_at: time::PrimitiveDateTime) -> ScrubState {
        ScrubState::Aborted {
            started_at: Some(ScrubTimestamp(started_at)),
            error_count: 0,
            duration_secs: Some(1),
            total_bytes: Some(1_073_741_824),
            rate_bytes_per_sec: Some(1_073_741_824),
        }
    }

    #[test]
    // Intent: only `Finished` + a present anchor + an age inside `[0, window)`
    //   classifies as Fresh; every other shape is Due, `Running` is
    //   AlreadyRunning, and `Unknown` is the caller's hard error.
    // Why it exists: this table *is* the scheduling policy, and its fail
    //   direction is the inverse of the busy gate's -- nothing ambiguous may
    //   read as fresh, because an unnecessary scrub is visible and
    //   self-limiting while silent starvation is not. The boundary rows pin
    //   the half-open window: one second short of it still suppresses, exactly
    //   at it no longer does.
    // Scenario: every state btrfs can report, at the ages that decide the poll.
    fn freshness_matrix() {
        let now = datetime!(2026-03-01 12:00:00);
        let window = time::Duration::days(30);
        let just_inside = now - window + time::Duration::seconds(1);
        let exactly_window = now - window;

        let cases: Vec<(&str, ScrubState, ScrubFreshness)> = vec![
            (
                "finished one second inside the window",
                finished_at(just_inside),
                ScrubFreshness::Fresh {
                    last: ScrubTimestamp(just_inside),
                    age: window - time::Duration::seconds(1),
                },
            ),
            (
                "finished exactly a window ago",
                finished_at(exactly_window),
                ScrubFreshness::Due { clock_skew: false },
            ),
            (
                "finished long ago",
                finished_at(datetime!(2024-01-01 00:00:00)),
                ScrubFreshness::Due { clock_skew: false },
            ),
            (
                "finished, but dated in the future",
                finished_at(now + time::Duration::hours(1)),
                ScrubFreshness::Due { clock_skew: true },
            ),
            (
                "finished with no parseable anchor",
                ScrubState::Finished {
                    started_at: None,
                    error_count: 0,
                    duration_secs: None,
                    total_bytes: None,
                    rate_bytes_per_sec: None,
                },
                ScrubFreshness::Due { clock_skew: false },
            ),
            (
                "aborted a minute ago",
                aborted_at(now - time::Duration::minutes(1)),
                ScrubFreshness::Due { clock_skew: false },
            ),
            (
                "interrupted a minute ago",
                ScrubState::Interrupted {
                    started_at: Some(ScrubTimestamp(now - time::Duration::minutes(1))),
                    error_count: 0,
                    duration_secs: Some(1),
                    total_bytes: None,
                    rate_bytes_per_sec: None,
                },
                ScrubFreshness::Due { clock_skew: false },
            ),
            (
                "never scrubbed",
                ScrubState::Never,
                ScrubFreshness::Due { clock_skew: false },
            ),
            (
                "a scrub is running",
                ScrubState::Running {
                    started_at: Some(ScrubTimestamp(now - time::Duration::minutes(1))),
                    duration_secs: Some(60),
                    time_left_secs: None,
                    eta: None,
                    total_bytes: None,
                    bytes_scrubbed: None,
                    rate_bytes_per_sec: None,
                    error_count: 0,
                },
                ScrubFreshness::AlreadyRunning,
            ),
        ];

        for (label, state, expected) in cases {
            assert_eq!(
                classify_freshness(&state, now, window).unwrap(),
                expected,
                "{label}"
            );
        }

        assert!(
            classify_freshness(&ScrubState::Unknown, now, window).is_err(),
            "an unclassifiable record must be neither fresh nor due"
        );
    }

    #[test]
    // Intent: a finished scrub that was *resumed* anchors on the resume time,
    //   not on a start line btrfs no longer prints.
    // Why it exists: after a resume btrfs emits only `Scrub resumed:`, and the
    //   parser folds it into the same field. A scheduler that ignored it would
    //   read a long-resumed scrub as ancient and re-scrub immediately; the
    //   operator-facing wording therefore says "started/resumed", not
    //   "started".
    // Scenario: an aborted scrub was resumed this morning and finished; the
    //   next poll must find the pool fresh.
    fn a_resumed_finished_scrub_anchors_on_the_resume_time() {
        let rig = Rig::new();
        let resumed = NOW - time::Duration::hours(3);
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus {
                mount_point: scrub_mp(),
            },
            RawCommandOutput {
                cmd: "btrfs scrub status --raw /mnt/storage".into(),
                stdout: format!(
                    "UUID:             cc86845b-aec3-408e-bef5-553affc1f2b1\n\
                     Scrub resumed:    {}\n\
                     Status:           finished\n\
                     Duration:         0:00:10\n\
                     Total to scrub:   1073741824\n\
                     Rate:             104857600/s\n\
                     Error summary:    no errors found\n",
                    crate::util::format_scrub_timestamp(&ScrubTimestamp(resumed))
                ),
                stderr: String::new(),
                exit_status: 0,
            },
        );

        let result = rig.run(&runner).unwrap();
        assert!(
            matches!(result, ScrubResumeOrStartResult::NotDue { .. }),
            "a scrub resumed 3 hours ago must be fresh, got {result:?}"
        );
    }

    #[test]
    // Intent: a fresh pool takes no pool lock, issues exactly one btrfs
    //   command, and writes nothing.
    // Why it exists: invariant 3 and the whole point of the redesign -- this is
    //   the outcome of almost every hourly poll, so it must not contend for the
    //   lock a `braid add` may be holding, and it must not cost more than the
    //   one probe the decision is made from.
    // Scenario: the hourly timer fires two days after the last scrub finished.
    fn a_fresh_pool_is_a_no_op() {
        let rig = Rig::new();
        let last = NOW - time::Duration::days(2);
        let (status_req, status_out) = scrub_status_finished_at(last);
        // Nothing else is registered: any further probe or scrub invocation
        // would surface as MissingMock rather than passing quietly.
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = rig.run(&runner).unwrap();
        let ScrubResumeOrStartResult::NotDue { detail } = result else {
            panic!("a two-day-old scrub must not be due, got {result:?}");
        };
        assert_eq!(
            detail,
            "last scrub started/resumed Fri Feb 27 12:00:00 2026 (2 days ago); next due in 28 days"
        );
        assert_eq!(
            runner.requests().len(),
            1,
            "the decision must cost exactly one probe: {:?}",
            runner.requests()
        );
        assert!(
            rig.lock_is_free(),
            "a fresh poll must never have taken the pool lock"
        );
    }

    #[test]
    // Intent: the pool lock is not even attempted on the fresh path.
    // Why it exists: `lock_is_free()` after the fact cannot tell "never
    //   acquired" from "acquired and released". A peer holding the lock proves
    //   the stronger claim: a fresh poll must return NotDue, not the
    //   lock-contended skip the gate would produce.
    // Scenario: `braid add` holds the pool lock while the hourly timer fires on
    //   a pool scrubbed yesterday.
    fn a_fresh_pool_does_not_contend_for_the_pool_lock() {
        let rig = Rig::new();
        let _peer = RealPoolLock::new(rig.lock_path.clone()).acquire().unwrap();
        let (status_req, status_out) = scrub_status_finished_at(NOW - time::Duration::days(1));
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = rig.run(&runner).unwrap();
        assert!(
            matches!(result, ScrubResumeOrStartResult::NotDue { .. }),
            "freshness is decided before the lock, got {result:?}"
        );
    }

    #[test]
    // Intent: a scrub already running at the entry probe returns AlreadyRunning
    //   without touching the pool.
    // Why it exists: a scrub in flight means nothing is owed -- it is a clean
    //   exit 0, not the exit-4 "owed but blocked" skip. braid used to let this
    //   reach `btrfs scrub resume`, which refuses with exit 1, and then alerted
    //   the operator for a pool that was being scrubbed correctly.
    // Scenario: the operator kicked off `btrfs scrub start /mnt/storage` by
    //   hand this afternoon; the hourly poll fires mid-run.
    fn an_already_running_scrub_is_not_owed_a_run() {
        let rig = Rig::new();
        let (status_req, status_out) = scrub_status_running();
        // No resume/start registered: reaching either would be a MissingMock.
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = rig.run(&runner).unwrap();
        assert_eq!(result, ScrubResumeOrStartResult::AlreadyRunning);
        assert_eq!(runner.requests().len(), 1);
        assert!(rig.lock_is_free(), "no pool lock may be taken");
    }

    #[test]
    // Intent: an unclassifiable entry probe is a hard error, not "due" and not
    //   "fresh".
    // Why it exists: parser drift must alert, in both directions -- reading it
    //   as due would scrub the pool hourly forever, reading it as fresh would
    //   silently stop scrubbing altogether.
    // Scenario: btrfs-progs changes scrub-status output past what braid parses.
    fn an_unreadable_entry_probe_is_a_hard_error() {
        let rig = Rig::new();
        let (status_req, status_out) = scrub_status_unknown();
        let runner = MockRunner::default().with_output(status_req, status_out);

        let result = rig.run(&runner);
        assert!(
            matches!(
                result,
                Err(ScrubResumeOrStartError::ScrubStatusUnreadable { .. })
            ),
            "expected ScrubStatusUnreadable, got {result:?}"
        );
        assert!(rig.lock_is_free(), "no pool lock may be taken");
    }

    // -----------------------------------------------------------------------
    // Gate: busy pools skip, unreadable gates fail (I1, I4)
    // -----------------------------------------------------------------------

    #[test]
    // Intent: a paused balance makes a due scrub skip without issuing any btrfs
    //   command beyond the entry probe, and without touching the cancel marker.
    // Why it exists: the caja incident -- a `braid add` convert balance was
    //   mid-flight with the scheduled scrub due at midnight, and nothing stopped
    //   the scrub from piling onto the same spindles. A paused balance is the
    //   sharpest case: sysfs still reports it, so "paused" must count as busy.
    // Scenario: operator paused an add's convert balance overnight; the poll
    //   fires on a pool that is genuinely due.
    fn skips_when_balance_paused() {
        let rig = Rig::with_fs(IdleMockFs::with_exclop("balance paused"));
        std::fs::write(rig.paths.scrub_cancel_requested(), b"stale").unwrap();
        let runner = due_entry_runner();

        let result = rig.run(&runner).unwrap();
        assert!(
            matches!(result, ScrubResumeOrStartResult::Skipped { .. }),
            "paused balance must skip, got {result:?}"
        );
        assert_eq!(
            runner.requests().len(),
            1,
            "a skip must issue nothing beyond the entry probe: {:?}",
            runner.requests()
        );
        assert!(
            rig.paths.scrub_cancel_requested().exists(),
            "a skip must leave the cancel marker untouched"
        );
        assert!(rig.lock_is_free(), "the gate must release the pool lock");
    }

    #[test]
    // Intent: a running device replace also skips.
    // Why it exists: a scheduled scrub firing during `btrfs replace` is
    //   kernel-rejected, exits 1, and spuriously fires the scrub-failed alert
    //   path -- the second half of the bug this gate closes.
    // Scenario: `braid replace` is rebuilding a disk when the poll finds the
    //   pool due.
    fn skips_when_device_replace_running() {
        let rig = Rig::with_fs(IdleMockFs::with_exclop("device replace"));
        let runner = due_entry_runner();

        let result = rig.run(&runner).unwrap();
        assert!(
            matches!(result, ScrubResumeOrStartResult::Skipped { .. }),
            "device replace must skip, got {result:?}"
        );
        assert_eq!(runner.requests().len(), 1);
    }

    #[test]
    // Intent: an interrupted-operation journal skips the scrub.
    // Why it exists: pending-op.json means membership may be inconsistent; a
    //   scrub then competes with the `braid recover` the operator has to run.
    // Scenario: an add was interrupted by a power cut; the poll fires before
    //   anyone ran `braid recover`.
    fn skips_when_pending_operation_present() {
        let rig = Rig::new();
        write_journal(&rig.paths);
        let runner = due_entry_runner();

        let result = rig.run(&runner).unwrap();
        assert!(
            matches!(result, ScrubResumeOrStartResult::Skipped { .. }),
            "pending op must skip, got {result:?}"
        );
        assert_eq!(runner.requests().len(), 1);
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
        let runner = due_entry_runner();

        let result = rig.run(&runner).unwrap();
        assert!(
            matches!(result, ScrubResumeOrStartResult::Skipped { .. }),
            "corrupt journal must skip, got {result:?}"
        );
        assert_eq!(runner.requests().len(), 1);
    }

    #[test]
    // Intent: a pool lock held by another braid process is itself a skip
    //   reason, with no scrub started.
    // Why it exists: the lock is the only gate that covers the LUKS work a
    //   mutator does *before* any btrfs exclusive operation exists for sysfs to
    //   see -- without it the scrub can start inside that window.
    // Scenario: `braid add` is still formatting the new disk's LUKS header when
    //   the poll finds the pool due.
    fn skips_when_pool_lock_held_by_peer() {
        let rig = Rig::new();
        let _peer = RealPoolLock::new(rig.lock_path.clone()).acquire().unwrap();
        let runner = due_entry_runner();

        let result = rig.run(&runner).unwrap();
        assert!(
            matches!(result, ScrubResumeOrStartResult::Skipped { .. }),
            "held pool lock must skip, got {result:?}"
        );
        assert_eq!(runner.requests().len(), 1);
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
        let runner = due_entry_runner();

        let result = rig.run(&runner);
        assert!(
            matches!(
                result,
                Err(ScrubResumeOrStartError::ExclusiveOpUnknown { .. })
            ),
            "unreadable sysfs must be an error, got {result:?}"
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
        let runner = due_entry_runner();

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
    // Pool lock lifetime across spawn/confirm/wait (I1, I5)
    // -----------------------------------------------------------------------

    #[test]
    // Intent: the pool lock is still held when the resume-exit-2 fallback
    //   spawns the fresh `btrfs scrub start`.
    // Why it exists: `resume -B` exit 2 ends no run, so releasing at child exit
    //   would leave the fallback start -- the scrub that actually happens --
    //   entirely ungated.
    // Scenario: a poll finds a due pool with nothing to resume, on an idle pool
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
        let runner = due_entry_runner()
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
        let runner = due_entry_runner()
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
    // Intent: a scrub visible to the confirm poll releases the lock early, and
    //   a non-collision exit 1 from braid's own child still classifies as a
    //   failure.
    // Why it exists: the confirm poll cannot tell "my scrub registered" from "a
    //   scrub was already running". Releasing early is correct either way -- a
    //   scrub is in flight, which is exactly the state the lock is released for
    //   -- but the release must not soften the exit-code classification (I5).
    //   Only btrfs's own already-running refusal downgrades an exit 1 to a
    //   skip; a scrub merely being visible must not, or the release would start
    //   swallowing real failures.
    // Scenario: the pool is being scrubbed when braid's `btrfs scrub start`
    //   fails for an unrelated reason (an I/O error reading the saved state).
    fn scrub_appearing_after_the_gate_releases_lock_but_exit_still_classifies() {
        let rig = Rig::new();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let (resume_req, resume_out) = scrub_resume_output(2);
        let (status_req, status_out) = scrub_status_running();
        let (_start_req, start_out) = scrub_start_output(1);
        let runner = due_entry_runner()
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

    /// A runner whose *entry* `btrfs scrub status` reports a long-finished
    /// scrub (dated 2024, far outside `WINDOW` from `NOW`), so the pool
    /// classifies as due and the test reaches the gate and the run path. Every
    /// later probe (the confirm poll) falls through to whatever the test
    /// registers.
    fn due_entry_runner() -> MockRunner {
        let (status_req, finished_out) = scrub_status_finished();
        MockRunner::default().with_output_sequence(status_req, vec![finished_out])
    }

    /// Due-entry runner whose confirm probe reports a running scrub, for the
    /// tests that only care about btrfs exit-code classification.
    ///
    /// The same `btrfs scrub status` request is issued twice with different
    /// questions -- the entry classifier asks "is a scrub owed?" (must be yes,
    /// or the run never starts) and the confirm poll asks "did my scrub
    /// register?" (must be yes). So the first probe is sequenced to report a
    /// stale finished scrub and every later one falls through to the running
    /// fixture.
    fn runner_with_status_running() -> MockRunner {
        let (status_req, running_out) = scrub_status_running();
        due_entry_runner().with_output(status_req, running_out)
    }

    #[test]
    // Intent: resume exit 0 returns Resumed without falling back to start.
    // Why it exists: scheduled scrub should continue saved work before starting fresh.
    // Scenario: a poll finds the pool due while cancelled scrub progress exists.
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
    // Scenario: a poll finds the pool due after all prior scrubs finished cleanly.
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
    // Intent: btrfs refusing the resume with its already-running rejection
    //   lands on AlreadyRunning -- a clean exit 0 -- not a failure, even though
    //   the entry probe had classified the pool as due.
    // Why it exists: the entry probe closes the window it can see, but an
    //   external scrub can still start between that probe and braid's spawn.
    //   That lost race used to reach ResumeFailed and beep the operator awake
    //   for a pool that was being scrubbed correctly at that very moment. The
    //   discrimination is btrfs's own refusal text and nothing else: a
    //   post-failure status re-probe would be racy in both directions.
    // Scenario: the poll finds an idle, due pool; an operator runs `btrfs scrub
    //   start /mnt/storage` by hand in the second before braid's own resume.
    fn resume_collision_with_an_external_scrub_is_not_a_failure() {
        let rig = Rig::new();
        let (reject_req, reject_out) = scrub_already_running_rejection(
            CmdRequest::BtrfsScrubResume {
                mount_point: scrub_mp(),
            },
            "btrfs scrub resume -B /mnt/storage",
        );
        let runner = runner_with_status_running().with_output(reject_req, reject_out);

        let result = rig.run(&runner).unwrap();
        assert_eq!(
            result,
            ScrubResumeOrStartResult::AlreadyRunning,
            "a lost race must land where the entry probe would have"
        );
        assert!(rig.lock_is_free(), "the run must release the pool lock");
    }

    #[test]
    // Intent: the same rejection on the start-after-fallback arm is not a
    //   failure either.
    // Why it exists: resume exit 2 hands off to `start`, so the collision can
    //   just as easily land on the second invocation. Classifying only the
    //   resume arm would leave the never-scrubbed-pool path still alerting.
    // Scenario: nothing to resume, so braid falls back to a fresh start -- and
    //   a hand-run scrub has claimed the pool in the meantime.
    fn start_collision_with_an_external_scrub_is_not_a_failure() {
        let rig = Rig::new();
        let (resume_req, resume_out) = scrub_resume_output(2);
        let (reject_req, reject_out) = scrub_already_running_rejection(
            CmdRequest::BtrfsScrubStart {
                mount_point: scrub_mp(),
            },
            "btrfs scrub start -B /mnt/storage",
        );
        let runner = runner_with_status_running()
            .with_output(resume_req, resume_out)
            .with_output(reject_req, reject_out);

        let result = rig.run(&runner).unwrap();
        assert_eq!(
            result,
            ScrubResumeOrStartResult::AlreadyRunning,
            "a lost race on the fallback start must classify the same way"
        );
    }

    #[test]
    // Intent: a deliberate teardown still wins over the collision shape -- the
    //   marker is checked first.
    // Why it exists: both outcomes keep the unit off onFailure, so a mix-up is
    //   invisible in the exit code but not in the journal: reporting a
    //   lock/suspend as "someone else is scrubbing" would send the operator
    //   hunting a hand-run scrub that never existed.
    // Scenario: `braid lock` runs while an external scrub holds the pool, so
    //   the marker is written and btrfs refuses with the already-running text.
    fn cancel_marker_outranks_the_collision_shape() {
        let rig = Rig::new();
        let marker = rig.paths.scrub_cancel_requested();
        let (_reject_req, reject_out) = scrub_already_running_rejection(
            CmdRequest::BtrfsScrubResume {
                mount_point: scrub_mp(),
            },
            "btrfs scrub resume -B /mnt/storage",
        );
        let runner = runner_with_status_running().with_handler(move |req: &CmdRequest| {
            if matches!(req, CmdRequest::BtrfsScrubResume { .. }) {
                std::fs::write(&marker, b"").unwrap();
                Some(Ok(reject_out.clone()))
            } else {
                None
            }
        });

        let result = rig.run(&runner).unwrap();
        assert_eq!(result, ScrubResumeOrStartResult::Cancelled);
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
        // Beyond the gate's status probe no output is registered, so any further
        // runner.run would surface as MissingMock and the returned
        // MarkerCleanupFailed proves cleanup short-circuited.
        let runner = due_entry_runner();

        let result = rig.run(&runner);
        assert!(
            matches!(
                result,
                Err(ScrubResumeOrStartError::MarkerCleanupFailed { .. })
            ),
            "unremovable marker must fail closed, got {result:?}"
        );
        assert!(
            !runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsScrubResume { .. } | CmdRequest::BtrfsScrubStart { .. }
            )),
            "no scrub may be resumed or started when entry cleanup fails"
        );
    }
}
