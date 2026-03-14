use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{DiskConfig, mapper_name};
use crate::probe::Filesystem;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub(crate) const HEADER_BACKUP_DIR: &str = "/var/lib/braid/luks-headers";

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
        return Err(LuksError::Validation(format!(
            "cryptsetup luksFormat failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim()
        )));
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
    let result = runner.run(&CmdRequest::CryptsetupLuksHeaderBackup {
        device: device.to_owned(),
        backup_path: backup_path.display().to_string(),
    })?;
    if result.exit_status != 0 {
        return Err(LuksError::Validation(format!(
            "LUKS header backup failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim()
        )));
    }
    // cryptsetup already creates the file as 0400, but enforce it ourselves for defense-in-depth
    std::fs::set_permissions(&backup_path, std::fs::Permissions::from_mode(0o400))?;

    // Clean up old .img backup if it exists (migration from .img → .luksheader)
    let old_path = dir.join(format!("{mapper}.img"));
    if old_path.exists() {
        let _ = std::fs::remove_file(&old_path);
    }

    Ok(backup_path)
}

/// Back up the LUKS header to /var/lib/braid/luks-headers/<mapper>.luksheader
pub fn backup_luks_header<R: CommandRunner>(
    runner: &R,
    device: &str,
    mapper: &str,
) -> Result<PathBuf, LuksError> {
    backup_luks_header_to(
        runner,
        device,
        mapper,
        std::path::Path::new(HEADER_BACKUP_DIR),
    )
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

/// Open a LUKS device if not already open.
pub fn ensure_luks_open<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    name: &str,
    disk: &DiskConfig,
    passphrase: &str,
) -> Result<(), LuksError> {
    let mn = mapper_name(name);
    let mapper_path = format!("/dev/mapper/{}", mn.0);
    if fs.exists(&mapper_path) {
        return Ok(());
    }

    let result = runner.run_with_stdin(
        &CmdRequest::CryptsetupLuksOpen {
            device: disk.by_id.0.clone(),
            mapper: mn.0.clone(),
        },
        passphrase.as_bytes(),
    )?;
    if result.exit_status != 0 {
        return Err(LuksError::Validation(format!(
            "failed to open LUKS device {}. Wrong passphrase?",
            disk.by_id
        )));
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
    disk: &DiskConfig,
    key_file_path: &std::path::Path,
) -> Result<(), LuksError> {
    let mn = mapper_name(name);
    let mapper_path = format!("/dev/mapper/{}", mn.0);
    if fs.exists(&mapper_path) {
        return Ok(());
    }

    let result = runner.run(&CmdRequest::CryptsetupLuksOpenKeyFile {
        device: disk.by_id.0.clone(),
        mapper: mn.0.clone(),
        key_file_path: key_file_path.display().to_string(),
    })?;
    if result.exit_status != 0 {
        return Err(LuksError::Validation(format!(
            "failed to open LUKS device {} with keyfile. Wrong keyfile?",
            disk.by_id
        )));
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
                .map_or(false, |ext| ext == "luksheader" || ext == "img")
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

/// Production wrapper — scans HEADER_BACKUP_DIR.
pub fn header_backup_advisories() -> Vec<String> {
    header_backup_advisories_in(std::path::Path::new(HEADER_BACKUP_DIR))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
