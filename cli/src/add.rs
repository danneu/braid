use crate::cmd::CommandRunner;
use crate::config::{config_read, mapper_name, Config, DiskConfig};
use crate::disk_map;
use crate::luks::{
    backup_luks_header, device_has_btrfs_superblock, ensure_luks_open, luks_format,
    luks_opts_from_env, read_passphrase, verify_passphrase,
};
use crate::pool::{
    pool_add_device, pool_balance_raid1, pool_bootstrap_mount, pool_bootstrap_mount_raid1,
};
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
    names: &[String],
    dry_run: bool,
    yes: bool,
    passphrase_stdin: bool,
    passphrase_file: Option<&Path>,
    enroll_key_file: Option<&Path>,
    progress: ProgressOutput,
) -> Result<(), AddError> {
    let config = config_read(config_path)?;
    let disk_map_state = disk_map::load_disk_map();
    disk_map::validate_config_name_stability(&config, &disk_map_state)
        .map_err(|e| AddError::Validation(e.to_string()))?;

    // Reject duplicate names upfront
    {
        let mut seen = std::collections::HashSet::new();
        for name in names {
            if !seen.insert(name.as_str()) {
                return Err(AddError::Validation(format!(
                    "duplicate disk name: '{name}'"
                )));
            }
        }
    }

    // Validate all names exist in config
    let disks: Vec<(&str, &DiskConfig)> = names
        .iter()
        .map(|name| {
            let disk = config.disk_by_name(name).ok_or_else(|| {
                let available: Vec<_> = config.names().into_iter().map(|s| s.as_str()).collect();
                AddError::Validation(format!(
                    "disk '{}' not found in config. Available: {}",
                    name,
                    available.join(", ")
                ))
            })?;
            Ok::<_, AddError>((name.as_str(), disk))
        })
        .collect::<Result<Vec<_>, _>>()?;

    // Probe all disks — fail early if any absent
    let probed: Vec<ConfigDisk> = disks
        .iter()
        .map(|(name, disk)| probe_config_disk(runner, fs, name, disk))
        .collect::<Result<Vec<_>, _>>()?;

    for (i, p) in probed.iter().enumerate() {
        if matches!(p.state, ConfigDiskState::Absent) {
            return Err(AddError::Validation(format!(
                "disk '{}' ({}) is not present. Is it plugged in?",
                names[i], disks[i].1.by_id
            )));
        }
    }

    // Probe pool + preflight (once)
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

    // Compile steps for dry-run display
    let steps = compile_add_steps_multi(names, &probed, &pool, &config)?;

    if dry_run {
        for step in &steps {
            println!("[{:<11}] {}", step.risk, step.description);
        }
        return Ok(());
    }

    if steps.is_empty() {
        let label = if names.len() == 1 {
            names[0].clone()
        } else {
            names.join(", ")
        };
        eprintln!("Nothing to do — {} already in pool.", label);
        return Ok(());
    }

    // Read passphrase (once)
    let passphrase = read_passphrase(passphrase_file, passphrase_stdin)?;

    // Confirmation — collect all disks that need LUKS format
    let needs_format: Vec<(&str, &str)> = probed
        .iter()
        .enumerate()
        .filter(|(_, p)| matches!(p.state, ConfigDiskState::PresentNotLuks))
        .map(|(i, _)| (names[i].as_str(), disks[i].1.by_id.0.as_str()))
        .collect();

    if !needs_format.is_empty() && !yes {
        eprintln!("{}", add_confirm_message_multi(&needs_format));
        eprint!("Type 'yes' to continue: ");
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| AddError::Validation(format!("failed to read confirmation: {e}")))?;
        if input.trim() != "yes" {
            return Err(AddError::Validation("aborted by user".into()));
        }
    }

    // Verify passphrase against existing pool member (once)
    if !needs_format.is_empty() {
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
    }

    // LUKS phase — for each disk: format/open as needed. Track which need pool add.
    let mut needs_pool_add: Vec<usize> = Vec::new();

    for (i, p) in probed.iter().enumerate() {
        let name = &names[i];
        let disk = disks[i].1;
        let mn = mapper_name(name);

        match &p.state {
            ConfigDiskState::Absent => unreachable!("already checked above"),
            ConfigDiskState::PresentNotLuks => {
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

                needs_pool_add.push(i);
            }
            ConfigDiskState::PresentLuks { mapper_open, .. } => {
                if !mapper_open {
                    ensure_luks_open(runner, fs, name, disk, &passphrase)?;
                    eprintln!("LUKS opened: {} → {}", disk.by_id, mn);
                }

                // Check btrfs membership
                let mapper_path = format!("/dev/mapper/{}", mn.0);
                if device_has_btrfs_superblock(runner, &mapper_path)? {
                    if pool.devices.iter().any(|d| d.mapper == mn) {
                        // Already in pool — skip
                        continue;
                    }
                }

                if !pool.devices.iter().any(|d| d.mapper == mn) {
                    eprintln!("note: LUKS mapper for {} is already open but device is not yet in pool. Completing add.", name);
                }

                needs_pool_add.push(i);
            }
        }
    }

    if needs_pool_add.is_empty() {
        let label = if names.len() == 1 {
            names[0].clone()
        } else {
            names.join(", ")
        };
        eprintln!("Nothing to do — {} already in pool.", label);
        return Ok(());
    }

    // Pool phase
    let mapper_paths: Vec<String> = needs_pool_add
        .iter()
        .map(|&i| format!("/dev/mapper/{}", mapper_name(&names[i]).0))
        .collect();

    if !pool.mounted {
        if mapper_paths.len() >= 2 {
            // Check if ALL target mappers are fresh (no btrfs superblock)
            let mut any_has_superblock = false;
            for mp in &mapper_paths {
                if device_has_btrfs_superblock(runner, mp)? {
                    any_has_superblock = true;
                    break;
                }
            }

            if any_has_superblock {
                return Err(AddError::Validation(
                    "pool is not mounted but some target devices have existing btrfs data. \
                     Run `braid unlock` first to bring the pool online, then add disks."
                        .into(),
                ));
            }

            // All fresh — bootstrap with mkfs.btrfs RAID1
            pool_bootstrap_mount_raid1(runner, &mapper_paths, config.mount_point())?;
            eprintln!(
                "Pool created (RAID1) and mounted at {}",
                config.mount_point()
            );
        } else {
            // Single disk bootstrap
            pool_bootstrap_mount(runner, &mapper_paths[0], config.mount_point())?;
            eprintln!("Pool created and mounted at {}", config.mount_point());
        }
    } else {
        // Add each to existing pool
        for mp in &mapper_paths {
            pool_add_device(runner, mp, config.mount_point())?;
            eprintln!("Device added to pool: {}", mp);
        }

        // Balance to RAID1 if total >= 2
        let total_after = pool.devices.len() + mapper_paths.len();
        if total_after >= 2 {
            eprintln!("Balancing to RAID1...");
            pool_balance_raid1(runner, config.mount_point(), progress)?;
            eprintln!("Balance complete.");
        }
    }

    // Finalize disk map for each added disk
    for &i in &needs_pool_add {
        finalize_add_disk_map_best_effort(
            runner,
            config.mount_point(),
            &names[i],
            &disks[i].1.by_id.0,
        );
    }

    let label = if names.len() == 1 {
        format!("{} is", names[0])
    } else {
        format!("{} are", names.join(", "))
    };
    eprintln!("Done. {} now part of the pool.", label);
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

fn compile_add_steps_multi(
    names: &[String],
    probed: &[ConfigDisk],
    pool: &PoolState,
    config: &Config,
) -> Result<Vec<AddStep>, AddError> {
    let mut steps = Vec::new();
    let mut needs_pool_add = 0usize;

    for (i, p) in probed.iter().enumerate() {
        let name = &names[i];
        let mn = mapper_name(name);
        let disk = config.disk_by_name(name).unwrap();

        match &p.state {
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
                needs_pool_add += 1;
            }
            ConfigDiskState::PresentLuks { mapper_open, .. } => {
                if !mapper_open {
                    steps.push(AddStep {
                        risk: "safe",
                        description: format!("LUKS open → {}", mn),
                    });
                }

                // If already in pool, skip
                if *mapper_open && pool.devices.iter().any(|d| d.mapper == mn) {
                    continue;
                }
                needs_pool_add += 1;
            }
        }
    }

    if needs_pool_add == 0 {
        return Ok(vec![]);
    }

    if !pool.mounted {
        if needs_pool_add >= 2 {
            let mapper_list: Vec<String> = names
                .iter()
                .map(|n| format!("/dev/mapper/{}", mapper_name(n).0))
                .collect();
            steps.push(AddStep {
                risk: "safe",
                description: format!("mkfs.btrfs RAID1 {}", mapper_list.join(" ")),
            });
        } else {
            // Single disk — find the one that needs pool add
            for (i, p) in probed.iter().enumerate() {
                let mn = mapper_name(&names[i]);
                let skip = matches!(&p.state, ConfigDiskState::PresentLuks { mapper_open, .. } if *mapper_open && pool.devices.iter().any(|d| d.mapper == mn));
                if !skip {
                    steps.push(AddStep {
                        risk: "safe",
                        description: format!("mkfs.btrfs /dev/mapper/{}", mn),
                    });
                    break;
                }
            }
        }
        steps.push(AddStep {
            risk: "safe",
            description: format!("mount → {}", config.mount_point()),
        });
    } else {
        for (i, p) in probed.iter().enumerate() {
            let mn = mapper_name(&names[i]);
            let skip = matches!(&p.state, ConfigDiskState::PresentLuks { mapper_open, .. } if *mapper_open && pool.devices.iter().any(|d| d.mapper == mn));
            if !skip {
                steps.push(AddStep {
                    risk: "safe",
                    description: format!(
                        "btrfs device add /dev/mapper/{} {}",
                        mn,
                        config.mount_point()
                    ),
                });
            }
        }
        let total_after = pool.devices.len() + needs_pool_add;
        if total_after >= 2 {
            steps.push(AddStep {
                risk: "long",
                description: "btrfs balance to RAID1".into(),
            });
        }
    }

    Ok(steps)
}

fn add_confirm_message_multi(disks: &[(&str, &str)]) -> String {
    if disks.len() == 1 {
        return format!(
            "WARNING: This will LUKS-format {} ({}). Existing data will be inaccessible.",
            disks[0].0, disks[0].1
        );
    }
    let mut msg =
        "WARNING: This will LUKS-format the following disks. Existing data will be inaccessible.\n"
            .to_string();
    for (name, by_id) in disks {
        msg.push_str(&format!("  - {} ({})\n", name, by_id));
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn add_confirm_message_single_disk() {
        let msg = add_confirm_message_multi(&[("data1", "/dev/disk/by-id/usb-WD_1234")]);
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

    #[test]
    fn add_confirm_message_multi_disk() {
        let msg = add_confirm_message_multi(&[
            ("toshiba", "/dev/disk/by-id/ata-Toshiba"),
            ("ironwolf", "/dev/disk/by-id/ata-Ironwolf"),
        ]);
        assert!(msg.contains("LUKS-format"), "should mention LUKS-format");
        assert!(msg.contains("toshiba"), "should mention first disk");
        assert!(msg.contains("ironwolf"), "should mention second disk");
        assert!(msg.contains("inaccessible"), "should warn about data loss");
    }

    #[test]
    fn duplicate_name_rejected() {
        use crate::cmd::MockRunner;
        use crate::probe::Filesystem;
        use std::io::Write;

        struct MockFs;
        impl Filesystem for MockFs {
            fn exists(&self, _path: &str) -> bool {
                false
            }
            fn is_block_device(&self, _path: &str) -> bool {
                false
            }
        }

        // Write a temp config
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let mut f = std::fs::File::create(&config_path).unwrap();
        write!(
            f,
            r#"{{"disks":{{"d1":{{"by_id":"/dev/sda"}}}},"mount_point":"/mnt/storage"}}"#
        )
        .unwrap();

        let runner = MockRunner::default();
        let fs = MockFs;

        let result = cmd_add(
            &runner,
            &fs,
            &config_path,
            &["d1".into(), "d1".into()],
            true,
            true,
            false,
            None,
            None,
            ProgressOutput::Off,
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("duplicate disk name"),
            "expected duplicate error, got: {err}"
        );
    }
}
