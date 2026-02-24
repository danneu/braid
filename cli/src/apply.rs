use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::{config_hash, config_read_raw, Config};
use crate::parse::btrfs_filesystem_show::{classify_btrfs_probe, DeviceBtrfsProbe};
use crate::parse::mount::{classify_mount_error, MountOutcome};
use crate::parse::{parse_btrfs_filesystem_show, parse_cryptsetup_luks_uuid};
use crate::plan::{compute_plan, mapper_name_for_by_id, to_plan_report};
use crate::probe::{probe_config_disk, probe_pool, Filesystem};
use crate::progress::{self, ProgressMode, ProgressOutput};
use crate::types::*;
use serde::{Deserialize, Serialize};
use std::path::Path;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const CHECKPOINT_DIR: &str = "/var/lib/braid";
const CHECKPOINT_FILE: &str = "/var/lib/braid/apply-state.json";
const HISTORY_DIR: &str = "/var/lib/braid/history";
const HISTORY_KEEP: usize = 20;

// ---------------------------------------------------------------------------
// ApplyFlags
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ApplyFlags {
    pub resume: bool,
    pub allow_remove_missing: bool,
    pub allow_remove_ambiguous: bool,
    pub progress: ProgressMode,
    pub json: bool,
}

impl Default for ApplyFlags {
    fn default() -> Self {
        Self {
            resume: false,
            allow_remove_missing: false,
            allow_remove_ambiguous: false,
            progress: ProgressMode::Auto,
            json: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Checkpoint
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Completed,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Checkpoint {
    pub schema_version: u32,
    pub plan_id: String,
    pub mount_point: String,
    pub status: PlanStatus,
    pub config_hash: String,
    pub created_at: String,
    pub updated_at: String,
    pub last_completed_action_id: String,
    pub is_bootstrap: bool,
    pub actions: Vec<Action>,
    pub warnings: Vec<Warning>,
    pub confirmations: Vec<Confirmation>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub run_outcome: Option<RunOutcome>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub failed_action_id: Option<String>,
}

// ---------------------------------------------------------------------------
// ApplyError
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum ApplyError {
    #[error("plan is blocked: {0}")]
    Blocked(String),
    #[error("checkpoint exists at {path}. Use --resume to continue.")]
    CheckpointExists { path: String },
    #[error("no checkpoint found. Run 'braid apply' first.")]
    NoCheckpoint,
    #[error("config has changed since checkpoint was created")]
    StaleCheckpoint,
    #[error("confirmation required: BRAID_CONFIRM='{phrase}'")]
    ConfirmationMissing { phrase: String },
    #[error("action {action_id} ({action_type}) failed: {detail}")]
    ActionFailed {
        action_id: String,
        action_type: String,
        detail: String,
    },
    #[error("target absent for pending action {action_id}: {target}")]
    ResumeTargetMissing {
        action_id: String,
        target: String,
    },
    #[error("{0}")]
    Io(String),
    #[error("{0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("{0}")]
    Probe(#[from] crate::probe::ProbeError),
}

// ---------------------------------------------------------------------------
// Checkpoint I/O
// ---------------------------------------------------------------------------

fn checkpoint_write(cp: &Checkpoint) -> Result<(), ApplyError> {
    std::fs::create_dir_all(CHECKPOINT_DIR)
        .map_err(|e| ApplyError::Io(format!("create {CHECKPOINT_DIR}: {e}")))?;

    let json = serde_json::to_string_pretty(cp)
        .map_err(|e| ApplyError::Io(format!("serialize checkpoint: {e}")))?;

    let tmp_path = format!("{CHECKPOINT_FILE}.tmp");
    std::fs::write(&tmp_path, &json)
        .map_err(|e| ApplyError::Io(format!("write {tmp_path}: {e}")))?;
    std::fs::rename(&tmp_path, CHECKPOINT_FILE)
        .map_err(|e| ApplyError::Io(format!("rename {tmp_path} -> {CHECKPOINT_FILE}: {e}")))?;

    Ok(())
}

fn checkpoint_read() -> Result<Checkpoint, ApplyError> {
    let data = std::fs::read_to_string(CHECKPOINT_FILE)
        .map_err(|_| ApplyError::NoCheckpoint)?;
    serde_json::from_str(&data)
        .map_err(|e| ApplyError::Io(format!("parse checkpoint: {e}")))
}

fn checkpoint_finalize(cp: &Checkpoint) -> Result<(), ApplyError> {
    std::fs::create_dir_all(HISTORY_DIR)
        .map_err(|e| ApplyError::Io(format!("create {HISTORY_DIR}: {e}")))?;

    let mut hist = cp.clone();
    hist.run_outcome = Some(RunOutcome::Completed);

    let history_path = format!("{HISTORY_DIR}/{}.json", cp.plan_id);
    let json = serde_json::to_string_pretty(&hist)
        .map_err(|e| ApplyError::Io(format!("serialize history: {e}")))?;
    std::fs::write(&history_path, &json)
        .map_err(|e| ApplyError::Io(format!("write {history_path}: {e}")))?;

    // Remove checkpoint
    let _ = std::fs::remove_file(CHECKPOINT_FILE);

    // Prune history to HISTORY_KEEP newest
    prune_history();

    Ok(())
}

fn checkpoint_write_failure_history(
    cp: &Checkpoint,
    failed_action_id: &str,
) -> Result<(), ApplyError> {
    std::fs::create_dir_all(HISTORY_DIR)
        .map_err(|e| ApplyError::Io(format!("create {HISTORY_DIR}: {e}")))?;

    let mut hist = cp.clone();
    hist.run_outcome = Some(RunOutcome::Failed);
    hist.failed_action_id = Some(failed_action_id.to_owned());

    let history_path = format!("{HISTORY_DIR}/{}-failed.json", cp.plan_id);
    let json = serde_json::to_string_pretty(&hist)
        .map_err(|e| ApplyError::Io(format!("serialize failure history: {e}")))?;

    let tmp_path = format!("{history_path}.tmp");
    std::fs::write(&tmp_path, &json)
        .map_err(|e| ApplyError::Io(format!("write {tmp_path}: {e}")))?;
    std::fs::rename(&tmp_path, &history_path)
        .map_err(|e| ApplyError::Io(format!("rename {tmp_path} -> {history_path}: {e}")))?;

    prune_history();

    Ok(())
}

fn prune_history() {
    let entries = match std::fs::read_dir(HISTORY_DIR) {
        Ok(e) => e,
        Err(_) => return,
    };

    let mut files: Vec<_> = entries
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path()
                .extension()
                .is_some_and(|ext| ext == "json")
        })
        .collect();

    if files.len() <= HISTORY_KEEP {
        return;
    }

    // Sort by name (plan IDs are timestamp-prefixed, so lexicographic = chronological)
    files.sort_by_key(|e| e.file_name());

    let to_remove = files.len() - HISTORY_KEEP;
    for entry in files.into_iter().take(to_remove) {
        let _ = std::fs::remove_file(entry.path());
    }
}

// ---------------------------------------------------------------------------
// Confirmation gates
// ---------------------------------------------------------------------------

fn check_confirmations(confirmations: &[Confirmation]) -> Result<(), ApplyError> {
    let confirm_env = std::env::var("BRAID_CONFIRM").unwrap_or_default();
    check_confirmations_with(confirmations, &confirm_env)
}

fn check_confirmations_with(
    confirmations: &[Confirmation],
    confirm_env: &str,
) -> Result<(), ApplyError> {
    if confirmations.is_empty() {
        return Ok(());
    }

    let provided: Vec<&str> = confirm_env.split(';').map(|s| s.trim()).collect();

    for c in confirmations {
        if !provided.iter().any(|p| *p == c.phrase) {
            return Err(ApplyError::ConfirmationMissing {
                phrase: c.phrase.clone(),
            });
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Action executors
// ---------------------------------------------------------------------------

fn execute_open_luks<R: CommandRunner>(
    runner: &R,
    fs: &dyn Filesystem,
    target: &str,
) -> Result<(), ApplyError> {
    let passphrase = std::env::var("BRAID_PASSPHRASE").map_err(|_| ApplyError::ActionFailed {
        action_id: String::new(),
        action_type: "OPEN_LUKS".into(),
        detail: "BRAID_PASSPHRASE not set".into(),
    })?;
    execute_open_luks_with(runner, fs, target, &passphrase)
}

fn execute_open_luks_with<R: CommandRunner>(
    runner: &R,
    fs: &dyn Filesystem,
    target: &str,
    passphrase: &str,
) -> Result<(), ApplyError> {
    if !fs.exists(target) {
        return Err(ApplyError::ActionFailed {
            action_id: String::new(),
            action_type: "OPEN_LUKS".into(),
            detail: format!("device {target} does not exist"),
        });
    }

    // Check isLuks
    let is_luks = runner.run(&CmdRequest::CryptsetupIsLuks {
        device: target.to_owned(),
    });
    match is_luks {
        Ok(out) if out.exit_status != 0 => {
            return Err(ApplyError::ActionFailed {
                action_id: String::new(),
                action_type: "OPEN_LUKS".into(),
                detail: format!("{target} is not a LUKS device"),
            });
        }
        Err(e) => {
            return Err(ApplyError::ActionFailed {
                action_id: String::new(),
                action_type: "OPEN_LUKS".into(),
                detail: format!("isLuks check failed: {e}"),
            });
        }
        _ => {}
    }

    // Derive mapper name from by-id path
    let mapper_name = match mapper_name_for_by_id(&ByIdPath(target.to_owned())) {
        Some(mn) => mn.0,
        None => {
            return Err(ApplyError::ActionFailed {
                action_id: String::new(),
                action_type: "OPEN_LUKS".into(),
                detail: format!("cannot derive mapper name from {target}"),
            });
        }
    };

    // Idempotency: check if mapper already open with same UUID
    let mapper_path = format!("/dev/mapper/{mapper_name}");
    if fs.exists(&mapper_path) {
        println!("  mapper {mapper_path} already open, skipping luksOpen");
        return Ok(());
    }

    // Open LUKS
    let result = runner.run_with_stdin(
        &CmdRequest::CryptsetupLuksOpen {
            device: target.to_owned(),
            mapper: mapper_name.clone(),
        },
        passphrase.as_bytes(),
    );

    match result {
        Ok(out) if out.exit_status != 0 => Err(ApplyError::ActionFailed {
            action_id: String::new(),
            action_type: "OPEN_LUKS".into(),
            detail: format!("luksOpen failed (exit {}): {}", out.exit_status, out.stderr),
        }),
        Err(e) => Err(ApplyError::ActionFailed {
            action_id: String::new(),
            action_type: "OPEN_LUKS".into(),
            detail: format!("luksOpen command error: {e}"),
        }),
        Ok(_) => Ok(()),
    }
}

fn execute_btrfs_add<R: CommandRunner>(
    runner: &R,
    target: &str,
    mount_point: &str,
    is_bootstrap: bool,
) -> Result<(), ApplyError> {
    // Check if pool is currently mounted
    let mounted = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.to_owned(),
    });
    let is_mounted = matches!(mounted, Ok(ref out) if out.exit_status == 0);

    if is_bootstrap && !is_mounted {
        // Planner thinks this is a fresh pool (unmounted, 0 known devices).
        // But an existing pool may be offline. Check the device superblock
        // before destroying anything: `btrfs filesystem show <device>` reads
        // the superblock directly without needing the full pool assembled.
        let has_btrfs = probe_device_has_btrfs(runner, target);

        match has_btrfs {
            DeviceBtrfsProbe::HasBtrfs => {
                // Existing btrfs metadata — NOT a true bootstrap.
                // Bring the existing pool online: scan + mount, then return.
                // Later ADD actions for other devices will see a mounted pool
                // and use the normal scan+add path.
                println!("  existing btrfs detected on {target}, mounting existing pool");
                let _ = runner.run(&CmdRequest::BtrfsDeviceScan {
                    device: target.to_owned(),
                });

                std::fs::create_dir_all(mount_point)
                    .map_err(|e| ApplyError::Io(format!("mkdir -p {mount_point}: {e}")))?;

                let mount_result = runner.run(&CmdRequest::Mount {
                    device: target.to_owned(),
                    mount_point: mount_point.to_owned(),
                });
                match mount_result {
                    Ok(out) if out.exit_status == 0 => {
                        // Pool mounted successfully. Done.
                    }
                    Ok(out) => {
                        match classify_mount_error(&out.stderr) {
                            MountOutcome::MissingMembersDeferred => {
                                // Multi-device pool with not all members open yet.
                                // Safe to defer — a later ADD will retry once more
                                // mappers are available. The key: we did NOT mkfs.
                                println!(
                                    "  mount deferred (missing members), will retry after more devices open"
                                );
                            }
                            MountOutcome::HardError(_) => {
                                return Err(ApplyError::ActionFailed {
                                    action_id: String::new(),
                                    action_type: "ADD_DISK_BTRFS_ADD".into(),
                                    detail: format!(
                                        "mount existing pool failed (exit {}): {}",
                                        out.exit_status, out.stderr
                                    ),
                                });
                            }
                        }
                    }
                    Err(e) => {
                        return Err(ApplyError::ActionFailed {
                            action_id: String::new(),
                            action_type: "ADD_DISK_BTRFS_ADD".into(),
                            detail: format!("mount existing pool error: {e}"),
                        });
                    }
                }

                // Device is already part of the pool. Done for this action.
                return Ok(());
            }
            DeviceBtrfsProbe::NoBtrfs => {
                // Device has no btrfs superblock — safe to bootstrap.
                println!("  bootstrap: creating new btrfs filesystem on {target}");
                let mkfs = runner.run(&CmdRequest::MkfsBtrfs {
                    device: target.to_owned(),
                });
                match mkfs {
                    Ok(out) if out.exit_status != 0 => {
                        return Err(ApplyError::ActionFailed {
                            action_id: String::new(),
                            action_type: "ADD_DISK_BTRFS_ADD".into(),
                            detail: format!("mkfs.btrfs failed (exit {}): {}", out.exit_status, out.stderr),
                        });
                    }
                    Err(e) => {
                        return Err(ApplyError::ActionFailed {
                            action_id: String::new(),
                            action_type: "ADD_DISK_BTRFS_ADD".into(),
                            detail: format!("mkfs.btrfs error: {e}"),
                        });
                    }
                    _ => {}
                }

                std::fs::create_dir_all(mount_point)
                    .map_err(|e| ApplyError::Io(format!("mkdir -p {mount_point}: {e}")))?;

                let mount_result = runner.run(&CmdRequest::Mount {
                    device: target.to_owned(),
                    mount_point: mount_point.to_owned(),
                });
                match mount_result {
                    Ok(out) if out.exit_status != 0 => {
                        return Err(ApplyError::ActionFailed {
                            action_id: String::new(),
                            action_type: "ADD_DISK_BTRFS_ADD".into(),
                            detail: format!("mount failed (exit {}): {}", out.exit_status, out.stderr),
                        });
                    }
                    Err(e) => {
                        return Err(ApplyError::ActionFailed {
                            action_id: String::new(),
                            action_type: "ADD_DISK_BTRFS_ADD".into(),
                            detail: format!("mount error: {e}"),
                        });
                    }
                    _ => {}
                }

                return Ok(());
            }
            DeviceBtrfsProbe::Unknown(detail) => {
                // Ambiguous — refuse to mkfs.
                return Err(ApplyError::ActionFailed {
                    action_id: String::new(),
                    action_type: "ADD_DISK_BTRFS_ADD".into(),
                    detail: format!(
                        "cannot determine if {target} has existing btrfs: {detail}"
                    ),
                });
            }
        }
    }

    // Existing pool — scan for returning member detection, then add
    let _ = runner.run(&CmdRequest::BtrfsDeviceScan {
        device: target.to_owned(),
    });

    // Check if device is already in the pool
    let show_result = runner.run(&CmdRequest::BtrfsFilesystemShow {
        mount_point: mount_point.to_owned(),
    });
    if let Ok(ref show_raw) = show_result {
        if let Ok(show) = parse_btrfs_filesystem_show(show_raw) {
            if show.devices.iter().any(|d| d.path == target) {
                println!("  {target} already in pool, skipping device add");
                return Ok(());
            }
        }
    }

    let add_result = runner.run(&CmdRequest::BtrfsDeviceAdd {
        device: target.to_owned(),
        mount_point: mount_point.to_owned(),
    });
    match add_result {
        Ok(out) if out.exit_status != 0 => Err(ApplyError::ActionFailed {
            action_id: String::new(),
            action_type: "ADD_DISK_BTRFS_ADD".into(),
            detail: format!("btrfs device add failed (exit {}): {}", out.exit_status, out.stderr),
        }),
        Err(e) => Err(ApplyError::ActionFailed {
            action_id: String::new(),
            action_type: "ADD_DISK_BTRFS_ADD".into(),
            detail: format!("btrfs device add error: {e}"),
        }),
        Ok(_) => Ok(()),
    }
}

fn execute_balance_raid1<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &str,
    output: ProgressOutput,
) -> Result<(), ApplyError> {
    let request = CmdRequest::BtrfsBalanceRaid1 {
        mount_point: mount_point.to_owned(),
    };
    let result = progress::run_with_progress(runner, &request, mount_point, output);
    match result {
        Ok(out) if out.exit_status != 0 => Err(ApplyError::ActionFailed {
            action_id: String::new(),
            action_type: "BALANCE_TO_RAID1".into(),
            detail: format!(
                "btrfs balance raid1 failed (exit {}): {}",
                out.exit_status, out.stderr
            ),
        }),
        Err(e) => Err(ApplyError::ActionFailed {
            action_id: String::new(),
            action_type: "BALANCE_TO_RAID1".into(),
            detail: format!("btrfs balance error: {e}"),
        }),
        Ok(_) => Ok(()),
    }
}

fn execute_remove_graceful<R: CommandRunner + Sync>(
    runner: &R,
    target: &str,
    mount_point: &str,
    output: ProgressOutput,
) -> Result<(), ApplyError> {
    // Count current devices to decide if we need single conversion
    let device_count = count_pool_devices(runner, mount_point);

    if device_count <= 2 {
        // Convert to single before removing (can't maintain raid1 with < 2 devices)
        println!("  converting to single profile before removal");
        let balance_req = CmdRequest::BtrfsBalanceSingle {
            mount_point: mount_point.to_owned(),
        };
        let conv = progress::run_with_progress(runner, &balance_req, mount_point, output);
        match conv {
            Ok(out) if out.exit_status != 0 => {
                return Err(ApplyError::ActionFailed {
                    action_id: String::new(),
                    action_type: "REMOVE_DISK_GRACEFUL".into(),
                    detail: format!(
                        "balance to single failed (exit {}): {}",
                        out.exit_status, out.stderr
                    ),
                });
            }
            Err(e) => {
                return Err(ApplyError::ActionFailed {
                    action_id: String::new(),
                    action_type: "REMOVE_DISK_GRACEFUL".into(),
                    detail: format!("balance to single error: {e}"),
                });
            }
            _ => {}
        }
    }

    let remove_req = CmdRequest::BtrfsDeviceRemove {
        device: target.to_owned(),
        mount_point: mount_point.to_owned(),
    };
    let result = progress::run_with_progress(runner, &remove_req, mount_point, output);
    match result {
        Ok(out) if out.exit_status != 0 => Err(ApplyError::ActionFailed {
            action_id: String::new(),
            action_type: "REMOVE_DISK_GRACEFUL".into(),
            detail: format!(
                "btrfs device remove failed (exit {}): {}",
                out.exit_status, out.stderr
            ),
        }),
        Err(e) => Err(ApplyError::ActionFailed {
            action_id: String::new(),
            action_type: "REMOVE_DISK_GRACEFUL".into(),
            detail: format!("btrfs device remove error: {e}"),
        }),
        Ok(_) => Ok(()),
    }
}

fn execute_remove_missing<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &str,
    output: ProgressOutput,
) -> Result<(), ApplyError> {
    // Count present devices
    let device_count = count_pool_devices(runner, mount_point);

    if device_count <= 1 {
        // After removing missing, only 0 or 1 present → need single profile
        println!("  converting to single profile before removing missing");
        let balance_req = CmdRequest::BtrfsBalanceSingle {
            mount_point: mount_point.to_owned(),
        };
        let conv = progress::run_with_progress(runner, &balance_req, mount_point, output);
        match conv {
            Ok(out) if out.exit_status != 0 => {
                return Err(ApplyError::ActionFailed {
                    action_id: String::new(),
                    action_type: "REMOVE_DISK_MISSING_EXPLICIT".into(),
                    detail: format!(
                        "balance to single failed (exit {}): {}",
                        out.exit_status, out.stderr
                    ),
                });
            }
            Err(e) => {
                return Err(ApplyError::ActionFailed {
                    action_id: String::new(),
                    action_type: "REMOVE_DISK_MISSING_EXPLICIT".into(),
                    detail: format!("balance to single error: {e}"),
                });
            }
            _ => {}
        }
    }

    let remove_req = CmdRequest::BtrfsDeviceRemoveMissing {
        mount_point: mount_point.to_owned(),
    };
    let result = progress::run_with_progress(runner, &remove_req, mount_point, output);
    match result {
        Ok(out) if out.exit_status != 0 => Err(ApplyError::ActionFailed {
            action_id: String::new(),
            action_type: "REMOVE_DISK_MISSING_EXPLICIT".into(),
            detail: format!(
                "btrfs device remove missing failed (exit {}): {}",
                out.exit_status, out.stderr
            ),
        }),
        Err(e) => Err(ApplyError::ActionFailed {
            action_id: String::new(),
            action_type: "REMOVE_DISK_MISSING_EXPLICIT".into(),
            detail: format!("btrfs device remove missing error: {e}"),
        }),
        Ok(_) => Ok(()),
    }
}

fn execute_close_luks<R: CommandRunner>(runner: &R, target: &str) -> Result<(), ApplyError> {
    // Extract mapper name from /dev/mapper/<name>
    let mapper_name = target
        .strip_prefix("/dev/mapper/")
        .unwrap_or(target);

    let result = runner.run(&CmdRequest::CryptsetupClose {
        mapper: mapper_name.to_owned(),
    });
    match result {
        Ok(out) if out.exit_status != 0 => {
            // Non-fatal — warn but don't fail
            eprintln!(
                "  warning: cryptsetup close {mapper_name} failed (exit {}): {}",
                out.exit_status, out.stderr
            );
            Ok(())
        }
        Err(e) => {
            eprintln!("  warning: cryptsetup close {mapper_name} error: {e}");
            Ok(())
        }
        Ok(_) => Ok(()),
    }
}

fn execute_verify_health<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
) -> Result<(), ApplyError> {
    // Check mounted
    let mounted = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.to_owned(),
    });
    if !matches!(mounted, Ok(ref out) if out.exit_status == 0) {
        eprintln!("  warning: {mount_point} is not mounted");
        return Ok(());
    }

    // Check btrfs filesystem show for missing
    let show = runner.run(&CmdRequest::BtrfsFilesystemShow {
        mount_point: mount_point.to_owned(),
    });
    if let Ok(ref raw) = show {
        if let Ok(parsed) = parse_btrfs_filesystem_show(raw) {
            if parsed.has_missing {
                eprintln!(
                    "  warning: pool has missing devices ({} present of {} total)",
                    parsed.devices.len(),
                    parsed.total_devices
                );
            }
        }
    }

    Ok(())
}

fn execute_verify_diskset<R: CommandRunner>(
    runner: &R,
    fs: &dyn Filesystem,
    config: &Config,
    mount_point: &str,
) -> Result<(), ApplyError> {
    let show = runner.run(&CmdRequest::BtrfsFilesystemShow {
        mount_point: mount_point.to_owned(),
    });
    let pool_devices: Vec<String> = match show {
        Ok(ref raw) => match parse_btrfs_filesystem_show(raw) {
            Ok(parsed) => parsed.devices.iter().map(|d| d.path.clone()).collect(),
            Err(_) => {
                eprintln!("  warning: could not parse btrfs filesystem show output");
                return Ok(());
            }
        },
        Err(_) => {
            eprintln!("  warning: could not run btrfs filesystem show");
            return Ok(());
        }
    };

    for disk in config.disks() {
        if !fs.exists(&disk.0) {
            eprintln!("  warning: config disk {} not present", disk.0);
            continue;
        }

        // Check LUKS UUID
        let uuid_result = runner.run(&CmdRequest::CryptsetupLuksUuid {
            device: disk.0.clone(),
        });
        if let Ok(ref raw) = uuid_result {
            if let Ok(_parsed) = parse_cryptsetup_luks_uuid(raw) {
                let mapper = mapper_name_for_by_id(disk);
                if let Some(mn) = mapper {
                    let mapper_path = format!("/dev/mapper/{}", mn.0);
                    if !pool_devices.contains(&mapper_path) {
                        eprintln!(
                            "  warning: config disk {} (mapper {}) not in pool",
                            disk.0, mapper_path
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if a device has btrfs metadata by reading its superblock directly.
/// `btrfs filesystem show <device>` reads the on-disk superblock without
/// needing the pool to be mounted or all members present.
fn probe_device_has_btrfs<R: CommandRunner>(runner: &R, device: &str) -> DeviceBtrfsProbe {
    let result = runner.run(&CmdRequest::BtrfsFilesystemShow {
        mount_point: device.to_owned(), // show accepts device path too
    });
    match result {
        Ok(ref out) => classify_btrfs_probe(out),
        Err(e) => DeviceBtrfsProbe::Unknown(format!("command error: {e}")),
    }
}

fn count_pool_devices<R: CommandRunner>(runner: &R, mount_point: &str) -> usize {
    let show = runner.run(&CmdRequest::BtrfsFilesystemShow {
        mount_point: mount_point.to_owned(),
    });
    match show {
        Ok(ref raw) => match parse_btrfs_filesystem_show(raw) {
            Ok(parsed) => parsed.devices.len(),
            Err(_) => 0,
        },
        Err(_) => 0,
    }
}

fn now_utc() -> String {
    use time::macros::format_description;
    let fmt = format_description!(
        "[year]-[month padding:zero]-[day padding:zero]T[hour padding:zero]:[minute padding:zero]:[second padding:zero]Z"
    );
    time::OffsetDateTime::now_utc()
        .format(&fmt)
        .unwrap_or_else(|_| "unknown".to_owned())
}

fn action_type_label(at: &ActionType) -> &'static str {
    match at {
        ActionType::OpenLuks => "OPEN_LUKS",
        ActionType::AddDiskBtrfsAdd => "ADD_DISK_BTRFS_ADD",
        ActionType::BalanceToRaid1 => "BALANCE_TO_RAID1",
        ActionType::RemoveDiskGraceful => "REMOVE_DISK_GRACEFUL",
        ActionType::RemoveDiskMissingExplicit => "REMOVE_DISK_MISSING_EXPLICIT",
        ActionType::CloseLuksMapper => "CLOSE_LUKS_MAPPER",
        ActionType::VerifyPoolHealth => "VERIFY_POOL_HEALTH",
        ActionType::VerifyExpectedDiskSet => "VERIFY_EXPECTED_DISK_SET",
    }
}

fn is_verify_action(at: &ActionType) -> bool {
    matches!(
        at,
        ActionType::VerifyPoolHealth | ActionType::VerifyExpectedDiskSet
    )
}

// ---------------------------------------------------------------------------
// Action dispatch
// ---------------------------------------------------------------------------

fn execute_action<R: CommandRunner + Sync>(
    runner: &R,
    fs: &dyn Filesystem,
    action: &Action,
    config: &Config,
    is_bootstrap: bool,
    output: ProgressOutput,
) -> Result<(), ApplyError> {
    if let Ok(v) = std::env::var("BRAID_TEST_FAIL_DURING_ACTION") {
        if v == action.id {
            return Err(ApplyError::Io(
                "simulated failure via BRAID_TEST_FAIL_DURING_ACTION".into(),
            ));
        }
    }

    match action.action_type {
        ActionType::OpenLuks => execute_open_luks(runner, fs, &action.target),
        ActionType::AddDiskBtrfsAdd => {
            execute_btrfs_add(runner, &action.target, config.mount_point(), is_bootstrap)
        }
        ActionType::BalanceToRaid1 => {
            execute_balance_raid1(runner, &action.target, output)
        }
        ActionType::RemoveDiskGraceful => {
            execute_remove_graceful(runner, &action.target, config.mount_point(), output)
        }
        ActionType::RemoveDiskMissingExplicit => {
            execute_remove_missing(runner, &action.target, output)
        }
        ActionType::CloseLuksMapper => execute_close_luks(runner, &action.target),
        ActionType::VerifyPoolHealth => execute_verify_health(runner, &action.target),
        ActionType::VerifyExpectedDiskSet => {
            execute_verify_diskset(runner, fs, config, &action.target)
        }
    }
}

// ---------------------------------------------------------------------------
// Resume target validation
// ---------------------------------------------------------------------------

fn validate_resume_targets(
    fs: &dyn Filesystem,
    actions: &[Action],
) -> Result<(), ApplyError> {
    // Build lookup table: action id -> &Action
    let by_id: std::collections::HashMap<&str, &Action> = actions
        .iter()
        .map(|a| (a.id.as_str(), a))
        .collect();

    for action in actions {
        if matches!(action.state, ActionState::Completed) {
            continue;
        }

        match action.action_type {
            ActionType::OpenLuks => {
                // Strict: physical device must exist now.
                if !fs.exists(&action.target) {
                    return Err(ApplyError::ResumeTargetMissing {
                        action_id: action.id.clone(),
                        target: action.target.clone(),
                    });
                }
            }
            ActionType::AddDiskBtrfsAdd | ActionType::RemoveDiskGraceful => {
                // Mapper may not exist yet if preceding OPEN_LUKS is still pending.
                if !fs.exists(&action.target) {
                    let can_recover = action.target.starts_with("/dev/mapper/")
                        && has_pending_open_for_mapper(action, &by_id);
                    if !can_recover {
                        return Err(ApplyError::ResumeTargetMissing {
                            action_id: action.id.clone(),
                            target: action.target.clone(),
                        });
                    }
                }
            }
            _ => {}
        }
    }

    Ok(())
}

/// Check whether `add_action` has a pending/in-progress OPEN_LUKS precondition
/// that will create the mapper `add_action.target`.
fn has_pending_open_for_mapper(
    add_action: &Action,
    by_id: &std::collections::HashMap<&str, &Action>,
) -> bool {
    for precond_id in &add_action.preconditions {
        let Some(precond) = by_id.get(precond_id.as_str()) else {
            continue;
        };
        if precond.action_type != ActionType::OpenLuks {
            continue;
        }
        if !matches!(
            precond.state,
            ActionState::Pending | ActionState::InProgress | ActionState::Failed { .. }
        ) {
            continue;
        }
        // Derive the mapper path that OPEN_LUKS will create from its by-id target.
        if let Some(mn) = mapper_name_for_by_id(&ByIdPath(precond.target.clone())) {
            let expected_mapper = format!("/dev/mapper/{}", mn.0);
            if expected_mapper == add_action.target {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Failure recording (best-effort history + infallible error construction)
// ---------------------------------------------------------------------------

/// Write failure history as best-effort, then return the `ActionFailed` error.
///
/// Returns `ApplyError` (not `Result`) — this makes it structurally impossible
/// for a history-write failure to mask the original action error via `?`.
fn record_failure_and_build_error(
    checkpoint: &Checkpoint,
    action_id: &str,
    action_type: &ActionType,
    detail: &str,
    write_failure_history: &impl Fn(&Checkpoint, &str) -> Result<(), ApplyError>,
) -> ApplyError {
    if let Err(hist_err) = write_failure_history(checkpoint, action_id) {
        eprintln!("warning: failed to write failure history: {hist_err}");
    }
    ApplyError::ActionFailed {
        action_id: action_id.to_owned(),
        action_type: action_type_label(action_type).to_owned(),
        detail: detail.to_owned(),
    }
}

// ---------------------------------------------------------------------------
// Execute loop (shared between fresh and resume)
// ---------------------------------------------------------------------------

fn run_execute_loop<R: CommandRunner + Sync>(
    runner: &R,
    fs: &dyn Filesystem,
    checkpoint: &mut Checkpoint,
    config: &Config,
    progress: ProgressMode,
    json: bool,
) -> Result<(), ApplyError> {
    let output = progress::resolve_progress_output(progress, {
        use std::io::IsTerminal;
        std::io::stderr().is_terminal()
    }, json);
    run_execute_loop_with(runner, fs, checkpoint, config, output, checkpoint_write_failure_history)
}

fn run_execute_loop_with<R: CommandRunner + Sync>(
    runner: &R,
    fs: &dyn Filesystem,
    checkpoint: &mut Checkpoint,
    config: &Config,
    output: ProgressOutput,
    write_failure_history: impl Fn(&Checkpoint, &str) -> Result<(), ApplyError>,
) -> Result<(), ApplyError> {
    for i in 0..checkpoint.actions.len() {
        if matches!(checkpoint.actions[i].state, ActionState::Completed) {
            println!(
                "[{}] {} — already completed, skipping.",
                checkpoint.actions[i].id,
                action_type_label(&checkpoint.actions[i].action_type),
            );
            continue;
        }

        println!(
            "[{}] {} target={}",
            checkpoint.actions[i].id,
            action_type_label(&checkpoint.actions[i].action_type),
            checkpoint.actions[i].target,
        );

        checkpoint.actions[i].state = ActionState::InProgress;
        checkpoint.updated_at = now_utc();
        checkpoint_write(checkpoint)?;

        let action_snapshot = checkpoint.actions[i].clone();
        match execute_action(runner, fs, &action_snapshot, config, checkpoint.is_bootstrap, output) {
            Ok(()) => {
                checkpoint.actions[i].state = ActionState::Completed;
                checkpoint.last_completed_action_id = checkpoint.actions[i].id.clone();
            }
            Err(e) => {
                checkpoint.actions[i].state =
                    ActionState::Failed { error: format!("{e}") };
                checkpoint_write(checkpoint)?;
                return Err(record_failure_and_build_error(
                    checkpoint,
                    &action_snapshot.id,
                    &action_snapshot.action_type,
                    &format!("{e}"),
                    &write_failure_history,
                ));
            }
        }

        checkpoint.updated_at = now_utc();
        checkpoint_write(checkpoint)?;

        // Test hook: BRAID_TEST_FAIL_AFTER_ACTION
        if let Ok(fail_after) = std::env::var("BRAID_TEST_FAIL_AFTER_ACTION") {
            if fail_after == checkpoint.actions[i].id {
                return Err(ApplyError::ActionFailed {
                    action_id: checkpoint.actions[i].id.clone(),
                    action_type: "test_hook".into(),
                    detail: "simulated failure".into(),
                });
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Main orchestrator
// ---------------------------------------------------------------------------

pub fn cmd_apply(config_path: &Path, flags: &ApplyFlags) -> Result<(), ApplyError> {
    let runner = crate::cmd::RealRunner;
    let fs = crate::probe::RealFilesystem;
    cmd_apply_with(config_path, flags, &runner, &fs)
}

pub fn cmd_apply_with<R: CommandRunner + Sync>(
    config_path: &Path,
    flags: &ApplyFlags,
    runner: &R,
    fs: &dyn Filesystem,
) -> Result<(), ApplyError> {
    if flags.resume {
        resume_apply(config_path, flags, runner, fs)
    } else {
        fresh_apply(config_path, flags, runner, fs)
    }
}

fn fresh_apply<R: CommandRunner + Sync>(
    config_path: &Path,
    flags: &ApplyFlags,
    runner: &R,
    fs: &dyn Filesystem,
) -> Result<(), ApplyError> {
    // Check no checkpoint exists
    if Path::new(CHECKPOINT_FILE).exists() {
        return Err(ApplyError::CheckpointExists {
            path: CHECKPOINT_FILE.to_owned(),
        });
    }

    // Read config
    let (config, raw_text) = config_read_raw(config_path)?;
    let hash = config_hash(&raw_text);

    // Probe
    let config_disks: Vec<_> = config
        .disks()
        .iter()
        .map(|d| probe_config_disk(runner, fs, d))
        .collect::<Result<Vec<_>, _>>()?;

    let pool = probe_pool(runner, config.mount_point())?;

    // Compute plan
    let plan_flags = PlanFlags {
        allow_remove_missing: flags.allow_remove_missing,
        allow_remove_ambiguous: flags.allow_remove_ambiguous,
    };

    let outcome = compute_plan(&config, &config_disks, &pool, &plan_flags);
    let report = to_plan_report(&outcome, &config);

    match outcome {
        PlanOutcome::Blocked { blocked_reasons, .. } => {
            let msgs: Vec<&str> = blocked_reasons.iter().map(|b| b.message.as_str()).collect();
            return Err(ApplyError::Blocked(msgs.join("; ")));
        }
        PlanOutcome::Applicable {
            plan_id,
            actions,
            warnings,
            confirmations,
        } => {
            // Print warnings
            for w in &warnings {
                eprintln!("warning: {}", w.message);
            }

            // Count mutation actions
            let mutation_count = actions
                .iter()
                .filter(|a| !is_verify_action(&a.action_type))
                .count();

            if mutation_count == 0 {
                println!("Nothing to do.");
                return Ok(());
            }

            // Check confirmations
            check_confirmations(&confirmations)?;

            // Determine bootstrap
            let is_bootstrap = !pool.mounted && pool.total_devices == 0;

            // Build checkpoint
            let now = now_utc();
            let mut checkpoint = Checkpoint {
                schema_version: 1,
                plan_id,
                mount_point: config.mount_point().to_owned(),
                status: report.status,
                config_hash: hash,
                created_at: now.clone(),
                updated_at: now,
                last_completed_action_id: String::new(),
                is_bootstrap,
                actions,
                warnings,
                confirmations,
                run_outcome: None,
                failed_action_id: None,
            };

            // Write initial checkpoint
            checkpoint_write(&checkpoint)?;

            // Execute loop
            run_execute_loop(runner, fs, &mut checkpoint, &config, flags.progress, flags.json)?;

            // Print footer
            let mutation_completed = checkpoint
                .actions
                .iter()
                .filter(|a| !is_verify_action(&a.action_type) && matches!(a.state, ActionState::Completed))
                .count();
            let warnings_skipped = checkpoint
                .warnings
                .iter()
                .filter(|w| {
                    matches!(
                        w.code,
                        WarningCode::DiskAbsentSkipped | WarningCode::InitRequired
                    )
                })
                .count();

            println!(
                "Applied {} actions, skipped {} with warnings, blocked 0",
                mutation_completed, warnings_skipped
            );

            // Finalize
            checkpoint_finalize(&checkpoint)?;
        }
    }

    Ok(())
}

fn resume_apply<R: CommandRunner + Sync>(
    config_path: &Path,
    flags: &ApplyFlags,
    runner: &R,
    fs: &dyn Filesystem,
) -> Result<(), ApplyError> {
    // Read checkpoint
    let mut checkpoint = checkpoint_read()?;

    // Read config and check staleness
    let (config, raw_text) = config_read_raw(config_path)?;
    let hash = config_hash(&raw_text);

    if hash != checkpoint.config_hash {
        return Err(ApplyError::StaleCheckpoint);
    }

    // Re-check confirmations
    check_confirmations(&checkpoint.confirmations)?;

    // Validate resume targets
    validate_resume_targets(fs, &checkpoint.actions)?;

    // Execute loop
    run_execute_loop(runner, fs, &mut checkpoint, &config, flags.progress, flags.json)?;

    // Print footer
    let mutation_completed = checkpoint
        .actions
        .iter()
        .filter(|a| !is_verify_action(&a.action_type) && matches!(a.state, ActionState::Completed))
        .count();
    let warnings_skipped = checkpoint
        .warnings
        .iter()
        .filter(|w| {
            matches!(
                w.code,
                WarningCode::DiskAbsentSkipped | WarningCode::InitRequired
            )
        })
        .count();

    println!(
        "Applied {} actions, skipped {} with warnings, blocked 0",
        mutation_completed, warnings_skipped
    );

    // Finalize
    checkpoint_finalize(&checkpoint)?;

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};

    fn ok_raw(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
            exit_status: exit_code,
        }
    }

    struct MockFs {
        paths: Vec<String>,
        block_devices: Vec<String>,
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                block_devices: vec![],
            }
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path.to_string())
        }

        fn is_block_device(&self, path: &str) -> bool {
            self.block_devices.contains(&path.to_string())
        }
    }

    #[test]
    fn check_confirmations_empty_is_ok() {
        assert!(check_confirmations_with(&[], "").is_ok());
    }

    #[test]
    fn check_confirmations_missing() {
        let confirmations = vec![Confirmation {
            action_id: "a1".to_owned(),
            phrase: "do the thing".to_owned(),
        }];
        let err = check_confirmations_with(&confirmations, "").unwrap_err();
        assert!(matches!(err, ApplyError::ConfirmationMissing { .. }));
    }

    #[test]
    fn check_confirmations_semicolon_multi() {
        let confirmations = vec![
            Confirmation {
                action_id: "a1".to_owned(),
                phrase: "phrase one".to_owned(),
            },
            Confirmation {
                action_id: "a2".to_owned(),
                phrase: "phrase two".to_owned(),
            },
        ];
        let result = check_confirmations_with(&confirmations, "phrase one; phrase two");
        assert!(result.is_ok());
    }

    #[test]
    fn validate_resume_targets_ok() {
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1", "/dev/mapper/disk-1"]);
        let actions = vec![
            Action {
                id: "a1".to_owned(),
                action_type: ActionType::OpenLuks,
                target: "/dev/disk/by-id/disk-1".to_owned(),
                preconditions: vec![],
                state: ActionState::Completed,
                commands: vec![],
            },
            Action {
                id: "a2".to_owned(),
                action_type: ActionType::AddDiskBtrfsAdd,
                target: "/dev/mapper/disk-1".to_owned(),
                preconditions: vec!["a1".to_owned()],
                state: ActionState::Pending,
                commands: vec![],
            },
        ];
        assert!(validate_resume_targets(&fs, &actions).is_ok());
    }

    #[test]
    fn validate_resume_targets_missing() {
        let fs = MockFs::new(&[]);
        let actions = vec![Action {
            id: "a1".to_owned(),
            action_type: ActionType::OpenLuks,
            target: "/dev/disk/by-id/disk-1".to_owned(),
            preconditions: vec![],
            state: ActionState::Pending,
            commands: vec![],
        }];
        let err = validate_resume_targets(&fs, &actions).unwrap_err();
        assert!(matches!(err, ApplyError::ResumeTargetMissing { .. }));
    }

    /// Bug: ADD_DISK_BTRFS_ADD targets /dev/mapper/<name> which doesn't exist
    /// until the preceding OPEN_LUKS runs. Resume should not reject this case.
    #[test]
    fn validate_resume_targets_allows_mapper_behind_pending_open() {
        // Only the by-id device exists; mapper is not yet open.
        let fs = MockFs::new(&["/dev/disk/by-id/disk-2"]);
        let actions = vec![
            Action {
                id: "a1".to_owned(),
                action_type: ActionType::OpenLuks,
                target: "/dev/disk/by-id/disk-2".to_owned(),
                preconditions: vec![],
                state: ActionState::Pending,
                commands: vec![],
            },
            Action {
                id: "a2".to_owned(),
                action_type: ActionType::AddDiskBtrfsAdd,
                target: "/dev/mapper/disk-2".to_owned(),
                preconditions: vec!["a1".to_owned()],
                state: ActionState::Pending,
                commands: vec![],
            },
        ];
        // Should succeed: mapper will be created by preceding OPEN_LUKS.
        let result = validate_resume_targets(&fs, &actions);
        assert!(
            result.is_ok(),
            "resume should allow mapper-missing when OPEN_LUKS precondition is pending: {result:?}"
        );
    }

    #[test]
    fn open_luks_skips_when_mapper_exists() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/disk-1".to_owned(),
                },
                ok_raw("cryptsetup isLuks", ""),
            );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1", "/dev/mapper/disk-1"]);
        let result = execute_open_luks_with(&runner, &fs, "/dev/disk/by-id/disk-1", "testpass");
        assert!(result.is_ok());
    }

    #[test]
    fn open_luks_passes_passphrase_via_stdin() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/disk-1".to_owned(),
                },
                ok_raw("cryptsetup isLuks", ""),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/disk-1".to_owned(),
                    mapper: "disk-1".to_owned(),
                },
                b"secret123".to_vec(),
                ok_raw("cryptsetup luksOpen", ""),
            );
        let fs = MockFs::new(&["/dev/disk/by-id/disk-1"]);
        let result = execute_open_luks_with(&runner, &fs, "/dev/disk/by-id/disk-1", "secret123");
        assert!(result.is_ok());
    }

    #[test]
    fn btrfs_add_has_btrfs_no_such_file_mount_must_fail() {
        let mount_point = "/tmp/braid-test-mount-no-such";
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: mount_point.to_owned(),
                },
                err_raw(&format!("mountpoint -q {mount_point}"), 1, ""),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: "/dev/mapper/disk-1".to_owned(),
                },
                ok_raw("btrfs filesystem show /dev/mapper/disk-1", "Label: none"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScan {
                    device: "/dev/mapper/disk-1".to_owned(),
                },
                ok_raw("btrfs device scan /dev/mapper/disk-1", ""),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/disk-1".to_owned(),
                    mount_point: mount_point.to_owned(),
                },
                err_raw(
                    &format!("mount /dev/mapper/disk-1 {mount_point}"),
                    32,
                    &format!("mount: {mount_point}: No such file or directory."),
                ),
            );

        let err = execute_btrfs_add(&runner, "/dev/mapper/disk-1", mount_point, true)
            .expect_err("no-such-file mount error must fail hard, not defer");

        assert!(
            matches!(err, ApplyError::ActionFailed { .. }),
            "expected ActionFailed, got: {err:?}"
        );
    }

    #[test]
    fn btrfs_add_has_btrfs_missing_members_mount_is_deferred() {
        let mount_point = "/tmp/braid-test-mount-missing";
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: mount_point.to_owned(),
                },
                err_raw(&format!("mountpoint -q {mount_point}"), 1, ""),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: "/dev/mapper/disk-1".to_owned(),
                },
                ok_raw("btrfs filesystem show /dev/mapper/disk-1", "Label: none"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScan {
                    device: "/dev/mapper/disk-1".to_owned(),
                },
                ok_raw("btrfs device scan /dev/mapper/disk-1", ""),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/disk-1".to_owned(),
                    mount_point: mount_point.to_owned(),
                },
                err_raw(
                    &format!("mount /dev/mapper/disk-1 {mount_point}"),
                    32,
                    &format!("ERROR: cannot mount {mount_point}: missing devid 2"),
                ),
            );

        let result = execute_btrfs_add(&runner, "/dev/mapper/disk-1", mount_point, true);
        assert!(result.is_ok(), "missing-member mount should defer, got: {result:?}");
    }

    #[test]
    fn btrfs_add_has_btrfs_fsconfig_dmesg_mount_is_deferred() {
        // Real kernel error when btrfs open_ctree fails due to missing members:
        // "fsconfig() failed: No such file or directory" + dmesg hint.
        let mount_point = "/tmp/braid-test-mount-fsconfig";
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: mount_point.to_owned(),
                },
                err_raw(&format!("mountpoint -q {mount_point}"), 1, ""),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: "/dev/mapper/disk-1".to_owned(),
                },
                ok_raw("btrfs filesystem show /dev/mapper/disk-1", "Label: none"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScan {
                    device: "/dev/mapper/disk-1".to_owned(),
                },
                ok_raw("btrfs device scan /dev/mapper/disk-1", ""),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/disk-1".to_owned(),
                    mount_point: mount_point.to_owned(),
                },
                err_raw(
                    &format!("mount /dev/mapper/disk-1 {mount_point}"),
                    32,
                    &format!(
                        "mount: {mount_point}: fsconfig() failed: No such file or directory.\n\
                         \x20      dmesg(1) may have more information after failed mount system call."
                    ),
                ),
            );

        let result = execute_btrfs_add(&runner, "/dev/mapper/disk-1", mount_point, true);
        assert!(
            result.is_ok(),
            "fsconfig+dmesg mount error should defer, got: {result:?}"
        );
    }

    #[test]
    fn btrfs_add_has_btrfs_fsconfig_alone_must_fail() {
        // fsconfig without dmesg hint — not enough signal to defer.
        let mount_point = "/tmp/braid-test-mount-fsconfig-only";
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: mount_point.to_owned(),
                },
                err_raw(&format!("mountpoint -q {mount_point}"), 1, ""),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: "/dev/mapper/disk-1".to_owned(),
                },
                ok_raw("btrfs filesystem show /dev/mapper/disk-1", "Label: none"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScan {
                    device: "/dev/mapper/disk-1".to_owned(),
                },
                ok_raw("btrfs device scan /dev/mapper/disk-1", ""),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/disk-1".to_owned(),
                    mount_point: mount_point.to_owned(),
                },
                err_raw(
                    &format!("mount /dev/mapper/disk-1 {mount_point}"),
                    32,
                    &format!("mount: {mount_point}: fsconfig() failed: unknown error"),
                ),
            );

        let err = execute_btrfs_add(&runner, "/dev/mapper/disk-1", mount_point, true)
            .expect_err("fsconfig without dmesg hint must fail hard");

        assert!(
            matches!(err, ApplyError::ActionFailed { .. }),
            "expected ActionFailed, got: {err:?}"
        );
    }

    #[test]
    fn probe_device_has_btrfs_no_btrfs_message() {
        // Real btrfs output: "ERROR: no btrfs on /dev/dm-0"
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: "/dev/mapper/disk-1".to_owned(),
            },
            err_raw(
                "btrfs filesystem show /dev/mapper/disk-1",
                1,
                "ERROR: no btrfs on /dev/dm-0",
            ),
        );

        assert!(matches!(
            probe_device_has_btrfs(&runner, "/dev/mapper/disk-1"),
            DeviceBtrfsProbe::NoBtrfs
        ));
    }

    #[test]
    fn probe_device_has_btrfs_not_valid_message() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: "/dev/mapper/disk-1".to_owned(),
            },
            err_raw(
                "btrfs filesystem show /dev/mapper/disk-1",
                1,
                "ERROR: not a valid btrfs filesystem on /dev/dm-0",
            ),
        );

        assert!(matches!(
            probe_device_has_btrfs(&runner, "/dev/mapper/disk-1"),
            DeviceBtrfsProbe::NoBtrfs
        ));
    }

    #[test]
    fn probe_device_has_btrfs_has_superblock() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: "/dev/mapper/disk-1".to_owned(),
            },
            ok_raw(
                "btrfs filesystem show /dev/mapper/disk-1",
                "Label: none  uuid: abc-123\n\tTotal devices 2",
            ),
        );

        assert!(matches!(
            probe_device_has_btrfs(&runner, "/dev/mapper/disk-1"),
            DeviceBtrfsProbe::HasBtrfs
        ));
    }

    #[test]
    fn probe_device_has_btrfs_unknown_error() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: "/dev/mapper/disk-1".to_owned(),
            },
            err_raw(
                "btrfs filesystem show /dev/mapper/disk-1",
                1,
                "ERROR: unexpected internal error",
            ),
        );

        assert!(matches!(
            probe_device_has_btrfs(&runner, "/dev/mapper/disk-1"),
            DeviceBtrfsProbe::Unknown(_)
        ));
    }

    #[test]
    fn close_luks_is_non_fatal() {
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupClose {
                mapper: "disk-1".to_owned(),
            },
            err_raw("cryptsetup close disk-1", 5, "Device is busy"),
        );
        let result = execute_close_luks(&runner, "/dev/mapper/disk-1");
        assert!(result.is_ok());
    }

    #[test]
    fn is_verify_action_classification() {
        assert!(is_verify_action(&ActionType::VerifyPoolHealth));
        assert!(is_verify_action(&ActionType::VerifyExpectedDiskSet));
        assert!(!is_verify_action(&ActionType::OpenLuks));
        assert!(!is_verify_action(&ActionType::AddDiskBtrfsAdd));
    }

    #[test]
    fn action_failure_not_masked_by_history_write_failure() {
        let cp = Checkpoint {
            schema_version: 1,
            plan_id: "test".to_owned(),
            mount_point: "/mnt/storage".to_owned(),
            status: PlanStatus::Applicable,
            config_hash: "sha256:0".to_owned(),
            created_at: "t0".to_owned(),
            updated_at: "t0".to_owned(),
            last_completed_action_id: String::new(),
            is_bootstrap: false,
            actions: vec![],
            warnings: vec![],
            confirmations: vec![],
            run_outcome: None,
            failed_action_id: None,
        };

        let err = record_failure_and_build_error(
            &cp,
            "a1",
            &ActionType::OpenLuks,
            "device not found",
            &|_, _| Err(ApplyError::Io("disk full".into())),
        );

        match err {
            ApplyError::ActionFailed {
                action_id,
                detail,
                ..
            } => {
                assert_eq!(action_id, "a1");
                assert_eq!(detail, "device not found");
            }
            other => panic!("expected ActionFailed, got: {other}"),
        }
    }
}
