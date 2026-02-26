use crate::checkpoint::{
    CheckpointError, InvocationCtx, LiveCtx, OpArgs, OpKind, Phase, PoolFingerprint, ResumeGate,
    SystemClock, TargetSnapshot, clear_checkpoint, hash_args, maybe_fail_after_checkpoint,
    new_checkpoint, resolve_resume_gate, run_phase_hooks, save_checkpoint_atomic,
};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::{config_hash, config_read_raw, mapper_name};
use crate::disk_map;
use crate::parse::parse_btrfs_device_usage;
use crate::pool::evict_present_device;
use crate::probe::{ProbeError, probe_pool};
use crate::progress::ProgressOutput;
use crate::status::format_bytes;
use crate::types::*;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
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

pub struct RemoveStep {
    pub risk: &'static str,
    pub description: String,
}

pub fn cmd_remove<R: CommandRunner + Sync>(
    runner: &R,
    config_path: &Path,
    key: &str,
    dry_run: bool,
    yes: bool,
    progress: ProgressOutput,
    checkpoint_path: &Path,
) -> Result<(), RemoveError> {
    let (config, config_raw) = config_read_raw(config_path)?;
    let disk_map_state = disk_map::load_disk_map();
    disk_map::validate_config_key_stability(&config, &disk_map_state)
        .map_err(|e| RemoveError::Validation(e.to_string()))?;

    let pool = match probe_pool(runner, config.mount_point()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return Err(RemoveError::Validation(
                "pool is not mounted. Nothing to remove.".into(),
            ));
        }
        Err(e) => return Err(RemoveError::Probe(e)),
    };

    if !pool.mounted {
        return Err(RemoveError::Validation(
            "pool is not mounted. Nothing to remove.".into(),
        ));
    }

    let mn = mapper_name(key);

    // Is the disk present in the pool?
    let in_pool = pool.devices.iter().any(|d| d.mapper == mn);

    if !in_pool {
        let mut msg = format!("disk '{}' not found in pool.", key);
        if pool.missing_count > 0 {
            msg.push_str(&format!(
                " ({} missing device{} detected. Use 'braid remove-missing' to remove missing devices.)",
                pool.missing_count,
                if pool.missing_count == 1 { "" } else { "s" }
            ));
        }
        return Err(RemoveError::Validation(msg));
    }

    // Pre-flight: reject if other devices lack space to absorb data from
    // the device being removed. Without this, btrfs will either ENOSPC
    // instantly or crash the filesystem to read-only mid-relocation.
    let target_devid = pool.devices.iter().find(|d| d.mapper == mn).map(|d| d.devid);
    if let Some(devid) = target_devid {
        check_eviction_space(runner, config.mount_point(), devid)?;
    }

    let steps = compile_remove_present_steps(key, &mn, &pool)?;

    if dry_run {
        for step in &steps {
            println!("[{:<11}] {}", step.risk, step.description);
        }
        return Ok(());
    }

    if steps.is_empty() {
        eprintln!("Nothing to do.");
        return Ok(());
    }

    // Resolve checkpoint before any mutating requests.
    let args_parts: Vec<String> = vec!["remove".into(), key.into()];
    let args_refs: Vec<&str> = args_parts.iter().map(|s| s.as_str()).collect();
    let args_hash = hash_args(&args_refs);

    let resume = match resolve_resume_gate(
        &config_raw,
        InvocationCtx {
            op: OpKind::Remove,
            op_args: OpArgs::remove(key),
            args_hash: args_hash.clone(),
            config_hash: config_hash(&config_raw),
        },
        LiveCtx {
            pool_fingerprint: PoolFingerprint::from_pool_state(&pool),
            primary_target_available: in_pool,
            secondary_target_available: None,
        },
        checkpoint_path,
    ) {
        ResumeGate::ResumeFrom(cp) => {
            eprintln!(
                "Resuming previous 'braid remove {}' at phase {}.",
                key,
                cp.phase.as_env_value()
            );
            Some(cp)
        }
        ResumeGate::NoCheckpoint => None,
        ResumeGate::Reject(error) => return Err(RemoveError::Checkpoint(error)),
    };

    // Confirm
    if !yes && resume.is_none() {
        let remaining = pool.devices.len() - 1;
        if remaining == 0 {
            return Err(RemoveError::Validation(
                "cannot remove the last disk from the pool".into(),
            ));
        }
        if remaining == 1 {
            eprintln!("WARNING: Removing this disk leaves only 1 disk — no redundancy.");
            eprint!("Type 'remove without redundancy' to confirm: ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).map_err(|e| {
                RemoveError::Validation(format!("failed to read confirmation: {e}"))
            })?;
            if input.trim() != "remove without redundancy" {
                return Err(RemoveError::Validation("aborted by user".into()));
            }
        } else {
            eprintln!(
                "Remove {} from pool? Data will migrate off this disk.",
                key
            );
            eprint!("Type 'yes' to continue: ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).map_err(|e| {
                RemoveError::Validation(format!("failed to read confirmation: {e}"))
            })?;
            if input.trim() != "yes" {
                return Err(RemoveError::Validation("aborted by user".into()));
            }
        }
    }

    // Execute
    if resume.is_none() {
        let cp = new_checkpoint(
            &SystemClock,
            OpKind::Remove,
            OpArgs::remove(key),
            Phase::RemoveStart,
            config_hash(&config_raw),
            args_hash,
            PoolFingerprint::from_pool_state(&pool),
            TargetSnapshot {
                primary: Some(key.to_owned()),
                secondary: None,
                missing_id: None,
            },
        );
        save_checkpoint_atomic(&cp, checkpoint_path)?;
        maybe_fail_after_checkpoint()?;
    }
    run_phase_hooks(&Phase::RemoveStart)?;

    evict_present_device(runner, &mn.0, config.mount_point(), progress)?;

    clear_checkpoint(checkpoint_path);

    // Update disk map (best effort — never fail the remove)
    disk_map::update_disk_map_best_effort(|map| {
        disk_map::remove_disk(map, key);
    });

    eprintln!(
        "Done. If not already done: remove '{}' from braid.disks and run nixos-rebuild switch.",
        key
    );
    Ok(())
}

/// Check that the remaining devices have enough unallocated space to absorb
/// data from the device being removed. If they don't, btrfs device remove will
/// either ENOSPC instantly or crash the filesystem to read-only mid-relocation.
///
/// If the check itself fails (parse error, command error), we log a warning and
/// proceed — a bug in the safety net shouldn't block a valid operation.
fn check_eviction_space<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
    target_devid: u64,
) -> Result<(), RemoveError> {
    let raw = match runner.run(&CmdRequest::BtrfsDeviceUsageRaw {
        mount_point: mount_point.to_owned(),
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("warning: ENOSPC pre-flight check failed: {e}; proceeding anyway");
            return Ok(());
        }
    };

    let usage = match parse_btrfs_device_usage(&raw) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("warning: ENOSPC pre-flight check failed: {e}; proceeding anyway");
            return Ok(());
        }
    };

    let mut target_allocated: u64 = 0;
    let mut other_unallocated: u64 = 0;

    for dev in &usage.devices {
        if dev.devid == target_devid {
            target_allocated += dev.used_bytes();
        } else {
            other_unallocated += dev.unallocated;
        }
    }

    if other_unallocated < target_allocated {
        return Err(RemoveError::Validation(format!(
            "not enough free space to remove this device.\n\n  \
             Device has {} allocated (must be relocated to other devices).\n  \
             Other devices have {} total unallocated.\n\n\
             Without enough space, btrfs will hang and then crash the filesystem to read-only.\n\
             Free up space by deleting files, or add a new device first with `braid add`.",
            format_bytes(target_allocated),
            format_bytes(other_unallocated),
        )));
    }

    Ok(())
}

fn compile_remove_present_steps(
    _key: &str,
    mn: &MapperName,
    pool: &PoolState,
) -> Result<Vec<RemoveStep>, RemoveError> {
    let remaining = pool.devices.len() - 1;
    if remaining == 0 {
        return Err(RemoveError::Validation(
            "cannot remove the last disk from the pool".into(),
        ));
    }

    let mut steps = Vec::new();
    if remaining == 1 {
        steps.push(RemoveStep {
            risk: "long",
            description: "btrfs balance -dconvert=single -mconvert=single -f (RAID1 → single)"
                .into(),
        });
    }
    steps.push(RemoveStep {
        risk: "long",
        description: format!(
            "btrfs device remove /dev/mapper/{} (data migrates off disk)",
            mn
        ),
    });
    steps.push(RemoveStep {
        risk: "safe",
        description: format!("cryptsetup close {}", mn),
    });
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checkpoint::{
        OpArgs, OpKind, Phase, PoolFingerprint, SystemClock, TargetSnapshot, new_checkpoint,
        save_checkpoint_atomic,
    };
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
    use crate::progress::ProgressOutput;
    use std::collections::BTreeMap;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
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
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                    0,
                )),
                CmdRequest::CryptsetupStatus { mapper } => {
                    let dev = if mapper == "braid-disk1" {
                        "/dev/vdb"
                    } else {
                        "/dev/vdc"
                    };
                    Ok(Self::out(
                        &format!("cryptsetup status {mapper}"),
                        &format!("{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"),
                        0,
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = if device == "/dev/vdb" {
                        "11111111-1111-1111-1111-111111111111"
                    } else {
                        "22222222-2222-2222-2222-222222222222"
                    };
                    Ok(Self::out(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                        0,
                    ))
                }
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

    #[derive(Clone)]
    struct RecordingRunner {
        log: Arc<Mutex<Vec<CmdRequest>>>,
    }

    impl RecordingRunner {
        fn new(log: Arc<Mutex<Vec<CmdRequest>>>) -> Self {
            Self { log }
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());

            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(GuardedRunner::out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                    0,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(GuardedRunner::out(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                    0,
                )),
                CmdRequest::CryptsetupStatus { mapper } => {
                    let dev = if mapper == "braid-disk1" {
                        "/dev/vdb"
                    } else {
                        "/dev/vdc"
                    };
                    Ok(GuardedRunner::out(
                        &format!("cryptsetup status {mapper}"),
                        &format!("{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"),
                        0,
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = if device == "/dev/vdb" {
                        "11111111-1111-1111-1111-111111111111"
                    } else {
                        "22222222-2222-2222-2222-222222222222"
                    };
                    Ok(GuardedRunner::out(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                        0,
                    ))
                }
                CmdRequest::BtrfsBalanceSingle { .. } => {
                    Ok(GuardedRunner::out("btrfs balance start", "", 0))
                }
                CmdRequest::BtrfsDeviceRemove { .. } => {
                    Ok(GuardedRunner::out("btrfs device remove", "", 0))
                }
                CmdRequest::CryptsetupClose { .. } => {
                    Ok(GuardedRunner::out("cryptsetup close", "", 0))
                }
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
    //   - If resume gate rejects a checkpoint, `braid remove` fails before any mutating command runs.
    //
    // Why it exists:
    // - What risk/regression this protects against.
    //   - Prevents accidental disk removal/migration when checkpoint validation fails.
    //
    // Scenario:
    // - Real-world situation this models (user/system story). Especially the
    //   specific scenario that inspired this test (like a real world bug).
    //   - An interrupted operation leaves stale state; operator reruns `remove` and command must reject before any mutation.
    fn invariant_rejected_checkpoint_runs_no_mutating_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        let checkpoint_path = tmp.path().join("op-state.json");

        let mut disks = BTreeMap::new();
        disks.insert(
            "disk1".to_owned(),
            serde_json::json!({ "by_id": "/dev/disk/by-id/virtio-disk1" }),
        );
        disks.insert(
            "disk2".to_owned(),
            serde_json::json!({ "by_id": "/dev/disk/by-id/virtio-disk2" }),
        );
        let config_json = serde_json::json!({
            "disks": disks,
            "mount_point": "/mnt/storage"
        });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();

        let checkpoint = new_checkpoint(
            &SystemClock,
            OpKind::Add,
            OpArgs::add("disk2"),
            Phase::AddBalanceRaid1,
            "sha256:placeholder".to_owned(),
            hash_args(&["add", "disk2"]),
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
        save_checkpoint_atomic(&checkpoint, &checkpoint_path).unwrap();

        let err = cmd_remove(
            &GuardedRunner,
            Path::new(&config_path),
            "disk2",
            false,
            true,
            ProgressOutput::Off,
            &checkpoint_path,
        )
        .expect_err("checkpoint mismatch should fail before mutation");

        assert!(
            err.to_string().contains("error[CHECKPOINT_OP_MISMATCH]:"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Intent:
    // - What behavior this test (tries to) verify.
    //   - `braid remove` converts RAID1 to single before removing a device when only one disk remains.
    //
    // Why it exists:
    // - What risk/regression this protects against.
    //   - Prevents command-order regressions that make `btrfs device remove` fail under RAID1 minimum-device constraints.
    //
    // Scenario:
    // - Real-world situation this models (user/system story). Especially the
    //   specific scenario that inspired this test (like a real world bug).
    //   - Operator removes one disk from a healthy two-disk pool and expects the operation to succeed end-to-end.
    fn remove_two_disk_pool_balances_single_before_device_remove() {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        let checkpoint_path = tmp.path().join("op-state.json");

        let mut disks = BTreeMap::new();
        disks.insert(
            "disk1".to_owned(),
            serde_json::json!({ "by_id": "/dev/disk/by-id/virtio-disk1" }),
        );
        disks.insert(
            "disk2".to_owned(),
            serde_json::json!({ "by_id": "/dev/disk/by-id/virtio-disk2" }),
        );
        let config_json = serde_json::json!({
            "disks": disks,
            "mount_point": "/mnt/storage"
        });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = RecordingRunner::new(log.clone());
        cmd_remove(
            &runner,
            Path::new(&config_path),
            "disk2",
            false,
            true,
            ProgressOutput::Off,
            &checkpoint_path,
        )
        .expect("remove should succeed");

        let calls = log.lock().unwrap();
        let balance_idx = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsBalanceSingle { .. }))
            .expect("expected balance-to-single request");
        let remove_idx = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. }))
            .expect("expected device-remove request");

        assert!(
            balance_idx < remove_idx,
            "expected balance-to-single before device-remove; calls: {calls:?}"
        );
    }
}
