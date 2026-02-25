use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{DiskConfig, mapper_name};
use crate::probe::Filesystem;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

pub(crate) const HEADER_BACKUP_DIR: &str = "/var/lib/braid/luks-headers";

#[derive(Debug, thiserror::Error)]
pub enum LuksError {
    #[error("{0}")]
    Validation(String),
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Read passphrase from --passphrase-file, BRAID_PASSPHRASE env, or TTY prompt.
pub fn read_passphrase(
    passphrase_file: Option<&std::path::Path>,
    yes: bool,
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
    if yes {
        return std::env::var("BRAID_PASSPHRASE").map_err(|_| {
            LuksError::Validation(
                "--yes requires BRAID_PASSPHRASE env var or --passphrase-file".to_owned(),
            )
        });
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

/// Back up the LUKS header to /var/lib/braid/luks-headers/<mapper>.img
pub fn backup_luks_header<R: CommandRunner>(
    runner: &R,
    device: &str,
    mapper: &str,
) -> Result<PathBuf, LuksError> {
    let dir = std::path::Path::new(HEADER_BACKUP_DIR);
    if !dir.exists() {
        std::fs::create_dir_all(dir)?;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }

    let backup_path = dir.join(format!("{mapper}.img"));
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
    Ok(backup_path)
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
