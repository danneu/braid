use crate::alert;
use crate::cmd::{CmdError, CmdRequest, CommandRunner, Step};
use crate::config::{Config, config_read, mapper_name, name_from_mapper};
use crate::confirm;
use crate::credential_verify::{
    Credential, CredentialVerifyError, CredentialVerifyTarget, verify_credential_for_targets,
};
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal;
use crate::luks::{
    OpenOutcome, PassphraseReader, backup_luks_header_post_mutation, ensure_luks_open,
    format_keyfile_asymmetry_warning, format_keyfile_enrollment_probe_failure, luks_format,
    probe_pool_keyfile_enrollment, read_passphrase_with,
};
use crate::mapper_close::close_mapper_with_retry;
use crate::membership::{self, PoolMembership};
use crate::parse::btrfs_filesystem_show::{DeviceBtrfsProbe, classify_btrfs_probe};
use crate::parse::parse_btrfs_filesystem_show;
use crate::pool::{
    pool_add_device, pool_balance_raid1, pool_bootstrap_mount, pool_bootstrap_mount_raid1,
};
use crate::preflight;
use crate::preview::{self, PerDiskStyle, PlanFailure, Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{Filesystem, ProbeError, probe_config_disk, probe_pool};
use crate::progress::ProgressOutput;
use crate::progress::RealSleeper;
use crate::state_paths::StatePaths;
use crate::status_tag::{StatusTag, color_enabled_for_stderr, emit_status, status_line};
use crate::types::*;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error("{0}")]
    Validation(String),
    #[error(
        "pool was modified, but acked-stats cleanup failed at {stage}: {detail}\n\
         health alert baselines may be stale -- run `rm /var/lib/braid/acked-stats.json` \
         before trusting `braid monitor`."
    )]
    AckCleanupFailed { stage: &'static str, detail: String },
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

/// Validate the preconditions for adding a PresentLuks disk.
/// Checks the cached LUKS label and mounted pool state.
/// No side effects -- works on the raw device, no mapper required.
fn validate_braid_preconditions(
    name: &str,
    device: &str,
    label: Option<&str>,
    pool: &PoolState,
) -> Result<(), AddError> {
    let expected_label = format!("braid-{name}");
    if label != Some(expected_label.as_str()) {
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
        let color_enabled = color_enabled_for_stderr();
        let sleeper = RealSleeper;
        for mapper in self.mappers.iter().rev() {
            let label = mapper.strip_prefix("braid-").unwrap_or(mapper);
            emit_status(&status_line(
                StatusTag::Wait,
                color_enabled,
                &format!("disk {label}: locking (cleanup)..."),
            ));
            match close_mapper_with_retry(self.runner, &sleeper, mapper, color_enabled) {
                Ok(()) => {
                    emit_status(&status_line(
                        StatusTag::Ok,
                        color_enabled,
                        &format!("disk {label}: locked (cleanup)"),
                    ));
                }
                Err(e) => {
                    emit_status(&status_line(
                        StatusTag::Warn,
                        color_enabled,
                        &format!("disk {label}: lock failed (cleanup, {e})"),
                    ));
                }
            }
        }
    }
}

struct PoolAddExecutionTarget {
    mapper_path: String,
    force: bool,
}

#[derive(Debug, Clone)]
struct AddConfirmDiskPlan {
    name: String,
    by_id: ByIdPath,
    needs_luks_format: bool,
}

#[derive(Debug, Clone)]
struct AddCredentialPrelude {
    confirm_disks: Vec<AddConfirmDiskPlan>,
    confirm_new: bool,
    verify_targets: Vec<CredentialVerifyTarget>,
    pool_target_count: usize,
}

#[derive(Debug, Clone)]
struct FreshLuksTarget {
    name: String,
    by_id: ByIdPath,
    mapper_name: String,
    mapper_path: String,
    luks_label: String,
    luks_format_extra_opts: Vec<String>,
    enroll_key_file: Option<PathBuf>,
    header_backup_path: PathBuf,
}

#[derive(Debug, Clone)]
struct RecoverableBraidTarget {
    name: String,
    by_id: ByIdPath,
    mapper_name: String,
    mapper_path: String,
    luks_uuid: LuksUuid,
    verified_pool_fsid: String,
    /// Keyfile to enroll into LUKS slot 1 if `add --enroll DIR` was
    /// passed against this target and the per-disk planner classified
    /// the disk as `NeedsEnroll`. `None` means either no `--enroll`
    /// flag, or the disk's slot 1 already authenticates with the
    /// supplied keyfile (idempotent skip).
    enroll_key_file: Option<PathBuf>,
    /// Where the post-enrollment LUKS header backup lands, computed at
    /// plan time so render_steps does not need access to `paths`.
    /// Mirrors `FreshLuksTarget::header_backup_path`. Unused when
    /// `enroll_key_file` is `None`.
    header_backup_path: PathBuf,
}

#[derive(Debug, Clone)]
struct ClosedPresentLuksCandidate {
    name: String,
    by_id: ByIdPath,
    mapper_name: String,
    mapper_path: String,
    luks_uuid: LuksUuid,
    /// Same semantics as `RecoverableBraidTarget::enroll_key_file`.
    /// Threaded through Pass-1 verification: when the closed disk's
    /// identity is verified at execution time, this keyfile is
    /// promoted into the runtime `RecoverableBraidTarget` and
    /// journaled, so crash-recovery can replay enrollment.
    enroll_key_file: Option<PathBuf>,
    /// See `RecoverableBraidTarget::header_backup_path`.
    header_backup_path: PathBuf,
}

#[derive(Debug, Clone)]
enum AddTargetWork {
    Fresh(FreshLuksTarget),
    OpenRecoverable(RecoverableBraidTarget),
    ClosedPresentLuks(ClosedPresentLuksCandidate),
}

impl AddTargetWork {
    fn mapper_path(&self) -> &str {
        match self {
            AddTargetWork::Fresh(target) => &target.mapper_path,
            AddTargetWork::OpenRecoverable(target) => &target.mapper_path,
            AddTargetWork::ClosedPresentLuks(target) => &target.mapper_path,
        }
    }
}

#[derive(Debug, Clone)]
struct AddWorkPlan {
    prelude: AddCredentialPrelude,
    targets: Vec<AddTargetWork>,
    initial_journal_targets: BTreeMap<String, journal::AddJournalTarget>,
    mount_point: MountPoint,
    pool_was_mounted: bool,
    existing_pool_device_count: usize,
}

impl AddWorkPlan {
    fn is_noop(&self) -> bool {
        self.targets.is_empty()
    }

    fn target_count(&self) -> usize {
        self.targets.len()
    }

    fn mapper_paths(&self) -> Vec<String> {
        self.targets
            .iter()
            .map(|target| target.mapper_path().to_owned())
            .collect()
    }

    fn render_steps(&self) -> Vec<Step> {
        let mut steps = Vec::new();

        for target in &self.targets {
            match target {
                AddTargetWork::Fresh(target) => {
                    steps.push(Step {
                        risk: "destructive",
                        description: format!("LUKS format {}", target.by_id),
                        commands: vec![CmdRequest::CryptsetupLuksFormat {
                            device: target.by_id.0.clone(),
                            extra_opts: target.luks_format_extra_opts.clone(),
                        }],
                    });
                    if let Some(kf) = &target.enroll_key_file {
                        steps.push(Step {
                            risk: "safe",
                            description: format!(
                                "enroll keyfile → LUKS slot 1 on {}",
                                target.by_id
                            ),
                            commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
                                device: target.by_id.0.clone(),
                                key_file_path: kf.display().to_string(),
                            }],
                        });
                    }
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "LUKS header backup → {}",
                            target.header_backup_path.display()
                        ),
                        commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
                            device: target.by_id.0.clone(),
                            backup_path: target.header_backup_path.display().to_string(),
                        }],
                    });
                    steps.push(Step {
                        risk: "safe",
                        description: format!("LUKS open → {}", target.mapper_name),
                        commands: vec![CmdRequest::CryptsetupLuksOpen {
                            device: target.by_id.0.clone(),
                            mapper: target.mapper_name.clone(),
                        }],
                    });
                }
                AddTargetWork::OpenRecoverable(target) => {
                    if let Some(kf) = &target.enroll_key_file {
                        push_returned_disk_enrollment_steps(
                            &mut steps,
                            &target.by_id,
                            kf,
                            &target.header_backup_path,
                        );
                    }
                    steps.push(forced_returned_device_add_step(
                        &target.mapper_path,
                        &self.mount_point,
                        "verified returned disk",
                    ));
                }
                AddTargetWork::ClosedPresentLuks(target) => {
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "LUKS open + identity verification at execution time → {}",
                            target.mapper_name
                        ),
                        commands: vec![CmdRequest::CryptsetupLuksOpen {
                            device: target.by_id.0.clone(),
                            mapper: target.mapper_name.clone(),
                        }],
                    });
                    if let Some(kf) = &target.enroll_key_file {
                        push_returned_disk_enrollment_steps(
                            &mut steps,
                            &target.by_id,
                            kf,
                            &target.header_backup_path,
                        );
                    }
                    steps.push(forced_returned_device_add_step(
                        &target.mapper_path,
                        &self.mount_point,
                        "if verified returned disk",
                    ));
                }
            }
        }

        if self.is_noop() {
            return steps;
        }

        if !self.pool_was_mounted {
            let mapper_paths = self.mapper_paths();
            if mapper_paths.len() >= 2 {
                steps.push(Step {
                    risk: "safe",
                    description: format!("mkfs.btrfs RAID1 {}", mapper_paths.join(" ")),
                    commands: vec![CmdRequest::MkfsBtrfsRaid1 {
                        devices: mapper_paths.clone(),
                    }],
                });
                steps.push(Step {
                    risk: "safe",
                    description: format!("mount → {}", self.mount_point),
                    commands: vec![CmdRequest::Mount {
                        device: mapper_paths[0].clone(),
                        mount_point: self.mount_point.clone(),
                    }],
                });
            } else {
                let mapper_path = mapper_paths
                    .first()
                    .expect("non-noop bootstrap plan must have a mapper path")
                    .clone();
                steps.push(Step {
                    risk: "safe",
                    description: format!("mkfs.btrfs {mapper_path}"),
                    commands: vec![CmdRequest::MkfsBtrfs {
                        device: mapper_path.clone(),
                    }],
                });
                steps.push(Step {
                    risk: "safe",
                    description: format!("mount → {}", self.mount_point),
                    commands: vec![CmdRequest::Mount {
                        device: mapper_path,
                        mount_point: self.mount_point.clone(),
                    }],
                });
            }
        } else {
            for target in &self.targets {
                if let AddTargetWork::Fresh(target) = target {
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "btrfs device add {} {}",
                            target.mapper_path, self.mount_point
                        ),
                        commands: vec![CmdRequest::BtrfsDeviceAdd {
                            device: target.mapper_path.clone(),
                            mount_point: self.mount_point.clone(),
                            force: false,
                        }],
                    });
                }
            }
            let total_after = self.existing_pool_device_count + self.target_count();
            if total_after >= 2 {
                steps.push(Step {
                    risk: "long",
                    description: "btrfs balance to RAID1".into(),
                    commands: vec![CmdRequest::BtrfsBalanceRaid1 {
                        mount_point: self.mount_point.clone(),
                    }],
                });
            }
        }

        steps
    }
}

/// Render the `cryptsetup luksAddKey` + `cryptsetup luksHeaderBackup`
/// pair for a returned-disk add target carrying `--enroll DIR`. Order
/// matches the FreshLuks render: addKey before backup so slot 1 is
/// captured in the post-mutation header backup.
fn push_returned_disk_enrollment_steps(
    steps: &mut Vec<Step>,
    by_id: &ByIdPath,
    key_file: &Path,
    header_backup_path: &Path,
) {
    steps.push(Step {
        risk: "safe",
        description: format!("enroll keyfile → LUKS slot 1 on {}", by_id),
        commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
            device: by_id.0.clone(),
            key_file_path: key_file.display().to_string(),
        }],
    });
    steps.push(Step {
        risk: "safe",
        description: format!("LUKS header backup → {}", header_backup_path.display()),
        commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
            device: by_id.0.clone(),
            backup_path: header_backup_path.display().to_string(),
        }],
    });
}

fn forced_returned_device_add_step(
    mapper_path: &str,
    mount_point: &MountPoint,
    condition: &str,
) -> Step {
    Step {
        risk: "safe",
        description: format!("btrfs device add -f {mapper_path} {mount_point} ({condition})"),
        commands: vec![
            CmdRequest::BtrfsDeviceScanForget {
                devices: vec![mapper_path.to_owned()],
            },
            CmdRequest::WipefsBtrfs {
                device: mapper_path.to_owned(),
            },
            CmdRequest::BtrfsDeviceAdd {
                device: mapper_path.to_owned(),
                mount_point: mount_point.clone(),
                force: true,
            },
        ],
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
    pub luks_format_extra_opts: &'a [String],
    pub progress: ProgressOutput,
    pub paths: &'a StatePaths,
    /// Seam for acquiring a logind sleep inhibitor before the irreversible
    /// portion of the add. Production passes `&RealSleepInhibitor`;
    /// unit tests pass `&RecordingInhibitor` to avoid spawning subprocesses.
    pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
    /// Seam for reading a LUKS passphrase from the TTY. Production
    /// passes `&RealTty`; tests pass a scripted reader so the
    /// bootstrap-confirm path is observable at the `cmd_add` layer.
    pub passphrase_reader: &'a dyn PassphraseReader,
}

/// Returns the missing-devices warning body (no legacy `warning:` prefix).
/// Both dry-run (`Preview::render` on stdout) and real-run
/// (`preview::render_notes_for_stderr` on stderr) wrap this in
/// `PreviewNote::Warn` and render it as the canonical `[warn] <body>`
/// -- one contract for both modes.
fn format_add_missing_devices_warning(missing_count: u64) -> String {
    format!(
        "pool has {} missing device{}. \
         Consider repairing with `braid replace --missing-id <devid>` first. \
         Use `braid status` to see device IDs.",
        missing_count,
        if missing_count == 1 { "" } else { "s" }
    )
}

/// Labels the disk set for no-op / done messages. Single-disk returns the
/// bare name; multi-disk joins names with `, `.
fn format_disk_name_list(names: &[String]) -> String {
    if names.len() == 1 {
        names[0].clone()
    } else {
        names.join(", ")
    }
}

/// Returns the no-op "nothing to do" message, without any channel-specific
/// formatting. Shared by the dry-run `PreviewNote::Info` and the real-run
/// stderr `eprintln!` so both paths see byte-identical wording.
fn format_add_noop(names: &[String]) -> String {
    format!(
        "Nothing to do -- {} already in pool.",
        format_disk_name_list(names)
    )
}

fn format_add_done(names: &[String]) -> String {
    let verb = if names.len() == 1 { "is" } else { "are" };
    format!(
        "Done. {} {verb} now part of the pool.",
        format_disk_name_list(names)
    )
}

fn devid_for_mapper_path(pool: &PoolState, mapper_path: &str) -> Option<u64> {
    let mapper = mapper_path
        .strip_prefix("/dev/mapper/")
        .unwrap_or(mapper_path);
    pool.devices
        .iter()
        .find(|device| device.mapper.0 == mapper)
        .map(|device| device.devid)
}

/// Dry-run preview source of truth for `braid add` plus the execute
/// inputs pre-computed during planning. `preview()` renders accumulated
/// notes plus steps from the semantic work plan; `execute()` renders
/// the accumulated notes to stderr through
/// `preview::render_notes_for_stderr` before any mutation. Warn notes
/// use canonical `[warn] <body>` wording and Info notes render bare.
pub struct AddPlan {
    pub notes: Vec<PreviewNote>,
    work_plan: AddWorkPlan,
    pub config: Config,
    pub parsed: Vec<(String, ByIdPath)>,
    pub names: Vec<String>,
    pub by_ids: Vec<ByIdPath>,
    pub probed: Vec<ConfigDisk>,
    pub pool: PoolState,
    pub pool_membership: PoolMembership,
}

impl AddPlan {
    /// Real-run and failure-path stderr for `add` use `Bracketed` per-disk
    /// style to match other note-carrying commands; note that `add` does not
    /// produce `PerDisk` notes in PR 7, so this constant exists only to
    /// satisfy the uniform stderr-note contract.
    pub const STDERR_STYLE: PerDiskStyle = PerDiskStyle::Bracketed;

    pub fn preview(&self) -> Preview {
        Preview {
            completeness: PreviewCompleteness::Complete,
            notes: self.notes.clone(),
            steps: self.work_plan.render_steps(),
        }
    }

    pub fn execute<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
        self,
        runner: &R,
        fs: &F,
        params: &AddParams<'_>,
    ) -> Result<(), AddError> {
        let color_enabled = color_enabled_for_stderr();
        // Render accumulated notes to stderr BEFORE any mutation via
        // the shared renderer. Warn notes emit as the canonical
        // `[warn] <body>` (same as dry-run stdout); the no-op Info
        // note (when steps are empty) emits as the bare noop line.
        // `cmd_add`'s preserved-context Err branch pipes `PlanFailure::notes`
        // through the same helper, so success, failure, and dry-run
        // stdout share one render contract for these notes.
        preview::emit_notes_to_stderr(&self.notes, Self::STDERR_STYLE);

        // No-op early-return: if the plan has zero steps, the Info
        // note emitted above is the whole user-visible output for
        // this add. Must return BEFORE inhibitor acquisition and
        // BEFORE journal write (pinned by `no_journal_on_noop_add`).
        if self.work_plan.is_noop() {
            return Ok(());
        }

        // Confirmation -- show device details for sanity-check
        if !params.yes {
            let confirm_disks: Vec<AddConfirmDisk> = self
                .work_plan
                .prelude
                .confirm_disks
                .iter()
                .map(|disk| {
                    let hw = confirm::query_disk_hw_info(runner, &disk.by_id.0);
                    AddConfirmDisk {
                        name: disk.name.as_str(),
                        by_id: &disk.by_id.0,
                        hw,
                        needs_luks_format: disk.needs_luks_format,
                    }
                })
                .collect();
            eprintln!("{}", format_add_confirm(&confirm_disks));
            confirm::confirm_yes().map_err(AddError::Validation)?;
        }

        // Confirm the new passphrase iff this add will `luks_format` without
        // a live keyslot to verify against. The planner records that gate so
        // preview and execution agree about whether fresh work exists.
        let passphrase = read_passphrase_with(
            params.passphrase_file,
            params.passphrase_stdin,
            self.work_plan.prelude.confirm_new,
            params.passphrase_reader,
        )?;

        let credential_targets = &self.work_plan.prelude.verify_targets;
        if !credential_targets.is_empty() {
            match verify_credential_for_targets(
                runner,
                credential_targets,
                Credential::Passphrase(&passphrase),
                color_enabled,
                |line| eprint!("{line}"),
            ) {
                Ok(()) => {}
                Err(CredentialVerifyError::Rejected { target }) => {
                    let target_idx = credential_targets
                        .iter()
                        .position(|t| t == &target)
                        .expect("rejected target should come from target list");
                    return Err(AddError::Validation(
                        if target_idx < self.work_plan.prelude.pool_target_count {
                            format!(
                                "passphrase does not match existing pool member '{}'. \
                             All disks must use the same passphrase.",
                                target.name
                            )
                        } else {
                            format!(
                                "passphrase rejected by candidate disk '{}' ({})",
                                target.name, target.device
                            )
                        },
                    ));
                }
                Err(CredentialVerifyError::Luks { source, .. }) => {
                    return Err(AddError::Luks(source));
                }
            }
        }

        // Pass 1: execute deferred closed-PresentLuks identity checks before
        // any irreversible operation. Open recoverable targets were already
        // verified during planning and live in the initial journal target set.
        // Guard closes any mappers we opened for FSID verification if validation fails.
        let mut luks_guard = LuksCleanupGuard::new(runner);
        let mut needs_pool_add: Vec<PoolAddExecutionTarget> = Vec::new();
        let mut journal_targets = self.work_plan.initial_journal_targets.clone();

        for target in &self.work_plan.targets {
            if let AddTargetWork::OpenRecoverable(target) = target {
                eprintln!(
                    "note: braid-labeled disk '{}' verified as pool member. \
                     Completing recovery add.",
                    target.name
                );
                needs_pool_add.push(PoolAddExecutionTarget {
                    mapper_path: target.mapper_path.clone(),
                    force: true,
                });
            }
        }

        for target in &self.work_plan.targets {
            let AddTargetWork::ClosedPresentLuks(target) = target else {
                continue;
            };
            emit_status(&status_line(
                StatusTag::Wait,
                color_enabled,
                &format!("disk {}: unlocking...", target.name),
            ));
            if ensure_luks_open(runner, &target.name, &target.by_id, &passphrase)?
                == OpenOutcome::Opened
            {
                luks_guard.track(target.mapper_name.clone());
            }
            emit_status(&status_line(
                StatusTag::Ok,
                color_enabled,
                &format!("disk {}: unlocked", target.name),
            ));

            let mapper = MapperName(target.mapper_name.clone());
            let identity = classify_braid_disk_fsid(runner, &target.name, &mapper, &self.pool)?;
            if let Some(err) = identity_to_error(&identity, &target.name) {
                return Err(err);
            }
            match identity {
                AddLuksIdentity::BraidLabeledAlreadyInPool => continue,
                AddLuksIdentity::BraidLabeledRecoverable => {
                    let verified_pool_fsid = self.pool.fsid.clone().ok_or_else(|| {
                        AddError::Validation(
                            "mounted pool has no FSID while journaling returned add target".into(),
                        )
                    })?;
                    eprintln!(
                        "note: braid-labeled disk '{}' verified as pool member. \
                         Completing recovery add.",
                        target.name
                    );
                    let verified = RecoverableBraidTarget {
                        name: target.name.clone(),
                        by_id: target.by_id.clone(),
                        mapper_name: target.mapper_name.clone(),
                        mapper_path: target.mapper_path.clone(),
                        luks_uuid: target.luks_uuid.clone(),
                        verified_pool_fsid,
                        enroll_key_file: target.enroll_key_file.clone(),
                        header_backup_path: target.header_backup_path.clone(),
                    };
                    journal_targets
                        .insert(verified.name.clone(), recoverable_journal_target(&verified));
                    needs_pool_add.push(PoolAddExecutionTarget {
                        mapper_path: verified.mapper_path,
                        force: true,
                    });
                }
                _ => unreachable!("error variants handled by identity_to_error above"),
            }
        }

        if journal_targets.is_empty() {
            luks_guard.disarm();
            eprintln!("{}", format_add_noop(&self.names));
            return Ok(());
        }

        // Hold a logind sleep inhibitor for the rest of the add operation --
        // covers Pass-2 LUKS format/open of fresh disks, the bootstrap-or-add
        // pool phase, and the conditional pool_balance_raid1 that converts
        // single-profile data to RAID1 when the post-add pool has >=2 devices.
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
        let mut target_membership = self.pool_membership.clone();
        for (name, target) in &journal_targets {
            target_membership.disks.insert(
                name.clone(),
                membership::DiskMember::from_by_id(target.by_id.clone()),
            );
        }
        let journal = journal::build_journal(
            self.pool_membership.clone(),
            target_membership,
            journal::OpKind::Add {
                phase: journal::AddPhase::PoolMutation,
                targets: journal_targets.clone(),
            },
        );
        journal::write_journal(params.paths, &journal)
            .map_err(|e| AddError::Validation(e.to_string()))?;

        // Pass 2: execute irreversible operations for fresh disks.
        for target in &self.work_plan.targets {
            let AddTargetWork::Fresh(target) = target else {
                continue;
            };
            let name = target.name.as_str();

            if !matches!(
                journal_targets.get(name).map(|t| &t.mode),
                Some(journal::AddJournalMode::FreshLuks { .. })
            ) {
                return Err(AddError::Validation(format!(
                    "fresh add target '{}' missing from journal",
                    name
                )));
            }

            eprint!(
                "{}",
                status_line(
                    StatusTag::Wait,
                    color_enabled,
                    &format!("disk {name}: formatting LUKS..."),
                )
            );
            luks_format(
                runner,
                &target.by_id.0,
                &passphrase,
                &target.luks_format_extra_opts,
            )?;
            eprint!(
                "{}",
                status_line(
                    StatusTag::Ok,
                    color_enabled,
                    &format!("disk {name}: LUKS formatted"),
                )
            );

            if let Some(kf) = &target.enroll_key_file {
                emit_status(&status_line(
                    StatusTag::Wait,
                    color_enabled,
                    &format!("disk {name}: enrolling keyfile in slot 1..."),
                ));
                crate::luks::enroll_key_file(runner, &target.by_id.0, &passphrase, kf)?;
                emit_status(&status_line(
                    StatusTag::Ok,
                    color_enabled,
                    &format!("disk {name}: keyfile enrolled in slot 1"),
                ));
            }

            let backup_path = backup_luks_header_post_mutation(
                runner,
                &target.by_id.0,
                &target.mapper_name,
                params.paths,
            )?;
            eprintln!("LUKS header backed up: {}", backup_path.display());

            eprint!(
                "{}",
                status_line(
                    StatusTag::Wait,
                    color_enabled,
                    &format!("disk {name}: unlocking..."),
                )
            );
            if ensure_luks_open(runner, name, &target.by_id, &passphrase)? == OpenOutcome::Opened {
                luks_guard.track(target.mapper_name.clone());
            }
            eprint!(
                "{}",
                status_line(
                    StatusTag::Ok,
                    color_enabled,
                    &format!("disk {name}: unlocked"),
                )
            );

            needs_pool_add.push(PoolAddExecutionTarget {
                mapper_path: target.mapper_path.clone(),
                force: false,
            });
        }

        // Pass 3: replay keyfile enrollment + header backup for any
        // returned-disk add targets carrying `--enroll DIR`. Mirrors
        // the addKey/backup block in Pass 2 (Fresh) but skips the
        // luks_format -- the disks are already LUKS-formatted, the
        // planner has already classified each as `NeedsEnroll` (slot
        // 1 empty), and we run the addKey here so the journaled
        // mutation is replayable on crash. Iterates `journal_targets`
        // because verified-`ClosedPresentLuks` targets only land in
        // that map (not in `work_plan.targets`); driving off the
        // journal also matches what recovery replays.
        for (name, journal_target) in &journal_targets {
            let journal::AddJournalMode::RecoverableBraidLabeled {
                enroll_key_file: Some(kf),
                ..
            } = &journal_target.mode
            else {
                continue;
            };
            emit_status(&status_line(
                StatusTag::Wait,
                color_enabled,
                &format!("disk {name}: enrolling keyfile in slot 1..."),
            ));
            crate::luks::enroll_key_file(runner, &journal_target.by_id.0, &passphrase, kf)?;
            emit_status(&status_line(
                StatusTag::Ok,
                color_enabled,
                &format!("disk {name}: keyfile enrolled in slot 1"),
            ));
            let backup_path = backup_luks_header_post_mutation(
                runner,
                &journal_target.by_id.0,
                &journal_target.mapper_name,
                params.paths,
            )?;
            eprintln!("LUKS header backed up: {}", backup_path.display());
        }

        // Both passes complete -- mappers are committed for pool operations.
        luks_guard.disarm();

        // Pool phase
        let mapper_paths: Vec<String> = needs_pool_add
            .iter()
            .map(|target| target.mapper_path.clone())
            .collect();

        let mount_point = self.config.mount_point();

        if !self.pool.mounted {
            if mapper_paths.len() >= 2 {
                // Bootstrap with mkfs.btrfs RAID1
                pool_bootstrap_mount_raid1(runner, &mapper_paths, mount_point)?;
                eprintln!("Pool created (RAID1) and mounted at {}", mount_point);
            } else {
                // Single disk bootstrap
                pool_bootstrap_mount(runner, &mapper_paths[0], mount_point)?;
                eprintln!("Pool created and mounted at {}", mount_point);
            }

            // Fresh pool identity: every previous acked baseline is stale.
            alert::remove_acked_stats(params.paths).map_err(|e| AddError::AckCleanupFailed {
                stage: "bootstrap",
                detail: e.to_string(),
            })?;

            // Bootstrap post-commit persist: write pool.json after mkfs + mount.
            // Enrich with live metadata (luks_uuid, devid) from pool probe.
            let mut final_membership = journal.target_membership.clone();
            if let Ok(pool_after) = probe_pool(runner, fs, mount_point) {
                membership::enrich_from_pool_state(&pool_after, &mut final_membership);
            }
            membership::save_membership(&final_membership, params.paths)?;
            // Order matters: save_membership before clear_journal. If
            // save_membership fails, the journal survives and recover can
            // reconstruct pool.json from the live pool.
            journal::clear_journal(params.paths)
                .map_err(|e| AddError::Validation(e.to_string()))?;
        } else {
            // Add each to existing pool
            for target in &needs_pool_add {
                pool_add_device(runner, &target.mapper_path, mount_point, target.force)?;
                eprintln!("Device added to pool: {}", target.mapper_path);
                let pool_after = probe_pool(runner, fs, mount_point).map_err(|e| {
                    AddError::AckCleanupFailed {
                        stage: "post-add probe",
                        detail: format!("{}: {e}", target.mapper_path),
                    }
                })?;
                let devid =
                    devid_for_mapper_path(&pool_after, &target.mapper_path).ok_or_else(|| {
                        AddError::AckCleanupFailed {
                            stage: "post-add probe",
                            detail: format!("{}: not found in pool after add", target.mapper_path),
                        }
                    })?;
                alert::drop_ghost_acked_for_devids(params.paths, &[devid]).map_err(|e| {
                    AddError::AckCleanupFailed {
                        stage: "live-pool add",
                        detail: format!("devid {devid}: {e}"),
                    }
                })?;
            }

            // Membership is committed by btrfs device add. Persist it before
            // the long post-add balance while leaving the journal in place so
            // recovery still knows the balance is owed if interrupted.
            let pool_after = probe_pool(runner, fs, mount_point)?;
            for target in journal_targets.values() {
                let mapper = &target.mapper_name;
                if !pool_after.devices.iter().any(|d| d.mapper.0 == *mapper) {
                    return Err(AddError::Validation(format!(
                        "disk '{}' was not found in the live pool after add",
                        mapper
                    )));
                }
            }
            let mut final_membership = journal.target_membership.clone();
            membership::enrich_from_pool_state(&pool_after, &mut final_membership);
            membership::save_membership(&final_membership, params.paths)?;

            let mut balance_journal = journal.clone();
            if let journal::OpKind::Add { phase, .. } = &mut balance_journal.op {
                *phase = journal::AddPhase::PostAddBalanceRaid1;
            }
            journal::write_journal(params.paths, &balance_journal)
                .map_err(|e| AddError::Validation(e.to_string()))?;

            // Balance to RAID1 if total >= 2
            let total_after = self.pool.devices.len() + mapper_paths.len();
            if total_after >= 2 {
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Wait,
                        color_enabled,
                        "pool: balancing to RAID1...",
                    )
                );
                pool_balance_raid1(runner, mount_point, params.progress)?;
                eprint!(
                    "{}",
                    status_line(StatusTag::Ok, color_enabled, "pool: RAID1 balance complete",)
                );
            }

            // Leave the journal until the balance completes; interruption
            // after the membership commit still needs recovery replay.
            journal::clear_journal(params.paths)
                .map_err(|e| AddError::Validation(e.to_string()))?;
        }

        eprintln!("{}", format_add_done(&self.names));
        Ok(())
    }
}

/// Plan a `braid add` run. Owns everything above today's `--dry-run` gate:
/// pending-op preflight, config read, disk-spec parsing, duplicate-name /
/// duplicate-by-id validation, keyfile path validation, membership load,
/// conflict validation, per-disk probe, pool probe, mutation preflight, UPS
/// preflight, the missing-devices warning, the keyfile-asymmetry warning,
/// and the semantic add work planner. On success, every accumulated note lives
/// on `plan.notes`; on failure after note accumulation, notes survive on
/// `PlanFailure::notes` so `cmd_add` can render them to stderr before the error.
pub fn plan_add<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &AddParams<'_>,
) -> Result<AddPlan, PlanFailure<AddError>> {
    // Accumulator for preview-context notes that must survive a later
    // planner error. Notes added here travel to `PlanFailure::notes` on
    // the Err branch and move into `plan.notes` on the Ok branch.
    let mut notes: Vec<PreviewNote> = Vec::new();

    if let Err(msg) = preflight::check_no_pending_operation(params.paths) {
        return Err(PlanFailure::empty(AddError::Validation(msg)));
    }

    let config = match config_read(params.config_path) {
        Ok(c) => c,
        Err(e) => return Err(PlanFailure::empty(e.into())),
    };

    // Parse disk specs: name=by_id
    let parsed: Vec<(String, ByIdPath)> = match params
        .disk_specs
        .iter()
        .map(|s| membership::parse_disk_spec(s))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) => v,
        Err(e) => return Err(PlanFailure::empty(e.into())),
    };

    let names: Vec<String> = parsed.iter().map(|(n, _)| n.clone()).collect();
    let by_ids: Vec<ByIdPath> = parsed.iter().map(|(_, b)| b.clone()).collect();

    // Reject duplicate names upfront
    {
        let mut seen = std::collections::HashSet::new();
        for name in &names {
            if !seen.insert(name.as_str()) {
                return Err(PlanFailure::empty(AddError::Validation(format!(
                    "duplicate disk name: '{name}'"
                ))));
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
                return Err(PlanFailure::empty(AddError::Validation(format!(
                    "duplicate by_id: '{}'",
                    by_id.0
                ))));
            }
        }
    }

    if let Some(kf) = params.enroll_key_file
        && let Err(e) = crate::enroll_key_file::validate_key_file_path(kf, false)
    {
        return Err(PlanFailure::empty(AddError::Validation(e.to_string())));
    }

    // Load existing membership (or empty if first add)
    let pool_membership = match membership::load_membership(params.paths) {
        Ok(m) => m,
        Err(membership::MembershipError::NotFound(_)) => PoolMembership::empty(),
        Err(e) => return Err(PlanFailure::empty(e.into())),
    };

    // Validate no conflicts
    for (name, by_id) in &parsed {
        if let Err(e) = membership::validate_no_conflicts(&pool_membership, name, &by_id.0) {
            return Err(PlanFailure::empty(e.into()));
        }
    }

    // Probe all disks -- fail early if any absent
    let probed: Vec<ConfigDisk> = match names
        .iter()
        .zip(by_ids.iter())
        .map(|(name, by_id)| probe_config_disk(runner, fs, name.as_str(), by_id))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) => v,
        Err(e) => return Err(PlanFailure::empty(e.into())),
    };

    for (i, p) in probed.iter().enumerate() {
        if matches!(p.state, ConfigDiskState::Absent) {
            return Err(PlanFailure::empty(AddError::Validation(format!(
                "disk '{}' ({}) is not present. Is it plugged in?",
                names[i], by_ids[i]
            ))));
        }
    }

    // Probe pool + preflight (once)
    let pool = match probe_pool(runner, fs, config.mount_point()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { fstype, .. }) => {
            return Err(PlanFailure::empty(AddError::Validation(format!(
                "{} is already mounted with {fstype}, not btrfs. Unmount it first.",
                config.mount_point()
            ))));
        }
        Err(e) => return Err(PlanFailure::empty(AddError::Probe(e))),
    };

    // Refuse if pool.json lists members but pool isn't unlocked. Catches the
    // silent-bootstrap case where a fresh disk + locked pool would otherwise
    // overwrite pool.json and orphan the existing locked members.
    if let Err(msg) = preflight::check_pool_unlocked_if_membership_exists(&pool_membership, &pool) {
        return Err(PlanFailure::empty(AddError::Validation(msg)));
    }

    if pool.mounted {
        let fsid = pool.fsid.as_deref().expect("mounted pool must have FSID");
        match preflight::require_mutation_preflight(runner, fs, fsid, config.mount_point()) {
            Ok(preflight_notes) => notes.extend(preflight_notes),
            Err(msg) => return Err(PlanFailure::empty(AddError::Validation(msg))),
        }
    }
    if let Err(msg) =
        preflight::check_ups_not_on_battery(runner, config.ups().map(|u| u.name.as_str()), "add")
    {
        return Err(PlanFailure::with_notes(notes, AddError::Validation(msg)));
    }

    // Missing-devices warning: body-only, no legacy `warning:` prefix.
    // Lives on `notes` so it surfaces on both dry-run stdout (via
    // `Preview::render`) and real-run stderr (via `AddPlan::execute`
    // using `preview::render_notes_for_stderr`).
    if pool.missing_count > 0 {
        notes.push(PreviewNote::Warn(format_add_missing_devices_warning(
            pool.missing_count,
        )));
    }

    let any_needs_format = probed
        .iter()
        .any(|p| matches!(p.state, ConfigDiskState::PresentNotLuks));

    // Keyfile-asymmetry warning: body-only, no legacy `WARNING:` prefix.
    // Appended after the missing-devices warning so `AddPlan::execute`
    // replays them in that order on stderr.
    if any_needs_format && params.enroll_key_file.is_none() {
        let keyfile_probe = probe_pool_keyfile_enrollment(runner, &pool.devices);
        if keyfile_probe.has_enrollment {
            notes.push(PreviewNote::Warn(format_keyfile_asymmetry_warning()));
        } else {
            notes.extend(keyfile_probe.failures.iter().map(|failure| {
                PreviewNote::Warn(format_keyfile_enrollment_probe_failure(failure))
            }));
        }
    }

    // Build the semantic work plan. This can fail on PresentLuks identity / foreign-pool
    // guards -- any accumulated notes up to here (missing-devices,
    // keyfile-asymmetry) must survive on `PlanFailure::notes` so the caller can
    // render them to stderr before the error.
    let names_refs: Vec<&str> = names.iter().map(String::as_str).collect();
    let by_ids_refs: Vec<&ByIdPath> = by_ids.iter().collect();
    let work_plan = match build_add_work_plan(
        runner,
        &AddStepsInput {
            names: &names_refs,
            by_ids: &by_ids_refs,
            probed: &probed,
            pool: &pool,
            mount_point: config.mount_point(),
            paths: params.paths,
            enroll_key_file: params.enroll_key_file,
            luks_format_extra_opts: params.luks_format_extra_opts,
        },
    ) {
        Ok(s) => s,
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, e));
        }
    };
    // No-op preview: zero steps + Info note naming the already-in-pool
    // target(s). The Info note suppresses `Preview::render`'s
    // `nothing to do.` fallback (see `preview.rs`:
    // `render_info_note_suppresses_nothing_to_do`), matching real-run's
    // `eprintln!("Nothing to do -- ...")` wording via the shared
    // `format_add_noop` helper.
    if work_plan.is_noop() {
        notes.push(PreviewNote::Info(format_add_noop(&names)));
    }

    let plan = AddPlan {
        notes,
        work_plan,
        config,
        parsed,
        names,
        by_ids,
        probed,
        pool,
        pool_membership,
    };

    Ok(plan)
}

pub fn cmd_add<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &AddParams<'_>,
) -> Result<(), AddError> {
    let plan = match plan_add(runner, fs, params) {
        Ok(p) => p,
        Err(PlanFailure { notes, error }) => {
            // Preserved-context failure: accumulated notes render to
            // stderr before the error via the SAME helper as the Ok
            // path (`AddPlan::execute`), so a `PreviewNote::Warn`
            // emitted on the refusal path is byte-identical to the
            // same note emitted on dry-run stdout and real-run
            // success stderr.
            preview::emit_notes_to_stderr(&notes, AddPlan::STDERR_STYLE);
            return Err(error);
        }
    };

    if params.dry_run {
        plan.preview().print_colored();
        return Ok(());
    }

    plan.execute(runner, fs, params)
}

struct AddStepsInput<'a> {
    names: &'a [&'a str],
    by_ids: &'a [&'a ByIdPath],
    probed: &'a [ConfigDisk],
    pool: &'a PoolState,
    mount_point: &'a MountPoint,
    paths: &'a StatePaths,
    enroll_key_file: Option<&'a Path>,
    luks_format_extra_opts: &'a [String],
}

fn build_add_credential_prelude(input: &AddStepsInput<'_>) -> AddCredentialPrelude {
    let confirm_disks = input
        .names
        .iter()
        .zip(input.by_ids.iter())
        .zip(input.probed.iter())
        .map(|((name, by_id), probed)| AddConfirmDiskPlan {
            name: (*name).to_owned(),
            by_id: (*by_id).clone(),
            needs_luks_format: matches!(probed.state, ConfigDiskState::PresentNotLuks),
        })
        .collect();

    let any_needs_format = input
        .probed
        .iter()
        .any(|p| matches!(p.state, ConfigDiskState::PresentNotLuks));
    let confirm_new = any_needs_format && input.pool.devices.is_empty();
    let pool_target_count = input.pool.devices.len();

    let mut verify_targets: Vec<CredentialVerifyTarget> = input
        .pool
        .devices
        .iter()
        .map(|device| CredentialVerifyTarget {
            name: name_from_mapper(&device.mapper.0)
                .unwrap_or(device.mapper.0.as_str())
                .to_owned(),
            device: device.underlying.clone(),
        })
        .collect();
    verify_targets.extend(input.probed.iter().enumerate().filter_map(|(i, probed)| {
        match &probed.state {
            ConfigDiskState::PresentLuks { .. } => Some(CredentialVerifyTarget {
                name: input.names[i].to_owned(),
                device: input.by_ids[i].0.clone(),
            }),
            ConfigDiskState::Absent | ConfigDiskState::PresentNotLuks => None,
        }
    }));

    AddCredentialPrelude {
        confirm_disks,
        confirm_new,
        verify_targets,
        pool_target_count,
    }
}

fn fresh_journal_target(target: &FreshLuksTarget) -> journal::AddJournalTarget {
    journal::AddJournalTarget {
        by_id: target.by_id.clone(),
        mapper_name: target.mapper_name.clone(),
        mode: journal::AddJournalMode::FreshLuks {
            luks_label: target.luks_label.clone(),
            luks_format_extra_opts: target.luks_format_extra_opts.clone(),
            enroll_key_file: target.enroll_key_file.clone(),
        },
    }
}

fn recoverable_journal_target(target: &RecoverableBraidTarget) -> journal::AddJournalTarget {
    journal::AddJournalTarget {
        by_id: target.by_id.clone(),
        mapper_name: target.mapper_name.clone(),
        mode: journal::AddJournalMode::RecoverableBraidLabeled {
            verified_pool_fsid: target.verified_pool_fsid.clone(),
            luks_uuid: target.luks_uuid.clone(),
            enroll_key_file: target.enroll_key_file.clone(),
        },
    }
}

/// Resolve `--enroll DIR` against an already-LUKS add target via the
/// shared per-disk classifier. Returns `Some(kf)` when the disk needs a
/// keyfile mutation, `None` for the no-`--enroll` and idempotent
/// `AlreadyEnrolled` cases. Slot-1-occupied conflicts surface as a
/// pre-journal `AddError::Validation`. Mirrors the equivalent
/// resolution in `plan_replace`; the same helper drives both so the
/// silent-drop bug is structurally impossible on either path.
fn resolve_existing_luks_enroll<R: CommandRunner>(
    runner: &R,
    name: &str,
    by_id: &ByIdPath,
    user_enroll_key_file: Option<&Path>,
) -> Result<Option<PathBuf>, AddError> {
    let Some(kf) = user_enroll_key_file else {
        return Ok(None);
    };
    match crate::enroll_key_file::plan_single_disk_enrollment(
        runner,
        name,
        by_id,
        kf,
        crate::enroll_key_file::EnrollmentPlanMode::ExistingKeyfile,
    ) {
        Ok(crate::enroll_key_file::DiskEnrollAction::AlreadyEnrolled { .. }) => Ok(None),
        Ok(crate::enroll_key_file::DiskEnrollAction::NeedsEnroll { .. }) => {
            Ok(Some(kf.to_path_buf()))
        }
        Err(e) => Err(AddError::Validation(e.to_string())),
    }
}

fn build_add_work_plan<R: CommandRunner>(
    runner: &R,
    input: &AddStepsInput<'_>,
) -> Result<AddWorkPlan, AddError> {
    let mut targets = Vec::new();
    let mut initial_journal_targets: BTreeMap<String, journal::AddJournalTarget> = BTreeMap::new();

    for (i, p) in input.probed.iter().enumerate() {
        let name = input.names[i];
        let by_id = input.by_ids[i];
        let mn = mapper_name(name);
        let mapper_name = mn.0.clone();
        let mapper_path = format!("/dev/mapper/{mapper_name}");

        match &p.state {
            ConfigDiskState::Absent => {
                return Err(AddError::Validation(format!(
                    "disk '{}' ({}) is not present. Is it plugged in?",
                    name, by_id
                )));
            }
            ConfigDiskState::PresentNotLuks => {
                let mut extra_opts = input.luks_format_extra_opts.to_vec();
                let luks_label = format!("braid-{name}");
                extra_opts.push("--label".into());
                extra_opts.push(luks_label.clone());
                let target = FreshLuksTarget {
                    name: name.to_owned(),
                    by_id: (*by_id).clone(),
                    mapper_name,
                    mapper_path,
                    luks_label,
                    luks_format_extra_opts: extra_opts,
                    enroll_key_file: input.enroll_key_file.map(Path::to_path_buf),
                    header_backup_path: input
                        .paths
                        .luks_headers_dir()
                        .join(format!("{}.luksheader", mn.0)),
                };
                initial_journal_targets.insert(name.to_owned(), fresh_journal_target(&target));
                targets.push(AddTargetWork::Fresh(target));
            }
            ConfigDiskState::PresentLuks {
                uuid,
                mapper_open,
                label,
            } => {
                // Preconditions always checked — no mapper required.
                validate_braid_preconditions(name, &by_id.0, label.as_deref(), input.pool)?;

                let resolved_enroll_key_file =
                    resolve_existing_luks_enroll(runner, name, by_id, input.enroll_key_file)?;

                if *mapper_open {
                    // Mapper is open — full classification without side effects
                    let identity = classify_braid_disk_fsid(runner, name, &mn, input.pool)?;
                    if let Some(err) = identity_to_error(&identity, name) {
                        return Err(err);
                    }
                    match identity {
                        AddLuksIdentity::BraidLabeledAlreadyInPool => continue,
                        AddLuksIdentity::BraidLabeledRecoverable => {
                            let verified_pool_fsid = input.pool.fsid.clone().ok_or_else(|| {
                                AddError::Validation(
                                    "mounted pool has no FSID while planning returned add target"
                                        .into(),
                                )
                            })?;
                            let target = RecoverableBraidTarget {
                                name: name.to_owned(),
                                by_id: (*by_id).clone(),
                                mapper_name,
                                mapper_path,
                                luks_uuid: uuid.clone(),
                                verified_pool_fsid,
                                enroll_key_file: resolved_enroll_key_file,
                                header_backup_path: input
                                    .paths
                                    .luks_headers_dir()
                                    .join(format!("{}.luksheader", mn.0)),
                            };
                            initial_journal_targets
                                .insert(name.to_owned(), recoverable_journal_target(&target));
                            targets.push(AddTargetWork::OpenRecoverable(target));
                        }
                        _ => unreachable!("error variants handled by identity_to_error above"),
                    }
                } else {
                    // Mapper closed — FSID verification deferred to execution time.
                    targets.push(AddTargetWork::ClosedPresentLuks(
                        ClosedPresentLuksCandidate {
                            name: name.to_owned(),
                            by_id: (*by_id).clone(),
                            mapper_name,
                            mapper_path,
                            luks_uuid: uuid.clone(),
                            enroll_key_file: resolved_enroll_key_file,
                            header_backup_path: input
                                .paths
                                .luks_headers_dir()
                                .join(format!("{}.luksheader", mn.0)),
                        },
                    ));
                }
            }
        }
    }

    Ok(AddWorkPlan {
        prelude: build_add_credential_prelude(input),
        targets,
        initial_journal_targets,
        mount_point: input.mount_point.clone(),
        pool_was_mounted: input.pool.mounted,
        existing_pool_device_count: input.pool.devices.len(),
    })
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
            msg.push_str(&format!(
                "  {:width$}{}\n",
                "",
                hw_line,
                width = d.name.len() + 2
            ));
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
    use crate::luks::{RealTty, ScriptedPassphraseReader};
    use crate::secret::Passphrase;

    fn test_paths() -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        (tmp, paths)
    }

    fn passphrase(s: &str) -> Passphrase {
        Passphrase::from_zeroizing(zeroize::Zeroizing::new(s.to_owned()))
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
        assert!(
            !msg.contains("LUKS-formatted"),
            "no format warning for existing LUKS"
        );
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
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &sp,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
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

    /*
     * Intent: devid_for_mapper_path resolves the btrfs devid assigned to a
     * mapper path returned from the add loop.
     *
     * Why it exists: live-pool add cleanup must delete the acked-stats entry
     * for the freshly assigned devid, not for the disk's config name or
     * by-id path. This helper is the narrow translation boundary.
     *
     * Scenario: `braid add` has just added /dev/mapper/braid-disk2, then
     * probes the pool and must find devid 4 for the cleanup call.
     */
    #[test]
    fn devid_for_mapper_path_matches_mapper_name() {
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk2".into()),
                luks_uuid: LuksUuid("22222222-2222-2222-2222-222222222222".into()),
                devid: 4,
                underlying: "/dev/vdc".into(),
            }],
            missing_count: 0,
            total_devices: 1,
            fsid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
            missing_devids: vec![],
            null_underlying: vec![],
        };

        assert_eq!(
            devid_for_mapper_path(&pool, "/dev/mapper/braid-disk2"),
            Some(4)
        );
        assert_eq!(devid_for_mapper_path(&pool, "/dev/mapper/missing"), None);
    }

    // --- add work-plan identity tests ---

    fn probed_present_luks(name: &str, mapper_open: bool, label: Option<String>) -> ConfigDisk {
        ConfigDisk {
            name: name.to_owned(),
            by_id_path: ByIdPath("/dev/disk/by-id/disk1".to_owned()),
            state: ConfigDiskState::PresentLuks {
                uuid: LuksUuid("a1b2c3d4-e5f6-7890-abcd-ef1234567890".into()),
                label,
                mapper_open,
            },
        }
    }

    #[test]
    fn dry_run_non_braid_luks_reports_blocked() {
        let runner = MockRunner::default();
        let probed = vec![probed_present_luks("disk1", true, None)];
        let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

        let result = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
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

        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShowTarget {
                target: "/dev/mapper/braid-disk1".into(),
            },
            btrfs_show_with_uuid(device_fsid),
        );
        let probed = vec![probed_present_luks(
            "disk1",
            true,
            Some("braid-disk1".to_owned()),
        )];
        let pool = pool_mounted_with_fsid(pool_fsid);

        let result = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
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
        let runner = MockRunner::default();
        let probed = vec![probed_present_luks(
            "disk1",
            false,
            Some("braid-disk1".to_owned()),
        )];
        let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

        let steps = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
            },
        )
        .unwrap()
        .render_steps();

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
        let runner = MockRunner::default();
        let probed = vec![probed_present_luks(
            "disk1",
            false,
            Some("braid-disk1".to_owned()),
        )];
        let pool = pool_unmounted();

        let result = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
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

        let steps = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
            },
        )
        .unwrap()
        .render_steps();

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

    // Intent: validate_braid_preconditions never dispatches cryptsetup
    //   luksDump for a PresentLuks disk whose label was captured at probe time.
    // Why it exists: prior implementation re-read luksDump during planning
    //   and execute Pass 1, creating a human-sized TOCTOU window.
    // Scenario: a pre-probed braid-labeled disk is already in the pool.
    //   The runner has no CryptsetupLuksDumpText stub, so a redundant
    //   dispatch fails with MissingMock.
    #[test]
    fn add_planning_and_pass1_do_not_redispatch_luksdump() {
        let (_tmp, paths) = test_paths();
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let pool_fsid = POOL_FSID;
        let by_id = ByIdPath("/dev/disk/by-id/virtio-disk2".into());
        let luks_uuid = LuksUuid("a1b2c3d4-e5f6-7890-abcd-ef1234567890".into());
        let probed = vec![ConfigDisk {
            name: "disk2".into(),
            by_id_path: by_id.clone(),
            state: ConfigDiskState::PresentLuks {
                uuid: luks_uuid.clone(),
                label: Some("braid-disk2".to_owned()),
                mapper_open: true,
            },
        }];
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk1".into()),
                luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
                devid: 1,
                underlying: "/dev/vdb".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: Some(pool_fsid.to_owned()),
            null_underlying: vec![],
        };
        let mut pool_membership = PoolMembership::empty();
        pool_membership.disks.insert(
            "disk1".into(),
            membership::DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let runner = NoDumpRunner {
            inner: UnlockingAddRunner {
                inner: AddTestRunner {
                    disk_in_pool: true,
                    fail_device_add: false,
                    no_btrfs_superblock: false,
                },
            },
        };

        let work_plan = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk2"],
                by_ids: &[&by_id],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &paths,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
            },
        )
        .expect("planning should use cached LUKS label");
        let steps = work_plan.render_steps();
        assert!(
            !steps.is_empty(),
            "recoverable PresentLuks disk should plan returned-disk work: {steps:?}"
        );

        let plan = AddPlan {
            notes: vec![],
            work_plan,
            config: Config::new(MountPoint("/mnt/storage".into())).unwrap(),
            parsed: vec![("disk2".into(), by_id.clone())],
            names: vec!["disk2".into()],
            by_ids: vec![by_id.clone()],
            probed,
            pool,
            pool_membership,
        };
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        plan.execute(
            &runner,
            &AddMockFs(vec![]),
            &AddParams {
                config_path: Path::new("/dev/null"),
                disk_specs: &[],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(passphrase_file.path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
            },
        )
        .expect("execute Pass 1 should use cached LUKS label");

        // The runner rejects CryptsetupLuksDumpText, so the successful
        // execution above proves the add path used the cached label instead
        // of redispatching luksDump.
    }

    // -----------------------------------------------------------------------
    // validate_braid_preconditions / identity_to_error canonical message tests
    // -----------------------------------------------------------------------

    #[test]
    fn preconditions_non_braid_label_canonical_message() {
        // Intent: validate_braid_preconditions emits the canonical label-mismatch error.
        // Why it exists: pins the error text so both cmd_add and add work-plan rendering
        //   can't drift — they both call this function.
        // Scenario: user tries to add a LUKS disk that was not created by braid.
        let pool = pool_unmounted();
        let err = validate_braid_preconditions(
            "disk1",
            "/dev/disk/by-id/disk1",
            Some("some-other-label"),
            &pool,
        )
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
        // Why it exists: pins the error text so both cmd_add and add work-plan rendering
        //   can't drift — they both call this function.
        // Scenario: user tries to add a braid-labeled disk when no pool is mounted
        //   (e.g. fresh bootstrap scenario with pre-existing encrypted disk).
        let pool = pool_unmounted();
        let err = validate_braid_preconditions(
            "disk1",
            "/dev/disk/by-id/disk1",
            Some("braid-disk1"),
            &pool,
        )
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
        //   between cmd_add and add work-plan rendering. Pinning it prevents recurrence.
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
        // Intent: add work-plan rendering and cmd_add produce identical BraidLabeledNoBtrfs
        //   error text, proving both call sites go through identity_to_error.
        // Why it exists: this is the exact message that had already diverged before the
        //   refactor. This test makes that divergence impossible to reintroduce silently.
        // Scenario: braid-labeled disk with mapper open, but no btrfs superblock inside.

        // dry-run path: add work-plan rendering with mapper_open=true
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShowTarget {
                target: "/dev/mapper/braid-disk1".into(),
            },
            btrfs_show_no_btrfs(),
        );
        let probed = vec![probed_present_luks(
            "disk1",
            true,
            Some("braid-disk1".to_owned()),
        )];
        let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

        let dry_err = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
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
        close_output: RawCommandOutput,
    }

    impl SpyRunner {
        fn new(inner: MockRunner) -> Self {
            Self {
                inner,
                closed: Mutex::new(Vec::new()),
                close_output: RawCommandOutput {
                    cmd: "cryptsetup close".into(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            }
        }

        fn with_close_output(mut self, close_output: RawCommandOutput) -> Self {
            self.close_output = close_output;
            self
        }
    }

    impl CommandRunner for SpyRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            if let CmdRequest::CryptsetupClose { mapper } = request {
                self.closed.lock().unwrap().push(mapper.clone());
                let mut output = self.close_output.clone();
                output.cmd = format!("cryptsetup close {mapper}");
                return Ok(output);
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
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            let mut guard = LuksCleanupGuard::new(&runner);
            guard.track("braid-aaa".into());
            guard.track("braid-bbb".into());
            // guard drops here while still armed
        });
        let closed = runner.closed.lock().unwrap();
        assert_eq!(
            *closed,
            vec!["braid-bbb", "braid-aaa"],
            "should close tracked mappers in reverse order"
        );
        assert!(
            captured.contains("[wait] disk bbb: locking (cleanup)...\n"),
            "expected cleanup wait row for bbb, got: {captured:?}"
        );
        assert!(
            captured.contains("[ok]   disk bbb: locked (cleanup)\n"),
            "expected cleanup ok row for bbb, got: {captured:?}"
        );
        assert!(
            captured.find("[wait] disk bbb: locking (cleanup)...")
                < captured.find("[ok]   disk bbb: locked (cleanup)"),
            "cleanup wait must precede ok, got: {captured:?}"
        );
    }

    #[test]
    fn guard_close_failure_emits_cleanup_warn_row() {
        // Intent: rollback close failures close their [wait] row with [warn].
        // Why it exists: add rollback is best-effort, so the command can
        // continue unwinding after a failed close without leaving a dangling
        // [wait] row.
        // Scenario: cryptsetup close returns non-zero while the cleanup guard
        // is closing a mapper after a later add step failed.
        let runner = SpyRunner::new(MockRunner::default()).with_close_output(RawCommandOutput {
            cmd: "cryptsetup close".into(),
            stdout: String::new(),
            stderr: "device is busy".into(),
            exit_status: 5,
        });
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            let mut guard = LuksCleanupGuard::new(&runner);
            guard.track("braid-aaa".into());
            // guard drops here while still armed
        });
        let wait = "[wait] disk aaa: locking (cleanup)...";
        let warn = "[warn] disk aaa: lock failed (cleanup, device busy: cryptsetup close braid-aaa failed (exit 5): device is busy)";
        assert!(captured.contains(wait), "missing wait row: {captured:?}");
        assert!(captured.contains(warn), "missing warn row: {captured:?}");
        assert!(
            captured.find(wait) < captured.find(warn),
            "cleanup wait must precede warn, got: {captured:?}"
        );
    }

    #[test]
    fn guard_retries_busy_close_before_success() {
        // Intent: add cleanup uses the shared retry-on-exit-5 close helper.
        // Why it exists: add and unlock must share close mechanics even though
        // add keeps its own best-effort warning policy.
        // Scenario: cleanup close for a mapper is busy once, then succeeds.
        let runner = MockRunner::default().with_output_sequence(
            CmdRequest::CryptsetupClose {
                mapper: "braid-aaa".into(),
            },
            vec![
                RawCommandOutput {
                    cmd: "cryptsetup close".into(),
                    stdout: String::new(),
                    stderr: "device is busy".into(),
                    exit_status: 5,
                },
                RawCommandOutput {
                    cmd: "cryptsetup close".into(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            ],
        );
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            let mut guard = LuksCleanupGuard::new(&runner);
            guard.track("braid-aaa".into());
            // guard drops here while still armed
        });

        let close_count = runner
            .requests()
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupClose { .. }))
            .count();
        assert_eq!(close_count, 2, "busy close should be retried once");
        assert!(
            captured.contains("cryptsetup close braid-aaa busy, retrying (1/3)..."),
            "missing shared retry warning: {captured:?}"
        );
        assert!(
            captured.contains("[ok]   disk aaa: locked (cleanup)"),
            "cleanup should finish with ok row: {captured:?}"
        );
        assert!(
            !captured.contains("lock failed (cleanup"),
            "successful retry must not emit final cleanup warning: {captured:?}"
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

    #[test]
    fn already_owned_open_outcome_is_not_tracked_by_guard() {
        // Intent: LuksCleanupGuard tracks only ensure_luks_open outcomes that
        // actually opened a mapper.
        // Why it exists: an already-owned mapper at execute time can come from
        // operator action between plan and execution; add must not close it on
        // a later failure.
        // Scenario: ensure_luks_open finds braid-existing already active with
        // the requested LUKS UUID, then the armed guard drops.
        let by_id = ByIdPath("/dev/disk/by-id/existing".into());
        let uuid = "11111111-1111-1111-1111-111111111111";
        let runner = MockRunner::default()
            .with_mapper_open("braid-existing", "/dev/vdb", uuid)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: by_id.0.clone(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksUUID".into(),
                    stdout: format!("{uuid}\n"),
                    stderr: String::new(),
                    exit_status: 0,
                },
            );

        {
            let mut guard = LuksCleanupGuard::new(&runner);
            if ensure_luks_open(&runner, "existing", &by_id, &passphrase("testpass")).unwrap()
                == OpenOutcome::Opened
            {
                guard.track("braid-existing".into());
            }
            // guard drops here while still armed
        }

        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupClose { mapper } if mapper == "braid-existing")),
            "already-owned mapper must not be closed by add cleanup guard"
        );
    }

    /// Wraps `AddTestRunner` to also satisfy `CryptsetupLuksOpen` (run via
    /// stdin) for the Pass-1 recoverable unlock test. The base runner is
    /// scoped to scenarios where every PresentLuks mapper is already open,
    /// so it has no built-in answer for the open-from-closed branch.
    struct UnlockingAddRunner {
        inner: AddTestRunner,
    }
    impl CommandRunner for UnlockingAddRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::CryptsetupLuksOpen { .. } => Ok(mock_ok("cryptsetup luksOpen", "")),
                CmdRequest::BtrfsDeviceScanForget { .. } => Ok(mock_ok("btrfs scan forget", "")),
                CmdRequest::WipefsBtrfs { .. } => Ok(mock_ok("wipefs", "")),
                CmdRequest::BtrfsBalanceRaid1 { .. } => Ok(mock_ok("btrfs balance", "")),
                _ => self.inner.run(request),
            }
        }
        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            if let CmdRequest::CryptsetupLuksOpen { .. } = request {
                return Ok(mock_ok("cryptsetup luksOpen", ""));
            }
            self.inner.run_with_stdin(request, stdin)
        }
    }

    struct NoDumpRunner<R> {
        inner: R,
    }

    impl<R: CommandRunner> CommandRunner for NoDumpRunner<R> {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            if matches!(request, CmdRequest::CryptsetupLuksDumpText { .. }) {
                return Err(CmdError::Failed(
                    "test runner must not redispatch luksDump".into(),
                ));
            }
            self.inner.run(request)
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            if matches!(request, CmdRequest::CryptsetupLuksDumpText { .. }) {
                return Err(CmdError::Failed(
                    "test runner must not redispatch luksDump".into(),
                ));
            }
            self.inner.run_with_stdin(request, stdin)
        }
    }

    struct RequestRecordingRunner<R> {
        inner: R,
        requests: Mutex<Vec<CmdRequest>>,
    }

    impl<R> RequestRecordingRunner<R> {
        fn new(inner: R) -> Self {
            Self {
                inner,
                requests: Mutex::new(Vec::new()),
            }
        }

        fn requests(&self) -> Vec<CmdRequest> {
            self.requests.lock().unwrap().clone()
        }
    }

    impl<R: CommandRunner> CommandRunner for RequestRecordingRunner<R> {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.requests.lock().unwrap().push(request.clone());
            self.inner.run(request)
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.requests.lock().unwrap().push(request.clone());
            self.inner.run_with_stdin(request, stdin)
        }
    }

    struct ClosedNoBtrfsRunner {
        inner: AddTestRunner,
    }

    impl CommandRunner for ClosedNoBtrfsRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::CryptsetupStatus { mapper } if mapper == "braid-disk2" => {
                    Ok(mock_status_inactive(mapper))
                }
                CmdRequest::CryptsetupLuksOpen { .. } => Ok(mock_ok("cryptsetup luksOpen", "")),
                _ => self.inner.run(request),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            if let CmdRequest::CryptsetupLuksOpen { .. } = request {
                return Ok(mock_ok("cryptsetup luksOpen", ""));
            }
            self.inner.run_with_stdin(request, stdin)
        }
    }

    /* Intent: add Pass-1's closed PresentLuks recoverable branch announces the
     * cryptsetup luksOpen with the canonical [wait]/[ok] rows.
     * Why it exists: Principle 13 requires every cryptsetup Argon2 wait window
     * to be announced; the BraidLabeledRecoverable + closed-mapper state
     * cannot be composed from existing braid commands without unverified
     * btrfs assumptions, so the row pin moves to the unit-test layer.
     * Scenario: a 2-disk add where disk1 is already in the pool and disk2 is
     * a recoverable braid-labeled disk whose mapper is closed -- Pass 1's
     * `if !mapper_open` block opens the mapper and emits the wait/ok pair.
     */
    #[test]
    fn pass1_recoverable_closed_mapper_emits_canonical_unlock_rows() {
        let (_state_tmp, paths, _tmp, _config_path, pass_path) = add_test_setup();
        // Crucially: /dev/mapper/braid-disk2 is NOT listed, so ensure_luks_open
        // in Pass 1 actually issues the cryptsetup open (rather than seeing an
        // already-existing mapper and short-circuiting).
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        // disk_in_pool: true so the post-BtrfsDeviceAdd probe_pool returns
        // disk1+disk2, letting save_membership and balance proceed cleanly.
        // The pre-add classify_braid_disk_fsid uses the AddPlan's `pool`
        // field (which we hand-build to contain only disk1), not the runner,
        // so identity is BraidLabeledRecoverable regardless.
        let runner = UnlockingAddRunner {
            inner: AddTestRunner {
                disk_in_pool: true,
                fail_device_add: false,
                no_btrfs_superblock: false,
            },
        };
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let config = crate::config::Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let by_id_disk2 = ByIdPath("/dev/disk/by-id/virtio-disk2".into());
        let mut pool_membership = membership::PoolMembership::empty();
        pool_membership.disks.insert(
            "disk1".into(),
            membership::DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk1".into()),
                luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
                devid: 1,
                underlying: "/dev/vdb".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: Some(POOL_FSID.into()),
            null_underlying: vec![],
        };
        let probed = vec![ConfigDisk {
            name: "disk2".into(),
            by_id_path: by_id_disk2.clone(),
            state: ConfigDiskState::PresentLuks {
                uuid: LuksUuid("22222222-2222-2222-2222-222222222222".into()),
                label: Some("braid-disk2".to_owned()),
                mapper_open: false,
            },
        }];
        let work_plan = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk2"],
                by_ids: &[&by_id_disk2],
                probed: &probed,
                pool: &pool,
                mount_point: config.mount_point(),
                paths: &paths,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
            },
        )
        .expect("closed recoverable target should plan");
        let plan = AddPlan {
            notes: vec![],
            work_plan,
            config,
            parsed: vec![("disk2".into(), by_id_disk2.clone())],
            names: vec!["disk2".into()],
            by_ids: vec![by_id_disk2.clone()],
            probed,
            pool,
            pool_membership,
        };

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            let result = plan.execute(
                &runner,
                &fs,
                &AddParams {
                    config_path: Path::new("/dev/null"),
                    disk_specs: &[],
                    dry_run: false,
                    yes: true,
                    passphrase_stdin: false,
                    passphrase_file: Some(pass_path.as_path()),
                    enroll_key_file: None,
                    luks_format_extra_opts: &[],
                    progress: ProgressOutput::Off,
                    paths: &paths,
                    sleep_inhibitor: &inhibitor,
                    passphrase_reader: &crate::luks::RealTty,
                },
            );
            // The recoverable add must succeed end-to-end -- if it does not,
            // the [wait]/[ok] rows we are pinning are still in the captured
            // buffer (they fire in Pass 1, before any later step could fail),
            // but a non-Ok result indicates an unexpected mock gap.
            assert!(result.is_ok(), "recoverable add should succeed: {result:?}");
        });

        let wait = "[wait] disk disk2: unlocking...";
        let ok = "[ok]   disk disk2: unlocked";
        assert!(captured.contains(wait), "missing wait row: {captured:?}");
        assert!(captured.contains(ok), "missing ok row: {captured:?}");
        assert!(
            captured.find(wait) < captured.find(ok),
            "wait must precede ok, got: {captured:?}"
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

    fn mock_not_luks(cmd: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: "Device is not a valid LUKS device.\n".into(),
            exit_status: 1,
        }
    }

    fn mock_luks_uuid(device: &str, uuid: &str) -> RawCommandOutput {
        mock_ok(
            &format!("cryptsetup luksUUID {device}"),
            &format!("{uuid}\n"),
        )
    }

    fn mock_status_active(mapper: &str, device: &str) -> RawCommandOutput {
        mock_ok(
            &format!("cryptsetup status {mapper}"),
            &format!(
                "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {device}\n  mode:    read/write\n"
            ),
        )
    }

    fn mock_status_inactive(mapper: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup status {mapper}"),
            stdout: String::new(),
            stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
            exit_status: 4,
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
            if path == "/proc/self/mountinfo" {
                Ok(
                    "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                        .to_owned(),
                )
            } else if path.ends_with("/exclusive_operation") {
                Ok("none\n".to_owned())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
            }
        }
        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    struct AddOfflineMockFs(Vec<String>);
    impl crate::probe::Filesystem for AddOfflineMockFs {
        fn exists(&self, path: &str) -> bool {
            self.0.iter().any(|p| p == path)
        }
        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path == "/proc/self/mountinfo" {
                Ok("26 25 0:23 / / rw shared:1 - ext4 /dev/sda1 rw\n".to_owned())
            } else if path.ends_with("/exclusive_operation") {
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
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {underlying}\n  mode:    read/write\n"
                        ),
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => {
                            "11111111-1111-1111-1111-111111111111"
                        }
                        _ => "22222222-2222-2222-2222-222222222222",
                    };
                    Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                    ))
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
                            stderr:
                                "ERROR: not a valid btrfs filesystem on /dev/mapper/braid-disk2"
                                    .into(),
                            exit_status: 1,
                        })
                    } else {
                        Ok(mock_ok(
                            &format!("btrfs filesystem show {target}"),
                            &format!(
                                "Label: none  uuid: {POOL_FSID}\n\tTotal devices 1 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n"
                            ),
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
                CmdRequest::CryptsetupTestPassphrase { device } => Ok(mock_ok(
                    &format!("cryptsetup open --test-passphrase {device}"),
                    "",
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

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    fn fresh_add_setup() -> (
        tempfile::TempDir,
        StatePaths,
        tempfile::TempDir,
        std::path::PathBuf,
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
        let pass_path = tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();

        (state_tmp, paths, tmp, config_path, pass_path)
    }

    fn test_acked_disk(missing_acked: bool, read_io_errs: u64) -> alert::AckedDisk {
        alert::AckedDisk {
            missing_acked,
            device_stats: alert::AckedDeviceCounters {
                read_io_errs,
                ..Default::default()
            },
        }
    }

    fn save_test_acked(
        paths: &StatePaths,
        entries: &[(&str, alert::AckedDisk)],
    ) -> std::collections::BTreeMap<String, alert::AckedDisk> {
        let map: std::collections::BTreeMap<String, alert::AckedDisk> = entries
            .iter()
            .map(|(key, value)| ((*key).to_owned(), value.clone()))
            .collect();
        alert::save_acked_stats(&alert::AckedStats(map.clone()), paths).unwrap();
        map
    }

    /// Stateful runner for full-path `cmd_add` acked-stats regression tests.
    /// It models a mounted one-disk pool for live adds, or an unmounted host
    /// for bootstrap, then changes `btrfs filesystem show` output after
    /// mount/device-add commits so command-level cleanup is exercised against
    /// the same post-commit probe shape production uses.
    struct AddFullPathRunner {
        mounted: Arc<AtomicBool>,
        added: Mutex<Vec<String>>,
        opened: Mutex<Vec<String>>,
        fail_bootstrap_post_mount_probe: bool,
        fail_second_add: bool,
        fail_post_add_probe: bool,
        fail_luks_format: bool,
        omit_new_mapper_from_probe: bool,
        disk2_devid: u64,
    }

    impl AddFullPathRunner {
        fn live() -> Self {
            Self {
                mounted: Arc::new(AtomicBool::new(true)),
                added: Mutex::new(Vec::new()),
                opened: Mutex::new(vec!["braid-disk1".to_owned()]),
                fail_bootstrap_post_mount_probe: false,
                fail_second_add: false,
                fail_post_add_probe: false,
                fail_luks_format: false,
                omit_new_mapper_from_probe: false,
                disk2_devid: 2,
            }
        }

        fn bootstrap() -> Self {
            let runner = Self::live();
            runner.mounted.store(false, Ordering::SeqCst);
            runner.opened.lock().unwrap().clear();
            runner
        }

        fn with_bootstrap_post_mount_probe_failure(mut self) -> Self {
            self.fail_bootstrap_post_mount_probe = true;
            self
        }

        fn with_second_add_failure(mut self) -> Self {
            self.fail_second_add = true;
            self
        }

        fn with_post_add_probe_failure(mut self) -> Self {
            self.fail_post_add_probe = true;
            self
        }

        fn with_luks_format_failure(mut self) -> Self {
            self.fail_luks_format = true;
            self
        }

        fn with_new_mapper_omitted_from_probe(mut self) -> Self {
            self.omit_new_mapper_from_probe = true;
            self
        }

        fn with_disk2_devid(mut self, devid: u64) -> Self {
            self.disk2_devid = devid;
            self
        }

        fn fs(&self, paths: Vec<String>) -> AddFullPathFs {
            AddFullPathFs {
                paths,
                mounted: Arc::clone(&self.mounted),
            }
        }

        fn added_mappers(&self) -> Vec<String> {
            self.added.lock().unwrap().clone()
        }

        fn mapper_devid(&self, mapper: &str) -> u64 {
            match mapper {
                "braid-disk1" => 1,
                "braid-disk2" => self.disk2_devid,
                "braid-disk3" => 3,
                other => panic!("unexpected mapper for devid mapping: {other}"),
            }
        }

        fn mapper_underlying(mapper: &str) -> &'static str {
            match mapper {
                "braid-disk1" => "/dev/vdb",
                "braid-disk2" => "/dev/vdc",
                "braid-disk3" => "/dev/vdd",
                other => panic!("unexpected mapper for underlying mapping: {other}"),
            }
        }

        fn luks_uuid_for_underlying(device: &str) -> Option<&'static str> {
            match device {
                "/dev/vdb" => Some("11111111-1111-1111-1111-111111111111"),
                "/dev/vdc" => Some("22222222-2222-2222-2222-222222222222"),
                "/dev/vdd" => Some("33333333-3333-3333-3333-333333333333"),
                _ => None,
            }
        }

        fn pool_show(&self) -> String {
            let mut mappers = vec!["braid-disk1".to_owned()];
            if !self.omit_new_mapper_from_probe {
                mappers.extend(self.added.lock().unwrap().iter().cloned());
            }
            let mut out = format!(
                "Label: none  uuid: {POOL_FSID}\n\
                 \tTotal devices {} FS bytes used 16.17MiB\n",
                mappers.len()
            );
            for mapper in mappers {
                let devid = self.mapper_devid(&mapper);
                out.push_str(&format!(
                    "\tdevid    {devid} size 496.00MiB used 121.56MiB path /dev/mapper/{mapper}\n"
                ));
            }
            out
        }
    }

    struct AddFullPathFs {
        paths: Vec<String>,
        mounted: Arc<AtomicBool>,
    }

    impl crate::probe::Filesystem for AddFullPathFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.iter().any(|p| p == path)
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }

        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path == "/proc/self/mountinfo" {
                if self.mounted.load(Ordering::SeqCst) {
                    Ok("36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                        .to_owned())
                } else {
                    Ok("26 25 0:23 / / rw shared:1 - ext4 /dev/sda1 rw\n".to_owned())
                }
            } else if path.ends_with("/exclusive_operation") {
                Ok("none\n".to_owned())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
            }
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    impl CommandRunner for AddFullPathRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    let has_added = !self.added.lock().unwrap().is_empty();
                    if self.fail_post_add_probe && has_added {
                        return Err(CmdError::Failed("post-add probe failed".into()));
                    }
                    if self.fail_bootstrap_post_mount_probe && self.mounted.load(Ordering::SeqCst) {
                        return Err(CmdError::Failed("post-mount probe failed".into()));
                    }
                    Ok(mock_ok(
                        &format!("btrfs filesystem show {mount_point}"),
                        &self.pool_show(),
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => {
                    if self.opened.lock().unwrap().iter().any(|m| m == mapper) {
                        Ok(mock_ok(
                            &format!("cryptsetup status {mapper}"),
                            &format!(
                                "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {}\n  mode:    read/write\n",
                                Self::mapper_underlying(mapper)
                            ),
                        ))
                    } else {
                        Ok(RawCommandOutput {
                            cmd: format!("cryptsetup status {mapper}"),
                            stdout: String::new(),
                            stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
                            exit_status: 4,
                        })
                    }
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    if let Some(uuid) = Self::luks_uuid_for_underlying(device) {
                        Ok(mock_ok(
                            &format!("cryptsetup luksUUID {device}"),
                            &format!("{uuid}\n"),
                        ))
                    } else {
                        Ok(RawCommandOutput {
                            cmd: format!("cryptsetup luksUUID {device}"),
                            stdout: String::new(),
                            stderr: "Device is not a valid LUKS device.\n".into(),
                            exit_status: 1,
                        })
                    }
                }
                CmdRequest::CryptsetupTestPassphrase { device } => Ok(mock_ok(
                    &format!("cryptsetup open --test-passphrase {device}"),
                    "",
                )),
                CmdRequest::CryptsetupLuksFormat { device, .. } if self.fail_luks_format => {
                    Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksFormat {device}"),
                        stdout: String::new(),
                        stderr: "mock: luksFormat failed after journal write".into(),
                        exit_status: 1,
                    })
                }
                CmdRequest::CryptsetupLuksFormat { device, .. } => {
                    Ok(mock_ok(&format!("cryptsetup luksFormat {device}"), ""))
                }
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device,
                    backup_path,
                } => {
                    std::fs::write(backup_path, b"mock luks header").unwrap();
                    Ok(mock_ok(
                        &format!("cryptsetup luksHeaderBackup {device}"),
                        "",
                    ))
                }
                CmdRequest::CryptsetupLuksOpen { device, mapper } => {
                    self.opened.lock().unwrap().push(mapper.clone());
                    Ok(mock_ok(
                        &format!("cryptsetup open --type luks {device} {mapper}"),
                        "",
                    ))
                }
                CmdRequest::BtrfsFilesystemShowTarget { target } => Ok(RawCommandOutput {
                    cmd: format!("btrfs filesystem show {target}"),
                    stdout: String::new(),
                    stderr: format!("ERROR: not a valid btrfs filesystem on {target}"),
                    exit_status: 1,
                }),
                CmdRequest::MkfsBtrfs { device } => {
                    Ok(mock_ok(&format!("mkfs.btrfs {device}"), ""))
                }
                CmdRequest::MkfsBtrfsRaid1 { devices } => {
                    Ok(mock_ok(&format!("mkfs.btrfs {}", devices.join(" ")), ""))
                }
                CmdRequest::Mount { device, .. } => {
                    self.mounted.store(true, Ordering::SeqCst);
                    Ok(mock_ok(&format!("mount {device}"), ""))
                }
                CmdRequest::BtrfsDeviceAdd { device, .. } => {
                    let mapper = device
                        .strip_prefix("/dev/mapper/")
                        .expect("test device-add path must be mapper")
                        .to_owned();
                    let mut added = self.added.lock().unwrap();
                    if self.fail_second_add && !added.is_empty() {
                        return Ok(RawCommandOutput {
                            cmd: format!("btrfs device add {device}"),
                            stdout: String::new(),
                            stderr: "ERROR: unable to add second device".into(),
                            exit_status: 1,
                        });
                    }
                    added.push(mapper);
                    Ok(mock_ok(&format!("btrfs device add {device}"), ""))
                }
                CmdRequest::BtrfsBalanceRaid1 { .. } => Ok(mock_ok("btrfs balance start", "")),
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

    /*
     * Intent: a live-pool `cmd_add` removes an acked-stats ghost entry for
     * the actual devid btrfs assigns to the newly added mapper, while leaving
     * an unrelated control entry unchanged.
     *
     * Why it exists: the safety boundary is the command callsite after
     * `btrfs device add` and post-add probe, not just the helper. A future
     * edit that skips the live-add cleanup would let a reused devid inherit
     * stale acknowledged baselines.
     *
     * Scenario: disk2 is formatted, opened, added to a mounted one-disk pool,
     * and btrfs reports it as devid 7. A stale ack for actual assigned devid
     * 7 must disappear, while the name-derived/index-like key 2 must survive
     * byte-for-byte at the value layer.
     */
    #[test]
    fn cmd_add_live_pool_drops_ghost_for_assigned_devid() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let name_derived_control = test_acked_disk(false, 22);
        let ghost = test_acked_disk(true, 5);
        save_test_acked(&paths, &[("2", name_derived_control.clone()), ("7", ghost)]);
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddFullPathRunner::live().with_disk2_devid(7);
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
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
            },
        )
        .expect("live add should succeed");

        assert_eq!(runner.added_mappers(), vec!["braid-disk2"]);
        let reloaded = alert::load_acked_stats(&paths);
        assert_eq!(
            reloaded.0.get("2"),
            Some(&name_derived_control),
            "cleanup must not derive the key from disk name or add index"
        );
        assert!(
            !reloaded.0.contains_key("7"),
            "newly assigned devid must not inherit a ghost ack"
        );
    }

    /*
     * Intent: bootstrap `cmd_add` deletes all pre-existing acked-stats before
     * the best-effort post-bootstrap probe/enrichment step, and still succeeds
     * when that probe fails.
     *
     * Why it exists: a fresh filesystem identity invalidates every old acked
     * baseline. The deletion must be wired into the bootstrap command path and
     * must not be delayed until the optional enrichment probe succeeds.
     *
     * Scenario: an old acked-stats.json exists before a first-disk bootstrap.
     * mkfs and mount commit, cleanup deletes the file, then the post-mount
     * probe fails. The command still persists membership and returns success.
     */
    #[test]
    fn cmd_add_bootstrap_clears_acked_stats_before_probe_enrich() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = fresh_add_setup();
        save_test_acked(
            &paths,
            &[
                ("1", test_acked_disk(true, 1)),
                ("2", test_acked_disk(true, 2)),
                ("7", test_acked_disk(false, 7)),
            ],
        );
        let runner = AddFullPathRunner::bootstrap().with_bootstrap_post_mount_probe_failure();
        let fs = runner.fs(vec!["/dev/disk/by-id/virtio-disk1".into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk1=/dev/disk/by-id/virtio-disk1".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
            },
        )
        .expect("bootstrap should succeed even when post-mount enrichment probe fails");

        assert!(
            !paths.acked_stats_json().exists(),
            "bootstrap cleanup must delete every stale acked baseline before enrichment"
        );
    }

    /*
     * Intent: a partial multi-add cleans the acked-stats ghost for each disk
     * whose `btrfs device add` already succeeded before a later add fails.
     *
     * Why it exists: the cleanup boundary is per committed device-add. A
     * future refactor that batches cleanup after all adds would leave disk2's
     * reused devid stale if disk3 fails before the batch runs.
     *
     * Scenario: disk2 and disk3 are both formatted and opened. Adding disk2
     * succeeds and btrfs assigns devid 2; adding disk3 then fails. The devid 2
     * ghost is gone, while disk3's uncommitted devid 3 ghost remains.
     */
    #[test]
    fn cmd_add_partial_multi_add_cleans_succeeded_disk_before_later_failure() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let ghost2 = test_acked_disk(true, 22);
        let ghost3 = test_acked_disk(true, 33);
        save_test_acked(&paths, &[("2", ghost2), ("3", ghost3.clone())]);
        let fs = AddMockFs(vec![
            "/dev/disk/by-id/virtio-disk2".into(),
            "/dev/disk/by-id/virtio-disk3".into(),
        ]);
        let runner = AddFullPathRunner::live().with_second_add_failure();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &[
                    "disk2=/dev/disk/by-id/virtio-disk2".into(),
                    "disk3=/dev/disk/by-id/virtio-disk3".into(),
                ],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
            },
        );

        assert!(result.is_err(), "second device add should fail");
        assert_eq!(runner.added_mappers(), vec!["braid-disk2"]);
        let reloaded = alert::load_acked_stats(&paths);
        assert!(
            !reloaded.0.contains_key("2"),
            "first committed add must clean its assigned devid"
        );
        assert_eq!(
            reloaded.0.get("3"),
            Some(&ghost3),
            "uncommitted later add must not drop its ghost entry"
        );
    }

    /*
     * Intent: a live-pool cleanup read/parse failure after `btrfs device add`
     * is fatal with the typed `AckCleanupFailed` stage `live-pool add`.
     *
     * Why it exists: mutation paths must fail closed when stale ack state may
     * exist but cannot be parsed. Asserting the typed error keeps the command
     * boundary pinned without relying on display-message wording.
     *
     * Scenario: disk2 is successfully added and post-add probe resolves devid
     * 2, but acked-stats.json contains invalid JSON. The command must return
     * `AddError::AckCleanupFailed` instead of silently proceeding.
     */
    #[test]
    fn cmd_add_live_pool_acked_cleanup_parse_failure_is_fatal() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        std::fs::write(paths.acked_stats_json(), "not json").unwrap();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddFullPathRunner::live();
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
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
            },
        );

        match result {
            Err(AddError::AckCleanupFailed { stage, .. }) => {
                assert_eq!(stage, "live-pool add");
            }
            other => panic!("expected live-pool AckCleanupFailed, got {other:?}"),
        }
        assert_eq!(
            runner.added_mappers(),
            vec!["braid-disk2"],
            "failure must occur after the irreversible device add"
        );
    }

    /*
     * Intent: bootstrap cleanup failure is fatal with the typed
     * `AckCleanupFailed` stage `bootstrap` after mkfs/mount have committed.
     *
     * Why it exists: bootstrap creates a new pool identity. If braid cannot
     * delete the old acked-stats artifact, it must stop loudly rather than
     * leave stale baselines alongside the fresh filesystem.
     *
     * Scenario: acked-stats.json is a directory, so `remove_file` fails after
     * the bootstrap mount succeeds. The returned error must identify the
     * bootstrap cleanup boundary.
     */
    #[test]
    fn cmd_add_bootstrap_acked_cleanup_failure_is_fatal() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = fresh_add_setup();
        std::fs::create_dir_all(paths.acked_stats_json()).unwrap();
        let runner = AddFullPathRunner::bootstrap();
        let fs = runner.fs(vec!["/dev/disk/by-id/virtio-disk1".into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk1=/dev/disk/by-id/virtio-disk1".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
            },
        );

        match result {
            Err(AddError::AckCleanupFailed { stage, .. }) => {
                assert_eq!(stage, "bootstrap");
            }
            other => panic!("expected bootstrap AckCleanupFailed, got {other:?}"),
        }
        assert!(
            runner.mounted.load(Ordering::SeqCst),
            "cleanup failure should happen after bootstrap mount"
        );
    }

    /*
     * Intent: post-add probe failures are fatal with the typed
     * `AckCleanupFailed` stage `post-add probe`, both when the probe command
     * itself fails and when the freshly added mapper is absent from the
     * successful probe result.
     *
     * Why it exists: live-add cleanup needs the assigned btrfs devid. If braid
     * cannot prove which devid was assigned, it must fail closed instead of
     * guessing or skipping cleanup.
     *
     * Scenario: disk2's `btrfs device add` succeeds. In one case the
     * following pool probe fails; in the other it succeeds but omits
     * /dev/mapper/braid-disk2. Both must stop at the post-add probe boundary.
     */
    #[test]
    fn cmd_add_post_add_probe_uncertainty_is_fatal() {
        for (label, runner) in [
            (
                "probe failure",
                AddFullPathRunner::live().with_post_add_probe_failure(),
            ),
            (
                "mapper omitted",
                AddFullPathRunner::live().with_new_mapper_omitted_from_probe(),
            ),
        ] {
            let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
            let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
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
                    luks_format_extra_opts: &[],
                    progress: ProgressOutput::Off,
                    paths: &paths,
                    sleep_inhibitor: &inhibitor,
                    passphrase_reader: &RealTty,
                },
            );

            match result {
                Err(AddError::AckCleanupFailed { stage, .. }) => {
                    assert_eq!(stage, "post-add probe", "{label}");
                }
                other => panic!("{label}: expected post-add AckCleanupFailed, got {other:?}"),
            }
            assert_eq!(
                runner.added_mappers(),
                vec!["braid-disk2"],
                "{label}: failure must happen after device add commits"
            );
        }
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
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
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
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
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
    // Intent: fresh existing-pool add journals the exact LUKS format options
    //   that the original invocation computed before `luksFormat`.
    //
    // Why it exists: recovery must replay a fresh target using the journaled
    //   format contract, not whatever flags a later invocation happens to
    //   pass. The easiest command-level proof is to fail `luksFormat` after
    //   the journal write and inspect the preserved `pending-op.json`.
    //
    // Scenario: the user adds a fresh disk with explicit LUKS format args;
    //   the machine crashes or the format command fails immediately after
    //   the journal is durable. Recovery must know the same opts, including
    //   the generated braid label.
    fn fresh_add_journal_stores_effective_luks_format_opts() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let runner = AddFullPathRunner::live().with_luks_format_failure();
        let fs = runner.fs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let luks_format_extra_opts = vec![
            "--pbkdf".to_owned(),
            "pbkdf2".to_owned(),
            "--iter-time".to_owned(),
            "1".to_owned(),
        ];
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
                luks_format_extra_opts: &luks_format_extra_opts,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
            },
        );

        assert!(
            result.is_err(),
            "forced luksFormat failure should abort add"
        );
        let journal = journal::load_journal(&paths)
            .unwrap()
            .expect("journal must survive the post-write format failure");
        let journal::OpKind::Add { phase, targets } = journal.op else {
            panic!("expected add journal, got: {:?}", journal.op);
        };
        assert_eq!(phase, journal::AddPhase::PoolMutation);
        let target = targets
            .get("disk2")
            .expect("disk2 target should be journaled");
        assert_eq!(target.by_id.0, "/dev/disk/by-id/virtio-disk2");
        assert_eq!(target.mapper_name, "braid-disk2");
        let journal::AddJournalMode::FreshLuks {
            luks_label,
            luks_format_extra_opts,
            enroll_key_file,
        } = &target.mode
        else {
            panic!("expected fresh LUKS target, got: {:?}", target.mode);
        };
        assert_eq!(luks_label, "braid-disk2");
        assert_eq!(
            luks_format_extra_opts,
            &vec![
                "--pbkdf".to_owned(),
                "pbkdf2".to_owned(),
                "--iter-time".to_owned(),
                "1".to_owned(),
                "--label".to_owned(),
                "braid-disk2".to_owned(),
            ]
        );
        assert!(enroll_key_file.is_none());
        assert_eq!(inhibitor.acquire_count(), 1);
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
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
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
    // Intent: a closed PresentLuks candidate that fails deferred identity
    //   verification still authenticates the shared passphrase first, then
    //   fails before sleep-inhibitor acquisition and journal write.
    //
    // Why it exists: closed returned disks cannot be FSID-checked during
    //   dry-run-safe planning. The execution path must keep the credential
    //   prelude ahead of mapper open/identity work, while still refusing
    //   BraidLabeledNoBtrfs before pending-op.json can be stranded.
    //
    // Scenario: disk2 is braid-labeled and closed during planning, unlocks
    //   with the shared passphrase, but contains no btrfs superblock after
    //   open. The command fails as ambiguous identity with no journal.
    fn closed_present_luks_identity_failure_verifies_credentials_before_no_journal() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = RequestRecordingRunner::new(ClosedNoBtrfsRunner {
            inner: AddTestRunner {
                disk_in_pool: false,
                fail_device_add: false,
                no_btrfs_superblock: true,
            },
        });
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let mut result = Ok(());
        crate::status_tag::testing::capture_with_color(false, || {
            result = cmd_add(
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
                    luks_format_extra_opts: &[],
                    progress: ProgressOutput::Off,
                    paths: &paths,
                    sleep_inhibitor: &inhibitor,
                    passphrase_reader: &RealTty,
                },
            );
        });

        let err = result.expect_err("closed identity failure should abort");
        assert!(
            err.to_string().contains("contains no btrfs superblock"),
            "expected BraidLabeledNoBtrfs refusal, got: {err}"
        );
        let requests = runner.requests();
        let credential = requests
            .iter()
            .position(|request| {
                matches!(
                    request,
                    CmdRequest::CryptsetupTestPassphrase { device }
                        if device == "/dev/disk/by-id/virtio-disk2"
                )
            })
            .unwrap_or_else(|| {
                panic!(
                    "candidate credential verification must run before identity check: {requests:?}"
                )
            });
        let identity = requests
            .iter()
            .position(|request| {
                matches!(
                    request,
                    CmdRequest::BtrfsFilesystemShowTarget { target }
                        if target == "/dev/mapper/braid-disk2"
                )
            })
            .expect("closed candidate should then enter the identity path");
        assert!(
            credential < identity,
            "credential verification must precede closed-candidate identity check: {requests:?}"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal should exist after deferred identity failure"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "deferred identity failure must NOT acquire the sleep inhibitor"
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
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
            },
        );

        let err = result
            .expect_err("duplicate by_id must be rejected")
            .to_string();
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
    // Why it exists: the "bootstrap only accepts fresh disks" guard is the
    //   invariant that makes the bootstrap path unreachable for PresentLuks
    //   disks. This test locks that invariant so a future refactor can't
    //   silently remove it.
    //
    // Scenario: user has a braid-labeled LUKS disk and no mounted pool. Running
    //   `braid add` must refuse rather than attempting bootstrap with an
    //   existing encrypted disk.
    fn bootstrap_rejects_braid_labeled_luks_disk() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        // The named contract here is the braid-labeled-LUKS bootstrap guard,
        // which only fires inside add work-plan rendering. Clear the
        // pre-seeded pool.json so the earlier locked-pool-with-membership
        // refusal (check_pool_unlocked_if_membership_exists) doesn't preempt
        // it; that earlier refusal has its own dedicated test
        // (cmd_add_refuses_when_pool_locked_with_membership).
        membership::save_membership(&membership::PoolMembership::empty(), &paths).unwrap();
        let fs = AddOfflineMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
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
            .with_mapper_closed("braid-disk2");

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
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
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
    // Intent: fresh bootstrap add rejects a pre-existing mapper name conflict
    // before mkfs.btrfs or mount can run.
    //
    // Why it exists: bootstrap helpers no longer probe btrfs themselves, so
    // the LUKS open helper's mapper ownership check is the boundary that
    // prevents an already-open `/dev/mapper/braid-disk1` from being treated as
    // the just-formatted disk.
    //
    // Scenario: an empty host adds disk1 as a fresh disk, but
    // `/dev/mapper/braid-disk1` already points at a non-LUKS backing device.
    fn cmd_add_fresh_bootstrap_mapper_conflict_stops_before_mkfs() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = fresh_add_setup();
        let by_id = "/dev/disk/by-id/virtio-disk1";
        let expected_uuid = "11111111-1111-1111-1111-111111111111";
        let backup_tmp = paths
            .luks_headers_dir()
            .join("braid-disk1.luksheader.tmp")
            .display()
            .to_string();
        let runner = MockRunner::default()
            .with_output_sequence(
                CmdRequest::CryptsetupLuksUuid {
                    device: by_id.into(),
                },
                vec![
                    mock_not_luks(&format!("cryptsetup luksUUID {by_id}")),
                    mock_luks_uuid(by_id, expected_uuid),
                ],
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksFormat {
                    device: by_id.into(),
                    extra_opts: vec!["--label".into(), "braid-disk1".into()],
                },
                b"test-passphrase".to_vec(),
                mock_ok("cryptsetup luksFormat", ""),
            )
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: by_id.into(),
                    backup_path: backup_tmp,
                },
                mock_ok("cryptsetup luksHeaderBackup", ""),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                mock_status_active("braid-disk1", "/dev/vdz"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdz".into(),
                },
                mock_not_luks("cryptsetup luksUUID /dev/vdz"),
            );
        let fs = AddOfflineMockFs(vec![by_id.into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk1=/dev/disk/by-id/virtio-disk1".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
            },
        );

        match result {
            Err(AddError::Luks(crate::luks::LuksError::MapperConflict { found: None, .. })) => {}
            other => panic!("expected MapperConflict with found=None, got {other:?}"),
        }
        let requests = runner.requests();
        assert!(
            !requests.iter().any(|request| matches!(
                request,
                CmdRequest::MkfsBtrfs { .. }
                    | CmdRequest::MkfsBtrfsRaid1 { .. }
                    | CmdRequest::Mount { .. }
            )),
            "mapper conflict must stop before mkfs or mount: {requests:?}"
        );
    }

    #[test]
    // Intent: mixed fresh + braid-labeled LUKS bootstrap is rejected before
    // journal write, inhibitor acquisition, or mkfs.btrfs RAID1.
    //
    // Why it exists: a multi-disk bootstrap must not let one fresh disk carry
    // an existing braid-labeled LUKS disk into the RAID1 bootstrap path.
    //
    // Scenario: an empty host runs `braid add disk1=<fresh> disk2=<luks>`;
    // disk2 cannot be identity-verified because there is no mounted pool.
    fn cmd_add_mixed_bootstrap_rejects_present_luks_before_raid1_mkfs() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = fresh_add_setup();
        let disk1 = "/dev/disk/by-id/virtio-disk1";
        let disk2 = "/dev/disk/by-id/virtio-disk2";
        let disk2_uuid = "22222222-2222-2222-2222-222222222222";
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: disk1.into(),
                },
                mock_not_luks(&format!("cryptsetup luksUUID {disk1}")),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: disk2.into(),
                },
                mock_luks_uuid(disk2, disk2_uuid),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: disk2.into(),
                },
                mock_ok(
                    &format!("cryptsetup luksDump {disk2}"),
                    "LUKS header information\nVersion:       \t2\nLabel:         \tbraid-disk2\nSubsystem:     \t(no subsystem)\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk2".into(),
                },
                mock_status_inactive("braid-disk2"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: disk2.into(),
                },
                b"test-passphrase".to_vec(),
                mock_ok("cryptsetup open --test-passphrase", ""),
            );
        let fs = AddOfflineMockFs(vec![disk1.into(), disk2.into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &[
                    "disk1=/dev/disk/by-id/virtio-disk1".into(),
                    "disk2=/dev/disk/by-id/virtio-disk2".into(),
                ],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
            },
        );

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("bootstrap only accepts fresh disks"),
            "expected bootstrap rejection, got: {err}"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal should exist after mixed bootstrap validation failure"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "mixed bootstrap rejection must NOT acquire the sleep inhibitor"
        );
        let requests = runner.requests();
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, CmdRequest::MkfsBtrfsRaid1 { .. })),
            "mixed bootstrap rejection must stop before RAID1 mkfs: {requests:?}"
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
        let luks_format_extra_opts = vec![
            "--pbkdf".to_owned(),
            "pbkdf2".to_owned(),
            "--iter-time".to_owned(),
            "1".to_owned(),
        ];

        let steps = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &luks_format_extra_opts,
            },
        )
        .unwrap()
        .render_steps();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // Steps: LUKS format, header backup, LUKS open, mkfs, mount = 5 steps × 2 lines = 10
        assert_eq!(lines.len(), 10, "expected 10 lines, got:\n{output}");

        // LUKS format
        assert!(lines[0].contains("[destructive]"));
        assert!(lines[0].contains("LUKS format"));
        assert!(lines[1].contains("$ cryptsetup luksFormat"));
        assert!(lines[1].contains("--pbkdf pbkdf2 --iter-time 1"));
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
    /*
     * Intent: with `--enroll-key-file`, the dry-run preview emits the LUKS
     *   init steps in the order LuksFormat -> LuksAddKeyFile ->
     *   LuksHeaderBackup -> LuksOpen for a fresh (PresentNotLuks) disk.
     *
     * Why: a previous version backed up the header before enrolling the
     *   keyfile, so the resulting `.luksheader` did not contain slot 1 and
     *   restoring it would silently wipe keyfile-based unlock. This test
     *   pins the post-fix ordering at the dry-run layer; the real-execute
     *   layer is pinned by `cmd_add_with_keyfile_orders_format_addkey_backup_open`.
     *   Substring `find` is used (rather than indexed `lines[N]` checks)
     *   so the assertion survives unrelated future inserts in the step list.
     *
     * Scenario: bootstrap `braid add disk1=... --enroll-key-file=/mnt/kf`.
     */
    fn dry_run_render_fresh_disk_with_keyfile_orders_backup_after_addkey() {
        let runner = MockRunner::default();
        let probed = vec![ConfigDisk {
            name: "disk1".to_owned(),
            by_id_path: ByIdPath("/dev/disk/by-id/disk1".to_owned()),
            state: ConfigDiskState::PresentNotLuks,
        }];
        let pool = pool_unmounted();
        let kf = std::path::Path::new("/mnt/usb/braid.key");

        let steps = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: Some(kf),
                luks_format_extra_opts: &[],
            },
        )
        .unwrap()
        .render_steps();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        let find = |needle: &str| -> usize {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing line containing {needle:?}; got:\n{output}"))
        };
        let format = find("$ cryptsetup luksFormat");
        let addkey = find("$ cryptsetup luksAddKey");
        let backup = find("$ cryptsetup luksHeaderBackup");
        let open = find("$ cryptsetup open --type luks");

        assert!(
            format < addkey && addkey < backup && backup < open,
            "expected luksFormat({format}) < luksAddKey({addkey}) < \
             luksHeaderBackup({backup}) < luksOpen({open}); got:\n{output}"
        );
        // Sanity: the addKey line must reference the keyfile path so a
        // future change that drops --enroll-key-file plumbing fails here too.
        assert!(
            lines[addkey].contains("/mnt/usb/braid.key"),
            "luksAddKey line must mention the keyfile path; got: {}",
            lines[addkey]
        );
    }

    // Intent: `add --enroll DIR` against a `ClosedPresentLuksCandidate`
    //   (returning braid disk, slot 1 empty) renders LUKS open ->
    //   addKey -> headerBackup -> btrfs device add -f. Idempotent skip
    //   (slot 1 already authenticates) renders the open + add only,
    //   without the addKey/backup pair.
    // Why it exists: this is the closed-disk side of the silent-drop
    //   bug fix. Pre-refactor, `add --enroll DIR` against a returning
    //   braid disk silently dropped the keyfile -- the disk shipped
    //   without slot 1 enrolled and the auto-unlock service couldn't
    //   open it. Pin the rendered command order (open before addKey,
    //   addKey before backup, backup before add) so the post-mutation
    //   header backup captures slot 1 and a regression that flips the
    //   ordering would surface here.
    // Scenario: a disk that was originally added without a keyfile (or
    //   whose keyfile was rotated since) is replugged with `--enroll
    //   /mnt/usb`.
    #[test]
    fn dry_run_render_closed_present_luks_with_enroll_renders_addkey_and_backup() {
        let kf = std::path::Path::new("/mnt/usb/braid.key");
        let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        let probed = vec![probed_present_luks(
            "disk1",
            false,
            Some("braid-disk1".to_owned()),
        )];

        // Mock the resolve_existing_luks_enroll calls: keyfile probe is
        // Rejected (slot 1 unenrolled), then luksDump shows slot 1
        // empty so the planner returns NeedsEnroll.
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupTestKeyFile {
                    device: "/dev/disk/by-id/disk1".into(),
                    key_file_path: kf.display().to_string(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup open --test-passphrase --key-file".into(),
                    stdout: String::new(),
                    stderr: "No key".into(),
                    exit_status: 2,
                },
            )
            .with_output(
                CmdRequest::CryptsetupLuksDump {
                    device: "/dev/disk/by-id/disk1".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksDump".into(),
                    stdout: r#"{"keyslots":{"0":{"type":"luks2"}}}"#.into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            );

        let steps = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: Some(kf),
                luks_format_extra_opts: &[],
            },
        )
        .unwrap()
        .render_steps();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();
        let find = |needle: &str| -> usize {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle:?} in:\n{output}"))
        };
        let open = find("$ cryptsetup open --type luks");
        let addkey = find("$ cryptsetup luksAddKey");
        let backup = find("$ cryptsetup luksHeaderBackup");
        let add = find("$ btrfs device add");
        assert!(
            open < addkey && addkey < backup && backup < add,
            "expected luksOpen({open}) < luksAddKey({addkey}) < \
             luksHeaderBackup({backup}) < btrfs device add({add}); got:\n{output}"
        );
        assert!(
            lines[addkey].contains("/mnt/usb/braid.key"),
            "addKey command must reference the keyfile path: {}",
            lines[addkey]
        );
    }

    // Intent: idempotent `add --enroll DIR` against a returning braid
    //   disk whose slot 1 already authenticates with the supplied
    //   keyfile elides the addKey + headerBackup commands and only
    //   renders LUKS open + btrfs device add.
    // Why it exists: pins the AlreadyEnrolled behavior of the per-disk
    //   classifier. Without this assertion, a regression that always
    //   ran addKey (even when slot 1 already authenticates) would
    //   uselessly re-enroll the same key on every recovery add.
    // Scenario: operator runs `braid add disk1=... --enroll /mnt/usb`
    //   for the second time, after the original add already enrolled
    //   the keyfile.
    #[test]
    fn dry_run_render_closed_present_luks_with_enroll_idempotent_skip_emits_no_addkey() {
        let kf = std::path::Path::new("/mnt/usb/braid.key");
        let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        let probed = vec![probed_present_luks(
            "disk1",
            false,
            Some("braid-disk1".to_owned()),
        )];

        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupTestKeyFile {
                device: "/dev/disk/by-id/disk1".into(),
                key_file_path: kf.display().to_string(),
            },
            RawCommandOutput {
                cmd: "cryptsetup open --test-passphrase --key-file".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 0,
            },
        );

        let steps = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk1"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk1".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: Some(kf),
                luks_format_extra_opts: &[],
            },
        )
        .unwrap()
        .render_steps();
        let output = Step::render_dry_run(&steps);
        assert!(
            !output.contains("$ cryptsetup luksAddKey"),
            "AlreadyEnrolled idempotent skip must not render luksAddKey; got:\n{output}"
        );
        assert!(
            !output.contains("$ cryptsetup luksHeaderBackup"),
            "AlreadyEnrolled idempotent skip must not render luksHeaderBackup; got:\n{output}"
        );
        assert!(
            output.contains("$ cryptsetup open --type luks"),
            "still expected the LUKS open step; got:\n{output}"
        );
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

        let steps = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &["disk2"],
                by_ids: &[&ByIdPath("/dev/disk/by-id/disk2".into())],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
            },
        )
        .unwrap()
        .render_steps();
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
        opened: Arc<Mutex<Vec<String>>>,
        /// When true, `CryptsetupLuksHeaderBackup` returns success and writes
        /// the backup file (matching `MockRunner`'s behavior). Default `false`
        /// preserves the historical "fail at backup so cmd_add aborts" abort
        /// scaffolding for the bootstrap-confirm tests. The keyfile-ordering
        /// test below sets this to `true` so execution continues past the
        /// backup and reaches the `CryptsetupLuksOpen` request, which is
        /// where the test deliberately aborts via `MissingMock`.
        backup_succeeds: bool,
        backup_failure_stderr: &'static str,
    }

    impl AddRecordingRunner {
        fn new(pool_mounted: bool) -> Self {
            Self {
                log: Arc::new(Mutex::new(Vec::new())),
                stdin_log: Arc::new(Mutex::new(Vec::new())),
                opened: Arc::new(Mutex::new(if pool_mounted {
                    vec!["braid-disk1".to_owned()]
                } else {
                    vec![]
                })),
                backup_succeeds: false,
                backup_failure_stderr: "mock: header backup forced to fail",
            }
        }
        fn with_backup_success(mut self) -> Self {
            self.backup_succeeds = true;
            self
        }
        fn with_backup_failure_stderr(mut self, stderr: &'static str) -> Self {
            self.backup_failure_stderr = stderr;
            self
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
                CmdRequest::CryptsetupStatus { mapper } => {
                    if self.opened.lock().unwrap().iter().any(|m| m == mapper) {
                        let underlying = match mapper.as_str() {
                            "braid-disk1" => "/dev/vdb",
                            "braid-disk2" => "/dev/vdc",
                            other => panic!("unexpected active mapper: {other}"),
                        };
                        Ok(RawCommandOutput {
                            cmd: format!("cryptsetup status {mapper}"),
                            stdout: format!(
                                "{mapper} is active and is in use.\n  \
                                 type:    LUKS2\n  device:  {underlying}\n  mode:    read/write\n"
                            ),
                            stderr: String::new(),
                            exit_status: 0,
                        })
                    } else {
                        Ok(RawCommandOutput {
                            cmd: format!("cryptsetup status {mapper}"),
                            stdout: String::new(),
                            stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
                            exit_status: 4,
                        })
                    }
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
                CmdRequest::CryptsetupLuksAddKeyFile { device, .. } => Ok(RawCommandOutput {
                    cmd: format!("cryptsetup luksAddKey {device}"),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                }),
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device,
                    backup_path,
                } => {
                    if self.backup_succeeds {
                        // Match MockRunner's behavior: create the backup file
                        // so `backup_luks_header_to`'s rename step succeeds.
                        if let Some(parent) = std::path::Path::new(backup_path.as_str()).parent() {
                            std::fs::create_dir_all(parent).map_err(|e| {
                                CmdError::Failed(format!("mock: create_dir_all: {e}"))
                            })?;
                        }
                        std::fs::write(backup_path, b"")
                            .map_err(|e| CmdError::Failed(format!("mock: write backup: {e}")))?;
                        Ok(RawCommandOutput {
                            cmd: format!("cryptsetup luksHeaderBackup {device}"),
                            stdout: String::new(),
                            stderr: String::new(),
                            exit_status: 0,
                        })
                    } else {
                        // Forced failure so cmd_add aborts cleanly after
                        // luks_format runs. Lets bootstrap-confirm tests
                        // assert on what ran without stubbing the full
                        // mkfs/mount chain.
                        Ok(RawCommandOutput {
                            cmd: format!("cryptsetup luksHeaderBackup {device}"),
                            stdout: String::new(),
                            stderr: self.backup_failure_stderr.into(),
                            exit_status: 1,
                        })
                    }
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

    struct PanicRunner;

    impl CommandRunner for PanicRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            panic!("planner-boundary test: runner must not be invoked; got: {request:?}");
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            panic!("planner-boundary test: runner must not be invoked; got: {request:?}");
        }
    }

    struct PanicFilesystem;

    impl Filesystem for PanicFilesystem {
        fn exists(&self, path: &str) -> bool {
            panic!("planner-boundary test: fs.exists must not be called; got: {path}");
        }

        fn is_block_device(&self, path: &str) -> bool {
            panic!("planner-boundary test: fs.is_block_device must not be called; got: {path}");
        }

        fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
            panic!("planner-boundary test: fs.list_dir must not be called; got: {path}");
        }

        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            panic!("planner-boundary test: fs.read_to_string must not be called; got: {path}");
        }
    }

    // Intent: `braid add --enroll` rejects a missing braid.key during
    // planning, before shell probes or Filesystem-backed disk probes.
    // Why it exists: a typoed keyfile path must not reach the destructive
    // LUKS format path and then fail only at keyfile enrollment.
    // Scenario: user passes a nonexistent enroll directory while adding a
    // fresh disk; the command refuses with a keyfile error and no probes run.
    #[test]
    fn plan_add_aborts_when_keyfile_missing_before_any_probe() {
        let (_state_tmp, paths, tmp, config_path) = confirm_test_setup();
        let kf_path = tmp.path().join("does-not-exist").join("braid.key");
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let failure = match plan_add(
            &PanicRunner,
            &PanicFilesystem,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk1=/dev/disk/by-id/virtio-disk1".into()],
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: Some(kf_path.as_path()),
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &crate::luks::RealTty,
            },
        ) {
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
            Err(failure) => failure,
        };

        match failure.error {
            AddError::Validation(msg) => assert!(
                msg.contains("keyfile not found"),
                "expected missing keyfile validation, got: {msg}"
            ),
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert!(
            failure.notes.is_empty(),
            "expected no notes: {:?}",
            failure.notes
        );
        assert_eq!(inhibitor.acquire_count(), 0);
    }

    // Intent: `braid add --enroll` rejects a directory at braid.key during
    // planning, before shell probes or Filesystem-backed disk probes.
    // Why it exists: checking only existence would still allow an invalid
    // keyfile path to reach destructive LUKS work before enrollment fails.
    // Scenario: user points --enroll at a directory containing a subdirectory
    // named braid.key; the command refuses before any disk inspection.
    #[test]
    fn plan_add_aborts_when_keyfile_is_directory_before_any_probe() {
        let (_state_tmp, paths, tmp, config_path) = confirm_test_setup();
        let kf_path = tmp.path().join("braid.key");
        std::fs::create_dir(&kf_path).unwrap();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let failure = match plan_add(
            &PanicRunner,
            &PanicFilesystem,
            &AddParams {
                config_path: &config_path,
                disk_specs: &["disk1=/dev/disk/by-id/virtio-disk1".into()],
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: Some(kf_path.as_path()),
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &crate::luks::RealTty,
            },
        ) {
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
            Err(failure) => failure,
        };

        match failure.error {
            AddError::Validation(msg) => assert!(
                msg.contains("is not a regular file"),
                "expected directory keyfile validation, got: {msg}"
            ),
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert!(
            failure.notes.is_empty(),
            "expected no notes: {:?}",
            failure.notes
        );
        assert_eq!(inhibitor.acquire_count(), 0);
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
        let fs = AddOfflineMockFs(vec!["/dev/disk/by-id/virtio-disk1".into()]);
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
                luks_format_extra_opts: &[],
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
        let fs = AddOfflineMockFs(vec!["/dev/disk/by-id/virtio-disk1".into()]);
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
                luks_format_extra_opts: &[],
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

    // Intent: fresh add enriches a LUKS header-backup failure after
    // luksFormat has already succeeded.
    // Why it exists: the add callsite must keep using the post-mutation
    // wrapper, not the raw local-backup helper.
    // Scenario: bootstrap `braid add disk1=...` formats the new disk, then
    // local header backup fails because the state directory is full.
    #[test]
    fn add_returns_enriched_error_when_post_format_backup_fails() {
        let (_state_tmp, paths, _tmp, config_path) = confirm_test_setup();
        let fs = AddOfflineMockFs(vec!["/dev/disk/by-id/virtio-disk1".into()]);
        let runner =
            AddRecordingRunner::new(false).with_backup_failure_stderr("No space left on device");
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let tty = ScriptedPassphraseReader::new(["ok", "ok"]);

        let err = cmd_add(
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
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &tty,
            },
        )
        .expect_err("post-format header-backup failure should abort add")
        .to_string();

        assert!(
            err.contains("cryptsetup luksHeaderBackup --header-backup-file"),
            "expected remediation command in: {err}"
        );
        assert!(
            err.contains("after the LUKS mutation completed"),
            "expected post-mutation framing in: {err}"
        );
    }

    /*
     * Intent: cmd_add with --enroll-key-file emits LUKS commands in the
     *   order LuksFormat -> LuksAddKeyFile -> LuksHeaderBackup -> LuksOpen
     *   when executing against a fresh (PresentNotLuks) disk.
     *
     * Why it exists: a previous version backed up the LUKS header
     *   immediately after luksFormat and only then ran luksAddKey. The
     *   resulting backup file did not contain slot 1, so restoring that
     *   backup wiped the keyfile slot -- breaking auto-unlock without
     *   the operator noticing until next boot. This test pins the full
     *   "format then addKey then backup then open" sequence so a future
     *   reorder that widens the no-backup window (open before backup) or
     *   re-introduces the missing-slot-1 backup (backup before addKey)
     *   fails immediately.
     *
     *   The dry-run preview path is pinned separately by
     *   `dry_run_render_fresh_disk_with_keyfile_orders_backup_after_addkey`;
     *   this test pins the real execute path that `ReplacePlan::execute`
     *   and `AddPlan::execute` reimplement inline.
     *
     * Scenario: bootstrap `braid add disk1=... --enroll-key-file=/tmp/kf`.
     *   The recording runner makes header backup succeed (so we proceed
     *   past it) and falls through to MissingMock at LuksOpen, which
     *   aborts cleanly while leaving the full request log behind.
     */
    #[test]
    fn cmd_add_with_keyfile_orders_format_addkey_backup_open() {
        let (_state_tmp, paths, _tmp, config_path) = confirm_test_setup();
        let fs = AddOfflineMockFs(vec!["/dev/disk/by-id/virtio-disk1".into()]);
        let runner = AddRecordingRunner::new(false).with_backup_success();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let tty = ScriptedPassphraseReader::new(["ok", "ok"]);
        let kf_dir = tempfile::tempdir().unwrap();
        let kf_path = kf_dir.path().join("braid.key");
        std::fs::write(&kf_path, [0u8; crate::luks::KEYFILE_SIZE]).unwrap();

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
                enroll_key_file: Some(&kf_path),
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &tty,
            },
        );

        assert!(
            result.is_err(),
            "cmd_add must abort at the unmocked LuksOpen request"
        );

        let log = runner.log();
        let position = |pred: fn(&CmdRequest) -> bool| -> usize {
            log.iter()
                .position(pred)
                .unwrap_or_else(|| panic!("expected request not found in log: {log:?}"))
        };
        let format = position(|r| matches!(r, CmdRequest::CryptsetupLuksFormat { .. }));
        let addkey = position(|r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. }));
        let backup = position(|r| matches!(r, CmdRequest::CryptsetupLuksHeaderBackup { .. }));
        let open = position(|r| matches!(r, CmdRequest::CryptsetupLuksOpen { .. }));

        assert!(
            format < addkey && addkey < backup && backup < open,
            "expected order LuksFormat({format}) < LuksAddKeyFile({addkey}) < \
             LuksHeaderBackup({backup}) < LuksOpen({open}); log = {log:?}"
        );
    }

    /*
     * Intent: cmd_add refuses BEFORE luksFormat / inhibitor / journal
     *   when pool.json lists members and the pool is not mounted (locked).
     *
     * Why it exists: this is the cmd_add-level regression for the
     *   silent-bootstrap bug. Previously, a fresh-disk add against a
     *   locked-but-populated pool fell through to the bootstrap branch
     *   (mkfs.btrfs single + mount), overwriting pool.json and orphaning
     *   the locked members. The refusal must be a validation failure
     *   that fires before any destructive step or environment-side
     *   resource acquisition. A unit-level test on the helper alone
     *   would not catch a wiring bug in plan_add, and a test asserting
     *   only the error message would not catch a regression that lets
     *   format run anyway.
     *
     * Scenario: 1-disk pool recorded in pool.json (disk1), pool locked
     *   (post-boot, pre-unlock). Operator forgets `braid unlock` and
     *   runs `braid add disk2=...`. The command must refuse, and no
     *   LUKS header must be written to disk2.
     */
    #[test]
    fn cmd_add_refuses_when_pool_locked_with_membership() {
        let (_state_tmp, paths, _tmp, config_path, _pass_path) = add_test_setup();
        let fs = AddOfflineMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddRecordingRunner::new(false);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let tty = ScriptedPassphraseReader::new(["SENTINEL"]);

        let result = cmd_add(
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
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &tty,
            },
        );

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("not unlocked"),
            "expected locked-pool refusal, got: {err}"
        );
        assert!(
            err.contains("disk1"),
            "error must name the locked member, got: {err}"
        );
        assert!(
            !runner.saw_format(),
            "luks_format must NOT run when refused for locked pool"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "validation failure must NOT acquire the sleep inhibitor"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal should exist after validation failure"
        );
        assert_eq!(
            tty.remaining(),
            1,
            "no prompts read; refusal happens before passphrase read"
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
                luks_format_extra_opts: &[],
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

    // -----------------------------------------------------------------------
    // PR 7: plan_add / AddPlan boundary tests (Preview migration)
    // -----------------------------------------------------------------------
    //
    // These tests pin the new Preview-model wiring for `braid add`:
    //   - pre-plan warnings become PreviewNote::Warn on plan.notes
    //   - already-in-pool is a note-only success (Info + zero steps)
    //   - dry-run render passes through plan.preview().render()
    //   - preserved-context failure carries accumulated notes on
    //     PlanFailure::notes when planning bails later
    //
    // Fixtures reuse add_test_setup/AddMockFs and a bespoke runner that
    // can toggle `missing_count` on the pool probe.

    /// Runner for plan_add boundary tests. Same stubs as AddTestRunner
    /// but exposes a `missing_count` knob that synthesizes `Total devices
    /// N` with `N - 1` real devid rows plus `N - 1` path-MISSING rows,
    /// driving probe_pool's missing-device arithmetic (`show.total_devices
    /// - devices.len()`).
    ///
    /// `probe_pool_keyfile_enrollment` looks at the existing pool
    /// members' underlying devices via CryptsetupLuksDump json.
    /// Tests that want keyfile-asymmetry or uncertainty warnings
    /// configure `keyfile_probes`.
    #[derive(Clone, Copy)]
    enum AddPlanKeyfileProbe {
        Empty,
        Occupied,
        Failure,
    }

    struct AddPlanTestRunner {
        missing_count: u64,
        keyfile_probes: Vec<AddPlanKeyfileProbe>,
    }

    impl AddPlanTestRunner {
        fn new() -> Self {
            Self {
                missing_count: 0,
                keyfile_probes: vec![AddPlanKeyfileProbe::Empty],
            }
        }

        fn with_missing(mut self, n: u64) -> Self {
            self.missing_count = n;
            self
        }

        fn with_keyfile(mut self) -> Self {
            self.keyfile_probes = vec![AddPlanKeyfileProbe::Occupied];
            self
        }

        fn with_keyfile_probe_failure(mut self) -> Self {
            self.keyfile_probes = vec![AddPlanKeyfileProbe::Failure];
            self
        }

        fn with_keyfile_probes(mut self, probes: Vec<AddPlanKeyfileProbe>) -> Self {
            self.keyfile_probes = probes;
            self
        }

        fn pool_underlying(index: usize) -> String {
            format!("/dev/vd{}", (b'b' + index as u8) as char)
        }
    }

    impl CommandRunner for AddPlanTestRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    // total = real devices + missing_count placeholders.
                    let real_devices = self.keyfile_probes.len() as u64;
                    let total = real_devices + self.missing_count;
                    let mut out = format!(
                        "Label: none  uuid: {POOL_FSID}\n\
                         \tTotal devices {total} FS bytes used 16.17MiB\n"
                    );
                    for i in 0..self.keyfile_probes.len() {
                        let devid = i + 1;
                        out.push_str(&format!(
                            "\tdevid    {devid} size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk{devid}\n"
                        ));
                    }
                    for i in 0..self.missing_count {
                        let devid = real_devices + 1 + i;
                        out.push_str(&format!("\tdevid    {devid} size 0 used 0 path MISSING\n"));
                    }
                    Ok(mock_ok(
                        &format!("btrfs filesystem show {mount_point}"),
                        &out,
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => {
                    let Some(suffix) = mapper.strip_prefix("braid-disk") else {
                        return Err(CmdError::MissingMock);
                    };
                    let index = suffix
                        .parse::<usize>()
                        .map_err(|_| CmdError::MissingMock)?
                        .checked_sub(1)
                        .ok_or(CmdError::MissingMock)?;
                    if index >= self.keyfile_probes.len() {
                        return Err(CmdError::MissingMock);
                    }
                    let underlying = Self::pool_underlying(index);
                    Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  \
                             type:    LUKS2\n  device:  {underlying}\n  mode:    read/write\n"
                        ),
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    if let Some(index) =
                        self.keyfile_probes
                            .iter()
                            .enumerate()
                            .find_map(|(index, _)| {
                                let disk = index + 1;
                                let underlying = Self::pool_underlying(index);
                                let by_id = format!("/dev/disk/by-id/virtio-disk{disk}");
                                (device == &underlying || device == &by_id).then_some(index)
                            })
                    {
                        Ok(mock_ok(
                            &format!("cryptsetup luksUUID {device}"),
                            &format!("11111111-1111-1111-1111-11111111111{index}\n"),
                        ))
                    } else {
                        Ok(RawCommandOutput {
                            cmd: format!("cryptsetup luksUUID {device}"),
                            stdout: String::new(),
                            stderr: "Device is not a valid LUKS device.\n".into(),
                            exit_status: 1,
                        })
                    }
                }
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                CmdRequest::CryptsetupLuksDump { .. } => {
                    let CmdRequest::CryptsetupLuksDump { device } = request else {
                        unreachable!();
                    };
                    let Some((index, probe)) = self
                        .keyfile_probes
                        .iter()
                        .enumerate()
                        .find(|(index, _)| device == &Self::pool_underlying(*index))
                    else {
                        return Err(CmdError::MissingMock);
                    };
                    match probe {
                        AddPlanKeyfileProbe::Empty => Ok(mock_ok(
                            "cryptsetup luksDump --dump-json-metadata",
                            r#"{"keyslots":{"0":{"type":"luks2"}}}"#,
                        )),
                        AddPlanKeyfileProbe::Occupied => Ok(mock_ok(
                            "cryptsetup luksDump --dump-json-metadata",
                            r#"{"keyslots":{"0":{"type":"luks2"},"1":{"type":"luks2"}}}"#,
                        )),
                        AddPlanKeyfileProbe::Failure => Ok(RawCommandOutput {
                            cmd: format!("cryptsetup luksDump --dump-json-metadata {device}"),
                            stdout: String::new(),
                            stderr: format!(
                                "forced luksDump failure on existing disk {}",
                                index + 1
                            ),
                            exit_status: 5,
                        }),
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

    /// Build a fresh-disk AddParams pointing `disk2` at a PresentNotLuks
    /// fixture. The caller supplies the runner; this helper owns the
    /// config, paths, and inhibitor lifetimes so each test stays small.
    struct PlanAddFixture {
        _state_tmp: tempfile::TempDir,
        paths: StatePaths,
        _tmp: tempfile::TempDir,
        config_path: std::path::PathBuf,
        pass_path: std::path::PathBuf,
        inhibitor: crate::inhibit::RecordingInhibitor,
    }

    fn plan_add_fixture() -> PlanAddFixture {
        let (state_tmp, paths, tmp, config_path, pass_path) = add_test_setup();
        PlanAddFixture {
            _state_tmp: state_tmp,
            paths,
            _tmp: tmp,
            config_path,
            pass_path,
            inhibitor: crate::inhibit::RecordingInhibitor::new(),
        }
    }

    impl PlanAddFixture {
        fn params<'a>(&'a self, disk_specs: &'a [String], dry_run: bool) -> AddParams<'a> {
            AddParams {
                config_path: &self.config_path,
                disk_specs,
                dry_run,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(self.pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &self.paths,
                sleep_inhibitor: &self.inhibitor,
                passphrase_reader: &RealTty,
            }
        }
    }

    /* Intent: plan_add surfaces a pool's missing devices as a single
     * PreviewNote::Warn whose body is exactly the output of
     * format_add_missing_devices_warning, with no legacy `warning:` prefix.
     * Why it exists: PR 7 moves the missing-devices diagnostic from a
     * direct stderr eprintln! into plan.notes. The renderer owns the
     * `[warn] ` / `warning: ` wrapping; a regression that leaks the
     * legacy prefix into the body would double up as `[warn] warning:
     * pool has...` on dry-run stdout.
     * Scenario: 1 real device + 1 MISSING placeholder, operator tries to
     * add a fresh disk2.
     */
    #[test]
    fn plan_add_missing_devices_becomes_single_warn_note() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new().with_missing(1);

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let report = plan_add(&runner, &fs, &fixture.params(&disk_specs, true));
        let plan = report.expect("plan_add must succeed even with a missing device present");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "expected exactly one Warn note, got {warns:?}"
        );
        assert_eq!(warns[0], &format_add_missing_devices_warning(1));
        assert!(
            !warns[0].starts_with("warning:"),
            "warn note body must not carry the legacy `warning:` prefix"
        );
    }

    /* Intent: plan_add emits exactly one PreviewNote::Warn with the
     * keyfile-asymmetry body when the existing pool has keyslot-1 enrolled
     * but the add omits `--enroll`.
     * Why it exists: PR 7 routes the legacy WARNING eprintln! through the
     * shared helper. A regression that left the `WARNING:` prefix baked
     * into the body would stack as `[warn] WARNING: ...` on dry-run.
     * Scenario: 1-disk pool with keyfile on disk1, operator adds a fresh
     * disk2 without --enroll.
     */
    #[test]
    fn plan_add_keyfile_asymmetry_becomes_warn_note() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new().with_keyfile();

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let report = plan_add(&runner, &fs, &fixture.params(&disk_specs, true));
        let plan = report.expect("plan_add should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "expected exactly one keyfile-asymmetry Warn, got {warns:?}"
        );
        assert_eq!(warns[0], &format_keyfile_asymmetry_warning());
        assert!(
            !warns[0].starts_with("WARNING:"),
            "warn note body must not carry the legacy `WARNING:` prefix"
        );
    }

    /* Intent: a failed keyfile-enrollment probe becomes a PreviewNote::Warn
     * with the exact shared body formatter output.
     * Why it exists: failed probes must be caller-routed diagnostics, not
     * direct stderr writes from the LUKS helper.
     * Scenario: a mounted pool has one existing member whose luksDump
     * fails while the operator previews adding a fresh disk without
     * --enroll.
     */
    #[test]
    fn plan_add_keyfile_probe_failure_becomes_warn_note() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new().with_keyfile_probe_failure();

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let report = plan_add(&runner, &fs, &fixture.params(&disk_specs, true));
        let plan = report.expect("plan_add should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "expected exactly one probe-failure Warn, got {warns:?}"
        );
        assert_eq!(
            warns[0],
            "could not check keyfile enrollment on /dev/vdb: cryptsetup luksDump failed (exit 5): forced luksDump failure on existing disk 1; proceeding as if no keyfile is enrolled"
        );
    }

    /* Intent: when both the missing-devices and keyfile-asymmetry
     * conditions hold simultaneously, plan.notes carries exactly two Warn
     * notes in the canonical order: missing-devices FIRST, keyfile
     * SECOND.
     * Why it exists: the real-run execute path replays plan.notes in
     * insertion order to preserve today's eprintln! sequence (missing
     * first at add.rs:348, keyfile second at :365). Swapping the two
     * would change the stderr order a user sees.
     * Scenario: 1 real + 1 MISSING, existing pool has keyslot-1, operator
     * adds a fresh disk2 without --enroll.
     */
    #[test]
    fn plan_add_warn_notes_preserve_missing_before_keyfile_order() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new().with_missing(1).with_keyfile();

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let report = plan_add(&runner, &fs, &fixture.params(&disk_specs, true));
        let plan = report.expect("plan_add should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(warns.len(), 2, "expected two Warn notes, got {warns:?}");
        assert_eq!(
            warns[0],
            &format_add_missing_devices_warning(1),
            "missing-devices warning must come first"
        );
        assert_eq!(
            warns[1],
            &format_keyfile_asymmetry_warning(),
            "keyfile-asymmetry warning must come second"
        );
    }

    /* Intent: when missing-device and keyfile-probe uncertainty warnings
     * both apply, the missing-device warning remains first.
     * Why it exists: add warning order is user-facing and the new probe
     * uncertainty warning must append after existing pool-health context.
     * Scenario: one missing device plus a failed luksDump probe on the
     * remaining live member while previewing a fresh add without --enroll.
     */
    #[test]
    fn plan_add_keyfile_probe_failure_orders_after_missing_warning() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new()
            .with_missing(1)
            .with_keyfile_probe_failure();

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let report = plan_add(&runner, &fs, &fixture.params(&disk_specs, true));
        let plan = report.expect("plan_add should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(warns.len(), 2, "expected two Warn notes, got {warns:?}");
        assert_eq!(
            warns[0],
            &format_add_missing_devices_warning(1),
            "missing-device warning must remain first"
        );
        assert!(
            warns[1].starts_with("could not check keyfile enrollment on /dev/vdb:"),
            "probe-failure warning must come second, got: {}",
            warns[1]
        );
    }

    /* Intent: once any existing device proves slot 1 is occupied, plan_add
     * emits the keyfile-asymmetry warning and suppresses probe-failure
     * uncertainty warnings.
     * Why it exists: an occupied slot resolves the user's action item; a
     * redundant uncertainty note would be noisy and less actionable.
     * Scenario: disk1's luksDump fails, disk2 reports slot 1 occupied, and
     * the operator previews adding raw disk3 without --enroll.
     */
    #[test]
    fn plan_add_keyfile_probe_failure_suppressed_when_enrollment_proven() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let runner = AddPlanTestRunner::new().with_keyfile_probes(vec![
            AddPlanKeyfileProbe::Failure,
            AddPlanKeyfileProbe::Occupied,
        ]);

        let disk_specs = ["disk3=/dev/disk/by-id/virtio-disk3".to_string()];
        let report = plan_add(&runner, &fs, &fixture.params(&disk_specs, true));
        let plan = report.expect("plan_add should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(warns.len(), 1, "expected one Warn note, got {warns:?}");
        assert_eq!(
            warns[0],
            &format_keyfile_asymmetry_warning(),
            "occupied slot 1 must emit only the keyfile-asymmetry warning"
        );
    }

    // Intent: pin the shared `braid add` no-op and done message helpers.
    //
    // Why it exists: the no-op and done paths both need the same disk-name
    //   list formatting; keeping their exact grammar under one test prevents
    //   a future inline formatter from drifting again.
    //
    // Scenario: the operator adds one disk or multiple disks, and the CLI
    //   reports either an already-in-pool no-op or a completed add.
    #[test]
    fn format_add_messages_pin_disk_name_list_and_grammar() {
        let single = vec!["disk2".to_string()];
        let multi = vec!["disk1".to_string(), "disk2".to_string()];

        assert_eq!(
            format_add_noop(&single),
            "Nothing to do -- disk2 already in pool."
        );
        assert_eq!(
            format_add_noop(&multi),
            "Nothing to do -- disk1, disk2 already in pool."
        );
        assert_eq!(
            format_add_done(&single),
            "Done. disk2 is now part of the pool."
        );
        assert_eq!(
            format_add_done(&multi),
            "Done. disk1, disk2 are now part of the pool."
        );
    }

    /* Intent: adding a disk that is already in the pool is a note-only
     * success: plan.preview().render() outputs exactly the
     * no-op message line, no `nothing to do.` fallback, no step
     * lines.
     * Why it exists: PR 7's dry-run contract requires already-in-pool to
     * become a Preview with zero steps + one Info note. A regression that
     * dropped the Info note would surface `nothing to do.` (generic
     * fallback) instead of the specific per-disk message. Complement is
     * that on the real-run path `no_journal_on_noop_add` still pins the
     * "no journal, no inhibitor" invariants.
     * Scenario: disk2 is already a pool member (AddMockFs + disk_in_pool).
     */
    #[test]
    fn plan_add_already_in_pool_is_note_only_success() {
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

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let report = plan_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &disk_specs,
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
            },
        );
        let plan = report.expect("plan_add should succeed for noop");
        let preview = plan.preview();
        assert!(
            preview.steps.is_empty(),
            "note-only success must have zero steps, got: {:?}",
            preview.steps
        );

        let rendered = preview.render();
        let expected = "Nothing to do -- disk2 already in pool.\n";
        assert_eq!(rendered, expected, "exact render must match noop Info line");
        assert!(
            !rendered.contains("nothing to do."),
            "generic `nothing to do.` fallback must NOT appear alongside the Info note"
        );
    }

    /* Intent: dry-run render for a fresh single-disk bootstrap goes
     * through plan_add().preview().render() end-to-end and includes the
     * LUKS init + mkfs + mount step block with zero accumulated notes on
     * this clean-bootstrap path.
     * Why it exists: the previous render test called
     * Step::render_dry_run(&steps) directly. Moving the assertion to the
     * plan boundary catches regressions where plan_add forgets to compile
     * steps, or introduces spurious notes on a fresh bootstrap.
     * Scenario: empty membership, fresh disk1 probed as PresentNotLuks,
     * pool unmounted.
     */
    #[test]
    fn plan_add_dry_run_render_fresh_single_disk_bootstrap() {
        // Fresh state (no pre-seeded membership) so the test exercises
        // the true bootstrap path.
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();

        let fs = AddOfflineMockFs(vec!["/dev/disk/by-id/virtio-disk1".into()]);
        // AddRecordingRunner with pool_mounted=false drives plan_add's
        // unmounted-pool branch and simulates disk1 as PresentNotLuks.
        let runner = AddRecordingRunner::new(false);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let disk_specs = ["disk1=/dev/disk/by-id/virtio-disk1".to_string()];
        let report = plan_add(
            &runner,
            &fs,
            &AddParams {
                config_path: &config_path,
                disk_specs: &disk_specs,
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                passphrase_reader: &RealTty,
            },
        );
        let plan = report.expect("plan_add should succeed for fresh bootstrap");
        assert!(
            plan.notes.is_empty(),
            "fresh bootstrap must emit no warning/info notes, got: {:?}",
            plan.notes
        );
        let rendered = plan.preview().render();
        assert!(
            rendered.contains("LUKS format"),
            "render must include destructive LUKS format step; got: {rendered}"
        );
        assert!(
            rendered.contains("mkfs.btrfs"),
            "render must include mkfs.btrfs step; got: {rendered}"
        );
        assert!(
            rendered.contains("mount"),
            "render must include mount step; got: {rendered}"
        );
    }

    /* Intent: when a Warn note is present and steps are non-empty, the
     * preview renders the note BEFORE the step block.
     * Why it exists: PR 7's preview contract is notes-first, then steps.
     * A regression that reversed the order would surface warnings after
     * the destructive plan, defeating their purpose.
     * Scenario: missing-devices warning on a pool, add a fresh disk with
     * real work to plan. Pool is mounted so add work-plan rendering
     * returns the `btrfs device add` + balance steps.
     */
    #[test]
    fn plan_add_render_emits_warn_above_steps() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new().with_missing(1);

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let report = plan_add(&runner, &fs, &fixture.params(&disk_specs, true));
        let plan = report.expect("plan_add should succeed");

        let rendered = plan.preview().render();
        let warn_pos = rendered
            .find("[warn] pool has 1 missing device")
            .expect("missing-devices Warn must appear on stdout render");
        let steps_pos = rendered
            .find("btrfs device add")
            .expect("device-add step must appear on stdout render");
        assert!(
            warn_pos < steps_pos,
            "Warn note must render above the step block; got:\n{rendered}"
        );
    }

    /* Intent: the plan-derived Warn notes for `add` render through
     * ONE contract -- the shared `preview::render_notes_for_stderr`
     * helper -- across dry-run stdout (via `Preview::render`),
     * real-run stderr (via `AddPlan::execute`), and preserved-context
     * Err stderr (via `cmd_add`). The canonical shape is
     * `[warn] <body>`; legacy `warning:` / `WARNING:` prefixes are
     * gone.
     * Why it exists: a previous iteration of this PR replayed
     * plan-derived Warn notes with the legacy prefixes on the Ok path
     * while the Err path used `[warn] ...`, producing two different
     * wordings for the same note. This test pins the unified
     * rendering at the renderer layer so any drift -- a stray
     * `warning:` prefix reintroduced into a body, an Info-note leak,
     * or a PerDisk-note leak -- fails here instead of silently
     * diverging between paths.
     * Scenario: a notes vec with every variant plus both add-specific
     * Warn kinds, rendered via the shared helper with
     * `AddPlan::STDERR_STYLE`.
     */
    #[test]
    fn add_warn_notes_render_canonical_bracketed_form() {
        let notes = vec![
            PreviewNote::Info("Nothing to do -- disk2 already in pool.".into()),
            PreviewNote::Warn(format_add_missing_devices_warning(1)),
            PreviewNote::Warn(format_keyfile_asymmetry_warning()),
            PreviewNote::PerDisk {
                name: "diskX".into(),
                level: crate::preview::NoteLevel::Skip,
                message: "not present".into(),
            },
        ];
        let rendered = preview::render_notes_for_stderr(&notes, AddPlan::STDERR_STYLE);
        let expected = concat!(
            "Nothing to do -- disk2 already in pool.\n",
            "[warn] pool has 1 missing device. Consider repairing with",
            " `braid replace --missing-id <devid>` first.",
            " Use `braid status` to see device IDs.\n",
            "[warn] Existing pool drives have a keyfile (keyslot-1) for auto-unlock,",
            " but the new drive will not.\n",
            "  Passphrase unlock still works, but the keyfile won't unlock the new drive",
            " until it's enrolled.\n",
            "  Fix: re-run with --enroll <dir>, or run `braid enroll <dir>` afterward.\n",
            "\n",
            "[skip] disk diskX: not present\n",
        );
        assert_eq!(rendered, expected);

        // Legacy prefixes MUST NOT appear anywhere in the canonical
        // render -- this is the intentional behavior change for `add`.
        assert!(
            !rendered.contains("warning:"),
            "legacy `warning:` prefix must be gone from add's render;\n{rendered}"
        );
        assert!(
            !rendered.contains("WARNING:"),
            "legacy `WARNING:` prefix must be gone from add's render;\n{rendered}"
        );
    }

    /* Intent: when plan_add accumulates a Warn note (e.g.
     * missing-devices) and then fails later inside
     * add work-plan rendering (e.g. BraidLabeledNoBtrfs identity), the
     * accumulated notes survive on `PlanFailure::notes` and the result is
     * Err(...).
     * Why it exists: `PlanFailure::notes` promises preserved context.
     * Without this, a refused add on a degraded pool would lose the
     * missing-devices context the user needs to understand the refusal.
     * Scenario: 2-disk pool with 1 MISSING placeholder, operator tries
     * to add disk2 which is a braid-labeled LUKS with no btrfs
     * superblock (ambiguous identity). plan_add accumulates the
     * missing-devices warn, then add work-plan rendering rejects the
     * identity.
     */
    #[test]
    fn plan_add_preserves_warn_notes_on_later_failure() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec![
            "/dev/disk/by-id/virtio-disk2".into(),
            "/dev/mapper/braid-disk2".into(),
        ]);
        // AddTestRunner supports no_btrfs_superblock AND reports the
        // pool with braid-disk1 only. Override by wrapping it with a
        // runner that also synthesizes MISSING rows.
        struct MissingAndNoBtrfsRunner {
            inner: AddTestRunner,
        }
        impl CommandRunner for MissingAndNoBtrfsRunner {
            fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
                if let CmdRequest::BtrfsFilesystemShow { mount_point } = request {
                    // 1 real device + 1 MISSING placeholder => missing_count = 1.
                    return Ok(mock_ok(
                        &format!("btrfs filesystem show {mount_point}"),
                        &format!(
                            "Label: none  uuid: {POOL_FSID}\n\
                             \tTotal devices 2 FS bytes used 16.17MiB\n\
                             \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
                             \tdevid    2 size 0 used 0 path MISSING\n"
                        ),
                    ));
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
        let runner = MissingAndNoBtrfsRunner {
            inner: AddTestRunner {
                disk_in_pool: false,
                fail_device_add: false,
                no_btrfs_superblock: true,
            },
        };

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let failure = match plan_add(&runner, &fs, &fixture.params(&disk_specs, true)) {
            Ok(_) => panic!("plan_add must fail on BraidLabeledNoBtrfs identity"),
            Err(failure) => failure,
        };
        let warns: Vec<&String> = failure
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "missing-devices warn must survive the Err branch on PlanFailure::notes, got: {warns:?}"
        );
        assert_eq!(warns[0], &format_add_missing_devices_warning(1));
    }

    /// AddMockFs variant with a configurable sysfs exclusive_operation
    /// body. Drives preflight's busy-op / paused-balance branches from
    /// the plan_add boundary tests. Existence-probe paths delegate to
    /// the inner `AddMockFs`.
    struct AddMockFsWithSysfs {
        inner: AddMockFs,
        sysfs_body: String,
    }

    impl AddMockFsWithSysfs {
        fn new(paths: Vec<String>, sysfs_body: &str) -> Self {
            Self {
                inner: AddMockFs(paths),
                sysfs_body: sysfs_body.to_owned(),
            }
        }
    }

    impl crate::probe::Filesystem for AddMockFsWithSysfs {
        fn exists(&self, path: &str) -> bool {
            self.inner.exists(path)
        }
        fn is_block_device(&self, path: &str) -> bool {
            self.inner.is_block_device(path)
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path.ends_with("/exclusive_operation") {
                Ok(self.sysfs_body.clone())
            } else {
                self.inner.read_to_string(path)
            }
        }
        fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
            self.inner.list_dir(path)
        }
    }

    /* Intent: plan_add surfaces an in-flight exclusive op as a single
     * PreviewNote::Info whose body says "waiting for in-flight <op> to
     * finish...", and the rendered preview contains that line above
     * the step block.
     * Why it exists: PR 7 moves the busy-op diagnostic from a direct
     * stderr eprintln! into plan.notes. A regression that leaked the
     * wording back to stderr would leave dry-run stdout silent about
     * the enqueue and also break the empty-stderr contract.
     * Scenario: sysfs reports "device add" while the operator runs
     * `braid add disk2 --dry-run` against an otherwise healthy pool.
     */
    #[test]
    fn plan_add_preflight_busy_op_becomes_info_note() {
        let fixture = plan_add_fixture();
        let fs =
            AddMockFsWithSysfs::new(vec!["/dev/disk/by-id/virtio-disk2".into()], "device add\n");
        let runner = AddPlanTestRunner::new();

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let report = plan_add(&runner, &fs, &fixture.params(&disk_specs, true));
        let plan = report.expect("plan_add should succeed on clean fixture + busy op");

        assert_eq!(
            plan.notes.len(),
            1,
            "expected one preflight Info note, got {:?}",
            plan.notes
        );
        assert!(
            matches!(
                &plan.notes[0],
                PreviewNote::Info(b) if b.contains("waiting for in-flight") && b.contains("device add")
            ),
            "notes[0]={:?}",
            plan.notes[0],
        );

        let rendered = plan.preview().render();
        assert!(
            rendered.contains("waiting for in-flight device add"),
            "rendered preview must carry the busy-op Info line, got:\n{rendered}",
        );
    }

    /* Intent: when plan_add accumulates a preflight Info note and then
     * fails on a later hard gate (UPS on battery), the accumulated notes
     * survive on `PlanFailure::notes` with `Err(...)`.
     * Why it exists: `PlanFailure::notes` promises preserved context.
     * Without this, a UPS refusal on an enqueued-busy pool would lose
     * the busy-op context the operator needs to understand what else is
     * happening.
     * Scenario: sysfs reports "device add" (enqueueable busy), UPS
     * reports OB (on battery), operator runs `braid add disk2`.
     */
    #[test]
    fn plan_add_preserves_preflight_notes_on_ups_failure() {
        let fixture = plan_add_fixture();
        let fs =
            AddMockFsWithSysfs::new(vec!["/dev/disk/by-id/virtio-disk2".into()], "device add\n");
        // Custom runner: delegates to AddPlanTestRunner for everything
        // except the UPS query, which returns OB. We need the UPS
        // config attached to params; construct a custom config.
        struct UpsOnBatteryRunner {
            inner: AddPlanTestRunner,
        }
        impl CommandRunner for UpsOnBatteryRunner {
            fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
                match request {
                    CmdRequest::UpscQuery { name } => Ok(RawCommandOutput {
                        cmd: format!("upsc {name}"),
                        stdout: "ups.status: OB\n".into(),
                        stderr: String::new(),
                        exit_status: 0,
                    }),
                    _ => self.inner.run(request),
                }
            }
            fn run_with_stdin(
                &self,
                request: &CmdRequest,
                stdin: &[u8],
            ) -> Result<RawCommandOutput, CmdError> {
                self.inner.run_with_stdin(request, stdin)
            }
        }
        let runner = UpsOnBatteryRunner {
            inner: AddPlanTestRunner::new(),
        };

        // Build a config.json that enables a UPS named "ups".
        let config_json = serde_json::json!({
            "mount_point": "/mnt/storage",
            "ups": { "enable": true, "name": "ups" },
        });
        std::fs::write(
            &fixture.config_path,
            serde_json::to_vec(&config_json).unwrap(),
        )
        .unwrap();

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let failure = match plan_add(&runner, &fs, &fixture.params(&disk_specs, true)) {
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
            Err(failure) => failure,
        };

        match &failure.error {
            AddError::Validation(msg) => {
                assert!(
                    msg.contains("utility power"),
                    "expected UPS refusal wording, got: {msg}"
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert_eq!(
            failure.notes.len(),
            1,
            "busy-op Info note must survive the UPS failure on PlanFailure::notes, got: {:?}",
            failure.notes,
        );
        assert!(
            matches!(
                &failure.notes[0],
                PreviewNote::Info(b) if b.contains("waiting for in-flight") && b.contains("device add")
            ),
            "notes[0]={:?}",
            failure.notes[0],
        );
    }

    /* Intent: when both pending-op.json and a locked pool with non-empty
     *   membership are present, plan_add returns the pending-op error,
     *   not the locked-pool error.
     *
     * Why it exists: pins the ordering claim that
     *   `check_no_pending_operation` runs before
     *   `check_pool_unlocked_if_membership_exists` in plan_add. Without
     *   this test, a future refactor could swap the order and hide the
     *   more-urgent pending-op signal behind the locked-pool refusal --
     *   the operator would see "run `braid unlock`" when the real issue
     *   is an interrupted operation that needs `braid recover`. The test
     *   distinguishes the two plausible orderings at the seam.
     *
     * Scenario: a previous add was interrupted (pending-op.json exists)
     *   and the pool is locked with disk1 in membership. Operator runs
     *   `braid add disk2=...`. The pending-op error must surface.
     */
    #[test]
    fn plan_add_pending_op_wins_over_locked_pool_refusal() {
        let fixture = plan_add_fixture();

        // Seed pending-op.json. add_test_setup pre-seeded pool.json with
        // disk1, so both the pending-op condition and the locked-pool
        // condition are simultaneously true.
        std::fs::write(
            fixture.paths.pending_op_json(),
            r#"{"started_at":"2024-01-01T00:00:00Z","op":{"op":"Add","phase":"PoolMutation","targets":{}},"pre_membership":{"disks":{}},"target_membership":{"disks":{}}}"#,
        )
        .unwrap();

        // Runner is unused: pending-op short-circuits at the top of
        // plan_add, before any disk probe or pool probe.
        let runner = MockRunner::default();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let failure = match plan_add(&runner, &fs, &fixture.params(&disk_specs, true)) {
            Ok(_) => panic!("plan_add must fail when pending-op.json is present"),
            Err(failure) => failure,
        };
        let err = failure.error.to_string();
        assert!(
            err.contains("interrupted operation detected"),
            "pending-op error must win over locked-pool refusal, got: {err}"
        );
        assert!(
            !err.contains("not unlocked"),
            "locked-pool error must NOT preempt the pending-op error, got: {err}"
        );
    }
}
