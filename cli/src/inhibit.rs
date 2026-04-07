//! Hold a logind sleep inhibitor for the lifetime of a value.
//!
//! Used during long-running braid operations (currently `replace`) where
//! suspending mid-flight produces kernel-level topology corruption — see
//! issues #45 and #48 and the upstream warning at
//! `reference/btrfs-progs/Documentation/btrfs-replace.rst:49-50`.
//!
//! # Runtime PATH dependencies
//!
//! [`SleepInhibitor::acquire`] spawns `systemd-inhibit`, which in turn
//! spawns a `sh -c "printf READY; exec sleep infinity"` child to hold the
//! inhibitor open. This requires `systemd-inhibit`, `sh`, and `sleep` on
//! `PATH`. The braid wrapper ([`modules/braid/wrapper.nix`]) puts
//! `pkgs.systemd` and the host PATH on the wrapped binary's PATH, so all
//! three are present in the supported NixOS deployment. systemd does not
//! ship a single-binary "block until killed with deterministic stdout"
//! helper, so the `sh + sleep` combination is the simplest portable way to
//! get a child program that signals readiness on stdout AND blocks until
//! killed by its parent.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

/// Kill the entire process group rooted at `child` and reap the direct
/// child. Used by both the `Drop` path and the failure path inside
/// `SleepInhibitor::acquire`, so the systemd-inhibit + sh + sleep tree is
/// always torn down as a unit instead of leaving the descendants as
/// orphans reparented to init.
fn kill_pgroup_and_reap(child: &mut Child) {
    // SAFETY: `libc::kill(-pgid, SIGKILL)` is the documented kernel
    // interface for signalling an entire process group. The pgid equals
    // the direct child's pid because we spawned with `process_group(0)`.
    // The pid is still valid in the kernel until we `wait()` on the
    // direct child below, so there is no pid-reuse window here.
    unsafe {
        libc::kill(-(child.id() as libc::pid_t), libc::SIGKILL);
    }
    let _ = child.wait();
}

/// Marker trait for sleep-inhibitor guards.
///
/// Blanket-implemented for any type so that implementations of
/// [`AcquireSleepInhibitor`] can choose any guard type they like — a real
/// [`SleepInhibitor`] in production, `()` in tests. A `Box<dyn SleepGuard>`
/// drops the inner value through its vtable when the box goes out of
/// scope, releasing the inhibitor.
pub trait SleepGuard {}
impl<T> SleepGuard for T {}

/// Seam for acquiring a sleep inhibitor from inside command flows.
///
/// Production code uses [`RealSleepInhibitor`], which spawns
/// `systemd-inhibit` and returns a [`SleepInhibitor`] guard. Unit tests use
/// [`RecordingInhibitor`], which returns a trivial guard so `cmd_*`
/// functions stay testable without spawning subprocesses, and records each
/// acquire call so tests can assert on the boundary placement.
pub trait AcquireSleepInhibitor {
    fn acquire(&self, why: &str) -> io::Result<Box<dyn SleepGuard>>;
}

/// Production implementation: spawns `systemd-inhibit`.
pub struct RealSleepInhibitor;

impl AcquireSleepInhibitor for RealSleepInhibitor {
    fn acquire(&self, why: &str) -> io::Result<Box<dyn SleepGuard>> {
        Ok(Box::new(SleepInhibitor::acquire(why)?))
    }
}

/// Test-only implementation: never spawns anything, records each acquire
/// call so tests can assert on whether (and how often) the seam was hit.
#[cfg(test)]
#[derive(Default)]
pub struct RecordingInhibitor {
    acquire_count: std::cell::Cell<usize>,
}

#[cfg(test)]
impl RecordingInhibitor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn acquire_count(&self) -> usize {
        self.acquire_count.get()
    }
}

#[cfg(test)]
impl AcquireSleepInhibitor for RecordingInhibitor {
    fn acquire(&self, _why: &str) -> io::Result<Box<dyn SleepGuard>> {
        self.acquire_count.set(self.acquire_count.get() + 1);
        Ok(Box::new(()))
    }
}

/// RAII guard holding a `What=sleep, Who=braid, Mode=block` logind inhibitor.
///
/// Spawns `systemd-inhibit` (which itself supervises a `sh + sleep` child
/// to keep the inhibitor open) in its own process group. The lock is held
/// for as long as the guard is alive. `Drop` SIGKILLs the entire process
/// group via `kill(-pgid, ...)` and reaps the direct child, so the
/// supervised `sh`/`sleep` is torn down with the parent instead of leaking
/// as an orphan reparented to init. logind releases the inhibitor as soon
/// as the holding process exits.
pub struct SleepInhibitor {
    child: Child,
}

impl SleepInhibitor {
    /// Acquire the inhibitor. Blocks until logind has registered it.
    ///
    /// `systemd-inhibit` acquires the inhibitor lock from logind before
    /// exec'ing its child argv, so reading the `READY` sentinel printed by
    /// the child is a race-free handshake that the lock is held. Without
    /// this handshake there would be a window between `Command::spawn`
    /// returning and the child registering with logind, during which a
    /// suspend could slip through.
    ///
    /// Returns an io error if `systemd-inhibit` cannot be spawned or exits
    /// before printing the sentinel (e.g. logind is unreachable).
    pub fn acquire(why: &str) -> io::Result<Self> {
        // process_group(0) puts systemd-inhibit (and the sh/sleep child it
        // supervises) in a fresh process group rooted at the systemd-inhibit
        // pid, so teardown can SIGKILL the whole group via `kill(-pgid, ...)`
        // instead of just the direct child. Without this the supervised
        // sleep would survive systemd-inhibit's death and leak as an orphan
        // reparented to init on every replace.
        let mut child = Command::new("systemd-inhibit")
            .args(["--what=sleep", "--who=braid", "--mode=block"])
            .arg(format!("--why={why}"))
            .args(["sh", "-c", "printf READY; exec sleep infinity"])
            .stdout(Stdio::piped())
            .process_group(0)
            .spawn()?;

        // Run the handshake in an inner closure so any failure (read_exact
        // EOF/io error, sentinel mismatch) flows through a single cleanup
        // branch below — never leak the spawned process group.
        let handshake = (|| -> io::Result<()> {
            let mut buf = [0u8; 5];
            child
                .stdout
                .as_mut()
                .expect("stdout was piped")
                .read_exact(&mut buf)?;
            if &buf != b"READY" {
                // Defensive — should be unreachable since we control the
                // child argv.
                return Err(io::Error::other(
                    "systemd-inhibit handshake produced unexpected output",
                ));
            }
            Ok(())
        })();

        match handshake {
            Ok(()) => Ok(Self { child }),
            Err(e) => {
                kill_pgroup_and_reap(&mut child);
                Err(e)
            }
        }
    }
}

impl Drop for SleepInhibitor {
    fn drop(&mut self) {
        kill_pgroup_and_reap(&mut self.child);
    }
}
