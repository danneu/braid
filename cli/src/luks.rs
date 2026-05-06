use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
use crate::config::mapper_name;
use crate::parse::{
    ParseError, cryptsetup_luks_uuid_reports_not_luks, parse_cryptsetup_luks_uuid,
    parse_cryptsetup_status,
};
use crate::state_paths::StatePaths;
use crate::types::{ByIdPath, LuksUuid, MapperName, PoolDevice};
use std::io::{BufRead, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::PathBuf;
use zeroize::Zeroizing;

/// LUKS key slot 1: binary random keyfile (no PBKDF, raw key material).
pub const LUKS_SLOT_KEYFILE: u8 = 1;

/// Canonical keyfile filename, hardcoded to match the NixOS auto-unlock module.
pub const KEYFILE_NAME: &str = "braid.key";
/// Keyfile size in bytes: 4096 bytes of random data from /dev/urandom.
pub const KEYFILE_SIZE: usize = 4096;

/// State of a LUKS key slot, as reported by `cryptsetup luksDump`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeySlotState {
    Empty,
    Occupied,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyfileEnrollmentProbe {
    pub has_enrollment: bool,
    pub failures: Vec<KeyfileEnrollmentProbeFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyfileEnrollmentProbeFailure {
    pub device: String,
    pub error: String,
}

#[derive(Debug, thiserror::Error)]
pub enum LuksError {
    #[error("{0}")]
    Validation(String),
    #[error("cryptsetup open failed for {device} (exit {exit_code}): {hint} -- {stderr}")]
    OpenFailed {
        device: String,
        exit_code: i32,
        hint: &'static str,
        stderr: String,
    },
    #[error("cryptsetup luksFormat failed for {device} (exit {exit_code}): {hint} -- {stderr}")]
    FormatFailed {
        device: String,
        exit_code: i32,
        hint: &'static str,
        stderr: String,
    },
    #[error(
        "disk '{name}' mapper '/dev/mapper/braid-{name}' is open but not \
         backed by the configured disk. Expected LUKS UUID {expected}, \
         found {}. Close the conflicting mapper with \
         'sudo cryptsetup close braid-{name}' and re-run.",
        luks_found_display(found)
    )]
    MapperConflict {
        name: String,
        expected: LuksUuid,
        found: Option<LuksUuid>,
    },
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

fn luks_found_display(found: &Option<LuksUuid>) -> String {
    match found {
        Some(uuid) => uuid.to_string(),
        None => "no backing (stale mapper)".to_owned(),
    }
}

/// Abstraction over the TTY passphrase read. Production uses
/// [`RealTty`] (termios-backed, echo-suppressed). Tests inject a
/// scripted reader so `cmd_add` can be exercised without a PTY.
pub trait PassphraseReader {
    /// Read a passphrase from the terminal with the given prompt label,
    /// suppressing echo. Returns the validated (non-empty, no embedded
    /// line-break characters) passphrase.
    fn read_tty(&self, label: &str) -> Result<String, LuksError>;
}

/// Production TTY reader backed by /dev/tty and libc termios.
pub struct RealTty;

impl PassphraseReader for RealTty {
    fn read_tty(&self, label: &str) -> Result<String, LuksError> {
        let mut tty = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .map_err(|_| {
                LuksError::Validation(
                    "no controlling terminal -- pass --passphrase-stdin or \
                     --passphrase-file for non-interactive use"
                        .into(),
                )
            })?;
        read_tty_from_file(&mut tty, label)
    }
}

#[doc(hidden)]
pub fn read_tty_from_file(tty: &mut std::fs::File, label: &str) -> Result<String, LuksError> {
    let fd = tty.as_raw_fd();
    let orig = tcgetattr(fd)?;
    let mut modified = orig;
    modified.c_lflag &= !libc::ECHO;
    modified.c_lflag |= libc::ECHONL;

    let _guard = TermiosGuard::install(fd, modified, orig)?;
    tty.write_all(label.as_bytes())?;
    tty.flush()?;

    let mut raw: Zeroizing<String> = Zeroizing::new(String::new());
    std::io::BufReader::new(&mut *tty).read_line(&mut *raw)?;
    validate_passphrase(&raw, "terminal")
}

fn tcgetattr(fd: RawFd) -> std::io::Result<libc::termios> {
    let mut termios = std::mem::MaybeUninit::<libc::termios>::zeroed();
    let rc = unsafe { libc::tcgetattr(fd, termios.as_mut_ptr()) };
    if rc == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { termios.assume_init() })
    }
}

fn tcsetattr_now(fd: RawFd, termios: &libc::termios) -> std::io::Result<()> {
    let rc = unsafe { libc::tcsetattr(fd, libc::TCSANOW, termios) };
    if rc == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

// Restores only on normal returns and Rust unwinds; process signals skip Drop.
struct TermiosGuard {
    fd: RawFd,
    orig: libc::termios,
}

impl TermiosGuard {
    fn install(
        fd: RawFd,
        modified: libc::termios,
        orig: libc::termios,
    ) -> std::io::Result<TermiosGuard> {
        tcsetattr_now(fd, &modified)?;
        Ok(TermiosGuard { fd, orig })
    }
}

impl Drop for TermiosGuard {
    fn drop(&mut self) {
        let _ = tcsetattr_now(self.fd, &self.orig);
    }
}

/// Test-only scripted passphrase reader. Pops from an internal queue
/// so tests can observe read-count without a PTY. Shared between
/// luks.rs unit tests and add.rs `cmd_add` regression tests.
#[cfg(test)]
pub(crate) struct ScriptedPassphraseReader {
    queue: std::cell::RefCell<std::collections::VecDeque<String>>,
}

#[cfg(test)]
impl ScriptedPassphraseReader {
    pub(crate) fn new<I, S>(items: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            queue: std::cell::RefCell::new(items.into_iter().map(Into::into).collect()),
        }
    }
    pub(crate) fn remaining(&self) -> usize {
        self.queue.borrow().len()
    }
}

#[cfg(test)]
impl PassphraseReader for ScriptedPassphraseReader {
    fn read_tty(&self, _label: &str) -> Result<String, LuksError> {
        match self.queue.borrow_mut().pop_front() {
            Some(s) => Ok(s),
            None => Err(LuksError::Validation(
                "ScriptedPassphraseReader: queue exhausted".into(),
            )),
        }
    }
}

/// Existing API -- read a passphrase without new-format confirmation.
/// Used by `replace`, `enroll-key-file`, and `mount::resolve_credential`
/// where a typo is caught by a live verification target.
pub fn read_passphrase(
    passphrase_file: Option<&std::path::Path>,
    passphrase_stdin: bool,
) -> Result<String, LuksError> {
    read_passphrase_with(passphrase_file, passphrase_stdin, false, &RealTty)
}

/// Read a passphrase, optionally confirming on the TTY when the caller
/// is about to `luks_format` without a live keyslot to verify against.
///
/// - File / stdin inputs ignore `confirm_new` and `tty` (single read).
/// - TTY input reads once; if `confirm_new`, prompts again and requires
///   a byte-exact match.
///
/// Thin wrapper -- only locks process stdin for the stdin branch.
pub fn read_passphrase_with(
    passphrase_file: Option<&std::path::Path>,
    passphrase_stdin: bool,
    confirm_new: bool,
    tty: &dyn PassphraseReader,
) -> Result<String, LuksError> {
    if passphrase_file.is_none() && passphrase_stdin {
        let mut stdin = std::io::stdin().lock();
        return read_passphrase_with_readers(
            passphrase_file,
            passphrase_stdin,
            confirm_new,
            &mut stdin,
            tty,
        );
    }
    let mut unused_stdin = std::io::Cursor::new(&[][..]);
    read_passphrase_with_readers(
        passphrase_file,
        passphrase_stdin,
        confirm_new,
        &mut unused_stdin,
        tty,
    )
}

/// Full form used by production (via `read_passphrase_with`) and by
/// tests (with a `Cursor` for stdin and a scripted `PassphraseReader`).
fn read_passphrase_with_readers(
    passphrase_file: Option<&std::path::Path>,
    passphrase_stdin: bool,
    confirm_new: bool,
    stdin: &mut dyn BufRead,
    tty: &dyn PassphraseReader,
) -> Result<String, LuksError> {
    if let Some(path) = passphrase_file {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            LuksError::Validation(format!(
                "failed to read passphrase file {}: {e}",
                path.display()
            ))
        })?;
        return validate_passphrase(&contents, &format!("file {}", path.display()));
    }
    if passphrase_stdin {
        return read_passphrase_stdin_from(stdin);
    }
    let first = tty.read_tty("LUKS passphrase: ")?;
    if !confirm_new {
        return Ok(first);
    }
    let second = tty.read_tty("Confirm LUKS passphrase: ")?;
    check_passphrase_match(first, second)
}

/// Normalize and validate a raw passphrase string.
/// Strips trailing line-break characters, then rejects empty values
/// and embedded line-breaks.
fn validate_passphrase(raw: &str, source: &str) -> Result<String, LuksError> {
    let passphrase = raw.trim_end_matches(['\n', '\r']).to_owned();
    if passphrase.is_empty() {
        return Err(LuksError::Validation(format!(
            "passphrase from {source} must not be empty"
        )));
    }
    if passphrase.contains('\n') || passphrase.contains('\r') {
        return Err(LuksError::Validation(format!(
            "passphrase from {source} contains line-break characters -- \
             this passphrase would be impossible to enter interactively"
        )));
    }
    Ok(passphrase)
}

/// Read a single line from the given reader and validate it as a
/// passphrase. Parallels `confirm_yes_from` -- the production caller
/// locks stdin and passes it in; tests pass `Cursor`.
fn read_passphrase_stdin_from(r: &mut dyn BufRead) -> Result<String, LuksError> {
    let mut buf = String::new();
    r.read_line(&mut buf)?;
    validate_passphrase(&buf, "stdin")
}

/// Require two passphrase reads to match byte-for-byte. Returns the
/// passphrase on match, Validation error on mismatch. Used to catch
/// typos on the fresh-format path where the typoed passphrase would
/// otherwise become the canonical pool passphrase.
fn check_passphrase_match(first: String, second: String) -> Result<String, LuksError> {
    if first == second {
        Ok(first)
    } else {
        Err(LuksError::Validation(
            "passphrases do not match -- aborting".to_owned(),
        ))
    }
}

/// LUKS format a device with the given passphrase.
pub fn luks_format<R: CommandRunner>(
    runner: &R,
    device: &str,
    passphrase: &str,
    extra_opts: &[String],
) -> Result<(), LuksError> {
    let result = runner.run_with_stdin(
        &CmdRequest::CryptsetupLuksFormat {
            device: device.to_owned(),
            extra_opts: extra_opts.to_vec(),
        },
        passphrase.as_bytes(),
    )?;
    if result.exit_status != 0 {
        return Err(LuksError::FormatFailed {
            device: device.to_owned(),
            exit_code: result.exit_status,
            hint: cryptsetup_format_hint(result.exit_status),
            stderr: result.stderr.trim().to_owned(),
        });
    }
    Ok(())
}

/// Back up the LUKS header to `dir/<mapper>.luksheader`.
/// Extracted so tests can pass a tempdir instead of the real path.
pub(crate) fn backup_luks_header_to<R: CommandRunner>(
    runner: &R,
    device: &str,
    mapper: &str,
    dir: &std::path::Path,
) -> Result<PathBuf, LuksError> {
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }

    let backup_path = dir.join(format!("{mapper}.luksheader"));
    let tmp_path = dir.join(format!("{mapper}.luksheader.tmp"));
    // Write to temp file, then atomic rename so we never lose an existing backup
    if tmp_path.exists() {
        std::fs::remove_file(&tmp_path)?;
    }
    let result = runner.run(&CmdRequest::CryptsetupLuksHeaderBackup {
        device: device.to_owned(),
        backup_path: tmp_path.display().to_string(),
    })?;
    if result.exit_status != 0 {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(LuksError::Validation(format!(
            "LUKS header backup failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim()
        )));
    }
    // cryptsetup already creates the file as 0400, but enforce it ourselves for defense-in-depth
    std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o400))?;
    crate::state_io::durable_rename(&tmp_path, &backup_path)?;

    Ok(backup_path)
}

/// Back up the LUKS header to <state_root>/luks-headers/<mapper>.luksheader
pub fn backup_luks_header<R: CommandRunner>(
    runner: &R,
    device: &str,
    mapper: &str,
    paths: &StatePaths,
) -> Result<PathBuf, LuksError> {
    backup_luks_header_to(runner, device, mapper, &paths.luks_headers_dir())
}

/// Outcome of a keyslot verification attempt. Exit 2 (EPERM) is the only
/// cryptsetup exit that semantically means "wrong credential" (see
/// `translate_errno` in `reference/cryptsetup/src/utils_tools.c`). Every
/// other non-zero exit is a real error -- busy, missing device, out of
/// memory, generic EINVAL -- and must not be silently treated as rejection
/// by callers, or the user gets a misleading "wrong passphrase" narrative
/// for an unrelated failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyOutcome {
    Authenticated,
    Rejected,
}

/// Classify a cryptsetup `--test-passphrase` (or `--test-passphrase
/// --key-file`) exit into `VerifyOutcome` or `LuksError::OpenFailed`.
/// Single source of truth for the verify-exit mapping so tests bind to
/// the real helper.
fn classify_verify_exit(
    device: &str,
    result: &RawCommandOutput,
) -> Result<VerifyOutcome, LuksError> {
    match result.exit_status {
        0 => Ok(VerifyOutcome::Authenticated),
        2 => Ok(VerifyOutcome::Rejected),
        code => Err(LuksError::OpenFailed {
            device: device.to_owned(),
            exit_code: code,
            hint: cryptsetup_open_hint(code),
            stderr: result.stderr.trim().to_owned(),
        }),
    }
}

/// Verify passphrase against an existing LUKS device (test-passphrase).
pub fn verify_passphrase<R: CommandRunner>(
    runner: &R,
    device: &str,
    passphrase: &str,
) -> Result<VerifyOutcome, LuksError> {
    let result = runner.run_with_stdin(
        &CmdRequest::CryptsetupTestPassphrase {
            device: device.to_owned(),
        },
        passphrase.as_bytes(),
    )?;
    classify_verify_exit(device, &result)
}

/// Semantic classification of cryptsetup exit codes.
/// Single source of truth — maps cryptsetup's `translate_errno` (utils_tools.c).
enum CryptsetupExitKind {
    GenericFailure,   // exit 1: EINVAL / ENOENT / ENOSYS / default
    PermissionDenied, // exit 2: EPERM
    OutOfMemory,      // exit 3: ENOMEM
    DeviceNotFound,   // exit 4: ENOTBLK / ENODEV
    DeviceBusy,       // exit 5: EEXIST / EBUSY
    Unknown,
}

fn classify_cryptsetup_exit(code: i32) -> CryptsetupExitKind {
    match code {
        1 => CryptsetupExitKind::GenericFailure,
        2 => CryptsetupExitKind::PermissionDenied,
        3 => CryptsetupExitKind::OutOfMemory,
        4 => CryptsetupExitKind::DeviceNotFound,
        5 => CryptsetupExitKind::DeviceBusy,
        _ => CryptsetupExitKind::Unknown,
    }
}

fn cryptsetup_open_hint(exit_code: i32) -> &'static str {
    match classify_cryptsetup_exit(exit_code) {
        CryptsetupExitKind::GenericFailure => "generic failure",
        CryptsetupExitKind::PermissionDenied => "wrong passphrase or permission denied",
        CryptsetupExitKind::OutOfMemory => "out of memory",
        CryptsetupExitKind::DeviceNotFound => "device not found or not a block device",
        CryptsetupExitKind::DeviceBusy => "device is already open or busy",
        CryptsetupExitKind::Unknown => "unknown error",
    }
}

fn cryptsetup_format_hint(exit_code: i32) -> &'static str {
    match classify_cryptsetup_exit(exit_code) {
        CryptsetupExitKind::GenericFailure => "generic failure",
        CryptsetupExitKind::PermissionDenied => "permission denied (not root?)",
        CryptsetupExitKind::OutOfMemory => "out of memory",
        CryptsetupExitKind::DeviceNotFound => "device not found or not a block device",
        CryptsetupExitKind::DeviceBusy => "device busy or already formatted",
        CryptsetupExitKind::Unknown => "unknown error",
    }
}

// ---------------------------------------------------------------------------
// LUKS header probe + remediation guidance
// ---------------------------------------------------------------------------

/// Outcome of probing a LUKS device's on-disk header. Used by both
/// `braid doctor` (for declared-disk health checks) and `braid unlock`
/// (for enriching open-failure errors with the real cause).
///
/// Terminology contract:
/// - `Unreadable` means braid cannot read or recognize a LUKS header at all.
/// - `Damaged` means braid recognized LUKS, but the header metadata is broken.
#[derive(Debug, Clone)]
pub(crate) enum LuksHeaderState {
    /// Both `isLuks` and `luksDump` succeeded; the header is intact.
    Ok,
    /// `isLuks` exited non-zero — the LUKS magic is gone or the header
    /// is otherwise unreadable. Severe.
    Unreadable,
    /// `isLuks` succeeded but `luksDump` exited non-zero — the magic is
    /// intact but LUKS2 metadata is damaged.
    Damaged,
    /// The cryptsetup command failed to execute (missing binary, IPC
    /// failure). NOT the same as cryptsetup finding corruption — callers
    /// must never emit repair/restore suggestions in this state.
    ProbeFailed(String),
}

/// Read-only LUKS header probe. Runs `cryptsetup isLuks` then
/// `cryptsetup luksDump` and classifies the result. Safe to call on a
/// device that is currently open via dm-crypt — the probe reads the
/// raw block device, not the mapper.
pub(crate) fn probe_luks_header<R: CommandRunner>(runner: &R, device: &str) -> LuksHeaderState {
    match runner.run(&CmdRequest::CryptsetupIsLuks {
        device: device.to_owned(),
    }) {
        Err(e) => return LuksHeaderState::ProbeFailed(e.to_string()),
        Ok(raw) if raw.exit_status != 0 => return LuksHeaderState::Unreadable,
        Ok(_) => {}
    }
    match runner.run(&CmdRequest::CryptsetupLuksDumpText {
        device: device.to_owned(),
    }) {
        Err(e) => LuksHeaderState::ProbeFailed(e.to_string()),
        Ok(raw) if raw.exit_status != 0 => LuksHeaderState::Damaged,
        Ok(_) => LuksHeaderState::Ok,
    }
}

/// Guidance text for an unreadable LUKS header. Deliberately generic —
/// never references local `/var/lib/braid/luks-headers/` files. `braid
/// status` and the TUI already warn when local header backups persist
/// on the same machine because the intended product workflow is to
/// export them off-system and remove the local copy; doctor and unlock
/// must not contradict that posture by instructing users to rely on
/// local copies as a safety net.
pub(crate) fn luks_header_unreadable_guidance() -> &'static str {
    "LUKS header unreadable. Restore from your off-system LUKS header \
    backup if you have one (cryptsetup luksHeaderRestore). Without an \
    off-system backup, recovery may be limited or impossible."
}

/// Guidance text for a damaged-metadata LUKS header. Always pairs the
/// `cryptsetup repair` suggestion with an explicit safe-backup warning
/// because repair mutates the header.
pub(crate) fn luks_header_damaged_guidance(device: &str) -> String {
    format!(
        "LUKS header metadata damaged. To attempt repair manually: \
        cryptsetup repair --type luks2 {device} -- make a safe backup of \
        the device header before running repair."
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MapperOwnership {
    Inactive,
    Owned,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpenOutcome {
    Opened,
    AlreadyOwned,
}

fn luks_uuid_for_device<R: CommandRunner>(runner: &R, device: &str) -> Result<LuksUuid, LuksError> {
    let raw = runner.run(&CmdRequest::CryptsetupLuksUuid {
        device: device.to_owned(),
    })?;
    Ok(parse_cryptsetup_luks_uuid(&raw)?.uuid)
}

fn mapper_ownership<R: CommandRunner>(
    runner: &R,
    name: &str,
    by_id: &ByIdPath,
    mapper: &MapperName,
) -> Result<MapperOwnership, LuksError> {
    let status_raw = runner.run(&CmdRequest::CryptsetupStatus {
        mapper: mapper.0.clone(),
    })?;
    let status = parse_cryptsetup_status(&status_raw)?;

    if !status.is_active {
        return Ok(MapperOwnership::Inactive);
    }

    let expected = luks_uuid_for_device(runner, &by_id.0)?;
    let underlying = match status.device.as_deref() {
        None | Some("") | Some("(null)") => {
            return Err(LuksError::MapperConflict {
                name: name.to_owned(),
                expected,
                found: None,
            });
        }
        Some(device) => device,
    };

    let backing_raw = runner.run(&CmdRequest::CryptsetupLuksUuid {
        device: underlying.to_owned(),
    })?;
    let found = match parse_cryptsetup_luks_uuid(&backing_raw) {
        Ok(out) => out.uuid,
        Err(_) if cryptsetup_luks_uuid_reports_not_luks(&backing_raw) => {
            return Err(LuksError::MapperConflict {
                name: name.to_owned(),
                expected,
                found: None,
            });
        }
        Err(e) => return Err(LuksError::Parse(e)),
    };

    if found == expected {
        Ok(MapperOwnership::Owned)
    } else {
        Err(LuksError::MapperConflict {
            name: name.to_owned(),
            expected,
            found: Some(found),
        })
    }
}

/// Open a LUKS device if not already open.
pub fn ensure_luks_open<R: CommandRunner>(
    runner: &R,
    name: &str,
    by_id: &ByIdPath,
    passphrase: &str,
) -> Result<OpenOutcome, LuksError> {
    let mn = mapper_name(name);
    if mapper_ownership(runner, name, by_id, &mn)? == MapperOwnership::Owned {
        return Ok(OpenOutcome::AlreadyOwned);
    }

    let result = runner.run_with_stdin(
        &CmdRequest::CryptsetupLuksOpen {
            device: by_id.0.clone(),
            mapper: mn.0.clone(),
        },
        passphrase.as_bytes(),
    )?;
    if result.exit_status != 0 {
        return Err(LuksError::OpenFailed {
            device: by_id.0.clone(),
            exit_code: result.exit_status,
            hint: cryptsetup_open_hint(result.exit_status),
            stderr: result.stderr.trim().to_owned(),
        });
    }
    Ok(OpenOutcome::Opened)
}

/// Check if a mapper device has a btrfs superblock.
pub fn device_has_btrfs_superblock<R: CommandRunner>(
    runner: &R,
    mapper_path: &str,
) -> Result<bool, LuksError> {
    // Use BtrfsDeviceScan on the specific device — if it succeeds, btrfs recognizes the device.
    // A non-btrfs device or empty device will fail.
    let result = runner.run(&CmdRequest::BtrfsDeviceScan {
        device: mapper_path.to_owned(),
    })?;
    Ok(result.exit_status == 0)
}

/// Open a LUKS device with a binary keyfile (no passphrase, no PBKDF).
pub fn ensure_luks_open_with_key_file<R: CommandRunner>(
    runner: &R,
    name: &str,
    by_id: &ByIdPath,
    key_file_path: &std::path::Path,
) -> Result<OpenOutcome, LuksError> {
    let mn = mapper_name(name);
    if mapper_ownership(runner, name, by_id, &mn)? == MapperOwnership::Owned {
        return Ok(OpenOutcome::AlreadyOwned);
    }

    let result = runner.run(&CmdRequest::CryptsetupLuksOpenKeyFile {
        device: by_id.0.clone(),
        mapper: mn.0.clone(),
        key_file_path: key_file_path.display().to_string(),
    })?;
    if result.exit_status != 0 {
        return Err(LuksError::OpenFailed {
            device: by_id.0.clone(),
            exit_code: result.exit_status,
            hint: cryptsetup_open_hint(result.exit_status),
            stderr: result.stderr.trim().to_owned(),
        });
    }
    Ok(OpenOutcome::Opened)
}

/// Verify a binary keyfile against an existing LUKS device.
pub fn verify_key_file<R: CommandRunner>(
    runner: &R,
    device: &str,
    key_file_path: &std::path::Path,
) -> Result<VerifyOutcome, LuksError> {
    let result = runner.run(&CmdRequest::CryptsetupTestKeyFile {
        device: device.to_owned(),
        key_file_path: key_file_path.display().to_string(),
    })?;
    classify_verify_exit(device, &result)
}

/// Enroll a binary keyfile into LUKS slot 1, authorized by the existing passphrase.
pub fn enroll_key_file<R: CommandRunner>(
    runner: &R,
    device: &str,
    passphrase: &str,
    key_file_path: &std::path::Path,
) -> Result<(), LuksError> {
    let result = runner.run_with_stdin(
        &CmdRequest::CryptsetupLuksAddKeyFile {
            device: device.to_owned(),
            key_file_path: key_file_path.display().to_string(),
        },
        passphrase.as_bytes(),
    )?;
    if result.exit_status != 0 {
        return Err(LuksError::Validation(format!(
            "cryptsetup luksAddKey failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim()
        )));
    }
    Ok(())
}

/// Check the state of a specific LUKS key slot via `cryptsetup luksDump`.
pub fn check_key_slot<R: CommandRunner>(
    runner: &R,
    device: &str,
    slot: u8,
) -> Result<KeySlotState, LuksError> {
    let raw = runner.run(&CmdRequest::CryptsetupLuksDump {
        device: device.to_owned(),
    })?;
    if raw.exit_status != 0 {
        return Err(LuksError::Validation(format!(
            "cryptsetup luksDump failed (exit {}): {}",
            raw.exit_status,
            raw.stderr.trim()
        )));
    }

    // Parse JSON: look for keyslots.<slot>
    let parsed: serde_json::Value = serde_json::from_str(&raw.stdout)
        .map_err(|e| LuksError::Validation(format!("failed to parse luksDump JSON: {e}")))?;

    let slot_key = slot.to_string();
    match parsed.get("keyslots").and_then(|ks| ks.get(&slot_key)) {
        Some(_) => Ok(KeySlotState::Occupied),
        None => Ok(KeySlotState::Empty),
    }
}

/// Best-effort check: does any live pool device have a keyfile in slot 1?
/// Scans devices until an occupied slot 1 is found. Probe failures are
/// returned as structured data so callers own diagnostic routing.
pub fn probe_pool_keyfile_enrollment<R: CommandRunner>(
    runner: &R,
    devices: &[PoolDevice],
) -> KeyfileEnrollmentProbe {
    let mut failures = Vec::new();
    for dev in devices {
        match check_key_slot(runner, &dev.underlying, LUKS_SLOT_KEYFILE) {
            Ok(KeySlotState::Occupied) => {
                return KeyfileEnrollmentProbe {
                    has_enrollment: true,
                    failures,
                };
            }
            Ok(KeySlotState::Empty) => {}
            Err(e) => failures.push(KeyfileEnrollmentProbeFailure {
                device: dev.underlying.clone(),
                error: e.to_string(),
            }),
        }
    }
    KeyfileEnrollmentProbe {
        has_enrollment: false,
        failures,
    }
}

pub fn format_keyfile_enrollment_probe_failure(failure: &KeyfileEnrollmentProbeFailure) -> String {
    format!(
        "could not check keyfile enrollment on {}: {}; proceeding as if no keyfile is enrolled",
        failure.device, failure.error
    )
}

/// Returns the keyfile-asymmetry warning body with no channel-specific prefix.
/// `PreviewNote::Warn` owns the `[warn]` prefix for dry-run stdout and real-run
/// stderr renders.
pub fn format_keyfile_asymmetry_warning() -> String {
    "Existing pool drives have a keyfile (keyslot-1) for auto-unlock, \
     but the new drive will not.\n  \
     Passphrase unlock still works, but the keyfile won't unlock the new drive \
     until it's enrolled.\n  \
     Fix: re-run with --enroll <dir>, or run `braid enroll <dir>` afterward.\n"
        .to_owned()
}

/// Scan `dir` for `.luksheader` or `.img` files and return advisories.
/// Extracted so tests can pass a tempdir instead of the real path.
fn header_backup_advisories_in(dir: &std::path::Path) -> Vec<String> {
    let has_backups = match std::fs::read_dir(dir) {
        Ok(entries) => entries.filter_map(|e| e.ok()).any(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "luksheader" || ext == "img")
        }),
        Err(_) => false,
    };
    if has_backups {
        vec![format!(
            "LUKS header backups exist in {} -- copy offsite and delete local copies",
            dir.display()
        )]
    } else {
        vec![]
    }
}

/// Scan the luks-headers directory for backup files and return advisories.
pub fn header_backup_advisories(paths: &StatePaths) -> Vec<String> {
    header_backup_advisories_in(&paths.luks_headers_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::types::ByIdPath;
    use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};

    fn open_pty_pair() -> (std::fs::File, std::fs::File) {
        let mut master = -1;
        let mut slave = -1;
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(rc, 0, "openpty failed: {}", std::io::Error::last_os_error());
        unsafe {
            (
                std::fs::File::from_raw_fd(master),
                std::fs::File::from_raw_fd(slave),
            )
        }
    }

    fn flip_echo(termios: &mut libc::termios) {
        if termios.c_lflag & libc::ECHO == 0 {
            termios.c_lflag |= libc::ECHO;
        } else {
            termios.c_lflag &= !libc::ECHO;
        }
    }

    fn termios_bytes(termios: &libc::termios) -> &[u8] {
        unsafe {
            std::slice::from_raw_parts(
                (termios as *const libc::termios).cast::<u8>(),
                std::mem::size_of::<libc::termios>(),
            )
        }
    }

    fn assert_termios_eq(expected: &libc::termios, actual: &libc::termios) {
        assert_eq!(termios_bytes(expected), termios_bytes(actual));
    }

    /*
     * Intent: TermiosGuard restores the original terminal attributes on drop.
     * Why it exists: the passphrase reader disables echo, so every normal
     *   return path must put the terminal back before the file descriptor drops.
     * Scenario: read_tty_from_file installs no-echo mode, finishes normally,
     *   and the user gets their shell prompt with echo restored.
     */
    #[test]
    fn termios_guard_restores_on_drop() {
        let (_master, slave) = open_pty_pair();
        let fd = slave.as_raw_fd();
        let before = tcgetattr(fd).unwrap();
        let mut modified = before;
        flip_echo(&mut modified);

        {
            let _guard = TermiosGuard::install(fd, modified, before).unwrap();
            let during = tcgetattr(fd).unwrap();
            assert_termios_eq(&modified, &during);
        }

        let after = tcgetattr(fd).unwrap();
        assert_termios_eq(&before, &after);
    }

    /*
     * Intent: TermiosGuard restores the original terminal attributes on `?`
     *   early return.
     * Why it exists: read_tty_from_file can fail after echo is disabled, and
     *   the error path must not strand the terminal in no-echo mode.
     * Scenario: a prompt write or line read fails after termios installation,
     *   returning through `?` while the guard is still in scope.
     */
    #[test]
    fn termios_guard_restores_on_question_mark_return() {
        fn install_then_fail(fd: RawFd, before: libc::termios) -> std::io::Result<()> {
            let mut modified = before;
            flip_echo(&mut modified);
            let _guard = TermiosGuard::install(fd, modified, before)?;
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "forced early return",
            ))?;
            Ok(())
        }

        let (_master, slave) = open_pty_pair();
        let fd = slave.as_raw_fd();
        let before = tcgetattr(fd).unwrap();

        let err = install_then_fail(fd, before).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::Other);

        let after = tcgetattr(fd).unwrap();
        assert_termios_eq(&before, &after);
    }

    /*
     * Intent: verify the hint helper maps exit 2 to passphrase/permission text.
     * Why: exit 2 is the most common open failure; a wrong mapping misleads users.
     * Scenario: cryptsetup open returns EPERM (wrong passphrase or permission denied).
     */
    #[test]
    fn hint_exit_2_is_passphrase() {
        assert_eq!(
            cryptsetup_open_hint(2),
            "wrong passphrase or permission denied"
        );
    }

    /*
     * Intent: verify the hint helper maps exit 5 to busy/already-open text.
     * Why: exit 5 was previously reported as "Wrong passphrase?" — the original bug.
     * Scenario: cryptsetup open returns EBUSY (device already open).
     */
    #[test]
    fn hint_exit_5_is_busy() {
        assert_eq!(cryptsetup_open_hint(5), "device is already open or busy");
    }

    /*
     * Intent: verify the hint helper returns a fallback for unknown exit codes.
     * Why: upstream may add new exit codes; the helper must not panic.
     * Scenario: future cryptsetup version returns an unmapped exit code.
     */
    #[test]
    fn hint_unknown_code() {
        assert_eq!(cryptsetup_open_hint(42), "unknown error");
    }

    /*
     * Intent: verify the format hint maps exit 2 to permission denied (not passphrase).
     * Why: luksFormat creates a passphrase, it doesn't verify one — exit 2 is purely EPERM.
     * Scenario: non-root user runs `braid add`, cryptsetup luksFormat returns EPERM.
     */
    #[test]
    fn hint_format_exit_2_is_permission() {
        assert_eq!(cryptsetup_format_hint(2), "permission denied (not root?)");
    }

    /*
     * Intent: verify the format hint maps exit 5 to busy/already-formatted.
     * Why: exit 5 means EBUSY or EEXIST — for format, "already formatted" is the likely cause.
     * Scenario: user tries to format a device that already has a LUKS header.
     */
    #[test]
    fn hint_format_exit_5_is_busy() {
        assert_eq!(
            cryptsetup_format_hint(5),
            "device busy or already formatted"
        );
    }

    /*
     * Intent: verify the format hint returns a fallback for unknown exit codes.
     * Why: upstream may add new exit codes; the helper must not panic.
     * Scenario: future cryptsetup version returns an unmapped exit code during format.
     */
    #[test]
    fn hint_format_unknown_code() {
        assert_eq!(cryptsetup_format_hint(42), "unknown error");
    }

    // ---- classify_verify_exit -------------------------------------------
    //
    // These tests pin the boundary between "cryptsetup said the credential
    // was wrong" (exit 2, the only semantically auth-related exit per
    // cryptsetup's translate_errno) and "cryptsetup could not complete the
    // verify attempt for any other reason". Without this boundary,
    // downstream narrates "wrong passphrase" for busy/missing/OOM/generic
    // failures -- the original bug.

    fn raw(exit_status: i32) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "cryptsetup open --test-passphrase".into(),
            stdout: String::new(),
            stderr: "stderr text".into(),
            exit_status,
        }
    }

    /*
     * Intent: exit 0 from --test-passphrase classifies as Authenticated.
     * Why: the success case is the only thing callers should trust to
     *   proceed with opening every disk with the verified credential.
     * Scenario: user enters the correct passphrase on a healthy pool.
     */
    #[test]
    fn classify_verify_exit_0_is_authenticated() {
        let out = classify_verify_exit("/dev/sda", &raw(0)).unwrap();
        assert_eq!(out, VerifyOutcome::Authenticated);
    }

    /*
     * Intent: exit 2 (EPERM) from --test-passphrase classifies as Rejected.
     * Why: EPERM is the one-and-only cryptsetup exit that means "wrong
     *   credential"; all other exits are environmental errors.
     * Scenario: user enters a wrong passphrase on a healthy pool.
     */
    #[test]
    fn classify_verify_exit_2_is_rejected() {
        let out = classify_verify_exit("/dev/sda", &raw(2)).unwrap();
        assert_eq!(out, VerifyOutcome::Rejected);
    }

    /*
     * Intent: exit 1 (generic/EINVAL) surfaces as OpenFailed, not Rejected.
     * Why: pre-fix, every non-zero exit collapsed to "wrong passphrase"
     *   through the downstream explain_open_failure path. Generic errors
     *   have nothing to do with the credential.
     * Scenario: cryptsetup hits a generic failure (e.g. bad invocation
     *   or ENOENT on a dependency).
     */
    #[test]
    fn classify_verify_exit_1_is_open_failed() {
        let err = classify_verify_exit("/dev/sda", &raw(1)).unwrap_err();
        match err {
            LuksError::OpenFailed {
                exit_code, hint, ..
            } => {
                assert_eq!(exit_code, 1);
                assert_eq!(hint, "generic failure");
            }
            other => panic!("expected OpenFailed, got {other:?}"),
        }
    }

    /*
     * Intent: exit 3 (ENOMEM) surfaces as OpenFailed with the OOM hint.
     * Why: a memory-exhaustion failure is not a credential rejection.
     * Scenario: low-memory machine exhausts PBKDF argon2 memory budget.
     */
    #[test]
    fn classify_verify_exit_3_is_open_failed() {
        let err = classify_verify_exit("/dev/sda", &raw(3)).unwrap_err();
        match err {
            LuksError::OpenFailed {
                exit_code, hint, ..
            } => {
                assert_eq!(exit_code, 3);
                assert_eq!(hint, "out of memory");
            }
            other => panic!("expected OpenFailed, got {other:?}"),
        }
    }

    /*
     * Intent: exit 4 (ENODEV/ENOTBLK) surfaces as OpenFailed with the
     *   device-not-found hint.
     * Why: a vanished device is not a wrong passphrase.
     * Scenario: hot-unplug or enclosure dropout between probe and verify.
     */
    #[test]
    fn classify_verify_exit_4_is_open_failed() {
        let err = classify_verify_exit("/dev/sda", &raw(4)).unwrap_err();
        match err {
            LuksError::OpenFailed {
                exit_code, hint, ..
            } => {
                assert_eq!(exit_code, 4);
                assert_eq!(hint, "device not found or not a block device");
            }
            other => panic!("expected OpenFailed, got {other:?}"),
        }
    }

    /*
     * Intent: exit 5 (EBUSY/EEXIST) surfaces as OpenFailed with the
     *   busy/already-open hint. This is the canonical misdiagnosis
     *   scenario: a stale mapper looked like a lockout pre-fix.
     * Why: the bug that motivated this whole plan.
     * Scenario: a previous open left a dm-crypt mapper on the same
     *   backing device; the user's verify attempt now gets EBUSY.
     */
    #[test]
    fn classify_verify_exit_5_is_open_failed() {
        let err = classify_verify_exit("/dev/sda", &raw(5)).unwrap_err();
        match err {
            LuksError::OpenFailed {
                exit_code, hint, ..
            } => {
                assert_eq!(exit_code, 5);
                assert_eq!(hint, "device is already open or busy");
            }
            other => panic!("expected OpenFailed, got {other:?}"),
        }
    }

    fn crypt_status_inactive(mapper: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup status {mapper}"),
            stdout: String::new(),
            stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
            exit_status: 4,
        }
    }

    fn crypt_status_active(mapper: &str, device: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup status {mapper}"),
            stdout: format!(
                "/dev/mapper/{mapper} is active and is in use.\n\
                 \ttype:    LUKS2\n\
                 \tdevice:  {device}\n\
                 \tmode:    read/write\n"
            ),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn crypt_status_active_missing_device(mapper: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup status {mapper}"),
            stdout: format!(
                "/dev/mapper/{mapper} is active and is in use.\n\
                 \ttype:    LUKS2\n\
                 \tmode:    read/write\n"
            ),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn luks_uuid_output(device: &str, uuid: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup luksUUID {device}"),
            stdout: format!("{uuid}\n"),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn luks_uuid_not_luks(device: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup luksUUID {device}"),
            stdout: String::new(),
            stderr: format!("Device {device} is not a valid LUKS device.\n"),
            exit_status: 1,
        }
    }

    fn luks_uuid_invalid(device: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup luksUUID {device}"),
            stdout: "not-a-uuid\n".into(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    /*
     * Intent: verify ensure_luks_open produces an exit-code-specific error for exit 2.
     * Why: previously all failures said "Wrong passphrase?" regardless of cause.
     * Scenario: cryptsetup open returns exit 2 (EPERM) with diagnostic stderr.
     */
    #[test]
    fn ensure_luks_open_exit_2_mentions_passphrase() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-testdisk".into(),
                },
                crypt_status_inactive("braid-testdisk"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/test-disk".into(),
                    mapper: "braid-testdisk".into(),
                },
                b"wrong".to_vec(),
                RawCommandOutput {
                    cmd: "cryptsetup open".into(),
                    stdout: String::new(),
                    stderr: "No key available with this passphrase.\n".into(),
                    exit_status: 2,
                },
            );
        let err = ensure_luks_open(
            &runner,
            "testdisk",
            &ByIdPath("/dev/disk/by-id/test-disk".into()),
            "wrong",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exit 2"),
            "should contain exit code, got: {msg}"
        );
        assert!(
            msg.contains("wrong passphrase or permission denied"),
            "should contain hint, got: {msg}"
        );
        assert!(
            msg.contains("No key available with this passphrase."),
            "should contain stderr, got: {msg}"
        );
    }

    /*
     * Intent: verify ensure_luks_open produces a device-not-found error for exit 4.
     * Why: exit 4 (ENODEV) was previously reported as "Wrong passphrase?".
     * Scenario: device disappears between probe and open (e.g. hot-unplug).
     */
    #[test]
    fn ensure_luks_open_exit_4_mentions_device_not_found() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-vanished".into(),
                },
                crypt_status_inactive("braid-vanished"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/vanished-disk".into(),
                    mapper: "braid-vanished".into(),
                },
                b"pass".to_vec(),
                RawCommandOutput {
                    cmd: "cryptsetup open".into(),
                    stdout: String::new(),
                    stderr: "Device /dev/disk/by-id/vanished-disk does not exist.\n".into(),
                    exit_status: 4,
                },
            );
        let err = ensure_luks_open(
            &runner,
            "vanished",
            &ByIdPath("/dev/disk/by-id/vanished-disk".into()),
            "pass",
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exit 4"),
            "should contain exit code, got: {msg}"
        );
        assert!(
            msg.contains("device not found"),
            "should contain hint, got: {msg}"
        );
    }

    /*
     * Intent: verify ensure_luks_open_with_key_file produces an exit-code-specific
     *   error for exit 2.
     * Why: the keyfile path had the same generic "Wrong keyfile?" bug.
     * Scenario: cryptsetup open with keyfile returns exit 2 (wrong key material).
     */
    #[test]
    fn ensure_luks_open_with_key_file_exit_2_mentions_passphrase() {
        let kf = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(kf.path(), b"badkey").unwrap();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-testdisk".into(),
                },
                crypt_status_inactive("braid-testdisk"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksOpenKeyFile {
                    device: "/dev/disk/by-id/test-disk".into(),
                    mapper: "braid-testdisk".into(),
                    key_file_path: kf.path().display().to_string(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup open".into(),
                    stdout: String::new(),
                    stderr: "No key available with this passphrase.\n".into(),
                    exit_status: 2,
                },
            );
        let err = ensure_luks_open_with_key_file(
            &runner,
            "testdisk",
            &ByIdPath("/dev/disk/by-id/test-disk".into()),
            kf.path(),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exit 2"),
            "should contain exit code, got: {msg}"
        );
        assert!(
            msg.contains("wrong passphrase or permission denied"),
            "should contain hint, got: {msg}"
        );
    }

    /*
     * Intent: verify ensure_luks_open_with_key_file produces a device-not-found
     *   error for exit 4.
     * Why: exit 4 (ENODEV) was previously reported as "Wrong keyfile?".
     * Scenario: device disappears between probe and keyfile-based open.
     */
    #[test]
    fn ensure_luks_open_with_key_file_exit_4_mentions_device_not_found() {
        let kf = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(kf.path(), b"key").unwrap();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-vanished".into(),
                },
                crypt_status_inactive("braid-vanished"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksOpenKeyFile {
                    device: "/dev/disk/by-id/vanished-disk".into(),
                    mapper: "braid-vanished".into(),
                    key_file_path: kf.path().display().to_string(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup open".into(),
                    stdout: String::new(),
                    stderr: "Device does not exist.\n".into(),
                    exit_status: 4,
                },
            );
        let err = ensure_luks_open_with_key_file(
            &runner,
            "vanished",
            &ByIdPath("/dev/disk/by-id/vanished-disk".into()),
            kf.path(),
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exit 4"),
            "should contain exit code, got: {msg}"
        );
        assert!(
            msg.contains("device not found"),
            "should contain hint, got: {msg}"
        );
    }

    /*
     * Intent: inactive mapper ownership checks fall through to cryptsetup open.
     * Why it exists: the new ownership gate must preserve normal unlock behavior
     *   when cryptsetup status reports the mapper is closed.
     * Scenario: fresh add has just formatted LUKS and now opens the new mapper.
     */
    #[test]
    fn ensure_luks_open_inactive_mapper_runs_open() {
        let by_id = ByIdPath("/dev/disk/by-id/disk1".into());
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                crypt_status_inactive("braid-disk1"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: by_id.0.clone(),
                    mapper: "braid-disk1".into(),
                },
                b"pass".to_vec(),
                RawCommandOutput {
                    cmd: "cryptsetup open".into(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            );

        ensure_luks_open(&runner, "disk1", &by_id, "pass").unwrap();

        assert_eq!(
            runner.requests(),
            vec![
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                CmdRequest::CryptsetupLuksOpen {
                    device: by_id.0,
                    mapper: "braid-disk1".into(),
                },
            ]
        );
    }

    /*
     * Intent: an active mapper backed by the requested LUKS UUID is accepted
     *   and does not run cryptsetup open again.
     * Why it exists: idempotent unlock should be safe without trusting only
     *   the presence of /dev/mapper/braid-<name>.
     * Scenario: a previous command already opened the correct disk.
     */
    #[test]
    fn ensure_luks_open_active_mapper_matching_uuid_skips_open() {
        let by_id = ByIdPath("/dev/disk/by-id/disk1".into());
        let uuid = "11111111-1111-1111-1111-111111111111";
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                crypt_status_active("braid-disk1", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: by_id.0.clone(),
                },
                luks_uuid_output(&by_id.0, uuid),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                luks_uuid_output("/dev/vdb", uuid),
            );

        ensure_luks_open(&runner, "disk1", &by_id, "pass").unwrap();

        assert_eq!(
            runner.requests(),
            vec![
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                CmdRequest::CryptsetupLuksUuid { device: by_id.0 },
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
            ]
        );
    }

    /*
     * Intent: an active mapper backed by a different LUKS UUID is rejected
     *   before cryptsetup open can run.
     * Why it exists: a stale or manually-created mapper using braid's mapper
     *   name must not let bootstrap format or mount the wrong disk.
     * Scenario: /dev/mapper/braid-disk1 points at another encrypted device.
     */
    #[test]
    fn ensure_luks_open_active_mapper_different_uuid_conflicts() {
        let by_id = ByIdPath("/dev/disk/by-id/disk1".into());
        let expected_uuid = "11111111-1111-1111-1111-111111111111";
        let found_uuid = "99999999-9999-9999-9999-999999999999";
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                crypt_status_active("braid-disk1", "/dev/vdz"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: by_id.0.clone(),
                },
                luks_uuid_output(&by_id.0, expected_uuid),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdz".into(),
                },
                luks_uuid_output("/dev/vdz", found_uuid),
            );

        let err = ensure_luks_open(&runner, "disk1", &by_id, "pass").unwrap_err();

        match err {
            LuksError::MapperConflict {
                name,
                expected,
                found,
            } => {
                assert_eq!(name, "disk1");
                assert_eq!(expected.0, expected_uuid);
                assert_eq!(found.map(|uuid| uuid.0), Some(found_uuid.to_owned()));
            }
            other => panic!("expected MapperConflict, got {other:?}"),
        }
        assert!(
            !runner
                .requests()
                .iter()
                .any(|request| matches!(request, CmdRequest::CryptsetupLuksOpen { .. })),
            "must not run cryptsetup open after mapper conflict"
        );
    }

    /*
     * Intent: an active mapper with `device: (null)` is rejected as a mapper
     *   conflict with no found UUID.
     * Why it exists: hot-unplug can leave a stale dm-crypt mapper with no
     *   backing device; that must not count as ownership.
     * Scenario: cryptsetup status reports an active mapper with null backing.
     */
    #[test]
    fn ensure_luks_open_active_mapper_null_device_conflicts() {
        let by_id = ByIdPath("/dev/disk/by-id/disk1".into());
        let expected_uuid = "11111111-1111-1111-1111-111111111111";
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                crypt_status_active("braid-disk1", "(null)"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: by_id.0.clone(),
                },
                luks_uuid_output(&by_id.0, expected_uuid),
            );

        let err = ensure_luks_open(&runner, "disk1", &by_id, "pass").unwrap_err();

        match err {
            LuksError::MapperConflict {
                name,
                expected,
                found,
            } => {
                assert_eq!(name, "disk1");
                assert_eq!(expected.0, expected_uuid);
                assert_eq!(found, None);
            }
            other => panic!("expected MapperConflict, got {other:?}"),
        }
    }

    /*
     * Intent: an active mapper backed by a non-LUKS device is rejected as a
     *   mapper conflict with no found UUID.
     * Why it exists: a mapper name can be aliased to something that no longer
     *   reports a LUKS header; this is ownership failure, not successful reuse.
     * Scenario: cryptsetup status names /dev/vdz, but luksUUID says it is not LUKS.
     */
    #[test]
    fn ensure_luks_open_active_mapper_non_luks_backing_conflicts() {
        let by_id = ByIdPath("/dev/disk/by-id/disk1".into());
        let expected_uuid = "11111111-1111-1111-1111-111111111111";
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                crypt_status_active("braid-disk1", "/dev/vdz"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: by_id.0.clone(),
                },
                luks_uuid_output(&by_id.0, expected_uuid),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdz".into(),
                },
                luks_uuid_not_luks("/dev/vdz"),
            );

        let err = ensure_luks_open(&runner, "disk1", &by_id, "pass").unwrap_err();

        assert!(matches!(err, LuksError::MapperConflict { found: None, .. }));
        assert!(
            !runner
                .requests()
                .iter()
                .any(|request| matches!(request, CmdRequest::CryptsetupLuksOpen { .. })),
            "must not run cryptsetup open after mapper conflict"
        );
    }

    /*
     * Intent: keyfile open accepts an already-open mapper only when its
     *   backing LUKS UUID matches the requested by-id disk.
     * Why it exists: the keyfile path shares the same ownership invariant as
     *   passphrase unlock.
     * Scenario: auto-unlock retries after the correct mapper is already open.
     */
    #[test]
    fn ensure_luks_open_with_key_file_active_mapper_matching_uuid_skips_open() {
        let by_id = ByIdPath("/dev/disk/by-id/disk1".into());
        let uuid = "11111111-1111-1111-1111-111111111111";
        let kf = tempfile::NamedTempFile::new().unwrap();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                crypt_status_active("braid-disk1", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: by_id.0.clone(),
                },
                luks_uuid_output(&by_id.0, uuid),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                luks_uuid_output("/dev/vdb", uuid),
            );

        ensure_luks_open_with_key_file(&runner, "disk1", &by_id, kf.path()).unwrap();

        assert!(
            !runner
                .requests()
                .iter()
                .any(|request| matches!(request, CmdRequest::CryptsetupLuksOpenKeyFile { .. })),
            "must not run cryptsetup open with keyfile for already-owned mapper"
        );
    }

    /*
     * Intent: keyfile open rejects an active mapper backed by a different
     *   LUKS UUID.
     * Why it exists: keyfile unlock must not trust mapper existence more than
     *   passphrase unlock does.
     * Scenario: auto-unlock sees /dev/mapper/braid-disk1, but it belongs to
     *   another encrypted device.
     */
    #[test]
    fn ensure_luks_open_with_key_file_active_mapper_different_uuid_conflicts() {
        let by_id = ByIdPath("/dev/disk/by-id/disk1".into());
        let expected_uuid = "11111111-1111-1111-1111-111111111111";
        let found_uuid = "99999999-9999-9999-9999-999999999999";
        let kf = tempfile::NamedTempFile::new().unwrap();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                crypt_status_active("braid-disk1", "/dev/vdz"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: by_id.0.clone(),
                },
                luks_uuid_output(&by_id.0, expected_uuid),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdz".into(),
                },
                luks_uuid_output("/dev/vdz", found_uuid),
            );

        let err = ensure_luks_open_with_key_file(&runner, "disk1", &by_id, kf.path()).unwrap_err();

        assert!(matches!(
            err,
            LuksError::MapperConflict { found: Some(_), .. }
        ));
        assert!(
            !runner
                .requests()
                .iter()
                .any(|request| matches!(request, CmdRequest::CryptsetupLuksOpenKeyFile { .. })),
            "must not run keyfile open after mapper conflict"
        );
    }

    /*
     * Intent: keyfile open rejects active mappers with `device: (null)`.
     * Why it exists: null backing means the mapper is stale, not owned by the
     *   requested by-id disk.
     * Scenario: auto-unlock runs after a drive vanished while its mapper stayed active.
     */
    #[test]
    fn ensure_luks_open_with_key_file_active_mapper_null_device_conflicts() {
        let by_id = ByIdPath("/dev/disk/by-id/disk1".into());
        let expected_uuid = "11111111-1111-1111-1111-111111111111";
        let kf = tempfile::NamedTempFile::new().unwrap();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                crypt_status_active("braid-disk1", "(null)"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: by_id.0.clone(),
                },
                luks_uuid_output(&by_id.0, expected_uuid),
            );

        let err = ensure_luks_open_with_key_file(&runner, "disk1", &by_id, kf.path()).unwrap_err();

        assert!(matches!(err, LuksError::MapperConflict { found: None, .. }));
    }

    /*
     * Intent: malformed active cryptsetup status output propagates as
     *   LuksError::Parse, not MapperConflict.
     * Why it exists: parser failures should stay parser failures; treating
     *   malformed tool output as an ownership conflict hides compatibility drift.
     * Scenario: cryptsetup status says active but omits the required device line.
     */
    #[test]
    fn ensure_luks_open_malformed_active_status_returns_parse_error() {
        let by_id = ByIdPath("/dev/disk/by-id/disk1".into());
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupStatus {
                mapper: "braid-disk1".into(),
            },
            crypt_status_active_missing_device("braid-disk1"),
        );

        let err = ensure_luks_open(&runner, "disk1", &by_id, "pass").unwrap_err();

        assert!(matches!(
            err,
            LuksError::Parse(crate::parse::ParseError::MissingField { .. })
        ));
    }

    /*
     * Intent: invalid UUID text from an active backing device propagates as
     *   LuksError::Parse, not MapperConflict.
     * Why it exists: an exit-0 luksUUID response with malformed text is parser
     *   drift, while a non-LUKS exit is an ownership conflict.
     * Scenario: requested by-id UUID parses, but backing luksUUID emits junk.
     */
    #[test]
    fn ensure_luks_open_invalid_backing_uuid_returns_parse_error() {
        let by_id = ByIdPath("/dev/disk/by-id/disk1".into());
        let expected_uuid = "11111111-1111-1111-1111-111111111111";
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                crypt_status_active("braid-disk1", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: by_id.0.clone(),
                },
                luks_uuid_output(&by_id.0, expected_uuid),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                luks_uuid_invalid("/dev/vdb"),
            );

        let err = ensure_luks_open(&runner, "disk1", &by_id, "pass").unwrap_err();

        assert!(matches!(
            err,
            LuksError::Parse(crate::parse::ParseError::InvalidText { .. })
        ));
    }

    /*
     * Intent: verify luks_format produces an exit-code-specific error for exit 2.
     * Why: previously all format failures said "cryptsetup luksFormat failed (exit N)" generically.
     * Scenario: non-root user runs `braid add`, cryptsetup luksFormat returns EPERM.
     */
    #[test]
    fn luks_format_exit_2_mentions_permission() {
        let runner = MockRunner::default().with_output_stdin(
            CmdRequest::CryptsetupLuksFormat {
                device: "/dev/sda".into(),
                extra_opts: vec![],
            },
            b"pass".to_vec(),
            RawCommandOutput {
                cmd: "cryptsetup luksFormat".into(),
                stdout: String::new(),
                stderr: "Cannot format device /dev/sda, permission denied.\n".into(),
                exit_status: 2,
            },
        );
        let err = luks_format(&runner, "/dev/sda", "pass", &[]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exit 2"),
            "should contain exit code, got: {msg}"
        );
        assert!(
            msg.contains("permission denied"),
            "should contain hint, got: {msg}"
        );
        assert!(
            msg.contains("Cannot format device"),
            "should contain stderr, got: {msg}"
        );
    }

    /*
     * Intent: verify luks_format produces a device-not-found error for exit 4.
     * Why: exit 4 (ENODEV) was previously indistinguishable from any other failure.
     * Scenario: device path is stale or mistyped, cryptsetup luksFormat returns ENODEV.
     */
    #[test]
    fn luks_format_exit_4_mentions_device_not_found() {
        let runner = MockRunner::default().with_output_stdin(
            CmdRequest::CryptsetupLuksFormat {
                device: "/dev/sdz".into(),
                extra_opts: vec![],
            },
            b"pass".to_vec(),
            RawCommandOutput {
                cmd: "cryptsetup luksFormat".into(),
                stdout: String::new(),
                stderr: "Device /dev/sdz does not exist.\n".into(),
                exit_status: 4,
            },
        );
        let err = luks_format(&runner, "/dev/sdz", "pass", &[]).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("exit 4"),
            "should contain exit code, got: {msg}"
        );
        assert!(
            msg.contains("device not found"),
            "should contain hint, got: {msg}"
        );
    }

    /*
     * Intent: verify advisory returns empty when directory doesn't exist.
     * Why: no false positives when no backups have ever been created.
     * Scenario: fresh system, /var/lib/braid/luks-headers doesn't exist yet.
     */
    #[test]
    fn advisory_empty_when_dir_missing() {
        let dir = std::path::Path::new("/tmp/nonexistent-braid-test-dir");
        assert!(header_backup_advisories_in(dir).is_empty());
    }

    /*
     * Intent: verify advisory returns empty when directory exists but has no backup files.
     * Why: no false positives from empty directories or unrelated files.
     * Scenario: backup dir was created but all backups were cleaned up.
     */
    #[test]
    fn advisory_empty_when_dir_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(header_backup_advisories_in(dir.path()).is_empty());
    }

    /*
     * Intent: verify advisory fires when .luksheader files are present.
     * Why: post-migration backups should trigger the security nudge.
     * Scenario: user ran `braid add`, header backup exists with new extension.
     */
    #[test]
    fn advisory_present_for_luksheader() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("braid-disk1.luksheader"), b"fake").unwrap();
        let advisories = header_backup_advisories_in(dir.path());
        assert_eq!(advisories.len(), 1);
        assert!(advisories[0].contains("copy offsite"));
    }

    /*
     * Intent: verify advisory fires when old .img files are present.
     * Why: pre-migration backups should still trigger the security nudge.
     * Scenario: user has old-format backups that haven't been migrated yet.
     */
    #[test]
    fn advisory_present_for_img() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("braid-disk1.img"), b"fake").unwrap();
        let advisories = header_backup_advisories_in(dir.path());
        assert_eq!(advisories.len(), 1);
        assert!(advisories[0].contains("copy offsite"));
    }

    /*
     * Intent: verify unrelated files don't trigger advisories.
     * Why: only actual LUKS header backups should produce warnings.
     * Scenario: other files exist in the directory (e.g. .json, .txt).
     */
    #[test]
    fn advisory_ignores_unrelated_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("notes.txt"), b"hello").unwrap();
        assert!(header_backup_advisories_in(dir.path()).is_empty());
    }

    /*
     * Intent: verify the public header_backup_advisories wrapper reads from
     *   the custom root rather than a hardcoded path.
     * Why: the wrapper is a thin delegation to header_backup_advisories_in;
     *   a regression that ignores StatePaths would silently use production paths.
     * Scenario: test creates a .luksheader file under a custom state root and
     *   confirms the public API sees it.
     */
    #[test]
    fn advisory_via_state_paths() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::state_paths::StatePaths::custom(dir.path().into());
        let headers_dir = paths.luks_headers_dir();
        std::fs::create_dir_all(&headers_dir).unwrap();
        std::fs::write(headers_dir.join("braid-disk1.luksheader"), b"fake").unwrap();
        let advisories = header_backup_advisories(&paths);
        assert_eq!(advisories.len(), 1);
        assert!(advisories[0].contains("copy offsite"));
    }

    // ── read_passphrase (file path) ──────────────────────────────────

    /*
     * Intent: passphrase file with trailing newline returns trimmed value.
     * Why: most text editors append a trailing newline; stripping it is
     *   the expected default so users don't have to think about it.
     * Scenario: user creates passphrase file with `echo "hunter2" > pw.txt`.
     */
    #[test]
    fn read_passphrase_file_simple() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw.txt");
        std::fs::write(&file, "hunter2\n").unwrap();
        let result = read_passphrase(Some(file.as_path()), false).unwrap();
        assert_eq!(result, "hunter2");
    }

    /*
     * Intent: passphrase file without trailing newline still works.
     * Why: `printf` and some tooling produce files without a final newline.
     * Scenario: user creates passphrase file with `printf "hunter2" > pw.txt`.
     */
    #[test]
    fn read_passphrase_file_no_trailing_newline() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw.txt");
        std::fs::write(&file, "hunter2").unwrap();
        let result = read_passphrase(Some(file.as_path()), false).unwrap();
        assert_eq!(result, "hunter2");
    }

    /*
     * Intent: passphrase file with CRLF line ending returns trimmed value.
     * Why: Windows-origin files use \r\n; both characters must be stripped
     *   from the trailing position.
     * Scenario: user edits passphrase file on Windows, copies to NAS.
     */
    #[test]
    fn read_passphrase_file_crlf_trailing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw.txt");
        std::fs::write(&file, "hunter2\r\n").unwrap();
        let result = read_passphrase(Some(file.as_path()), false).unwrap();
        assert_eq!(result, "hunter2");
    }

    /*
     * Intent: passphrase file with embedded newline is rejected.
     * Why: a multi-line passphrase works via --key-file=- but cannot be
     *   entered interactively (TTY/stdin read one line), locking the user
     *   into file-only input.
     * Scenario: user accidentally pastes two lines into their passphrase file.
     */
    #[test]
    fn read_passphrase_file_embedded_newline_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw.txt");
        std::fs::write(&file, "line1\nline2\n").unwrap();
        let err = read_passphrase(Some(file.as_path()), false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("line-break"),
            "expected 'line-break' in: {msg}"
        );
    }

    /*
     * Intent: passphrase file with embedded carriage return is rejected.
     * Why: \r is also a line-break character that cannot be typed at a
     *   TTY prompt; same lock-in risk as \n.
     * Scenario: corrupted or binary-pasted passphrase file.
     */
    #[test]
    fn read_passphrase_file_embedded_cr_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw.txt");
        std::fs::write(&file, "ab\rcd\n").unwrap();
        let err = read_passphrase(Some(file.as_path()), false).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("line-break"),
            "expected 'line-break' in: {msg}"
        );
    }

    /*
     * Intent: passphrase file containing only a newline is rejected as empty.
     * Why: after trimming the trailing newline, the passphrase is empty;
     *   stdin and TTY paths already reject empty passphrases.
     * Scenario: user creates file with `echo > pw.txt` (writes just "\n").
     */
    #[test]
    fn read_passphrase_file_empty_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw.txt");
        std::fs::write(&file, "\n").unwrap();
        let err = read_passphrase(Some(file.as_path()), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("empty"), "expected 'empty' in: {msg}");
    }

    /*
     * Intent: passphrase file containing only CRLF is rejected as empty.
     * Why: same as above but for Windows-style line endings.
     * Scenario: empty passphrase file created on Windows.
     */
    #[test]
    fn read_passphrase_file_only_crlf_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw.txt");
        std::fs::write(&file, "\r\n").unwrap();
        let err = read_passphrase(Some(file.as_path()), false).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("empty"), "expected 'empty' in: {msg}");
    }

    /*
     * Intent: leading/trailing spaces are preserved in the passphrase.
     * Why: spaces may be intentional passphrase characters; only line-break
     *   characters should be stripped, not whitespace in general.
     * Scenario: user deliberately includes spaces in their passphrase.
     */
    #[test]
    fn read_passphrase_file_preserves_spaces() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw.txt");
        std::fs::write(&file, " spaced \n").unwrap();
        let result = read_passphrase(Some(file.as_path()), false).unwrap();
        assert_eq!(result, " spaced ");
    }

    /*
     * Intent: nonexistent passphrase file returns an error.
     * Why: clear error message is better than a panic or generic I/O error.
     * Scenario: user typos the path in --passphrase-file.
     */
    #[test]
    fn read_passphrase_file_missing() {
        let result = read_passphrase(Some(std::path::Path::new("/no/such/file")), false);
        assert!(result.is_err());
    }

    // ── check_passphrase_match ───────────────────────────────────────
    //
    // The byte-equality comparator that gates fresh-format adds. These
    // unit tests pin the pure function; the cmd-level regression lives
    // in add.rs.

    /*
     * Intent: two identical passphrases return Ok with the passphrase.
     * Why: happy path for the confirm-twice flow on bootstrap add.
     * Scenario: user types the intended passphrase both times.
     */
    #[test]
    fn check_passphrase_match_ok_on_equal() {
        let got = check_passphrase_match("secret".into(), "secret".into()).unwrap();
        assert_eq!(got, "secret");
    }

    /*
     * Intent: differing passphrases return Validation with "do not match".
     * Why: the primary regression -- a typo on fresh format must be
     *   rejected before `luks_format` runs.
     * Scenario: user typos the second prompt.
     */
    #[test]
    fn check_passphrase_match_err_on_differ() {
        let err = check_passphrase_match("abc".into(), "xyz".into()).unwrap_err();
        match err {
            LuksError::Validation(msg) => assert!(
                msg.contains("do not match"),
                "expected 'do not match' in: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /*
     * Intent: comparison is case-sensitive.
     * Why: LUKS passphrases are opaque byte strings -- "ABC" and "abc"
     *   are different keys and must not silently match.
     * Scenario: user holds shift on the second prompt by accident.
     */
    #[test]
    fn check_passphrase_match_case_sensitive() {
        assert!(check_passphrase_match("ABC".into(), "abc".into()).is_err());
    }

    /*
     * Intent: trailing whitespace is significant.
     * Why: same reason as case-sensitivity -- the LUKS keyslot binds
     *   to the exact byte string.
     * Scenario: user hits space after the second prompt.
     */
    #[test]
    fn check_passphrase_match_trailing_whitespace_sensitive() {
        assert!(check_passphrase_match("abc".into(), "abc ".into()).is_err());
    }

    // ── read_passphrase_stdin_from ───────────────────────────────────
    //
    // Mirrors the `confirm_yes_from` pattern in cli/src/confirm.rs:101.
    // Tests feed a Cursor so the production stdin path is unchanged but
    // behavior is pinned at unit level.

    /*
     * Intent: a well-formed line returns the trimmed passphrase.
     * Why: core happy path for `--passphrase-stdin`.
     * Scenario: `echo "secret" | braid add --passphrase-stdin ...`.
     */
    #[test]
    fn read_passphrase_stdin_from_ok() {
        let mut cur = std::io::Cursor::new(b"secret\n");
        let got = read_passphrase_stdin_from(&mut cur).unwrap();
        assert_eq!(got, "secret");
    }

    /*
     * Intent: a line containing only a newline is rejected as empty.
     * Why: symmetry with file/TTY paths -- empty passphrases are
     *   forbidden everywhere.
     * Scenario: operator pipes an empty string by mistake.
     */
    #[test]
    fn read_passphrase_stdin_from_empty_rejected() {
        let mut cur = std::io::Cursor::new(b"\n");
        let err = read_passphrase_stdin_from(&mut cur).unwrap_err();
        match err {
            LuksError::Validation(msg) => {
                assert!(msg.contains("empty"), "expected 'empty' in: {msg}")
            }
            other => panic!("expected Validation, got {other:?}"),
        }
    }

    /*
     * Intent: CRLF line endings are stripped to the passphrase.
     * Why: Windows-origin pipes may carry \r\n; symmetry with the file
     *   path's `read_passphrase_file_crlf_trailing` coverage.
     * Scenario: passphrase piped from a Windows-authored source.
     */
    #[test]
    fn read_passphrase_stdin_from_strips_crlf() {
        let mut cur = std::io::Cursor::new(b"secret\r\n");
        let got = read_passphrase_stdin_from(&mut cur).unwrap();
        assert_eq!(got, "secret");
    }

    // ── read_passphrase_with_readers (branch selection) ──────────────
    //
    // These tests pin the file/stdin/TTY branch selection inside the
    // seam-aware helper. Using a scripted PassphraseReader + Cursor
    // lets us observe which branch actually consumed input, without
    // swapping process stdin. `ScriptedPassphraseReader` lives at
    // module scope (above) so add.rs tests can reuse it.

    /*
     * Intent: TTY branch with confirm_new=false reads exactly once.
     * Why: non-bootstrap add must not double-prompt; a regression that
     *   always confirms would consume the SENTINEL entry.
     * Scenario: subsequent `braid add` to a live pool -- one TTY read,
     *   then verify_passphrase catches any typo.
     */
    #[test]
    fn read_passphrase_with_readers_tty_no_confirm_single_read() {
        let tty = ScriptedPassphraseReader::new(["pw", "SENTINEL"]);
        let mut stdin = std::io::Cursor::new(&[][..]);
        let got = read_passphrase_with_readers(None, false, false, &mut stdin, &tty).unwrap();
        assert_eq!(got, "pw");
        assert_eq!(tty.remaining(), 1, "SENTINEL must remain unconsumed");
    }

    /*
     * Intent: TTY branch with confirm_new=true consumes both prompts on match.
     * Why: the primary confirm flow on bootstrap add.
     * Scenario: user correctly types the same passphrase twice.
     */
    #[test]
    fn read_passphrase_with_readers_tty_confirm_consumes_two() {
        let tty = ScriptedPassphraseReader::new(["pw", "pw"]);
        let mut stdin = std::io::Cursor::new(&[][..]);
        let got = read_passphrase_with_readers(None, false, true, &mut stdin, &tty).unwrap();
        assert_eq!(got, "pw");
        assert_eq!(tty.remaining(), 0);
    }

    /*
     * Intent: TTY branch with confirm_new=true returns Validation on mismatch.
     * Why: the load-bearing reject path for bootstrap typo protection.
     * Scenario: user typos the second prompt.
     */
    #[test]
    fn read_passphrase_with_readers_tty_confirm_mismatch_err() {
        let tty = ScriptedPassphraseReader::new(["pw", "typo"]);
        let mut stdin = std::io::Cursor::new(&[][..]);
        let err = read_passphrase_with_readers(None, false, true, &mut stdin, &tty).unwrap_err();
        match err {
            LuksError::Validation(msg) => assert!(
                msg.contains("do not match"),
                "expected 'do not match' in: {msg}"
            ),
            other => panic!("expected Validation, got {other:?}"),
        }
        assert_eq!(tty.remaining(), 0, "both prompts must have been read");
    }

    /*
     * Intent: file branch short-circuits both the stdin and TTY readers.
     * Why: `--passphrase-file` is an automation path; it must not
     *   trigger a confirmation prompt even when confirm_new is true.
     * Scenario: automation pipeline passes `--passphrase-file` during a
     *   fresh bootstrap add.
     */
    #[test]
    fn read_passphrase_with_readers_file_short_circuits_stdin_and_tty() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("pw.txt");
        std::fs::write(&file, b"from-file\n").unwrap();
        let tty = ScriptedPassphraseReader::new(["SENTINEL"]);
        let mut stdin = std::io::Cursor::new(b"STDIN_SHOULD_NOT_BE_READ\n".to_vec());
        let got = read_passphrase_with_readers(Some(&file), false, true, &mut stdin, &tty).unwrap();
        assert_eq!(got, "from-file");
        assert_eq!(tty.remaining(), 1, "SENTINEL must remain unconsumed");
        assert_eq!(
            stdin.position(),
            0,
            "stdin cursor must not have advanced (branch must short-circuit)"
        );
    }

    /*
     * Intent: stdin branch short-circuits the TTY reader even when confirm_new=true.
     * Why: pins the stdin-vs-TTY branch selection inside
     *   `read_passphrase_with_readers`. A regression that routed
     *   `--passphrase-stdin` through the TTY path (or accidentally
     *   prompted twice over stdin) would still satisfy the other tests,
     *   so this is the seam that catches that class of bug.
     * Scenario: `echo "pw" | braid add --passphrase-stdin ...` on a
     *   fresh bootstrap.
     */
    #[test]
    fn read_passphrase_with_readers_stdin_short_circuits_tty() {
        let tty = ScriptedPassphraseReader::new(["SENTINEL"]);
        let mut stdin = std::io::Cursor::new(b"from-stdin\n".to_vec());
        let got = read_passphrase_with_readers(None, true, true, &mut stdin, &tty).unwrap();
        assert_eq!(got, "from-stdin");
        assert_eq!(tty.remaining(), 1, "SENTINEL must remain unconsumed");
    }

    // -- probe_pool_keyfile_enrollment tests --

    use crate::types::{LuksUuid, MapperName, PoolDevice};

    fn make_pool_device(name: &str, underlying: &str) -> PoolDevice {
        PoolDevice {
            mapper: MapperName(format!("braid-{name}")),
            luks_uuid: LuksUuid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
            devid: 1,
            underlying: underlying.into(),
        }
    }

    fn luks_dump_slot1_empty(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksDump {
                device: device.to_owned(),
            },
            RawCommandOutput {
                cmd: format!("cryptsetup luksDump {device}"),
                stdout: r#"{"keyslots":{"0":{"type":"luks2"}}}"#.into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn luks_dump_slot1_occupied(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksDump {
                device: device.to_owned(),
            },
            RawCommandOutput {
                cmd: format!("cryptsetup luksDump {device}"),
                stdout: r#"{"keyslots":{"0":{"type":"luks2"},"1":{"type":"luks2"}}}"#.into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn luks_dump_error(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksDump {
                device: device.to_owned(),
            },
            RawCommandOutput {
                cmd: format!("cryptsetup luksDump {device}"),
                stdout: String::new(),
                stderr: "Device not found".into(),
                exit_status: 5,
            },
        )
    }

    /*
     * Intent: empty device list returns no enrollment and no failures.
     * Why: bootstrap case — no existing pool members to inspect.
     * Scenario: first `braid add` creating a new pool.
     */
    #[test]
    fn enrollment_check_empty_devices() {
        let runner = MockRunner::default();
        let probe = probe_pool_keyfile_enrollment(&runner, &[]);
        assert!(!probe.has_enrollment);
        assert!(probe.failures.is_empty());
    }

    /*
     * Intent: detect enrollment when a device has slot 1 occupied.
     * Why: core positive case for the warning.
     * Scenario: pool with USB keyfile auto-unlock, user runs `braid add` without --enroll.
     */
    #[test]
    fn enrollment_check_slot1_occupied() {
        let dev = make_pool_device("data1", "/dev/sda");
        let (req, resp) = luks_dump_slot1_occupied("/dev/sda");
        let runner = MockRunner::default().with_output(req, resp);
        let probe = probe_pool_keyfile_enrollment(&runner, &[dev]);
        assert!(probe.has_enrollment);
        assert!(probe.failures.is_empty());
    }

    /*
     * Intent: no enrollment when slot 1 is empty.
     * Why: core negative case — no warning needed.
     * Scenario: pool using passphrase only, no keyfile enrolled.
     */
    #[test]
    fn enrollment_check_slot1_empty() {
        let dev = make_pool_device("data1", "/dev/sda");
        let (req, resp) = luks_dump_slot1_empty("/dev/sda");
        let runner = MockRunner::default().with_output(req, resp);
        let probe = probe_pool_keyfile_enrollment(&runner, &[dev]);
        assert!(!probe.has_enrollment);
        assert!(probe.failures.is_empty());
    }

    /*
     * Intent: scan all devices, not just the first.
     * Why: mixed pools (first disk unenrolled, second enrolled) must still detect.
     * Scenario: user enrolled keyfile on some drives but not all.
     */
    #[test]
    fn enrollment_check_scans_all_devices() {
        let dev1 = make_pool_device("data1", "/dev/sda");
        let dev2 = make_pool_device("data2", "/dev/sdb");
        let (req1, resp1) = luks_dump_slot1_empty("/dev/sda");
        let (req2, resp2) = luks_dump_slot1_occupied("/dev/sdb");
        let runner = MockRunner::default()
            .with_output(req1, resp1)
            .with_output(req2, resp2);
        let probe = probe_pool_keyfile_enrollment(&runner, &[dev1, dev2]);
        assert!(probe.has_enrollment);
        assert!(probe.failures.is_empty());
    }

    /*
     * Intent: probe errors are returned as structured failures.
     * Why: callers own diagnostic routing and must not inherit stderr side effects.
     * Scenario: luksDump fails on a device (transient I/O error).
     */
    #[test]
    fn enrollment_check_error_is_structured_failure() {
        let dev = make_pool_device("data1", "/dev/sda");
        let (req, resp) = luks_dump_error("/dev/sda");
        let runner = MockRunner::default().with_output(req, resp);
        let probe = probe_pool_keyfile_enrollment(&runner, &[dev]);
        assert!(!probe.has_enrollment);
        assert_eq!(probe.failures.len(), 1);
        assert_eq!(probe.failures[0].device, "/dev/sda");
        assert!(
            probe.failures[0]
                .error
                .contains("cryptsetup luksDump failed (exit 5): Device not found"),
            "unexpected error text: {:?}",
            probe.failures[0].error
        );
        assert_eq!(
            format_keyfile_enrollment_probe_failure(&probe.failures[0]),
            "could not check keyfile enrollment on /dev/sda: cryptsetup luksDump failed (exit 5): Device not found; proceeding as if no keyfile is enrolled"
        );
    }

    /*
     * Intent: a later occupied slot still wins after an earlier probe failure.
     * Why: callers suppress uncertainty notes once any device proves enrollment.
     * Scenario: one pool member's luksDump fails, another member reports slot 1.
     */
    #[test]
    fn enrollment_check_failure_then_occupied_reports_both() {
        let dev1 = make_pool_device("data1", "/dev/sda");
        let dev2 = make_pool_device("data2", "/dev/sdb");
        let (req1, resp1) = luks_dump_error("/dev/sda");
        let (req2, resp2) = luks_dump_slot1_occupied("/dev/sdb");
        let runner = MockRunner::default()
            .with_output(req1, resp1)
            .with_output(req2, resp2);
        let probe = probe_pool_keyfile_enrollment(&runner, &[dev1, dev2]);
        assert!(probe.has_enrollment);
        assert_eq!(probe.failures.len(), 1);
        assert_eq!(probe.failures[0].device, "/dev/sda");
    }

    // --- probe_luks_header and guidance helpers ---

    fn is_luks_ok(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupIsLuks {
                device: device.to_owned(),
            },
            RawCommandOutput {
                cmd: format!("cryptsetup isLuks {device}"),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn is_luks_fail(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupIsLuks {
                device: device.to_owned(),
            },
            RawCommandOutput {
                cmd: format!("cryptsetup isLuks {device}"),
                stdout: String::new(),
                stderr: format!("Device {device} is not a valid LUKS device.\n"),
                exit_status: 1,
            },
        )
    }

    fn luks_dump_text_ok(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksDumpText {
                device: device.to_owned(),
            },
            RawCommandOutput {
                cmd: format!("cryptsetup luksDump {device}"),
                stdout: "LUKS header information\nVersion: 2\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn luks_dump_text_fail(device: &str) -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::CryptsetupLuksDumpText {
                device: device.to_owned(),
            },
            RawCommandOutput {
                cmd: format!("cryptsetup luksDump {device}"),
                stdout: String::new(),
                stderr: "Cannot read LUKS header metadata.\n".into(),
                exit_status: 1,
            },
        )
    }

    /*
     * Intent: probe returns Ok when both isLuks and luksDump succeed.
     * Why it exists: this is the happy path — doctor's healthy-disk case and
     *   unlock's "header intact, failure must be passphrase/invariant" case
     *   both depend on this classification being reliable.
     * Scenario: a LUKS2 device with an intact header.
     */
    #[test]
    fn probe_luks_header_ok() {
        let device = "/dev/disk/by-id/wwn-0xOK";
        let (is_req, is_out) = is_luks_ok(device);
        let (dump_req, dump_out) = luks_dump_text_ok(device);
        let runner = MockRunner::default()
            .with_output(is_req, is_out)
            .with_output(dump_req, dump_out);
        assert!(matches!(
            probe_luks_header(&runner, device),
            LuksHeaderState::Ok
        ));
    }

    /*
     * Intent: probe returns Unreadable (and skips luksDump) when isLuks fails.
     * Why it exists: classifying "magic bytes gone" must not cascade into a
     *   second cryptsetup call and must not confuse the Unreadable case with
     *   the ProbeFailed case. The mock for luksDump is deliberately absent —
     *   if probe_luks_header tried to call it, MissingMock would turn the
     *   return into ProbeFailed and this test would fail.
     * Scenario: an HDD whose first sectors were clobbered by a misdirected dd.
     */
    #[test]
    fn probe_luks_header_unreadable_when_is_luks_fails() {
        let device = "/dev/disk/by-id/wwn-0xDEAD";
        let (is_req, is_out) = is_luks_fail(device);
        let runner = MockRunner::default().with_output(is_req, is_out);
        assert!(matches!(
            probe_luks_header(&runner, device),
            LuksHeaderState::Unreadable
        ));
    }

    /*
     * Intent: probe returns Damaged when isLuks passes but luksDump fails.
     * Why it exists: this is the less-severe LUKS2 metadata corruption case
     *   that cryptsetup repair --type luks2 may be able to fix. The test
     *   requires both probes to run in order; it is the only test that
     *   exercises the second probe succeeding after the first.
     * Scenario: a disk with one corrupted LUKS2 header copy (LUKS2 stores two
     *   header copies for redundancy) or damaged keyslot metadata.
     */
    #[test]
    fn probe_luks_header_damaged_when_dump_fails() {
        let device = "/dev/disk/by-id/wwn-0xCAFE";
        let (is_req, is_out) = is_luks_ok(device);
        let (dump_req, dump_out) = luks_dump_text_fail(device);
        let runner = MockRunner::default()
            .with_output(is_req, is_out)
            .with_output(dump_req, dump_out);
        assert!(matches!(
            probe_luks_header(&runner, device),
            LuksHeaderState::Damaged
        ));
    }

    /*
     * Intent: probe returns ProbeFailed (not Unreadable) when the runner
     *   itself errors before cryptsetup can respond.
     * Why it exists: conflating execution failure with header corruption
     *   would tell users to repair or restore a healthy disk. This test
     *   pins the distinction at the probe layer.
     * Scenario: cryptsetup binary missing from PATH, or any IPC failure.
     */
    #[test]
    fn probe_luks_header_probe_failed_on_runner_error() {
        let runner = MockRunner::default();
        let state = probe_luks_header(&runner, "/dev/disk/by-id/wwn-0xGONE");
        match state {
            LuksHeaderState::ProbeFailed(_) => {}
            other => panic!("expected ProbeFailed, got {other:?}"),
        }
    }

    /*
     * Intent: the unreadable guidance text is generic and never references
     *   local /var/lib/braid/luks-headers/ files.
     * Why it exists: this helper is the single source of truth for the
     *   cross-command invariant that doctor, unlock, status, and the TUI
     *   all tell the same story about LUKS header corruption recovery. If
     *   a maintainer ever adds a local-backup-path reference here, every
     *   downstream caller regresses silently.
     * Scenario: code review for any future edit to the helper.
     */
    #[test]
    fn luks_header_unreadable_guidance_is_generic() {
        let msg = luks_header_unreadable_guidance();
        assert!(msg.contains("header unreadable"), "missing phrase: {msg}");
        assert!(msg.contains("off-system"), "missing 'off-system': {msg}");
        assert!(
            msg.contains("luksHeaderRestore"),
            "missing 'luksHeaderRestore': {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "must not reference local .luksheader files: {msg}"
        );
    }

    /*
     * Intent: the damaged guidance text interpolates the device path, pairs
     *   cryptsetup repair with an explicit safe-backup warning, and does
     *   not reference local .luksheader files.
     * Why it exists: cryptsetup repair mutates the header, so any mention
     *   of it must always come with a "back up first" warning (cryptsetup
     *   docs require this). The test also pins the no-local-backup invariant.
     * Scenario: code review for any future edit to the helper.
     */
    #[test]
    fn luks_header_damaged_guidance_interpolates_device_and_warns() {
        let device = "/dev/disk/by-id/wwn-0xCAFE";
        let msg = luks_header_damaged_guidance(device);
        assert!(msg.contains("metadata damaged"), "missing phrase: {msg}");
        assert!(
            msg.contains(&format!("cryptsetup repair --type luks2 {device}")),
            "missing repair command with device: {msg}"
        );
        assert!(
            msg.contains("safe backup"),
            "missing safe-backup warning: {msg}"
        );
        assert!(
            !msg.contains("/var/lib/braid/luks-headers/"),
            "must not reference local backup directory: {msg}"
        );
        assert!(
            !msg.contains(".luksheader"),
            "must not reference local .luksheader files: {msg}"
        );
    }
}
