use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{config_read, mapper_name};
use crate::journal;
use crate::luks::{
    backup_luks_header, ensure_luks_open, luks_format, luks_opts_from_env, read_passphrase,
    verify_passphrase,
};
use crate::membership::{self, PoolMembership};
use crate::parse::btrfs_filesystem_show::{classify_btrfs_probe, DeviceBtrfsProbe};
use crate::parse::parse_btrfs_filesystem_show;
use crate::pool::{
    pool_add_device, pool_balance_raid1, pool_bootstrap_mount, pool_bootstrap_mount_raid1,
};
use crate::preflight;
use crate::probe::{probe_config_disk, probe_pool, Filesystem, ProbeError};
use crate::progress::ProgressOutput;
use crate::state_paths::StatePaths;
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
    #[error("membership error: {0}")]
    Membership(#[from] membership::MembershipError),
}

/// A step in the add operation, for dry-run display.
#[derive(Debug)]
pub struct AddStep {
    pub risk: &'static str, // "destructive", "safe", "long", "blocked"
    pub description: String,
}

// ---------------------------------------------------------------------------
// Add-local identity classification for PresentLuks disks
// ---------------------------------------------------------------------------

/// Identity classification for a PresentLuks disk in the add path.
/// This is add-local — not shared with unlock, status, replace, etc.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(dead_code)] // NonBraid/BraidLabeledNoPool are handled before classify_braid_disk_fsid
enum AddLuksIdentity {
    /// LUKS label is not braid-<name> (or absent).
    NonBraid,
    /// Correct braid label, but pool is not mounted — can't verify.
    BraidLabeledNoPool,
    /// Correct braid label, mapper open, no btrfs superblock.
    /// Ambiguous: could be clean eviction, partial init, or manual wipe.
    /// Refused — operator must wipe the disk to re-add as fresh.
    BraidLabeledNoBtrfs,
    /// Correct braid label, mapper open, btrfs FSID differs from pool.
    BraidLabeledForeignPool,
    /// Correct braid label, mapper open, btrfs FSID matches pool, already in pool.
    BraidLabeledAlreadyInPool,
    /// Correct braid label, mapper open, btrfs FSID matches pool, not yet in pool.
    BraidLabeledRecoverable,
}

/// Read the LUKS label from a raw device (no mapper open required).
fn read_luks_label<R: CommandRunner>(runner: &R, device: &str) -> Result<Option<String>, AddError> {
    let raw = runner.run(&CmdRequest::CryptsetupLuksDumpText {
        device: device.to_owned(),
    })?;
    let out = crate::parse::parse_cryptsetup_luks_label(&raw)?;
    Ok(out.label)
}

/// Validate the preconditions for adding a PresentLuks disk.
/// Reads the LUKS label and checks the pool is mounted.
/// No side effects — works on the raw device, no mapper required.
fn validate_braid_preconditions<R: CommandRunner>(
    runner: &R,
    name: &str,
    device: &str,
    pool: &PoolState,
) -> Result<(), AddError> {
    let label = read_luks_label(runner, device)?;
    let expected_label = format!("braid-{name}");
    if label.as_deref() != Some(expected_label.as_str()) {
        return Err(AddError::Validation(format!(
            "disk '{}' ({}) is already a LUKS container but is not labeled as {}; \
             braid will not adopt a non-braid encrypted device",
            name, device, expected_label,
        )));
    }
    if !pool.mounted {
        return Err(AddError::Validation(format!(
            "disk '{}' is braid-labeled but no mounted pool exists to verify identity; \
             bootstrap only accepts fresh disks",
            name,
        )));
    }
    Ok(())
}

/// Classify a braid-labeled PresentLuks disk whose mapper is already open.
/// Checks btrfs superblock presence and FSID against the mounted pool.
/// Caller must ensure: pool.mounted == true, mapper is open.
fn classify_braid_disk_fsid<R: CommandRunner>(
    runner: &R,
    name: &str,
    mapper: &MapperName,
    pool: &PoolState,
) -> Result<AddLuksIdentity, AddError> {
    let mapper_path = format!("/dev/mapper/{}", mapper.0);
    let show_raw = runner.run(&CmdRequest::BtrfsFilesystemShowTarget {
        target: mapper_path,
    })?;

    match classify_btrfs_probe(&show_raw) {
        DeviceBtrfsProbe::NoBtrfs => return Ok(AddLuksIdentity::BraidLabeledNoBtrfs),
        DeviceBtrfsProbe::Unknown(msg) => {
            return Err(AddError::Cmd(CmdError::Failed(format!(
                "disk '{}': {}",
                name, msg
            ))));
        }
        DeviceBtrfsProbe::HasBtrfs => {}
    }

    let show = parse_btrfs_filesystem_show(&show_raw)?;

    // The device passed HasBtrfs (exit 0) so btrfs filesystem show should
    // have printed a uuid line. None means the parser couldn't extract it —
    // fail rather than silently skipping the foreign-pool guard.
    let device_fsid = show.uuid.as_ref().ok_or_else(|| {
        AddError::Validation(format!(
            "disk '{}': btrfs superblock present but no UUID in \
             btrfs filesystem show output",
            name,
        ))
    })?;

    // pool.fsid is guaranteed Some for mounted pools by probe_pool.
    let pool_fsid = pool.fsid.as_ref().expect("mounted pool must have FSID");

    if device_fsid != pool_fsid {
        return Ok(AddLuksIdentity::BraidLabeledForeignPool);
    }

    if pool.devices.iter().any(|d| d.mapper == *mapper) {
        return Ok(AddLuksIdentity::BraidLabeledAlreadyInPool);
    }

    Ok(AddLuksIdentity::BraidLabeledRecoverable)
}

/// Map an AddLuksIdentity error variant to a canonical AddError.
/// Returns None for non-error outcomes (AlreadyInPool, Recoverable).
fn identity_to_error(identity: &AddLuksIdentity, name: &str) -> Option<AddError> {
    match identity {
        AddLuksIdentity::BraidLabeledNoBtrfs => Some(AddError::Validation(format!(
            "disk '{}' is braid-labeled but contains no btrfs superblock; \
             identity is ambiguous, so braid will not re-add it automatically. \
             Wipe the disk and add it again as fresh.",
            name,
        ))),
        AddLuksIdentity::BraidLabeledForeignPool => Some(AddError::Validation(format!(
            "disk '{}' is a braid-managed device from a different btrfs filesystem; \
             braid will not merge foreign pools",
            name,
        ))),
        _ => None,
    }
}

/// Tracks LUKS mappers opened by this invocation of cmd_add.
/// On drop (error path), closes them best-effort.
/// Call `disarm()` on the success path to skip cleanup.
struct LuksCleanupGuard<'a, R: CommandRunner> {
    runner: &'a R,
    mappers: Vec<String>,
    armed: bool,
}

impl<'a, R: CommandRunner> LuksCleanupGuard<'a, R> {
    fn new(runner: &'a R) -> Self {
        Self {
            runner,
            mappers: Vec::new(),
            armed: true,
        }
    }

    fn track(&mut self, mapper: String) {
        self.mappers.push(mapper);
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<R: CommandRunner> Drop for LuksCleanupGuard<'_, R> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        for mapper in self.mappers.iter().rev() {
            match self.runner.run(&CmdRequest::CryptsetupClose {
                mapper: mapper.clone(),
            }) {
                Ok(r) if r.exit_status == 0 => {
                    eprintln!("cleanup: closed LUKS mapper {}", mapper);
                }
                Ok(r) => {
                    eprintln!(
                        "cleanup: failed to close LUKS mapper {} (exit {}): {}",
                        mapper,
                        r.exit_status,
                        r.stderr.trim()
                    );
                }
                Err(e) => {
                    eprintln!("cleanup: failed to close LUKS mapper {}: {}", mapper, e);
                }
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn cmd_add<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config_path: &Path,
    disk_specs: &[String],
    dry_run: bool,
    yes: bool,
    passphrase_stdin: bool,
    passphrase_file: Option<&Path>,
    enroll_key_file: Option<&Path>,
    progress: ProgressOutput,
    paths: &StatePaths,
) -> Result<(), AddError> {
    preflight::check_no_pending_operation(paths).map_err(AddError::Validation)?;

    let config = config_read(config_path)?;

    // Parse disk specs: name=by_id
    let parsed: Vec<(String, ByIdPath)> = disk_specs
        .iter()
        .map(|s| membership::parse_disk_spec(s))
        .collect::<Result<Vec<_>, _>>()?;

    let names: Vec<&str> = parsed.iter().map(|(n, _)| n.as_str()).collect();
    let by_ids: Vec<&ByIdPath> = parsed.iter().map(|(_, b)| b).collect();

    // Reject duplicate names upfront
    {
        let mut seen = std::collections::HashSet::new();
        for name in &names {
            if !seen.insert(*name) {
                return Err(AddError::Validation(format!(
                    "duplicate disk name: '{name}'"
                )));
            }
        }
    }

    // Load existing membership (or empty if first add)
    let pool_membership = match membership::load_membership(paths) {
        Ok(m) => m,
        Err(membership::MembershipError::NotFound(_)) => PoolMembership::empty(),
        Err(e) => return Err(e.into()),
    };

    // Validate no conflicts
    for (name, by_id) in &parsed {
        membership::validate_no_conflicts(&pool_membership, name, &by_id.0)?;
    }

    // Probe all disks — fail early if any absent
    let probed: Vec<ConfigDisk> = names
        .iter()
        .zip(by_ids.iter())
        .map(|(name, by_id)| probe_config_disk(runner, fs, name, by_id))
        .collect::<Result<Vec<_>, _>>()?;

    for (i, p) in probed.iter().enumerate() {
        if matches!(p.state, ConfigDiskState::Absent) {
            return Err(AddError::Validation(format!(
                "disk '{}' ({}) is not present. Is it plugged in?",
                names[i], by_ids[i]
            )));
        }
    }

    // Probe pool + preflight (once)
    let pool = match probe_pool(runner, config.mount_point().as_str()) {
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
        preflight::check_no_exclusive_op(runner, config.mount_point().as_str())
            .map_err(AddError::Validation)?;
        preflight::check_not_read_only(runner, config.mount_point().as_str())
            .map_err(AddError::Validation)?;
    }
    if pool.missing_count > 0 {
        eprintln!(
            "warning: pool has {} missing device{}. \
             Consider repairing with `braid replace --missing-id <devid>` first. \
             Use `braid status` to see device IDs.",
            pool.missing_count,
            if pool.missing_count == 1 { "" } else { "s" }
        );
    }

    // Compile steps for dry-run display
    let steps = compile_add_steps_multi(
        runner,
        &names,
        &by_ids,
        &probed,
        &pool,
        config.mount_point(),
    )?;

    if dry_run {
        for step in &steps {
            println!("[{:<11}] {}", step.risk, step.description);
        }
        return Ok(());
    }

    if steps.is_empty() {
        let label = if names.len() == 1 {
            names[0].to_owned()
        } else {
            names.iter().copied().collect::<Vec<_>>().join(", ")
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
        .map(|(i, _)| (names[i], by_ids[i].0.as_str()))
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

    // Pass 1: validate PresentLuks disk identities before any irreversible operation.
    // Guard closes any mappers we opened for FSID verification if validation fails.
    let mut luks_guard = LuksCleanupGuard::new(runner);
    let mut needs_pool_add: Vec<usize> = Vec::new();

    for (i, p) in probed.iter().enumerate() {
        let ConfigDiskState::PresentLuks { mapper_open, .. } = &p.state else {
            continue;
        };
        let name = names[i];
        let by_id = by_ids[i];
        let mn = mapper_name(name);

        validate_braid_preconditions(runner, name, &by_id.0, &pool)?;

        // Open mapper if closed (now we know it's braid-labeled + pool is up)
        if !mapper_open {
            ensure_luks_open(runner, fs, name, by_id, &passphrase)?;
            luks_guard.track(mn.0.clone());
            eprintln!("LUKS opened: {} → {}", by_id, mn);
        }

        let identity = classify_braid_disk_fsid(runner, name, &mn, &pool)?;
        if let Some(err) = identity_to_error(&identity, name) {
            return Err(err);
        }
        match identity {
            AddLuksIdentity::BraidLabeledAlreadyInPool => continue,
            AddLuksIdentity::BraidLabeledRecoverable => {
                eprintln!(
                    "note: braid-labeled disk '{}' verified as pool member. \
                     Completing recovery add.",
                    name
                );
                needs_pool_add.push(i);
            }
            _ => unreachable!("error variants handled by identity_to_error above"),
        }
    }

    let has_fresh_disks = probed
        .iter()
        .any(|p| matches!(p.state, ConfigDiskState::PresentNotLuks));

    if !has_fresh_disks && needs_pool_add.is_empty() {
        luks_guard.disarm();
        let label = if names.len() == 1 {
            names[0].to_owned()
        } else {
            names.iter().copied().collect::<Vec<_>>().join(", ")
        };
        eprintln!("Nothing to do — {} already in pool.", label);
        return Ok(());
    }

    // All identity checks passed. Write journal before irreversible disk operations.
    let mut target_membership = pool_membership.clone();
    for (name, by_id) in &parsed {
        target_membership.disks.insert(
            name.clone(),
            membership::DiskMember::from_by_id(by_id.clone()),
        );
    }
    let journal = journal::build_journal(
        pool_membership.clone(),
        target_membership,
        journal::OpKind::Add {
            disks: parsed.iter().map(|(n, b)| (n.clone(), b.clone())).collect(),
        },
    );
    journal::write_journal(paths, &journal).map_err(|e| AddError::Validation(e.to_string()))?;

    // Pass 2: execute irreversible operations for PresentNotLuks disks.
    for (i, p) in probed.iter().enumerate() {
        if !matches!(p.state, ConfigDiskState::PresentNotLuks) {
            continue;
        }
        let name = names[i];
        let by_id = by_ids[i];
        let mn = mapper_name(name);

        let mut luks_opts = luks_opts_from_env();
        luks_opts.push("--label".into());
        luks_opts.push(format!("braid-{name}"));
        luks_format(runner, &by_id.0, &passphrase, &luks_opts)?;
        eprintln!("LUKS formatted: {}", by_id);

        let backup_path = backup_luks_header(runner, &by_id.0, &mn.0, paths)?;
        eprintln!("LUKS header backed up: {}", backup_path.display());

        ensure_luks_open(runner, fs, name, by_id, &passphrase)?;
        luks_guard.track(mn.0.clone());
        eprintln!("LUKS opened: {} → {}", by_id, mn);

        if let Some(kf) = enroll_key_file {
            crate::luks::enroll_key_file(runner, &by_id.0, &passphrase, kf)?;
            eprintln!("Keyfile enrolled in slot 1: {}", by_id);
        }

        needs_pool_add.push(i);
    }

    // Both passes complete — mappers are committed for pool operations.
    luks_guard.disarm();

    // Pool phase
    let mapper_paths: Vec<String> = needs_pool_add
        .iter()
        .map(|&i| format!("/dev/mapper/{}", mapper_name(names[i]).0))
        .collect();

    if !pool.mounted {
        if mapper_paths.len() >= 2 {
            // Bootstrap with mkfs.btrfs RAID1
            pool_bootstrap_mount_raid1(runner, &mapper_paths, config.mount_point().as_str())?;
            eprintln!(
                "Pool created (RAID1) and mounted at {}",
                config.mount_point()
            );
        } else {
            // Single disk bootstrap
            pool_bootstrap_mount(runner, &mapper_paths[0], config.mount_point().as_str())?;
            eprintln!("Pool created and mounted at {}", config.mount_point());
        }
    } else {
        // Add each to existing pool
        for mp in &mapper_paths {
            pool_add_device(runner, mp, config.mount_point().as_str())?;
            eprintln!("Device added to pool: {}", mp);
        }

        // Balance to RAID1 if total >= 2
        let total_after = pool.devices.len() + mapper_paths.len();
        if total_after >= 2 {
            eprintln!("Balancing to RAID1...");
            pool_balance_raid1(runner, config.mount_point().as_str(), progress)?;
            eprintln!("Balance complete.");
        }
    }

    // Post-commit persist: write pool.json only after all disk ops succeed.
    // Enrich with live metadata (luks_uuid, devid) from pool probe.
    let mut final_membership = journal.target_membership.clone();
    if let Ok(pool_after) = probe_pool(runner, config.mount_point().as_str()) {
        for dev in &pool_after.devices {
            let Some(name) = crate::config::name_from_mapper(&dev.mapper.0) else {
                continue;
            };
            if let Some(member) = final_membership.disks.get_mut(name) {
                member.luks_uuid = Some(dev.luks_uuid.clone());
                member.devid = Some(dev.devid);
                if member.added_at.is_none() {
                    member.added_at = Some(crate::util::now_iso());
                }
            }
        }
    }
    // Order matters: save_membership before clear_journal. If save_membership
    // fails (disk full, permissions), the journal survives and braid recover can
    // reconstruct pool.json from the live pool. The reverse order (clear first,
    // then save fails) would leave no recovery path.
    membership::save_membership(&final_membership, paths)?;
    journal::clear_journal(paths).map_err(|e| AddError::Validation(e.to_string()))?;

    let label = if names.len() == 1 {
        format!("{} is", names[0])
    } else {
        format!("{} are", names.join(", "))
    };
    eprintln!("Done. {} now part of the pool.", label);
    Ok(())
}

fn compile_add_steps_multi<R: CommandRunner>(
    runner: &R,
    names: &[&str],
    by_ids: &[&ByIdPath],
    probed: &[ConfigDisk],
    pool: &PoolState,
    mount_point: &MountPoint,
) -> Result<Vec<AddStep>, AddError> {
    let mut steps = Vec::new();
    let mut needs_pool_add = 0usize;

    for (i, p) in probed.iter().enumerate() {
        let name = names[i];
        let by_id = by_ids[i];
        let mn = mapper_name(name);

        match &p.state {
            ConfigDiskState::Absent => {
                return Err(AddError::Validation(format!(
                    "disk '{}' ({}) is not present. Is it plugged in?",
                    name, by_id
                )));
            }
            ConfigDiskState::PresentNotLuks => {
                steps.push(AddStep {
                    risk: "destructive",
                    description: format!("LUKS format {}", by_id),
                });
                steps.push(AddStep {
                    risk: "safe",
                    description: format!("LUKS open → {}", mn),
                });
                needs_pool_add += 1;
            }
            ConfigDiskState::PresentLuks { mapper_open, .. } => {
                // Preconditions always checked — no mapper required.
                validate_braid_preconditions(runner, name, &by_id.0, pool)?;

                if *mapper_open {
                    // Mapper is open — full classification without side effects
                    let identity = classify_braid_disk_fsid(runner, name, &mn, pool)?;
                    if let Some(err) = identity_to_error(&identity, name) {
                        return Err(err);
                    }
                    match identity {
                        AddLuksIdentity::BraidLabeledAlreadyInPool => continue,
                        AddLuksIdentity::BraidLabeledRecoverable => {
                            steps.push(AddStep {
                                risk: "safe",
                                description: format!(
                                    "btrfs device add /dev/mapper/{} {} (recovery)",
                                    mn, mount_point
                                ),
                            });
                            needs_pool_add += 1;
                        }
                        _ => unreachable!("error variants handled by identity_to_error above"),
                    }
                } else {
                    // Mapper closed — FSID verification deferred to execution time.
                    steps.push(AddStep {
                        risk: "safe",
                        description: format!(
                            "LUKS open + identity verification at execution time → {}",
                            mn
                        ),
                    });
                    needs_pool_add += 1;
                }
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
            description: format!("mount → {}", mount_point),
        });
    } else {
        for (i, p) in probed.iter().enumerate() {
            let mn = mapper_name(&names[i]);
            // PresentNotLuks disks still need the device-add step
            if matches!(&p.state, ConfigDiskState::PresentNotLuks) {
                steps.push(AddStep {
                    risk: "safe",
                    description: format!("btrfs device add /dev/mapper/{} {}", mn, mount_point),
                });
            }
            // PresentLuks recovery steps already added above
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
            fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
                Ok(vec![])
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
            &[
                "d1=/dev/disk/by-id/ata-D1".into(),
                "d1=/dev/disk/by-id/ata-D1".into(),
            ],
            true,
            true,
            false,
            None,
            None,
            ProgressOutput::Off,
            &StatePaths::production(),
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("duplicate disk name"),
            "expected duplicate error, got: {err}"
        );
    }

    // -----------------------------------------------------------------------
    // Identity classification tests
    // -----------------------------------------------------------------------

    use crate::cmd::{MockRunner, RawCommandOutput};

    fn luks_dump_text_with_label(label: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "cryptsetup luksDump /dev/disk/by-id/disk1".into(),
            stdout: format!(
                "LUKS header information\n\
                 Version:       \t2\n\
                 Label:         \t{label}\n\
                 Subsystem:     \t(no subsystem)\n"
            ),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn luks_dump_text_no_label() -> RawCommandOutput {
        RawCommandOutput {
            cmd: "cryptsetup luksDump /dev/disk/by-id/disk1".into(),
            stdout: "LUKS header information\n\
                     Version:       \t2\n\
                     Label:         \t(no label)\n\
                     Subsystem:     \t(no subsystem)\n"
                .into(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn btrfs_show_with_uuid(uuid: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "btrfs filesystem show /dev/mapper/braid-disk1".into(),
            stdout: format!(
                "Label: none  uuid: {uuid}\n\
                 \tTotal devices 1 FS bytes used 16.00MiB\n\
                 \tdevid    1 size 500.00MiB used 100.00MiB path /dev/mapper/braid-disk1\n"
            ),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn btrfs_show_no_btrfs() -> RawCommandOutput {
        RawCommandOutput {
            cmd: "btrfs filesystem show /dev/mapper/braid-disk1".into(),
            stdout: String::new(),
            stderr: "ERROR: not a valid btrfs filesystem on /dev/mapper/braid-disk1".into(),
            exit_status: 1,
        }
    }

    fn pool_mounted_with_fsid(fsid: &str) -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-existing".into()),
                luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
                devid: 1,
                underlying: "/dev/vda".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: Some(fsid.to_owned()),
        }
    }

    fn pool_unmounted() -> PoolState {
        PoolState {
            mounted: false,
            devices: vec![],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 0,
            fsid: None,
        }
    }

    // --- read_luks_label tests ---

    #[test]
    fn read_label_extracts_braid_label() {
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksDumpText {
                device: "/dev/disk/by-id/disk1".into(),
            },
            luks_dump_text_with_label("braid-disk1"),
        );
        let label = read_luks_label(&runner, "/dev/disk/by-id/disk1").unwrap();
        assert_eq!(label, Some("braid-disk1".to_owned()));
    }

    #[test]
    fn read_label_returns_none_for_no_label() {
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksDumpText {
                device: "/dev/disk/by-id/disk1".into(),
            },
            luks_dump_text_no_label(),
        );
        let label = read_luks_label(&runner, "/dev/disk/by-id/disk1").unwrap();
        assert_eq!(label, None);
    }

    #[test]
    fn read_label_returns_non_braid_label() {
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksDumpText {
                device: "/dev/disk/by-id/disk1".into(),
            },
            luks_dump_text_with_label("other-thing"),
        );
        let label = read_luks_label(&runner, "/dev/disk/by-id/disk1").unwrap();
        assert_eq!(label, Some("other-thing".to_owned()));
    }

    // --- classify_braid_disk_fsid tests ---

    #[test]
    fn classify_fsid_no_btrfs() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShowTarget {
                target: "/dev/mapper/braid-disk1".into(),
            },
            btrfs_show_no_btrfs(),
        );
        let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        let mn = MapperName("braid-disk1".into());

        let result = classify_braid_disk_fsid(&runner, "disk1", &mn, &pool).unwrap();
        assert_eq!(result, AddLuksIdentity::BraidLabeledNoBtrfs);
    }

    #[test]
    fn classify_fsid_foreign_pool() {
        let pool_fsid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let device_fsid = "11111111-2222-3333-4444-555555555555";

        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShowTarget {
                target: "/dev/mapper/braid-disk1".into(),
            },
            btrfs_show_with_uuid(device_fsid),
        );
        let pool = pool_mounted_with_fsid(pool_fsid);
        let mn = MapperName("braid-disk1".into());

        let result = classify_braid_disk_fsid(&runner, "disk1", &mn, &pool).unwrap();
        assert_eq!(result, AddLuksIdentity::BraidLabeledForeignPool);
    }

    #[test]
    fn classify_fsid_already_in_pool() {
        let fsid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShowTarget {
                target: "/dev/mapper/braid-disk1".into(),
            },
            btrfs_show_with_uuid(fsid),
        );
        // Pool contains braid-disk1 already
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk1".into()),
                luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
                devid: 1,
                underlying: "/dev/vda".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: Some(fsid.to_owned()),
        };
        let mn = MapperName("braid-disk1".into());

        let result = classify_braid_disk_fsid(&runner, "disk1", &mn, &pool).unwrap();
        assert_eq!(result, AddLuksIdentity::BraidLabeledAlreadyInPool);
    }

    #[test]
    fn classify_fsid_recoverable() {
        let fsid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShowTarget {
                target: "/dev/mapper/braid-disk1".into(),
            },
            btrfs_show_with_uuid(fsid),
        );
        // Pool does NOT contain braid-disk1 yet
        let pool = pool_mounted_with_fsid(fsid);
        let mn = MapperName("braid-disk1".into());

        let result = classify_braid_disk_fsid(&runner, "disk1", &mn, &pool).unwrap();
        assert_eq!(result, AddLuksIdentity::BraidLabeledRecoverable);
    }

    // Device has a btrfs superblock (exit 0) but the parser finds no uuid
    // line. This is the dangerous case: without a device FSID, the
    // foreign-pool guard cannot run. Must fail rather than fall through
    // to Recoverable.
    #[test]
    fn classify_fsid_errors_on_missing_device_uuid() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShowTarget {
                target: "/dev/mapper/braid-disk1".into(),
            },
            RawCommandOutput {
                cmd: "btrfs filesystem show /dev/mapper/braid-disk1".into(),
                stdout: "\tTotal devices 1 FS bytes used 16.00MiB\n\
                         \tdevid    1 size 500.00MiB used 100.00MiB path /dev/mapper/braid-disk1\n"
                    .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        let mn = MapperName("braid-disk1".into());

        let result = classify_braid_disk_fsid(&runner, "disk1", &mn, &pool);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no UUID"),
            "expected error about missing UUID, got: {err}"
        );
    }

    // --- compile_add_steps_multi identity tests ---

    fn probed_present_luks(name: &str, mapper_open: bool) -> ConfigDisk {
        ConfigDisk {
            name: name.to_owned(),
            by_id_path: ByIdPath("/dev/disk/by-id/disk1".to_owned()),
            state: ConfigDiskState::PresentLuks {
                uuid: LuksUuid("a1b2c3d4-e5f6-7890-abcd-ef1234567890".into()),
                mapper_open,
            },
        }
    }

    #[test]
    fn dry_run_non_braid_luks_reports_blocked() {
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksDumpText {
                device: "/dev/disk/by-id/disk1".into(),
            },
            luks_dump_text_no_label(),
        );
        let probed = vec![probed_present_luks("disk1", true)];
        let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

        let result = compile_add_steps_multi(
            &runner,
            &["disk1"],
            &[&ByIdPath("/dev/disk/by-id/disk1".into())],
            &probed,
            &pool,
            &MountPoint("/mnt/storage".into()),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("not labeled as braid-disk1"),
            "expected non-braid error, got: {err}"
        );
    }

    #[test]
    fn dry_run_braid_labeled_foreign_fsid_reports_blocked() {
        let pool_fsid = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        let device_fsid = "11111111-2222-3333-4444-555555555555";

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk1".into(),
                },
                luks_dump_text_with_label("braid-disk1"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk1".into(),
                },
                btrfs_show_with_uuid(device_fsid),
            );
        let probed = vec![probed_present_luks("disk1", true)];
        let pool = pool_mounted_with_fsid(pool_fsid);

        let result = compile_add_steps_multi(
            &runner,
            &["disk1"],
            &[&ByIdPath("/dev/disk/by-id/disk1".into())],
            &probed,
            &pool,
            &MountPoint("/mnt/storage".into()),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("different btrfs filesystem"),
            "expected foreign-pool error, got: {err}"
        );
    }

    #[test]
    fn dry_run_braid_labeled_mapper_closed_reports_deferred() {
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksDumpText {
                device: "/dev/disk/by-id/disk1".into(),
            },
            luks_dump_text_with_label("braid-disk1"),
        );
        let probed = vec![probed_present_luks("disk1", false)];
        let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

        let steps = compile_add_steps_multi(
            &runner,
            &["disk1"],
            &[&ByIdPath("/dev/disk/by-id/disk1".into())],
            &probed,
            &pool,
            &MountPoint("/mnt/storage".into()),
        )
        .unwrap();

        assert!(
            steps.iter().any(|s| s
                .description
                .contains("identity verification at execution time")),
            "expected deferred verification step, got: {:?}",
            steps.iter().map(|s| &s.description).collect::<Vec<_>>()
        );
    }

    #[test]
    fn dry_run_braid_labeled_no_pool_reports_blocked() {
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksDumpText {
                device: "/dev/disk/by-id/disk1".into(),
            },
            luks_dump_text_with_label("braid-disk1"),
        );
        let probed = vec![probed_present_luks("disk1", false)];
        let pool = pool_unmounted();

        let result = compile_add_steps_multi(
            &runner,
            &["disk1"],
            &[&ByIdPath("/dev/disk/by-id/disk1".into())],
            &probed,
            &pool,
            &MountPoint("/mnt/storage".into()),
        );
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no mounted pool exists"),
            "expected no-pool error, got: {err}"
        );
    }

    #[test]
    fn dry_run_raw_disk_still_shows_destructive_format() {
        let runner = MockRunner::default();
        let probed = vec![ConfigDisk {
            name: "disk1".to_owned(),
            by_id_path: ByIdPath("/dev/disk/by-id/disk1".to_owned()),
            state: ConfigDiskState::PresentNotLuks,
        }];
        let pool = pool_unmounted();

        let steps = compile_add_steps_multi(
            &runner,
            &["disk1"],
            &[&ByIdPath("/dev/disk/by-id/disk1".into())],
            &probed,
            &pool,
            &MountPoint("/mnt/storage".into()),
        )
        .unwrap();

        assert!(
            steps
                .iter()
                .any(|s| s.risk == "destructive" && s.description.contains("LUKS format")),
            "expected destructive LUKS format step, got: {:?}",
            steps
                .iter()
                .map(|s| format!("[{}] {}", s.risk, s.description))
                .collect::<Vec<_>>()
        );
    }

    // -----------------------------------------------------------------------
    // validate_braid_preconditions / identity_to_error canonical message tests
    // -----------------------------------------------------------------------

    #[test]
    fn preconditions_non_braid_label_canonical_message() {
        // Intent: validate_braid_preconditions emits the canonical label-mismatch error.
        // Why it exists: pins the error text so both cmd_add and compile_add_steps_multi
        //   can't drift — they both call this function.
        // Scenario: user tries to add a LUKS disk that was not created by braid.
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksDumpText {
                device: "/dev/disk/by-id/disk1".into(),
            },
            luks_dump_text_with_label("some-other-label"),
        );
        let pool = pool_unmounted();
        let err = validate_braid_preconditions(&runner, "disk1", "/dev/disk/by-id/disk1", &pool)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not labeled as braid-disk1"), "got: {err}");
        assert!(
            err.contains("braid will not adopt a non-braid encrypted device"),
            "got: {err}"
        );
    }

    #[test]
    fn preconditions_no_pool_canonical_message() {
        // Intent: validate_braid_preconditions emits the canonical no-mounted-pool error.
        // Why it exists: pins the error text so both cmd_add and compile_add_steps_multi
        //   can't drift — they both call this function.
        // Scenario: user tries to add a braid-labeled disk when no pool is mounted
        //   (e.g. fresh bootstrap scenario with pre-existing encrypted disk).
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksDumpText {
                device: "/dev/disk/by-id/disk1".into(),
            },
            luks_dump_text_with_label("braid-disk1"),
        );
        let pool = pool_unmounted();
        let err = validate_braid_preconditions(&runner, "disk1", "/dev/disk/by-id/disk1", &pool)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no mounted pool exists to verify identity"),
            "got: {err}"
        );
        assert!(
            err.contains("bootstrap only accepts fresh disks"),
            "got: {err}"
        );
    }

    #[test]
    fn identity_to_error_no_btrfs_canonical_message() {
        // Intent: identity_to_error emits the canonical BraidLabeledNoBtrfs error.
        // Why it exists: this was the variant where message text had already diverged
        //   between cmd_add and compile_add_steps_multi. Pinning it prevents recurrence.
        // Scenario: a braid-labeled disk has its LUKS contents wiped or is partially
        //   initialized — btrfs superblock is absent.
        let err = identity_to_error(&AddLuksIdentity::BraidLabeledNoBtrfs, "disk1")
            .unwrap()
            .to_string();
        assert!(err.contains("contains no btrfs superblock"), "got: {err}");
        assert!(err.contains("identity is ambiguous"), "got: {err}");
        assert!(
            err.contains("Wipe the disk and add it again as fresh"),
            "got: {err}"
        );
    }

    #[test]
    fn identity_to_error_foreign_pool_canonical_message() {
        // Intent: identity_to_error emits the canonical BraidLabeledForeignPool error.
        // Why it exists: pins the error text so both call sites can't drift independently.
        // Scenario: user tries to add a braid-labeled disk from a different NAS.
        let err = identity_to_error(&AddLuksIdentity::BraidLabeledForeignPool, "disk1")
            .unwrap()
            .to_string();
        assert!(err.contains("different btrfs filesystem"), "got: {err}");
        assert!(
            err.contains("braid will not merge foreign pools"),
            "got: {err}"
        );
    }

    #[test]
    fn identity_to_error_success_variants_return_none() {
        // Intent: identity_to_error returns None for non-error outcomes.
        // Why it exists: callers rely on None meaning "proceed" — ensures neither
        //   success variant accidentally becomes an error after future edits.
        // Scenario: normal add (AlreadyInPool → no-op, Recoverable → recovery add).
        assert!(identity_to_error(&AddLuksIdentity::BraidLabeledAlreadyInPool, "disk1").is_none());
        assert!(identity_to_error(&AddLuksIdentity::BraidLabeledRecoverable, "disk1").is_none());
    }

    #[test]
    fn dry_run_and_execution_produce_same_no_btrfs_error() {
        // Intent: compile_add_steps_multi and cmd_add produce identical BraidLabeledNoBtrfs
        //   error text, proving both call sites go through identity_to_error.
        // Why it exists: this is the exact message that had already diverged before the
        //   refactor. This test makes that divergence impossible to reintroduce silently.
        // Scenario: braid-labeled disk with mapper open, but no btrfs superblock inside.

        // dry-run path: compile_add_steps_multi with mapper_open=true
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk1".into(),
                },
                luks_dump_text_with_label("braid-disk1"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk1".into(),
                },
                btrfs_show_no_btrfs(),
            );
        let probed = vec![probed_present_luks("disk1", true)];
        let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

        let dry_err = compile_add_steps_multi(
            &runner,
            &["disk1"],
            &[&ByIdPath("/dev/disk/by-id/disk1".into())],
            &probed,
            &pool,
            &MountPoint("/mnt/storage".into()),
        )
        .unwrap_err()
        .to_string();

        // execution path: identity_to_error is the shared function cmd_add calls
        let exec_err = identity_to_error(&AddLuksIdentity::BraidLabeledNoBtrfs, "disk1")
            .unwrap()
            .to_string();

        assert_eq!(
            dry_err, exec_err,
            "dry-run and execution paths must produce identical BraidLabeledNoBtrfs error"
        );
    }

    // -----------------------------------------------------------------------
    // LuksCleanupGuard tests
    // -----------------------------------------------------------------------

    use std::sync::Mutex;

    /// Test-only CommandRunner that delegates to MockRunner but records
    /// which mapper names were passed to CryptsetupClose.
    struct SpyRunner {
        inner: MockRunner,
        closed: Mutex<Vec<String>>,
    }

    impl SpyRunner {
        fn new(inner: MockRunner) -> Self {
            Self {
                inner,
                closed: Mutex::new(Vec::new()),
            }
        }
    }

    impl CommandRunner for SpyRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            if let CmdRequest::CryptsetupClose { mapper } = request {
                self.closed.lock().unwrap().push(mapper.clone());
                return Ok(RawCommandOutput {
                    cmd: format!("cryptsetup close {mapper}"),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                });
            }
            self.inner.run(request)
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.inner.run_with_stdin(request, stdin)
        }
    }

    #[test]
    fn guard_closes_on_armed_drop() {
        // Intent: Drop calls CryptsetupClose for each tracked mapper.
        // Why it exists: core correctness — without this, the guard is dead code.
        // Scenario: cmd_add opens a mapper, a later step in the LUKS phase
        // fails, the guard fires on unwind and closes the mapper.
        let runner = SpyRunner::new(MockRunner::default());
        {
            let mut guard = LuksCleanupGuard::new(&runner);
            guard.track("braid-aaa".into());
            guard.track("braid-bbb".into());
            // guard drops here while still armed
        }
        let closed = runner.closed.lock().unwrap();
        assert_eq!(
            *closed,
            vec!["braid-bbb", "braid-aaa"],
            "should close tracked mappers in reverse order"
        );
    }

    #[test]
    fn guard_noop_when_disarmed() {
        // Intent: disarm() prevents close on drop.
        // Why it exists: successful LUKS phase must not close the mappers it
        // just opened — they're needed for the pool phase.
        // Scenario: all identity checks pass, guard is disarmed, drop is a no-op.
        let runner = SpyRunner::new(MockRunner::default());
        {
            let mut guard = LuksCleanupGuard::new(&runner);
            guard.track("braid-aaa".into());
            guard.disarm();
            // guard drops here, disarmed
        }
        let closed = runner.closed.lock().unwrap();
        assert!(
            closed.is_empty(),
            "disarmed guard should not close anything, got: {:?}",
            *closed
        );
    }

    #[test]
    fn preexisting_mapper_not_closed() {
        // Intent: a mapper already open before cmd_add is not tracked or closed.
        // Why it exists: closing a pre-existing mapper would break a running pool.
        // Scenario: PresentLuks with mapper_open=true fails identity check;
        // the guard must not close that mapper since we didn't open it.
        let runner = SpyRunner::new(MockRunner::default());
        {
            let mut guard = LuksCleanupGuard::new(&runner);
            // Only track mappers we opened ourselves.
            // Pre-existing mapper "braid-existing" is NOT tracked.
            guard.track("braid-new".into());
            // guard drops here while armed — simulates error path
        }
        let closed = runner.closed.lock().unwrap();
        assert_eq!(*closed, vec!["braid-new"]);
        assert!(
            !closed.contains(&"braid-existing".to_string()),
            "must not close a mapper we didn't open"
        );
    }

    // -----------------------------------------------------------------------
    // Journal survival tests
    // -----------------------------------------------------------------------

    use crate::membership;
    use crate::state_paths::StatePaths;

    fn mock_ok(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    struct AddMockFs(Vec<String>);
    impl crate::probe::Filesystem for AddMockFs {
        fn exists(&self, path: &str) -> bool {
            self.0.iter().any(|p| p == path)
        }
        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    const POOL_FSID: &str = "cc86845b-aec3-408e-bef5-553affc1f2b1";

    /// Runner for add tests. Pool has 1 device (disk1). New disk (disk2) is
    /// LUKS-labeled and open.
    struct AddTestRunner {
        /// If true, the new disk's mapper is already in the pool (no-op path).
        /// If false, the disk's FSID matches but it's not in pool (recoverable).
        disk_in_pool: bool,
        fail_device_add: bool,
        /// If true, BtrfsFilesystemShowTarget returns "not a valid btrfs" (BraidLabeledNoBtrfs).
        no_btrfs_superblock: bool,
    }

    impl CommandRunner for AddTestRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_ok(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    let disk2_line = if self.disk_in_pool {
                        "\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n"
                    } else {
                        ""
                    };
                    let total = if self.disk_in_pool { 2 } else { 1 };
                    Ok(mock_ok(
                        &format!("btrfs filesystem show {mount_point}"),
                        &format!(
                            "Label: none  uuid: {POOL_FSID}\n\tTotal devices {total} FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n{disk2_line}",
                        ),
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_ok(
                    &format!("cryptsetup status {mapper}"),
                    &format!("{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"),
                )),
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" => "11111111-1111-1111-1111-111111111111",
                        _ => "22222222-2222-2222-2222-222222222222",
                    };
                    Ok(mock_ok(&format!("cryptsetup luksUUID {device}"), &format!("{uuid}\n")))
                }
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                // LUKS label check for new disk
                CmdRequest::CryptsetupLuksDumpText { .. } => Ok(mock_ok(
                    "cryptsetup luksDump",
                    "LUKS header information\nVersion:       \t2\nLabel:         \tbraid-disk2\nSubsystem:     \t(no subsystem)\n",
                )),
                // FSID check for new disk's mapper
                CmdRequest::BtrfsFilesystemShowTarget { target } => {
                    if self.no_btrfs_superblock {
                        Ok(RawCommandOutput {
                            cmd: format!("btrfs filesystem show {target}"),
                            stdout: String::new(),
                            stderr: "ERROR: not a valid btrfs filesystem on /dev/mapper/braid-disk2".into(),
                            exit_status: 1,
                        })
                    } else {
                        Ok(mock_ok(
                            &format!("btrfs filesystem show {target}"),
                            &format!("Label: none  uuid: {POOL_FSID}\n\tTotal devices 1 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n"),
                        ))
                    }
                }
                CmdRequest::BtrfsDeviceAdd { .. } => {
                    if self.fail_device_add {
                        Ok(RawCommandOutput {
                            cmd: "btrfs device add".into(),
                            stdout: String::new(),
                            stderr: "ERROR: unable to add device".into(),
                            exit_status: 1,
                        })
                    } else {
                        Ok(mock_ok("btrfs device add", ""))
                    }
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

    fn add_test_setup() -> (
        tempfile::TempDir,
        StatePaths,
        tempfile::TempDir,
        std::path::PathBuf,
        std::path::PathBuf,
    ) {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = membership::PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            membership::DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let pass_path = tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        (state_tmp, paths, tmp, config_path, pass_path)
    }

    #[test]
    // Intent: no journal is written when all disks are already in pool (no-op).
    //
    // Why it exists: the journal is written only after identity validation
    //   passes and before irreversible operations. When all disks are
    //   AlreadyInPool, no irreversible work is needed, so no journal is written.
    //
    // Scenario: user runs `braid add` for a disk that's already a pool member.
    //   The command succeeds as a no-op without ever writing pending-op.json.
    fn no_journal_on_noop_add() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let fs = AddMockFs(vec![
            "/dev/disk/by-id/virtio-disk2".into(),
            "/dev/mapper/braid-disk2".into(),
        ]);
        let runner = AddTestRunner {
            disk_in_pool: true,
            fail_device_add: false,
            no_btrfs_superblock: false,
        };

        cmd_add(
            &runner,
            &fs,
            &config_path,
            &["disk2=/dev/disk/by-id/virtio-disk2".into()],
            false,
            true,
            false,
            Some(pass_path.as_path()),
            None,
            ProgressOutput::Off,
            &paths,
        )
        .expect("no-op add should succeed");

        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "pending-op.json should be cleared after no-op add"
        );
    }

    #[test]
    // Intent: pending-op.json survives when btrfs device add fails.
    //
    // Why it exists: JournalGuard previously cleared the journal on any exit,
    //   including error returns. A failed btrfs device add would leave pool.json
    //   missing the new disk entry with no recovery path.
    //
    // Scenario: user adds a LUKS-labeled disk whose FSID matches but isn't in
    //   the pool yet. btrfs device add fails. Journal must persist for recovery.
    fn journal_survives_btrfs_device_add_failure() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let fs = AddMockFs(vec![
            "/dev/disk/by-id/virtio-disk2".into(),
            "/dev/mapper/braid-disk2".into(),
        ]);
        let runner = AddTestRunner {
            disk_in_pool: false,
            fail_device_add: true,
            no_btrfs_superblock: false,
        };

        let result = cmd_add(
            &runner,
            &fs,
            &config_path,
            &["disk2=/dev/disk/by-id/virtio-disk2".into()],
            false,
            true,
            false,
            Some(pass_path.as_path()),
            None,
            ProgressOutput::Off,
            &paths,
        );

        assert!(
            result.is_err(),
            "add should fail when btrfs device add fails"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
    }

    #[test]
    // Intent: no journal is written when a PresentLuks disk fails identity
    //   validation (BraidLabeledNoBtrfs).
    //
    // Why it exists: the journal was previously written before identity checks,
    //   so a BraidLabeledNoBtrfs refusal left a stale pending-op.json that
    //   blocked all subsequent commands. The fix moves journal write to after
    //   identity validation completes.
    //
    // Scenario: user adds a braid-labeled disk that has no btrfs superblock
    //   (ambiguous identity). The command fails validation. No irreversible
    //   operation happened, so no journal should exist.
    fn no_journal_on_identity_failure() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let fs = AddMockFs(vec![
            "/dev/disk/by-id/virtio-disk2".into(),
            "/dev/mapper/braid-disk2".into(),
        ]);
        let runner = AddTestRunner {
            disk_in_pool: false,
            fail_device_add: false,
            no_btrfs_superblock: true,
        };

        let result = cmd_add(
            &runner,
            &fs,
            &config_path,
            &["disk2=/dev/disk/by-id/virtio-disk2".into()],
            false,
            true,
            false,
            Some(pass_path.as_path()),
            None,
            ProgressOutput::Off,
            &paths,
        );

        assert!(result.is_err(), "add should fail on BraidLabeledNoBtrfs");
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal should exist after pre-mutation identity failure"
        );
    }

    #[test]
    // Intent: unmounted bootstrap rejects braid-labeled PresentLuks disks.
    //
    // Why it exists: the guard at line 367 ("bootstrap only accepts fresh
    //   disks") is the invariant that makes the bootstrap path unreachable for
    //   PresentLuks disks. This test locks that invariant so a future refactor
    //   can't silently remove it.
    //
    // Scenario: user has a braid-labeled LUKS disk and no mounted pool. Running
    //   `braid add` must refuse rather than attempting bootstrap with an
    //   existing encrypted disk.
    fn bootstrap_rejects_braid_labeled_luks_disk() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksUUID /dev/disk/by-id/virtio-disk2".into(),
                    stdout: "a1b2c3d4-e5f6-7890-abcd-ef1234567890\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksDump /dev/disk/by-id/virtio-disk2".into(),
                    stdout: "LUKS header information\nVersion:       \t2\nLabel:         \tbraid-disk2\nSubsystem:     \t(no subsystem)\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                RawCommandOutput {
                    cmd: "findmnt --json --mountpoint /mnt/storage".into(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 1,
                },
            );

        let result = cmd_add(
            &runner,
            &fs,
            &config_path,
            &["disk2=/dev/disk/by-id/virtio-disk2".into()],
            false,
            true,
            false,
            Some(pass_path.as_path()),
            None,
            ProgressOutput::Off,
            &paths,
        );

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("bootstrap only accepts fresh disks"),
            "expected bootstrap rejection, got: {err}"
        );
    }
}
