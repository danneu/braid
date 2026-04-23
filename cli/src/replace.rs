use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::{config_read, mapper_name};
use crate::confirm;
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal;
use crate::luks::{
    backup_luks_header, ensure_luks_open, luks_format, luks_opts_from_env,
    pool_has_keyfile_enrollment, read_passphrase, verify_passphrase, VerifyOutcome,
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

pub struct ReplaceParams<'a> {
    pub config_path: &'a Path,
    pub old_name: &'a str,
    pub new_name: &'a str,
    pub missing_id: Option<u64>,
    pub dry_run: bool,
    pub yes: bool,
    pub passphrase_stdin: bool,
    pub passphrase_file: Option<&'a Path>,
    pub enroll_key_file: Option<&'a Path>,
    pub progress: ProgressOutput,
    pub paths: &'a StatePaths,
    /// Seam for acquiring a logind sleep inhibitor before the irreversible
    /// portion of the replace. Production passes `&RealSleepInhibitor`;
    /// unit tests pass `&NoopSleepInhibitor` to avoid spawning subprocesses.
    pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
}

pub fn cmd_replace<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &ReplaceParams<'_>,
) -> Result<(), ReplaceError> {
    preflight::check_no_pending_operation(params.paths).map_err(ReplaceError::Validation)?;

    let config = config_read(params.config_path)?;

    // Parse new_name as name=by_id spec
    let (new_name_parsed, new_by_id) = membership::parse_disk_spec(params.new_name)
        .map_err(|e| ReplaceError::Validation(e.to_string()))?;
    let new_name = new_name_parsed.as_str();

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
    let fsid = pool.fsid.as_deref().expect("mounted pool must have FSID");
    preflight::require_mutation_preflight(runner, fs, fsid, config.mount_point())
        .map_err(ReplaceError::Validation)?;
    preflight::check_ups_not_on_battery(runner, config.ups().map(|u| u.name.as_str()), "replace")
        .map_err(ReplaceError::Validation)?;

    // --old == --new: reject early.
    if params.old_name == new_name {
        return Err(ReplaceError::Validation(
            "--old and --new must be different disks".into(),
        ));
    }

    // Resolve --old: live or missing (by devid).
    let old_mn = mapper_name(params.old_name);
    let replace_source = resolve_replace_source(
        runner,
        params.old_name,
        &old_mn,
        params.missing_id,
        &pool,
        config.mount_point(),
    )?;

    // Validate --old against pool.json membership before any irreversible work.
    // build_replacement_membership rejects absent old_name and (on Missing path)
    // a devid mismatch between pool.json and the resolved missing devid. Running
    // it here -- before the inhibitor and journal write -- means a typo in --old
    // aborts cleanly with no pending-op.json on disk and no systemd-inhibit held.
    let pre_membership = membership::load_membership(params.paths)
        .map_err(|e| ReplaceError::Validation(format!("failed to load pool membership: {e}")))?;
    let target_membership =
        build_replacement_membership(&pre_membership, params.old_name, new_name, &new_by_id, &replace_source)?;

    // Probe --new disk state
    let new_probed = probe_config_disk(runner, fs, new_name, &new_by_id)?;

    // Compile steps
    let will_clear_last_missing =
        matches!(&replace_source, ReplaceSource::Missing { .. }) && pool.missing_count == 1;
    let steps = compile_replace_steps(&ReplaceStepsInput {
        new_name,
        new_by_id: &new_by_id,
        new_probed: &new_probed,
        replace_source: &replace_source,
        mount_point: config.mount_point(),
        will_clear_last_missing,
        total_devices: pool.total_devices,
        paths: params.paths,
        enroll_key_file: params.enroll_key_file,
    })?;

    if params.dry_run {
        Step::print_dry_run(&steps);
        return Ok(());
    }

    // Confirm
    if !params.yes {
        let old_underlying = match &replace_source {
            ReplaceSource::Live { .. } => pool
                .devices
                .iter()
                .find(|d| d.mapper == old_mn)
                .map(|d| d.underlying.as_str()),
            ReplaceSource::Missing { .. } => None,
        };
        let old_hw = old_underlying.map(|u| confirm::query_disk_hw_info(runner, u));
        let new_hw = confirm::query_disk_hw_info(runner, &new_by_id.0);
        let is_missing = matches!(&replace_source, ReplaceSource::Missing { .. });

        eprintln!(
            "{}",
            format_replace_confirm(
                &ReplaceConfirmOld {
                    name: params.old_name,
                    hw: old_hw.as_ref(),
                    source: &replace_source,
                },
                &ReplaceConfirmNew {
                    name: new_name,
                    by_id: &new_by_id.0,
                    hw: &new_hw,
                    needs_luks_format: matches!(new_probed.state, ConfigDiskState::PresentNotLuks),
                    is_rebuild: is_missing,
                },
                pool.total_devices,
            )
        );
        if pool.total_devices == 1 {
            eprintln!("WARNING: This replace leaves only 1 disk -- no redundancy.\n");
        }
        if matches!(new_probed.state, ConfigDiskState::PresentNotLuks)
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
        confirm::confirm_yes().map_err(ReplaceError::Validation)?;
    }

    // Read passphrase
    let passphrase = read_passphrase(params.passphrase_file, params.passphrase_stdin)?;
    let new_mn = mapper_name(new_name);

    // Reversible checks: reject absent disk, verify passphrase, check not already in pool.
    if matches!(new_probed.state, ConfigDiskState::Absent) {
        return Err(ReplaceError::Validation(format!(
            "new disk '{}' ({}) is not present. Is it plugged in?",
            new_name, new_by_id
        )));
    }

    if matches!(new_probed.state, ConfigDiskState::PresentNotLuks)
        && let Some(existing) = pool.devices.first() {
            let status_raw = runner.run(&crate::cmd::CmdRequest::CryptsetupStatus {
                mapper: existing.mapper.0.clone(),
            })?;
            let status = crate::parse::parse_cryptsetup_status(&status_raw)?;
            if let Some(underlying) = status.device {
                match verify_passphrase(runner, &underlying, &passphrase)? {
                    VerifyOutcome::Authenticated => {}
                    VerifyOutcome::Rejected => {
                        return Err(ReplaceError::Validation(
                            "passphrase does not match existing pool member".into(),
                        ));
                    }
                }
            }
        }

    // Reversible check: for an already-formatted but closed LUKS disk,
    // verify the passphrase against the new disk's own LUKS header before
    // committing the journal. Without this, ensure_luks_open below would be
    // the first thing to notice a wrong passphrase -- and by then the
    // journal is already written and the user is forced into braid recover
    // mode for what is conceptually a preflight failure (see decision 019).
    if let ConfigDiskState::PresentLuks { mapper_open: false, .. } = new_probed.state {
        match verify_passphrase(runner, &new_by_id.0, &passphrase)? {
            VerifyOutcome::Authenticated => {}
            VerifyOutcome::Rejected => {
                return Err(ReplaceError::Validation(format!(
                    "passphrase rejected by new disk '{new_name}' ({new_by_id})"
                )));
            }
        }
    }

    // Guard: new disk must not already be in the pool.
    check_new_not_in_pool(new_name, &new_mn, &pool)?;

    // Hold a logind sleep inhibitor for the rest of the replace operation --
    // covers Step 1 LUKS init, the long-running btrfs replace start, and
    // the post-replace soft balance for missing-path replaces. Suspending
    // mid-replace produces kernel-level topology corruption on every kernel
    // -- see issues #45 and #48 and the upstream warning at
    // reference/btrfs-progs/Documentation/btrfs-replace.rst:49-50.
    //
    // Acquired here, AFTER all interactive/reversible work (confirmation,
    // passphrase read+verify, check_new_not_in_pool) and BEFORE
    // journal::write_journal, so that:
    //   - operator-idle prompts do not block suspend
    //   - a logind failure aborts cleanly without stranding pending-op.json
    //     and forcing the user into recovery mode for a preflight failure.
    let _sleep_inhibitor_guard = params
        .sleep_inhibitor
        .acquire("replace in progress")
        .map_err(|e| {
            ReplaceError::Validation(format!(
                "could not acquire sleep inhibitor (is logind running?): {e}"
            ))
        })?;

    // Write journal before irreversible disk ops. pre_membership and
    // target_membership were computed earlier, before the inhibitor.
    let journal = journal::build_journal(
        pre_membership,
        target_membership.clone(),
        journal::OpKind::Replace {
            old_name: params.old_name.to_owned(),
            new_name: new_name.to_owned(),
            new_by_id: new_by_id.clone(),
        },
    );
    journal::write_journal(params.paths, &journal)
        .map_err(|e| ReplaceError::Validation(e.to_string()))?;

    // Step 1: Init new disk (LUKS format/open) -- irreversible from here.
    match new_probed.state {
        ConfigDiskState::Absent => unreachable!("already checked above"),
        ConfigDiskState::PresentNotLuks => {
            // Passphrase already verified above.
            let mut luks_opts = luks_opts_from_env();
            luks_opts.push("--label".into());
            luks_opts.push(format!("braid-{new_name}"));
            luks_format(runner, &new_by_id.0, &passphrase, &luks_opts)?;
            eprintln!("LUKS formatted: {}", new_by_id);

            let backup_path = backup_luks_header(runner, &new_by_id.0, &new_mn.0, params.paths)?;
            eprintln!("LUKS header backed up: {}", backup_path.display());

            ensure_luks_open(runner, fs, new_name, &new_by_id, &passphrase)?;
            eprintln!("LUKS opened: {} -> {}", new_by_id, new_mn);

            if let Some(kf) = params.enroll_key_file {
                crate::luks::enroll_key_file(runner, &new_by_id.0, &passphrase, kf)?;
                eprintln!("Keyfile enrolled in slot 1: {}", new_by_id);
            }
        }
        ConfigDiskState::PresentLuks { mapper_open, .. } => {
            if !mapper_open {
                ensure_luks_open(runner, fs, new_name, &new_by_id, &passphrase)?;
                eprintln!("LUKS opened: {} -> {}", new_by_id, new_mn);
            } else if !pool.devices.iter().any(|d| d.mapper == new_mn) {
                eprintln!(
                    "note: LUKS mapper is already open but device is not yet in pool. Completing replace."
                );
            }
        }
    }

    let new_mapper_path = format!("/dev/mapper/{}", new_mn.0);

    // Step 2+: Execute replacement -- both paths use btrfs replace start.
    // Live-only: warn if the source device has accumulated I/O errors.
    if let ReplaceSource::Live { mapper, devid } = &replace_source {
        let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStatsJson {
            mount_point: config.mount_point().clone(),
        });
        if let Ok(ref raw) = stats_raw
            && let Ok(stats) = parse_btrfs_device_stats(raw)
        {
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

    // Kickoff wording differs (replace-in-place vs rebuild-missing), but the
    // underlying `btrfs replace start` + resize sequence is identical. Bind
    // devid here so the shared spine below runs once.
    let devid = match &replace_source {
        ReplaceSource::Live { devid, .. } => {
            eprintln!("Replacing device (devid {devid}) with {}...", new_mn);
            *devid
        }
        ReplaceSource::Missing { devid } => {
            eprintln!(
                "Rebuilding missing device (devid {devid}) onto {}...",
                new_mn
            );
            *devid
        }
    };

    pool_replace_device(
        runner,
        devid,
        &new_mapper_path,
        config.mount_point(),
        params.progress,
    )?;
    eprintln!("Replace complete.");

    // Live-only: best-effort close of old mapper. Runs BEFORE the resize
    // so a resize failure does not `?` out and strand the old dm slot
    // bound to the backing disk until `braid lock` or reboot. Missing has
    // no old mapper to close.
    if let ReplaceSource::Live { mapper, .. } = &replace_source {
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
            _ => {
                eprintln!(
                    "Old device closed. If repurposing the physical disk, wipe it separately."
                );
            }
        }
    }

    pool_resize_device(runner, devid, config.mount_point())?;

    // Restore RAID1 redundancy for missing-path replacements that clear the last missing device
    if matches!(&replace_source, ReplaceSource::Missing { .. }) {
        crate::pool::maybe_restore_raid1(
            runner,
            config.mount_point(),
            pool.missing_count,
            params.progress,
        )
        .map_err(ReplaceError::Pool)?;
    }

    // Post-commit: write pool.json with enriched metadata and clear journal.
    let mut target_membership = target_membership;
    if let Ok(pool_after) = probe_pool(runner, config.mount_point()) {
        for dev in &pool_after.devices {
            let Some(name) = crate::config::name_from_mapper(&dev.mapper.0) else {
                continue;
            };
            if let Some(member) = target_membership.disks.get_mut(name) {
                member.luks_uuid = Some(dev.luks_uuid.clone());
                member.devid = Some(dev.devid);
                if member.added_at.is_none() {
                    member.added_at = Some(crate::util::now_iso());
                }
            }
        }
    }
    membership::save_membership(&target_membership, params.paths)
        .map_err(|e| ReplaceError::Validation(format!("failed to persist pool membership: {e}")))?;
    journal::clear_journal(params.paths).map_err(|e| ReplaceError::Validation(e.to_string()))?;

    eprintln!("Done. Replaced {} with {}.", params.old_name, new_name);
    Ok(())
}

#[derive(Debug)]
enum ReplaceSource {
    /// Old disk is alive in the pool -- replace via `btrfs replace start`.
    Live { mapper: MapperName, devid: u64 },
    /// Old disk is missing -- replace via `btrfs replace start` by devid.
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
    mount_point: &MountPoint,
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

    // Old disk not in pool -- dead/missing path.
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

struct ReplaceStepsInput<'a> {
    new_name: &'a str,
    new_by_id: &'a ByIdPath,
    new_probed: &'a ConfigDisk,
    replace_source: &'a ReplaceSource,
    mount_point: &'a MountPoint,
    will_clear_last_missing: bool,
    total_devices: u64,
    paths: &'a StatePaths,
    enroll_key_file: Option<&'a Path>,
}

fn compile_replace_steps(input: &ReplaceStepsInput<'_>) -> Result<Vec<Step>, ReplaceError> {
    let new_mn = mapper_name(input.new_name);
    let mut steps = Vec::new();

    match &input.new_probed.state {
        ConfigDiskState::Absent => {
            return Err(ReplaceError::Validation(format!(
                "new disk '{}' ({}) is not present. Is it plugged in?",
                input.new_name, input.new_by_id
            )));
        }
        ConfigDiskState::PresentNotLuks => {
            let mut extra_opts = luks_opts_from_env();
            extra_opts.push("--label".into());
            extra_opts.push(format!("braid-{}", input.new_name));
            steps.push(Step {
                risk: "destructive",
                description: format!("LUKS format {}", input.new_by_id),
                commands: vec![CmdRequest::CryptsetupLuksFormat {
                    device: input.new_by_id.0.clone(),
                    extra_opts,
                }],
            });
            let backup_path = input
                .paths
                .luks_headers_dir()
                .join(format!("{}.luksheader", new_mn.0));
            steps.push(Step {
                risk: "safe",
                description: format!("LUKS header backup -> {}", backup_path.display()),
                commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
                    device: input.new_by_id.0.clone(),
                    backup_path: backup_path.display().to_string(),
                }],
            });
            steps.push(Step {
                risk: "safe",
                description: format!("LUKS open -> {}", new_mn),
                commands: vec![CmdRequest::CryptsetupLuksOpen {
                    device: input.new_by_id.0.clone(),
                    mapper: new_mn.0.clone(),
                }],
            });
            if let Some(kf) = input.enroll_key_file {
                steps.push(Step {
                    risk: "safe",
                    description: format!("enroll keyfile -> LUKS slot 1 on {}", input.new_by_id),
                    commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
                        device: input.new_by_id.0.clone(),
                        key_file_path: kf.display().to_string(),
                    }],
                });
            }
        }
        ConfigDiskState::PresentLuks { mapper_open, .. } => {
            if !mapper_open {
                steps.push(Step {
                    risk: "safe",
                    description: format!("LUKS open -> {}", new_mn),
                    commands: vec![CmdRequest::CryptsetupLuksOpen {
                        device: input.new_by_id.0.clone(),
                        mapper: new_mn.0.clone(),
                    }],
                });
            }
        }
    }

    let new_mapper_path = format!("/dev/mapper/{}", new_mn.0);
    let devid = match input.replace_source {
        ReplaceSource::Live { devid, .. } | ReplaceSource::Missing { devid } => *devid,
    };

    // Shared: btrfs replace start.
    steps.push(Step {
        risk: "long",
        description: format!(
            "btrfs replace start {} /dev/mapper/{} {}",
            devid, new_mn, input.mount_point
        ),
        commands: vec![CmdRequest::BtrfsReplaceStart {
            devid,
            target_device: new_mapper_path,
            mount_point: input.mount_point.clone(),
        }],
    });

    // Live-only: close old mapper before the resize -- mirrors the ordering
    // in cmd_replace, which runs the close before resize so a resize error
    // does not strand the old dm slot.
    if let ReplaceSource::Live { mapper, .. } = input.replace_source {
        steps.push(Step {
            risk: "safe",
            description: format!("cryptsetup close {}", mapper),
            commands: vec![CmdRequest::CryptsetupClose {
                mapper: mapper.0.clone(),
            }],
        });
    }

    // Shared: btrfs filesystem resize.
    steps.push(Step {
        risk: "safe",
        description: format!(
            "btrfs filesystem resize {}:max {}",
            devid, input.mount_point
        ),
        commands: vec![CmdRequest::BtrfsFilesystemResize {
            devid,
            mount_point: input.mount_point.clone(),
        }],
    });

    // Missing-only: restore RAID1 redundancy after the last missing device
    // clears. Live replace never creates single-profile chunks, so this
    // step is unconditionally absent on the Live path.
    if let ReplaceSource::Missing { .. } = input.replace_source
        && input.will_clear_last_missing
        && input.total_devices >= 2
    {
        steps.push(Step {
            risk: "long",
            description:
                "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft (restore redundancy)"
                    .into(),
            commands: vec![CmdRequest::BtrfsBalanceRaid1Soft {
                mount_point: input.mount_point.clone(),
            }],
        });
    }

    Ok(steps)
}

// ---------------------------------------------------------------------------
// Confirmation formatter
// ---------------------------------------------------------------------------

struct ReplaceConfirmOld<'a> {
    name: &'a str,
    hw: Option<&'a confirm::DiskHwInfo>,
    source: &'a ReplaceSource,
}

struct ReplaceConfirmNew<'a> {
    name: &'a str,
    by_id: &'a str,
    hw: &'a confirm::DiskHwInfo,
    needs_luks_format: bool,
    is_rebuild: bool,
}

fn format_replace_confirm(
    old: &ReplaceConfirmOld,
    new: &ReplaceConfirmNew,
    total_devices: u64,
) -> String {
    let mut msg = "Replace disk:\n".to_string();

    // Old disk
    match old.source {
        ReplaceSource::Live { devid, .. } => {
            let old_hw_line = old.hw.and_then(confirm::format_hw_info_line);
            if let Some(hw) = &old_hw_line {
                msg.push_str(&format!("  old: {}   {}\n", old.name, hw));
                msg.push_str(&format!(
                    "  {:width$}devid {} | will be replaced in-place\n",
                    "",
                    devid,
                    width = old.name.len() + 7,
                ));
            } else {
                msg.push_str(&format!(
                    "  old: {}   devid {} | will be replaced in-place\n",
                    old.name, devid
                ));
            }
        }
        ReplaceSource::Missing { devid } => {
            msg.push_str(&format!(
                "  old: {} (devid {})  missing -- no hardware info available\n",
                old.name, devid
            ));
        }
    }

    // New disk
    let new_hw_line = confirm::format_hw_info_line(new.hw);
    let indent = new.name.len() + 7; // "  new: " + name + "  "
    msg.push_str(&format!("  new: {}  {}\n", new.name, new.by_id));
    if let Some(hw) = &new_hw_line {
        msg.push_str(&format!("  {:width$}{}\n", "", hw, width = indent));
    }
    if new.needs_luks_format {
        msg.push_str(&format!(
            "  {:width$}Will be LUKS-formatted (existing data will be inaccessible)\n",
            "",
            width = indent,
        ));
    }
    if new.is_rebuild {
        msg.push_str(&format!(
            "  {:width$}Data will be rebuilt from RAID redundancy.\n",
            "",
            width = indent,
        ));
    }

    // Pool summary
    msg.push_str(&format!(
        "\nPool: {} {} -> {} {}\n",
        total_devices,
        if total_devices == 1 { "disk" } else { "disks" },
        total_devices,
        if total_devices == 1 { "disk" } else { "disks" },
    ));

    msg
}

fn build_replacement_membership(
    existing: &membership::PoolMembership,
    old_name: &str,
    new_name: &str,
    new_by_id: &ByIdPath,
    replace_source: &ReplaceSource,
) -> Result<membership::PoolMembership, ReplaceError> {
    let existing_member = existing.disks.get(old_name).ok_or_else(|| {
        ReplaceError::Validation(format!(
            "'{old_name}' not found in pool.json membership -- \
             no disk entry has this name. Pool membership may need manual repair."
        ))
    })?;

    if let ReplaceSource::Missing { devid } = replace_source
        && existing_member.devid != Some(*devid)
    {
        return Err(ReplaceError::Validation(format!(
            "--old '{old_name}' records devid {pool_devid:?} in pool.json, \
             but btrfs reports missing devid {devid}. \
             --old and --missing-id disagree about which member is being replaced.",
            pool_devid = existing_member.devid
        )));
    }

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
    use crate::state_paths::StatePaths;

    fn test_paths() -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        (tmp, paths)
    }

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
    }

    #[test]
    fn replace_confirm_warns_about_luks_format_for_non_luks_disk() {
        let new_hw = confirm::DiskHwInfo {
            model: Some("WD Elements".into()),
            serial: Some("5678EFGH".into()),
            size: Some(12_000_000_000_000),
        };
        let msg = format_replace_confirm(
            &ReplaceConfirmOld {
                name: "old1",
                hw: None,
                source: &ReplaceSource::Missing { devid: 2 },
            },
            &ReplaceConfirmNew {
                name: "new1",
                by_id: "/dev/disk/by-id/usb-WD_5678",
                hw: &new_hw,
                needs_luks_format: true,
                is_rebuild: true,
            },
            3,
        );
        assert!(msg.contains("LUKS-formatted"), "should mention LUKS-format");
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
        let new_hw = confirm::DiskHwInfo::default();
        let msg = format_replace_confirm(
            &ReplaceConfirmOld {
                name: "old1",
                hw: None,
                source: &ReplaceSource::Missing { devid: 2 },
            },
            &ReplaceConfirmNew {
                name: "new1",
                by_id: "/dev/disk/by-id/usb-WD_5678",
                hw: &new_hw,
                needs_luks_format: false,
                is_rebuild: true,
            },
            3,
        );
        assert!(
            msg.contains("devid 2"),
            "should mention missing devid, got: {}",
            msg
        );
        assert!(
            msg.contains("missing"),
            "should indicate missing device, got: {}",
            msg
        );
        assert!(
            msg.contains("rebuilt from RAID redundancy"),
            "should mention rebuild, got: {}",
            msg
        );
        assert!(
            !msg.contains("LUKS-formatted"),
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
            null_underlying: vec![],
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
            CmdRequest::BtrfsDeviceUsageRaw { mount_point: mp() },
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
    // Why: core behavior -- replace must accept live disks when pool has no missing.
    // Scenario: operator swaps a slow-but-alive drive for a faster one.
    fn live_old_resolution_succeeds_no_missing() {
        let pool = two_device_pool();
        let runner = MockRunner::default();
        let mn = MapperName("braid-disk2".into());
        let result = resolve_replace_source(&runner, "disk2", &mn, None, &pool, &mp());
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
        let err = resolve_replace_source(&runner, "disk2", &mn, Some(99), &pool, &mp())
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
            resolve_replace_source(&runner, "disk2", &mn, None, &pool, &mp()).unwrap_err();
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
            &ReplaceSource::Live {
                mapper: MapperName("braid-disk1".into()),
                devid: 1,
            },
        )
        .expect_err("should reject by-id rename conflict");

        assert!(
            err.to_string().contains("cannot register"),
            "unexpected error: {err}"
        );
    }

    fn disk_member_with_devid(by_id: &str, devid: u64) -> membership::DiskMember {
        let mut m = membership::DiskMember::from_by_id(ByIdPath(by_id.into()));
        m.devid = Some(devid);
        m
    }

    #[test]
    // Intent: Missing-path build rejects when --old is absent from pool.json.
    // Why: silent HashMap::remove on a missing key previously produced orphan
    //   entries in pool.json on operator typo, which broke the next unlock
    //   via mount::plan_open_pool's Absent-member detection.
    // Scenario: operator types `braid replace --old disk2 --missing-id 2 --new ...`
    //   but pool.json only knows about disk1.
    fn build_replacement_membership_missing_rejects_absent_old_name() {
        let mut m = membership::PoolMembership::empty();
        m.disks
            .insert("disk1".into(), disk_member_with_devid("/dev/disk/by-id/virtio-disk1", 1));

        let result = build_replacement_membership(
            &m,
            "disk2",
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &ReplaceSource::Missing { devid: 2 },
        );

        assert!(
            matches!(result, Err(ReplaceError::Validation(_))),
            "expected Err(Validation(_)), got: {result:?}"
        );
    }

    #[test]
    // Intent: Missing-path build rejects when pool.json's devid for --old
    //   disagrees with the resolved missing devid.
    // Why: --old and --missing-id disagreeing silently would let the journal
    //   record one devid while pool.json describes another, leaving
    //   pool.json inconsistent with btrfs.
    // Scenario: operator runs --old disk2 --missing-id 2, but pool.json
    //   records disk2 with devid 3.
    fn build_replacement_membership_missing_rejects_devid_mismatch() {
        let mut m = membership::PoolMembership::empty();
        m.disks
            .insert("disk2".into(), disk_member_with_devid("/dev/disk/by-id/virtio-disk2", 3));

        let result = build_replacement_membership(
            &m,
            "disk2",
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &ReplaceSource::Missing { devid: 2 },
        );

        assert!(
            matches!(result, Err(ReplaceError::Validation(_))),
            "expected Err(Validation(_)), got: {result:?}"
        );
    }

    #[test]
    // Intent: Live-path build also rejects when --old is absent from pool.json.
    // Why: symmetric guard -- the silent .remove no-op applies to both paths;
    //   a Live-path typo would also leave an orphan btrfs member in pool.json.
    // Scenario: operator runs live replace with a typo in --old.
    fn build_replacement_membership_live_rejects_absent_old_name() {
        let mut m = membership::PoolMembership::empty();
        m.disks
            .insert("disk1".into(), disk_member_with_devid("/dev/disk/by-id/virtio-disk1", 1));

        let result = build_replacement_membership(
            &m,
            "disk2",
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &ReplaceSource::Live {
                mapper: MapperName("braid-disk2".into()),
                devid: 2,
            },
        );

        assert!(
            matches!(result, Err(ReplaceError::Validation(_))),
            "expected Err(Validation(_)), got: {result:?}"
        );
    }

    #[test]
    // Intent: Missing-path happy path returns Ok with the old entry removed
    //   and the new entry inserted.
    // Why: pins the positive branch so the rejection tests can't drift into
    //   false positives (e.g. a bug that rejects everything).
    // Scenario: operator replaces disk2 (missing devid 2) with disk3; pool.json
    //   has disk2 recorded with devid 2.
    fn build_replacement_membership_missing_happy_path() {
        let mut m = membership::PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk1", 1),
        );
        m.disks.insert(
            "disk2".into(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk2", 2),
        );

        let next = build_replacement_membership(
            &m,
            "disk2",
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &ReplaceSource::Missing { devid: 2 },
        )
        .expect("happy path");

        assert!(!next.disks.contains_key("disk2"));
        assert!(next.disks.contains_key("disk3"));
        assert!(next.disks.contains_key("disk1"));
    }

    #[test]
    // Intent: Live-path happy path returns Ok with the old entry removed and
    //   the new entry inserted. Devid cross-check does not apply.
    // Why: same rationale as the Missing-path happy path.
    // Scenario: operator swaps a live disk2 for a fresh disk3.
    fn build_replacement_membership_live_happy_path() {
        let mut m = membership::PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk1", 1),
        );
        m.disks.insert(
            "disk2".into(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk2", 2),
        );

        let next = build_replacement_membership(
            &m,
            "disk2",
            "disk3",
            &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            &ReplaceSource::Live {
                mapper: MapperName("braid-disk2".into()),
                devid: 2,
            },
        )
        .expect("happy path");

        assert!(!next.disks.contains_key("disk2"));
        assert!(next.disks.contains_key("disk3"));
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
        let _config: crate::config::Config =
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
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
        })
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
        let _config: crate::config::Config =
            serde_json::from_value(config_json).expect("valid config");
        let new_probed = ConfigDisk {
            name: "disk3".into(),
            by_id_path: ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            state: ConfigDiskState::PresentNotLuks,
        };
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: true,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
        })
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
    // Scenario: operator typo -- specifies an existing pool member as --new.
    fn new_disk_already_in_pool_rejected() {
        let pool = two_device_pool(); // has braid-disk1 and braid-disk2
        let new_mn = mapper_name("disk2"); // -> "braid-disk2"
        let err = check_new_not_in_pool("disk2", &new_mn, &pool).unwrap_err();
        assert!(
            err.to_string().contains("already a member"),
            "expected 'already a member' error, got: {err}"
        );
    }

    #[test]
    // Intent: a disk NOT in the pool passes the guard.
    // Why: regression -- the guard must not block valid replacements.
    // Scenario: normal replace with a fresh disk.
    fn new_disk_not_in_pool_passes() {
        let pool = two_device_pool();
        let new_mn = mapper_name("disk3");
        check_new_not_in_pool("disk3", &new_mn, &pool).expect("disk3 is not in pool -- should pass");
    }

    #[test]
    // Intent: confirm text for live path does NOT say "dead".
    // Why: calling a live disk "dead" is confusing.
    // Scenario: operator sees confirmation prompt for live replace.
    fn replace_confirm_live_does_not_say_dead() {
        let old_hw = confirm::DiskHwInfo {
            model: Some("Toshiba MN07".into()),
            serial: None,
            size: Some(12_000_000_000_000),
        };
        let new_hw = confirm::DiskHwInfo::default();
        let msg = format_replace_confirm(
            &ReplaceConfirmOld {
                name: "disk2",
                hw: Some(&old_hw),
                source: &ReplaceSource::Live {
                    mapper: MapperName("braid-disk2".into()),
                    devid: 2,
                },
            },
            &ReplaceConfirmNew {
                name: "disk3",
                by_id: "/dev/disk/by-id/virtio-disk3",
                hw: &new_hw,
                needs_luks_format: false,
                is_rebuild: false,
            },
            3,
        );
        assert!(
            !msg.contains("dead"),
            "live replace prompt should not say 'dead', got: {msg}"
        );
        assert!(
            msg.contains("replaced in-place"),
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
        let result = resolve_replace_source(&runner, "disk2", &mn, None, &pool, &mp());
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
        let result = resolve_replace_source(&runner, "disk2", &mn, Some(2), &pool, &mp());
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
        let err = resolve_replace_source(&runner, "disk2", &mn, Some(1), &pool, &mp())
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
        let err = resolve_replace_source(&runner, "disk2", &mn, Some(99), &pool, &mp())
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
            resolve_replace_source(&runner, "disk2", &mn, None, &pool, &mp()).unwrap_err();
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
    // Scenario: 3-disk pool, 2 missing, replacing 1 -- still degraded after.
    fn dry_run_missing_not_last_omits_rebalance() {
        let _config = make_replace_config();
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 3,
            paths: &test_paths().1,
            enroll_key_file: None,
        })
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
        let _config = make_replace_config();
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: true,
            total_devices: 1,
            paths: &test_paths().1,
            enroll_key_file: None,
        })
        .unwrap();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("-dconvert=raid1,soft")),
            "should NOT show soft balance with total_devices == 1, got: {descriptions:?}"
        );
    }

    use crate::cmd::{CmdError, CommandRunner as CmdRunner2};
    use crate::membership::{self, DiskMember, PoolMembership};

    fn mock_ok(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    /// Mock filesystem where specific paths exist.
    struct ReplaceMockFs(Vec<String>);
    impl crate::probe::Filesystem for ReplaceMockFs {
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

    /// Runner for live replace that fails on BtrfsReplaceStart.
    /// Handles all preflight/probe commands successfully.
    struct FailingReplaceRunner;

    impl CmdRunner2 for FailingReplaceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_ok(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                )),
                CmdRequest::CryptsetupStatus { mapper } => {
                    let dev = match mapper.as_str() {
                        "braid-disk1" => "/dev/vdb",
                        "braid-disk2" => "/dev/vdc",
                        "braid-disk3" => "/dev/vdd",
                        _ => "/dev/vdz",
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!("{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"),
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                            "11111111-1111-1111-1111-111111111111"
                        }
                        "/dev/vdc" | "/dev/disk/by-id/virtio-disk2" => {
                            "22222222-2222-2222-2222-222222222222"
                        }
                        // new disk: its backing via the braid-disk3 mapper is /dev/vdd
                        "/dev/vdd" | "/dev/disk/by-id/virtio-disk3" => {
                            "33333333-3333-3333-3333-333333333333"
                        }
                        _ => "99999999-9999-9999-9999-999999999999",
                    };
                    Ok(mock_ok(&format!("cryptsetup luksUUID {device}"), &format!("{uuid}\n")))
                }
                CmdRequest::CryptsetupLuksDumpText { device } => Ok(mock_ok(
                    &format!("cryptsetup luksDump {device}"),
                    "LUKS header information\nVersion:       \t2\n",
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                CmdRequest::BtrfsDeviceStatsJson { .. } => Ok(mock_ok("btrfs device stats", r#"{"device-stats": []}"#)),
                CmdRequest::BtrfsReplaceStart { .. } => Ok(RawCommandOutput {
                    cmd: "btrfs replace start".into(),
                    stdout: String::new(),
                    stderr: "ERROR: target device is too small".into(),
                    exit_status: 1,
                }),
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
    // Intent: pending-op.json survives when btrfs replace start fails.
    //
    // Why it exists: JournalGuard previously cleared the journal on any exit,
    //   including error returns. After LUKS init on the new disk, a failed
    //   btrfs replace would leave pool.json stale with no recovery path.
    //
    // Scenario: live replace, new disk already LUKS-open, btrfs replace start
    //   fails (e.g. target too small). Journal must persist for recovery.
    fn journal_survives_replace_failure() {
        // Set up state
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        // Passphrase file (required by cmd_replace)
        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        // Filesystem mock: new disk and its mapper exist
        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let runner = FailingReplaceRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        assert!(
            result.is_err(),
            "replace should fail when btrfs replace fails"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        // The journal exists, which proves we got past journal::write_journal,
        // which proves the inhibitor was acquired exactly once on the way in.
        // Locks in the seam placement: if a refactor moves the acquire to a
        // post-journal point or skips it entirely, this assert flips.
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
        );
    }

    #[test]
    // Intent: cmd_replace rejects --old == --new (post-parse) with a
    //   Validation error, on the reversible side of the inhibitor/journal
    //   seam.
    //
    // Why it exists: the old==new guard at replace.rs:94-98 is a
    //   user-visible CLI contract (operator typo protection). It fires
    //   before probe_config_disk's mapper-conflict detection would
    //   otherwise surface the same bug as a confusing MapperConflict
    //   probe error. Without direct cmd-level coverage, a refactor that
    //   drops the guard would change the rejection variant from
    //   Validation("must be different") to Probe(MapperConflict), and a
    //   refactor that moved the guard past the inhibitor/journal seam
    //   would strand a pending-op.json and a held logind inhibitor on
    //   what is conceptually a preflight rejection. Replaces a prior
    //   tautological test (assert_eq!("disk1", "disk1", ...)) that
    //   exercised no production code.
    //
    // Scenario: operator runs
    //   `braid replace --old disk1 --new disk1=/dev/disk/by-id/virtio-disk3`
    //   -- same name on both sides after parsing the new-name spec.
    fn cmd_replace_rejects_old_equals_new() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let runner = FailingReplaceRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk1",
                new_name: "disk1=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        match &result {
            Err(ReplaceError::Validation(msg)) => {
                assert!(
                    msg.contains("must be different"),
                    "expected old==new guard message, got: {msg}"
                );
            }
            other => panic!("expected Err(ReplaceError::Validation), got: {other:?}"),
        }
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "old==new typo must be caught before the inhibitor seam -- a caught typo must not hold logind"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal may be written when old==new"
        );
    }

    #[test]
    // Intent: --dry-run must not acquire the sleep inhibitor.
    //
    // Why it exists: dry-run takes no irreversible action and never reaches
    //   the irreversible section that the inhibitor is meant to protect. If
    //   acquisition leaks into the dry-run path it would spawn systemd-inhibit
    //   for nothing -- wasteful and a UX surprise (operators do not expect
    //   --dry-run to require logind).
    //
    // Scenario: operator runs `braid replace --old disk2 --new disk3=... --dry-run`
    //   to preview the plan. cmd_replace must short-circuit at the dry-run
    //   branch before the inhibitor seam fires.
    fn dry_run_does_not_acquire_inhibitor() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let runner = FailingReplaceRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        assert!(result.is_ok(), "dry-run should succeed: {result:?}");
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "dry-run must NOT acquire the sleep inhibitor -- it has no irreversible work to protect"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "dry-run must not write the journal"
        );
    }

    /// Runner for a live replace where btrfs replace + cryptsetup close
    /// succeed but the post-replace `btrfs filesystem resize` fails.
    /// Records every request so the test can assert that the close ran
    /// BEFORE the failing resize (regression for the Live arm ordering
    /// bug where a resize `?` would skip the close).
    struct ResizeFailingLoggingRunner {
        log: std::sync::Arc<std::sync::Mutex<Vec<CmdRequest>>>,
    }

    impl CmdRunner2 for ResizeFailingLoggingRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_ok(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                )),
                CmdRequest::CryptsetupStatus { mapper } => {
                    let dev = match mapper.as_str() {
                        "braid-disk1" => "/dev/vdb",
                        "braid-disk2" => "/dev/vdc",
                        "braid-disk3" => "/dev/vdd",
                        _ => "/dev/vdz",
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!("{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"),
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                            "11111111-1111-1111-1111-111111111111"
                        }
                        "/dev/vdc" | "/dev/disk/by-id/virtio-disk2" => {
                            "22222222-2222-2222-2222-222222222222"
                        }
                        "/dev/vdd" | "/dev/disk/by-id/virtio-disk3" => {
                            "33333333-3333-3333-3333-333333333333"
                        }
                        _ => "99999999-9999-9999-9999-999999999999",
                    };
                    Ok(mock_ok(&format!("cryptsetup luksUUID {device}"), &format!("{uuid}\n")))
                }
                CmdRequest::CryptsetupLuksDumpText { device } => Ok(mock_ok(
                    &format!("cryptsetup luksDump {device}"),
                    "LUKS header information\nVersion:       \t2\n",
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                CmdRequest::BtrfsDeviceStatsJson { .. } => {
                    Ok(mock_ok("btrfs device stats", r#"{"device-stats": []}"#))
                }
                CmdRequest::BtrfsReplaceStart { .. } => {
                    Ok(mock_ok("btrfs replace start", ""))
                }
                CmdRequest::CryptsetupClose { .. } => Ok(mock_ok("cryptsetup close", "")),
                CmdRequest::BtrfsFilesystemResize { .. } => Ok(RawCommandOutput {
                    cmd: "btrfs filesystem resize".into(),
                    stdout: String::new(),
                    stderr: "ERROR: unable to resize".into(),
                    exit_status: 1,
                }),
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
    // Intent: close of old mapper must run even when the post-replace
    //   `btrfs filesystem resize` fails.
    //
    // Why it exists: a resize failure returning `?` previously skipped the
    //   best-effort cryptsetup close of the old mapper, leaving the old
    //   dm slot bound to its backing disk until the next `braid lock` or
    //   reboot. The ordering in the Live arm of cmd_replace must be
    //   close-then-resize so the close always runs.
    //
    // Scenario: live replace of disk2 -> disk3. `btrfs replace start`
    //   succeeds; `btrfs filesystem resize devid=2:max` fails (exit 1);
    //   cmd_replace must still have issued `cryptsetup close braid-disk2`
    //   before the resize error propagated out.
    fn close_runs_before_resize_on_live_replace() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let runner = ResizeFailingLoggingRunner { log: log.clone() };
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        match &result {
            Err(ReplaceError::Pool(crate::pool::PoolError::Failed(msg))) => {
                assert!(
                    msg.contains("btrfs filesystem resize failed"),
                    "expected typed PoolError::Failed carrying resize message, got: {msg}"
                );
            }
            other => panic!(
                "expected Err(ReplaceError::Pool(PoolError::Failed(..))), got: {other:?}"
            ),
        }

        let log = log.lock().unwrap();
        let close_idx = log
            .iter()
            .position(|r| matches!(
                r,
                CmdRequest::CryptsetupClose { mapper } if mapper == "braid-disk2"
            ))
            .expect(
                "cryptsetup close on braid-disk2 must be issued even when resize fails",
            );
        let resize_idx = log
            .iter()
            .position(|r| matches!(
                r,
                CmdRequest::BtrfsFilesystemResize { devid: 2, .. }
            ))
            .expect("btrfs filesystem resize on devid 2 must be issued");
        assert!(
            close_idx < resize_idx,
            "close (index {close_idx}) must run BEFORE resize (index {resize_idx}) \
             so a resize failure does not strand the old dm slot"
        );

        assert!(
            journal::load_journal(&paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
    }

    /// Runner for a missing-path replace where --old is a typo'd name absent
    /// from pool.json. probe_pool sees 1 live disk + 1 missing devid (devid 2);
    /// probe_missing_devids reports [2]. The runner is scoped narrowly so
    /// cmd_replace can reach the `build_replacement_membership` guard before
    /// touching any downstream commands.
    struct MissingPathReplaceRunner;

    impl CmdRunner2 for MissingPathReplaceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_ok(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
                     \tTotal devices 2 FS bytes used 16.17MiB\n\
                     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
                     \t*** Some devices missing\n",
                )),
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_ok(
                    &format!("cryptsetup status {mapper}"),
                    &format!("{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vda\n  mode:    read/write\n"),
                )),
                CmdRequest::CryptsetupLuksUuid { device } => Ok(mock_ok(
                    &format!("cryptsetup luksUUID {device}"),
                    "11111111-1111-1111-1111-111111111111\n",
                )),
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(mock_ok(
                    "btrfs device usage --raw",
                    "/dev/mapper/braid-disk1, ID: 1\n\
                     \tDevice size:           520093696\n\
                     \tDevice slack:                  0\n\
                     \tData,RAID1:            469762048\n\
                     \tUnallocated:            50331648\n\n\
                     <missing disk>, ID: 2\n\
                     \tDevice size:                  0\n\
                     \tDevice slack:                  0\n\
                     \tData,RAID1:            469762048\n\
                     \tUnallocated:                  0\n\n",
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
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
    // Intent: cmd_replace's missing path rejects a --old name that is absent
    //   from pool.json, with no inhibitor acquired and no journal written.
    //
    // Why it exists: resolve_replace_source only consulted btrfs state, so a
    //   typo in --old on the missing path slipped through and
    //   build_replacement_membership's HashMap::remove silently no-oped before
    //   inserting the new name. pool.json kept the orphan old entry, and the
    //   next `braid unlock` tripped DegradedRefused in mount::plan_open_pool.
    //
    // Scenario: pool has 1 live disk (disk1, devid 1) and 1 missing
    //   (devid 2). pool.json only records disk1. Operator runs
    //   `braid replace --old disk2 --missing-id 2 --new disk3=...`. The guard
    //   must fire before the inhibitor seam at the "reversible preflight
    //   before inhibitor" boundary (cli/src/replace.rs:224-229).
    fn cmd_replace_missing_path_rejects_old_name_absent_from_membership() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());

        // Seed pool.json with ONLY disk1 -- no disk2 entry. This is the typo
        // scenario: btrfs knows devid 2 is missing, but pool.json does not
        // record any member named "disk2".
        let mut m = PoolMembership::empty();
        let mut disk1 = DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into()));
        disk1.devid = Some(1);
        m.disks.insert("disk1".into(), disk1);
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let runner = MissingPathReplaceRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: Some(2),
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        assert!(
            matches!(result, Err(ReplaceError::Validation(_))),
            "expected Err(ReplaceError::Validation(_)) for --old absent from pool.json, got: {result:?}"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "validation must fire before the inhibitor seam -- a caught typo must not hold logind"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal may be written when --old is absent from pool.json"
        );
    }

    #[test]
    // Intent: live-path dry-run still shows NO soft balance step.
    // Why: live replace doesn't create single-profile chunks -- no degraded mode involved.
    // Scenario: swapping a working drive for a bigger one.
    fn dry_run_live_path_no_soft_balance() {
        let _config = make_replace_config();
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Live {
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
        })
        .unwrap();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("-dconvert=raid1,soft")),
            "live path should NOT show soft balance, got: {descriptions:?}"
        );
    }

    #[test]
    // Intent: dry-run for fresh-disk live replace shows full LUKS init + replace commands.
    // Why: verifies header backup and keyfile enrollment appear in dry-run.
    // Scenario: replacing disk2 with a fresh disk3, with keyfile enrollment.
    fn dry_run_render_fresh_disk_live_replace_with_keyfile() {
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Live {
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };
        let kf = Path::new("/mnt/usb/braid.key");
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: Some(kf),
        })
        .unwrap();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // Steps: LUKS format, header backup, LUKS open, keyfile enroll,
        //        replace start, close old, resize = 7 steps x 2 lines each = 14
        assert_eq!(lines.len(), 14, "expected 14 lines, got:\n{output}");

        // LUKS format
        assert!(lines[0].contains("[destructive]"));
        assert!(lines[1].contains("$ cryptsetup luksFormat"));
        assert!(lines[1].contains("--label braid-disk3"));

        // Header backup
        assert!(lines[2].contains("LUKS header backup"));
        assert!(lines[3].contains("$ cryptsetup luksHeaderBackup"));

        // LUKS open
        assert!(lines[4].contains("LUKS open"));
        assert!(lines[5].contains("$ cryptsetup open --type luks"));

        // Keyfile enrollment
        assert!(lines[6].contains("enroll keyfile"));
        assert!(lines[7].contains("$ cryptsetup luksAddKey"));
        assert!(lines[7].contains("/mnt/usb/braid.key"));

        // Replace start
        assert!(lines[8].contains("[long       ]"));
        assert!(lines[9].contains("$ btrfs replace start"));

        // Close old mapper (before resize: a resize failure must not strand
        // the old dm slot)
        assert!(lines[10].contains("cryptsetup close"));
        assert_eq!(lines[11], "               $ cryptsetup close braid-disk2");

        // Resize
        assert!(lines[12].contains("btrfs filesystem resize"));
    }

    #[test]
    // Intent: dry-run for a fresh-disk missing-path replace renders the
    //   expected step ordering: LUKS init of the new disk, then
    //   `btrfs replace start`, then `btrfs filesystem resize`, then the
    //   post-replace soft balance. No `cryptsetup close` step -- the missing
    //   path has no old mapper.
    //
    // Why it exists: the live-path render order is pinned by
    //   `dry_run_render_fresh_disk_live_replace_with_keyfile`, but the
    //   missing path only had presence/absence coverage. A regression that
    //   moved the soft balance before `btrfs replace start`/`resize` would
    //   ship broken dry-run output without tripping the existing test. This
    //   test fails if the order breaks even when every substring is still
    //   present.
    //
    // Scenario: operator replaces a missing disk with a fresh disk3. The
    //   pool has 2 devices and this clears the last missing one, so the
    //   soft-balance tail appears.
    fn dry_run_render_missing_path_ordering() {
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = compile_replace_steps(&ReplaceStepsInput {
            new_name: "disk3",
            new_by_id: &ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: true,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
        })
        .unwrap();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // Substring order: LUKS format -> header backup -> LUKS open ->
        // btrfs replace start -> btrfs filesystem resize -> soft balance.
        // Pin the order by resolving each substring to an index and
        // asserting strict monotonic increase.
        let find = |needle: &str| -> usize {
            lines
                .iter()
                .position(|l| l.contains(needle))
                .unwrap_or_else(|| panic!("expected '{needle}' in dry-run output:\n{output}"))
        };
        let luks_format = find("$ cryptsetup luksFormat");
        let header_backup = find("$ cryptsetup luksHeaderBackup");
        let luks_open = find("$ cryptsetup open --type luks");
        let replace_start = find("$ btrfs replace start");
        let resize = find("btrfs filesystem resize");
        let soft_balance = find("-dconvert=raid1,soft");

        assert!(
            luks_format < header_backup
                && header_backup < luks_open
                && luks_open < replace_start
                && replace_start < resize
                && resize < soft_balance,
            "missing-path dry-run step ordering violated \
             (format={luks_format}, header_backup={header_backup}, \
             luks_open={luks_open}, replace_start={replace_start}, \
             resize={resize}, soft_balance={soft_balance}):\n{output}"
        );

        // Missing path has no old mapper, so no cryptsetup close anywhere.
        assert!(
            !output.contains("cryptsetup close"),
            "missing path must not render a cryptsetup close step:\n{output}"
        );
    }

    /// Runner for a replace where the new disk is already LUKS-formatted but
    /// the mapper is closed (PresentLuks { mapper_open: false }) and the
    /// supplied passphrase is wrong: CryptsetupTestPassphrase on the new
    /// disk's by_id returns exit 2 (EPERM). Everything else mirrors
    /// FailingReplaceRunner except that braid-disk3's mapper is inactive,
    /// so probe_config_disk reports mapper_open: false.
    struct ClosedLuksWrongPassRunner;

    impl CmdRunner2 for ClosedLuksWrongPassRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_ok(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                )),
                CmdRequest::CryptsetupStatus { mapper } => match mapper.as_str() {
                    "braid-disk1" => Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!("{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"),
                    )),
                    "braid-disk2" => Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!("{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdc\n  mode:    read/write\n"),
                    )),
                    // new disk's mapper is closed -- this is the key
                    // difference vs FailingReplaceRunner.
                    "braid-disk3" => Ok(RawCommandOutput {
                        cmd: format!("cryptsetup status {mapper}"),
                        stdout: String::new(),
                        stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
                        exit_status: 4,
                    }),
                    _ => Err(CmdError::MissingMock),
                },
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                            "11111111-1111-1111-1111-111111111111"
                        }
                        "/dev/vdc" | "/dev/disk/by-id/virtio-disk2" => {
                            "22222222-2222-2222-2222-222222222222"
                        }
                        "/dev/disk/by-id/virtio-disk3" => {
                            "33333333-3333-3333-3333-333333333333"
                        }
                        _ => "99999999-9999-9999-9999-999999999999",
                    };
                    Ok(mock_ok(&format!("cryptsetup luksUUID {device}"), &format!("{uuid}\n")))
                }
                CmdRequest::CryptsetupLuksDumpText { device } => Ok(mock_ok(
                    &format!("cryptsetup luksDump {device}"),
                    "LUKS header information\nVersion:       \t2\n",
                )),
                CmdRequest::CryptsetupTestPassphrase { device } => {
                    if device == "/dev/disk/by-id/virtio-disk3" {
                        Ok(RawCommandOutput {
                            cmd: format!("cryptsetup luksOpen --test-passphrase {device}"),
                            stdout: String::new(),
                            stderr: "No key available with this passphrase.\n".into(),
                            exit_status: 2,
                        })
                    } else {
                        Err(CmdError::MissingMock)
                    }
                }
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
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
    // Intent: wrong passphrase on a PresentLuks { mapper_open: false } new
    //   disk must fail before the journal is written.
    //
    // Why it exists: the closed-LUKS replacement path previously deferred
    //   passphrase verification to the post-journal ensure_luks_open call,
    //   so a wrong passphrase stranded pending-op.json and forced the user
    //   into braid recover for a pure preflight failure -- contradicting
    //   decision 019's "logind failure aborts cleanly without stranding
    //   pending-op.json...for a preflight failure" guidance. Re-introducing
    //   that ordering must flip this assertion.
    //
    // Scenario: operator runs `braid replace --old disk2 --new disk3=...`
    //   where disk3 is already LUKS-formatted (mapper closed) and types the
    //   wrong passphrase. The command must abort cleanly: no journal, no
    //   inhibitor acquired, Err(Validation).
    fn wrong_passphrase_on_closed_luks_new_disk_does_not_write_journal() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"wrong-passphrase\n").unwrap();

        // Only the new disk's by_id exists. /dev/mapper/braid-disk3 is
        // absent because the mapper is closed.
        let fs = ReplaceMockFs(vec!["/dev/disk/by-id/virtio-disk3".into()]);

        let runner = ClosedLuksWrongPassRunner;
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        assert!(
            matches!(result, Err(ReplaceError::Validation(_))),
            "expected Err(ReplaceError::Validation(_)) for wrong passphrase on a closed-LUKS new disk, got: {result:?}"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "pending-op.json must not be written -- wrong passphrase is a reversible preflight failure"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "sleep inhibitor must not be acquired before passphrase verification"
        );
    }

    /// Recording wrapper around FailingReplaceRunner that logs every
    /// CmdRequest before dispatching. Used by the mapper_open: true
    /// negative-coverage test to assert CryptsetupTestPassphrase and
    /// CryptsetupLuksOpen against the new disk are never issued.
    struct RecordingReplaceRunner {
        inner: FailingReplaceRunner,
        log: std::sync::Arc<std::sync::Mutex<Vec<CmdRequest>>>,
    }

    impl RecordingReplaceRunner {
        fn new() -> Self {
            Self {
                inner: FailingReplaceRunner,
                log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            }
        }
    }

    impl CmdRunner2 for RecordingReplaceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            self.inner.run(request)
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            self.inner.run_with_stdin(request, stdin)
        }
    }

    #[test]
    // Intent: when the new disk is PresentLuks { mapper_open: true },
    //   cmd_replace must not issue CryptsetupTestPassphrase or
    //   CryptsetupLuksOpen against that disk's by_id.
    //
    // Why it exists: the pre-journal passphrase check added for the
    //   closed-LUKS branch targets only mapper_open: false. A future
    //   refactor that accidentally broadens it to all PresentLuks -- or
    //   re-adds a post-journal ensure_luks_open on the already-open path --
    //   would surface an unnecessary credential demand or second open.
    //   This test pins the no-op shape of the open-mapper branch by
    //   inspecting a recorded call log directly, so the assertion is
    //   insensitive to error-plumbing refactors.
    //
    // Scenario: a previous replace/add opened /dev/mapper/braid-disk3 but
    //   never added it to the pool (e.g. crash). Operator retries
    //   `braid replace --old disk2 --new disk3=...`; the command picks up
    //   the already-open mapper and proceeds to btrfs replace start without
    //   a second LUKS interaction on the new disk.
    fn mapper_open_true_does_not_verify_or_open_new_disk_luks() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        m.disks.insert(
            "disk2".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let runner = RecordingReplaceRunner::new();
        let log = runner.log.clone();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        // The inner runner forces BtrfsReplaceStart to fail (exit 1), so
        // cmd_replace must return a Pool error -- this confirms the flow
        // reached the btrfs phase rather than stopping short, which is a
        // prerequisite for the zero-counts below to mean "not called"
        // instead of "test aborted early".
        assert!(
            matches!(result, Err(ReplaceError::Pool(_))),
            "expected Err(ReplaceError::Pool(_)) from btrfs replace start failure, got: {result:?}"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_some(),
            "journal must be written -- the failure is post-journal"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the way in"
        );

        let log = log.lock().unwrap();
        let new_by_id = "/dev/disk/by-id/virtio-disk3";

        let test_passphrase_calls = log
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupTestPassphrase { device } if device == new_by_id))
            .count();
        assert_eq!(
            test_passphrase_calls, 0,
            "mapper_open: true must not trigger CryptsetupTestPassphrase on the new disk"
        );

        let open_calls = log
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupLuksOpen { device, .. } if device == new_by_id))
            .count();
        assert_eq!(
            open_calls, 0,
            "mapper_open: true must not trigger CryptsetupLuksOpen on the new disk"
        );
    }

    /// Runner for a successful missing-path replace. Drives cmd_replace all
    /// the way through the replace -> resize -> soft-balance sequence.
    ///
    /// Stateful: `BtrfsFilesystemShow` returns a degraded layout (disk1 live,
    /// devid 2 missing) until `BtrfsReplaceStart` is issued, then flips to a
    /// healthy 2-device layout (disk1 + disk3) so the second `probe_pool`
    /// inside `maybe_restore_raid1` sees `missing_count == 0` with
    /// `devices.len() >= 2` -- the minimal condition set for the soft
    /// balance to fire.
    struct MissingPathSuccessRunner {
        log: std::sync::Arc<std::sync::Mutex<Vec<CmdRequest>>>,
        replace_done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl CmdRunner2 for MissingPathSuccessRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());
            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_ok(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs","options":"rw,relatime"}]}"#,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    let show = if self.replace_done.load(std::sync::atomic::Ordering::Relaxed) {
                        // post-replace: disk1 + disk3, no missing
                        "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
                         \tTotal devices 2 FS bytes used 16.17MiB\n\
                         \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
                         \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n"
                    } else {
                        // pre-replace: disk1 live, devid 2 missing
                        "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
                         \tTotal devices 2 FS bytes used 16.17MiB\n\
                         \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
                         \t*** Some devices missing\n"
                    };
                    Ok(mock_ok(
                        &format!("btrfs filesystem show {mount_point}"),
                        show,
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => {
                    // disk3's mapper is already open: skips the LUKS
                    // format/open/enroll init steps so the test focuses on
                    // the shared replace spine + missing-path tail.
                    let dev = match mapper.as_str() {
                        "braid-disk1" => "/dev/vdb",
                        "braid-disk3" => "/dev/vdd",
                        _ => return Err(CmdError::MissingMock),
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!("{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"),
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                            "11111111-1111-1111-1111-111111111111"
                        }
                        "/dev/vdd" | "/dev/disk/by-id/virtio-disk3" => {
                            "33333333-3333-3333-3333-333333333333"
                        }
                        _ => return Err(CmdError::MissingMock),
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                    ))
                }
                CmdRequest::CryptsetupLuksDumpText { device } => Ok(mock_ok(
                    &format!("cryptsetup luksDump {device}"),
                    "LUKS header information\nVersion:       \t2\n",
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(mock_ok(
                    "btrfs device usage --raw",
                    "/dev/mapper/braid-disk1, ID: 1\n\
                     \tDevice size:           520093696\n\
                     \tDevice slack:                  0\n\
                     \tData,RAID1:            469762048\n\
                     \tUnallocated:            50331648\n\n\
                     <missing disk>, ID: 2\n\
                     \tDevice size:                  0\n\
                     \tDevice slack:                  0\n\
                     \tData,RAID1:            469762048\n\
                     \tUnallocated:                  0\n\n",
                )),
                CmdRequest::BtrfsReplaceStart { .. } => {
                    self.replace_done
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                    Ok(mock_ok("btrfs replace start", ""))
                }
                CmdRequest::BtrfsFilesystemResize { .. } => {
                    Ok(mock_ok("btrfs filesystem resize", ""))
                }
                CmdRequest::BtrfsBalanceRaid1Soft { .. } => {
                    Ok(mock_ok("btrfs balance raid1 soft", ""))
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
    // Intent: on the missing path, `cmd_replace` issues the soft-balance
    //   follow-up after the replace-start + resize sequence, and does not
    //   close any old LUKS mapper (there is none).
    //
    // Why it exists: the missing arm of `cmd_replace` delegates the
    //   post-replace redundancy restoration to `crate::pool::maybe_restore_raid1`.
    //   An end-to-end VM test of this is infeasible -- the only way to
    //   create the single-profile chunks the soft balance is meant to
    //   clean up is to write while degraded, and that same state prevents
    //   `btrfs replace start` from succeeding (kernel returns ENOSPC from
    //   `inc_block_group_ro` during staging; see
    //   `reference/linux/fs/btrfs/block-group.c:1366`). Without a wiring
    //   test at this layer, a refactor that dropped the
    //   `maybe_restore_raid1` call on the missing path -- or reordered it
    //   before the replace/resize -- would ship undetected.
    //
    // Scenario: pool has disk1 live + devid 2 missing. Operator runs
    //   `braid replace --old disk2 --missing-id 2 --new disk3=...` with an
    //   already-LUKS-open disk3 (PresentLuks { mapper_open: true }), which
    //   skips the LUKS init steps and focuses the test on the shared
    //   replace spine + missing-path tail. The runner reports degraded
    //   btrfs state until `BtrfsReplaceStart` is issued, then flips to a
    //   healthy 2-device layout so `maybe_restore_raid1`'s probe sees
    //   `missing_count == 0` with `devices.len() >= 2` and fires the soft
    //   balance.
    fn cmd_replace_missing_path_runs_soft_balance_after_replace_and_resize() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".into(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        // disk2 is the missing entry being replaced. Record its devid so
        // `build_replacement_membership` matches the --missing-id argument
        // to the right pool.json row.
        let mut disk2 = DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into()));
        disk2.devid = Some(2);
        m.disks.insert("disk2".into(), disk2);
        membership::save_membership(&m, &paths).unwrap();

        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let pass_path = config_tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        // disk3 is already LUKS-open (PresentLuks { mapper_open: true }),
        // so cmd_replace skips LUKS format/open/enroll. That keeps the test
        // focused on the replace+resize+balance sequence.
        let fs = ReplaceMockFs(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);

        let log = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let replace_done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let runner = MissingPathSuccessRunner {
            log: log.clone(),
            replace_done: replace_done.clone(),
        };
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_replace(
            &runner,
            &fs,
            &ReplaceParams {
                config_path: &config_path,
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: Some(2),
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        assert!(
            matches!(result, Ok(())),
            "expected Ok(()) from successful missing-path replace, got: {result:?}"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "pending-op.json must be cleared on successful completion"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the way in"
        );

        let log = log.lock().unwrap();
        let replace_idx = log
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsReplaceStart { devid: 2, .. }))
            .expect("btrfs replace start on devid 2 must be issued");
        let resize_idx = log
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsFilesystemResize { devid: 2, .. }))
            .expect("btrfs filesystem resize on devid 2 must be issued");
        let balance_idx = log
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsBalanceRaid1Soft { .. }))
            .expect(
                "btrfs soft balance must be issued after replace+resize on missing path \
                 -- maybe_restore_raid1 is part of the `replace` contract per \
                 docs/principles.md",
            );
        assert!(
            replace_idx < resize_idx && resize_idx < balance_idx,
            "missing-path command order violated \
             (replace={replace_idx}, resize={resize_idx}, balance={balance_idx}) -- \
             soft balance must run AFTER the replace-start and resize"
        );

        let close_calls = log
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupClose { .. }))
            .count();
        assert_eq!(
            close_calls, 0,
            "missing path has no old LUKS mapper to close -- CryptsetupClose must not be issued"
        );
    }
}
