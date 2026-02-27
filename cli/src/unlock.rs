use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{mapper_name, Config, ConfigError};
use crate::luks::{self, LuksError};
use crate::pool::PoolError;
use crate::probe::{self, Filesystem, ProbeError};
use crate::types::ConfigDiskState;

#[derive(Debug, thiserror::Error)]
pub enum UnlockError {
    #[error("{0}")]
    Probe(#[from] ProbeError),
    #[error("{0}")]
    Luks(#[from] LuksError),
    #[error("{0}")]
    Pool(#[from] PoolError),
    #[error("{0}")]
    Config(#[from] ConfigError),
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Failed(String),
}

/// Status line tag for output.
fn tag(label: &str) -> String {
    format!("[{:<4}]", label)
}

pub fn cmd_unlock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    passphrase_stdin: bool,
    passphrase_file: Option<&std::path::Path>,
    key_file: Option<&std::path::Path>,
) -> Result<(), UnlockError> {
    let mount_point = config.mount_point();

    // 1. If pool already mounted → print message, exit 0
    let mp_result = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.to_owned(),
    })?;
    if mp_result.exit_status == 0 {
        eprintln!("pool already mounted at {mount_point}");
        return Ok(());
    }

    // 2. Probe each config disk
    let mut to_unlock = Vec::new(); // (key, disk) pairs needing unlock
    let mut any_open = false;
    let mut any_absent = false;
    let mut any_not_luks = false;

    for (key, disk) in config.disks() {
        let probed = probe::probe_config_disk(runner, fs, key, disk)?;
        match &probed.state {
            ConfigDiskState::Absent => {
                eprintln!("{}  disk: {:<10}not found (unplugged?)", tag("skip"), key);
                any_absent = true;
            }
            ConfigDiskState::PresentNotLuks => {
                eprintln!(
                    "{}  disk: {:<10}not initialized, run `braid add {}`",
                    tag("skip"),
                    key,
                    key
                );
                any_not_luks = true;
            }
            ConfigDiskState::PresentLuks {
                mapper_open: true, ..
            } => {
                eprintln!("{}  disk: {:<10}already open", tag("ok"), key);
                any_open = true;
            }
            ConfigDiskState::PresentLuks {
                mapper_open: false, ..
            } => {
                eprintln!("{}  disk: {:<10}found", tag("ok"), key);
                to_unlock.push((key.clone(), disk.clone()));
            }
        }
    }

    // 3. If no disks to unlock AND none already open → error
    if to_unlock.is_empty() && !any_open {
        if any_not_luks {
            return Err(UnlockError::Failed(
                "no unlockable disks found; some disks are not initialized (run `braid add`)"
                    .into(),
            ));
        }
        return Err(UnlockError::Failed("no unlockable disks found".into()));
    }

    // 4. If disks need opening → verify credential, then open each disk
    if !to_unlock.is_empty() {
        if let Some(kf) = key_file {
            // Keyfile path: verify against first disk, then open each
            let (ref first_key, ref first_disk) = to_unlock[0];
            let ok = luks::verify_key_file(runner, &first_disk.by_id.0, kf)?;
            if !ok {
                return Err(UnlockError::Failed(format!(
                    "wrong keyfile (verified against {})",
                    first_key
                )));
            }

            for (key, disk) in &to_unlock {
                luks::ensure_luks_open_with_key_file(runner, fs, key, disk, kf)?;
                eprintln!("{}  disk: {:<10}unlocked", tag("ok"), key);
            }
        } else {
            // Passphrase path (unchanged)
            let passphrase = luks::read_passphrase(passphrase_file, passphrase_stdin)?;

            let (ref first_key, ref first_disk) = to_unlock[0];
            let ok = luks::verify_passphrase(runner, &first_disk.by_id.0, &passphrase)?;
            if !ok {
                return Err(UnlockError::Failed(format!(
                    "wrong passphrase (verified against {})",
                    first_key
                )));
            }

            for (key, disk) in &to_unlock {
                luks::ensure_luks_open(runner, fs, key, disk, &passphrase)?;
                eprintln!("{}  disk: {:<10}unlocked", tag("ok"), key);
            }
        }
    }

    // 5. btrfs device scan
    let scan = runner.run(&CmdRequest::BtrfsDeviceScanAll)?;
    if scan.exit_status != 0 {
        return Err(UnlockError::Failed(format!(
            "btrfs device scan failed (exit {}): {}",
            scan.exit_status,
            scan.stderr.trim()
        )));
    }

    // 6. mkdir -p mount_point, then mount
    let _ = std::fs::create_dir_all(mount_point);

    let mount_result = if any_absent {
        // Some disks missing → degraded mount
        runner.run(&CmdRequest::MountWithOptions {
            device: format!(
                "/dev/mapper/{}",
                mapper_name(
                    &to_unlock
                        .first()
                        .map(|(k, _)| k.as_str())
                        .or_else(|| {
                            // All disks were already open — find first open mapper
                            config.disks().keys().next().map(|k| k.as_str())
                        })
                        .unwrap_or("unknown")
                )
                .0
            ),
            mount_point: mount_point.to_owned(),
            options: vec!["degraded".to_owned()],
        })?
    } else {
        // All disks present — find a mapper device to mount from
        let mount_key = to_unlock
            .first()
            .map(|(k, _)| k.as_str())
            .or_else(|| config.disks().keys().next().map(|k| k.as_str()))
            .unwrap_or("unknown");
        runner.run(&CmdRequest::Mount {
            device: format!("/dev/mapper/{}", mapper_name(mount_key).0),
            mount_point: mount_point.to_owned(),
        })?
    };

    if mount_result.exit_status != 0 {
        return Err(UnlockError::Failed(format!(
            "mount failed (exit {}): {}",
            mount_result.exit_status,
            mount_result.stderr.trim()
        )));
    }

    eprintln!("{}  {:<10}mounted {}", tag("ok"), "pool", mount_point);
    Ok(())
}
