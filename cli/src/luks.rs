use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::mapper_name;
use crate::probe::Filesystem;
use crate::state_paths::StatePaths;
use crate::types::ByIdPath;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

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

#[derive(Debug, thiserror::Error)]
pub enum LuksError {
    #[error("{0}")]
    Validation(String),
    #[error("cryptsetup open failed for {device} (exit {exit_code}): {hint} — {stderr}")]
    OpenFailed {
        device: String,
        exit_code: i32,
        hint: &'static str,
        stderr: String,
    },
    #[error("cryptsetup luksFormat failed for {device} (exit {exit_code}): {hint} — {stderr}")]
    FormatFailed {
        device: String,
        exit_code: i32,
        hint: &'static str,
        stderr: String,
    },
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Read passphrase from --passphrase-file, --passphrase-stdin, or TTY prompt.
pub fn read_passphrase(
    passphrase_file: Option<&std::path::Path>,
    passphrase_stdin: bool,
) -> Result<String, LuksError> {
    if let Some(path) = passphrase_file {
        let contents = std::fs::read_to_string(path).map_err(|e| {
            LuksError::Validation(format!(
                "failed to read passphrase file {}: {e}",
                path.display()
            ))
        })?;
        // Strip only trailing newline(s), not all whitespace — leading/trailing
        // spaces may be intentional passphrase characters.
        return Ok(contents.trim_end_matches('\n').to_owned());
    }
    if passphrase_stdin {
        let mut buf = String::new();
        std::io::stdin().read_line(&mut buf)?;
        let passphrase = buf.trim_end_matches('\n').trim_end_matches('\r').to_owned();
        if passphrase.is_empty() {
            return Err(LuksError::Validation("passphrase must not be empty".into()));
        }
        return Ok(passphrase);
    }
    prompt_passphrase_tty()
}

fn prompt_passphrase_tty() -> Result<String, LuksError> {
    eprint!("LUKS passphrase: ");
    let passphrase = rpassword::read_password().map_err(|e| {
        LuksError::Validation(format!("failed to read passphrase from terminal: {e}"))
    })?;
    if passphrase.is_empty() {
        return Err(LuksError::Validation("passphrase must not be empty".into()));
    }
    Ok(passphrase)
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

/// Verify passphrase against an existing LUKS device (test-passphrase).
pub fn verify_passphrase<R: CommandRunner>(
    runner: &R,
    device: &str,
    passphrase: &str,
) -> Result<bool, LuksError> {
    let result = runner.run_with_stdin(
        &CmdRequest::CryptsetupTestPassphrase {
            device: device.to_owned(),
        },
        passphrase.as_bytes(),
    )?;
    Ok(result.exit_status == 0)
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

/// Open a LUKS device if not already open.
pub fn ensure_luks_open<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    name: &str,
    by_id: &ByIdPath,
    passphrase: &str,
) -> Result<(), LuksError> {
    let mn = mapper_name(name);
    let mapper_path = format!("/dev/mapper/{}", mn.0);
    if fs.exists(&mapper_path) {
        return Ok(());
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
    Ok(())
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

/// Read LUKS opts from BRAID_LUKS_OPTS env var, split using shell words.
pub fn luks_opts_from_env() -> Vec<String> {
    let raw = std::env::var("BRAID_LUKS_OPTS").unwrap_or_default();
    if raw.is_empty() {
        return vec![];
    }
    shell_words::split(&raw).unwrap_or_default()
}

/// Open a LUKS device with a binary keyfile (no passphrase, no PBKDF).
pub fn ensure_luks_open_with_key_file<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    name: &str,
    by_id: &ByIdPath,
    key_file_path: &std::path::Path,
) -> Result<(), LuksError> {
    let mn = mapper_name(name);
    let mapper_path = format!("/dev/mapper/{}", mn.0);
    if fs.exists(&mapper_path) {
        return Ok(());
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
    Ok(())
}

/// Verify a binary keyfile against an existing LUKS device.
pub fn verify_key_file<R: CommandRunner>(
    runner: &R,
    device: &str,
    key_file_path: &std::path::Path,
) -> Result<bool, LuksError> {
    let result = runner.run(&CmdRequest::CryptsetupTestKeyFile {
        device: device.to_owned(),
        key_file_path: key_file_path.display().to_string(),
    })?;
    Ok(result.exit_status == 0)
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
            "LUKS header backups exist in {} — copy offsite and delete local copies",
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
    use crate::probe::Filesystem;
    use crate::types::ByIdPath;

    struct MockFs {
        paths: Vec<String>,
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
            }
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path.to_string())
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }

        fn read_to_string(&self, _path: &str) -> Result<String, std::io::Error> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
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

    /*
     * Intent: verify ensure_luks_open produces an exit-code-specific error for exit 2.
     * Why: previously all failures said "Wrong passphrase?" regardless of cause.
     * Scenario: cryptsetup open returns exit 2 (EPERM) with diagnostic stderr.
     */
    #[test]
    fn ensure_luks_open_exit_2_mentions_passphrase() {
        let fs = MockFs::new(&[]);
        let runner = MockRunner::default().with_output_stdin(
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
            &fs,
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
        let fs = MockFs::new(&[]);
        let runner = MockRunner::default().with_output_stdin(
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
            &fs,
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
        let fs = MockFs::new(&[]);
        let kf = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(kf.path(), b"badkey").unwrap();
        let runner = MockRunner::default().with_output(
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
            &fs,
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
        let fs = MockFs::new(&[]);
        let kf = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(kf.path(), b"key").unwrap();
        let runner = MockRunner::default().with_output(
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
            &fs,
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
}
