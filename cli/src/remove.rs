use crate::checkpoint::{
    clear_checkpoint, hash_args, load_checkpoint, save_checkpoint, CheckpointValidity,
    OpCheckpoint, PoolFingerprint,
};
use crate::cmd::CommandRunner;
use crate::config::{config_hash, config_read_raw, mapper_name};
use crate::pool::{pool_balance_single, pool_remove_device};
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

    if !in_pool {
        let mut msg = format!("disk '{}' not found in pool.", name);
        if pool.missing_count > 0 {
            msg.push_str(&format!(
                " ({} missing device{} detected. Use 'braid remove-missing' to remove missing devices.)",
                pool.missing_count,
                if pool.missing_count == 1 { "" } else { "s" }
            ));
        }
        return Err(RemoveError::Validation(msg));
    }

    let steps = compile_remove_present_steps(name, &mn, &pool)?;

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
    let args_parts: Vec<String> = vec!["remove".into(), name.into()];
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

    let remaining = pool.devices.len() - 1;
    if remaining == 1 {
        eprintln!("Converting pool from RAID1 to single profile...");
        pool_balance_single(runner, config.mount_point(), progress)?;
    }

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

    clear_checkpoint();
    eprintln!("Done. If not already done: remove '{}' from braid.disks and run nixos-rebuild switch.", name);
    Ok(())
}

fn compile_remove_present_steps(
    _name: &str,
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
            description: "btrfs balance -dconvert=single -mconvert=single -f (RAID1 → single)".into(),
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

fn now_iso() -> String {
    use time::format_description::well_known::Iso8601;
    time::OffsetDateTime::now_utc()
        .format(&Iso8601::DEFAULT)
        .unwrap_or_else(|_| "unknown".into())
}
