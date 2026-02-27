use crate::cmd::CommandRunner;
use crate::config::{config_read_raw, mapper_name, Config};
use crate::disk_map;
use crate::luks::{
    backup_luks_header, device_has_btrfs_superblock, ensure_luks_open, luks_format,
    luks_opts_from_env, read_passphrase, verify_passphrase,
};
use crate::pool::{pool_add_device, pool_balance_raid1, pool_bootstrap_mount};
use crate::preflight;
use crate::probe::{probe_config_disk, probe_pool, Filesystem, ProbeError};
use crate::progress::ProgressOutput;
use crate::types::*;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error("{0}")]
    Validation(String),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("luks error: {0}")]
    Luks(#[from] crate::luks::LuksError),
    #[error("pool error: {0}")]
    Pool(#[from] crate::pool::PoolError),
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] crate::parse::ParseError),
}

/// A step in the add operation, for dry-run display.
pub struct AddStep {
    pub risk: &'static str, // "destructive", "safe", "long"
    pub description: String,
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_add<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config_path: &Path,
    name: &str,
    dry_run: bool,
    yes: bool,
    passphrase_stdin: bool,
    passphrase_file: Option<&Path>,
    enroll_key_file: Option<&Path>,
    progress: ProgressOutput,
) -> Result<(), AddError> {
    let (config, _config_raw) = config_read_raw(config_path)?;
    let disk_map_state = disk_map::load_disk_map();
    disk_map::validate_config_name_stability(&config, &disk_map_state)
        .map_err(|e| AddError::Validation(e.to_string()))?;

    let disk = config.disk_by_name(name).ok_or_else(|| {
        let available: Vec<_> = config.names().into_iter().map(|s| s.as_str()).collect();
        AddError::Validation(format!(
            "disk '{}' not found in config. Available: {}",
            name,
            available.join(", ")
        ))
    })?;

    let probed = probe_config_disk(runner, fs, name, disk)?;
    let pool = match probe_pool(runner, config.mount_point()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { fstype, .. }) => {
            return Err(AddError::Validation(format!(
                "{} is already mounted with {fstype}, not btrfs. Unmount it first.",
                config.mount_point()
            )));
        }
        Err(e) => return Err(AddError::Probe(e)),
    };

    // Preflight
    if pool.mounted {
        preflight::check_no_exclusive_op(runner, config.mount_point())
            .map_err(AddError::Validation)?;
        preflight::check_not_read_only(runner, config.mount_point())
            .map_err(AddError::Validation)?;
    }
    if pool.missing_count > 0 {
        eprintln!(
            "warning: pool has {} missing device{}. Consider running `braid remove-missing` first.",
            pool.missing_count,
            if pool.missing_count == 1 { "" } else { "s" }
        );
    }

    // Compile steps based on actual disk state
    let steps = compile_add_steps(name, &probed, &pool, &config)?;

    if dry_run {
        for step in &steps {
            println!("[{:<11}] {}", step.risk, step.description);
        }
        return Ok(());
    }

    if steps.is_empty() {
        eprintln!("Nothing to do — {} is already a pool member.", name);
        return Ok(());
    }

    // Read passphrase
    let passphrase = read_passphrase(passphrase_file, passphrase_stdin)?;
    let mn = mapper_name(name);

    // Execute steps
    match probed.state {
        ConfigDiskState::Absent => {
            return Err(AddError::Validation(format!(
                "disk '{}' ({}) is not present. Is it plugged in?",
                name, disk.by_id
            )));
        }
        ConfigDiskState::PresentNotLuks => {
            // Fresh disk — LUKS format
            if !yes {
                eprintln!("{}", add_confirm_message(name, &disk.by_id.0));
                eprint!("Type 'yes' to continue: ");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input).map_err(|e| {
                    AddError::Validation(format!("failed to read confirmation: {e}"))
                })?;
                if input.trim() != "yes" {
                    return Err(AddError::Validation("aborted by user".into()));
                }
            }

            // If pool exists, verify passphrase against existing member
            if let Some(existing) = pool.devices.first() {
                let status_raw = runner.run(&crate::cmd::CmdRequest::CryptsetupStatus {
                    mapper: existing.mapper.0.clone(),
                })?;
                let status = crate::parse::parse_cryptsetup_status(&status_raw)?;
                if let Some(underlying) = status.device {
                    let ok = verify_passphrase(runner, &underlying, &passphrase)?;
                    if !ok {
                        return Err(AddError::Validation(
                            "passphrase does not match existing pool member. All disks must use the same passphrase."
                                .into(),
                        ));
                    }
                }
            }

            let mut luks_opts = luks_opts_from_env();
            luks_opts.push("--label".into());
            luks_opts.push(format!("braid-{name}"));
            luks_format(runner, &disk.by_id.0, &passphrase, &luks_opts)?;
            eprintln!("LUKS formatted: {}", disk.by_id);

            let backup_path = backup_luks_header(runner, &disk.by_id.0, &mn.0)?;
            eprintln!("LUKS header backed up: {}", backup_path.display());

            ensure_luks_open(runner, fs, name, disk, &passphrase)?;
            eprintln!("LUKS opened: {} → {}", disk.by_id, mn);

            if let Some(kf) = enroll_key_file {
                crate::luks::enroll_key_file(runner, &disk.by_id.0, &passphrase, kf)?;
                eprintln!("Keyfile enrolled in slot 1: {}", disk.by_id);
            }
        }
        ConfigDiskState::PresentLuks { mapper_open, .. } => {
            if !mapper_open {
                ensure_luks_open(runner, fs, name, disk, &passphrase)?;
                eprintln!("LUKS opened: {} → {}", disk.by_id, mn);
            }

            // Check btrfs membership
            let mapper_path = format!("/dev/mapper/{}", mn.0);
            if device_has_btrfs_superblock(runner, &mapper_path)? {
                // Check if already in this pool
                if pool.devices.iter().any(|d| d.mapper == mn) {
                    eprintln!("Already a pool member. Nothing to do.");
                    return Ok(());
                }
            }

            if !pool.devices.iter().any(|d| d.mapper == mn) {
                eprintln!("note: LUKS mapper is already open but device is not yet in pool. Completing add.");
            }
        }
    }

    // Pool operations
    let mapper_path = format!("/dev/mapper/{}", mn.0);
    if !pool.mounted {
        // Bootstrap — first disk
        pool_bootstrap_mount(runner, &mapper_path, config.mount_point())?;
        eprintln!("Pool created and mounted at {}", config.mount_point());
    } else {
        // Add to existing pool
        pool_add_device(runner, &mapper_path, config.mount_point())?;
        eprintln!("Device added to pool: {}", mn);

        // Balance to RAID1 if 2+ disks
        let total_after = pool.devices.len() + 1;
        if total_after >= 2 {
            eprintln!("Balancing to RAID1...");
            pool_balance_raid1(runner, config.mount_point(), progress)?;
            eprintln!("Balance complete.");
        }
    }

    finalize_add_disk_map_best_effort(runner, config.mount_point(), name, &disk.by_id.0);

    eprintln!("Done. {} is now part of the pool.", name);
    Ok(())
}

fn finalize_add_disk_map_best_effort<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &str,
    name: &str,
    by_id: &str,
) {
    // Best effort only: never fail add due to disk-map write issues.
    if let Ok(pool_after) = probe_pool(runner, mount_point) {
        let mn = mapper_name(name);
        if let Some(dev) = pool_after.devices.iter().find(|d| d.mapper == mn) {
            disk_map::update_disk_map_best_effort(|map| {
                disk_map::record_disk(map, name, by_id, &dev.luks_uuid.0, dev.devid);
            });
        }
    }
}

fn compile_add_steps(
    name: &str,
    probed: &ConfigDisk,
    pool: &PoolState,
    config: &Config,
) -> Result<Vec<AddStep>, AddError> {
    let mn = mapper_name(name);
    let disk = config.disk_by_name(name).unwrap();
    let mut steps = Vec::new();

    match &probed.state {
        ConfigDiskState::Absent => {
            return Err(AddError::Validation(format!(
                "disk '{}' ({}) is not present. Is it plugged in?",
                name, disk.by_id
            )));
        }
        ConfigDiskState::PresentNotLuks => {
            steps.push(AddStep {
                risk: "destructive",
                description: format!("LUKS format {}", disk.by_id),
            });
            steps.push(AddStep {
                risk: "safe",
                description: format!("LUKS open → {}", mn),
            });
        }
        ConfigDiskState::PresentLuks { mapper_open, .. } => {
            if !mapper_open {
                steps.push(AddStep {
                    risk: "safe",
                    description: format!("LUKS open → {}", mn),
                });
            }

            // If already in pool, no-op
            if *mapper_open && pool.devices.iter().any(|d| d.mapper == mn) {
                return Ok(vec![]);
            }
        }
    }

    if !pool.mounted {
        steps.push(AddStep {
            risk: "safe",
            description: format!("mkfs.btrfs /dev/mapper/{}", mn),
        });
        steps.push(AddStep {
            risk: "safe",
            description: format!("mount → {}", config.mount_point()),
        });
    } else {
        steps.push(AddStep {
            risk: "safe",
            description: format!(
                "btrfs device add /dev/mapper/{} {}",
                mn,
                config.mount_point()
            ),
        });
        let total_after = pool.devices.len() + 1;
        if total_after >= 2 {
            steps.push(AddStep {
                risk: "long",
                description: "btrfs balance to RAID1".into(),
            });
        }
    }

    Ok(steps)
}

fn add_confirm_message(name: &str, by_id: &str) -> String {
    format!(
        "WARNING: This will LUKS-format {} ({}). Existing data will be inaccessible.",
        name, by_id
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_confirm_message_warns_about_luks_format() {
        let msg = add_confirm_message("data1", "/dev/disk/by-id/usb-WD_1234");
        assert!(msg.contains("LUKS-format"), "should mention LUKS-format");
        assert!(msg.contains("data1"), "should mention disk name");
        assert!(
            msg.contains("/dev/disk/by-id/usb-WD_1234"),
            "should mention by-id"
        );
        assert!(
            msg.contains("inaccessible"),
            "should say data will be inaccessible"
        );
        assert!(
            !msg.contains("DESTROY"),
            "should not use inaccurate 'DESTROY' wording"
        );
    }
}
