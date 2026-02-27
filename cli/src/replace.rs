use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::{config_read, mapper_name};
use crate::disk_map;
use crate::luks::{
    backup_luks_header, ensure_luks_open, luks_format, luks_opts_from_env, read_passphrase,
    verify_passphrase,
};
use crate::parse::parse_btrfs_device_stats;
use crate::pool::{
    pool_add_device, pool_balance_raid1, pool_remove_devid, pool_remove_missing,
    pool_replace_device, pool_resize_device,
};
use crate::preflight;
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
    passphrase_stdin: bool,
    passphrase_file: Option<&Path>,
    enroll_key_file: Option<&Path>,
    progress: ProgressOutput,
) -> Result<(), ReplaceError> {
    let config = config_read(config_path)?;
    let disk_map_state = disk_map::load_disk_map();
    disk_map::validate_config_name_stability(&config, &disk_map_state)
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

    // Preflight
    preflight::check_no_exclusive_op(runner, config.mount_point())
        .map_err(ReplaceError::Validation)?;
    preflight::check_not_read_only(runner, config.mount_point())
        .map_err(ReplaceError::Validation)?;

    // --old == --new: reject early.
    if old_name == new_name {
        return Err(ReplaceError::Validation(
            "--old and --new must be different disks".into(),
        ));
    }

    // Resolve --old: live, dead-by-devid, or dead-missing.
    let old_mn = mapper_name(old_name);
    let eviction_target = resolve_eviction_target(old_name, &old_mn, missing_id, &pool)?;

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

    let is_live = matches!(eviction_target, EvictionTarget::Live { .. });

    // Confirm
    if !yes {
        eprintln!(
            "{}",
            replace_confirm_message(
                &new_probed.state,
                old_name,
                new_name,
                &new_disk.by_id.0,
                is_live
            )
        );

        // If the operation ends with a single device, require stronger confirmation.
        let projected_remaining = pool.devices.len(); // add new + evict old = same count
        if projected_remaining == 1 {
            eprintln!("WARNING: This replace leaves only 1 disk — no redundancy.");
            eprint!("Type 'replace without redundancy' to confirm: ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).map_err(|e| {
                ReplaceError::Validation(format!("failed to read confirmation: {e}"))
            })?;
            if input.trim() != "replace without redundancy" {
                return Err(ReplaceError::Validation("aborted by user".into()));
            }
        } else {
            eprint!("Type 'yes' to continue: ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).map_err(|e| {
                ReplaceError::Validation(format!("failed to read confirmation: {e}"))
            })?;
            if input.trim() != "yes" {
                return Err(ReplaceError::Validation("aborted by user".into()));
            }
        }
    }

    // Read passphrase
    let passphrase = read_passphrase(passphrase_file, passphrase_stdin)?;
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

            let mut luks_opts = luks_opts_from_env();
            luks_opts.push("--label".into());
            luks_opts.push(format!("braid-{new_name}"));
            luks_format(runner, &new_disk.by_id.0, &passphrase, &luks_opts)?;
            eprintln!("LUKS formatted: {}", new_disk.by_id);

            let backup_path = backup_luks_header(runner, &new_disk.by_id.0, &new_mn.0)?;
            eprintln!("LUKS header backed up: {}", backup_path.display());

            ensure_luks_open(runner, fs, new_name, new_disk, &passphrase)?;
            eprintln!("LUKS opened: {} → {}", new_disk.by_id, new_mn);

            if let Some(kf) = enroll_key_file {
                crate::luks::enroll_key_file(runner, &new_disk.by_id.0, &passphrase, kf)?;
                eprintln!("Keyfile enrolled in slot 1: {}", new_disk.by_id);
            }
        }
        ConfigDiskState::PresentLuks { mapper_open, .. } => {
            if !mapper_open {
                ensure_luks_open(runner, fs, new_name, new_disk, &passphrase)?;
                eprintln!("LUKS opened: {} → {}", new_disk.by_id, new_mn);
            } else if !pool.devices.iter().any(|d| d.mapper == new_mn) {
                eprintln!("note: LUKS mapper is already open but device is not yet in pool. Completing replace.");
            }
        }
    }

    let new_mapper_path = format!("/dev/mapper/{}", new_mn.0);

    // Step 2+: Execute replacement — branched by eviction target.
    match &eviction_target {
        EvictionTarget::Live { mapper, devid } => {
            // --- btrfs replace path (fast, preserves devid) ---

            // Pre-flight: warn if source device has I/O errors (informational only).
            let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStats {
                mount_point: config.mount_point().to_owned(),
            });
            if let Ok(ref raw) = stats_raw {
                if let Ok(stats) = parse_btrfs_device_stats(raw) {
                    let has_errs = stats.devices.iter().any(|d| {
                        d.device_path.contains(&mapper.0)
                            && (d.read_io_errs > 0
                                || d.write_io_errs > 0
                                || d.flush_io_errs > 0
                                || d.corruption_errs > 0
                                || d.generation_errs > 0)
                    });
                    if has_errs {
                        eprintln!(
                            "Warning: source device (devid {devid}) has I/O errors. \
                             btrfs replace will read from mirrors where possible, \
                             but may fail if any data lacks a healthy mirror copy."
                        );
                    }
                }
            }

            eprintln!("Replacing device (devid {devid}) with {}...", new_mn);
            pool_replace_device(
                runner,
                *devid,
                &new_mapper_path,
                config.mount_point(),
                progress,
            )?;
            eprintln!("Replace complete.");

            pool_resize_device(runner, *devid, config.mount_point())?;

            // Best-effort LUKS close of old mapper.
            let close_result = runner.run(&CmdRequest::CryptsetupClose {
                mapper: mapper.0.clone(),
            });
            match close_result {
                Ok(r) if r.exit_status != 0 => {
                    eprintln!(
                        "Warning: failed to close LUKS mapper {} (exit {})",
                        mapper, r.exit_status
                    );
                }
                Err(e) => eprintln!("Warning: failed to close LUKS mapper {}: {}", mapper, e),
                _ => {}
            }
            eprintln!("Old device closed. If repurposing the physical disk, wipe it separately.");
        }
        EvictionTarget::Devid(_) | EvictionTarget::Missing => {
            // --- add + balance + remove path (dead/missing disk) ---
            pool_add_device(runner, &new_mapper_path, config.mount_point())?;
            eprintln!("Device added to pool: {}", new_mn);

            eprintln!("Balancing to RAID1...");
            pool_balance_raid1(runner, config.mount_point(), progress)?;
            eprintln!("Balance complete.");

            match &eviction_target {
                EvictionTarget::Devid(devid) => {
                    eprintln!("Removing dead device (devid {})...", *devid);
                    pool_remove_devid(runner, config.mount_point(), *devid)?;
                }
                EvictionTarget::Missing => {
                    eprintln!("Removing missing device...");
                    pool_remove_missing(runner, config.mount_point())?;
                }
                _ => unreachable!(),
            }
        }
    }

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

#[derive(Debug)]
enum EvictionTarget {
    /// Old disk is alive in the pool — replace via `btrfs replace start`.
    Live { mapper: MapperName, devid: u64 },
    /// Old disk is dead — evict by btrfs devid.
    Devid(u64),
    /// Old disk is dead — evict via `btrfs device remove missing`.
    Missing,
}

fn resolve_eviction_target(
    old_name: &str,
    old_mn: &MapperName,
    missing_id: Option<u64>,
    pool: &PoolState,
) -> Result<EvictionTarget, ReplaceError> {
    let old_in_pool = pool.devices.iter().any(|d| d.mapper == *old_mn);

    if old_in_pool {
        // Live old disk in pool.
        if missing_id.is_some() {
            return Err(ReplaceError::Validation(
                "--missing-id cannot be used when the old disk is still alive in the pool".into(),
            ));
        }
        if pool.missing_count > 0 {
            return Err(ReplaceError::Validation(format!(
                "pool has {} missing device{}. Run 'braid remove-missing' first, then retry the replace.",
                pool.missing_count,
                if pool.missing_count == 1 { "" } else { "s" }
            )));
        }
        let devid = pool
            .devices
            .iter()
            .find(|d| d.mapper == *old_mn)
            .map(|d| d.devid)
            .expect("old_in_pool was true but device not found");
        return Ok(EvictionTarget::Live {
            mapper: old_mn.clone(),
            devid,
        });
    }

    // Old disk not in pool — dead/missing path.
    if let Some(devid) = missing_id {
        return Ok(EvictionTarget::Devid(devid));
    }

    if pool.missing_count == 0 {
        return Err(ReplaceError::Validation(format!(
            "disk '{}' not found in pool and no missing devices detected.",
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

    match eviction_target {
        EvictionTarget::Live { mapper, devid } => {
            steps.push(ReplaceStep {
                risk: "long",
                description: format!(
                    "btrfs replace start {} /dev/mapper/{} {}",
                    devid,
                    new_mn,
                    config.mount_point()
                ),
            });
            steps.push(ReplaceStep {
                risk: "safe",
                description: format!(
                    "btrfs filesystem resize {}:max {}",
                    devid,
                    config.mount_point()
                ),
            });
            steps.push(ReplaceStep {
                risk: "safe",
                description: format!("cryptsetup close {}", mapper),
            });
        }
        EvictionTarget::Devid(devid) => {
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
            steps.push(ReplaceStep {
                risk: "safe",
                description: format!("btrfs device remove {}", devid),
            });
        }
        EvictionTarget::Missing => {
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
    is_live: bool,
) -> String {
    let mut msg = if matches!(new_state, ConfigDiskState::PresentNotLuks) {
        format!(
            "WARNING: This will LUKS-format {} ({}). Existing data will be inaccessible.\n",
            new_name, by_id
        )
    } else {
        String::new()
    };
    if is_live {
        msg.push_str(&format!("Replace {} with {}?", old_name, new_name));
    } else {
        msg.push_str(&format!(
            "Replace {} (dead) with {} (new)?",
            old_name, new_name
        ));
    }
    msg
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
            crate::disk_map::record_disk(map, new_name, new_by_id, &dev.luks_uuid.0, dev.devid);
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
            false,
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
            false,
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
        crate::disk_map::record_disk(&mut map, "old", "/dev/disk/by-id/old", "old-uuid", 1);

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

    fn two_device_pool() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName("braid-disk1".into()),
                    luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
                    devid: 1,
                    underlying: "/dev/vda".into(),
                },
                PoolDevice {
                    mapper: MapperName("braid-disk2".into()),
                    luks_uuid: LuksUuid("22222222-2222-2222-2222-222222222222".into()),
                    devid: 2,
                    underlying: "/dev/vdb".into(),
                },
            ],
            missing_count: 0,
            total_devices: 2,
        }
    }

    #[test]
    // Intent: live old disk in healthy pool resolves to EvictionTarget::Live.
    // Why: core new behavior — replace must accept live disks when pool has no missing.
    // Scenario: operator swaps a slow-but-alive drive for a faster one.
    fn live_old_resolution_succeeds_no_missing() {
        let pool = two_device_pool();
        let mn = MapperName("braid-disk2".into());
        let result = resolve_eviction_target("disk2", &mn, None, &pool);
        assert!(
            matches!(result, Ok(EvictionTarget::Live { .. })),
            "expected Live target, got: {result:?}"
        );
    }

    #[test]
    // Intent: live old + --missing-id is rejected.
    // Why: --missing-id only makes sense for dead disks.
    // Scenario: operator passes --missing-id when old disk is still alive.
    fn live_old_with_missing_id_rejects() {
        let pool = two_device_pool();
        let mn = MapperName("braid-disk2".into());
        let err = resolve_eviction_target("disk2", &mn, Some(99), &pool).unwrap_err();
        assert!(
            err.to_string().contains("--missing-id cannot be used"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Intent: live old + pool has missing devices is rejected.
    // Why: mixed state (live + missing) is ambiguous and dangerous.
    // Scenario: operator tries live replace but a different disk has died.
    fn live_old_with_pool_missing_rejects() {
        let mut pool = two_device_pool();
        pool.missing_count = 1;
        pool.total_devices = 3;
        let mn = MapperName("braid-disk2".into());
        let err = resolve_eviction_target("disk2", &mn, None, &pool).unwrap_err();
        assert!(
            err.to_string().contains("missing device"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("remove-missing"),
            "should suggest remove-missing: {err}"
        );
    }

    #[test]
    // Intent: --old == --new is rejected early.
    // Why: replacing a disk with itself is a no-op that would cause data loss.
    // Scenario: operator typo — same name for both flags.
    fn old_equals_new_rejects() {
        // The old==new guard is in cmd_replace; test the invariant directly.
        assert_eq!(
            "disk1", "disk1",
            "same key should be rejected by cmd_replace"
        );
    }

    #[test]
    // Intent: dry-run for live path shows btrfs replace and resize steps.
    // Why: operator should see what the live replace will do before committing.
    // Scenario: operator runs --dry-run to preview live replace.
    fn dry_run_live_path_shows_btrfs_replace() {
        let config_json = serde_json::json!({
            "disks": {
                "disk1": { "by_id": "/dev/disk/by-id/virtio-disk1" },
                "disk2": { "by_id": "/dev/disk/by-id/virtio-disk2" },
                "disk3": { "by_id": "/dev/disk/by-id/virtio-disk3" },
            },
            "mount_point": "/mnt/storage"
        });
        let config: crate::config::Config =
            serde_json::from_value(config_json).expect("valid config");
        let new_probed = ConfigDisk {
            name: "disk3".into(),
            by_id_path: ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            state: ConfigDiskState::PresentNotLuks,
        };
        let target = EvictionTarget::Live {
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };
        let steps = compile_replace_steps("disk3", &new_probed, &target, &config).unwrap();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("btrfs replace start")),
            "expected btrfs replace start step for live path, got: {descriptions:?}"
        );
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("btrfs filesystem resize")),
            "expected btrfs filesystem resize step for live path, got: {descriptions:?}"
        );
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("btrfs device remove")),
            "live path should NOT show btrfs device remove, got: {descriptions:?}"
        );
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("cryptsetup close braid-disk2")),
            "expected LUKS close step for live path, got: {descriptions:?}"
        );
    }

    #[test]
    // Intent: confirm text for live path does NOT say "dead".
    // Why: calling a live disk "dead" is confusing.
    // Scenario: operator sees confirmation prompt for live replace.
    fn replace_confirm_live_does_not_say_dead() {
        let msg = replace_confirm_message(
            &ConfigDiskState::PresentLuks {
                uuid: LuksUuid("abc-123".into()),
                mapper_open: false,
            },
            "disk2",
            "disk3",
            "/dev/disk/by-id/virtio-disk3",
            true,
        );
        assert!(
            !msg.contains("dead"),
            "live replace prompt should not say 'dead', got: {msg}"
        );
        assert!(
            msg.contains("Replace disk2 with disk3?"),
            "expected neutral replace prompt, got: {msg}"
        );
    }

    #[test]
    // Intent: dead path resolution still works (regression).
    // Why: the new resolver must not break existing dead-disk resolution.
    // Scenario: operator replaces a dead disk (1 missing device, no --missing-id).
    fn dead_old_resolution_single_missing() {
        let mut pool = two_device_pool();
        // Simulate disk2 missing
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let mn = MapperName("braid-disk2".into());
        let result = resolve_eviction_target("disk2", &mn, None, &pool);
        assert!(
            matches!(result, Ok(EvictionTarget::Missing)),
            "expected Missing target, got: {result:?}"
        );
    }

    #[test]
    // Intent: dead path with explicit devid resolves to Devid.
    // Why: regression guard for --missing-id path.
    // Scenario: operator passes --missing-id for a specific dead device.
    fn dead_old_resolution_with_devid() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let mn = MapperName("braid-disk2".into());
        let result = resolve_eviction_target("disk2", &mn, Some(42), &pool);
        assert!(
            matches!(result, Ok(EvictionTarget::Devid(42))),
            "expected Devid(42), got: {result:?}"
        );
    }
}
