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

/// Marker trait for pool-lock guards so production and tests can choose
/// different concrete guard types while dispatch owns the drop boundary.
pub trait PoolLockGuard {}
impl<T> PoolLockGuard for T {}

/// Acquire the global pool-operation lock with command-specific wait policy.
pub trait AcquirePoolLock {
    fn acquire(&self) -> Result<Box<dyn PoolLockGuard>, PoolLockError>;

    fn acquire_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Box<dyn PoolLockGuard>, PoolLockError>;

    fn acquire_with_systemd_stop_deadline(
        &self,
        deadline: Duration,
    ) -> Result<Box<dyn PoolLockGuard>, PoolLockError>;
}

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

impl AcquirePoolLock for RealPoolLock {
    fn acquire(&self) -> Result<Box<dyn PoolLockGuard>, PoolLockError> {
        Ok(Box::new(self.try_acquire()?))
    }

    fn acquire_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Box<dyn PoolLockGuard>, PoolLockError> {
        Ok(Box::new(
            self.poll_acquire(timeout, |_| PoolLockError::AlreadyHeld)?,
        ))
    }

    fn acquire_with_systemd_stop_deadline(
        &self,
        deadline: Duration,
    ) -> Result<Box<dyn PoolLockGuard>, PoolLockError> {
        Ok(Box::new(self.poll_acquire(deadline, |waited| {
            PoolLockError::DeadlineExpired { waited }
        })?))
    }
}

/// Owns the open file description that carries the kernel flock.
pub struct RealPoolLockGuard {
    _file: File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordedPoolLockMode {
    NonBlocking,
    Timeout(Duration),
    SystemdStopDeadline(Duration),
}

/// Test seam that records which acquisition policy dispatch selected.
#[cfg(test)]
#[derive(Default)]
pub struct RecordingPoolLock {
    calls: std::sync::Mutex<Vec<RecordedPoolLockMode>>,
    already_held: std::sync::atomic::AtomicBool,
}

#[cfg(test)]
impl RecordingPoolLock {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_already_held(&self, held: bool) {
        self.already_held
            .store(held, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn calls(&self) -> Vec<RecordedPoolLockMode> {
        self.calls.lock().expect("recording lock poisoned").clone()
    }
}

#[cfg(test)]
impl AcquirePoolLock for RecordingPoolLock {
    fn acquire(&self) -> Result<Box<dyn PoolLockGuard>, PoolLockError> {
        self.calls
            .lock()
            .expect("recording lock poisoned")
            .push(RecordedPoolLockMode::NonBlocking);
        if self.already_held.load(std::sync::atomic::Ordering::SeqCst) {
            Err(PoolLockError::AlreadyHeld)
        } else {
            Ok(Box::new(()))
        }
    }

    fn acquire_with_timeout(
        &self,
        timeout: Duration,
    ) -> Result<Box<dyn PoolLockGuard>, PoolLockError> {
        self.calls
            .lock()
            .expect("recording lock poisoned")
            .push(RecordedPoolLockMode::Timeout(timeout));
        if self.already_held.load(std::sync::atomic::Ordering::SeqCst) {
            Err(PoolLockError::AlreadyHeld)
        } else {
            Ok(Box::new(()))
        }
    }

    fn acquire_with_systemd_stop_deadline(
        &self,
        deadline: Duration,
    ) -> Result<Box<dyn PoolLockGuard>, PoolLockError> {
        self.calls
            .lock()
            .expect("recording lock poisoned")
            .push(RecordedPoolLockMode::SystemdStopDeadline(deadline));
        if self.already_held.load(std::sync::atomic::Ordering::SeqCst) {
            Err(PoolLockError::DeadlineExpired { waited: deadline })
        } else {
            Ok(Box::new(()))
        }
    }
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

    pub fn acquire(&self) -> Result<StopCoordinatorGuard, StopCoordinatorError> {
        let file = open_lock_file(&self.path)?;
        match flock(file.as_raw_fd(), FlockArg::LockExclusiveNonblock) {
            Ok(()) => {
                file.set_len(0).map_err(StopCoordinatorError::Io)?;
                Ok(StopCoordinatorGuard { file })
            }
            Err(e) if would_block(e) => Err(StopCoordinatorError::Held),
            Err(e) => Err(StopCoordinatorError::Io(io_from_errno(e))),
        }
    }

    pub fn poll_for_done_or_release(&self, deadline: Duration) -> StopCoordinatorPollResult {
        let start = Instant::now();
        loop {
            if std::fs::read(&self.path).is_ok_and(|bytes| bytes == DONE_MARKER) {
                return StopCoordinatorPollResult::Done;
            }
            match self.acquire() {
                Ok(guard) => return StopCoordinatorPollResult::Acquired(guard),
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
    fn already_held_display_is_wrapper_compatible_verbatim() {
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
