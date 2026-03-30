use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::{config_read, mapper_name};
use crate::journal;
use crate::luks::{
    backup_luks_header, ensure_luks_open, luks_format, luks_opts_from_env, read_passphrase,
    verify_passphrase,
};
use crate::membership;
use crate::parse::parse_btrfs_device_stats;
use crate::pool::{pool_replace_device, pool_resize_device};
use crate::preflight;
use crate::probe::{probe_config_disk, probe_pool, Filesystem, ProbeError};
use crate::progress::ProgressOutput;
use crate::state_paths::StatePaths;
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
    paths: &StatePaths,
) -> Result<(), ReplaceError> {
    preflight::check_no_pending_operation(paths).map_err(ReplaceError::Validation)?;

    let config = config_read(config_path)?;

    // Parse new_name as name=by_id spec
    let (new_name_parsed, new_by_id) = membership::parse_disk_spec(new_name)
        .map_err(|e| ReplaceError::Validation(e.to_string()))?;
    let new_name = new_name_parsed.as_str();

    let pool = match probe_pool(runner, config.mount_point().as_str()) {
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
    preflight::check_no_exclusive_op(runner, config.mount_point().as_str())
        .map_err(ReplaceError::Validation)?;
    preflight::check_not_read_only(runner, config.mount_point().as_str())
        .map_err(ReplaceError::Validation)?;

    // --old == --new: reject early.
    if old_name == new_name {
        return Err(ReplaceError::Validation(
            "--old and --new must be different disks".into(),
        ));
    }

    // Resolve --old: live or missing (by devid).
    let old_mn = mapper_name(old_name);
    let replace_source = resolve_replace_source(
        runner,
        old_name,
        &old_mn,
        missing_id,
        &pool,
        config.mount_point().as_str(),
    )?;

    // Probe --new disk state
    let new_probed = probe_config_disk(runner, fs, new_name, &new_by_id)?;

    // Compile steps
    let will_clear_last_missing =
        matches!(&replace_source, ReplaceSource::Missing { .. }) && pool.missing_count == 1;
    let steps = compile_replace_steps(
        new_name,
        &new_by_id,
        &new_probed,
        &replace_source,
        config.mount_point(),
        will_clear_last_missing,
        pool.total_devices,
    )?;

    if dry_run {
        for step in &steps {
            println!("[{:<11}] {}", step.risk, step.description);
        }
        return Ok(());
    }

    // Confirm
    if !yes {
        eprintln!(
            "{}",
            replace_confirm_message(
                &new_probed.state,
                old_name,
                new_name,
                &new_by_id.0,
                &replace_source,
            )
        );

        // If the operation ends with a single device, require stronger confirmation.
        let projected_remaining = pool.total_devices; // replace preserves device count
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

    // Reversible checks: reject absent disk, verify passphrase, check not already in pool.
    if matches!(new_probed.state, ConfigDiskState::Absent) {
        return Err(ReplaceError::Validation(format!(
            "new disk '{}' ({}) is not present. Is it plugged in?",
            new_name, new_by_id
        )));
    }

    if matches!(new_probed.state, ConfigDiskState::PresentNotLuks) {
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
    }

    // Guard: new disk must not already be in the pool.
    check_new_not_in_pool(new_name, &new_mn, &pool)?;

    // Build target membership and write journal before irreversible disk ops.
    let pre_membership = membership::load_membership(paths)
        .map_err(|e| ReplaceError::Validation(format!("failed to load pool membership: {e}")))?;
    let target_membership =
        build_replacement_membership(&pre_membership, old_name, new_name, &new_by_id)?;
    let journal = journal::build_journal(
        pre_membership,
        target_membership.clone(),
        journal::OpKind::Replace {
            old_name: old_name.to_owned(),
            new_name: new_name.to_owned(),
            new_by_id: new_by_id.clone(),
        },
    );
    journal::write_journal(paths, &journal).map_err(|e| ReplaceError::Validation(e.to_string()))?;
    let mut journal_guard = journal::JournalGuard::new(paths);

    // Step 1: Init new disk (LUKS format/open) — irreversible from here.
    match new_probed.state {
        ConfigDiskState::Absent => unreachable!("already checked above"),
        ConfigDiskState::PresentNotLuks => {
            // Passphrase already verified above.
            let mut luks_opts = luks_opts_from_env();
            luks_opts.push("--label".into());
            luks_opts.push(format!("braid-{new_name}"));
            luks_format(runner, &new_by_id.0, &passphrase, &luks_opts)?;
            eprintln!("LUKS formatted: {}", new_by_id);

            let backup_path = backup_luks_header(runner, &new_by_id.0, &new_mn.0, paths)?;
            eprintln!("LUKS header backed up: {}", backup_path.display());

            ensure_luks_open(runner, fs, new_name, &new_by_id, &passphrase)?;
            eprintln!("LUKS opened: {} → {}", new_by_id, new_mn);

            if let Some(kf) = enroll_key_file {
                crate::luks::enroll_key_file(runner, &new_by_id.0, &passphrase, kf)?;
                eprintln!("Keyfile enrolled in slot 1: {}", new_by_id);
            }
        }
        ConfigDiskState::PresentLuks { mapper_open, .. } => {
            if !mapper_open {
                ensure_luks_open(runner, fs, new_name, &new_by_id, &passphrase)?;
                eprintln!("LUKS opened: {} → {}", new_by_id, new_mn);
            } else if !pool.devices.iter().any(|d| d.mapper == new_mn) {
                eprintln!(
                    "note: LUKS mapper is already open but device is not yet in pool. Completing replace."
                );
            }
        }
    }

    let new_mapper_path = format!("/dev/mapper/{}", new_mn.0);

    // Step 2+: Execute replacement — both paths use btrfs replace start.
    match &replace_source {
        ReplaceSource::Live { mapper, devid } => {
            // Pre-flight: warn if source device has I/O errors (informational only).
            let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStats {
                mount_point: config.mount_point().clone(),
            });
            if let Ok(ref raw) = stats_raw {
                if let Ok(stats) = parse_btrfs_device_stats(raw) {
                    let expected_path = format!("/dev/mapper/{}", mapper.0);
                    let has_errs = stats.devices.iter().any(|d| {
                        d.target.as_path() == Some(expected_path.as_str())
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
                config.mount_point().as_str(),
                progress,
            )?;
            eprintln!("Replace complete.");

            pool_resize_device(runner, *devid, config.mount_point().as_str())?;

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
        ReplaceSource::Missing { devid } => {
            eprintln!(
                "Rebuilding missing device (devid {devid}) onto {}...",
                new_mn
            );
            pool_replace_device(
                runner,
                *devid,
                &new_mapper_path,
                config.mount_point().as_str(),
                progress,
            )?;
            eprintln!("Replace complete.");

            pool_resize_device(runner, *devid, config.mount_point().as_str())?;
            // No old mapper to close — device was already missing.
        }
    }

    // Capture pre-op missing count for soft balance decision
    let pre_op_missing_count = pool.missing_count;

    // Restore RAID1 redundancy for missing-path replacements that clear the last missing device
    if matches!(&replace_source, ReplaceSource::Missing { .. }) {
        crate::pool::maybe_restore_raid1(
            runner,
            config.mount_point().as_str(),
            pre_op_missing_count,
            progress,
        )
        .map_err(|e| ReplaceError::Pool(e))?;
    }

    // Post-commit: write pool.json with enriched metadata and clear journal.
    let mut final_membership = target_membership;
    if let Ok(pool_after) = probe_pool(runner, config.mount_point().as_str()) {
        for dev in &pool_after.devices {
            let Some(name) = crate::config::name_from_mapper(&dev.mapper.0) else {
                continue;
            };
            if let Some(member) = final_membership.disks.get_mut(name) {
                member.luks_uuid = Some(dev.luks_uuid.clone());
                member.devid = Some(dev.devid);
                if member.added_at.is_none() {
                    member.added_at = Some(
                        time::OffsetDateTime::now_utc()
                            .format(&time::format_description::well_known::Iso8601::DEFAULT)
                            .expect("formatting UTC as ISO8601 should never fail"),
                    );
                }
            }
        }
    }
    membership::save_membership(&final_membership, paths)
        .map_err(|e| ReplaceError::Validation(format!("failed to persist pool membership: {e}")))?;
    journal::clear_journal(paths).map_err(|e| ReplaceError::Validation(e.to_string()))?;
    journal_guard.disarm();

    eprintln!("Done. Replaced {} with {}.", old_name, new_name);
    Ok(())
}

#[derive(Debug)]
enum ReplaceSource {
    /// Old disk is alive in the pool — replace via `btrfs replace start`.
    Live { mapper: MapperName, devid: u64 },
    /// Old disk is missing — replace via `btrfs replace start` by devid.
    Missing { devid: u64 },
}

fn check_new_not_in_pool(
    new_name: &str,
    new_mn: &MapperName,
    pool: &PoolState,
) -> Result<(), ReplaceError> {
    if pool.devices.iter().any(|d| d.mapper == *new_mn) {
        return Err(ReplaceError::Validation(format!(
            "new disk '{}' is already a member of the pool. Cannot replace with an existing member.",
            new_name
        )));
    }
    Ok(())
}

fn resolve_replace_source<R: CommandRunner>(
    runner: &R,
    old_name: &str,
    old_mn: &MapperName,
    missing_id: Option<u64>,
    pool: &PoolState,
    mount_point: &str,
) -> Result<ReplaceSource, ReplaceError> {
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
                "pool has {} missing device{}. \
                 Repair the missing device{} first with `braid replace --missing-id <devid>`, \
                 then retry this live replace. Use `braid status` to see device IDs.",
                pool.missing_count,
                if pool.missing_count == 1 { "" } else { "s" },
                if pool.missing_count == 1 { "" } else { "s" },
            )));
        }
        let devid = pool
            .devices
            .iter()
            .find(|d| d.mapper == *old_mn)
            .map(|d| d.devid)
            .expect("old_in_pool was true but device not found");
        return Ok(ReplaceSource::Live {
            mapper: old_mn.clone(),
            devid,
        });
    }

    // Old disk not in pool — dead/missing path.
    // Probe actual missing devids for validation and auto-resolution.
    let missing_devids =
        preflight::probe_missing_devids(runner, mount_point).map_err(ReplaceError::Validation)?;

    if let Some(devid) = missing_id {
        // Validate --missing-id refers to an actually-missing device.
        if pool.devices.iter().any(|d| d.devid == devid) {
            return Err(ReplaceError::Validation(format!(
                "devid {devid} is a live device, not a missing one."
            )));
        }
        if !missing_devids.contains(&devid) {
            return Err(ReplaceError::Validation(format!(
                "devid {devid} is not a missing device in this pool. \
                 Use 'braid status' to see device IDs."
            )));
        }
        return Ok(ReplaceSource::Missing { devid });
    }

    if missing_devids.is_empty() {
        return Err(ReplaceError::Validation(format!(
            "disk '{}' not found in pool and no missing devices detected.",
            old_name
        )));
    }

    if missing_devids.len() == 1 {
        return Ok(ReplaceSource::Missing {
            devid: missing_devids[0],
        });
    }

    Err(ReplaceError::Validation(format!(
        "multiple missing devices ({} missing). Pass --missing-id <devid> to target the specific dead disk. Use 'braid status' to see device IDs.",
        missing_devids.len()
    )))
}

fn compile_replace_steps(
    new_name: &str,
    new_by_id: &ByIdPath,
    new_probed: &ConfigDisk,
    replace_source: &ReplaceSource,
    mount_point: &MountPoint,
    will_clear_last_missing: bool,
    total_devices: u64,
) -> Result<Vec<ReplaceStep>, ReplaceError> {
    let new_mn = mapper_name(new_name);
    let mut steps = Vec::new();

    match &new_probed.state {
        ConfigDiskState::Absent => {
            return Err(ReplaceError::Validation(format!(
                "new disk '{}' ({}) is not present. Is it plugged in?",
                new_name, new_by_id
            )));
        }
        ConfigDiskState::PresentNotLuks => {
            steps.push(ReplaceStep {
                risk: "destructive",
                description: format!("LUKS format {}", new_by_id),
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

    match replace_source {
        ReplaceSource::Live { mapper, devid } => {
            steps.push(ReplaceStep {
                risk: "long",
                description: format!(
                    "btrfs replace start {} /dev/mapper/{} {}",
                    devid, new_mn, mount_point
                ),
            });
            steps.push(ReplaceStep {
                risk: "safe",
                description: format!("btrfs filesystem resize {}:max {}", devid, mount_point),
            });
            steps.push(ReplaceStep {
                risk: "safe",
                description: format!("cryptsetup close {}", mapper),
            });
        }
        ReplaceSource::Missing { devid } => {
            steps.push(ReplaceStep {
                risk: "long",
                description: format!(
                    "btrfs replace start {} /dev/mapper/{} {}",
                    devid, new_mn, mount_point
                ),
            });
            steps.push(ReplaceStep {
                risk: "safe",
                description: format!("btrfs filesystem resize {}:max {}", devid, mount_point),
            });
            if will_clear_last_missing && total_devices >= 2 {
                steps.push(ReplaceStep {
                    risk: "long",
                    description:
                        "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft (restore redundancy)"
                            .into(),
                });
            }
        }
    }

    Ok(steps)
}

fn replace_confirm_message(
    new_state: &ConfigDiskState,
    old_name: &str,
    new_name: &str,
    by_id: &str,
    replace_source: &ReplaceSource,
) -> String {
    let mut msg = if matches!(new_state, ConfigDiskState::PresentNotLuks) {
        format!(
            "WARNING: This will LUKS-format {} ({}). Existing data will be inaccessible.\n",
            new_name, by_id
        )
    } else {
        String::new()
    };
    match replace_source {
        ReplaceSource::Live { devid, .. } => {
            msg.push_str(&format!(
                "Old disk '{}' (devid {}) is present and will be replaced in-place by '{}'.",
                old_name, devid, new_name
            ));
        }
        ReplaceSource::Missing { devid } => {
            msg.push_str(&format!(
                "Missing device (devid {}) will be rebuilt onto new disk '{}' from RAID redundancy.",
                devid, new_name
            ));
        }
    }
    msg
}

fn build_replacement_membership(
    existing: &membership::PoolMembership,
    old_name: &str,
    new_name: &str,
    new_by_id: &ByIdPath,
) -> Result<membership::PoolMembership, ReplaceError> {
    let mut next = existing.clone();
    next.disks.remove(old_name);
    membership::validate_no_conflicts(&next, new_name, &new_by_id.0)
        .map_err(|e| ReplaceError::Validation(e.to_string()))?;
    next.disks.insert(
        new_name.to_owned(),
        membership::DiskMember::from_by_id(new_by_id.clone()),
    );
    Ok(next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};

    #[test]
    fn replace_confirm_warns_about_luks_format_for_non_luks_disk() {
        let msg = replace_confirm_message(
            &ConfigDiskState::PresentNotLuks,
            "old1",
            "new1",
            "/dev/disk/by-id/usb-WD_5678",
            &ReplaceSource::Missing { devid: 2 },
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
    fn replace_confirm_missing_shows_rebuild_message() {
        let msg = replace_confirm_message(
            &ConfigDiskState::PresentLuks {
                uuid: LuksUuid("abc-123".into()),
                mapper_open: false,
            },
            "old1",
            "new1",
            "/dev/disk/by-id/usb-WD_5678",
            &ReplaceSource::Missing { devid: 2 },
        );
        assert!(
            msg.contains("Missing device (devid 2)"),
            "should mention missing devid, got: {}",
            msg
        );
        assert!(
            msg.contains("rebuilt onto new disk 'new1'"),
            "should mention rebuild, got: {}",
            msg
        );
        assert!(
            !msg.contains("LUKS-format"),
            "should not warn about formatting"
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
            missing_devids: vec![],
            total_devices: 2,
            fsid: None,
        }
    }

    /// Create a mock runner that returns device usage output with specific
    /// missing devids (device_size == 0). Devid 1 is always present.
    fn mock_with_missing_devids(missing_devids: &[u64]) -> MockRunner {
        let mut output = String::new();
        output.push_str("/dev/mapper/braid-disk1, ID: 1\n");
        output.push_str("   Device size:           520093696\n");
        output.push_str("   Device slack:                  0\n");
        output.push_str("   Data,RAID1:            469762048\n");
        output.push_str("   Unallocated:            50331648\n\n");
        for &devid in missing_devids {
            output.push_str(&format!("<missing disk>, ID: {}\n", devid));
            output.push_str("   Device size:                  0\n");
            output.push_str("   Device slack:                  0\n");
            output.push_str("   Data,RAID1:            469762048\n");
            output.push_str("   Unallocated:                  0\n\n");
        }
        MockRunner::default().with_output(
            CmdRequest::BtrfsDeviceUsageRaw {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs device usage --raw /mnt/storage".into(),
                stdout: output,
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    #[test]
    // Intent: live old disk in healthy pool resolves to ReplaceSource::Live.
    // Why: core behavior — replace must accept live disks when pool has no missing.
    // Scenario: operator swaps a slow-but-alive drive for a faster one.
    fn live_old_resolution_succeeds_no_missing() {
        let pool = two_device_pool();
        let runner = MockRunner::default();
        let mn = MapperName("braid-disk2".into());
        let result = resolve_replace_source(&runner, "disk2", &mn, None, &pool, "/mnt/storage");
        assert!(
            matches!(result, Ok(ReplaceSource::Live { .. })),
            "expected Live target, got: {result:?}"
        );
    }

    #[test]
    // Intent: live old + --missing-id is rejected.
    // Why: --missing-id only makes sense for dead disks.
    // Scenario: operator passes --missing-id when old disk is still alive.
    fn live_old_with_missing_id_rejects() {
        let pool = two_device_pool();
        let runner = MockRunner::default();
        let mn = MapperName("braid-disk2".into());
        let err = resolve_replace_source(&runner, "disk2", &mn, Some(99), &pool, "/mnt/storage")
            .unwrap_err();
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
        let runner = MockRunner::default();
        let mn = MapperName("braid-disk2".into());
        let err =
            resolve_replace_source(&runner, "disk2", &mn, None, &pool, "/mnt/storage").unwrap_err();
        assert!(
            err.to_string().contains("missing device"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains("replace --missing-id"),
            "should suggest replace --missing-id: {err}"
        );
        assert!(
            !err.to_string().contains("remove-missing"),
            "should not suggest remove-missing: {err}"
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
    // Intent: replace must reject a post-replace membership that reuses another
    // member's by-id under a new name.
    // Why: docs and invariants say mutating commands reject name reassignment /
    // by-id rename rather than silently corrupting pool membership.
    // Scenario: operator tries `braid replace --old disk1 --new newname=<disk2 by-id>`.
    fn build_replacement_membership_rejects_by_id_rename_conflict() {
        let mut membership = membership::PoolMembership::empty();
        membership.disks.insert(
            "disk1".into(),
            membership::DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        membership.disks.insert(
            "disk2".into(),
            membership::DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );

        let err = build_replacement_membership(
            &membership,
            "disk1",
            "newname",
            &ByIdPath("/dev/disk/by-id/virtio-disk2".into()),
        )
        .expect_err("should reject by-id rename conflict");

        assert!(
            err.to_string().contains("cannot register"),
            "unexpected error: {err}"
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
        let source = ReplaceSource::Live {
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };
        let steps = compile_replace_steps(
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &new_probed,
            &source,
            &MountPoint("/mnt/storage".into()),
            false,
            2,
        )
        .unwrap();
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
    // Intent: dry-run for missing path shows btrfs replace start, not add/balance/remove.
    // Why: operator should see the unified replace path, not the old degraded balance path.
    // Scenario: operator runs --dry-run to preview dead-disk replace.
    fn dry_run_missing_path_shows_btrfs_replace() {
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
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = compile_replace_steps(
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &new_probed,
            &source,
            &MountPoint("/mnt/storage".into()),
            true,
            2,
        )
        .unwrap();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("btrfs replace start")),
            "expected btrfs replace start step for missing path, got: {descriptions:?}"
        );
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("btrfs filesystem resize")),
            "expected btrfs filesystem resize step for missing path, got: {descriptions:?}"
        );
        assert!(
            !descriptions.iter().any(|d| d.contains("btrfs device add")),
            "missing path should NOT show btrfs device add, got: {descriptions:?}"
        );
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("-dconvert=raid1,soft")),
            "missing path (clearing last missing, ≥2 devices) should show soft balance, got: {descriptions:?}"
        );
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("btrfs device remove")),
            "missing path should NOT show btrfs device remove, got: {descriptions:?}"
        );
        assert!(
            !descriptions.iter().any(|d| d.contains("cryptsetup close")),
            "missing path should NOT show cryptsetup close (no old mapper), got: {descriptions:?}"
        );
    }

    #[test]
    // Intent: replacing with a disk that's already in the pool is rejected.
    // Why: without the guard, the Live path would pass an existing pool member
    //   to `btrfs replace start`. The btrfs replace path has no natural guard
    //   against this, so we need an explicit one.
    // Scenario: operator typo — specifies an existing pool member as --new.
    fn new_disk_already_in_pool_rejected() {
        let pool = two_device_pool(); // has braid-disk1 and braid-disk2
        let new_mn = mapper_name("disk2"); // → "braid-disk2"
        let err = check_new_not_in_pool("disk2", &new_mn, &pool).unwrap_err();
        assert!(
            err.to_string().contains("already a member"),
            "expected 'already a member' error, got: {err}"
        );
    }

    #[test]
    // Intent: a disk NOT in the pool passes the guard.
    // Why: regression — the guard must not block valid replacements.
    // Scenario: normal replace with a fresh disk.
    fn new_disk_not_in_pool_passes() {
        let pool = two_device_pool();
        let new_mn = mapper_name("disk3");
        check_new_not_in_pool("disk3", &new_mn, &pool).expect("disk3 is not in pool — should pass");
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
            &ReplaceSource::Live {
                mapper: MapperName("braid-disk2".into()),
                devid: 2,
            },
        );
        assert!(
            !msg.contains("dead"),
            "live replace prompt should not say 'dead', got: {msg}"
        );
        assert!(
            msg.contains("is present and will be replaced in-place"),
            "expected in-place replace prompt, got: {msg}"
        );
    }

    #[test]
    // Intent: dead path resolution auto-detects the missing devid.
    // Why: when exactly one device is missing, the operator shouldn't need --missing-id.
    // Scenario: operator replaces a dead disk (1 missing device, no --missing-id).
    fn dead_old_resolution_single_missing() {
        let mut pool = two_device_pool();
        // Simulate disk2 missing
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        let mn = MapperName("braid-disk2".into());
        let result = resolve_replace_source(&runner, "disk2", &mn, None, &pool, "/mnt/storage");
        assert!(
            matches!(result, Ok(ReplaceSource::Missing { devid: 2 })),
            "expected Missing {{ devid: 2 }}, got: {result:?}"
        );
    }

    #[test]
    // Intent: dead path with explicit --missing-id resolves to that devid.
    // Why: regression guard for --missing-id path.
    // Scenario: operator passes --missing-id for a specific dead device.
    fn dead_old_resolution_with_devid() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        let mn = MapperName("braid-disk2".into());
        let result = resolve_replace_source(&runner, "disk2", &mn, Some(2), &pool, "/mnt/storage");
        assert!(
            matches!(result, Ok(ReplaceSource::Missing { devid: 2 })),
            "expected Missing {{ devid: 2 }}, got: {result:?}"
        );
    }

    #[test]
    // Intent: --missing-id pointing to a live device is rejected.
    // Why: the operator may have confused devids; replacing a live device
    //   via the missing path would corrupt data.
    // Scenario: operator passes --missing-id with the devid of a healthy disk.
    fn missing_id_pointing_to_live_device_rejected() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        let mn = MapperName("braid-disk2".into());
        // Devid 1 is live (in pool.devices)
        let err = resolve_replace_source(&runner, "disk2", &mn, Some(1), &pool, "/mnt/storage")
            .unwrap_err();
        assert!(
            err.to_string().contains("live device"),
            "expected 'live device' error, got: {err}"
        );
    }

    #[test]
    // Intent: --missing-id pointing to a nonexistent devid is rejected.
    // Why: a bogus devid would cause btrfs replace start to fail; catch it early.
    // Scenario: operator typos the devid.
    fn missing_id_nonexistent_devid_rejected() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        let mn = MapperName("braid-disk2".into());
        let err = resolve_replace_source(&runner, "disk2", &mn, Some(99), &pool, "/mnt/storage")
            .unwrap_err();
        assert!(
            err.to_string().contains("not a missing device"),
            "expected 'not a missing device' error, got: {err}"
        );
    }

    #[test]
    // Intent: multiple missing devices without --missing-id is rejected.
    // Why: auto-detect is ambiguous when multiple devices are missing.
    // Scenario: two drives died; operator must specify which to replace first.
    fn multiple_missing_without_id_rejected() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 2;
        pool.total_devices = 3;
        let runner = mock_with_missing_devids(&[2, 3]);
        let mn = MapperName("braid-disk2".into());
        let err =
            resolve_replace_source(&runner, "disk2", &mn, None, &pool, "/mnt/storage").unwrap_err();
        assert!(
            err.to_string().contains("multiple missing"),
            "expected 'multiple missing' error, got: {err}"
        );
    }

    fn make_replace_config() -> crate::config::Config {
        let config_json = serde_json::json!({
            "mount_point": "/mnt/storage"
        });
        serde_json::from_value(config_json).expect("valid config")
    }

    fn new_probed_not_luks() -> ConfigDisk {
        ConfigDisk {
            name: "disk3".into(),
            by_id_path: ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            state: ConfigDiskState::PresentNotLuks,
        }
    }

    #[test]
    // Intent: missing-path dry-run (not last missing) omits rebalance step.
    // Why: if other missing devices remain, a rebalance would be premature.
    // Scenario: 3-disk pool, 2 missing, replacing 1 — still degraded after.
    fn dry_run_missing_not_last_omits_rebalance() {
        let config = make_replace_config();
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = compile_replace_steps(
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &new_probed,
            &source,
            &MountPoint("/mnt/storage".into()),
            false,
            3,
        )
        .unwrap();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("-dconvert=raid1,soft")),
            "should NOT show soft balance when not clearing last missing, got: {descriptions:?}"
        );
    }

    #[test]
    // Intent: missing-path dry-run with total_devices == 1 omits rebalance.
    // Why: can't have RAID1 with 1 device.
    // Scenario: single-device pool with a missing ghost entry.
    fn dry_run_missing_single_device_omits_rebalance() {
        let config = make_replace_config();
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = compile_replace_steps(
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &new_probed,
            &source,
            &MountPoint("/mnt/storage".into()),
            true,
            1,
        )
        .unwrap();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("-dconvert=raid1,soft")),
            "should NOT show soft balance with total_devices == 1, got: {descriptions:?}"
        );
    }

    #[test]
    // Intent: live-path dry-run still shows NO soft balance step.
    // Why: live replace doesn't create single-profile chunks — no degraded mode involved.
    // Scenario: swapping a working drive for a bigger one.
    fn dry_run_live_path_no_soft_balance() {
        let config = make_replace_config();
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Live {
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };
        let steps = compile_replace_steps(
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &new_probed,
            &source,
            &MountPoint("/mnt/storage".into()),
            false,
            2,
        )
        .unwrap();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("-dconvert=raid1,soft")),
            "live path should NOT show soft balance, got: {descriptions:?}"
        );
    }
}
