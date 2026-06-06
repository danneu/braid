//! Immutable-attribute guard for the bare pool mountpoint while it is offline.
//!
//! When the pool is NOT mounted, the bare mountpoint directory is a writable
//! directory on the root filesystem; any process writing under it lands data
//! on root and gets shadowed when the pool later mounts over it. Setting the
//! inode immutable flag (`FS_IMMUTABLE_FL`, `chattr +i`) turns that silent
//! write-to-root into a loud `EPERM`. See
//! `docs/design/decisions/028-immutable-unmounted-mountpoint.md`.
//!
//! The single hard timing rule -- never set `+i` on a path that is CURRENTLY a
//! mount root, or we seal the mounted filesystem's own root inode and block all
//! pool writes -- is enforced atomically on one fd via `statx`'s
//! `STATX_ATTR_MOUNT_ROOT`, so a racing mount can never trick us into sealing a
//! live root. This module mirrors the ioctl seam in `btrfs_ioctl.rs`: a trait
//! plus a real Linux implementation, a non-Linux stub, and a mock.

use std::path::Path;

use nix::errno::Errno;
use thiserror::Error;

#[cfg(target_os = "linux")]
use nix::fcntl::{OFlag, open};
#[cfg(target_os = "linux")]
use nix::sys::stat::Mode;
#[cfg(target_os = "linux")]
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::RawFd;

/// The kernel inode flag that makes a directory immutable
/// (`reference/linux/include/uapi/linux/fs.h` -> `FS_IMMUTABLE_FL`). Typed as
/// `FsFlagsArg` so it composes with the ioctl buffer without per-use casts.
#[cfg(target_os = "linux")]
const FS_IMMUTABLE_FL: FsFlagsArg = 0x10;

/// ABI buffer type for `FS_IOC_{GET,SET}FLAGS`. Load-bearing: the kernel
/// defines these as `_IOR('f',1,long)` / `_IOW('f',2,long)`, so the request
/// number is derived from `sizeof(long)`. A `c_int` here would encode the wrong
/// request number, the switch would never match, and the ioctl would silently
/// return `ENOTTY` (protection inert). Defined once and shared by both ioctl
/// macro invocations and the ABI request-number assertion so a regression can
/// only flip it in one place.
#[cfg(target_os = "linux")]
type FsFlagsArg = libc::c_long;

// Generated ioctl wrappers. The request number is computed from
// `size_of::<FsFlagsArg>()`; the const-assertion test below pins it.
#[cfg(target_os = "linux")]
nix::ioctl_read!(fs_ioc_getflags, b'f', 1, FsFlagsArg);
#[cfg(target_os = "linux")]
nix::ioctl_write_ptr!(fs_ioc_setflags, b'f', 2, FsFlagsArg);

/// Result of a single `enforce` call. Distinguishes the outcomes the boot seal
/// logs as debug (steady state), the operator-facing maintenance forms map to
/// exit codes, and the doctor reads back -- one variant per remediation the
/// operator might need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardOutcome {
    /// `+i` was newly set.
    Set,
    /// `+i` was newly cleared.
    Cleared,
    /// `+i` was already set; no change.
    AlreadyImmutable,
    /// `+i` was already clear; no change.
    AlreadyMutable,
    /// Path is currently a mount root; flags were not touched (timing rule).
    SkippedMounted,
    /// Path does not exist.
    Absent,
    /// Filesystem does not support the immutable attribute (`ENOTTY`/`EOPNOTSUPP`).
    Unsupported,
    /// Mount state could not be determined (kernel lacks `STATX_ATTR_MOUNT_ROOT`,
    /// or `statx` failed); flags were not touched (fail closed).
    MountStateUnknown,
    /// Path exists but is not a directory; refused.
    NotADirectory,
}

/// Errors at the guard syscall boundary. `enforce` returns these only for
/// genuine failures (a permission error on the set, an unexpected open/statx/
/// ioctl errno); recoverable conditions travel as `GuardOutcome` instead.
/// `is_immutable` returns `Err` for every non-bool case (absent, unsupported,
/// old kernel, I/O), which the doctor maps to an `Indeterminate` probe.
#[derive(Debug, Clone, Error)]
pub enum GuardError {
    /// The filesystem does not support the immutable attribute. Returned by
    /// `is_immutable` (and the non-Linux stub) where there is no honest bool.
    #[error("filesystem does not support the immutable attribute")]
    Unsupported,
    #[error("open {path}: {errno}")]
    Open { path: String, errno: Errno },
    #[error("statx {path}: {errno}")]
    Statx { path: String, errno: Errno },
    #[error("read inode flags on {path}: {errno}")]
    GetFlags { path: String, errno: Errno },
    #[error("set inode flags on {path}: {errno}")]
    SetFlags { path: String, errno: Errno },
}

/// The single seam the seal site, the maintenance levers, and the doctor share
/// so the not-a-mountpoint invariant and the flag write live behind one
/// testable boundary (mock in tests, real ioctl in production).
pub trait MountpointGuard {
    /// Enforce immutability on `path`, but ONLY when `path` is not currently a
    /// mountpoint. The not-a-mountpoint check and the flag write happen on the
    /// same fd so a racing mount can never cause us to seal a live fs root.
    fn enforce(&self, path: &Path, want_immutable: bool) -> Result<GuardOutcome, GuardError>;
    /// Read current immutability (for doctor). The doctor maps `Err` (absent /
    /// unsupported / old kernel / I/O) to an `Indeterminate` probe, not a
    /// failure.
    fn is_immutable(&self, path: &Path) -> Result<bool, GuardError>;
}

/// Production guard. On Linux it is an fd + `statx` + `FS_IOC_*` ioctl; on every
/// other target it is an inert stub so the cross-platform crate still builds and
/// `just test-rust` runs the abstract half on the macOS host.
pub struct RealMountpointGuard;

#[cfg(target_os = "linux")]
impl MountpointGuard for RealMountpointGuard {
    fn enforce(&self, path: &Path, want_immutable: bool) -> Result<GuardOutcome, GuardError> {
        // 1. Open as a directory. O_DIRECTORY makes the kernel reject a
        //    non-directory at open; O_CLOEXEC keeps the fd from leaking.
        let fd = match open(
            path,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        ) {
            Ok(fd) => fd,
            Err(Errno::ENOENT) => return Ok(GuardOutcome::Absent),
            Err(Errno::ENOTDIR) => return Ok(GuardOutcome::NotADirectory),
            Err(errno) => {
                return Err(GuardError::Open {
                    path: path.display().to_string(),
                    errno,
                });
            }
        };
        let raw = fd.as_raw_fd();

        // 2. Authoritative fd-based mount-root check. If the path is a mount
        //    root (when mounted, `open` followed into the pool root), never
        //    touch flags. A mount that races in AFTER open leaves this fd on the
        //    underlying bare-dir inode, so step 4 still seals the bare dir.
        match mount_root_state(raw, path)? {
            MountRootState::IsMountRoot => return Ok(GuardOutcome::SkippedMounted),
            MountRootState::Unknown => return Ok(GuardOutcome::MountStateUnknown),
            MountRootState::NotMountRoot => {}
        }

        // 3. Read current flags. ENOTTY/EOPNOTSUPP => the root fs does not
        //    support the attribute.
        let mut flags: FsFlagsArg = 0;
        // SAFETY: `raw` is a valid open directory fd; `flags` points to an
        // `FsFlagsArg` buffer matching the ioctl's declared type.
        match unsafe { fs_ioc_getflags(raw, &mut flags) } {
            Ok(_) => {}
            Err(Errno::ENOTTY) | Err(Errno::EOPNOTSUPP) => {
                return Ok(GuardOutcome::Unsupported);
            }
            Err(errno) => {
                return Err(GuardError::GetFlags {
                    path: path.display().to_string(),
                    errno,
                });
            }
        }

        // 4. Compute the desired flags and write only on a real change.
        let desired = if want_immutable {
            flags | FS_IMMUTABLE_FL
        } else {
            flags & !FS_IMMUTABLE_FL
        };
        if desired == flags {
            return Ok(if want_immutable {
                GuardOutcome::AlreadyImmutable
            } else {
                GuardOutcome::AlreadyMutable
            });
        }
        // SAFETY: `raw` is a valid open directory fd; `desired` points to an
        // `FsFlagsArg` buffer matching the ioctl's declared type.
        match unsafe { fs_ioc_setflags(raw, &desired) } {
            Ok(_) => Ok(if want_immutable {
                GuardOutcome::Set
            } else {
                GuardOutcome::Cleared
            }),
            Err(errno) => Err(GuardError::SetFlags {
                path: path.display().to_string(),
                errno,
            }),
        }
    }

    fn is_immutable(&self, path: &Path) -> Result<bool, GuardError> {
        // Pure read: open whatever the path resolves to (the bare dir when
        // offline, the pool root when mounted) and report its flag. The doctor
        // uses the mounted-vs-offline distinction separately.
        let fd = open(
            path,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| GuardError::Open {
            path: path.display().to_string(),
            errno,
        })?;
        let mut flags: FsFlagsArg = 0;
        // SAFETY: `fd` is a valid open directory fd; `flags` points to an
        // `FsFlagsArg` buffer matching the ioctl's declared type.
        match unsafe { fs_ioc_getflags(fd.as_raw_fd(), &mut flags) } {
            Ok(_) => Ok(flags & FS_IMMUTABLE_FL != 0),
            Err(Errno::ENOTTY) | Err(Errno::EOPNOTSUPP) => Err(GuardError::Unsupported),
            Err(errno) => Err(GuardError::GetFlags {
                path: path.display().to_string(),
                errno,
            }),
        }
    }
}

#[cfg(not(target_os = "linux"))]
impl MountpointGuard for RealMountpointGuard {
    fn enforce(&self, _path: &Path, _want_immutable: bool) -> Result<GuardOutcome, GuardError> {
        Ok(GuardOutcome::Unsupported)
    }
    fn is_immutable(&self, _path: &Path) -> Result<bool, GuardError> {
        Err(GuardError::Unsupported)
    }
}

/// Tri-state result of the fd-based mount-root probe, separate from
/// `GuardOutcome` so the `enforce` step reads as a decision, not a side effect.
#[cfg(target_os = "linux")]
enum MountRootState {
    IsMountRoot,
    NotMountRoot,
    /// Mount state could not be determined; the caller must fail closed.
    Unknown,
}

/// `statx(fd, "", AT_EMPTY_PATH)` mount-root predicate. `STATX_ATTR_MOUNT_ROOT`
/// is authoritative -- unlike an `st_dev`-vs-parent comparison it also detects
/// same-device and bind mountpoints. If the kernel does not report the bit (old
/// kernel) or `statx` fails, returns `Unknown` so the caller makes no flag
/// change rather than guessing.
#[cfg(target_os = "linux")]
fn mount_root_state(fd: RawFd, path: &Path) -> Result<MountRootState, GuardError> {
    // SAFETY: `statx` only writes into `buf`, a zeroed owned `statx` struct.
    let mut buf: libc::statx = unsafe { std::mem::zeroed() };
    // SAFETY: empty pathname + AT_EMPTY_PATH operates on `fd` itself; `buf` is a
    // valid writable statx buffer.
    let rc = unsafe {
        libc::statx(
            fd,
            c"".as_ptr(),
            libc::AT_EMPTY_PATH,
            libc::STATX_BASIC_STATS,
            &mut buf,
        )
    };
    if rc != 0 {
        // statx itself failed (e.g. kernel without statx). We cannot determine
        // mount state, so fail closed rather than surface a hard error: the boot
        // seal must still exit 0. Record the errno for the maintenance forms.
        let errno = Errno::last();
        return match errno {
            // ENOSYS (no statx) / EINVAL: treat as "cannot determine".
            Errno::ENOSYS | Errno::EINVAL => Ok(MountRootState::Unknown),
            other => Err(GuardError::Statx {
                path: path.display().to_string(),
                errno: other,
            }),
        };
    }
    let bit = libc::STATX_ATTR_MOUNT_ROOT as u64;
    if buf.stx_attributes_mask & bit == 0 {
        // Kernel does not report this attribute (older than 5.8).
        return Ok(MountRootState::Unknown);
    }
    if buf.stx_attributes & bit != 0 {
        Ok(MountRootState::IsMountRoot)
    } else {
        Ok(MountRootState::NotMountRoot)
    }
}

/// Severity of a seal log line, kept separate from emission so the
/// outcome->severity mapping is unit-testable without capturing stderr.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SealLogLevel {
    Info,
    Debug,
    Warn,
}

/// Pure outcome->log mapping for the best-effort boot seal. Steady-state
/// no-ops are `Debug` (kept out of the journal); the inert-protection outcomes
/// (`Unsupported`/`MountStateUnknown`/`NotADirectory`) and any `Err` are `Warn`
/// so a mountpoint left unprotected is loud, not invisible.
pub(crate) fn seal_log(
    outcome: &Result<GuardOutcome, GuardError>,
    path: &Path,
) -> (SealLogLevel, String) {
    let p = path.display();
    match outcome {
        Ok(GuardOutcome::Set) => (
            SealLogLevel::Info,
            format!("sealed offline mountpoint {p} (immutable while unmounted)"),
        ),
        Ok(GuardOutcome::Cleared) => (
            SealLogLevel::Info,
            format!("cleared immutable attribute on {p}"),
        ),
        Ok(GuardOutcome::AlreadyImmutable) => (
            SealLogLevel::Debug,
            format!("{p} already immutable -- no change"),
        ),
        Ok(GuardOutcome::AlreadyMutable) => (
            SealLogLevel::Debug,
            format!("{p} already mutable -- no change"),
        ),
        Ok(GuardOutcome::SkippedMounted) => (
            SealLogLevel::Debug,
            format!("{p} is a live mount point -- not touching flags"),
        ),
        Ok(GuardOutcome::Absent) => (
            SealLogLevel::Debug,
            format!("{p} does not exist -- nothing to seal"),
        ),
        Ok(GuardOutcome::NotADirectory) => (
            SealLogLevel::Warn,
            format!("{p} is not a directory -- refusing to set immutable; check braid.mountPoint"),
        ),
        Ok(GuardOutcome::Unsupported) => (
            SealLogLevel::Warn,
            format!(
                "root filesystem does not support the immutable attribute -- unmounted-mountpoint protection unavailable for {p}"
            ),
        ),
        Ok(GuardOutcome::MountStateUnknown) => (
            SealLogLevel::Warn,
            format!(
                "cannot determine whether {p} is a mount point (kernel lacks STATX_ATTR_MOUNT_ROOT) -- unmounted-mountpoint protection unavailable; no attribute change made"
            ),
        ),
        Err(e) => (
            SealLogLevel::Warn,
            format!("failed to seal mountpoint {p}: {e}"),
        ),
    }
}

/// Best-effort enforcement for the BARE boot/configured-path form of
/// `seal-mountpoint` (the sole automatic seal site). Never fails the caller and
/// always requests `want_immutable = true` -- the seal is non-configurable, and
/// a missing/inert guard must not block boot. The explicit-path forms call
/// `run_explicit_seal` / `run_explicit_unseal` instead so an operator
/// remediation surfaces a failed seal/clear rather than swallowing it.
pub fn seal_offline_mountpoint(path: &Path, guard: &dyn MountpointGuard) {
    let outcome = guard.enforce(path, true);
    let (level, message) = seal_log(&outcome, path);
    match level {
        SealLogLevel::Warn => eprintln!("braid: WARNING: {message}"),
        SealLogLevel::Info => eprintln!("braid: {message}"),
        // Steady-state boot no-ops: keep the journal quiet.
        SealLogLevel::Debug => {}
    }
}

/// Explicit-path `seal-mountpoint <path>` form (an operator remediation for
/// separate-path subvolume mountpoints the boot seal does not cover). Reports an
/// HONEST desired-state result: `Ok` iff the path ends up immutable, so a manual
/// seal that silently failed to protect a path is visible rather than a green
/// no-op. `main.rs` maps `Ok`->exit 0, `Err`->non-zero.
pub fn run_explicit_seal(guard: &dyn MountpointGuard, path: &Path) -> Result<String, String> {
    let outcome = guard.enforce(path, true);
    match &outcome {
        Ok(GuardOutcome::Set) => Ok(format!("sealed {} (immutable)", path.display())),
        Ok(GuardOutcome::AlreadyImmutable) => {
            Ok(format!("{} is already immutable", path.display()))
        }
        _ => Err(explicit_failure_message(&outcome, path, true)),
    }
}

/// Explicit-path `seal-mountpoint --unseal <path>` form -- the lever for the
/// reconfiguration / separate-path-cleanup caveats. REFUSES the currently
/// configured `mount_point` (clearing it just reopens the bug until the next
/// activation re-seals). Reports an honest desired-state result: `Ok` iff the
/// path ends up mutable (`Cleared` OR `AlreadyMutable`, so a repeat unseal of an
/// orphan reports success). `main.rs` maps `Ok`->exit 0, `Err`->non-zero.
pub fn run_explicit_unseal(
    guard: &dyn MountpointGuard,
    path: &Path,
    configured_mount_point: &Path,
) -> Result<String, String> {
    if paths_refer_to_same(path, configured_mount_point) {
        return Err(format!(
            "refusing to unseal the configured mount point {} -- it must stay immutable while the pool is offline. \
             Change braid.mountPoint first, then unseal the old path.",
            path.display()
        ));
    }
    let outcome = guard.enforce(path, false);
    match &outcome {
        Ok(GuardOutcome::Cleared) => {
            Ok(format!("cleared immutable attribute on {}", path.display()))
        }
        Ok(GuardOutcome::AlreadyMutable) => Ok(format!("{} is already mutable", path.display())),
        _ => Err(explicit_failure_message(&outcome, path, false)),
    }
}

/// Operator-facing message for an explicit seal/unseal that did not reach its
/// desired state. Shared by both forms so the wording stays symmetric.
fn explicit_failure_message(
    outcome: &Result<GuardOutcome, GuardError>,
    path: &Path,
    want_immutable: bool,
) -> String {
    let action = if want_immutable { "seal" } else { "unseal" };
    let p = path.display();
    match outcome {
        Ok(GuardOutcome::SkippedMounted) => {
            format!("cannot {action} {p} -- it is a live mount point")
        }
        Ok(GuardOutcome::Absent) => format!("cannot {action} {p} -- path does not exist"),
        Ok(GuardOutcome::NotADirectory) => format!("cannot {action} {p} -- not a directory"),
        Ok(GuardOutcome::Unsupported) => {
            format!("cannot {action} {p} -- filesystem does not support the immutable attribute")
        }
        Ok(GuardOutcome::MountStateUnknown) => format!(
            "cannot {action} {p} -- kernel cannot report mount state (no STATX_ATTR_MOUNT_ROOT)"
        ),
        // Success variants are handled by the callers; keep this exhaustive.
        Ok(
            GuardOutcome::Set
            | GuardOutcome::Cleared
            | GuardOutcome::AlreadyImmutable
            | GuardOutcome::AlreadyMutable,
        ) => format!("unexpected outcome while trying to {action} {p}"),
        Err(e) => format!("failed to {action} {p}: {e}"),
    }
}

/// True when two paths refer to the same directory. Exact comparison first
/// (handles trailing slashes via `Path` component equality), then canonicalized
/// comparison so a symlinked alias to the configured mount point is still
/// refused by `--unseal`.
fn paths_refer_to_same(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    matches!(
        (a.canonicalize(), b.canonicalize()),
        (Ok(ca), Ok(cb)) if ca == cb
    )
}

/// Test double recording every `enforce` call so dispatch tests can assert the
/// supplied path and `want_immutable`, and returning configured outcomes.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::*;
    use std::cell::RefCell;
    use std::path::PathBuf;

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(crate) struct EnforceCall {
        pub path: PathBuf,
        pub want_immutable: bool,
    }

    pub(crate) struct MockMountpointGuard {
        enforce_outcome: Result<GuardOutcome, GuardError>,
        is_immutable_outcome: Result<bool, GuardError>,
        calls: RefCell<Vec<EnforceCall>>,
    }

    impl MockMountpointGuard {
        pub(crate) fn new(enforce_outcome: Result<GuardOutcome, GuardError>) -> Self {
            Self {
                enforce_outcome,
                // The dispatch tests exercise only `enforce`; the trait's
                // `is_immutable` is covered through the real guard and the pure
                // doctor classifier, so the mock returns a fixed read.
                is_immutable_outcome: Ok(false),
                calls: RefCell::new(Vec::new()),
            }
        }

        pub(crate) fn calls(&self) -> Vec<EnforceCall> {
            self.calls.borrow().clone()
        }
    }

    impl MountpointGuard for MockMountpointGuard {
        fn enforce(&self, path: &Path, want_immutable: bool) -> Result<GuardOutcome, GuardError> {
            self.calls.borrow_mut().push(EnforceCall {
                path: path.to_path_buf(),
                want_immutable,
            });
            self.enforce_outcome.clone()
        }

        fn is_immutable(&self, _path: &Path) -> Result<bool, GuardError> {
            self.is_immutable_outcome.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::MockMountpointGuard;
    use super::*;
    use std::path::PathBuf;

    // Intent: the best-effort boot seal always requests immutability, never the
    //   clear, regardless of how it is wired.
    // Why it exists: the seal is a non-configurable safety invariant; a wiring
    //   bug that passed want_immutable=false would silently disable protection.
    // Scenario: braid-seal-mountpoint.service runs `braid seal-mountpoint` at boot.
    #[test]
    fn seal_offline_mountpoint_always_requests_immutability() {
        let guard = MockMountpointGuard::new(Ok(GuardOutcome::Set));
        seal_offline_mountpoint(Path::new("/mnt/storage"), &guard);
        let calls = guard.calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].path, PathBuf::from("/mnt/storage"));
        assert!(
            calls[0].want_immutable,
            "boot seal must request +i, not clear"
        );
    }

    // Intent: every GuardOutcome (and a guard error) maps to a log line without
    //   panicking, and the inert-protection cases warn rather than skip.
    // Why it exists: an unsupported root fs or an old kernel must not silently
    //   disable protection -- the operator needs a loud signal.
    // Scenario: the boot seal encounters each possible enforce result.
    #[test]
    fn seal_log_maps_every_outcome_and_warns_on_inert_protection() {
        let p = Path::new("/mnt/storage");
        let info = [GuardOutcome::Set, GuardOutcome::Cleared];
        for o in info {
            assert_eq!(seal_log(&Ok(o), p).0, SealLogLevel::Info, "{o:?}");
        }
        let debug = [
            GuardOutcome::AlreadyImmutable,
            GuardOutcome::AlreadyMutable,
            GuardOutcome::SkippedMounted,
            GuardOutcome::Absent,
        ];
        for o in debug {
            assert_eq!(seal_log(&Ok(o), p).0, SealLogLevel::Debug, "{o:?}");
        }
        // The inert-protection outcomes must WARN, not silently skip.
        let warn = [
            GuardOutcome::NotADirectory,
            GuardOutcome::Unsupported,
            GuardOutcome::MountStateUnknown,
        ];
        for o in warn {
            assert_eq!(seal_log(&Ok(o), p).0, SealLogLevel::Warn, "{o:?}");
        }
        let err: Result<GuardOutcome, GuardError> = Err(GuardError::SetFlags {
            path: "/mnt/storage".into(),
            errno: Errno::EPERM,
        });
        assert_eq!(seal_log(&err, p).0, SealLogLevel::Warn);

        // And the best-effort wrapper never panics for any outcome.
        for o in info.into_iter().chain(debug).chain(warn) {
            seal_offline_mountpoint(p, &MockMountpointGuard::new(Ok(o)));
        }
        seal_offline_mountpoint(p, &MockMountpointGuard::new(err));
    }

    // Intent: explicit `seal-mountpoint <path>` reports honest desired-state
    //   exit semantics -- success iff the path ends up immutable.
    // Why it exists: this lever protects separate-path subvolume mountpoints the
    //   doctor cannot see; a silent exit 0 would hide an unprotected path (F2).
    // Scenario: an operator seals /var/lib/jellyfin/media manually.
    #[test]
    fn run_explicit_seal_succeeds_only_when_immutable() {
        let path = Path::new("/var/lib/jellyfin/media");
        for ok in [GuardOutcome::Set, GuardOutcome::AlreadyImmutable] {
            let guard = MockMountpointGuard::new(Ok(ok));
            assert!(run_explicit_seal(&guard, path).is_ok(), "{ok:?}");
            assert!(guard.calls()[0].want_immutable);
        }
        for bad in [
            GuardOutcome::SkippedMounted,
            GuardOutcome::Absent,
            GuardOutcome::Unsupported,
            GuardOutcome::MountStateUnknown,
            GuardOutcome::NotADirectory,
        ] {
            let guard = MockMountpointGuard::new(Ok(bad));
            assert!(run_explicit_seal(&guard, path).is_err(), "{bad:?}");
        }
        let guard = MockMountpointGuard::new(Err(GuardError::Unsupported));
        assert!(run_explicit_seal(&guard, path).is_err());
    }

    // Intent: explicit `--unseal <path>` clears (never seals), succeeds iff the
    //   path ends up mutable, and treats an already-mutable path as success.
    // Why it exists: a repeat unseal of a cleared orphan must report success,
    //   not failure (F2); a skipped/absent/unsupported clear must surface.
    // Scenario: an operator clears an orphaned old mountpoint after a reconfig.
    #[test]
    fn run_explicit_unseal_succeeds_only_when_mutable() {
        let path = Path::new("/mnt/orphan");
        let configured = Path::new("/mnt/storage");
        for ok in [GuardOutcome::Cleared, GuardOutcome::AlreadyMutable] {
            let guard = MockMountpointGuard::new(Ok(ok));
            assert!(
                run_explicit_unseal(&guard, path, configured).is_ok(),
                "{ok:?}"
            );
            assert!(
                !guard.calls()[0].want_immutable,
                "unseal must clear, not seal"
            );
        }
        for bad in [
            GuardOutcome::SkippedMounted,
            GuardOutcome::Absent,
            GuardOutcome::Unsupported,
            GuardOutcome::MountStateUnknown,
            GuardOutcome::NotADirectory,
        ] {
            let guard = MockMountpointGuard::new(Ok(bad));
            assert!(
                run_explicit_unseal(&guard, path, configured).is_err(),
                "{bad:?}"
            );
        }
    }

    // Intent: `--unseal` refuses the currently configured mount point without
    //   ever calling enforce.
    // Why it exists: clearing the live configured path reopens the data-safety
    //   bug until the next activation re-seals (F4).
    // Scenario: an operator runs `braid seal-mountpoint --unseal /mnt/storage`.
    #[test]
    fn run_explicit_unseal_refuses_configured_mount_point() {
        let configured = Path::new("/mnt/storage");
        let guard = MockMountpointGuard::new(Ok(GuardOutcome::Cleared));
        let result = run_explicit_unseal(&guard, configured, configured);
        assert!(result.is_err());
        assert!(
            guard.calls().is_empty(),
            "refuse must short-circuit before enforce"
        );
    }

    // Intent: a trailing-slash spelling of the configured mount point is still
    //   refused by `--unseal`.
    // Why it exists: the refuse-configured guard must not be bypassable by a
    //   cosmetic path variation.
    // Scenario: `braid seal-mountpoint --unseal /mnt/storage/`.
    #[test]
    fn run_explicit_unseal_refuses_configured_mount_point_trailing_slash() {
        let guard = MockMountpointGuard::new(Ok(GuardOutcome::Cleared));
        let result = run_explicit_unseal(
            &guard,
            Path::new("/mnt/storage/"),
            Path::new("/mnt/storage"),
        );
        assert!(result.is_err());
        assert!(guard.calls().is_empty());
    }

    // Intent: on a real Linux fd, `enforce` refuses a non-directory path with
    //   NotADirectory and sets no flags.
    // Why it exists: O_DIRECTORY must reject a typo'd `braid.mountPoint` that
    //   points at a regular file rather than sealing it.
    // Scenario: braid.mountPoint accidentally names a file.
    #[cfg(target_os = "linux")]
    #[test]
    fn enforce_refuses_non_directory() {
        let file = tempfile::NamedTempFile::new().expect("temp file");
        let outcome = RealMountpointGuard
            .enforce(file.path(), true)
            .expect("non-directory is a clean outcome, not an error");
        assert_eq!(outcome, GuardOutcome::NotADirectory);
    }

    // Intent: the FS_IOC_{GET,SET}FLAGS request numbers derived from FsFlagsArg
    //   match the kernel's LP64 _IOR('f',1,long) / _IOW('f',2,long) constants.
    // Why it exists: a regression flipping FsFlagsArg to c_int would encode
    //   0x8004_6601 / 0x4004_6602, the switch would never match, and protection
    //   would silently go inert -- this fails in just test-rust, unlike the mock
    //   tests which bypass the real request number.
    // Scenario: the Linux test lane builds the ioctl wrappers.
    #[cfg(target_os = "linux")]
    #[test]
    fn fs_ioc_flag_request_numbers_match_lp64_kernel_abi() {
        let getflags = nix::request_code_read!(b'f', 1, std::mem::size_of::<FsFlagsArg>());
        let setflags = nix::request_code_write!(b'f', 2, std::mem::size_of::<FsFlagsArg>());
        assert_eq!(getflags, 0x8008_6601, "FS_IOC_GETFLAGS request number");
        assert_eq!(setflags, 0x4008_6602, "FS_IOC_SETFLAGS request number");
    }

    // Intent: an `#[ignore]`d smoke test for the real ioctl round-trip and the
    //   bind-mount mount-root predicate, runnable by maintainers as root.
    // Why it exists: unit tests cover the abstract half; this proves the running
    //   kernel accepts the request number and that STATX_ATTR_MOUNT_ROOT refuses
    //   a same-device bind mount an st_dev check would miss.
    // Scenario: a maintainer runs `cargo test -- --ignored` as root.
    #[cfg(target_os = "linux")]
    #[ignore = "requires root and mount/chattr privileges"]
    #[test]
    fn enforce_round_trip_and_bind_mount_smoke() {
        let dir = tempfile::tempdir().expect("temp dir");
        let guard = RealMountpointGuard;

        // Set then clear on a plain offline directory.
        assert_eq!(guard.enforce(dir.path(), true).unwrap(), GuardOutcome::Set);
        assert_eq!(guard.is_immutable(dir.path()).unwrap(), true);
        assert_eq!(
            guard.enforce(dir.path(), true).unwrap(),
            GuardOutcome::AlreadyImmutable
        );
        assert_eq!(
            guard.enforce(dir.path(), false).unwrap(),
            GuardOutcome::Cleared
        );
        assert_eq!(guard.is_immutable(dir.path()).unwrap(), false);

        // A same-device bind mount must be detected as a mount root.
        let src = tempfile::tempdir().expect("bind source");
        let mount_status = std::process::Command::new("mount")
            .args([
                "--bind",
                &src.path().display().to_string(),
                &dir.path().display().to_string(),
            ])
            .status()
            .expect("mount --bind");
        assert!(mount_status.success(), "bind mount failed");
        let outcome = guard.enforce(dir.path(), true).unwrap();
        let _ = std::process::Command::new("umount")
            .arg(dir.path())
            .status();
        assert_eq!(
            outcome,
            GuardOutcome::SkippedMounted,
            "bind mount root must not be sealed"
        );
    }

    // Intent: on non-Linux hosts the real guard degrades to a clean no-finding
    //   (never a false Warn/Failure, never a compile error).
    // Why it exists: just test-rust builds on aarch64-darwin; the stub must link
    //   and its is_immutable Err must travel to the doctor as Indeterminate.
    // Scenario: the macOS test lane builds RealMountpointGuard.
    #[cfg(not(target_os = "linux"))]
    #[test]
    fn non_linux_stub_degrades_to_no_finding() {
        use crate::doctor::{
            ImmutabilityProbe, ImmutableFinding, classify_mountpoint_immutability,
        };

        let guard = RealMountpointGuard;
        assert_eq!(
            guard.enforce(Path::new("/mnt/storage"), true).unwrap(),
            GuardOutcome::Unsupported
        );
        assert!(matches!(
            guard.is_immutable(Path::new("/mnt/storage")),
            Err(GuardError::Unsupported)
        ));
        let probe = ImmutabilityProbe::from_result(guard.is_immutable(Path::new("/mnt/storage")));
        assert_eq!(probe, ImmutabilityProbe::Indeterminate);
        assert_eq!(
            classify_mountpoint_immutability("/mnt/storage", Some(false), probe),
            ImmutableFinding::None
        );
    }
}
