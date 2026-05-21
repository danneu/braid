//! Rust-owned pool-operation locking for commands that mutate pool state.
//!
//! The lock is acquired at CLI dispatch so config reads, membership reads,
//! probes, prompts, journals, and lifecycle fixups all sit inside the same
//! serialized critical section.

#![allow(deprecated)] // This module deliberately uses BSD flock(2) via nix.

use nix::errno::Errno;
use nix::fcntl::{FlockArg, OFlag, flock, open};
use nix::sys::stat::Mode;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};
use thiserror::Error;

const POOL_LOCK_PATH: &str = "/run/braid-pool.lock";
const STOP_COORDINATOR_PATH: &str = "/run/braid-stop-coordinator.lock";
const POOL_POLL_INTERVAL: Duration = Duration::from_millis(250);
const STOP_COORDINATOR_POLL_INTERVAL: Duration = Duration::from_millis(100);
const DONE_MARKER: &[u8] = b"done\n";

#[derive(Debug, Error)]
pub enum PoolLockError {
    #[error(
        "braid: another braid operation is already in progress (pool lock /run/braid-pool.lock is held); retry once it finishes"
    )]
    AlreadyHeld,
    #[error("braid: pool lock not released within {waited:.0?}; aborting --systemd-stop")]
    DeadlineExpired { waited: Duration },
    #[error("pool lock I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Production BSD `flock(2)` owner for `/run/braid-pool.lock`.
pub struct RealPoolLock {
    path: PathBuf,
}

impl RealPoolLock {
    pub fn production() -> Self {
        Self::new(POOL_LOCK_PATH)
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Non-blocking acquisition shared by fail-fast user operations and
    /// timer-driven monitor cycles.
    pub fn acquire(&self) -> Result<RealPoolLockGuard, PoolLockError> {
        self.try_acquire()
    }

    /// Bounded user-operation wait for cases where short monitor contention is
    /// expected and retrying immediately would be noisy.
    pub fn acquire_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<RealPoolLockGuard, PoolLockError> {
        self.poll_acquire(timeout, |_| PoolLockError::AlreadyHeld)
    }

    /// Deadline-aware shutdown wait so ExecStop fails before systemd's outer
    /// timeout can kill cleanup mid-operation.
    pub fn acquire_with_systemd_stop_deadline(
        &self,
        deadline: Duration,
    ) -> Result<RealPoolLockGuard, PoolLockError> {
        self.poll_acquire(deadline, |waited| PoolLockError::DeadlineExpired { waited })
    }

    fn try_acquire(&self) -> Result<RealPoolLockGuard, PoolLockError> {
        let file = open_lock_file(&self.path)?;
        match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
            Ok(()) => Ok(RealPoolLockGuard { _file: file }),
            Err(e) if would_block(e) => Err(PoolLockError::AlreadyHeld),
            Err(e) => Err(PoolLockError::Io(io_from_errno(e))),
        }
    }

    fn poll_acquire(
        &self,
        timeout: Duration,
        expired: impl Fn(Duration) -> PoolLockError,
    ) -> Result<RealPoolLockGuard, PoolLockError> {
        let start = Instant::now();
        loop {
            match self.try_acquire() {
                Ok(guard) => return Ok(guard),
                Err(PoolLockError::AlreadyHeld) if start.elapsed() < timeout => {
                    thread::sleep(POOL_POLL_INTERVAL);
                }
                Err(PoolLockError::AlreadyHeld) => return Err(expired(start.elapsed())),
                Err(e) => return Err(e),
            }
        }
    }
}

/// Owns the open file description that carries the kernel flock.
pub struct RealPoolLockGuard {
    _file: File,
}

#[derive(Debug, Error)]
pub enum StopCoordinatorError {
    #[error("stop coordinator is held")]
    Held,
    #[error("stop coordinator I/O error: {0}")]
    Io(#[from] io::Error),
}

/// Result of polling a held stop coordinator during `--systemd-stop`.
pub enum StopCoordinatorPollResult {
    Done,
    Acquired(StopCoordinatorGuard),
    Deadline,
}

/// Coordinates plain `braid lock` with its recursive ExecStop reentry.
pub struct RealStopCoordinator {
    path: PathBuf,
}

impl RealStopCoordinator {
    pub fn production() -> Self {
        Self::new(STOP_COORDINATOR_PATH)
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Opens and locks the coordinator without touching content so polling can
    /// preserve the predecessor's marker while fresh transitions still truncate.
    fn open_and_lock(&self) -> Result<File, StopCoordinatorError> {
        let file = open_lock_file(&self.path)?;
        match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
            Ok(()) => Ok(file),
            Err(e) if would_block(e) => Err(StopCoordinatorError::Held),
            Err(e) => Err(StopCoordinatorError::Io(io_from_errno(e))),
        }
    }

    pub fn acquire(&self) -> Result<StopCoordinatorGuard, StopCoordinatorError> {
        let file = self.open_and_lock()?;
        file.set_len(0).map_err(StopCoordinatorError::Io)?;
        Ok(StopCoordinatorGuard { file })
    }

    pub fn poll_for_done_or_release(&self, deadline: Duration) -> StopCoordinatorPollResult {
        self.poll_for_done_or_release_inner(deadline, || {})
    }

    /// Test seam for the TOCTOU window between pre-read and flock acquisition.
    fn poll_for_done_or_release_inner<F: FnMut()>(
        &self,
        deadline: Duration,
        mut after_pre_read: F,
    ) -> StopCoordinatorPollResult {
        let start = Instant::now();
        loop {
            if std::fs::read(&self.path).is_ok_and(|bytes| bytes == DONE_MARKER) {
                return StopCoordinatorPollResult::Done;
            }
            after_pre_read();
            match self.open_and_lock() {
                Ok(file) => {
                    if std::fs::read(&self.path).is_ok_and(|bytes| bytes == DONE_MARKER) {
                        drop(file);
                        return StopCoordinatorPollResult::Done;
                    }
                    return StopCoordinatorPollResult::Acquired(StopCoordinatorGuard { file });
                }
                Err(StopCoordinatorError::Held) if start.elapsed() < deadline => {
                    thread::sleep(STOP_COORDINATOR_POLL_INTERVAL);
                }
                Err(StopCoordinatorError::Held) => return StopCoordinatorPollResult::Deadline,
                Err(_) if start.elapsed() < deadline => {
                    thread::sleep(STOP_COORDINATOR_POLL_INTERVAL);
                }
                Err(_) => return StopCoordinatorPollResult::Deadline,
            }
        }
    }
}

/// Held coordinator lock plus the `done\n` marker writer.
pub struct StopCoordinatorGuard {
    file: File,
}

impl StopCoordinatorGuard {
    pub fn mark_done(&self) -> io::Result<()> {
        use std::os::unix::fs::FileExt;

        self.file.set_len(0)?;
        self.file.write_all_at(DONE_MARKER, 0)
    }
}

fn open_lock_file(path: &Path) -> io::Result<File> {
    let fd = open(
        path,
        OFlag::O_RDWR | OFlag::O_CREAT | OFlag::O_CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(io_from_errno)?;
    Ok(File::from(fd))
}

fn would_block(errno: Errno) -> bool {
    errno == Errno::EWOULDBLOCK || errno == Errno::EAGAIN
}

fn io_from_errno(errno: Errno) -> io::Error {
    io::Error::from_raw_os_error(errno as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn already_held_display_matches_pinned_contention_string() {
        assert_eq!(
            PoolLockError::AlreadyHeld.to_string(),
            "braid: another braid operation is already in progress (pool lock /run/braid-pool.lock is held); retry once it finishes"
        );
    }

    #[test]
    fn deadline_expired_display_distinguishes_from_already_held() {
        let msg = PoolLockError::DeadlineExpired {
            waited: Duration::from_secs(5),
        }
        .to_string();
        assert!(msg.contains("--systemd-stop"));
        assert!(msg.contains("pool lock not released"));
    }

    #[test]
    fn acquire_returns_already_held_on_second_attempt() {
        let dir = tempfile::tempdir().unwrap();
        let lock = RealPoolLock::new(dir.path().join("pool.lock"));
        let _first = lock.acquire().unwrap();
        let err = match lock.acquire() {
            Ok(_) => panic!("second acquire should contend"),
            Err(err) => err,
        };
        assert!(matches!(err, PoolLockError::AlreadyHeld));
    }

    #[test]
    fn acquire_with_timeout_returns_already_held_on_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let lock = RealPoolLock::new(dir.path().join("pool.lock"));
        let _first = lock.acquire().unwrap();
        let err = match lock.acquire_with_timeout(Duration::from_millis(20)) {
            Ok(_) => panic!("timeout should report user contention"),
            Err(err) => err,
        };
        assert!(matches!(err, PoolLockError::AlreadyHeld));
    }

    // Intent: acquire_with_timeout returns Ok when the holder releases
    // mid-poll, exercising poll_acquire's sleep-then-retry branch -- not only
    // the uncontested fast path or the expiry path.
    // Why it exists: protects the positive-shape gate for `braid ack`'s
    // bounded wait. A regression in poll_acquire -- an off-by-one on
    // `start.elapsed() < timeout`, exit-on-first AlreadyHeld, or a change that
    // turns the polled LockExclusiveNonblock into a one-shot try -- would
    // silently make ack refuse to wait out a short concurrent operation while
    // existing Rust unit and VM tests still passed.
    // Scenario: `braid ack` runs while a concurrent monitor cycle briefly
    // holds the pool lock; ack should observe the release within its bounded
    // wait window and proceed.
    #[test]
    fn acquire_with_timeout_polls_then_succeeds_after_holder_release() {
        let dir = tempfile::tempdir().unwrap();
        let lock = RealPoolLock::new(dir.path().join("pool.lock"));

        let holder = lock.try_acquire().expect("initial holder acquire");
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(holder);
        });

        let start = Instant::now();
        let result = lock.acquire_with_timeout(Duration::from_secs(2));
        let elapsed = start.elapsed();
        releaser.join().expect("releaser panicked");

        assert!(
            result.is_ok(),
            "expected Ok after holder release; got {:?}",
            result.err()
        );
        assert!(
            elapsed >= POOL_POLL_INTERVAL,
            "main thread did not exercise the retry path; elapsed={:?}",
            elapsed
        );
    }

    #[test]
    fn acquire_with_systemd_stop_deadline_returns_deadline_expired_on_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let lock = RealPoolLock::new(dir.path().join("pool.lock"));
        let _first = lock.acquire().unwrap();
        let err = match lock.acquire_with_systemd_stop_deadline(Duration::from_millis(20)) {
            Ok(_) => panic!("deadline should report systemd-stop expiry"),
            Err(err) => err,
        };
        assert!(matches!(err, PoolLockError::DeadlineExpired { .. }));
    }

    // Intent: acquire_with_systemd_stop_deadline returns Ok when the holder
    // releases mid-poll, exercising poll_acquire's sleep-then-retry branch --
    // not only the uncontested fast path or the expiry path.
    // Why it exists: protects the positive-shape gate for `braid lock
    // --systemd-stop`'s shutdown wait. A regression in poll_acquire -- an
    // off-by-one on `start.elapsed() < timeout`, exit-on-first AlreadyHeld, or
    // a change that turns the polled LockExclusiveNonblock into a one-shot
    // try -- would silently make the systemd stop path refuse to wait out a
    // short concurrent operation while existing Rust unit and VM tests still
    // passed.
    // Scenario: `braid lock --systemd-stop` runs while a concurrent mutator
    // briefly holds the pool lock during shutdown; the stop path should
    // observe the release within its deadline and proceed.
    #[test]
    fn acquire_with_systemd_stop_deadline_polls_then_succeeds_after_holder_release() {
        let dir = tempfile::tempdir().unwrap();
        let lock = RealPoolLock::new(dir.path().join("pool.lock"));

        let holder = lock.try_acquire().expect("initial holder acquire");
        let releaser = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(holder);
        });

        let start = Instant::now();
        let result = lock.acquire_with_systemd_stop_deadline(Duration::from_secs(2));
        let elapsed = start.elapsed();
        releaser.join().expect("releaser panicked");

        assert!(
            result.is_ok(),
            "expected Ok after holder release; got {:?}",
            result.err()
        );
        assert!(
            elapsed >= POOL_POLL_INTERVAL,
            "main thread did not exercise the retry path; elapsed={:?}",
            elapsed
        );
    }

    #[test]
    fn stop_coordinator_acquire_then_second_acquire_returns_held() {
        let dir = tempfile::tempdir().unwrap();
        let coord = RealStopCoordinator::new(dir.path().join("coord.lock"));
        let _first = coord.acquire().unwrap();
        let err = match coord.acquire() {
            Ok(_) => panic!("second acquire should contend"),
            Err(err) => err,
        };
        assert!(matches!(err, StopCoordinatorError::Held));
    }

    #[test]
    fn stop_coordinator_acquire_truncates_stale_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coord.lock");
        std::fs::write(&path, DONE_MARKER).unwrap();
        let coord = RealStopCoordinator::new(&path);
        let _guard = coord.acquire().unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"");
    }

    // Intent: open_and_lock takes the flock without touching file content.
    // Why it exists: poll_for_done_or_release depends on the post-acquire
    // re-read to disambiguate "predecessor died after mark_done" from
    // "predecessor died before mark_done". A refactor that re-introduces
    // truncate-on-acquire into this helper would silently reintroduce the
    // redundant-cmd_lock race. acquire() truncates only because it is reserved
    // for fresh-transition callers where pre-existing content is stale.
    // Scenario: a prior session wrote done\n then exited; this session's poll
    // path calls open_and_lock as part of disambiguating predecessor state.
    #[test]
    fn open_and_lock_preserves_pre_seeded_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coord.lock");
        std::fs::write(&path, DONE_MARKER).unwrap();
        let coord = RealStopCoordinator::new(&path);
        let file = coord
            .open_and_lock()
            .expect("flock should succeed on fresh file");
        drop(file);
        assert_eq!(std::fs::read(&path).unwrap(), DONE_MARKER);
    }

    // Intent: poll_for_done_or_release returns Done and preserves the on-disk
    // done\n marker when the predecessor wrote done\n and died in the window
    // between the poller's pre-read and the poller's flock attempt.
    // Why it exists: this is the specific TOCTOU race the fix closes. The
    // pre-read short-circuit cannot see the marker because the predecessor has
    // not written it yet at pre-read time; the bug is whether the post-acquire
    // branch re-reads and observes the marker that the predecessor wrote in
    // between. A regression that re-introduced truncate-on-acquire would
    // silently wipe the marker on the post-acquire branch and reduce this
    // test's expected Done to Acquired.
    // Scenario: plain `braid lock` is in cmd_lock at the moment the reentry's
    // first poll iteration fires. Plain finishes cmd_lock, writes done\n via
    // mark_done, and is then SIGKILL'd inside mark_offline before its
    // coordinator guard drops naturally. The kernel releases the flock on
    // process death. The reentry's next open_and_lock wins the flock and must
    // observe the surviving done\n on the post-acquire re-read.
    #[test]
    fn poll_for_done_or_release_returns_done_when_predecessor_marks_done_and_dies_between_pre_read_and_acquire()
     {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coord.lock");
        let coord = RealStopCoordinator::new(&path);

        let predecessor = coord.acquire().expect("predecessor wins flock");
        let predecessor_cell = std::cell::Cell::new(Some(predecessor));

        let result = coord.poll_for_done_or_release_inner(Duration::from_millis(100), || {
            if let Some(p) = predecessor_cell.take() {
                p.mark_done().expect("mark_done writes DONE_MARKER");
                drop(p);
            }
        });

        assert!(
            matches!(result, StopCoordinatorPollResult::Done),
            "expected Done after predecessor wrote done\\n and died in the TOCTOU window"
        );
        assert_eq!(
            std::fs::read(&path).unwrap(),
            DONE_MARKER,
            "post-acquire branch must preserve the on-disk done\\n marker"
        );
    }

    #[test]
    fn stop_coordinator_poll_returns_done_while_holder_still_holds() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coord.lock");
        let coord = RealStopCoordinator::new(&path);
        let guard = coord.acquire().unwrap();
        guard.mark_done().unwrap();

        let other = RealStopCoordinator::new(&path);
        match other.poll_for_done_or_release(Duration::from_millis(50)) {
            StopCoordinatorPollResult::Done => {}
            _ => panic!("expected done while original holder still owns flock"),
        }
        drop(guard);
    }

    #[test]
    fn stop_coordinator_poll_returns_acquired_after_holder_releases_without_done() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coord.lock");
        let coord = RealStopCoordinator::new(&path);
        let guard = coord.acquire().unwrap();
        let other = RealStopCoordinator::new(&path);
        drop(guard);

        match other.poll_for_done_or_release(Duration::from_millis(50)) {
            StopCoordinatorPollResult::Acquired(_) => {}
            _ => panic!("expected acquired after release without done"),
        }
    }

    #[test]
    fn stop_coordinator_poll_returns_deadline_when_held_with_empty_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("coord.lock");
        let coord = RealStopCoordinator::new(&path);
        let _guard = coord.acquire().unwrap();
        let other = RealStopCoordinator::new(&path);

        match other.poll_for_done_or_release(Duration::from_millis(20)) {
            StopCoordinatorPollResult::Deadline => {}
            _ => panic!("expected deadline"),
        }
    }
}
