use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::config::{config_read, mapper_name};
use crate::luks::{
    backup_luks_header, device_has_btrfs_superblock, ensure_luks_open, luks_format,
    luks_opts_from_env, read_passphrase, verify_passphrase,
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

    if let (Some(device_fsid), Some(pool_fsid)) = (&show.uuid, &pool.fsid) {
        if device_fsid != pool_fsid {
            return Ok(AddLuksIdentity::BraidLabeledForeignPool);
        }
    }

    if pool.devices.iter().any(|d| d.mapper == *mapper) {
        return Ok(AddLuksIdentity::BraidLabeledAlreadyInPool);
    }

    Ok(AddLuksIdentity::BraidLabeledRecoverable)
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
    let mut pool_membership = match membership::load_membership(paths) {
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

    // Pre-commit persist: save membership after all reversible checks pass,
    // but before the first irreversible disk operation (LUKS format).
    for (name, by_id) in &parsed {
        pool_membership.disks.insert(
            name.clone(),
            membership::DiskMember::from_by_id(by_id.clone()),
        );
    }
    membership::save_membership(&pool_membership, paths)?;

    // LUKS phase — for each disk: format/open as needed. Track which need pool add.
    // Guard closes any mappers we opened if the LUKS phase fails partway through.
    let mut luks_guard = LuksCleanupGuard::new(runner);
    let mut needs_pool_add: Vec<usize> = Vec::new();

    for (i, p) in probed.iter().enumerate() {
        let name = names[i];
        let by_id = by_ids[i];
        let mn = mapper_name(name);

        match &p.state {
            ConfigDiskState::Absent => unreachable!("already checked above"),
            ConfigDiskState::PresentNotLuks => {
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
            ConfigDiskState::PresentLuks { mapper_open, .. } => {
                // Identity check: read LUKS label (works on raw device, no mapper needed)
                let label = read_luks_label(runner, &by_id.0)?;
                let expected_label = format!("braid-{name}");

                if label.as_deref() != Some(expected_label.as_str()) {
                    return Err(AddError::Validation(format!(
                        "disk '{}' ({}) is already a LUKS container but is not labeled as {}; \
                         braid will not adopt a non-braid encrypted device",
                        name, by_id, expected_label,
                    )));
                }

                // Braid-labeled — pool must be mounted to verify FSID
                if !pool.mounted {
                    return Err(AddError::Validation(format!(
                        "disk '{}' is braid-labeled but no mounted pool exists to verify identity; \
                         bootstrap only accepts fresh disks",
                        name,
                    )));
                }

                // Open mapper if closed (now we know it's braid-labeled + pool is up)
                if !mapper_open {
                    ensure_luks_open(runner, fs, name, by_id, &passphrase)?;
                    luks_guard.track(mn.0.clone());
                    eprintln!("LUKS opened: {} → {}", by_id, mn);
                }

                // Full FSID-based identity classification
                match classify_braid_disk_fsid(runner, name, &mn, &pool)? {
                    AddLuksIdentity::BraidLabeledNoBtrfs => {
                        return Err(AddError::Validation(format!(
                            "disk '{}' is braid-labeled but contains no btrfs superblock; \
                             identity is ambiguous, so braid will not re-add it automatically. \
                             Wipe the disk and add it again as fresh.",
                            name,
                        )));
                    }
                    AddLuksIdentity::BraidLabeledForeignPool => {
                        return Err(AddError::Validation(format!(
                            "disk '{}' is a braid-managed device from a different btrfs filesystem; \
                             braid will not merge foreign pools",
                            name,
                        )));
                    }
                    AddLuksIdentity::BraidLabeledAlreadyInPool => {
                        // No-op — already in pool
                        continue;
                    }
                    AddLuksIdentity::BraidLabeledRecoverable => {
                        eprintln!(
                            "note: braid-labeled disk '{}' verified as pool member. \
                             Completing recovery add.",
                            name
                        );
                        needs_pool_add.push(i);
                    }
                    // These cases are handled above before calling classify_braid_disk_fsid
                    AddLuksIdentity::NonBraid | AddLuksIdentity::BraidLabeledNoPool => {
                        unreachable!("handled before FSID classification")
                    }
                }
            }
        }
    }

    // LUKS phase complete — mappers are committed for pool operations.
    luks_guard.disarm();

    if needs_pool_add.is_empty() {
        let label = if names.len() == 1 {
            names[0].to_owned()
        } else {
            names.iter().copied().collect::<Vec<_>>().join(", ")
        };
        eprintln!("Nothing to do — {} already in pool.", label);
        return Ok(());
    }

    // Pool phase
    let mapper_paths: Vec<String> = needs_pool_add
        .iter()
        .map(|&i| format!("/dev/mapper/{}", mapper_name(names[i]).0))
        .collect();

    if !pool.mounted {
        if mapper_paths.len() >= 2 {
            // Check if ALL target mappers are fresh (no btrfs superblock)
            let mut any_has_superblock = false;
            for mp in &mapper_paths {
                if device_has_btrfs_superblock(runner, mp)? {
                    any_has_superblock = true;
                    break;
                }
            }

            if any_has_superblock {
                return Err(AddError::Validation(
                    "pool is not mounted but some target devices have existing btrfs data. \
                     Run `braid unlock` first to bring the pool online, then add disks."
                        .into(),
                ));
            }

            // All fresh — bootstrap with mkfs.btrfs RAID1
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

    // Best-effort: enrich pool.json with live metadata (luks_uuid, devid).
    if let Ok(pool_after) = probe_pool(runner, config.mount_point().as_str()) {
        membership::refresh_pool_metadata(&pool_after, paths);
    }

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
                // Dry-run identity check: read LUKS label (no side effects)
                let label = read_luks_label(runner, &by_id.0)?;
                let expected_label = format!("braid-{name}");

                if label.as_deref() != Some(expected_label.as_str()) {
                    return Err(AddError::Validation(format!(
                        "disk '{}' ({}) is already a LUKS container but is not labeled as {}; \
                         braid will not adopt a non-braid encrypted device",
                        name, by_id, expected_label,
                    )));
                }

                // Braid-labeled: check pool
                if !pool.mounted {
                    return Err(AddError::Validation(format!(
                        "disk '{}' is braid-labeled but no mounted pool exists to verify identity; \
                         bootstrap only accepts fresh disks",
                        name,
                    )));
                }

                if *mapper_open {
                    // Mapper is open — full classification without side effects
                    match classify_braid_disk_fsid(runner, name, &mn, pool)? {
                        AddLuksIdentity::BraidLabeledNoBtrfs => {
                            return Err(AddError::Validation(format!(
                                "disk '{}' is braid-labeled but contains no btrfs superblock; \
                                 blocked: identity is ambiguous. \
                                 Wipe the disk to re-add it as fresh.",
                                name,
                            )));
                        }
                        AddLuksIdentity::BraidLabeledForeignPool => {
                            return Err(AddError::Validation(format!(
                                "disk '{}' is a braid-managed device from a different btrfs \
                                 filesystem; braid will not merge foreign pools",
                                name,
                            )));
                        }
                        AddLuksIdentity::BraidLabeledAlreadyInPool => {
                            continue;
                        }
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
                        AddLuksIdentity::NonBraid | AddLuksIdentity::BraidLabeledNoPool => {
                            unreachable!("handled above")
                        }
                    }
                } else {
                    // Mapper closed — can't open in dry-run, defer full verification
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
}
