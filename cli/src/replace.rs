use crate::checkpoint::{
    clear_checkpoint, hash_args, load_checkpoint, save_checkpoint, CheckpointValidity,
    OpCheckpoint, PoolFingerprint,
};
use crate::cmd::CommandRunner;
use crate::config::{config_hash, config_read_raw, mapper_name};
use crate::disk_map;
use crate::luks::{
    backup_luks_header, ensure_luks_open, luks_format,
    luks_opts_from_env, read_passphrase, verify_passphrase,
};
use crate::pool::{pool_add_device, pool_balance_raid1, pool_remove_devid, pool_remove_missing};
use crate::probe::{probe_config_disk, probe_pool, Filesystem, ProbeError};
use crate::progress::ProgressOutput;
use crate::types::*;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum ReplaceError {
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
    Checkpoint(#[from] std::io::Error),
}

pub struct ReplaceStep {
    pub risk: &'static str,
    pub description: String,
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_replace<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config_path: &Path,
    old_name: &str,
    new_name: &str,
    missing_id: Option<u64>,
    dry_run: bool,
    yes: bool,
    passphrase_file: Option<&Path>,
    progress: ProgressOutput,
) -> Result<(), ReplaceError> {
    let (config, config_raw) = config_read_raw(config_path)?;
    let disk_map_state = disk_map::load_disk_map();
    disk_map::validate_config_key_stability(&config, &disk_map_state)
        .map_err(|e| ReplaceError::Validation(e.to_string()))?;

    // --new must be in config
    let new_disk = config.disk_by_name(new_name).ok_or_else(|| {
        let available: Vec<_> = config.names().into_iter().map(|s| s.as_str()).collect();
        ReplaceError::Validation(format!(
            "new disk '{}' not found in config. Available: {}",
            new_name,
            available.join(", ")
        ))
    })?;

    let pool = match probe_pool(runner, config.mount_point()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return Err(ReplaceError::Validation(
                "pool is not mounted. Cannot replace.".into(),
            ));
        }
        Err(e) => return Err(ReplaceError::Probe(e)),
    };

    if !pool.mounted {
        return Err(ReplaceError::Validation(
            "pool is not mounted. Cannot replace.".into(),
        ));
    }

    // Resolve --old
    let old_mn = mapper_name(old_name);
    let old_in_pool = pool.devices.iter().find(|d| d.mapper == old_mn);

    if let Some(_dev) = old_in_pool {
        // Device is alive — refuse
        return Err(ReplaceError::Validation(format!(
            "disk '{}' is alive in the pool. Use 'braid remove {}' + 'braid add {}' separately.",
            old_name, old_name, new_name
        )));
    }

    // Old disk not found as alive — check missing devices
    let eviction_target = resolve_eviction_target(old_name, missing_id, &pool)?;

    // Probe --new disk state
    let new_probed = probe_config_disk(runner, fs, new_name, new_disk)?;

    // Compile steps
    let steps = compile_replace_steps(new_name, &new_probed, &eviction_target, &config)?;

    if dry_run {
        for step in &steps {
            println!("[{:<11}] {}", step.risk, step.description);
        }
        return Ok(());
    }

    // Check checkpoint
    let args_parts: Vec<String> = if let Some(id) = missing_id {
        vec![
            "replace".into(),
            old_name.into(),
            new_name.into(),
            id.to_string(),
        ]
    } else {
        vec!["replace".into(), old_name.into(), new_name.into()]
    };
    let args_refs: Vec<&str> = args_parts.iter().map(|s| s.as_str()).collect();
    let args_hash = hash_args(&args_refs);

    match load_checkpoint(&config_raw, &pool, "replace", &args_hash) {
        CheckpointValidity::Valid(cp) => {
            eprintln!(
                "Resuming previous 'braid replace' interrupted at step {}.",
                cp.step
            );
        }
        CheckpointValidity::Stale(reason) => {
            eprintln!("Previous checkpoint invalidated: {reason}. Starting fresh.");
        }
        CheckpointValidity::None => {}
    }

    // Confirm
    if !yes {
        eprintln!(
            "{}",
            replace_confirm_message(&new_probed.state, old_name, new_name, &new_disk.by_id.0)
        );
        eprint!("Type 'yes' to continue: ");
        let mut input = String::new();
        std::io::stdin()
            .read_line(&mut input)
            .map_err(|e| ReplaceError::Validation(format!("failed to read confirmation: {e}")))?;
        if input.trim() != "yes" {
            return Err(ReplaceError::Validation("aborted by user".into()));
        }
    }

    // Read passphrase
    let passphrase = read_passphrase(passphrase_file, yes)?;
    let new_mn = mapper_name(new_name);

    // Step 1: Init new disk if needed
    match new_probed.state {
        ConfigDiskState::Absent => {
            return Err(ReplaceError::Validation(format!(
                "new disk '{}' ({}) is not present. Is it plugged in?",
                new_name, new_disk.by_id
            )));
        }
        ConfigDiskState::PresentNotLuks => {
            // If pool exists, verify passphrase against existing member
            if let Some(existing) = pool.devices.first() {
                let status_raw = runner.run(&crate::cmd::CmdRequest::CryptsetupStatus {
                    mapper: existing.mapper.0.clone(),
                })?;
                let status = crate::parse::parse_cryptsetup_status(&status_raw)?;
                if let Some(underlying) = status.device {
                    let ok = verify_passphrase(runner, &underlying, &passphrase)?;
                    if !ok {
                        return Err(ReplaceError::Validation(
                            "passphrase does not match existing pool member".into(),
                        ));
                    }
                }
            }

            let luks_opts = luks_opts_from_env();
            luks_format(runner, &new_disk.by_id.0, &passphrase, &luks_opts)?;
            eprintln!("LUKS formatted: {}", new_disk.by_id);

            let backup_path = backup_luks_header(runner, &new_disk.by_id.0, &new_mn.0)?;
            eprintln!("LUKS header backed up: {}", backup_path.display());

            ensure_luks_open(runner, fs, new_name, new_disk, &passphrase)?;
            eprintln!("LUKS opened: {} → {}", new_disk.by_id, new_mn);
        }
        ConfigDiskState::PresentLuks { mapper_open, .. } => {
            if !mapper_open {
                ensure_luks_open(runner, fs, new_name, new_disk, &passphrase)?;
                eprintln!("LUKS opened: {} → {}", new_disk.by_id, new_mn);
            }
        }
    }

    // Step 2: Add new disk to pool
    let new_mapper_path = format!("/dev/mapper/{}", new_mn.0);
    pool_add_device(runner, &new_mapper_path, config.mount_point())?;
    eprintln!("Device added to pool: {}", new_mn);

    // Step 3: Balance to RAID1 (with checkpoint)
    let cp = OpCheckpoint {
        op: "replace".into(),
        disk: new_name.into(),
        step: 3,
        started_at: now_iso(),
        config_hash: config_hash(&config_raw),
        args_hash: args_hash.clone(),
        pool_fingerprint: PoolFingerprint::from_pool_state(&pool),
        old_disk: Some(old_name.into()),
        new_disk: Some(new_name.into()),
    };
    save_checkpoint(&cp)?;

    eprintln!("Balancing to RAID1...");
    pool_balance_raid1(runner, config.mount_point(), progress)?;
    eprintln!("Balance complete.");

    // Step 4: Evict dead disk
    match eviction_target {
        EvictionTarget::Devid(devid) => {
            eprintln!("Removing dead device (devid {})...", devid);
            pool_remove_devid(runner, config.mount_point(), devid)?;
        }
        EvictionTarget::Missing => {
            eprintln!("Removing missing device...");
            pool_remove_missing(runner, config.mount_point())?;
        }
    }

    clear_checkpoint();

    // Update disk map (best effort — never fail the replace)
    let pool_after = probe_pool(runner, config.mount_point()).ok();
    let new_mn = mapper_name(new_name);
    let mut map_warning: Option<String> = None;
    disk_map::update_disk_map_best_effort(|map| {
        map_warning = apply_replace_disk_map_update(
            map,
            old_name,
            new_name,
            &new_disk.by_id.0,
            &new_mn,
            pool_after.as_ref(),
        );
    });
    if let Some(w) = map_warning {
        eprintln!("{w}");
    }

    eprintln!(
        "Done. Replaced {} with {}. If not already done: update braid.disks and run nixos-rebuild switch.",
        old_name, new_name
    );
    Ok(())
}

enum EvictionTarget {
    Devid(u64),
    Missing,
}

fn resolve_eviction_target(
    old_name: &str,
    missing_id: Option<u64>,
    pool: &PoolState,
) -> Result<EvictionTarget, ReplaceError> {
    if let Some(devid) = missing_id {
        return Ok(EvictionTarget::Devid(devid));
    }

    if pool.missing_count == 0 {
        return Err(ReplaceError::Validation(format!(
            "no dead disk to replace. '{}' not found in pool and no missing devices.",
            old_name
        )));
    }

    if pool.missing_count == 1 {
        return Ok(EvictionTarget::Missing);
    }

    Err(ReplaceError::Validation(format!(
        "multiple missing devices ({} missing). Pass --missing-id <devid> to target the specific dead disk. Use 'braid status --verbose' to see device IDs.",
        pool.missing_count
    )))
}

fn compile_replace_steps(
    new_name: &str,
    new_probed: &ConfigDisk,
    eviction_target: &EvictionTarget,
    config: &crate::config::Config,
) -> Result<Vec<ReplaceStep>, ReplaceError> {
    let new_disk = config.disk_by_name(new_name).unwrap();
    let new_mn = mapper_name(new_name);
    let mut steps = Vec::new();

    match &new_probed.state {
        ConfigDiskState::Absent => {
            return Err(ReplaceError::Validation(format!(
                "new disk '{}' ({}) is not present. Is it plugged in?",
                new_name, new_disk.by_id
            )));
        }
        ConfigDiskState::PresentNotLuks => {
            steps.push(ReplaceStep {
                risk: "destructive",
                description: format!("LUKS format {}", new_disk.by_id),
            });
            steps.push(ReplaceStep {
                risk: "safe",
                description: format!("LUKS open → {}", new_mn),
            });
        }
        ConfigDiskState::PresentLuks { mapper_open, .. } => {
            if !mapper_open {
                steps.push(ReplaceStep {
                    risk: "safe",
                    description: format!("LUKS open → {}", new_mn),
                });
            }
        }
    }

    steps.push(ReplaceStep {
        risk: "safe",
        description: format!(
            "btrfs device add /dev/mapper/{} {}",
            new_mn,
            config.mount_point()
        ),
    });
    steps.push(ReplaceStep {
        risk: "long",
        description: "btrfs balance to RAID1".into(),
    });

    match eviction_target {
        EvictionTarget::Devid(devid) => {
            steps.push(ReplaceStep {
                risk: "safe",
                description: format!("btrfs device remove {}", devid),
            });
        }
        EvictionTarget::Missing => {
            steps.push(ReplaceStep {
                risk: "safe",
                description: "btrfs device remove missing".into(),
            });
        }
    }

    Ok(steps)
}

fn replace_confirm_message(
    new_state: &ConfigDiskState,
    old_name: &str,
    new_name: &str,
    by_id: &str,
) -> String {
    match new_state {
        ConfigDiskState::PresentNotLuks => format!(
            "WARNING: This will LUKS-format {} ({}). Existing data will be inaccessible.",
            new_name, by_id
        ),
        _ => format!("Replace {} (dead) with {} (new)?", old_name, new_name),
    }
}

fn now_iso() -> String {
    use time::format_description::well_known::Iso8601;
    time::OffsetDateTime::now_utc()
        .format(&Iso8601::DEFAULT)
        .unwrap_or_else(|_| "unknown".into())
}

fn apply_replace_disk_map_update(
    map: &mut crate::disk_map::DiskMap,
    old_name: &str,
    new_name: &str,
    new_by_id: &str,
    new_mn: &MapperName,
    pool_after: Option<&PoolState>,
) -> Option<String> {
    crate::disk_map::remove_disk(map, old_name);

    if let Some(pool_after) = pool_after {
        if let Some(dev) = pool_after.devices.iter().find(|d| d.mapper == *new_mn) {
            crate::disk_map::record_disk(
                map,
                new_name,
                new_by_id,
                &dev.luks_uuid.0,
                dev.devid,
            );
            None
        } else {
            Some(format!(
                "Warning: replace succeeded but could not find '{}' in post-operation pool probe; old disk map entry removed, new entry not recorded.",
                new_name
            ))
        }
    } else {
        Some(
            "Warning: replace succeeded but post-operation pool probe failed; old disk map entry removed, new entry not recorded."
                .to_owned(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_confirm_warns_about_luks_format_for_non_luks_disk() {
        let msg = replace_confirm_message(
            &ConfigDiskState::PresentNotLuks,
            "old1",
            "new1",
            "/dev/disk/by-id/usb-WD_5678",
        );
        assert!(msg.contains("LUKS-format"), "should mention LUKS-format");
        assert!(msg.contains("new1"), "should mention new disk name");
        assert!(
            msg.contains("/dev/disk/by-id/usb-WD_5678"),
            "should mention by-id"
        );
        assert!(
            msg.contains("inaccessible"),
            "should say data will be inaccessible"
        );
    }

    #[test]
    fn replace_confirm_generic_for_luks_disk() {
        let msg = replace_confirm_message(
            &ConfigDiskState::PresentLuks {
                uuid: LuksUuid("abc-123".into()),
                mapper_open: false,
            },
            "old1",
            "new1",
            "/dev/disk/by-id/usb-WD_5678",
        );
        assert!(
            msg.contains("Replace old1 (dead) with new1 (new)?"),
            "should be generic replace prompt, got: {}",
            msg
        );
        assert!(
            !msg.contains("LUKS-format"),
            "should not warn about formatting"
        );
    }

    #[test]
    fn spec_replace_probe_failure_still_removes_old_map_entry() {
        let mut map = crate::disk_map::DiskMap::new();
        crate::disk_map::record_disk(
            &mut map,
            "old",
            "/dev/disk/by-id/old",
            "old-uuid",
            1,
        );

        let new_mn = MapperName("braid-new".into());
        let _ = apply_replace_disk_map_update(
            &mut map,
            "old",
            "new",
            "/dev/disk/by-id/new",
            &new_mn,
            None,
        );

        assert!(
            !map.disks.contains_key("old"),
            "old entry should be removed even if post-replace re-probe fails"
        );
    }

    #[test]
    fn spec_replace_probe_failure_requests_warning() {
        let mut map = crate::disk_map::DiskMap::new();
        let new_mn = MapperName("braid-new".into());

        let warning = apply_replace_disk_map_update(
            &mut map,
            "old",
            "new",
            "/dev/disk/by-id/new",
            &new_mn,
            None,
        );

        assert!(
            warning.is_some(),
            "expected a warning when post-replace disk-map update is skipped due to re-probe failure"
        );
    }
}
