use crate::checkpoint::{
    clear_checkpoint, hash_args, load_checkpoint, save_checkpoint, CheckpointValidity,
    OpCheckpoint, PoolFingerprint,
};
use crate::cmd::CommandRunner;
use crate::config::{config_hash, config_read_raw, mapper_name};
use crate::pool::{pool_remove_device, pool_remove_devid, pool_remove_missing};
use crate::probe::{probe_pool, ProbeError};
use crate::progress::ProgressOutput;
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
    Checkpoint(#[from] std::io::Error),
}

pub struct RemoveStep {
    pub risk: &'static str,
    pub description: String,
}

pub fn cmd_remove<R: CommandRunner + Sync>(
    runner: &R,
    config_path: &Path,
    name: &str,
    missing_id: Option<u64>,
    dry_run: bool,
    yes: bool,
    progress: ProgressOutput,
) -> Result<(), RemoveError> {
    let (config, config_raw) = config_read_raw(config_path)?;

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

    let mn = mapper_name(name);

    // Is the disk present in the pool?
    let in_pool = pool.devices.iter().any(|d| d.mapper == mn);

    let steps = if in_pool {
        compile_remove_present_steps(name, &mn, &pool)?
    } else {
        compile_remove_missing_steps(name, missing_id, &pool)?
    };

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

    // Check checkpoint
    let args_parts: Vec<String> = if let Some(id) = missing_id {
        vec!["remove".into(), name.into(), id.to_string()]
    } else {
        vec!["remove".into(), name.into()]
    };
    let args_refs: Vec<&str> = args_parts.iter().map(|s| s.as_str()).collect();
    let args_hash = hash_args(&args_refs);

    match load_checkpoint(&config_raw, &pool, "remove", &args_hash) {
        CheckpointValidity::Valid(cp) => {
            eprintln!(
                "Resuming previous 'braid remove {}' interrupted at step {}.",
                name, cp.step
            );
        }
        CheckpointValidity::Stale(reason) => {
            eprintln!("Previous checkpoint invalidated: {reason}. Starting fresh.");
        }
        CheckpointValidity::None => {}
    }

    // Confirm
    if !yes {
        if in_pool {
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
                eprintln!("Remove {} from pool? Data will migrate off this disk.", name);
                eprint!("Type 'yes' to continue: ");
                let mut input = String::new();
                std::io::stdin().read_line(&mut input).map_err(|e| {
                    RemoveError::Validation(format!("failed to read confirmation: {e}"))
                })?;
                if input.trim() != "yes" {
                    return Err(RemoveError::Validation("aborted by user".into()));
                }
            }
        } else {
            eprintln!("Remove missing device from pool?");
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
    let cp = OpCheckpoint {
        op: "remove".into(),
        disk: name.into(),
        step: 1,
        started_at: now_iso(),
        config_hash: config_hash(&config_raw),
        args_hash,
        pool_fingerprint: PoolFingerprint::from_pool_state(&pool),
        old_disk: None,
        new_disk: None,
    };
    save_checkpoint(&cp)?;

    if in_pool {
        let device = format!("/dev/mapper/{}", mn.0);
        eprintln!("Removing {} from pool (data will migrate)...", name);
        pool_remove_device(runner, &device, config.mount_point(), progress)?;

        // Close LUKS mapper
        let result = runner.run(&crate::cmd::CmdRequest::CryptsetupClose {
            mapper: mn.0.clone(),
        })?;
        if result.exit_status != 0 {
            eprintln!(
                "Warning: failed to close LUKS mapper {} (exit {})",
                mn, result.exit_status
            );
        }
    } else if let Some(devid) = missing_id {
        eprintln!("Removing missing device (devid {}) from pool...", devid);
        pool_remove_devid(runner, config.mount_point(), devid)?;
    } else {
        eprintln!("Removing missing device from pool...");
        pool_remove_missing(runner, config.mount_point())?;
    }

    clear_checkpoint();
    eprintln!("Done. If not already done: remove '{}' from braid.disks and run nixos-rebuild switch.", name);
    Ok(())
}

fn compile_remove_present_steps(
    name: &str,
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

fn compile_remove_missing_steps(
    name: &str,
    missing_id: Option<u64>,
    pool: &PoolState,
) -> Result<Vec<RemoveStep>, RemoveError> {
    if pool.missing_count == 0 {
        return Err(RemoveError::Validation(format!(
            "disk '{}' not found in pool and no missing devices detected",
            name
        )));
    }

    if pool.missing_count > 1 && missing_id.is_none() {
        return Err(RemoveError::Validation(format!(
            "multiple missing devices ({} missing). Pass --missing-id <devid> to target a specific one. Use 'braid status --verbose' to see device IDs.",
            pool.missing_count
        )));
    }

    let mut steps = Vec::new();
    if let Some(devid) = missing_id {
        steps.push(RemoveStep {
            risk: "long",
            description: format!("btrfs device remove {} (target specific missing device)", devid),
        });
    } else {
        steps.push(RemoveStep {
            risk: "long",
            description: "btrfs device remove missing".into(),
        });
    }
    Ok(steps)
}

fn now_iso() -> String {
    use time::format_description::well_known::Iso8601;
    time::OffsetDateTime::now_utc()
        .format(&Iso8601::DEFAULT)
        .unwrap_or_else(|_| "unknown".into())
}
