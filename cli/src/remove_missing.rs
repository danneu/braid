use crate::checkpoint::{
    CheckpointError, InvocationCtx, LiveCtx, OpArgs, OpKind, Phase, PoolFingerprint, ResumeGate,
    SystemClock, TargetSnapshot, clear_checkpoint, hash_args, maybe_fail_after_checkpoint,
    new_checkpoint, resolve_resume_gate, run_phase_hooks, save_checkpoint_atomic,
};
use crate::cmd::CommandRunner;
use crate::config::{config_hash, config_read_raw};
use crate::disk_map;
use crate::pool::{pool_remove_devid, pool_remove_missing};
use crate::probe::{ProbeError, probe_pool};
use crate::types::*;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum RemoveMissingError {
    #[error("{0}")]
    Validation(String),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("pool error: {0}")]
    Pool(#[from] crate::pool::PoolError),
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("checkpoint error: {0}")]
    Checkpoint(#[from] CheckpointError),
    #[error("checkpoint IO error: {0}")]
    CheckpointIo(#[from] std::io::Error),
}

pub struct RemoveMissingStep {
    pub risk: &'static str,
    pub description: String,
}

pub fn cmd_remove_missing<R: CommandRunner + Sync>(
    runner: &R,
    config_path: &Path,
    missing_id: Option<u64>,
    dry_run: bool,
    yes: bool,
) -> Result<(), RemoveMissingError> {
    let (config, config_raw) = config_read_raw(config_path)?;
    let disk_map_state = disk_map::load_disk_map();
    disk_map::validate_config_key_stability(&config, &disk_map_state)
        .map_err(|e| RemoveMissingError::Validation(e.to_string()))?;

    let pool = match probe_pool(runner, config.mount_point()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return Err(RemoveMissingError::Validation(
                "pool is not mounted. Nothing to remove.".into(),
            ));
        }
        Err(e) => return Err(RemoveMissingError::Probe(e)),
    };

    if !pool.mounted {
        return Err(RemoveMissingError::Validation(
            "pool is not mounted. Nothing to remove.".into(),
        ));
    }

    if pool.missing_count == 0 {
        return Err(RemoveMissingError::Validation(
            "no missing devices detected in pool.".into(),
        ));
    }

    if pool.missing_count > 1 && missing_id.is_none() {
        return Err(RemoveMissingError::Validation(format!(
            "multiple missing devices ({} missing). Pass --missing-id <devid> to target a specific one. Use 'braid status --verbose' to see device IDs.",
            pool.missing_count
        )));
    }

    let steps = compile_steps(missing_id, &pool);

    if dry_run {
        for step in &steps {
            println!("[{:<11}] {}", step.risk, step.description);
        }
        return Ok(());
    }

    // Resolve checkpoint before any mutating requests.
    let args_parts: Vec<String> = if let Some(id) = missing_id {
        vec!["remove-missing".into(), id.to_string()]
    } else {
        vec!["remove-missing".into()]
    };
    let args_refs: Vec<&str> = args_parts.iter().map(|s| s.as_str()).collect();
    let args_hash = hash_args(&args_refs);

    let resume = match resolve_resume_gate(
        &config_raw,
        InvocationCtx {
            op: OpKind::RemoveMissing,
            op_args: OpArgs::remove_missing(missing_id),
            args_hash: args_hash.clone(),
            config_hash: config_hash(&config_raw),
        },
        LiveCtx {
            pool_fingerprint: PoolFingerprint::from_pool_state(&pool),
            primary_target_available: pool.missing_count > 0,
            secondary_target_available: None,
        },
    ) {
        ResumeGate::ResumeFrom(cp) => {
            eprintln!(
                "Resuming previous 'braid remove-missing' at phase {}.",
                cp.phase.as_env_value()
            );
            Some(cp)
        }
        ResumeGate::NoCheckpoint => None,
        ResumeGate::Reject(error) => return Err(RemoveMissingError::Checkpoint(error)),
    };

    // Confirm
    if !yes && resume.is_none() {
        if let Some(devid) = missing_id {
            eprintln!("Remove missing device (devid {}) from pool?", devid);
        } else {
            eprintln!("Remove missing device from pool?");
        }
        eprint!("Type 'remove missing' to confirm: ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).map_err(|e| {
            RemoveMissingError::Validation(format!("failed to read confirmation: {e}"))
        })?;
        if input.trim() != "remove missing" {
            return Err(RemoveMissingError::Validation("aborted by user".into()));
        }
    }

    // Execute
    if resume.is_none() {
        let cp = new_checkpoint(
            &SystemClock,
            OpKind::RemoveMissing,
            OpArgs::remove_missing(missing_id),
            Phase::RemoveMissingStart,
            config_hash(&config_raw),
            args_hash,
            PoolFingerprint::from_pool_state(&pool),
            TargetSnapshot {
                primary: Some("missing".to_owned()),
                secondary: None,
                missing_id,
            },
        );
        save_checkpoint_atomic(&cp)?;
        maybe_fail_after_checkpoint()?;
    }
    run_phase_hooks(&Phase::RemoveMissingStart)?;

    if let Some(devid) = missing_id {
        eprintln!("Removing missing device (devid {}) from pool...", devid);
        pool_remove_devid(runner, config.mount_point(), devid)?;
    } else {
        eprintln!("Removing missing device from pool...");
        pool_remove_missing(runner, config.mount_point())?;
    }

    clear_checkpoint();

    // Update disk map (best effort — never fail the remove-missing)
    if let Some(devid) = missing_id {
        // Targeted removal: prune entries with this specific devid
        disk_map::update_disk_map_best_effort(|map| {
            disk_map::remove_disks_by_devids(map, &[devid]);
        });
    } else if let Ok(pool_after) = probe_pool(runner, config.mount_point()) {
        // General removal: prune entries whose devid is no longer in pool
        let live_devids: Vec<u64> = pool_after.devices.iter().map(|d| d.devid).collect();
        disk_map::update_disk_map_best_effort(|map| {
            disk_map::prune_absent_devids(map, &live_devids);
        });
    }

    eprintln!("Done. Missing device removed from pool.");
    Ok(())
}

fn compile_steps(missing_id: Option<u64>, _pool: &PoolState) -> Vec<RemoveMissingStep> {
    let mut steps = Vec::new();
    if let Some(devid) = missing_id {
        steps.push(RemoveMissingStep {
            risk: "long",
            description: format!(
                "btrfs device remove {} (target specific missing device)",
                devid
            ),
        });
    } else {
        steps.push(RemoveMissingStep {
            risk: "long",
            description: "btrfs device remove missing".into(),
        });
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::{
        OpArgs, OpKind, Phase, PoolFingerprint, SystemClock, TargetSnapshot,
        checkpoint_test_env_lock, new_checkpoint, save_checkpoint_atomic,
    };
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
    use std::collections::BTreeMap;
    use std::path::Path;
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
                CmdRequest::FindmntJson { mount_point } => Ok(Self::out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                    0,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(Self::out(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n",
                    0,
                )),
                CmdRequest::CryptsetupStatus { mapper } => Ok(Self::out(
                    &format!("cryptsetup status {mapper}"),
                    &format!("{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"),
                    0,
                )),
                CmdRequest::CryptsetupLuksUuid { device } => Ok(Self::out(
                    &format!("cryptsetup luksUUID {device}"),
                    "11111111-1111-1111-1111-111111111111\n",
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
    // Intent:
    // - What behavior this test (tries to) verify.
    //   - If resume gate rejects a checkpoint, `braid remove-missing` fails before any mutating command runs.
    //
    // Why it exists:
    // - What risk/regression this protects against.
    //   - Prevents removing the wrong missing/dead member when checkpoint context is invalid.
    //
    // Scenario:
    // - Real-world situation this models (user/system story). Especially the
    //   specific scenario that inspired this test (like a real world bug).
    //   - Pool is degraded and operator retries cleanup with stale checkpoint metadata; command must fail closed.
    fn invariant_rejected_checkpoint_runs_no_mutating_requests() {
        let _guard = checkpoint_test_env_lock().lock().unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        let checkpoint_path = tmp.path().join("op-state.json");

        let mut disks = BTreeMap::new();
        disks.insert(
            "disk1".to_owned(),
            serde_json::json!({ "by_id": "/dev/disk/by-id/virtio-disk1" }),
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
            OpKind::Add,
            OpArgs::add("disk1"),
            Phase::AddBalanceRaid1,
            "sha256:placeholder".to_owned(),
            hash_args(&["add", "disk1"]),
            PoolFingerprint {
                devices: vec![],
                missing_count: 0,
                total_devices: 0,
                mounted: false,
            },
            TargetSnapshot {
                primary: Some("disk1".to_owned()),
                secondary: None,
                missing_id: None,
            },
        );
        save_checkpoint_atomic(&checkpoint).unwrap();

        let err = cmd_remove_missing(&GuardedRunner, Path::new(&config_path), None, false, true)
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
