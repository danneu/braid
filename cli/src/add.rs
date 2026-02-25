use crate::checkpoint::{
    CheckpointError, InvocationCtx, LiveCtx, OpArgs, OpKind, Phase, PoolFingerprint, ResumeGate,
    SystemClock, TargetSnapshot, clear_checkpoint, hash_args, maybe_fail_after_checkpoint,
    new_checkpoint, resolve_resume_gate, run_phase_hooks, save_checkpoint_atomic,
};
use crate::cmd::CommandRunner;
use crate::config::{Config, config_hash, config_read_raw, mapper_name};
use crate::disk_map;
use crate::luks::{
    backup_luks_header, device_has_btrfs_superblock, ensure_luks_open, luks_format,
    luks_opts_from_env, read_passphrase, verify_passphrase,
};
use crate::pool::{pool_add_device, pool_balance_raid1, pool_bootstrap_mount};
use crate::probe::{Filesystem, ProbeError, probe_config_disk, probe_pool};
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
    #[error("checkpoint error: {0}")]
    Checkpoint(#[from] CheckpointError),
    #[error("checkpoint IO error: {0}")]
    CheckpointIo(#[from] std::io::Error),
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
    passphrase_file: Option<&Path>,
    progress: ProgressOutput,
) -> Result<(), AddError> {
    let (config, config_raw) = config_read_raw(config_path)?;
    let disk_map_state = disk_map::load_disk_map();
    disk_map::validate_config_key_stability(&config, &disk_map_state)
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
        Err(ProbeError::NotBtrfs { .. }) => PoolState {
            mounted: false,
            devices: vec![],
            missing_count: 0,
            total_devices: 0,
        },
        Err(e) => return Err(AddError::Probe(e)),
    };

    // Compile steps based on actual disk state
    let steps = compile_add_steps(name, &probed, &pool, &config)?;

    if dry_run {
        for step in &steps {
            println!("[{:<11}] {}", step.risk, step.description);
        }
        return Ok(());
    }

    // Resolve checkpoint before any mutating requests.
    let args_hash = hash_args(&["add", name]);
    let resume = match resolve_resume_gate(
        &config_raw,
        InvocationCtx {
            op: OpKind::Add,
            op_args: OpArgs::add(name),
            args_hash: args_hash.clone(),
            config_hash: config_hash(&config_raw),
        },
        LiveCtx {
            pool_fingerprint: PoolFingerprint::from_pool_state(&pool),
            primary_target_available: !matches!(probed.state, ConfigDiskState::Absent),
            secondary_target_available: None,
        },
    ) {
        ResumeGate::ResumeFrom(cp) => {
            eprintln!(
                "Resuming previous 'braid add {}' at phase {}.",
                name,
                cp.phase.as_env_value()
            );
            Some(cp)
        }
        ResumeGate::NoCheckpoint => None,
        ResumeGate::Reject(error) => return Err(AddError::Checkpoint(error)),
    };

    // Resume from long-running phase directly.
    if matches!(
        resume.as_ref().map(|cp| &cp.phase),
        Some(Phase::AddBalanceRaid1)
    ) {
        run_phase_hooks(&Phase::AddBalanceRaid1)?;
        eprintln!("Balancing to RAID1...");
        pool_balance_raid1(runner, config.mount_point(), progress)?;
        clear_checkpoint();
        eprintln!("Balance complete.");
        eprintln!("Done. {} is now part of the pool.", name);
        return Ok(());
    }

    if steps.is_empty() {
        eprintln!("Nothing to do — {} is already a pool member.", name);
        return Ok(());
    }

    // Read passphrase
    let passphrase = read_passphrase(passphrase_file, yes)?;
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

            let luks_opts = luks_opts_from_env();
            luks_format(runner, &disk.by_id.0, &passphrase, &luks_opts)?;
            eprintln!("LUKS formatted: {}", disk.by_id);

            let backup_path = backup_luks_header(runner, &disk.by_id.0, &mn.0)?;
            eprintln!("LUKS header backed up: {}", backup_path.display());

            ensure_luks_open(runner, fs, name, disk, &passphrase)?;
            eprintln!("LUKS opened: {} → {}", disk.by_id, mn);
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

        // Re-probe after device add so checkpoint fingerprint matches live resume topology.
        let pool_after_add = probe_pool(runner, config.mount_point())?;

        // Balance to RAID1 if 2+ disks
        let total_after = pool.devices.len() + 1;
        if total_after >= 2 {
            let cp = new_checkpoint(
                &SystemClock,
                OpKind::Add,
                OpArgs::add(name),
                Phase::AddBalanceRaid1,
                config_hash(&config_raw),
                hash_args(&["add", name]),
                PoolFingerprint::from_pool_state(&pool_after_add),
                TargetSnapshot {
                    primary: Some(name.to_owned()),
                    secondary: None,
                    missing_id: None,
                },
            );
            save_checkpoint_atomic(&cp)?;
            maybe_fail_after_checkpoint()?;

            run_phase_hooks(&Phase::AddBalanceRaid1)?;
            eprintln!("Balancing to RAID1...");
            pool_balance_raid1(runner, config.mount_point(), progress)?;
            clear_checkpoint();
            eprintln!("Balance complete.");
        }
    }

    // Update disk map (best effort — never fail the add)
    if let Ok(pool_after) = probe_pool(runner, config.mount_point()) {
        let mn = mapper_name(name);
        if let Some(dev) = pool_after.devices.iter().find(|d| d.mapper == mn) {
            disk_map::update_disk_map_best_effort(|map| {
                disk_map::record_disk(map, name, &disk.by_id.0, &dev.luks_uuid.0, dev.devid);
            });
        }
    }

    eprintln!("Done. {} is now part of the pool.", name);
    Ok(())
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
    use crate::checkpoint::{
        OpArgs, OpKind, Phase, PoolFingerprint, SystemClock, TargetSnapshot, new_checkpoint,
        save_checkpoint_atomic,
    };
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
    use crate::probe::Filesystem;
    use crate::progress::ProgressOutput;
    use std::collections::BTreeMap;
    use std::path::Path;

    struct StaticFs {
        paths: Vec<String>,
    }

    impl Filesystem for StaticFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.iter().any(|p| p == path)
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
    }

    struct GuardedRunner;

    impl GuardedRunner {
        fn out(cmd: &str, stdout: &str, exit_status: i32) -> RawCommandOutput {
            RawCommandOutput {
                cmd: cmd.to_owned(),
                stdout: stdout.to_owned(),
                stderr: String::new(),
                exit_status,
            }
        }

        fn is_mutating(req: &CmdRequest) -> bool {
            matches!(
                req,
                CmdRequest::CryptsetupLuksOpen { .. }
                    | CmdRequest::CryptsetupClose { .. }
                    | CmdRequest::BtrfsDeviceAdd { .. }
                    | CmdRequest::BtrfsDeviceRemove { .. }
                    | CmdRequest::BtrfsDeviceRemoveMissing { .. }
                    | CmdRequest::BtrfsDeviceScan { .. }
                    | CmdRequest::BtrfsDeviceScanAll
                    | CmdRequest::BtrfsBalanceRaid1 { .. }
                    | CmdRequest::BtrfsBalanceSingle { .. }
                    | CmdRequest::MkfsBtrfs { .. }
                    | CmdRequest::Mount { .. }
                    | CmdRequest::CryptsetupLuksFormat { .. }
                    | CmdRequest::CryptsetupTestPassphrase { .. }
                    | CmdRequest::CryptsetupLuksHeaderBackup { .. }
            )
        }
    }

    impl CommandRunner for GuardedRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            if Self::is_mutating(request) {
                panic!("mutating request should not run before resume gate: {request:?}");
            }

            match request {
                CmdRequest::CryptsetupLuksUuid { device } => Ok(Self::out(
                    &format!("cryptsetup luksUUID {device}"),
                    "Device /dev/test is not a valid LUKS device.\n",
                    4,
                )),
                CmdRequest::FindmntJson { mount_point } => Ok(Self::out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    "{\"filesystems\":[]}\n",
                    0,
                )),
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.run(request)
        }
    }

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

    #[test]
    fn invariant_rejected_checkpoint_runs_no_mutating_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        let checkpoint_path = tmp.path().join("op-state.json");

        let mut disks = BTreeMap::new();
        disks.insert(
            "disk2".to_owned(),
            serde_json::json!({ "by_id": "/dev/disk/by-id/virtio-disk2" }),
        );
        let config_json = serde_json::json!({
            "disks": disks,
            "mount_point": "/mnt/storage"
        });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();

        unsafe {
            std::env::set_var(
                "BRAID_TEST_CHECKPOINT_FILE",
                checkpoint_path.to_string_lossy().to_string(),
            );
        }
        let checkpoint = new_checkpoint(
            &SystemClock,
            OpKind::Remove,
            OpArgs::remove("disk2"),
            Phase::RemoveStart,
            "sha256:placeholder".to_owned(),
            hash_args(&["remove", "disk2"]),
            PoolFingerprint {
                devices: vec![],
                missing_count: 0,
                total_devices: 0,
                mounted: false,
            },
            TargetSnapshot {
                primary: Some("disk2".to_owned()),
                secondary: None,
                missing_id: None,
            },
        );
        save_checkpoint_atomic(&checkpoint).unwrap();

        let fs = StaticFs {
            paths: vec!["/dev/disk/by-id/virtio-disk2".to_owned()],
        };
        let err = cmd_add(
            &GuardedRunner,
            &fs,
            Path::new(&config_path),
            "disk2",
            false,
            true,
            None,
            ProgressOutput::Off,
        )
        .expect_err("checkpoint mismatch should fail before mutation");

        assert!(
            err.to_string().contains("error[CHECKPOINT_OP_MISMATCH]:"),
            "unexpected error: {err}"
        );
        unsafe {
            std::env::remove_var("BRAID_TEST_CHECKPOINT_FILE");
        }
    }
}
