use crate::cmd::{CmdError, CmdRequest, CommandRunner, Step};
use crate::config::{config_read, mapper_name};
use crate::confirm;
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal;
use crate::luks::{
    backup_luks_header, ensure_luks_open, luks_format, luks_opts_from_env,
    pool_has_keyfile_enrollment, read_passphrase_with, verify_passphrase, PassphraseReader,
    VerifyOutcome,
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

pub struct AddParams<'a> {
    pub config_path: &'a Path,
    pub disk_specs: &'a [String],
    pub dry_run: bool,
    pub yes: bool,
    pub passphrase_stdin: bool,
    pub passphrase_file: Option<&'a Path>,
    pub enroll_key_file: Option<&'a Path>,
    pub progress: ProgressOutput,
    pub paths: &'a StatePaths,
    /// Seam for acquiring a logind sleep inhibitor before the irreversible
    /// portion of the add. Production passes `&RealSleepInhibitor`;
    /// unit tests pass `&RecordingInhibitor` to avoid spawning subprocesses.
    pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
    /// Seam for reading a LUKS passphrase from the TTY. Production
    /// passes `&RpasswordTty`; tests pass a scripted reader so the
    /// bootstrap-confirm path is observable at the `cmd_add` layer.
    pub passphrase_reader: &'a dyn PassphraseReader,
}

pub fn cmd_add<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &AddParams<'_>,
) -> Result<(), AddError> {
    preflight::check_no_pending_operation(params.paths).map_err(AddError::Validation)?;

    let config = config_read(params.config_path)?;

    // Parse disk specs: name=by_id
    let parsed: Vec<(String, ByIdPath)> = params
        .disk_specs
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

    // Reject duplicate by_id values within the same invocation. Runs before
    // any probing, confirmation, passphrase read, inhibitor acquisition, or
    // journal write so a typo like `d1=/dev/disk/by-id/X d2=/dev/disk/by-id/X`
    // fails fast with no side effects. Compares raw strings only -- symlink
    // alias resolution is out of scope.
    {
        let mut seen = std::collections::HashSet::new();
        for by_id in &by_ids {
            if !seen.insert(by_id.0.as_str()) {
                return Err(AddError::Validation(format!(
                    "duplicate by_id: '{}'",
                    by_id.0
                )));
            }
        }
    }

    // Load existing membership (or empty if first add)
    let pool_membership = match membership::load_membership(params.paths) {
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
    let pool = match probe_pool(runner, config.mount_point()) {
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
        let fsid = pool.fsid.as_deref().expect("mounted pool must have FSID");
        preflight::require_mutation_preflight(runner, fs, fsid, config.mount_point())
            .map_err(AddError::Validation)?;
    }
    preflight::check_ups_not_on_battery(runner, config.ups().map(|u| u.name.as_str()), "add")
        .map_err(AddError::Validation)?;
    if pool.missing_count > 0 {
        eprintln!(
            "warning: pool has {} missing device{}. \
             Consider repairing with `braid replace --missing-id <devid>` first. \
             Use `braid status` to see device IDs.",
            pool.missing_count,
            if pool.missing_count == 1 { "" } else { "s" }
        );
    }

    let any_needs_format = probed
        .iter()
        .any(|p| matches!(p.state, ConfigDiskState::PresentNotLuks));

    if any_needs_format
        && params.enroll_key_file.is_none()
        && pool_has_keyfile_enrollment(runner, &pool.devices)
    {
        eprintln!(
            "WARNING: Existing pool drives have a keyfile (keyslot-1) for auto-unlock, \
             but the new drive will not.\n  \
             Passphrase unlock still works, but the keyfile won't unlock the new drive \
             until it's enrolled.\n  \
             Fix: re-run with --enroll <dir>, or run `braid enroll <dir>` afterward.\n"
        );
    }

    // Compile steps for dry-run display
    let steps = compile_add_steps_multi(
        runner,
        &AddStepsInput {
            names: &names,
            by_ids: &by_ids,
            probed: &probed,
            pool: &pool,
            mount_point: config.mount_point(),
            paths: params.paths,
            enroll_key_file: params.enroll_key_file,
        },
    )?;

    if params.dry_run {
        Step::print_dry_run(&steps);
        return Ok(());
    }

    if steps.is_empty() {
        let label = if names.len() == 1 {
            names[0].to_owned()
        } else {
            names.to_vec().join(", ")
        };
        eprintln!("Nothing to do -- {} already in pool.", label);
        return Ok(());
    }

    // Confirmation — show device details for sanity-check
    if !params.yes {
        let confirm_disks: Vec<AddConfirmDisk> = names
            .iter()
            .zip(by_ids.iter())
            .zip(probed.iter())
            .map(|((name, by_id), p)| {
                let hw = confirm::query_disk_hw_info(runner, &by_id.0);
                AddConfirmDisk {
                    name,
                    by_id: &by_id.0,
                    hw,
                    needs_luks_format: matches!(p.state, ConfigDiskState::PresentNotLuks),
                }
            })
            .collect();
        eprintln!("{}", format_add_confirm(&confirm_disks));
        confirm::confirm_yes().map_err(AddError::Validation)?;
    }

    // Confirm the new passphrase iff this add will `luks_format` without
    // a live keyslot to verify against. Otherwise a typo either (a) gets
    // caught by the `verify_passphrase` block below against the live pool
    // member, or (b) lands on a no-format path where subsequent open/
    // identity validation aborts -- both recoverable. Bootstrap and
    // fresh-disk-into-unassembled-pool have no such safety net, so a typo
    // becomes the canonical pool passphrase; see plans/wip for details.
    let confirm_new = any_needs_format && pool.devices.is_empty();
    let passphrase = read_passphrase_with(
        params.passphrase_file,
        params.passphrase_stdin,
        confirm_new,
        params.passphrase_reader,
    )?;

    // Verify passphrase against existing pool member (once)
    if any_needs_format
        && let Some(existing) = pool.devices.first() {
            let status_raw = runner.run(&crate::cmd::CmdRequest::CryptsetupStatus {
                mapper: existing.mapper.0.clone(),
            })?;
            let status = crate::parse::parse_cryptsetup_status(&status_raw)?;
            if let Some(underlying) = status.device {
                match verify_passphrase(runner, &underlying, &passphrase)? {
                    VerifyOutcome::Authenticated => {}
                    VerifyOutcome::Rejected => {
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
            names.to_vec().join(", ")
        };
        eprintln!("Nothing to do -- {} already in pool.", label);
        return Ok(());
    }

    // Hold a logind sleep inhibitor for the rest of the add operation —
    // covers Pass-2 LUKS format/open of fresh disks, the bootstrap-or-add
    // pool phase, and the conditional pool_balance_raid1 that converts
    // single-profile data to RAID1 when the post-add pool has ≥2 devices.
    // The balance is the long-running phase; suspending mid-balance
    // interrupts the conversion and leaves new data unprotected.
    //
    // Acquired here, AFTER all interactive/reversible work (confirmation,
    // passphrase read+verify, PresentLuks identity checks) and BEFORE
    // journal::write_journal, so that:
    //   - operator-idle prompts do not block suspend
    //   - a logind failure aborts cleanly without stranding pending-op.json
    //     and forcing the user into recovery mode for an environmental error.
    let _sleep_inhibitor_guard = params
        .sleep_inhibitor
        .acquire("adding disk(s) to pool")
        .map_err(|e| {
            AddError::Validation(format!(
                "could not acquire sleep inhibitor (is logind running?): {e}"
            ))
        })?;

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
    journal::write_journal(params.paths, &journal)
        .map_err(|e| AddError::Validation(e.to_string()))?;

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

        let backup_path = backup_luks_header(runner, &by_id.0, &mn.0, params.paths)?;
        eprintln!("LUKS header backed up: {}", backup_path.display());

        ensure_luks_open(runner, fs, name, by_id, &passphrase)?;
        luks_guard.track(mn.0.clone());
        eprintln!("LUKS opened: {} → {}", by_id, mn);

        if let Some(kf) = params.enroll_key_file {
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
            pool_bootstrap_mount_raid1(runner, &mapper_paths, config.mount_point())?;
            eprintln!(
                "Pool created (RAID1) and mounted at {}",
                config.mount_point()
            );
        } else {
            // Single disk bootstrap
            pool_bootstrap_mount(runner, &mapper_paths[0], config.mount_point())?;
            eprintln!("Pool created and mounted at {}", config.mount_point());
        }
    } else {
        // Add each to existing pool
        for mp in &mapper_paths {
            pool_add_device(runner, mp, config.mount_point())?;
            eprintln!("Device added to pool: {}", mp);
        }

        // Balance to RAID1 if total >= 2
        let total_after = pool.devices.len() + mapper_paths.len();
        if total_after >= 2 {
            eprintln!("Balancing to RAID1...");
            pool_balance_raid1(runner, config.mount_point(), params.progress)?;
            eprintln!("Balance complete.");
        }
    }

    // Post-commit persist: write pool.json only after all disk ops succeed.
    // Enrich with live metadata (luks_uuid, devid) from pool probe.
    let mut final_membership = journal.target_membership.clone();
    if let Ok(pool_after) = probe_pool(runner, config.mount_point()) {
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
    membership::save_membership(&final_membership, params.paths)?;
    journal::clear_journal(params.paths).map_err(|e| AddError::Validation(e.to_string()))?;

    let label = if names.len() == 1 {
        format!("{} is", names[0])
    } else {
        format!("{} are", names.join(", "))
    };
    eprintln!("Done. {} now part of the pool.", label);
    Ok(())
}

struct AddStepsInput<'a> {
    names: &'a [&'a str],
    by_ids: &'a [&'a ByIdPath],
    probed: &'a [ConfigDisk],
    pool: &'a PoolState,
    mount_point: &'a MountPoint,
    paths: &'a StatePaths,
    enroll_key_file: Option<&'a Path>,
}

fn compile_add_steps_multi<R: CommandRunner>(
    runner: &R,
    input: &AddStepsInput<'_>,
) -> Result<Vec<Step>, AddError> {
    let luks_extra_opts = luks_opts_from_env();
    let mut steps = Vec::new();
    let mut needs_pool_add = 0usize;

    for (i, p) in input.probed.iter().enumerate() {
        let name = input.names[i];
        let by_id = input.by_ids[i];
        let mn = mapper_name(name);

        match &p.state {
            ConfigDiskState::Absent => {
                return Err(AddError::Validation(format!(
                    "disk '{}' ({}) is not present. Is it plugged in?",
                    name, by_id
                )));
            }
            ConfigDiskState::PresentNotLuks => {
                let mut extra_opts = luks_extra_opts.clone();
                extra_opts.push("--label".into());
                extra_opts.push(format!("braid-{name}"));
                steps.push(Step {
                    risk: "destructive",
                    description: format!("LUKS format {}", by_id),
                    commands: vec![CmdRequest::CryptsetupLuksFormat {
                        device: by_id.0.clone(),
                        extra_opts,
                    }],
                });
                let backup_path = input
                    .paths
                    .luks_headers_dir()
                    .join(format!("{}.luksheader", mn.0));
                steps.push(Step {
                    risk: "safe",
                    description: format!("LUKS header backup → {}", backup_path.display()),
                    commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
                        device: by_id.0.clone(),
                        backup_path: backup_path.display().to_string(),
                    }],
                });
                steps.push(Step {
                    risk: "safe",
                    description: format!("LUKS open → {}", mn),
                    commands: vec![CmdRequest::CryptsetupLuksOpen {
                        device: by_id.0.clone(),
                        mapper: mn.0.clone(),
                    }],
                });
                if let Some(kf) = input.enroll_key_file {
                    steps.push(Step {
                        risk: "safe",
                        description: format!("enroll keyfile → LUKS slot 1 on {}", by_id),
                        commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
                            device: by_id.0.clone(),
                            key_file_path: kf.display().to_string(),
                        }],
                    });
                }
                needs_pool_add += 1;
            }
            ConfigDiskState::PresentLuks { mapper_open, .. } => {
                // Preconditions always checked — no mapper required.
                validate_braid_preconditions(runner, name, &by_id.0, input.pool)?;

                if *mapper_open {
                    // Mapper is open — full classification without side effects
                    let identity = classify_braid_disk_fsid(runner, name, &mn, input.pool)?;
                    if let Some(err) = identity_to_error(&identity, name) {
                        return Err(err);
                    }
                    match identity {
                        AddLuksIdentity::BraidLabeledAlreadyInPool => continue,
                        AddLuksIdentity::BraidLabeledRecoverable => {
                            let mapper_path = format!("/dev/mapper/{}", mn);
                            steps.push(Step {
                                risk: "safe",
                                description: format!(
                                    "btrfs device add /dev/mapper/{} {} (recovery)",
                                    mn, input.mount_point
                                ),
                                commands: vec![CmdRequest::BtrfsDeviceAdd {
                                    device: mapper_path,
                                    mount_point: input.mount_point.clone(),
                                }],
                            });
                            needs_pool_add += 1;
                        }
                        _ => unreachable!("error variants handled by identity_to_error above"),
                    }
                } else {
                    // Mapper closed — FSID verification deferred to execution time.
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "LUKS open + identity verification at execution time → {}",
                            mn
                        ),
                        commands: vec![CmdRequest::CryptsetupLuksOpen {
                            device: by_id.0.clone(),
                            mapper: mn.0.clone(),
                        }],
                    });
                    needs_pool_add += 1;
                }
            }
        }
    }

    if needs_pool_add == 0 {
        return Ok(vec![]);
    }

    if !input.pool.mounted {
        if needs_pool_add >= 2 {
            let mapper_list: Vec<String> = input
                .names
                .iter()
                .map(|n| format!("/dev/mapper/{}", mapper_name(n).0))
                .collect();
            steps.push(Step {
                risk: "safe",
                description: format!("mkfs.btrfs RAID1 {}", mapper_list.join(" ")),
                commands: vec![CmdRequest::MkfsBtrfsRaid1 {
                    devices: mapper_list.clone(),
                }],
            });
            // Mount uses first device
            steps.push(Step {
                risk: "safe",
                description: format!("mount → {}", input.mount_point),
                commands: vec![CmdRequest::Mount {
                    device: mapper_list[0].clone(),
                    mount_point: input.mount_point.clone(),
                }],
            });
        } else {
            // Single disk — find the one that needs pool add
            for (i, p) in input.probed.iter().enumerate() {
                let mn = mapper_name(input.names[i]);
                let skip = matches!(&p.state, ConfigDiskState::PresentLuks { mapper_open, .. } if *mapper_open && input.pool.devices.iter().any(|d| d.mapper == mn));
                if !skip {
                    let mapper_path = format!("/dev/mapper/{}", mn);
                    steps.push(Step {
                        risk: "safe",
                        description: format!("mkfs.btrfs /dev/mapper/{}", mn),
                        commands: vec![CmdRequest::MkfsBtrfs {
                            device: mapper_path.clone(),
                        }],
                    });
                    steps.push(Step {
                        risk: "safe",
                        description: format!("mount → {}", input.mount_point),
                        commands: vec![CmdRequest::Mount {
                            device: mapper_path,
                            mount_point: input.mount_point.clone(),
                        }],
                    });
                    break;
                }
            }
        }
    } else {
        for (i, p) in input.probed.iter().enumerate() {
            let mn = mapper_name(input.names[i]);
            // PresentNotLuks disks still need the device-add step
            if matches!(&p.state, ConfigDiskState::PresentNotLuks) {
                let mapper_path = format!("/dev/mapper/{}", mn);
                steps.push(Step {
                    risk: "safe",
                    description: format!(
                        "btrfs device add /dev/mapper/{} {}",
                        mn, input.mount_point
                    ),
                    commands: vec![CmdRequest::BtrfsDeviceAdd {
                        device: mapper_path,
                        mount_point: input.mount_point.clone(),
                    }],
                });
            }
            // PresentLuks recovery steps already added above
        }
        let total_after = input.pool.devices.len() + needs_pool_add;
        if total_after >= 2 {
            steps.push(Step {
                risk: "long",
                description: "btrfs balance to RAID1".into(),
                commands: vec![CmdRequest::BtrfsBalanceRaid1 {
                    mount_point: input.mount_point.clone(),
                }],
            });
        }
    }

    Ok(steps)
}

struct AddConfirmDisk<'a> {
    name: &'a str,
    by_id: &'a str,
    hw: confirm::DiskHwInfo,
    needs_luks_format: bool,
}

fn format_add_confirm(disks: &[AddConfirmDisk]) -> String {
    let mut msg = "Add to pool:\n".to_string();
    for d in disks {
        msg.push_str(&format!("  {}  {}\n", d.name, d.by_id));
        if let Some(hw_line) = confirm::format_hw_info_line(&d.hw) {
            msg.push_str(&format!("  {:width$}{}\n", "", hw_line, width = d.name.len() + 2));
        }
        if d.needs_luks_format {
            msg.push_str(&format!(
                "  {:width$}Will be LUKS-formatted (existing data will be inaccessible)\n",
                "",
                width = d.name.len() + 2
            ));
        }
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::luks::{RpasswordTty, ScriptedPassphraseReader};

    fn test_paths() -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        (tmp, paths)
    }

    #[test]
    fn add_confirm_single_disk_with_luks_format() {
        let disks = vec![AddConfirmDisk {
            name: "data1",
            by_id: "/dev/disk/by-id/usb-WD_1234",
            hw: confirm::DiskHwInfo {
                model: Some("WD Elements".into()),
                serial: Some("1234ABCD".into()),
                size: Some(12_000_138_625_024),
            },
            needs_luks_format: true,
        }];
        let msg = format_add_confirm(&disks);
        assert!(msg.contains("Add to pool:"));
        assert!(msg.contains("data1"));
        assert!(msg.contains("/dev/disk/by-id/usb-WD_1234"));
        assert!(msg.contains("WD Elements"));
        assert!(msg.contains("TiB"));
        assert!(msg.contains("serial 1234ABCD"));
        assert!(msg.contains("LUKS-formatted"));
        assert!(msg.contains("inaccessible"));
    }

    #[test]
    fn add_confirm_multi_disk() {
        let disks = vec![
            AddConfirmDisk {
                name: "toshiba",
                by_id: "/dev/disk/by-id/ata-Toshiba",
                hw: confirm::DiskHwInfo {
                    model: Some("Toshiba MN07".into()),
                    serial: None,
                    size: Some(12_000_000_000_000),
                },
                needs_luks_format: true,
            },
            AddConfirmDisk {
                name: "ironwolf",
                by_id: "/dev/disk/by-id/ata-Ironwolf",
                hw: confirm::DiskHwInfo::default(),
                needs_luks_format: false,
            },
        ];
        let msg = format_add_confirm(&disks);
        assert!(msg.contains("toshiba"), "should mention first disk");
        assert!(msg.contains("ironwolf"), "should mention second disk");
        assert!(msg.contains("Toshiba MN07"), "should show model");
        // ironwolf has no hw info and doesn't need format — minimal entry
        assert!(
            msg.matches("LUKS-formatted").count() == 1,
            "only toshiba needs format"
        );
    }

    #[test]
    fn add_confirm_already_luks_disk() {
        let disks = vec![AddConfirmDisk {
            name: "data1",
            by_id: "/dev/disk/by-id/usb-WD_1234",
            hw: confirm::DiskHwInfo {
                model: Some("WD Elements".into()),
                serial: None,
                size: Some(1_000_000_000_000),
            },
            needs_luks_format: false,
        }];
        let msg = format_add_confirm(&disks);
        assert!(msg.contains("Add to pool:"));
        assert!(msg.contains("data1"));
        assert!(!msg.contains("LUKS-formatted"), "no format warning for existing LUKS");
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
            fn read_to_string(&self, _path: &str) -> Result<String, std::io::Error> {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
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
        let (_state_dir, sp) = test_paths();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &[
                    "d1=/dev/disk/by-id/ata-D1".into(),
                    "d1=/dev/disk/by-id/ata-D1".into(),
                ],
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: None,
                progress: ProgressOutput::Off,
                paths: &sp,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RpasswordTty,
            },
        );
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("duplicate disk name"),
            "expected duplicate error, got: {err}"
        );
        // Validation failure (duplicate disk name) must NOT acquire the inhibitor.
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "validation failure must NOT acquire the sleep inhibitor"
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
            null_underlying: vec![],
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
            null_underlying: vec![],
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
            null_underlying: vec![],
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
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
            },
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
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
            },
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
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
            },
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
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
            },
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
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
            },
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
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
            },
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
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path.ends_with("/exclusive_operation") {
                Ok("none\n".to_owned())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
            }
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
                CmdRequest::CryptsetupStatus { mapper } => {
                    let underlying = match mapper.as_str() {
                        "braid-disk1" => "/dev/vdb",
                        "braid-disk2" => "/dev/vdc",
                        _ => "/dev/vdz",
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!("{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {underlying}\n  mode:    read/write\n"),
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                            "11111111-1111-1111-1111-111111111111"
                        }
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
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk2=/dev/disk/by-id/virtio-disk2".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RpasswordTty,
            },
        )
        .expect("no-op add should succeed");

        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "pending-op.json should be cleared after no-op add"
        );
        // No-op add returns before journal::write_journal — the inhibitor seam
        // sits AFTER the no-op early-return at line ~466, so it must NOT have
        // been acquired.
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "no-op add must NOT acquire the sleep inhibitor — the inhibitor seam sits after the early-return"
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
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk2=/dev/disk/by-id/virtio-disk2".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RpasswordTty,
            },
        );

        assert!(
            result.is_err(),
            "add should fail when btrfs device add fails"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        // The journal exists, which proves we got past journal::write_journal,
        // which proves the inhibitor was acquired exactly once on the way in.
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
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
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk2=/dev/disk/by-id/virtio-disk2".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RpasswordTty,
            },
        );

        assert!(result.is_err(), "add should fail on BraidLabeledNoBtrfs");
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal should exist after pre-mutation identity failure"
        );
        // Identity validation failure happens BEFORE the inhibitor seam, so
        // the inhibitor must NOT be acquired. This is the same property as
        // "no journal" — both seams are gated on identity validation passing.
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "pre-mutation identity failure must NOT acquire the sleep inhibitor"
        );
    }

    #[test]
    // Intent: two disk specs with different names pointing at the same
    //   /dev/disk/by-id/... are rejected before any probing, confirmation,
    //   passphrase read, inhibitor acquisition, or journal write.
    //
    // Why it exists: cmd_add already dedups disk names (see
    //   duplicate_name_rejected), but a typo of the form
    //   `d1=/dev/disk/by-id/X d2=/dev/disk/by-id/X` slipped past validation
    //   and failed at execution time -- after the journal was written and
    //   the inhibitor was held. Fast-fail at the validation phase keeps the
    //   state dir clean on operator typo.
    //
    // Scenario: operator pastes the same by_id twice with two different
    //   logical names and runs a non-dry-run `braid add`. cmd_add must
    //   reject before any runner.run() call (empty MockRunner would fail
    //   with MissingMock if anything tried to execute).
    fn duplicate_by_id_rejected() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        // fs lookups never happen — dedup fires before any filesystem probe.
        let fs = AddMockFs(vec![]);
        // Empty MockRunner: any runner.run() would return MissingMock, which
        // would surface in the returned error rather than a "duplicate by_id"
        // message. Asserting the duplicate error text indirectly pins
        // "nothing executed".
        let runner = MockRunner::default();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &[
                    "d1=/dev/disk/by-id/virtio-disk2".into(),
                    "d2=/dev/disk/by-id/virtio-disk2".into(),
                ],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                // Only here for robustness against future code reordering.
                // Dedup runs before read_passphrase, so this is never consulted.
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RpasswordTty,
            },
        );

        let err = result.expect_err("duplicate by_id must be rejected").to_string();
        assert!(
            err.contains("duplicate by_id"),
            "expected duplicate by_id error, got: {err}"
        );
        assert!(
            err.contains("/dev/disk/by-id/virtio-disk2"),
            "error must name the offending by_id, got: {err}"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal should exist after pre-probe validation failure"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "validation failure must NOT acquire the sleep inhibitor"
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
            .with_mapper_closed("braid-disk2")
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

        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk2=/dev/disk/by-id/virtio-disk2".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RpasswordTty,
            },
        );

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("bootstrap only accepts fresh disks"),
            "expected bootstrap rejection, got: {err}"
        );
        // Validation failure (bootstrap rejection) happens BEFORE the inhibitor
        // seam, so the inhibitor must NOT be acquired.
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "bootstrap rejection must NOT acquire the sleep inhibitor"
        );
    }

    #[test]
    // Intent: dry-run for fresh single-disk bootstrap shows LUKS init + mkfs + mount commands.
    // Why: verifies header backup and mount commands appear, with correct CmdRequests.
    // Scenario: first disk added to an empty pool (no pool mounted yet).
    fn dry_run_render_fresh_single_disk_bootstrap() {
        let runner = MockRunner::default();
        let probed = vec![ConfigDisk {
            name: "disk1".to_owned(),
            by_id_path: ByIdPath("/dev/disk/by-id/disk1".to_owned()),
            state: ConfigDiskState::PresentNotLuks,
        }];
        let pool = pool_unmounted();

        let steps = compile_add_steps_multi(
            &runner,
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
            },
        )
        .unwrap();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // Steps: LUKS format, header backup, LUKS open, mkfs, mount = 5 steps × 2 lines = 10
        assert_eq!(lines.len(), 10, "expected 10 lines, got:\n{output}");

        // LUKS format
        assert!(lines[0].contains("[destructive]"));
        assert!(lines[0].contains("LUKS format"));
        assert!(lines[1].contains("$ cryptsetup luksFormat"));
        assert!(lines[1].contains("--label braid-disk1"));

        // Header backup
        assert!(lines[2].contains("LUKS header backup"));
        assert!(lines[3].contains("$ cryptsetup luksHeaderBackup"));

        // LUKS open
        assert!(lines[4].contains("LUKS open"));
        assert!(lines[5].contains("$ cryptsetup open --type luks"));

        // mkfs
        assert!(lines[6].contains("mkfs.btrfs"));
        assert!(lines[7].contains("$ mkfs.btrfs"));

        // mount
        assert!(lines[8].contains("mount"));
        assert!(lines[9].contains("$ mount"));
        assert!(lines[9].contains("/mnt/storage"));
    }

    #[test]
    // Intent: dry-run for adding to existing pool shows device add + balance commands.
    // Why: verifies the pool-mounted path includes balance to RAID1.
    // Scenario: adding a fresh disk to a 1-disk pool (pool already mounted).
    fn dry_run_render_add_to_existing_pool_with_balance() {
        let runner = MockRunner::default();
        let probed = vec![ConfigDisk {
            name: "disk2".to_owned(),
            by_id_path: ByIdPath("/dev/disk/by-id/disk2".to_owned()),
            state: ConfigDiskState::PresentNotLuks,
        }];
        let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

        let steps = compile_add_steps_multi(
            &runner,
            &AddStepsInput {
                names: &["disk2"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk2".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
            },
        )
        .unwrap();
        let output = Step::render_dry_run(&steps);

        // Should contain: LUKS format, header backup, LUKS open, device add, balance
        assert!(output.contains("LUKS format"), "missing LUKS format step");
        assert!(
            output.contains("LUKS header backup"),
            "missing header backup step"
        );
        assert!(output.contains("LUKS open"), "missing LUKS open step");
        assert!(
            output.contains("btrfs device add"),
            "missing device add step"
        );
        assert!(
            output.contains("btrfs balance to RAID1"),
            "missing balance step"
        );
        assert!(
            output.contains("$ btrfs balance start"),
            "missing balance command"
        );
    }

    // -----------------------------------------------------------------------
    // Bootstrap-confirm regression tests (cmd_add gate matrix)
    // -----------------------------------------------------------------------
    //
    // These tests pin the `confirm_new = any_needs_format &&
    // pool.devices.first().is_none()` gate by driving `cmd_add` directly
    // through its passphrase read. The gate has two axes:
    //
    //   any_needs_format  | live_target | confirm_new
    //   ------------------+-------------+------------
    //   true              | false       | TRUE  (bootstrap, locked-pool-fresh-add)
    //   true              | true        | false (live pool: verify_passphrase catches typos)
    //   false             | *           | false (no format: gate short-circuits)
    //
    // Each cmd-level cell where any_needs_format=true has a test below.
    // The no-format cell is covered at helper level
    // (`read_passphrase_with_readers_tty_no_confirm_single_read`).

    use std::sync::Arc;

    /// Recording runner for cmd_add regression tests. Stubs just enough to
    /// reach the passphrase read and the first few format/backup commands,
    /// and logs every call so tests can assert on what ran.
    ///
    /// `CryptsetupLuksHeaderBackup` is hard-coded to fail (exit 1) so that
    /// `cmd_add` aborts deterministically after `luks_format` runs, without
    /// having to mock every downstream command (mkfs, mount, pool probe).
    type CmdLog = Arc<Mutex<Vec<CmdRequest>>>;
    type StdinLog = Arc<Mutex<Vec<(CmdRequest, Vec<u8>)>>>;

    #[derive(Clone)]
    struct AddRecordingRunner {
        log: CmdLog,
        stdin_log: StdinLog,
        pool_mounted: bool,
    }

    impl AddRecordingRunner {
        fn new(pool_mounted: bool) -> Self {
            Self {
                log: Arc::new(Mutex::new(Vec::new())),
                stdin_log: Arc::new(Mutex::new(Vec::new())),
                pool_mounted,
            }
        }
        fn log(&self) -> std::sync::MutexGuard<'_, Vec<CmdRequest>> {
            self.log.lock().unwrap()
        }
        fn stdin_log(&self) -> std::sync::MutexGuard<'_, Vec<(CmdRequest, Vec<u8>)>> {
            self.stdin_log.lock().unwrap()
        }
        fn saw_format(&self) -> bool {
            self.log()
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksFormat { .. }))
        }
        fn saw_verify(&self) -> bool {
            self.log()
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupTestPassphrase { .. }))
        }
        fn format_stdin(&self) -> Option<Vec<u8>> {
            self.stdin_log()
                .iter()
                .find(|(r, _)| matches!(r, CmdRequest::CryptsetupLuksFormat { .. }))
                .map(|(_, s)| s.clone())
        }
    }

    const LIVE_POOL_FSID: &str = "cc86845b-aec3-408e-bef5-553affc1f2b1";
    const DISK1_UUID: &str = "11111111-1111-1111-1111-111111111111";

    impl CommandRunner for AddRecordingRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            match request {
                CmdRequest::FindmntJson { mount_point } => {
                    if self.pool_mounted {
                        Ok(RawCommandOutput {
                            cmd: format!("findmnt --json --mountpoint {mount_point}"),
                            stdout: r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs","options":"rw,relatime"}]}"#.into(),
                            stderr: String::new(),
                            exit_status: 0,
                        })
                    } else {
                        // Unmounted: findmnt exits 1 with empty stderr ->
                        // parse_findmnt_json treats as empty filesystems list.
                        Ok(RawCommandOutput {
                            cmd: format!("findmnt --json --mountpoint {mount_point}"),
                            stdout: String::new(),
                            stderr: String::new(),
                            exit_status: 1,
                        })
                    }
                }
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(RawCommandOutput {
                    cmd: format!("btrfs filesystem show {mount_point}"),
                    stdout: format!(
                        "Label: none  uuid: {LIVE_POOL_FSID}\n\
                         \tTotal devices 1 FS bytes used 16.17MiB\n\
                         \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n"
                    ),
                    stderr: String::new(),
                    exit_status: 0,
                }),
                CmdRequest::CryptsetupStatus { mapper } if mapper == "braid-disk1" => {
                    Ok(RawCommandOutput {
                        cmd: format!("cryptsetup status {mapper}"),
                        stdout: format!(
                            "{mapper} is active and is in use.\n  \
                             type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                        ),
                        stderr: String::new(),
                        exit_status: 0,
                    })
                }
                CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vdb" => {
                    Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksUUID {device}"),
                        stdout: format!("{DISK1_UUID}\n"),
                        stderr: String::new(),
                        exit_status: 0,
                    })
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    // Disk under test is PresentNotLuks -- luksUUID fails.
                    Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksUUID {device}"),
                        stdout: String::new(),
                        stderr: "Device is not a valid LUKS device.\n".into(),
                        exit_status: 1,
                    })
                }
                CmdRequest::CryptsetupLuksFormat { device, .. } => Ok(RawCommandOutput {
                    cmd: format!("cryptsetup luksFormat {device}"),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                }),
                CmdRequest::CryptsetupLuksHeaderBackup { device, .. } => {
                    // Forced failure so cmd_add aborts cleanly after
                    // luks_format runs. Lets tests assert on what ran
                    // without stubbing the full mkfs/mount chain.
                    Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksHeaderBackup {device}"),
                        stdout: String::new(),
                        stderr: "mock: header backup forced to fail".into(),
                        exit_status: 1,
                    })
                }
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(RawCommandOutput {
                    cmd: "btrfs balance status".into(),
                    stdout: "No balance found on '/mnt/storage'\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                }),
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.stdin_log
                .lock()
                .unwrap()
                .push((request.clone(), stdin.to_vec()));
            // For CryptsetupTestPassphrase in the live-pool case, return
            // Authenticated (exit 0). All other stdin-carrying commands
            // fall through to `run`.
            if let CmdRequest::CryptsetupTestPassphrase { device } = request {
                self.log.lock().unwrap().push(request.clone());
                return Ok(RawCommandOutput {
                    cmd: format!("cryptsetup open --test-passphrase {device}"),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                });
            }
            self.run(request)
        }
    }

    /// Shared test setup for the confirm-regression tests. Writes a minimal
    /// config. Membership is NOT pre-seeded -- add_test_setup pre-seeds
    /// disk1; this helper does not, so the tempdir is a true fresh state.
    fn confirm_test_setup() -> (
        tempfile::TempDir,
        StatePaths,
        tempfile::TempDir,
        std::path::PathBuf,
    ) {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        (state_tmp, paths, tmp, config_path)
    }

    /*
     * Intent: bootstrap `braid add` with a mismatched confirmation prompt
     *   aborts BEFORE `cryptsetup luksFormat` runs.
     *
     * Why it exists: this is the primary regression for the fresh-format
     *   typo trap. On bootstrap there is no live keyslot to verify
     *   against, so a typoed passphrase would otherwise become the
     *   canonical pool passphrase -- unrecoverable without an external
     *   key backup. A test at the helper layer (check_passphrase_match)
     *   would still pass if the cmd_add callsite forgot to enable
     *   confirmation, so the assertion must be at the cmd_add layer.
     *
     * Scenario: user runs `braid add disk1=...` on a fresh system, types
     *   the passphrase once, fat-fingers the confirmation, and the CLI
     *   aborts without touching the LUKS header.
     */
    #[test]
    fn cmd_add_bootstrap_aborts_on_passphrase_mismatch() {
        let (_state_tmp, paths, _tmp, config_path) = confirm_test_setup();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk1".into()]);
        let runner = AddRecordingRunner::new(false);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let tty = ScriptedPassphraseReader::new(["typo-one", "typo-two"]);

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk1=/dev/disk/by-id/virtio-disk1".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: None,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &tty,
            },
        );

        match result {
            Err(AddError::Luks(crate::luks::LuksError::Validation(msg))) => assert!(
                msg.contains("do not match"),
                "expected 'do not match' in: {msg}"
            ),
            other => panic!(
                "expected AddError::Luks(Validation), got {:?}",
                other.map(|_| "Ok").unwrap_or("Err")
            ),
        }
        assert!(
            !runner.saw_format(),
            "luks_format must NOT run when confirmation mismatches"
        );
        assert_eq!(tty.remaining(), 0, "both prompts must have been read");
        // Pre-format failure: inhibitor never acquired, journal never written.
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "mismatch rejection must NOT acquire the sleep inhibitor"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal should exist after pre-format mismatch rejection"
        );
    }

    /*
     * Intent: bootstrap `braid add` with matching confirmation reaches
     *   `cryptsetup luksFormat` and passes the confirmed passphrase as
     *   stdin to the format command.
     *
     * Why it exists: the happy path for the confirm flow. Pairs with the
     *   mismatch test to pin both edges of the gate: mismatch blocks
     *   format, match allows format to run with the exact bytes the user
     *   confirmed.
     *
     * Scenario: user types the same passphrase twice; LUKS format runs
     *   with that passphrase. The test deliberately fails at header
     *   backup so we don't need to stub the full mkfs+mount chain.
     */
    #[test]
    fn cmd_add_bootstrap_proceeds_on_passphrase_match() {
        let (_state_tmp, paths, _tmp, config_path) = confirm_test_setup();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk1".into()]);
        let runner = AddRecordingRunner::new(false);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let tty = ScriptedPassphraseReader::new(["ok", "ok"]);

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk1=/dev/disk/by-id/virtio-disk1".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: None,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &tty,
            },
        );

        assert!(
            result.is_err(),
            "cmd_add must abort at forced header-backup failure"
        );
        assert!(runner.saw_format(), "luks_format must have run");
        assert_eq!(
            runner.format_stdin().as_deref(),
            Some(b"ok".as_ref()),
            "luks_format must receive the confirmed passphrase as stdin"
        );
        assert_eq!(tty.remaining(), 0, "both prompts must have been read");
    }

    /*
     * Intent: `braid add` for a fresh disk when `pool_membership.disks`
     *   is non-empty but no live verify target exists (pool not
     *   currently assembled) STILL confirms -- two reads, then format.
     *
     * Why it exists: guards against the rejected
     *   `pool_membership.disks.is_empty()` gate that would have missed
     *   this cell. The good gate reads
     *   `any_needs_format && pool.devices.first().is_none()`; when
     *   membership has a prior disk but the pool is locked/unmounted,
     *   the verify block is skipped yet we are still about to
     *   `luks_format` -- so confirmation is still required.
     *
     * Scenario: user has a pool with disk1 recorded in membership but
     *   currently locked (post-boot, pre-unlock). They add a new fresh
     *   disk2 without first unlocking. Typo protection still applies.
     */
    #[test]
    fn cmd_add_existing_membership_no_live_target_confirms() {
        let (_state_tmp, paths, _tmp, config_path, _pass_path) = add_test_setup();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddRecordingRunner::new(false);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let tty = ScriptedPassphraseReader::new(["pw", "pw", "SENTINEL"]);

        let _ = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk2=/dev/disk/by-id/virtio-disk2".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: None,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &tty,
            },
        );

        assert!(runner.saw_format(), "luks_format must have run");
        assert_eq!(
            runner.format_stdin().as_deref(),
            Some(b"pw".as_ref()),
            "luks_format must receive the confirmed passphrase"
        );
        assert_eq!(
            tty.remaining(),
            1,
            "exactly two prompts must have been read (SENTINEL remains)"
        );
    }

    /*
     * Intent: `braid add` for a fresh disk into a LIVE mounted pool
     *   reads the passphrase ONCE (no confirm), then `verify_passphrase`
     *   catches any typo against the live keyslot before format.
     *
     * Why it exists: guards against a regression that over-triggers the
     *   confirm gate (e.g., `confirm_new = any_needs_format`). The live
     *   pool already has a safety net -- a typo is rejected by the
     *   existing verify_passphrase block -- so doubling the prompt is
     *   gratuitous and the test must fail if the gate forgets to check
     *   `pool.devices.first().is_none()`.
     *
     * Scenario: user adds a new disk to an already-mounted 1-disk pool.
     *   One prompt, then verify_passphrase authenticates, then format
     *   runs.
     */
    #[test]
    fn cmd_add_live_pool_fresh_add_single_prompt() {
        let (_state_tmp, paths, _tmp, config_path, _pass_path) = add_test_setup();
        let fs = AddMockFs(vec![
            "/dev/disk/by-id/virtio-disk2".into(),
            // /sys/fs/btrfs/<fsid>/exclusive_operation is served by
            // AddMockFs::read_to_string, not exists(). No entry needed.
        ]);
        let runner = AddRecordingRunner::new(true);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let tty = ScriptedPassphraseReader::new(["pw", "SENTINEL"]);

        let _ = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk2=/dev/disk/by-id/virtio-disk2".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: None,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &tty,
            },
        );

        assert!(
            runner.saw_verify(),
            "verify_passphrase must run against live pool member"
        );
        assert!(runner.saw_format(), "luks_format must have run");
        assert_eq!(
            tty.remaining(),
            1,
            "exactly one prompt must have been read (SENTINEL remains)"
        );
    }
}
