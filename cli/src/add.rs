use crate::alert;
use crate::cmd::{CmdError, CmdRequest, CommandRunner, Step};
use crate::config::{Config, luks_label_for, mapper_name};
use crate::confirm;
use crate::credential_verify::{
    Credential, CredentialVerifyError, CredentialVerifyTarget, verify_credential_for_targets,
};
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal;
use crate::luks::{
    BackingPathResolver, HeaderBackupPath, KeySlotState, LUKS_SLOT_KEYFILE, OpenOutcome,
    PassphraseReader, backup_luks_header_post_mutation, check_key_slot, ensure_luks_open,
    format_keyfile_asymmetry_warning, format_keyfile_enrollment_probe_failure,
    format_target_keyfile_probe_failure, luks_format, luks_header_backup_path,
    probe_pool_keyfile_enrollment, read_passphrase_with,
};
use crate::mapper_close::{CloseContext, TrackedMapper, close_mapper_best_effort};
use crate::membership::{self, DiskMember, LuksUuidMap, PoolMembership};
use crate::parse::btrfs_filesystem_show::{DeviceBtrfsProbe, classify_btrfs_probe};
use crate::parse::{parse_btrfs_filesystem_show, parse_cryptsetup_luks_uuid};
use crate::pool::{
    pool_add_device, pool_balance_raid1, pool_bootstrap_mount, pool_bootstrap_mount_raid1,
    pool_can_host_raid1,
};
use crate::preflight;
use crate::preview::{self, PerDiskStyle, PlanFailure, Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{Filesystem, ProbeError, probe_config_disk, probe_pool};
use crate::progress::ProgressOutput;
use crate::progress::RealSleeper;
use crate::repair_hint;
use crate::state_paths::StatePaths;
use crate::status_tag::{StatusTag, color_enabled_for_stderr, emit_status, status_line};
use crate::types::*;
use std::path::Path;

/// Errors raised by `braid add` planning and execution. Two refusals cover
/// duplicate LUKS UUIDs, both raised from planning BEFORE any
/// `LuksUuidMap::insert` on the journal `targets` map AND BEFORE
/// `PoolMembership::insert`. `DuplicateUuid` is the discover-symmetric
/// refusal for the in-flight and membership arms, where both colliding
/// parties are real, legitimately-resolved identities, so it names both
/// `(name, by_id)` pairs explicitly rather than falling through to the
/// generic `MembershipError::Conflict`. `DuplicateUuidLivePool` is the
/// live-pool arm, where the colliding device is a foreign/cloned btrfs
/// member absent from membership: per ADR 024 braid does not invent an
/// identity for the clone, so it names only the real add target and reports
/// the colliding side by scope -- mirroring `replace`'s
/// `DuplicateUuid { scope: LivePool }` name-nothing-foreign contract.
/// `ManagedFormatFlag` surfaces a rejected `--luks-format-arg` through
/// the `AddError` chain so the CLI matches a single error type at the
/// boundary.
#[derive(Debug, thiserror::Error)]
pub enum AddError {
    #[error("{0}")]
    Validation(String),
    #[error(
        "pool was modified, but acked-stats cleanup failed at {stage}: {detail}\n\
         pending-op.json is preserved -- rm /var/lib/braid/acked-stats.json before trusting \
         `braid monitor`, then run `braid recover` to finish."
    )]
    AckCleanupFailed { stage: &'static str, detail: String },
    /// Post-mutation, pre-persist failure in the live-pool add loop:
    /// `btrfs device add` already committed membership, but the follow-up
    /// `probe_pool` failed (or did not yet list the new device), so braid
    /// stopped before `save_membership` wrote pool.json. Distinct from
    /// `AckCleanupFailed` because acked-stats was never reached -- the
    /// PoolMutation journal is still pending, so the remediation is
    /// `braid recover` (which replays the journal and skips already-live
    /// members), not deleting alert baselines.
    #[error(
        "disk added to pool, but pool.json was not persisted: {detail}\n\
         pending-op.json is preserved -- run `braid recover` to finish persisting pool membership."
    )]
    PostAddProbeFailed { detail: String },
    /// Pre-journal-write refusal: a target's LUKS UUID collides with
    /// another in-flight add target or an existing pool member. Raised by
    /// `assert_target_uuid_unique` before journal write, before any
    /// `CryptsetupLuksFormat`, and before any `PoolMembership::insert`,
    /// so the operator-facing message names both `(name, by_id)` pairs
    /// and suggests cloning as the typical cause. The live `pool.devices`
    /// collision arm is `DuplicateUuidLivePool`, not this variant. Mirrors
    /// `DiscoverError::DuplicateUuid`.
    #[error(
        "duplicate LUKS UUID: braid-{name1} ({by_id1}) and braid-{name2} ({by_id2}) share UUID {uuid} -- detach the cloned or unintended disk before retrying (this typically indicates a dd-cloned disk)"
    )]
    DuplicateUuid {
        uuid: LuksUuid,
        name1: DiskName,
        by_id1: ByIdPath,
        name2: DiskName,
        by_id2: ByIdPath,
    },
    /// Live-pool UUID collision: an add target's LUKS UUID matches a
    /// device already live in the btrfs pool but absent from membership
    /// (a foreign/cloned device). Per ADR 024 braid does not invent an
    /// identity for the clone, so this names only the real add target and
    /// reports the colliding side by scope -- mirroring
    /// `ReplaceError::DuplicateUuid { scope: LivePool }`.
    #[error(
        "duplicate LUKS UUID {uuid}: add target braid-{name} ({by_id}) \
         collides with a device already in the live pool -- detach the \
         cloned or unintended disk before retrying (this typically \
         indicates a dd-cloned disk)"
    )]
    DuplicateUuidLivePool {
        uuid: LuksUuid,
        name: DiskName,
        by_id: ByIdPath,
    },
    /// Closed-PresentLuks target identity drift between planning-time
    /// probe and the live disk immediately before Pass 1
    /// `ensure_luks_open`. Mirrors
    /// `ReplaceError::NewTargetUuidMismatchAtOpen` so operator
    /// remediation reads identically across add and replace.
    #[error(
        "add target '{by_id}' LUKS UUID mismatch: expected {expected}, found {observed} -- detach the foreign disk and retry"
    )]
    TargetUuidMismatchAtOpen {
        by_id: ByIdPath,
        expected: LuksUuid,
        observed: String,
    },
    /// Operator passed `--luks-format-arg` containing a braid-managed
    /// cryptsetup option (`--uuid`, `--label`). Surfaced through
    /// `AddError` (rather than `LuksFormatExtraOptsError` directly) so
    /// the CLI matches one error type at the boundary.
    #[error("{0}")]
    ManagedFormatFlag(#[from] LuksFormatExtraOptsError),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("luks error: {0}")]
    Luks(#[from] crate::luks::LuksError),
    #[error("pool error: {0}")]
    Pool(#[from] crate::pool::PoolError),
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

/// Btrfs-only probe result for a braid-labeled PresentLuks mapper.
/// Live-pool membership is decided separately from LUKS UUID plus
/// backing-path proof so mapper names never become identity.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AddLuksBtrfsProbe {
    /// Correct braid label, mapper open, no btrfs superblock.
    NoBtrfs,
    /// Correct braid label, mapper open, btrfs FSID differs from pool.
    ForeignPool,
    /// Correct braid label, mapper open, btrfs FSID matches pool.
    SamePool,
}

/// Live-pool correlation for a PresentLuks target after UUID match.
/// A UUID match only counts as ownership when the candidate and live
/// pool row also resolve to the same backing block device. All variants
/// are unit: per ADR 024 a different-backing (cloned/foreign) row is
/// refused by scope alone, so no foreign `PoolDevice` handle is carried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LivePoolMatch {
    /// The target is already represented by a live row with the same backing.
    SameBacking,
    /// A live row carries the target's UUID under a different backing path
    /// (a cloned/foreign device, refused by scope per ADR 024).
    DifferentBacking,
    NoMatch,
}

/// Validate the preconditions for adding a PresentLuks disk.
/// Checks the cached LUKS label and mounted pool state.
/// No side effects -- works on the raw device, no mapper required.
fn validate_braid_preconditions(
    name: &DiskName,
    device: &str,
    label: Option<&str>,
    pool: &PoolState,
) -> Result<(), AddError> {
    let expected_label = luks_label_for(name);
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
) -> Result<AddLuksBtrfsProbe, AddError> {
    let mapper_path = mapper.dev_path();
    let show_raw = runner.run(&CmdRequest::BtrfsFilesystemShowTarget {
        target: mapper_path,
    })?;

    match classify_btrfs_probe(&show_raw) {
        DeviceBtrfsProbe::NoBtrfs => return Ok(AddLuksBtrfsProbe::NoBtrfs),
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
        return Ok(AddLuksBtrfsProbe::ForeignPool);
    }

    Ok(AddLuksBtrfsProbe::SamePool)
}

/// Map an AddLuksBtrfsProbe error variant to a canonical AddError.
/// Returns None when the mapper's btrfs FSID matches the mounted pool.
fn identity_to_error(identity: &AddLuksBtrfsProbe, name: &str) -> Option<AddError> {
    match identity {
        AddLuksBtrfsProbe::NoBtrfs => Some(AddError::Validation(format!(
            "disk '{}' is braid-labeled but contains no btrfs superblock; \
             identity is ambiguous, so braid will not re-add it automatically. \
             Wipe the disk and add it again as fresh.",
            name,
        ))),
        AddLuksBtrfsProbe::ForeignPool => Some(AddError::Validation(format!(
            "disk '{}' is a braid-managed device from a different btrfs filesystem; \
             braid will not merge foreign pools",
            name,
        ))),
        AddLuksBtrfsProbe::SamePool => None,
    }
}

/// Classify live-pool membership for an add target by UUID and backing path.
/// Different backing dominates so cloned LUKS headers fail closed even if
/// another live row proves the candidate itself is already open.
fn classify_live_pool_match(
    target_uuid: &LuksUuid,
    target_by_id: &ByIdPath,
    pool: &PoolState,
    resolver: &dyn BackingPathResolver,
) -> Result<LivePoolMatch, AddError> {
    let target_backing = resolver
        .canonicalize(target_by_id.as_str())
        .map_err(|source| {
            AddError::Validation(format!(
                "could not canonicalize add target backing path '{}': {source}",
                target_by_id
            ))
        })?;
    let mut same_backing = false;
    let mut different_backing = false;

    for device in pool
        .devices
        .iter()
        .filter(|device| device.luks_uuid == *target_uuid)
    {
        let live_backing = resolver
            .canonicalize(&device.underlying)
            .map_err(|source| {
                AddError::Validation(format!(
                    "could not canonicalize live pool backing path '{}' for mapper '{}': {source}",
                    device.underlying, device.mapper
                ))
            })?;
        if live_backing != target_backing {
            different_backing = true;
        } else {
            same_backing = true;
        }
    }

    if different_backing {
        Ok(LivePoolMatch::DifferentBacking)
    } else if same_backing {
        Ok(LivePoolMatch::SameBacking)
    } else {
        Ok(LivePoolMatch::NoMatch)
    }
}

/// Execute-time pool identity guard so a plan cannot be replayed against a
/// different mount state or btrfs filesystem before journal write.
fn validate_execute_pool_identity(
    planned_pool: &PoolState,
    fresh_pool: &PoolState,
    mount_point: &MountPoint,
) -> Result<(), AddError> {
    if fresh_pool.mounted != planned_pool.mounted {
        if planned_pool.mounted {
            return Err(AddError::Validation(format!(
                "pool unmounted between planning and execution -- aborting before journal write. Re-mount {mount_point} and re-run `braid add`."
            )));
        }
        return Err(AddError::Validation(format!(
            "a pool appeared at {mount_point} between planning and execution -- aborting before `mkfs.btrfs`. braid will not bootstrap on top of a live filesystem; identify the mounted pool and unmount it (or unify your config) before re-running `braid add`."
        )));
    }

    if planned_pool.mounted && fresh_pool.mounted && fresh_pool.fsid != planned_pool.fsid {
        let planned = planned_pool
            .fsid
            .as_ref()
            .map(Fsid::as_str)
            .unwrap_or("<unknown>");
        let fresh = fresh_pool
            .fsid
            .as_ref()
            .map(Fsid::as_str)
            .unwrap_or("<unknown>");
        return Err(AddError::Validation(format!(
            "pool fsid changed between planning and execution (was {planned}, now {fresh}) -- aborting before journal write. The pool you planned against is no longer the same filesystem."
        )));
    }

    Ok(())
}

/// Re-check pending add targets against a fresh live-pool probe after Pass 1
/// so confirmation-time races still hit the canonical duplicate-UUID refusal.
fn recheck_execute_live_pool_targets(
    journal_targets: &LuksUuidMap<journal::AddJournalTarget>,
    fresh_pool: &PoolState,
    resolver: &dyn BackingPathResolver,
) -> Result<(), AddError> {
    for (uuid, target) in journal_targets {
        match classify_live_pool_match(uuid, &target.by_id, fresh_pool, resolver)? {
            LivePoolMatch::NoMatch => {}
            LivePoolMatch::DifferentBacking => {
                return Err(duplicate_live_pool_uuid_error(
                    uuid,
                    &target.name,
                    &target.by_id,
                ));
            }
            LivePoolMatch::SameBacking => {
                return Err(AddError::Validation(format!(
                    "pool state changed between planning and execution -- disk '{}' (UUID `{}`) is now a live pool member. Re-run `braid add` to converge.",
                    target.name, uuid
                )));
            }
        }
    }

    Ok(())
}

/// Open-boundary re-probe for ClosedPresentLuks before mapper open.
/// Mirrors replace's ExistingLuks gate so a by-id swap is rejected
/// before braid opens a foreign LUKS volume.
fn probe_closed_present_luks_target_uuid<R: CommandRunner>(
    runner: &R,
    by_id: &ByIdPath,
    expected: &LuksUuid,
) -> Result<(), AddError> {
    let probe = runner
        .run(&CmdRequest::CryptsetupLuksUuid {
            device: by_id.as_str().to_owned(),
        })
        .map_err(|e| AddError::TargetUuidMismatchAtOpen {
            by_id: by_id.clone(),
            expected: expected.clone(),
            observed: format!("probe failed: {e}"),
        })?;
    match parse_cryptsetup_luks_uuid(&probe) {
        Ok(parsed) if parsed.uuid == *expected => Ok(()),
        Ok(parsed) => Err(AddError::TargetUuidMismatchAtOpen {
            by_id: by_id.clone(),
            expected: expected.clone(),
            observed: parsed.uuid.as_str().to_owned(),
        }),
        Err(e) => Err(AddError::TargetUuidMismatchAtOpen {
            by_id: by_id.clone(),
            expected: expected.clone(),
            observed: format!("probe parse failed: {e}"),
        }),
    }
}

/// Tracks LUKS mappers opened by this invocation of cmd_add.
/// On drop (error path), closes them best-effort.
/// Call `disarm()` on the success path to skip cleanup.
struct LuksCleanupGuard<'a, R: CommandRunner> {
    runner: &'a R,
    mappers: Vec<TrackedMapper>,
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

    fn track(&mut self, name: DiskName, mapper: MapperName) {
        self.mappers.push(TrackedMapper { name, mapper });
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
        for tracked in self.mappers.iter().rev() {
            // Best-effort in Drop: the returned bool gates a post-success
            // trailer for pool-maintenance callers, but rollback cleanup has
            // none, so it is intentionally ignored here.
            let _ = close_mapper_best_effort(
                self.runner,
                &sleeper,
                &tracked.mapper,
                &tracked.name,
                CloseContext::Cleanup,
                color_enabled,
            );
        }
    }
}

struct PoolAddExecutionTarget {
    mapper_path: String,
    force: bool,
    luks_uuid: LuksUuid,
    name: DiskName,
}

#[derive(Debug, Clone)]
struct AddConfirmDiskPlan {
    name: DiskName,
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

/// Fresh-LUKS target adopted into `AddWorkPlan`. `luks_uuid` is generated
/// at planning time via `LuksUuid::new_v4()` so the journal records the
/// authoritative identity before `cryptsetup luksFormat` runs, and so a
/// mid-format crash and replay reformat under the same identity.
#[derive(Debug, Clone)]
struct FreshLuksTarget {
    name: DiskName,
    by_id: ByIdPath,
    mapper_name: MapperName,
    mapper_path: String,
    luks_uuid: LuksUuid,
    luks_format_extra_opts: LuksFormatExtraOpts,
    enroll_key_file: Option<KeyFilePath>,
    header_backup_path: HeaderBackupPath,
}

#[derive(Debug, Clone)]
struct RecoverableBraidTarget {
    name: DiskName,
    by_id: ByIdPath,
    mapper_path: String,
    luks_uuid: LuksUuid,
    verified_pool_fsid: Fsid,
    /// Keyfile to enroll into LUKS slot 1 if `add --enroll DIR` was
    /// passed against this target and the per-disk planner classified
    /// the disk as `NeedsEnroll`. `None` means either no `--enroll`
    /// flag, or the disk's slot 1 already authenticates with the
    /// supplied keyfile (idempotent skip).
    enroll_key_file: Option<KeyFilePath>,
    /// Where the post-enrollment LUKS header backup lands, computed at
    /// plan time so render_steps does not need access to `paths`.
    /// Mirrors `FreshLuksTarget::header_backup_path`. Unused when
    /// `enroll_key_file` is `None`.
    header_backup_path: HeaderBackupPath,
}

#[derive(Debug, Clone)]
struct ClosedPresentLuksCandidate {
    name: DiskName,
    by_id: ByIdPath,
    mapper_name: MapperName,
    mapper_path: String,
    luks_uuid: LuksUuid,
    /// Same semantics as `RecoverableBraidTarget::enroll_key_file`.
    /// Threaded through Pass-1 verification: when the closed disk's
    /// identity is verified at execution time, this keyfile is
    /// promoted into the runtime `RecoverableBraidTarget` and
    /// journaled, so crash-recovery can replay enrollment.
    enroll_key_file: Option<KeyFilePath>,
    /// See `RecoverableBraidTarget::header_backup_path`.
    header_backup_path: HeaderBackupPath,
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

    /// Borrow the target's display `DiskName`. Used by the sort helper
    /// that builds the operator-visible iteration order; see
    /// `AddWorkPlan::targets_sorted_by_name`.
    fn name(&self) -> &DiskName {
        match self {
            AddTargetWork::Fresh(target) => &target.name,
            AddTargetWork::OpenRecoverable(target) => &target.name,
            AddTargetWork::ClosedPresentLuks(target) => &target.name,
        }
    }

    /// Borrow the target's hardware `by_id`. Used to build the add
    /// confirmation list from the actual work targets so the prompt never
    /// names a disk that won't be added.
    fn by_id(&self) -> &ByIdPath {
        match self {
            AddTargetWork::Fresh(target) => &target.by_id,
            AddTargetWork::OpenRecoverable(target) => &target.by_id,
            AddTargetWork::ClosedPresentLuks(target) => &target.by_id,
        }
    }
}

/// Plan-time preview prediction of the add pool phase. This is preview-only:
/// `AddPlan::execute` makes the authoritative balance call independently from
/// the fresh post-add `pool_after` probe (`pool_can_host_raid1`) and may
/// diverge from this prediction when a member drops or returns after planning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AddPreviewPhase {
    /// Pool not mounted: preview renders mkfs and mount. Single-vs-RAID1
    /// topology follows the target count, so it stays local to the work plan.
    Bootstrap,
    /// Pool live: preview renders add-target work followed by a predicted
    /// balance decision.
    LiveAdd(PreviewedBalance),
}

/// Plan-time prediction of the post-device-add RAID1 hard convert. Consumed
/// only by the preview step builder and the dry-run skip note.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreviewedBalance {
    /// The plan-time pool is whole and total_after >= 2, so preview predicts
    /// the hard RAID1 convert will run.
    Run,
    /// The plan-time pool is still degraded and total_after >= 2, so preview
    /// predicts the convert is skipped and emits one skip note.
    SkipDegraded,
    /// total_after < 2; no balance and no note.
    NotApplicable,
}

/// Decide the add preview phase once from the planning pool snapshot and target
/// count so the dry-run steps and skip note share one source.
fn add_preview_phase(pool: &PoolState, target_count: usize) -> AddPreviewPhase {
    if !pool.mounted {
        return AddPreviewPhase::Bootstrap;
    }

    let total_after = pool.devices.len() + target_count;
    let balance = if total_after < 2 {
        PreviewedBalance::NotApplicable
    } else if pool.missing_count == 0 {
        PreviewedBalance::Run
    } else {
        PreviewedBalance::SkipDegraded
    };

    AddPreviewPhase::LiveAdd(balance)
}

/// Semantic add work plan. `initial_journal_targets` is keyed by
/// `LuksUuid` so the journaled identity is the authoritative key from
/// t=0; operator-visible iteration sorts by `DiskName` separately. The
/// in-flight uniqueness assert runs once per target during planning
/// before this map is committed.
#[derive(Debug, Clone)]
struct AddWorkPlan {
    prelude: AddCredentialPrelude,
    targets: Vec<AddTargetWork>,
    initial_journal_targets: LuksUuidMap<journal::AddJournalTarget>,
    mount_point: MountPoint,
    preview_phase: AddPreviewPhase,
}

/// Single definition of the `DiskName`-sorted add-target order used by
/// confirmation and work-step output. The confirmation prelude is built
/// before `AddWorkPlan` exists, so callers share this helper instead of
/// copying the comparator.
fn sort_targets_by_name(targets: &[AddTargetWork]) -> Vec<&AddTargetWork> {
    let mut v: Vec<&AddTargetWork> = targets.iter().collect();
    v.sort_by(|a, b| a.name().cmp(b.name()));
    v
}

impl AddWorkPlan {
    fn is_noop(&self) -> bool {
        self.targets.is_empty()
    }

    fn mapper_paths(&self) -> Vec<String> {
        self.targets
            .iter()
            .map(|target| target.mapper_path().to_owned())
            .collect()
    }

    /// Disk names that survived classification, in input spec order.
    /// The done summary uses this so already-in-pool no-ops are not
    /// reported as newly added.
    fn target_names(&self) -> Vec<DiskName> {
        self.targets
            .iter()
            .map(|target| target.name().clone())
            .collect()
    }

    /// Build a `DiskName`-sorted view of `self.targets`. Every
    /// operator-visible work-step iteration of work-plan targets MUST
    /// iterate this sorted view so progress lines and dry-run output
    /// do not reorder on each fresh `braid add` (UUIDs are random per
    /// disk for fresh targets, so a UUID-keyed iteration is effectively
    /// random per invocation). Internal-only loops and summaries that
    /// preserve input order iterate `self.targets` directly.
    fn targets_sorted_by_name(&self) -> Vec<&AddTargetWork> {
        sort_targets_by_name(&self.targets)
    }

    fn render_steps(&self) -> Vec<Step> {
        let mut steps = Vec::new();

        // Operator-visible iteration: sort targets by DiskName so dry-run
        // preview ordering is independent of UUID-lex order.
        let sorted_targets = self.targets_sorted_by_name();
        for target in &sorted_targets {
            match target {
                AddTargetWork::Fresh(target) => {
                    let label = luks_label_for(&target.name);
                    steps.push(Step {
                        risk: "destructive",
                        description: format!("LUKS format {}", target.by_id),
                        // preview variant: real uuid minted at execute; ADR-022
                        commands: vec![CmdRequest::CryptsetupLuksFormatPreview {
                            device: target.by_id.as_str().to_owned(),
                            label,
                            extra_opts: target.luks_format_extra_opts.clone(),
                        }],
                    });
                    if let Some(kf) = &target.enroll_key_file {
                        steps.push(Step {
                            risk: "safe",
                            description: format!(
                                "enroll keyfile -> LUKS slot 1 on {}",
                                target.by_id
                            ),
                            commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
                                device: target.by_id.as_str().to_owned(),
                                key_file_path: kf.as_path().display().to_string(),
                            }],
                        });
                    }
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "LUKS header backup -> {}",
                            target.header_backup_path.as_path().display()
                        ),
                        commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
                            device: target.by_id.as_str().to_owned(),
                            backup_path: target.header_backup_path.as_path().display().to_string(),
                        }],
                    });
                    steps.push(Step {
                        risk: "safe",
                        description: format!("LUKS open -> {}", target.mapper_name),
                        commands: vec![CmdRequest::CryptsetupLuksOpen {
                            device: target.by_id.as_str().to_owned(),
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
                            "LUKS open + identity verification at execution time -> {}",
                            target.mapper_name
                        ),
                        commands: vec![CmdRequest::CryptsetupLuksOpen {
                            device: target.by_id.as_str().to_owned(),
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

        match self.preview_phase {
            AddPreviewPhase::Bootstrap => {
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
                        description: format!("mount -> {}", self.mount_point),
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
                        description: format!("mount -> {}", self.mount_point),
                        commands: vec![CmdRequest::Mount {
                            device: mapper_path,
                            mount_point: self.mount_point.clone(),
                        }],
                    });
                }
            }
            AddPreviewPhase::LiveAdd(balance) => {
                // Operator-visible iteration: sort by DiskName for the device-add
                // step ordering shown in dry-run preview.
                for target in &sorted_targets {
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
                if balance == PreviewedBalance::Run {
                    steps.push(Step {
                        risk: "long",
                        description: "btrfs balance to RAID1".into(),
                        // HARD convert, not ,soft. When growing an already-RAID1
                        // pool (3rd+ device), every chunk is already RAID1, so only
                        // a hard rewrite redistributes copies onto the new device;
                        // ,soft would skip them all and leave it empty. A 1->2 add
                        // converts existing single chunks either way. See
                        // docs/internals/btrfs/balance-soft.md.
                        commands: vec![CmdRequest::BtrfsBalanceRaid1 {
                            mount_point: self.mount_point.clone(),
                        }],
                    });
                }
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
    key_file: &KeyFilePath,
    header_backup_path: &HeaderBackupPath,
) {
    steps.push(Step {
        risk: "safe",
        description: format!("enroll keyfile -> LUKS slot 1 on {}", by_id),
        commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
            device: by_id.as_str().to_owned(),
            key_file_path: key_file.as_path().display().to_string(),
        }],
    });
    steps.push(Step {
        risk: "safe",
        description: format!(
            "LUKS header backup -> {}",
            header_backup_path.as_path().display()
        ),
        commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
            device: by_id.as_str().to_owned(),
            backup_path: header_backup_path.as_path().display().to_string(),
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

/// Per-invocation `braid add` configuration. `luks_format_extra_opts` is
/// the raw CLI vector; the planner parses it into a single
/// `LuksFormatExtraOpts` early so a managed flag (`--uuid`/`--label`) is
/// refused before any probing, journal write, or `CryptsetupLuksFormat`.
/// The same vector flows through `replace`'s symmetric path.
pub struct AddParams<'a> {
    pub config: &'a Config,
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
    /// Seam for the operator go/no-go prompt. Production prints the
    /// assembled prompt and reads from the tty; tests record the prompt
    /// and provide a deterministic verdict.
    pub confirm: &'a dyn confirm::Confirm,
    /// Seam for reading a LUKS passphrase from the TTY. Production
    /// passes `&RealTty`; tests pass a scripted reader so the
    /// bootstrap-confirm path is observable at the `cmd_add` layer.
    pub passphrase_reader: &'a dyn PassphraseReader,
    /// Seam for resolving by-id paths and mapper backings to the same
    /// kernel block-device namespace at the already-open mapper boundary.
    pub backing_path_resolver: &'a dyn BackingPathResolver,
}

/// Returns the missing-devices warning body (no legacy `warning:` prefix).
/// Both dry-run (`Preview::render` on stdout) and real-run
/// (`preview::render_notes_for_stderr` on stderr) wrap this in
/// `PreviewNote::Warn` and render it as the canonical `[warn] <body>`
/// -- one contract for both modes.
fn format_add_missing_devices_warning(missing_count: u64) -> String {
    let repair_command = repair_hint::missing_replace_command(None);
    let status_hint = repair_hint::see_missing_names_in_status(missing_count);
    format!(
        "pool has {} missing device{}. \
         Consider repairing with `{repair_command}` first. \
         {status_hint}",
        missing_count,
        if missing_count == 1 { "" } else { "s" }
    )
}

/// Body for the degraded-add balance-skip note, rendered `[skip] <body>` in both
/// dry-run preview and real-run stderr via `PreviewNote::Skip`. One source so the
/// two modes never drift (mirrors `format_add_missing_devices_warning`).
fn format_add_degraded_balance_skip() -> String {
    "pool: RAID1 balance skipped -- pool still has a missing device; redundancy \
     not restored. Run `braid remove-missing` or `braid replace` to restore it."
        .into()
}

/// Labels the disk set for no-op / done messages. Single-disk returns the
/// bare name; multi-disk joins names with `, `. The slice is iterated in
/// input order so the operator-visible list matches the spec order the
/// operator typed.
fn format_disk_name_list(names: &[DiskName]) -> String {
    if names.len() == 1 {
        names[0].to_string()
    } else {
        names
            .iter()
            .map(|n| n.as_str().to_owned())
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// Returns the no-op "nothing to do" message, without any channel-specific
/// formatting. Builds the planning-time `PreviewNote::Info`; dry-run renders
/// it via `Preview::render` and real-run emits it via `emit_notes_to_stderr`,
/// so both channels see byte-identical wording from one source.
fn format_add_noop(names: &[DiskName]) -> String {
    format!(
        "Nothing to do -- {} already in pool.",
        format_disk_name_list(names)
    )
}

fn format_add_done(names: &[DiskName]) -> String {
    let verb = if names.len() == 1 { "is" } else { "are" };
    format!(
        "Done. {} {verb} now part of the pool.",
        format_disk_name_list(names)
    )
}

/// Dry-run preview source of truth for `braid add` plus the execute
/// inputs pre-computed during planning. `preview()` renders accumulated
/// notes plus steps from the semantic work plan; `execute()` renders
/// the accumulated Warn/Info notes to stderr before any mutation. The
/// degraded-balance `PreviewNote::Skip` is a dry-run prediction only:
/// execute filters it from the replay and lets the live balance gate
/// re-emit it when the post-add probe is still degraded.
pub struct AddPlan {
    pub notes: Vec<PreviewNote>,
    work_plan: AddWorkPlan,
    pub config: Config,
    pub parsed: Vec<(DiskName, ByIdPath)>,
    pub names: Vec<DiskName>,
    pub by_ids: Vec<ByIdPath>,
    pub probed: Vec<PresentConfigDisk>,
    pub pool: PoolState,
    pub pool_membership: PoolMembership,
}

impl AddPlan {
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
        // stdout share one render contract for these notes. The
        // degraded-balance Skip note is a dry-run prediction; execute's
        // live balance gate below is the sole real-run emitter for it.
        let replay_notes: Vec<_> = self
            .notes
            .iter()
            .filter(|note| !matches!(note, PreviewNote::Skip(_)))
            .cloned()
            .collect();
        preview::emit_notes_to_stderr(&replay_notes, PerDiskStyle::Bracketed);

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
                    let hw = confirm::query_disk_hw_info(runner, disk.by_id.as_str());
                    AddConfirmDisk {
                        name: disk.name.as_str(),
                        by_id: disk.by_id.as_str(),
                        hw,
                        needs_luks_format: disk.needs_luks_format,
                    }
                })
                .collect();
            let prompt = format!("{}\n", format_add_confirm(&confirm_disks));
            params
                .confirm
                .confirm(&prompt)
                .map_err(AddError::Validation)?;
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
                                target.name()
                            )
                        } else {
                            format!(
                                "passphrase rejected by candidate disk '{}' ({})",
                                target.name(),
                                target.device()
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

        // Operator-visible iteration: sort by DiskName before emitting
        // the "verified as pool member" note. The internal needs_pool_add
        // push order can follow input order without UX impact because
        // mapper_paths flow through later loops that the sorted-vec
        // contract covers separately.
        let sorted_targets = self.work_plan.targets_sorted_by_name();
        for target in &sorted_targets {
            if let AddTargetWork::OpenRecoverable(target) = target {
                eprintln!(
                    "note: braid-labeled disk '{}' verified as pool member. \
                     Completing recovery add.",
                    target.name
                );
                needs_pool_add.push(PoolAddExecutionTarget {
                    mapper_path: target.mapper_path.clone(),
                    force: true,
                    luks_uuid: target.luks_uuid.clone(),
                    name: target.name.clone(),
                });
            }
        }

        // Operator-visible iteration: sort by DiskName for the
        // closed-PresentLuks unlock+identity progress lines.
        for target in &sorted_targets {
            let AddTargetWork::ClosedPresentLuks(target) = target else {
                continue;
            };
            probe_closed_present_luks_target_uuid(runner, &target.by_id, &target.luks_uuid)?;
            emit_status(&status_line(
                StatusTag::Wait,
                color_enabled,
                &format!("disk {}: unlocking...", target.name),
            ));
            if ensure_luks_open(
                runner,
                &target.name,
                &target.by_id,
                params.backing_path_resolver,
                &passphrase,
            )? == OpenOutcome::Opened
            {
                luks_guard.track(target.name.clone(), target.mapper_name.clone());
            }
            emit_status(&status_line(
                StatusTag::Ok,
                color_enabled,
                &format!("disk {}: unlocked", target.name),
            ));

            let identity = classify_braid_disk_fsid(
                runner,
                target.name.as_str(),
                &target.mapper_name,
                &self.pool,
            )?;
            if let Some(err) = identity_to_error(&identity, target.name.as_str()) {
                return Err(err);
            }
            match identity {
                AddLuksBtrfsProbe::SamePool => {
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
                        mapper_path: target.mapper_path.clone(),
                        luks_uuid: target.luks_uuid.clone(),
                        verified_pool_fsid,
                        enroll_key_file: target.enroll_key_file.clone(),
                        header_backup_path: target.header_backup_path.clone(),
                    };
                    journal_targets
                        .insert(
                            verified.luks_uuid.clone(),
                            recoverable_journal_target(&verified),
                        )
                        .map_err(|conflict| target_uuid_map_conflict_to_validation(&conflict))?;
                    needs_pool_add.push(PoolAddExecutionTarget {
                        mapper_path: verified.mapper_path,
                        force: true,
                        luks_uuid: verified.luks_uuid,
                        name: verified.name,
                    });
                }
                _ => unreachable!("error variants handled by identity_to_error above"),
            }
        }

        // A non-empty work plan must yield at least one journal target:
        // is_noop() (targets.is_empty()) already returned at the top of
        // execute(), and every surviving target either inserts into
        // journal_targets (Fresh/OpenRecoverable at planning,
        // ClosedPresentLuks SamePool above) or returns Err. Empty here is an
        // internal accounting bug -- fail closed before the journal write
        // instead of falling through. The downstream pool_after .expect()
        // relies on this.
        if journal_targets.is_empty() {
            return Err(AddError::Validation(
                "add work plan has targets but produced no journal targets after \
                 identity verification"
                    .into(),
            ));
        }

        let mount_point = self.config.mount_point();
        let fresh_pool = probe_pool(runner, fs, mount_point)?;
        validate_execute_pool_identity(&self.pool, &fresh_pool, mount_point)?;
        recheck_execute_live_pool_targets(
            &journal_targets,
            &fresh_pool,
            params.backing_path_resolver,
        )?;

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
            .acquire("adding disks to pool")
            .map_err(|e| {
                AddError::Validation(format!(
                    "could not acquire sleep inhibitor (is logind running?): {e}"
                ))
            })?;

        // All identity checks passed. Write journal before irreversible disk operations.
        // Build target_membership via the typed PoolMembership::insert
        // path (four-axis uniqueness invariant: UUID + name + by-id +
        // non-None devid). The pre-write `assert_target_uuid_unique`
        // gate ran during planning so UUID collisions were already
        // refused with `AddError::DuplicateUuid`; this insert is the
        // defense-in-depth backstop.
        let mut target_membership = self.pool_membership.clone();
        // Internal iteration: operator-visible output uses the sorted
        // vec built above (sorted_targets) and the sorted journal vec
        // built below for Pass-3 keyfile enrollment progress lines.
        for (uuid, target) in &journal_targets {
            let member = DiskMember {
                name: target.name.clone(),
                by_id: target.by_id.clone(),
                devid: None,
                added_at: None,
            };
            target_membership.insert(uuid.clone(), member)?;
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
        // Operator-visible iteration: sort by DiskName so progress
        // lines stay in alphabetical order regardless of UUID-lex
        // order of the journal map.
        for target in &sorted_targets {
            let AddTargetWork::Fresh(target) = target else {
                continue;
            };
            let name = &target.name;

            if !matches!(
                journal_targets.get(&target.luks_uuid).map(|t| &t.mode),
                Some(journal::AddJournalMode::FreshLuks { .. })
            ) {
                return Err(AddError::Validation(format!(
                    "fresh add target '{}' missing from journal",
                    name
                )));
            }

            let label = luks_label_for(name);
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
                target.by_id.as_str(),
                &passphrase,
                &target.luks_uuid,
                &label,
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
                crate::luks::enroll_key_file(runner, target.by_id.as_str(), &passphrase, kf)?;
                emit_status(&status_line(
                    StatusTag::Ok,
                    color_enabled,
                    &format!("disk {name}: keyfile enrolled in slot 1"),
                ));
            }

            let backup_path = backup_luks_header_post_mutation(
                runner,
                target.by_id.as_str(),
                &target.mapper_name,
                params.paths,
            )?;
            eprintln!("LUKS header backed up: {backup_path}");

            eprint!(
                "{}",
                status_line(
                    StatusTag::Wait,
                    color_enabled,
                    &format!("disk {name}: unlocking..."),
                )
            );
            if ensure_luks_open(
                runner,
                name,
                &target.by_id,
                params.backing_path_resolver,
                &passphrase,
            )? == OpenOutcome::Opened
            {
                luks_guard.track(name.clone(), target.mapper_name.clone());
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
                luks_uuid: target.luks_uuid.clone(),
                name: target.name.clone(),
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
        //
        // Operator-visible iteration: sort by DiskName so per-target
        // progress lines are alphabetical.
        let mut sorted_journal: Vec<(&LuksUuid, &journal::AddJournalTarget)> =
            journal_targets.iter().collect();
        sorted_journal.sort_by(|a, b| a.1.name.cmp(&b.1.name));
        for (_, journal_target) in &sorted_journal {
            let journal::AddJournalMode::RecoverableBraidLabeled {
                enroll_key_file: Some(kf),
                ..
            } = &journal_target.mode
            else {
                continue;
            };
            let name = &journal_target.name;
            emit_status(&status_line(
                StatusTag::Wait,
                color_enabled,
                &format!("disk {name}: enrolling keyfile in slot 1..."),
            ));
            crate::luks::enroll_key_file(runner, journal_target.by_id.as_str(), &passphrase, kf)?;
            emit_status(&status_line(
                StatusTag::Ok,
                color_enabled,
                &format!("disk {name}: keyfile enrolled in slot 1"),
            ));
            let mapper = mapper_name(name);
            let backup_path = backup_luks_header_post_mutation(
                runner,
                journal_target.by_id.as_str(),
                &mapper,
                params.paths,
            )?;
            eprintln!("LUKS header backed up: {backup_path}");
        }

        // Both passes complete -- mappers are committed for pool operations.
        luks_guard.disarm();

        // Pool phase
        let mapper_paths: Vec<String> = needs_pool_add
            .iter()
            .map(|target| target.mapper_path.clone())
            .collect();

        if !self.pool.mounted {
            if mapper_paths.len() >= 2 {
                // Bootstrap with mkfs.btrfs RAID1
                pool_bootstrap_mount_raid1(runner, fs, &mapper_paths, mount_point)?;
                eprintln!("Pool created (RAID1) and mounted at {}", mount_point);
            } else {
                // Single disk bootstrap
                pool_bootstrap_mount(runner, fs, &mapper_paths[0], mount_point)?;
                eprintln!(
                    "Pool created (data single; metadata/system DUP -- no RAID1 disk redundancy) and mounted at {}",
                    mount_point
                );
            }

            // Fresh pool identity: every previous acked baseline is stale.
            alert::remove_acked_stats(params.paths).map_err(|e| AddError::AckCleanupFailed {
                stage: "bootstrap",
                detail: e.to_string(),
            })?;

            // Bootstrap post-commit persist: write pool.json after mkfs + mount.
            // Enrich with live metadata (devid) from pool probe, best-effort:
            // if the probe itself fails, warn and save the target membership
            // unenriched. Pinned by
            // cmd_add_bootstrap_warns_when_post_mount_probe_errors.
            let mut final_membership = journal.target_membership.clone();
            match probe_pool(runner, fs, mount_point) {
                Ok(pool_after) => {
                    membership::enrich_from_pool_state(&mut final_membership, &pool_after);
                }
                Err(e) => crate::status_tag::emit_status(&format!(
                    "Warning: failed to probe pool for metadata refresh: {e}\n"
                )),
            }
            membership::save_membership(&final_membership, params.paths)?;
            // Order matters: save_membership before clear_journal. If
            // save_membership fails, the journal survives and recover can
            // reconstruct pool.json from the live pool.
            journal::clear_journal(params.paths)
                .map_err(|e| AddError::Validation(e.to_string()))?;
        } else {
            // Add each to existing pool
            let mut pool_after: Option<PoolState> = None;
            for target in &needs_pool_add {
                pool_add_device(runner, &target.mapper_path, mount_point, target.force)?;
                eprintln!("Device added to pool: {}", target.mapper_path);
                let probe = probe_pool(runner, fs, mount_point).map_err(|e| {
                    AddError::PostAddProbeFailed {
                        detail: format!("{}: {e}", target.name),
                    }
                })?;
                let dev = probe.device_by_uuid(&target.luks_uuid).ok_or_else(|| {
                    AddError::PostAddProbeFailed {
                        detail: format!("{}: not found in pool after add", target.name),
                    }
                })?;
                alert::drop_ghost_acked_for_devids(params.paths, &[dev.devid]).map_err(|e| {
                    AddError::AckCleanupFailed {
                        stage: "live-pool add",
                        detail: format!("devid {}: {e}", dev.devid),
                    }
                })?;
                pool_after = Some(probe);
            }

            // Membership is committed by btrfs device add. Persist it before
            // the long post-add balance while leaving the journal in place so
            // recovery still knows the balance is owed if interrupted.
            let pool_after = pool_after.expect(
                "needs_pool_add is non-empty in the live-pool branch: \
                 journal_targets.is_empty() short-circuits earlier, and \
                 journal_targets and needs_pool_add are populated in lockstep",
            );
            // End-state post-condition over every journaled member against
            // the final pool probe. The per-target loop above proves each add
            // in its own immediate probe and extracts the devid for ghost
            // cleanup; it does not prove an earlier add is still present after
            // a later add. Fail before save_membership: enrich_from_pool_state
            // skips missing members, which would persist a vanished disk with
            // no devid and then balance a degraded pool. This shares
            // PostAddProbeFailed per docs/dev/safety-heuristics.md: same
            // post-commit lifecycle point and same braid recover remediation,
            // regardless of detection site.
            for (uuid, target) in journal_targets.iter() {
                if pool_after.device_by_uuid(uuid).is_none() {
                    return Err(AddError::PostAddProbeFailed {
                        detail: format!(
                            "{}: no longer present in the live pool after all disks were added",
                            target.name
                        ),
                    });
                }
            }
            let mut final_membership = journal.target_membership.clone();
            membership::enrich_from_pool_state(&mut final_membership, &pool_after);
            membership::save_membership(&final_membership, params.paths)?;

            let mut balance_journal = journal.clone();
            if let journal::OpKind::Add { phase, .. } = &mut balance_journal.op {
                *phase = journal::AddPhase::PostAddBalanceRaid1;
            }
            journal::write_journal(params.paths, &balance_journal)
                .map_err(|e| AddError::Validation(e.to_string()))?;

            // Authoritative live gate for the hard post-add convert. The
            // preview step is only a plan-time predictor; this final probe
            // closes the confirmation/passphrase/format/add window where an
            // existing member can go missing or return before the balance.
            if pool_can_host_raid1(&pool_after) {
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
            } else if pool_after.missing_count > 0 {
                // Live pool is degraded: skip the hard convert and announce it.
                // The plan-time Skip note was filtered from the replay above,
                // so this is the one real-run source for the balance-skip line.
                preview::emit_notes_to_stderr(
                    &[PreviewNote::Skip(format_add_degraded_balance_skip())],
                    PerDiskStyle::Bracketed,
                );
            }

            // Leave the journal until the balance completes; interruption
            // after the membership commit still needs recovery replay.
            journal::clear_journal(params.paths)
                .map_err(|e| AddError::Validation(e.to_string()))?;
        }

        eprintln!("{}", format_add_done(&self.work_plan.target_names()));
        Ok(())
    }
}

/// Plan a `braid add` run after dispatch has already checked for a pending
/// operation and loaded config under the pool lock. Owns disk-spec parsing,
/// duplicate-name / duplicate-by-id validation, keyfile path validation,
/// membership load, conflict validation, pool probe, mutation preflight,
/// per-disk probe, UPS preflight, the missing-devices warning, the keyfile-asymmetry
/// warning, and the semantic add work planner. On success, every accumulated
/// note lives on `plan.notes`; on failure after note accumulation, notes
/// survive on `PlanFailure::notes` so `cmd_add` can render them to stderr
/// before the error.
pub fn plan_add<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &AddParams<'_>,
) -> Result<AddPlan, PlanFailure<AddError>> {
    // Accumulator for preview-context notes that must survive a later
    // planner error. Notes added here travel to `PlanFailure::notes` on
    // the Err branch and move into `plan.notes` on the Ok branch.
    let mut notes: Vec<PreviewNote> = Vec::new();

    let config = params.config;

    // Validate `--luks-format-arg` at the CLI boundary BEFORE any
    // probing, journal write, or `cryptsetup luksFormat`. A managed
    // token (`--uuid`/`--label`) surfaces as
    // `AddError::ManagedFormatFlag` so the CLI matches one error type
    // at the boundary.
    let luks_format_extra_opts = match LuksFormatExtraOpts::parse(params.luks_format_extra_opts) {
        Ok(o) => o,
        Err(e) => return Err(PlanFailure::empty(AddError::ManagedFormatFlag(e))),
    };

    // Parse disk specs into typed (DiskName, ByIdPath). Validation
    // (name character set, by-id prefix) happens at this boundary; the
    // rest of the planner consumes the typed values.
    let parsed: Vec<(DiskName, ByIdPath)> = match params
        .disk_specs
        .iter()
        .map(|s| membership::parse_disk_spec(s))
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) => v,
        Err(e) => return Err(PlanFailure::empty(AddError::Validation(e.to_string()))),
    };

    let names: Vec<DiskName> = parsed.iter().map(|(n, _)| n.clone()).collect();
    let by_ids: Vec<ByIdPath> = parsed.iter().map(|(_, b)| b.clone()).collect();

    // Reject duplicate names upfront (by typed DiskName).
    {
        let mut seen: std::collections::HashSet<&DiskName> = std::collections::HashSet::new();
        for name in &names {
            if !seen.insert(name) {
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
        let mut seen: std::collections::HashSet<&str> = std::collections::HashSet::new();
        for by_id in &by_ids {
            if !seen.insert(by_id.as_str()) {
                return Err(PlanFailure::empty(AddError::Validation(format!(
                    "duplicate by_id: '{}'",
                    by_id
                ))));
            }
        }
    }

    if let Some(kf) = params.enroll_key_file
        && let Err(e) = crate::enroll_key_file::validate_key_file_path(kf, false)
    {
        return Err(PlanFailure::empty(AddError::Validation(e.to_string())));
    }

    // Load existing membership (or empty if pool.json absent). A
    // hard-corrupt pool.json fails closed via `MembershipError::Corrupt`;
    // a missing file is the legitimate bootstrap case and maps to
    // `PoolMembership::empty()`.
    let pool_membership = match membership::load_membership(params.paths) {
        Ok(m) => m,
        Err(membership::MembershipError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            PoolMembership::empty()
        }
        Err(e) => return Err(PlanFailure::empty(e.into())),
    };

    // Fail-fast name/by-id conflict gate. Runs before any probe, passphrase
    // read, inhibitor acquisition, or journal write so a name/by-id collision
    // with an existing member fails with zero side effects. In-invocation
    // duplicate names and by-ids were already rejected above, so each spec is
    // mutually distinct and only needs checking against existing members.
    //
    // Only the name and by-id axes are checked here: no LUKS UUID has been
    // assigned yet (it is generated/probed per target later) and devid is
    // enrichment-only. The full four-axis uniqueness invariant is enforced by
    // the real `PoolMembership::insert` that builds `target_membership` at
    // commit time -- this is the early fail-fast, that is the backstop.
    for (name, by_id) in &parsed {
        if let Some((existing_uuid, existing)) = pool_membership.by_name(name) {
            // Exact existing member (same name AND by-id): re-specifying a
            // disk already in the pool is the documented already-in-pool
            // no-op, classified downstream -- not a conflict.
            if &existing.by_id == by_id {
                continue;
            }
            return Err(PlanFailure::empty(
                membership::MembershipError::Conflict(format!(
                    "name '{name}' already in use under UUID {existing_uuid}"
                ))
                .into(),
            ));
        }
        if let Some((existing_uuid, _)) = pool_membership.by_by_id(by_id) {
            return Err(PlanFailure::empty(
                membership::MembershipError::Conflict(format!(
                    "by_id '{by_id}' already in use under UUID {existing_uuid}"
                ))
                .into(),
            ));
        }
    }

    // Probe pool + preflight before per-disk LUKS probing so a short live
    // exclusive op is observed close to command entry. The btrfs operations
    // use --enqueue, but the operator-visible wait note is still part of the
    // preview contract.
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
        let fsid = pool.fsid.as_ref().expect("mounted pool must have FSID");
        match preflight::require_mutation_preflight(fs, fsid, config.mount_point()) {
            Ok(preflight_notes) => notes.extend(preflight_notes),
            Err(msg) => return Err(PlanFailure::empty(AddError::Validation(msg))),
        }
    }

    // Probe all disks, then refine to the present-only shape consumed by
    // downstream builders.
    let probed: Vec<ConfigDisk> = match names
        .iter()
        .zip(by_ids.iter())
        .map(|(name, by_id)| {
            probe_config_disk(runner, fs, name, by_id, params.backing_path_resolver)
        })
        .collect::<Result<Vec<_>, _>>()
    {
        Ok(v) => v,
        Err(e) => {
            let error = e.into();
            return Err(if notes.is_empty() {
                PlanFailure::empty(error)
            } else {
                PlanFailure::with_notes(notes, error)
            });
        }
    };

    let mut present_probed = Vec::with_capacity(probed.len());
    for probed_disk in probed {
        match PresentConfigDisk::try_from(probed_disk) {
            Ok(p) => present_probed.push(p),
            Err(orig) => {
                let error = AddError::Validation(format!(
                    "disk '{}' ({}) is not present. Is it plugged in?",
                    orig.name, orig.by_id_path
                ));
                return Err(if notes.is_empty() {
                    PlanFailure::empty(error)
                } else {
                    PlanFailure::with_notes(notes, error)
                });
            }
        }
    }
    let probed: Vec<PresentConfigDisk> = present_probed;

    if let Err(msg) =
        preflight::check_ups_not_on_battery(runner, config.ups().map(|u| u.name.as_str()), "add")
    {
        return Err(PlanFailure::with_notes(notes, AddError::Validation(msg)));
    }

    // Build the semantic work plan. This can fail on PresentLuks identity /
    // foreign-pool guards.
    let by_ids_refs: Vec<&ByIdPath> = by_ids.iter().collect();
    let work_plan_result = build_add_work_plan(
        runner,
        &AddStepsInput {
            names: &names,
            by_ids: &by_ids_refs,
            probed: &probed,
            pool: &pool,
            mount_point: config.mount_point(),
            paths: params.paths,
            enroll_key_file: params.enroll_key_file,
            luks_format_extra_opts: &luks_format_extra_opts,
            backing_path_resolver: params.backing_path_resolver,
            pool_membership: &pool_membership,
        },
    );

    // Missing-devices warning: body-only, no legacy `warning:` prefix. Lives on
    // `notes` so it surfaces on both dry-run stdout (via `Preview::render`) and
    // real-run stderr (via `AddPlan::execute` using
    // `preview::render_notes_for_stderr`).
    //
    // Intentionally NOT gated on `is_noop` -- unlike the keyfile-asymmetry
    // warning (derived from `work_plan.targets`, so no-ops do not warn) and
    // the balance-skip note (`!is_noop`-gated below). Those two describe the
    // work (the new drive, the skipped balance step) and are meaningless when
    // nothing is added. This warning describes the pool's existing health and
    // is true whether or not work happens, so a degraded no-op re-add still
    // surfaces it: the `braid replace` hint is the repair pointer an operator
    // who ran `add` against a degraded pool needs, and staying quiet would run
    // counter to "never silently degraded"
    // (docs/design/principles.md#1-resilient-by-default). Pinned by the
    // `plan_add_degraded_noop_keeps_missing_warning` unit test and the
    // real-run no-op phase in tests/cli/braid-add-warnings.py.
    if pool.missing_count > 0 {
        notes.push(PreviewNote::Warn(format_add_missing_devices_warning(
            pool.missing_count,
        )));
    }

    // Accumulated notes (missing-devices) must survive on `PlanFailure::notes`
    // so the caller can render them to stderr before the error.
    let work_plan = match work_plan_result {
        Ok(s) => s,
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, e));
        }
    };
    // Keyfile-asymmetry warning: body-only, no legacy `WARNING:` prefix.
    // Derived from the actual work targets so already-in-pool no-ops do
    // not warn about a disk that will not be adopted.
    if params.enroll_key_file.is_none() {
        let mut any_target_lacks_keyfile = false;
        for target in &work_plan.targets {
            match target {
                AddTargetWork::Fresh(_) => any_target_lacks_keyfile = true,
                AddTargetWork::OpenRecoverable(target) => {
                    match check_key_slot(runner, target.by_id.as_str(), LUKS_SLOT_KEYFILE) {
                        Ok(KeySlotState::Empty) => any_target_lacks_keyfile = true,
                        Ok(KeySlotState::Occupied) => {}
                        Err(err) => notes.push(PreviewNote::Warn(
                            format_target_keyfile_probe_failure(&target.by_id, &err),
                        )),
                    }
                }
                AddTargetWork::ClosedPresentLuks(target) => {
                    match check_key_slot(runner, target.by_id.as_str(), LUKS_SLOT_KEYFILE) {
                        Ok(KeySlotState::Empty) => any_target_lacks_keyfile = true,
                        Ok(KeySlotState::Occupied) => {}
                        Err(err) => notes.push(PreviewNote::Warn(
                            format_target_keyfile_probe_failure(&target.by_id, &err),
                        )),
                    }
                }
            }
        }

        if any_target_lacks_keyfile {
            let keyfile_probe = probe_pool_keyfile_enrollment(runner, &pool.devices);
            if keyfile_probe.has_enrollment {
                notes.push(PreviewNote::Warn(format_keyfile_asymmetry_warning()));
            } else {
                notes.extend(keyfile_probe.failures.iter().map(|failure| {
                    PreviewNote::Warn(format_keyfile_enrollment_probe_failure(failure))
                }));
            }
        }
    }
    // Degraded-add balance skip predicted from the plan-time pool probe. The
    // note is dry-run-only on success; real execute filters it from replay and
    // lets the post-add live gate emit the same body for all degraded outcomes.
    if !work_plan.is_noop()
        && work_plan.preview_phase == AddPreviewPhase::LiveAdd(PreviewedBalance::SkipDegraded)
    {
        notes.push(PreviewNote::Skip(format_add_degraded_balance_skip()));
    }
    // No-op preview: zero steps + Info note naming the already-in-pool
    // target(s). The Info note suppresses `Preview::render`'s
    // `nothing to do.` fallback (see `preview.rs`:
    // `render_info_note_suppresses_nothing_to_do`). Real-run emits this same
    // Info note via `emit_notes_to_stderr`, so dry-run and real-run share one
    // `format_add_noop` source with no separate real-run `eprintln!`.
    if work_plan.is_noop() {
        notes.push(PreviewNote::Info(format_add_noop(&names)));
    }

    let plan = AddPlan {
        notes,
        work_plan,
        config: config.clone(),
        parsed,
        names,
        by_ids,
        probed,
        pool,
        pool_membership,
    };

    Ok(plan)
}

/// Plan-then-execute device enrollment through LUKS format and btrfs add.
/// Dry-run renders the same typed plan; planning fails closed on duplicate
/// LUKS UUID/name preflight before any mutation.
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
            preview::emit_notes_to_stderr(&notes, PerDiskStyle::Bracketed);
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
    names: &'a [DiskName],
    by_ids: &'a [&'a ByIdPath],
    probed: &'a [PresentConfigDisk],
    pool: &'a PoolState,
    mount_point: &'a MountPoint,
    paths: &'a StatePaths,
    enroll_key_file: Option<&'a Path>,
    luks_format_extra_opts: &'a LuksFormatExtraOpts,
    /// Same canonical backing-path resolver used by mapper ownership
    /// checks so PresentLuks live-pool correlation proves UUID and disk.
    backing_path_resolver: &'a dyn BackingPathResolver,
    /// Borrowed pool membership for the pre-journal-write uniqueness
    /// assert on freshly generated / probed UUIDs.
    pool_membership: &'a PoolMembership,
}

/// Build the credential prelude from the sorted work targets so the
/// confirmation prompt neither names skipped disks nor reorders relative
/// to dry-run work steps.
fn build_add_credential_prelude(
    input: &AddStepsInput<'_>,
    targets: &[AddTargetWork],
) -> AddCredentialPrelude {
    let confirm_disks = sort_targets_by_name(targets)
        .into_iter()
        .map(|target| AddConfirmDiskPlan {
            name: target.name().clone(),
            by_id: target.by_id().clone(),
            needs_luks_format: matches!(target, AddTargetWork::Fresh(_)),
        })
        .collect();

    let any_needs_format = input
        .probed
        .iter()
        .any(|p| matches!(p.state, PresentConfigDiskState::PresentNotLuks));
    let confirm_new = any_needs_format && input.pool.devices.is_empty();
    let pool_target_count = input.pool.devices.len();

    let mut verify_targets: Vec<CredentialVerifyTarget> = input
        .pool
        .devices
        .iter()
        .map(|device| CredentialVerifyTarget::existing_pool_member(input.pool_membership, device))
        .collect();
    verify_targets.extend(input.probed.iter().enumerate().filter_map(|(i, probed)| {
        let PresentConfigDiskState::PresentLuks { uuid, .. } = &probed.state else {
            return None;
        };
        if input.pool.device_by_uuid(uuid).is_some() {
            return None;
        }
        Some(CredentialVerifyTarget::named_candidate(
            &input.names[i],
            input.by_ids[i],
        ))
    }));

    AddCredentialPrelude {
        confirm_disks,
        confirm_new,
        verify_targets,
        pool_target_count,
    }
}

/// Build an `AddJournalTarget` for a fresh-format target. Identity
/// lives in the `LuksUuidMap` key the planner uses for `insert`; this
/// helper carries only the presentation `name`, hardware `by_id`, and
/// mode-specific extras.
fn fresh_journal_target(target: &FreshLuksTarget) -> journal::AddJournalTarget {
    journal::AddJournalTarget {
        name: target.name.clone(),
        by_id: target.by_id.clone(),
        mode: journal::AddJournalMode::FreshLuks {
            extra_opts: target.luks_format_extra_opts.clone(),
            enroll_key_file: target.enroll_key_file.clone(),
        },
    }
}

/// Build an `AddJournalTarget` for a returning braid-labeled target.
/// Identity is the `LuksUuid` key; `verified_pool_fsid` backstops the
/// Add-recovery FSID cross-check that UUID alone does not subsume.
fn recoverable_journal_target(target: &RecoverableBraidTarget) -> journal::AddJournalTarget {
    journal::AddJournalTarget {
        name: target.name.clone(),
        by_id: target.by_id.clone(),
        mode: journal::AddJournalMode::RecoverableBraidLabeled {
            verified_pool_fsid: target.verified_pool_fsid.clone(),
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
    name: &DiskName,
    by_id: &ByIdPath,
    user_enroll_key_file: Option<&Path>,
) -> Result<Option<KeyFilePath>, AddError> {
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
            Ok(Some(KeyFilePath::new(kf.to_path_buf())))
        }
        Err(e) => Err(AddError::Validation(e.to_string())),
    }
}

fn build_add_work_plan<R: CommandRunner>(
    runner: &R,
    input: &AddStepsInput<'_>,
) -> Result<AddWorkPlan, AddError> {
    let mut targets = Vec::new();
    let mut initial_journal_targets: LuksUuidMap<journal::AddJournalTarget> = LuksUuidMap::new();

    // Internal iteration: build the work plan in input spec order so
    // each probe gets visited deterministically. Operator-visible
    // output uses `AddWorkPlan::targets_sorted_by_name` (in
    // `AddPlan::execute` and `render_steps`).
    for (i, p) in input.probed.iter().enumerate() {
        let name = &input.names[i];
        let by_id = input.by_ids[i];
        let mn = mapper_name(name);
        let mapper_path = mn.dev_path();

        match &p.state {
            PresentConfigDiskState::PresentNotLuks => {
                // FreshLuks: pre-generate the LUKS UUID at planning so
                // the journal records authoritative identity from t=0.
                // A mid-format crash and replay reformats under the
                // same identity.
                let luks_uuid = LuksUuid::new_v4();
                let header_backup_path =
                    luks_header_backup_path(&input.paths.luks_headers_dir(), &mn);
                let target = FreshLuksTarget {
                    name: name.clone(),
                    by_id: (*by_id).clone(),
                    mapper_name: mn.clone(),
                    mapper_path,
                    luks_uuid: luks_uuid.clone(),
                    luks_format_extra_opts: input.luks_format_extra_opts.clone(),
                    enroll_key_file: input
                        .enroll_key_file
                        .map(|p| KeyFilePath::new(p.to_path_buf())),
                    header_backup_path,
                };
                // Identity scopes first (in-flight + membership), then the
                // live-pool guard -- the same order the old single assert
                // used, so a generated UUID that collides with a known member
                // still reports the informative `DuplicateUuid` (both real
                // parties), not the scope-only `DuplicateUuidLivePool`.
                assert_target_uuid_unique(
                    &luks_uuid,
                    input.pool_membership,
                    &initial_journal_targets,
                    name,
                    by_id,
                )?;
                assert_fresh_uuid_absent_from_live_pool(&luks_uuid, input.pool, name, by_id)?;
                initial_journal_targets
                    .insert(luks_uuid, fresh_journal_target(&target))
                    .map_err(|conflict| target_uuid_map_conflict_to_validation(&conflict))?;
                targets.push(AddTargetWork::Fresh(target));
            }
            PresentConfigDiskState::PresentLuks {
                uuid,
                mapper_open,
                label,
            } => {
                // Preconditions always checked — no mapper required.
                validate_braid_preconditions(name, by_id.as_str(), label.as_deref(), input.pool)?;

                let resolved_enroll_key_file =
                    resolve_existing_luks_enroll(runner, name, by_id, input.enroll_key_file)?;

                if *mapper_open {
                    // Mapper is open — full classification without side effects
                    let identity =
                        classify_braid_disk_fsid(runner, name.as_str(), &mn, input.pool)?;
                    if let Some(err) = identity_to_error(&identity, name.as_str()) {
                        return Err(err);
                    }
                    match identity {
                        AddLuksBtrfsProbe::SamePool => {
                            // Two-tier defense, mirroring the closed branch: the
                            // backing-aware `classify_live_pool_match` below owns
                            // the live-pool concern (proving same-backing no-ops,
                            // rejecting different-backing clones), so the
                            // subsequent `assert_target_uuid_unique` is left to
                            // catch only in-flight and membership identity
                            // collisions.
                            match classify_live_pool_match(
                                uuid,
                                by_id,
                                input.pool,
                                input.backing_path_resolver,
                            )? {
                                LivePoolMatch::SameBacking => continue,
                                LivePoolMatch::DifferentBacking => {
                                    return Err(duplicate_live_pool_uuid_error(uuid, name, by_id));
                                }
                                LivePoolMatch::NoMatch => {}
                            }
                            let verified_pool_fsid = input.pool.fsid.clone().ok_or_else(|| {
                                AddError::Validation(
                                    "mounted pool has no FSID while planning returned add target"
                                        .into(),
                                )
                            })?;
                            let target = RecoverableBraidTarget {
                                name: name.clone(),
                                by_id: (*by_id).clone(),
                                mapper_path,
                                luks_uuid: uuid.clone(),
                                verified_pool_fsid,
                                enroll_key_file: resolved_enroll_key_file,
                                header_backup_path: luks_header_backup_path(
                                    &input.paths.luks_headers_dir(),
                                    &mn,
                                ),
                            };
                            assert_target_uuid_unique(
                                &target.luks_uuid,
                                input.pool_membership,
                                &initial_journal_targets,
                                name,
                                by_id,
                            )?;
                            initial_journal_targets
                                .insert(
                                    target.luks_uuid.clone(),
                                    recoverable_journal_target(&target),
                                )
                                .map_err(|conflict| {
                                    target_uuid_map_conflict_to_validation(&conflict)
                                })?;
                            targets.push(AddTargetWork::OpenRecoverable(target));
                        }
                        _ => unreachable!("error variants handled by identity_to_error above"),
                    }
                } else {
                    // Mapper closed -- FSID verification deferred to execution time.
                    // Two-tier defense for the cached `uuid`:
                    //   (a) plan-time live-pool matching below proves same-backing
                    //       no-ops, rejects different-backing clones, then
                    //       `assert_target_uuid_unique` rejects in-flight and
                    //       membership collisions.
                    //   (b) execute-time live-UUID re-probe before `ensure_luks_open`
                    //       (see Pass-1 loop) rejects plan-to-execute disk swaps so a
                    //       foreign disk at this by-id cannot pass through to
                    //       `btrfs device add`; after Pass 1, a fresh live-pool
                    //       re-classification covers both ClosedPresentLuks and
                    //       OpenRecoverable targets before journal write.
                    match classify_live_pool_match(
                        uuid,
                        by_id,
                        input.pool,
                        input.backing_path_resolver,
                    )? {
                        LivePoolMatch::SameBacking => continue,
                        LivePoolMatch::DifferentBacking => {
                            return Err(duplicate_live_pool_uuid_error(uuid, name, by_id));
                        }
                        LivePoolMatch::NoMatch => {}
                    }
                    assert_target_uuid_unique(
                        uuid,
                        input.pool_membership,
                        &initial_journal_targets,
                        name,
                        by_id,
                    )?;
                    targets.push(AddTargetWork::ClosedPresentLuks(
                        ClosedPresentLuksCandidate {
                            name: name.clone(),
                            by_id: (*by_id).clone(),
                            mapper_name: mn.clone(),
                            mapper_path,
                            luks_uuid: uuid.clone(),
                            enroll_key_file: resolved_enroll_key_file,
                            header_backup_path: luks_header_backup_path(
                                &input.paths.luks_headers_dir(),
                                &mn,
                            ),
                        },
                    ));
                }
            }
        }
    }

    let preview_phase = add_preview_phase(input.pool, targets.len());
    Ok(AddWorkPlan {
        prelude: build_add_credential_prelude(input, &targets),
        targets,
        initial_journal_targets,
        mount_point: input.mount_point.clone(),
        preview_phase,
    })
}

/// Pre-journal-write per-target identity-collision assert. Runs once per
/// target inside `build_add_work_plan` after the target's UUID is
/// generated (FreshLuks) or probed (PresentLuks /
/// RecoverableBraidLabeled). Both arms refuse a UUID that already belongs
/// to a real, braid-resolved identity, so both name both `(name, by_id)`
/// pairs explicitly:
///   1. If the UUID is already in the in-flight `targets` map under a
///      different by-id, raise `AddError::DuplicateUuid` naming both
///      `(name, by_id)` pairs (the cloned-disk-across-targets case).
///   2. Otherwise, if the UUID matches a membership key, raise
///      `AddError::DuplicateUuid` naming the in-flight target plus the
///      colliding member's real `(name, by_id)`.
///
/// Live-pool collisions -- a UUID matching a device live in the btrfs pool
/// but absent from membership (a foreign/cloned disk) -- are NOT this
/// gate's concern. They are owned by the per-caller live-pool guards:
/// `classify_live_pool_match` for the `PresentLuks` arms (backing-aware,
/// telling a same-backing returned-disk no-op apart from a
/// different-backing clone) and `assert_fresh_uuid_absent_from_live_pool`
/// for `FreshLuks` (a plain `pool.devices` scan, right-sized for a
/// freshly-minted UUID that has no legitimate same-backing match). Both
/// route their refusal through `duplicate_live_pool_uuid_error`, so this
/// assert never needs to invent an identity for the foreign device.
///
/// `LuksUuidMap::insert` fail-closed and `PoolMembership::insert` are
/// the defense-in-depth backstops; this gate is the pre-write refusal
/// so the operator gets a structured collision message naming the real
/// parties rather than falling through to a generic conflict error.
fn assert_target_uuid_unique(
    uuid: &LuksUuid,
    membership: &PoolMembership,
    in_flight: &LuksUuidMap<journal::AddJournalTarget>,
    this_name: &DiskName,
    this_by_id: &ByIdPath,
) -> Result<(), AddError> {
    // (1) In-flight collision (cloned-disk-across-targets).
    if let Some(prior) = in_flight.get(uuid) {
        return Err(duplicate_uuid_error(
            uuid.clone(),
            this_name,
            this_by_id,
            &prior.name,
            &prior.by_id,
        ));
    }
    // (2) Membership-key collision.
    if let Some(existing) = membership.by_uuid(uuid) {
        return Err(duplicate_uuid_error(
            uuid.clone(),
            this_name,
            this_by_id,
            &existing.name,
            &existing.by_id,
        ));
    }
    Ok(())
}

/// FreshLuks's plan-time live-pool guard. A freshly-generated `new_v4()`
/// UUID has no legitimate same-backing live match (unlike a returned
/// PresentLuks disk), so a plain `pool.devices` scan is the right-sized
/// check -- backing-path classification is unnecessary. Fail closed on
/// the astronomically unlikely collision before journal write; refuse by
/// scope per ADR 024 (braid invents no identity for the foreign device),
/// routing through `duplicate_live_pool_uuid_error` so the variant and
/// message are byte-identical to the `PresentLuks` gates' refusal.
fn assert_fresh_uuid_absent_from_live_pool(
    uuid: &LuksUuid,
    live_pool: &PoolState,
    name: &DiskName,
    by_id: &ByIdPath,
) -> Result<(), AddError> {
    if live_pool.device_by_uuid(uuid).is_some() {
        return Err(duplicate_live_pool_uuid_error(uuid, name, by_id));
    }
    Ok(())
}

/// Render a live-pool UUID collision naming only the real add target,
/// reporting the colliding foreign device by scope. Per ADR 024 braid
/// invents no identity for the clone, so nothing here is derived from the
/// foreign device's mapper -- mirroring `replace`'s
/// `DuplicateUuid { scope: LivePool }` name-nothing-foreign contract.
fn duplicate_live_pool_uuid_error(uuid: &LuksUuid, name: &DiskName, by_id: &ByIdPath) -> AddError {
    AddError::DuplicateUuidLivePool {
        uuid: uuid.clone(),
        name: name.clone(),
        by_id: by_id.clone(),
    }
}

/// Sort the two `(name, by_id)` pairs lexicographically by `by_id`
/// (matching `discover.rs`'s `label_collision` ordering) so the
/// rendered `Display` is deterministic across cloned-disk inputs.
fn duplicate_uuid_error(
    uuid: LuksUuid,
    a_name: &DiskName,
    a_by_id: &ByIdPath,
    b_name: &DiskName,
    b_by_id: &ByIdPath,
) -> AddError {
    let (n1, b1, n2, b2) = if a_by_id <= b_by_id {
        (
            a_name.clone(),
            a_by_id.clone(),
            b_name.clone(),
            b_by_id.clone(),
        )
    } else {
        (
            b_name.clone(),
            b_by_id.clone(),
            a_name.clone(),
            a_by_id.clone(),
        )
    };
    AddError::DuplicateUuid {
        uuid,
        name1: n1,
        by_id1: b1,
        name2: n2,
        by_id2: b2,
    }
}

/// Defense-in-depth backstop: convert a `LuksUuidMap::insert`
/// conflict (which only fires if `assert_target_uuid_unique`
/// missed a case -- a logic bug) into an `AddError::Validation`.
/// The pre-write gate is the primary refusal; this path should
/// never trigger in practice.
fn target_uuid_map_conflict_to_validation(conflict: &membership::LuksUuidMapConflict) -> AddError {
    AddError::Validation(format!(
        "internal: duplicate LUKS UUID {} inserted into add targets map (defense-in-depth backstop fired; should have been refused by assert_target_uuid_unique)",
        conflict.uuid
    ))
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
    use crate::test_fixtures::{assert_lines_in_order, line_index};
    use std::collections::HashMap;

    fn disk(name: &str) -> DiskName {
        DiskName::parse(name).expect("test disk name")
    }

    fn test_paths() -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        (tmp, paths)
    }

    fn test_config() -> Config {
        Config::new(MountPoint::new("/mnt/storage".into())).unwrap()
    }

    fn read_test_config(path: &Path) -> Config {
        let bytes = std::fs::read(path).expect("test config should load");
        serde_json::from_slice(&bytes).expect("test config should parse")
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
            fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
                unreachable!(
                    "add::MockFs: read-only dry-run fixture; create_dir_all must never be called"
                )
            }
        }

        // Write a temp config
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.json");
        let mut f = std::fs::File::create(&config_path).unwrap();
        write!(f, r#"{{"mount_point":"/mnt/storage"}}"#).unwrap();

        let runner = MockRunner::default();
        let fs = MockFs;
        let (_state_dir, sp) = test_paths();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
                mapper: MapperName::from_basename("braid-existing".into()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vda".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: Some(Fsid::parse(fsid).unwrap()),
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
        let mn = MapperName::from_basename("braid-disk1".into());

        let result = classify_braid_disk_fsid(&runner, "disk1", &mn, &pool).unwrap();
        assert_eq!(result, AddLuksBtrfsProbe::NoBtrfs);
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
        let mn = MapperName::from_basename("braid-disk1".into());

        let result = classify_braid_disk_fsid(&runner, "disk1", &mn, &pool).unwrap();
        assert_eq!(result, AddLuksBtrfsProbe::ForeignPool);
    }

    // Intent: classify_braid_disk_fsid returns SamePool for a matching btrfs
    // FSID without deciding live-pool membership.
    // Why it exists: live-pool correlation must be UUID plus backing path,
    // not mapper-name equality inside the btrfs probe.
    // Scenario: a braid-labeled mapper has the mounted pool FSID; whether it
    // is already live or recoverable is decided by classify_live_pool_match.
    #[test]
    fn classify_fsid_same_pool() {
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
                mapper: MapperName::from_basename("braid-disk1".into()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vda".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: Some(Fsid::parse(fsid).unwrap()),
            null_underlying: vec![],
        };
        let mn = MapperName::from_basename("braid-disk1".into());

        let result = classify_braid_disk_fsid(&runner, "disk1", &mn, &pool).unwrap();
        assert_eq!(result, AddLuksBtrfsProbe::SamePool);
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
        let mn = MapperName::from_basename("braid-disk1".into());

        let result = classify_braid_disk_fsid(&runner, "disk1", &mn, &pool);
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("no UUID"),
            "expected error about missing UUID, got: {err}"
        );
    }

    fn pool_with_live_devices(devices: Vec<PoolDevice>) -> PoolState {
        let total_devices = devices.len() as u64;
        PoolState {
            mounted: true,
            devices,
            missing_count: 0,
            missing_devids: vec![],
            total_devices,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            null_underlying: vec![],
        }
    }

    fn live_pool_device(mapper: &str, uuid: &LuksUuid, underlying: &str) -> PoolDevice {
        PoolDevice {
            mapper: MapperName::from_basename(mapper.to_owned()),
            luks_uuid: uuid.clone(),
            devid: Devid::new(1),
            underlying: underlying.to_owned(),
        }
    }

    // Intent: classify_live_pool_match recognizes a UUID match as already
    // live only after the target by-id and pool row backing path match.
    // Why it exists: the add planner must tolerate mapper-name drift without
    // using mapper names as persistent identity.
    // Scenario: the candidate by-id resolves to the same kernel path as a
    // live pool row whose mapper is named braid-drifted.
    #[test]
    fn live_pool_match_same_backing() {
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap();
        let pool =
            pool_with_live_devices(vec![live_pool_device("braid-drifted", &uuid, "/dev/vdb")]);
        let resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path(by_id.as_str(), "/dev/vdb");

        let result = classify_live_pool_match(&uuid, &by_id, &pool, &resolver).unwrap();

        assert_eq!(result, LivePoolMatch::SameBacking);
    }

    // Intent: classify_live_pool_match rejects a UUID match whose backing
    // path differs from the target by-id.
    // Why it exists: a cloned LUKS header has the same UUID but is a
    // different physical disk, so treating UUID alone as a no-op is unsafe.
    // Scenario: the candidate by-id resolves to /dev/vdb while the live pool
    // row with the same UUID resolves to /dev/vdc.
    #[test]
    fn live_pool_match_different_backing() {
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap();
        let pool = pool_with_live_devices(vec![live_pool_device("braid-clone", &uuid, "/dev/vdc")]);
        let resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path(by_id.as_str(), "/dev/vdb");

        let result = classify_live_pool_match(&uuid, &by_id, &pool, &resolver).unwrap();

        assert_eq!(result, LivePoolMatch::DifferentBacking);
    }

    // Intent: classify_live_pool_match reports NoMatch when no live pool row
    // carries the candidate UUID.
    // Why it exists: returned-disk recovery must remain possible when the
    // disk belongs to the mounted btrfs FSID but is not currently live.
    // Scenario: the mounted pool has devices, but none have the target LUKS
    // UUID.
    #[test]
    fn live_pool_match_no_uuid() {
        let target_uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let other_uuid = LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap();
        let pool = pool_with_live_devices(vec![live_pool_device(
            "braid-existing",
            &other_uuid,
            "/dev/vdb",
        )]);

        let result = classify_live_pool_match(
            &target_uuid,
            &by_id,
            &pool,
            crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
        )
        .unwrap();

        assert_eq!(result, LivePoolMatch::NoMatch);
    }

    // Intent: classify_live_pool_match turns backing-path resolver failures
    // into hard validation errors.
    // Why it exists: ADR-024 requires backing-path proof before UUID reuse;
    // the add planner must not guess when canonicalization fails.
    // Scenario: a live pool row has the target UUID, but canonicalizing that
    // row's underlying device fails.
    #[test]
    fn live_pool_match_canonicalize_error() {
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap();
        let pool =
            pool_with_live_devices(vec![live_pool_device("braid-existing", &uuid, "/dev/vdb")]);
        let resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path(by_id.as_str(), "/dev/vdb")
            .with_error("/dev/vdb", std::io::ErrorKind::NotFound);

        let err = classify_live_pool_match(&uuid, &by_id, &pool, &resolver).unwrap_err();

        assert!(matches!(err, AddError::Validation(_)));
        assert!(
            err.to_string()
                .contains("could not canonicalize live pool backing path"),
            "expected live backing canonicalization error, got: {err}"
        );
    }

    // Intent: classify_live_pool_match still scans later matching UUID rows
    // after seeing an earlier different backing.
    // Why it exists: a clone row must not mask a later canonicalization
    // failure; backing-path proof is required for every same-UUID live row.
    // Scenario: the first live row has different backing, and a second
    // same-UUID live row cannot be canonicalized.
    #[test]
    fn live_pool_match_canonicalize_error_after_different_backing() {
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap();
        let pool = pool_with_live_devices(vec![
            live_pool_device("braid-clone", &uuid, "/dev/vdc"),
            live_pool_device("braid-unknown", &uuid, "/dev/missing"),
        ]);
        let resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path(by_id.as_str(), "/dev/vdb")
            .with_error("/dev/missing", std::io::ErrorKind::NotFound);

        let err = classify_live_pool_match(&uuid, &by_id, &pool, &resolver).unwrap_err();

        assert!(matches!(err, AddError::Validation(_)));
        assert!(
            err.to_string().contains("/dev/missing"),
            "expected later backing canonicalization error, got: {err}"
        );
    }

    // Intent: classify_live_pool_match gives DifferentBacking precedence
    // when duplicate live UUID rows include both same and different backing.
    // Why it exists: probe_pool does not dedupe by LUKS UUID, so a clone can
    // appear alongside the legitimate open mapper.
    // Scenario: one live row matches the candidate backing path and another
    // live row with the same UUID points at a different kernel device.
    #[test]
    fn live_pool_match_mixed_same_and_different_backing() {
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap();
        let pool = pool_with_live_devices(vec![
            live_pool_device("braid-legit", &uuid, "/dev/vdb"),
            live_pool_device("braid-clone", &uuid, "/dev/vdc"),
        ]);
        let resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path(by_id.as_str(), "/dev/vdb");

        let result = classify_live_pool_match(&uuid, &by_id, &pool, &resolver).unwrap();

        // DifferentBacking dominates: had the same-backing legit row won, this
        // would be SameBacking. The colliding row's handle is intentionally not
        // carried (per ADR 024 the refusal names nothing foreign).
        assert_eq!(result, LivePoolMatch::DifferentBacking);
    }

    fn recoverable_target(name: &str, by_id: &str, uuid: &str) -> RecoverableBraidTarget {
        let name = DiskName::parse(name).unwrap();
        RecoverableBraidTarget {
            mapper_path: mapper_name(&name).dev_path(),
            header_backup_path: luks_header_backup_path(Path::new("/tmp"), &mapper_name(&name)),
            by_id: ByIdPath::parse(by_id).unwrap(),
            luks_uuid: LuksUuid::parse(uuid).unwrap(),
            verified_pool_fsid: Fsid::parse(POOL_FSID).unwrap(),
            enroll_key_file: None,
            name,
        }
    }

    fn fresh_target(name: &str, by_id: &str, uuid: &str) -> FreshLuksTarget {
        let name = DiskName::parse(name).unwrap();
        FreshLuksTarget {
            mapper_name: mapper_name(&name),
            mapper_path: mapper_name(&name).dev_path(),
            header_backup_path: luks_header_backup_path(Path::new("/tmp"), &mapper_name(&name)),
            by_id: ByIdPath::parse(by_id).unwrap(),
            luks_uuid: LuksUuid::parse(uuid).unwrap(),
            luks_format_extra_opts: LuksFormatExtraOpts::default(),
            enroll_key_file: None,
            name,
        }
    }

    fn journal_targets_with(
        uuid: LuksUuid,
        target: journal::AddJournalTarget,
    ) -> LuksUuidMap<journal::AddJournalTarget> {
        let mut targets = LuksUuidMap::new();
        targets.insert(uuid, target).expect("unique test UUID");
        targets
    }

    fn plan_for_execute_target(
        target: AddTargetWork,
        initial_journal_targets: LuksUuidMap<journal::AddJournalTarget>,
        pool: PoolState,
    ) -> AddPlan {
        let (name, by_id, probed_state) = match &target {
            AddTargetWork::Fresh(target) => (
                target.name.clone(),
                target.by_id.clone(),
                PresentConfigDiskState::PresentNotLuks,
            ),
            AddTargetWork::OpenRecoverable(target) => (
                target.name.clone(),
                target.by_id.clone(),
                PresentConfigDiskState::PresentLuks {
                    uuid: target.luks_uuid.clone(),
                    label: Some(luks_label_for(&target.name).as_str().to_owned()),
                    mapper_open: true,
                },
            ),
            AddTargetWork::ClosedPresentLuks(target) => (
                target.name.clone(),
                target.by_id.clone(),
                PresentConfigDiskState::PresentLuks {
                    uuid: target.luks_uuid.clone(),
                    label: Some(luks_label_for(&target.name).as_str().to_owned()),
                    mapper_open: false,
                },
            ),
        };
        let probed = vec![PresentConfigDisk {
            name: name.clone(),
            by_id_path: by_id.clone(),
            state: probed_state,
        }];
        let targets = vec![target];
        let preview_phase = add_preview_phase(&pool, targets.len());
        AddPlan {
            notes: vec![],
            work_plan: AddWorkPlan {
                prelude: AddCredentialPrelude {
                    confirm_disks: vec![],
                    confirm_new: false,
                    verify_targets: vec![],
                    pool_target_count: 0,
                },
                targets,
                initial_journal_targets,
                mount_point: MountPoint::new("/mnt/storage".into()),
                preview_phase,
            },
            config: Config::new(MountPoint::new("/mnt/storage".into())).unwrap(),
            parsed: vec![(name.clone(), by_id.clone())],
            names: vec![name],
            by_ids: vec![by_id],
            probed,
            pool,
            pool_membership: PoolMembership::empty(),
        }
    }

    fn execute_fixture() -> (
        tempfile::TempDir,
        StatePaths,
        tempfile::TempDir,
        std::path::PathBuf,
    ) {
        let (state_tmp, paths) = test_paths();
        let tmp = tempfile::tempdir().unwrap();
        let pass_path = tmp.path().join("passphrase");
        std::fs::write(&pass_path, b"test-passphrase\n").unwrap();
        (state_tmp, paths, tmp, pass_path)
    }

    fn btrfs_show_pool(fsid: &str, devices: &[(&str, u64)]) -> RawCommandOutput {
        let mut out = format!(
            "Label: none  uuid: {fsid}\n\tTotal devices {} FS bytes used 16.17MiB\n",
            devices.len()
        );
        for (mapper, devid) in devices {
            out.push_str(&format!(
                "\tdevid    {devid} size 496.00MiB used 121.56MiB path /dev/mapper/{mapper}\n"
            ));
        }
        mock_ok("btrfs filesystem show /mnt/storage", &out)
    }

    fn pool_probe_runner(fsid: &str, devices: &[(&str, u64, &str, &str)]) -> MockRunner {
        let show_devices: Vec<(&str, u64)> = devices
            .iter()
            .map(|(mapper, devid, _, _)| (*mapper, *devid))
            .collect();
        let mut runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: MountPoint::new("/mnt/storage".into()),
            },
            btrfs_show_pool(fsid, &show_devices),
        );
        for (mapper, _, underlying, uuid) in devices {
            runner = runner
                .with_output(
                    CmdRequest::CryptsetupStatus {
                        mapper: MapperName::from_basename((*mapper).to_owned()),
                    },
                    mock_status_active(mapper, underlying),
                )
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: (*underlying).to_owned(),
                    },
                    mock_luks_uuid(underlying, uuid),
                );
        }
        runner
    }

    #[derive(Default)]
    struct CountingBackingPathResolver {
        inner: crate::test_fixtures::MockBackingPathResolver,
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl CountingBackingPathResolver {
        fn with_path(mut self, path: &str, canonical: &str) -> Self {
            self.inner = self.inner.with_path(path, canonical);
            self
        }

        fn calls_to(&self, path: &str) -> usize {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .filter(|called| called.as_str() == path)
                .count()
        }
    }

    impl BackingPathResolver for CountingBackingPathResolver {
        fn canonicalize(&self, path: &str) -> Result<String, std::io::Error> {
            self.calls.lock().unwrap().push(path.to_owned());
            self.inner.canonicalize(path)
        }
    }

    // Intent: AddPlan::execute rejects an OpenRecoverable target when a fresh
    // live-pool probe sees the same LUKS UUID under a different backing path.
    // Why it exists: OpenRecoverable enters execute through the planner's
    // `NoMatch` branch and had no execute-time live-pool collision check.
    // Scenario: an external actor adds a cloned-header mapper to the pool
    // while `braid add` is waiting for confirmation; execute must surface the
    // canonical duplicate-UUID refusal before journal write.
    #[test]
    fn execute_live_pool_recheck_rejects_different_backing() {
        const UUID: &str = "22222222-2222-2222-2222-222222222222";
        const BY_ID: &str = "/dev/disk/by-id/virtio-disk2";
        let target = recoverable_target("disk2", BY_ID, UUID);
        let journal_targets = journal_targets_with(
            target.luks_uuid.clone(),
            recoverable_journal_target(&target),
        );
        let plan = plan_for_execute_target(
            AddTargetWork::OpenRecoverable(target),
            journal_targets,
            pool_mounted_with_fsid(POOL_FSID),
        );
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let runner = pool_probe_runner(POOL_FSID, &[("clone-foreign", 2, "/dev/vde", UUID)]);
        let fs = AddMockFs(vec![BY_ID.into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        let resolver =
            crate::test_fixtures::MockBackingPathResolver::default().with_path(BY_ID, "/dev/vdc");

        let err = plan
            .execute(
                &runner,
                &fs,
                &AddParams {
                    config: &test_config(),
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
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver: &resolver,
                },
            )
            .unwrap_err();

        let body = err.to_string();
        match err {
            AddError::DuplicateUuidLivePool { uuid, name, by_id } => {
                assert_eq!(uuid.as_str(), UUID);
                assert_eq!(name.as_str(), "disk2");
                assert_eq!(by_id.as_str(), BY_ID);
            }
            other => panic!("expected DuplicateUuidLivePool, got: {other:?}"),
        }
        assert!(
            body.contains("add target braid-disk2 (/dev/disk/by-id/virtio-disk2)"),
            "live-pool refusal must name the real add target: {body}"
        );
        assert!(
            body.contains("live pool"),
            "live-pool refusal must report the colliding side by scope: {body}"
        );
        assert!(
            !body.contains("braid-braid"),
            "live-pool refusal must not double-prefix: {body}"
        );
        assert!(
            !body.contains("clone-foreign"),
            "live-pool refusal must surface nothing derived from the foreign mapper: {body}"
        );
        assert_eq!(inhibitor.acquire_count(), 0);
        assert!(journal::load_journal(&paths).unwrap().is_none());
    }

    // Intent: AddPlan::execute rejects a target that became a live pool member
    // under its own backing path between planning and journal write.
    // Why it exists: SameBacking at execute time means the plan is stale; add
    // must fail closed rather than journal work that another actor already did.
    // Scenario: recovery replay or a parallel operator adds the candidate
    // mapper after planning but before this invocation reaches the pool phase.
    #[test]
    fn execute_live_pool_recheck_rejects_same_backing() {
        const UUID: &str = "22222222-2222-2222-2222-222222222222";
        const BY_ID: &str = "/dev/disk/by-id/virtio-disk2";
        let target = recoverable_target("disk2", BY_ID, UUID);
        let journal_targets = journal_targets_with(
            target.luks_uuid.clone(),
            recoverable_journal_target(&target),
        );
        let plan = plan_for_execute_target(
            AddTargetWork::OpenRecoverable(target),
            journal_targets,
            pool_mounted_with_fsid(POOL_FSID),
        );
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let runner = pool_probe_runner(POOL_FSID, &[("braid-disk2", 2, "/dev/vdc", UUID)]);
        let fs = AddMockFs(vec![BY_ID.into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        let resolver =
            crate::test_fixtures::MockBackingPathResolver::default().with_path(BY_ID, "/dev/vdc");

        let err = plan
            .execute(
                &runner,
                &fs,
                &AddParams {
                    config: &test_config(),
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
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver: &resolver,
                },
            )
            .unwrap_err();

        match err {
            AddError::Validation(msg) => {
                assert!(
                    msg.contains("pool state changed between planning and execution"),
                    "expected stale-pool validation, got: {msg}"
                );
                assert!(msg.contains("disk 'disk2'"));
                assert!(msg.contains(UUID));
                assert!(msg.contains("Re-run `braid add`"));
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert_eq!(inhibitor.acquire_count(), 0);
        assert!(journal::load_journal(&paths).unwrap().is_none());
    }

    // Intent: the NoMatch execute-time re-check actually invokes
    // classify_live_pool_match for each journal target before journal write.
    // Why it exists: NoMatch is the silent pass arm; a downstream failure
    // alone would not prove the fresh-pool check still ran.
    // Scenario: a fresh target reaches execute, the live pool has no matching
    // UUID, and a forced luksFormat failure leaves the journal in place.
    #[test]
    fn execute_live_pool_recheck_no_match_invokes_resolver_for_target() {
        const UUID: &str = "33333333-3333-3333-3333-333333333333";
        const BY_ID: &str = "/dev/disk/by-id/virtio-disk2";
        let target = fresh_target("disk2", BY_ID, UUID);
        let journal_targets =
            journal_targets_with(target.luks_uuid.clone(), fresh_journal_target(&target));
        let plan = plan_for_execute_target(
            AddTargetWork::Fresh(target.clone()),
            journal_targets,
            pool_mounted_with_fsid(POOL_FSID),
        );
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let runner = pool_probe_runner(
            POOL_FSID,
            &[(
                "braid-disk1",
                1,
                "/dev/vdb",
                "11111111-1111-1111-1111-111111111111",
            )],
        )
        .with_output(
            CmdRequest::CryptsetupLuksFormat {
                device: BY_ID.to_owned(),
                uuid: target.luks_uuid.clone(),
                label: luks_label_for(&target.name),
                extra_opts: LuksFormatExtraOpts::default(),
            },
            RawCommandOutput {
                cmd: "cryptsetup luksFormat".into(),
                stdout: String::new(),
                stderr: "mock luksFormat failure after journal write".into(),
                exit_status: 9,
            },
        );
        let fs = AddMockFs(vec![BY_ID.into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        let resolver = CountingBackingPathResolver::default().with_path(BY_ID, "/dev/vdc");

        let err = plan
            .execute(
                &runner,
                &fs,
                &AddParams {
                    config: &test_config(),
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
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver: &resolver,
                },
            )
            .unwrap_err();

        assert!(
            err.to_string().contains("mock luksFormat failure"),
            "expected forced luksFormat failure, got: {err}"
        );
        assert!(
            resolver.calls_to(BY_ID) >= 1,
            "execute re-check must canonicalize the target by-id"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_some(),
            "journal should survive the forced post-write failure"
        );
        assert_eq!(inhibitor.acquire_count(), 1);
    }

    // Intent: execute rejects a mounted-plan snapshot when the fresh pool
    // probe reports the mount disappeared before journal write.
    // Why it exists: without the mount-state guard, an empty fresh pool would
    // make every per-target UUID check look like NoMatch.
    // Scenario: the pool unmounts after planning a live add but before
    // execution reaches the irreversible section.
    #[test]
    fn execute_pool_identity_guard_rejects_planned_mounted_now_unmounted() {
        const UUID: &str = "33333333-3333-3333-3333-333333333333";
        const BY_ID: &str = "/dev/disk/by-id/virtio-disk2";
        let target = fresh_target("disk2", BY_ID, UUID);
        let journal_targets =
            journal_targets_with(target.luks_uuid.clone(), fresh_journal_target(&target));
        let plan = plan_for_execute_target(
            AddTargetWork::Fresh(target),
            journal_targets,
            pool_mounted_with_fsid(POOL_FSID),
        );
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let runner = MockRunner::default();
        let fs = AddOfflineMockFs(vec![BY_ID.into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        let resolver = CountingBackingPathResolver::default().with_path(BY_ID, "/dev/vdc");

        let err = plan
            .execute(
                &runner,
                &fs,
                &AddParams {
                    config: &test_config(),
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
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver: &resolver,
                },
            )
            .unwrap_err();

        match err {
            AddError::Validation(msg) => {
                assert!(msg.contains("pool unmounted between planning and execution"));
                assert!(msg.contains("Re-mount /mnt/storage"));
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert_eq!(resolver.calls_to(BY_ID), 0);
        assert_eq!(inhibitor.acquire_count(), 0);
        assert!(journal::load_journal(&paths).unwrap().is_none());
    }

    // Intent: execute rejects a bootstrap plan when a pool appears at the
    // mount point before the destructive mkfs.btrfs branch.
    // Why it exists: the unmounted-plan path must not bootstrap over a
    // filesystem that appeared after planning.
    // Scenario: `braid add` plans a fresh bootstrap, then another actor mounts
    // a pool at /mnt/storage before execution reaches journal write.
    #[test]
    fn execute_pool_identity_guard_rejects_planned_unmounted_now_mounted() {
        const UUID: &str = "33333333-3333-3333-3333-333333333333";
        const BY_ID: &str = "/dev/disk/by-id/virtio-disk2";
        let target = fresh_target("disk2", BY_ID, UUID);
        let journal_targets =
            journal_targets_with(target.luks_uuid.clone(), fresh_journal_target(&target));
        let plan = plan_for_execute_target(
            AddTargetWork::Fresh(target),
            journal_targets,
            pool_unmounted(),
        );
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let runner = RequestRecordingRunner::new(pool_probe_runner(
            POOL_FSID,
            &[(
                "braid-disk1",
                1,
                "/dev/vdb",
                "11111111-1111-1111-1111-111111111111",
            )],
        ));
        let fs = AddMockFs(vec![BY_ID.into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        let resolver = CountingBackingPathResolver::default().with_path(BY_ID, "/dev/vdc");

        let err = plan
            .execute(
                &runner,
                &fs,
                &AddParams {
                    config: &test_config(),
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
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver: &resolver,
                },
            )
            .unwrap_err();

        match err {
            AddError::Validation(msg) => {
                assert!(msg.contains("a pool appeared at /mnt/storage"));
                assert!(msg.contains("aborting before `mkfs.btrfs`"));
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert_eq!(resolver.calls_to(BY_ID), 0);
        assert!(
            !runner
                .requests()
                .iter()
                .any(|req| matches!(req, CmdRequest::MkfsBtrfs { .. })),
            "mkfs.btrfs must not run after mount-state drift"
        );
        assert_eq!(inhibitor.acquire_count(), 0);
        assert!(journal::load_journal(&paths).unwrap().is_none());
    }

    // Intent: execute rejects a mounted plan when the fresh pool probe reports
    // a different btrfs FSID before journal write.
    // Why it exists: matching mount state is not enough; live-pool
    // re-classification must be against the same filesystem the planner saw.
    // Scenario: a different btrfs filesystem is mounted at the configured
    // mount point between planning and execution.
    #[test]
    fn execute_pool_identity_guard_rejects_fsid_drift() {
        const UUID: &str = "33333333-3333-3333-3333-333333333333";
        const BY_ID: &str = "/dev/disk/by-id/virtio-disk2";
        const OLD_FSID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
        const NEW_FSID: &str = "bbbbbbbb-cccc-dddd-eeee-ffffffffffff";
        let target = fresh_target("disk2", BY_ID, UUID);
        let journal_targets =
            journal_targets_with(target.luks_uuid.clone(), fresh_journal_target(&target));
        let plan = plan_for_execute_target(
            AddTargetWork::Fresh(target),
            journal_targets,
            pool_mounted_with_fsid(OLD_FSID),
        );
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let runner = pool_probe_runner(
            NEW_FSID,
            &[(
                "braid-disk1",
                1,
                "/dev/vdb",
                "11111111-1111-1111-1111-111111111111",
            )],
        );
        let fs = AddMockFs(vec![BY_ID.into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        let resolver = CountingBackingPathResolver::default().with_path(BY_ID, "/dev/vdc");

        let err = plan
            .execute(
                &runner,
                &fs,
                &AddParams {
                    config: &test_config(),
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
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver: &resolver,
                },
            )
            .unwrap_err();

        match err {
            AddError::Validation(msg) => {
                assert!(msg.contains("pool fsid changed between planning and execution"));
                assert!(msg.contains(OLD_FSID));
                assert!(msg.contains(NEW_FSID));
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert_eq!(resolver.calls_to(BY_ID), 0);
        assert_eq!(inhibitor.acquire_count(), 0);
        assert!(journal::load_journal(&paths).unwrap().is_none());
    }

    // --- add work-plan identity tests ---

    fn probed_present_luks(
        name: &str,
        mapper_open: bool,
        label: Option<String>,
    ) -> PresentConfigDisk {
        PresentConfigDisk {
            name: DiskName::parse(name).expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/disk1").unwrap(),
            state: PresentConfigDiskState::PresentLuks {
                uuid: LuksUuid::parse("a1b2c3d4-e5f6-7890-abcd-ef1234567890").unwrap(),
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
                names: &[DiskName::parse("disk1").unwrap()],
                by_ids: &[&ByIdPath::parse("/dev/disk/by-id/disk1").unwrap()],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
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
                names: &[DiskName::parse("disk1").unwrap()],
                by_ids: &[&ByIdPath::parse("/dev/disk/by-id/disk1").unwrap()],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
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
                names: &[DiskName::parse("disk1").unwrap()],
                by_ids: &[&ByIdPath::parse("/dev/disk/by-id/disk1").unwrap()],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
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
                names: &[DiskName::parse("disk1").unwrap()],
                by_ids: &[&ByIdPath::parse("/dev/disk/by-id/disk1").unwrap()],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
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
        let probed = vec![PresentConfigDisk {
            name: DiskName::parse("disk1").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/disk1").unwrap(),
            state: PresentConfigDiskState::PresentNotLuks,
        }];
        let pool = pool_unmounted();

        let steps = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &[DiskName::parse("disk1").unwrap()],
                by_ids: &[&ByIdPath::parse("/dev/disk/by-id/disk1").unwrap()],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
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
        let by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap();
        let luks_uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let probed = vec![PresentConfigDisk {
            name: DiskName::parse("disk2").expect("valid disk name in test fixture"),
            by_id_path: by_id.clone(),
            state: PresentConfigDiskState::PresentLuks {
                uuid: luks_uuid.clone(),
                label: Some("braid-disk2".to_owned()),
                mapper_open: true,
            },
        }];
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-disk1".into()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vdb".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: Some(Fsid::parse(pool_fsid).unwrap()),
            null_underlying: vec![],
        };
        let mut pool_membership = PoolMembership::empty();
        pool_membership
            .insert(
                crate::test_fixtures::test_uuid(500),
                membership::DiskMember {
                    name: DiskName::parse("disk1").unwrap(),
                    by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
                    devid: None,
                    added_at: None,
                },
            )
            .expect("test fixture insert");
        let runner = NoDumpRunner {
            inner: RecoverableAddRunner::new(),
        };

        let work_plan = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &[DiskName::parse("disk2").unwrap()],
                by_ids: &[&by_id],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &paths,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
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
            config: Config::new(MountPoint::new("/mnt/storage".into())).unwrap(),
            parsed: vec![(DiskName::parse("disk2").unwrap(), by_id.clone())],
            names: vec![DiskName::parse("disk2").unwrap()],
            by_ids: vec![by_id.clone()],
            probed,
            pool,
            pool_membership,
        };
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        plan.execute(
            &runner,
            &AddMockFs(vec![]),
            &AddParams {
                config: &test_config(),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
            &disk("disk1"),
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
            &disk("disk1"),
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
        // Intent: identity_to_error emits the canonical NoBtrfs error.
        // Why it exists: this was the variant where message text had already diverged
        //   between cmd_add and add work-plan rendering. Pinning it prevents recurrence.
        // Scenario: a braid-labeled disk has its LUKS contents wiped or is partially
        //   initialized; btrfs superblock is absent.
        let err = identity_to_error(&AddLuksBtrfsProbe::NoBtrfs, "disk1")
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
        // Intent: identity_to_error emits the canonical ForeignPool error.
        // Why it exists: pins the error text so both call sites can't drift independently.
        // Scenario: user tries to add a braid-labeled disk from a different NAS.
        let err = identity_to_error(&AddLuksBtrfsProbe::ForeignPool, "disk1")
            .unwrap()
            .to_string();
        assert!(err.contains("different btrfs filesystem"), "got: {err}");
        assert!(
            err.contains("braid will not merge foreign pools"),
            "got: {err}"
        );
    }

    #[test]
    fn identity_to_error_same_pool_returns_none() {
        // Intent: identity_to_error returns None for a same-pool btrfs probe.
        // Why it exists: callers rely on None meaning the btrfs probe succeeded
        //   and live-pool membership classification should continue separately.
        // Scenario: normal add sees the mounted pool FSID on a braid-labeled mapper.
        assert!(identity_to_error(&AddLuksBtrfsProbe::SamePool, "disk1").is_none());
    }

    #[test]
    fn dry_run_and_execution_produce_same_no_btrfs_error() {
        // Intent: add work-plan rendering and cmd_add produce identical NoBtrfs
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
                names: &[DiskName::parse("disk1").unwrap()],
                by_ids: &[&ByIdPath::parse("/dev/disk/by-id/disk1").unwrap()],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
            },
        )
        .unwrap_err()
        .to_string();

        // execution path: identity_to_error is the shared function cmd_add calls
        let exec_err = identity_to_error(&AddLuksBtrfsProbe::NoBtrfs, "disk1")
            .unwrap()
            .to_string();

        assert_eq!(
            dry_err, exec_err,
            "dry-run and execution paths must produce identical NoBtrfs error"
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
                self.closed.lock().unwrap().push(mapper.as_str().to_owned());
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
            guard.track(disk("aaa"), MapperName::from_basename("braid-aaa".into()));
            guard.track(disk("bbb"), MapperName::from_basename("braid-bbb".into()));
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
            guard.track(disk("aaa"), MapperName::from_basename("braid-aaa".into()));
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
                mapper: MapperName::from_basename("braid-aaa".into()),
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
            guard.track(disk("aaa"), MapperName::from_basename("braid-aaa".into()));
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

    // Intent: cleanup [wait]/[ok] rows label the disk by its operator
    //   DiskName, never the tracked mapper's basename.
    // Why it exists: ADR 024 forbids deriving user-facing disk labels from a
    //   mapper basename; a regression to strip_prefix("braid-") would silently
    //   re-introduce mapper-derived labels and otherwise slip through.
    // Scenario: add tracked a mapper opened under a drifted basename
    //   (braid-WRONG) for the disk the operator named `disk2`; the guard fires
    //   on unwind and the cleanup rows must say `disk disk2`, not `WRONG`.
    #[test]
    fn guard_cleanup_row_uses_disk_name_under_mapper_drift() {
        let runner = SpyRunner::new(MockRunner::default());
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            let mut guard = LuksCleanupGuard::new(&runner);
            guard.track(
                disk("disk2"),
                MapperName::from_basename("braid-WRONG".into()),
            );
        });

        assert!(
            captured.contains("[wait] disk disk2: locking (cleanup)..."),
            "missing drift-safe wait row: {captured:?}"
        );
        assert!(
            captured.contains("[ok]   disk disk2: locked (cleanup)"),
            "missing drift-safe ok row: {captured:?}"
        );
        assert!(
            !captured.contains("WRONG"),
            "cleanup row must not echo drifted mapper basename: {captured:?}"
        );
        let closed = runner.closed.lock().unwrap();
        assert_eq!(*closed, vec!["braid-WRONG"]);
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
            guard.track(disk("aaa"), MapperName::from_basename("braid-aaa".into()));
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
            guard.track(disk("new"), MapperName::from_basename("braid-new".into()));
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
        let by_id = ByIdPath::parse("/dev/disk/by-id/existing").unwrap();
        let uuid = "11111111-1111-1111-1111-111111111111";
        let runner = MockRunner::default()
            .with_mapper_open("braid-existing", "/dev/vdb", uuid)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: by_id.as_str().to_owned(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksUUID".into(),
                    stdout: format!("{uuid}\n"),
                    stderr: String::new(),
                    exit_status: 0,
                },
            );
        let backing_path_resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path("/dev/disk/by-id/existing", "/dev/vdb");

        {
            let mut guard = LuksCleanupGuard::new(&runner);
            if ensure_luks_open(
                &runner,
                &disk("existing"),
                &by_id,
                &backing_path_resolver,
                &passphrase("testpass"),
            )
            .unwrap()
                == OpenOutcome::Opened
            {
                guard.track(
                    disk("existing"),
                    MapperName::from_basename("braid-existing".into()),
                );
            }
            // guard drops here while still armed
        }

        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-existing")),
            "already-owned mapper must not be closed by add cleanup guard"
        );
    }

    struct RecoverableAddRunner {
        disk2_added: std::sync::atomic::AtomicBool,
        disk2_opened: std::sync::atomic::AtomicBool,
        /// When set, `pool_show` appends a `path MISSING` row and bumps
        /// `Total devices`, so the fresh execute-time probe is degraded too
        /// and the degraded-add skip test models reality instead of leaning
        /// only on the planned `PoolState.missing_count`.
        degraded: bool,
        /// When set, `pool_show` reports disk1 as MISSING only after disk2 was
        /// added. This models a plan-healthy pool that degrades in the window
        /// before the authoritative post-add balance gate.
        degrade_after_add: bool,
        /// When set, answer `LsblkField` for the disk2 by-id target with the
        /// canonical `HW_*` model/serial/size so the confirm-prompt routing
        /// test can pin that hw is probed via the by-id handle (decision 024).
        /// Default off so the shared `::new()`/`::degraded()` callers keep the
        /// `MissingMock` -> blank-hw contract their byte-exact prompts assume.
        report_hw: bool,
    }

    impl RecoverableAddRunner {
        /// Canonical hardware the by-id probe reports when `report_hw` is set.
        /// Shared between the gated `LsblkField` arm and the confirm-routing
        /// test's `expected`, so the two cannot drift apart.
        const HW_MODEL: &'static str = "Samsung SSD 870 QVO";
        const HW_SERIAL: &'static str = "ADD2SERIAL";
        const HW_SIZE: u64 = 8_000_000_000_000;

        fn new() -> Self {
            Self {
                disk2_added: std::sync::atomic::AtomicBool::new(false),
                disk2_opened: std::sync::atomic::AtomicBool::new(false),
                degraded: false,
                degrade_after_add: false,
                report_hw: false,
            }
        }

        fn degraded() -> Self {
            Self {
                degraded: true,
                ..Self::new()
            }
        }

        fn degrades_after_add() -> Self {
            Self {
                degrade_after_add: true,
                ..Self::new()
            }
        }

        /// Like `new()`, but answers the disk2 by-id `LsblkField` probe so the
        /// confirm prompt's hw line resolves. Isolated behind its own
        /// constructor so the many `::new()` callers keep blank-hw prompts.
        fn with_hw_info() -> Self {
            Self {
                report_hw: true,
                ..Self::new()
            }
        }

        fn pool_show(&self) -> String {
            let disk2_added = self.disk2_added.load(std::sync::atomic::Ordering::SeqCst);
            let disk2_line = if disk2_added {
                "\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n"
            } else {
                ""
            };
            if self.degrade_after_add && disk2_added {
                return format!(
                    "Label: none  uuid: {POOL_FSID}\n\
                     \tTotal devices 2 FS bytes used 16.17MiB\n\
                     \tdevid    1 size 0 used 0 path MISSING\n\
                     {disk2_line}"
                );
            }
            let present = if disk2_line.is_empty() { 1 } else { 2 };
            // Missing placeholder sits at the devid after the present devices,
            // mirroring AddPlanTestRunner's synthesis; total counts it so
            // probe_pool computes missing_count = total - present.
            let (total, missing_line) = if self.degraded {
                let missing_devid = present + 1;
                (
                    present + 1,
                    format!("\tdevid    {missing_devid} size 0 used 0 path MISSING\n"),
                )
            } else {
                (present, String::new())
            };
            format!(
                "Label: none  uuid: {POOL_FSID}\n\
                 \tTotal devices {total} FS bytes used 16.17MiB\n\
                 \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
                 {disk2_line}{missing_line}"
            )
        }
    }

    impl CommandRunner for RecoverableAddRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    &self.pool_show(),
                )),
                CmdRequest::CryptsetupStatus { mapper } => match mapper.as_str() {
                    "braid-disk1" => Ok(mock_status_active("braid-disk1", "/dev/vdb")),
                    "braid-disk2"
                        if self.disk2_opened.load(std::sync::atomic::Ordering::SeqCst)
                            || self.disk2_added.load(std::sync::atomic::Ordering::SeqCst) =>
                    {
                        Ok(mock_status_active("braid-disk2", "/dev/vdc"))
                    }
                    "braid-disk2" => Ok(mock_status_inactive("braid-disk2")),
                    _ => Err(CmdError::MissingMock),
                },
                CmdRequest::CryptsetupLuksUuid { device } => match device.as_str() {
                    "/dev/vdb" | "/dev/disk/by-id/virtio-disk1" => Ok(mock_luks_uuid(
                        device,
                        "11111111-1111-1111-1111-111111111111",
                    )),
                    "/dev/vdc" | "/dev/disk/by-id/virtio-disk2" => Ok(mock_luks_uuid(
                        device,
                        "22222222-2222-2222-2222-222222222222",
                    )),
                    _ => Err(CmdError::MissingMock),
                },
                CmdRequest::BtrfsFilesystemShowTarget { target } => {
                    let mut out = btrfs_show_with_uuid(POOL_FSID);
                    out.cmd = format!("btrfs filesystem show {target}");
                    Ok(out)
                }
                CmdRequest::CryptsetupTestPassphrase { device } => Ok(mock_ok(
                    &format!("cryptsetup open --test-passphrase {device}"),
                    "",
                )),
                CmdRequest::CryptsetupLuksFormat { .. } => Ok(mock_ok("cryptsetup luksFormat", "")),
                CmdRequest::CryptsetupLuksHeaderBackup { backup_path, .. } => {
                    if let Some(parent) = std::path::Path::new(backup_path.as_str()).parent() {
                        std::fs::create_dir_all(parent)
                            .map_err(|e| CmdError::Failed(format!("mock: create_dir_all: {e}")))?;
                    }
                    std::fs::write(backup_path, b"")
                        .map_err(|e| CmdError::Failed(format!("mock: write backup: {e}")))?;
                    Ok(mock_ok("cryptsetup luksHeaderBackup", ""))
                }
                CmdRequest::CryptsetupLuksOpen { .. } => {
                    self.disk2_opened
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(mock_ok("cryptsetup luksOpen", ""))
                }
                CmdRequest::BtrfsDeviceScanForget { .. } => Ok(mock_ok("btrfs scan forget", "")),
                CmdRequest::WipefsBtrfs { .. } => Ok(mock_ok("wipefs", "")),
                CmdRequest::BtrfsDeviceAdd { .. } => {
                    self.disk2_added
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(mock_ok("btrfs device add", ""))
                }
                CmdRequest::BtrfsBalanceRaid1 { .. } => Ok(mock_ok("btrfs balance", "")),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                CmdRequest::LsblkField { device, field }
                    if self.report_hw && device == "/dev/disk/by-id/virtio-disk2" =>
                {
                    let value = match field {
                        crate::cmd::LsblkFieldKind::Model => Self::HW_MODEL.to_owned(),
                        crate::cmd::LsblkFieldKind::Serial => Self::HW_SERIAL.to_owned(),
                        crate::cmd::LsblkFieldKind::Size => Self::HW_SIZE.to_string(),
                    };
                    Ok(mock_ok("lsblk", &value))
                }
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::CryptsetupLuksOpen { .. } => {
                    self.disk2_opened
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                    Ok(mock_ok("cryptsetup luksOpen", ""))
                }
                CmdRequest::CryptsetupTestPassphrase { .. } => self.run(request),
                _ => self.run(request),
            }
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
                CmdRequest::CryptsetupStatus { mapper } if mapper.as_str() == "braid-disk2" => {
                    Ok(mock_status_inactive(mapper.as_str()))
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
     * to be announced; the SamePool + closed-mapper state
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
        // The runner reports only disk1 until BtrfsDeviceAdd succeeds, then
        // reports disk1+disk2 so the execute-time re-check sees the same
        // pre-add pool the planner saw while the post-add probe can enrich
        // membership normally.
        let runner = RecoverableAddRunner::new();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();

        let config = crate::config::Config::new(MountPoint::new("/mnt/storage".into())).unwrap();
        let by_id_disk2 = ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap();
        let mut pool_membership = membership::PoolMembership::empty();
        pool_membership
            .insert(
                crate::test_fixtures::test_uuid(501),
                membership::DiskMember {
                    name: DiskName::parse("disk1").unwrap(),
                    by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
                    devid: None,
                    added_at: None,
                },
            )
            .expect("test fixture insert");
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-disk1".into()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vdb".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: Some(Fsid::parse(POOL_FSID).unwrap()),
            null_underlying: vec![],
        };
        let probed = vec![PresentConfigDisk {
            name: DiskName::parse("disk2").expect("valid disk name in test fixture"),
            by_id_path: by_id_disk2.clone(),
            state: PresentConfigDiskState::PresentLuks {
                uuid: LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap(),
                label: Some("braid-disk2".to_owned()),
                mapper_open: false,
            },
        }];
        let work_plan = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &[DiskName::parse("disk2").unwrap()],
                by_ids: &[&by_id_disk2],
                probed: &probed,
                pool: &pool,
                mount_point: config.mount_point(),
                paths: &paths,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
            },
        )
        .expect("closed recoverable target should plan");
        let plan = AddPlan {
            notes: vec![],
            work_plan,
            config,
            parsed: vec![(DiskName::parse("disk2").unwrap(), by_id_disk2.clone())],
            names: vec![DiskName::parse("disk2").unwrap()],
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
                    config: &test_config(),
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
                    confirm: &confirm,
                    passphrase_reader: &crate::luks::RealTty,
                    backing_path_resolver:
                        crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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

    // Intent: executing a fresh add into a mounted single-device pool issues
    // exactly one RAID1 balance after the btrfs device add.
    // Why it exists: the live-pool add path must convert data to RAID1 once
    // the pool reaches two devices; otherwise new data can remain unprotected.
    // Scenario: disk1 is already mounted in the pool and disk2 is a fresh
    // LUKS target that gets formatted, opened, added, and balanced.
    #[test]
    fn execute_fresh_add_to_mounted_single_device_pool_balances_once() {
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = RequestRecordingRunner::new(RecoverableAddRunner::new());
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        let target = fresh_target(
            "disk2",
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let target_uuid = target.luks_uuid.clone();
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-disk1".into()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vdb".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: Some(Fsid::parse(POOL_FSID).unwrap()),
            null_underlying: vec![],
        };
        let plan = plan_for_execute_target(
            AddTargetWork::Fresh(target.clone()),
            journal_targets_with(target_uuid, fresh_journal_target(&target)),
            pool,
        );
        let config = test_config();

        let result = plan.execute(
            &runner,
            &fs,
            &AddParams {
                config: &config,
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        );

        assert!(result.is_ok(), "fresh add should succeed: {result:?}");
        let requests = runner.requests();
        let balance_count = requests
            .iter()
            .filter(|request| matches!(request, CmdRequest::BtrfsBalanceRaid1 { .. }))
            .count();
        assert_eq!(
            balance_count, 1,
            "fresh add should issue exactly one RAID1 balance: {requests:?}"
        );
    }

    // Intent: executing an add into a *degraded* mounted pool (a member is
    //   missing) adds the disk but issues NO RAID1 convert balance.
    // Why it exists: the hard convert would rewrite every chunk while the
    //   pool has no redundancy; braid defers restoration to the purpose-built
    //   `remove-missing`/`replace` repair path. This pins the execute-side
    //   `missing_count == 0` gate so a regression that re-enables the degraded
    //   balance fails here.
    // Scenario: a 2-disk RAID1 has lost disk2 (still mounted -o degraded). The
    //   operator runs `braid add disk3` -- modeled here as disk2 the fresh
    //   target -- expecting the disk to join but redundancy to stay deferred.
    #[test]
    fn execute_degraded_add_skips_raid1_balance() {
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = RequestRecordingRunner::new(RecoverableAddRunner::degraded());
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        let target = fresh_target(
            "disk2",
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let target_uuid = target.luks_uuid.clone();
        // Planned pool: one present member (disk1) plus one missing member,
        // so the execute balance gate keys off missing_count > 0. The degraded
        // RecoverableAddRunner makes the fresh probe degraded too.
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-disk1".into()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vdb".into(),
            }],
            missing_count: 1,
            missing_devids: vec![Devid::new(2)],
            total_devices: 2,
            fsid: Some(Fsid::parse(POOL_FSID).unwrap()),
            null_underlying: vec![],
        };
        let plan = plan_for_execute_target(
            AddTargetWork::Fresh(target.clone()),
            journal_targets_with(target_uuid, fresh_journal_target(&target)),
            pool,
        );
        let config = test_config();

        let result = plan.execute(
            &runner,
            &fs,
            &AddParams {
                config: &config,
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        );

        assert!(result.is_ok(), "degraded add should succeed: {result:?}");
        let requests = runner.requests();
        let balance_count = requests
            .iter()
            .filter(|request| matches!(request, CmdRequest::BtrfsBalanceRaid1 { .. }))
            .count();
        assert_eq!(
            balance_count, 0,
            "degraded add must issue NO RAID1 balance: {requests:?}"
        );
        let device_add_count = requests
            .iter()
            .filter(|request| matches!(request, CmdRequest::BtrfsDeviceAdd { .. }))
            .count();
        assert_eq!(
            device_add_count, 1,
            "degraded add must still issue the btrfs device add: {requests:?}"
        );
    }

    // Intent: if a healthy planned add becomes degraded before the post-add
    //   balance gate, execute skips the hard RAID1 convert and tells the
    //   operator why.
    // Why it exists: the gate must use the fresh post-add probe, not the
    //   stale plan-time missing count, or add can rewrite a newly degraded
    //   pool with no redundancy.
    // Scenario: disk1 is healthy at planning and execute-start; after disk2 is
    //   added, the final probe reports disk2 present and disk1 as MISSING.
    #[test]
    fn execute_newly_degraded_add_skips_raid1_balance_and_emits_note() {
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = RequestRecordingRunner::new(RecoverableAddRunner::degrades_after_add());
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        let target = fresh_target(
            "disk2",
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let target_uuid = target.luks_uuid.clone();
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-disk1".into()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vdb".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: Some(Fsid::parse(POOL_FSID).unwrap()),
            null_underlying: vec![],
        };
        let plan = plan_for_execute_target(
            AddTargetWork::Fresh(target.clone()),
            journal_targets_with(target_uuid, fresh_journal_target(&target)),
            pool,
        );
        let config = test_config();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(plan.execute(
                &runner,
                &fs,
                &AddParams {
                    config: &config,
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
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver:
                        crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                },
            ));
        });

        result
            .expect("execute should run")
            .expect("newly degraded add should still succeed");
        let requests = runner.requests();
        let balance_count = requests
            .iter()
            .filter(|request| matches!(request, CmdRequest::BtrfsBalanceRaid1 { .. }))
            .count();
        assert_eq!(
            balance_count, 0,
            "newly degraded add must issue NO RAID1 balance: {requests:?}"
        );
        let skip_body = format_add_degraded_balance_skip();
        assert_eq!(
            captured.matches(skip_body.as_str()).count(),
            1,
            "newly degraded add must emit one balance-skip note; got: {captured:?}"
        );
    }

    // Intent: if a degraded planned add becomes healthy before the post-add
    //   balance gate, execute runs the hard RAID1 convert and suppresses the
    //   stale plan-time balance-skip prediction.
    // Why it exists: real-run output must not say the RAID1 balance was
    //   skipped immediately before execute balances the now-healthy pool.
    // Scenario: disk2 was missing at planning time, then returns before the
    //   final post-add probe; the warning note replays, the skip note does
    //   not, and the live gate balances.
    #[test]
    fn execute_member_returned_add_balances_without_stale_skip_note() {
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = RequestRecordingRunner::new(RecoverableAddRunner::new());
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        let target = fresh_target(
            "disk2",
            "/dev/disk/by-id/virtio-disk2",
            "22222222-2222-2222-2222-222222222222",
        );
        let target_uuid = target.luks_uuid.clone();
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-disk1".into()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vdb".into(),
            }],
            missing_count: 1,
            missing_devids: vec![Devid::new(2)],
            total_devices: 2,
            fsid: Some(Fsid::parse(POOL_FSID).unwrap()),
            null_underlying: vec![],
        };
        let mut plan = plan_for_execute_target(
            AddTargetWork::Fresh(target.clone()),
            journal_targets_with(target_uuid, fresh_journal_target(&target)),
            pool,
        );
        plan.notes
            .push(PreviewNote::Warn(format_add_missing_devices_warning(1)));
        plan.notes
            .push(PreviewNote::Skip(format_add_degraded_balance_skip()));
        let config = test_config();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(plan.execute(
                &runner,
                &fs,
                &AddParams {
                    config: &config,
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
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver:
                        crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                },
            ));
        });

        result
            .expect("execute should run")
            .expect("member-returned add should still succeed");
        let requests = runner.requests();
        let balance_count = requests
            .iter()
            .filter(|request| matches!(request, CmdRequest::BtrfsBalanceRaid1 { .. }))
            .count();
        assert_eq!(
            balance_count, 1,
            "member-returned add should issue exactly one RAID1 balance: {requests:?}"
        );
        let device_add_count = requests
            .iter()
            .filter(|request| matches!(request, CmdRequest::BtrfsDeviceAdd { .. }))
            .count();
        assert_eq!(
            device_add_count, 1,
            "member-returned add must still issue the btrfs device add: {requests:?}"
        );
        let skip_body = format_add_degraded_balance_skip();
        assert!(
            !captured.contains(skip_body.as_str()),
            "member-returned add must suppress stale balance-skip note; got: {captured:?}"
        );
        let warn_body = format_add_missing_devices_warning(1);
        assert!(
            captured.contains(warn_body.as_str()),
            "member-returned add must still replay non-Skip notes; got: {captured:?}"
        );
    }

    fn fresh_add_confirm_plan() -> AddPlan {
        const UUID: &str = "22222222-2222-2222-2222-222222222222";
        const BY_ID: &str = "/dev/disk/by-id/virtio-disk2";
        let target = fresh_target("disk2", BY_ID, UUID);
        let target_uuid = target.luks_uuid.clone();
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-disk1".into()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vdb".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: Some(Fsid::parse(POOL_FSID).unwrap()),
            null_underlying: vec![],
        };
        let mut plan = plan_for_execute_target(
            AddTargetWork::Fresh(target.clone()),
            journal_targets_with(target_uuid, fresh_journal_target(&target)),
            pool,
        );
        plan.work_plan.prelude.confirm_disks = vec![AddConfirmDiskPlan {
            name: disk("disk2"),
            by_id: ByIdPath::parse(BY_ID).unwrap(),
            needs_luks_format: true,
        }];
        plan
    }

    // Intent: a declined add confirmation aborts before irreversible side
    //   effects.
    // Why it exists: the interactive gate must remain before passphrase work,
    //   sleep-inhibitor acquisition, and journal write.
    // Scenario: an operator starts adding a fresh disk to a one-disk mounted
    //   pool and declines at the confirmation prompt.
    #[test]
    fn add_declined_confirm_aborts_before_side_effects() {
        const BY_ID: &str = "/dev/disk/by-id/virtio-disk2";
        let plan = fresh_add_confirm_plan();
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let runner = RequestRecordingRunner::new(RecoverableAddRunner::new());
        let fs = AddMockFs(vec![BY_ID.into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        confirm.decline();
        let config = test_config();

        let err = plan
            .execute(
                &runner,
                &fs,
                &AddParams {
                    config: &config,
                    disk_specs: &[],
                    dry_run: false,
                    yes: false,
                    passphrase_stdin: false,
                    passphrase_file: Some(pass_path.as_path()),
                    enroll_key_file: None,
                    luks_format_extra_opts: &[],
                    progress: ProgressOutput::Off,
                    paths: &paths,
                    sleep_inhibitor: &inhibitor,
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver:
                        crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                },
            )
            .expect_err("declined confirm should abort");

        assert_eq!(err.to_string(), "aborted by user");
        assert_eq!(inhibitor.acquire_count(), 0);
        assert!(journal::load_journal(&paths).unwrap().is_none());
        let requests = runner.requests();
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, CmdRequest::BtrfsDeviceAdd { .. })),
            "declined confirm must not issue BtrfsDeviceAdd: {requests:?}"
        );
    }

    // Intent: accepted add confirmation records the exact assembled prompt,
    //   with the target's hw line resolved from the by-id handle
    //   (/dev/disk/by-id/virtio-disk2) -- the not-yet-present add target's
    //   only valid probe path per decision 024.
    // Why it exists: the confirm seam must receive the formatter output plus
    //   its trailing newline exactly once for the planned fresh target. Until
    //   this used `with_hw_info()`, the prompt was built from
    //   `DiskHwInfo::default()` against a runner that returns `MissingMock`
    //   for `LsblkField`, so `get_lsblk_field`'s `.ok()?` swallow blanked the
    //   hw line no matter which device was queried -- the routing was unpinned.
    //   The populated `DiskHwInfo` now matches only if the probe hit the by-id
    //   path; a regression to any other path leaves the line blank and fails.
    // Scenario: adding a fresh disk to a mounted pool prompts for the disk
    //   that will be LUKS-formatted, showing its model/serial/size.
    #[test]
    fn add_accepted_confirm_records_prompt() {
        const BY_ID: &str = "/dev/disk/by-id/virtio-disk2";
        let plan = fresh_add_confirm_plan();
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let runner = RequestRecordingRunner::new(RecoverableAddRunner::with_hw_info());
        let fs = AddMockFs(vec![BY_ID.into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        confirm.accept();
        let config = test_config();

        plan.execute(
            &runner,
            &fs,
            &AddParams {
                config: &config,
                disk_specs: &[],
                dry_run: false,
                yes: false,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        )
        .expect("accepted confirm should proceed");

        let expected = format!(
            "{}\n",
            format_add_confirm(&[AddConfirmDisk {
                name: "disk2",
                by_id: BY_ID,
                hw: crate::confirm::DiskHwInfo {
                    model: Some(RecoverableAddRunner::HW_MODEL.into()),
                    serial: Some(RecoverableAddRunner::HW_SERIAL.into()),
                    size: Some(RecoverableAddRunner::HW_SIZE),
                },
                needs_luks_format: true,
            }])
        );
        assert_eq!(confirm.prompts(), vec![expected]);
    }

    // Intent: accepted add confirmation does not block mutation.
    // Why it exists: the seam must preserve the happy path, not just the
    //   declined abort path.
    // Scenario: the operator accepts the fresh add prompt and braid reaches
    //   `btrfs device add`.
    #[test]
    fn add_accepted_confirm_proceeds_to_device_add() {
        const BY_ID: &str = "/dev/disk/by-id/virtio-disk2";
        let plan = fresh_add_confirm_plan();
        let (_state_tmp, paths, _tmp, pass_path) = execute_fixture();
        let runner = RequestRecordingRunner::new(RecoverableAddRunner::new());
        let fs = AddMockFs(vec![BY_ID.into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        confirm.accept();
        let config = test_config();

        plan.execute(
            &runner,
            &fs,
            &AddParams {
                config: &config,
                disk_specs: &[],
                dry_run: false,
                yes: false,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        )
        .expect("accepted confirm should proceed");

        let requests = runner.requests();
        assert!(
            requests
                .iter()
                .any(|request| matches!(request, CmdRequest::BtrfsDeviceAdd { .. })),
            "accepted confirm must reach BtrfsDeviceAdd: {requests:?}"
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
        fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
            Ok(())
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
        fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
            Ok(())
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
        /// If true, BtrfsFilesystemShowTarget returns "not a valid btrfs" (NoBtrfs).
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
        m.insert(
            crate::test_fixtures::test_uuid(502),
            membership::DiskMember {
                name: DiskName::parse("disk1").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
                devid: None,
                added_at: None,
            },
        )
        .expect("test fixture insert");
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
        formatted_uuids: Mutex<std::collections::BTreeMap<String, String>>,
        fail_bootstrap_post_mount_probe: bool,
        malformed_bootstrap_post_mount_probe: bool,
        fail_second_add: bool,
        fail_post_add_probe: bool,
        fail_luks_format: bool,
        omit_new_mapper_from_probe: bool,
        degraded: bool,
        vanished_after_later_add: Option<String>,
        added_mapper_drift: Option<String>,
        disk2_devid: u64,
    }

    impl AddFullPathRunner {
        fn live() -> Self {
            Self {
                mounted: Arc::new(AtomicBool::new(true)),
                added: Mutex::new(Vec::new()),
                opened: Mutex::new(vec!["braid-disk1".to_owned()]),
                formatted_uuids: Mutex::new(std::collections::BTreeMap::new()),
                fail_bootstrap_post_mount_probe: false,
                malformed_bootstrap_post_mount_probe: false,
                fail_second_add: false,
                fail_post_add_probe: false,
                fail_luks_format: false,
                omit_new_mapper_from_probe: false,
                degraded: false,
                vanished_after_later_add: None,
                added_mapper_drift: None,
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

        fn with_malformed_bootstrap_post_mount_probe(mut self) -> Self {
            self.malformed_bootstrap_post_mount_probe = true;
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

        fn degraded(mut self) -> Self {
            self.degraded = true;
            self
        }

        fn with_mapper_vanished_after_later_add(mut self, mapper: &str) -> Self {
            self.vanished_after_later_add = Some(mapper.to_owned());
            self
        }

        fn with_added_mapper_drifted(mut self, rename: &str) -> Self {
            self.added_mapper_drift = Some(rename.to_owned());
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

        fn canonical_mapper(&self, mapper: &str) -> String {
            if self
                .added_mapper_drift
                .as_ref()
                .is_some_and(|drifted| drifted == mapper)
            {
                "braid-disk2".to_owned()
            } else {
                mapper.to_owned()
            }
        }

        fn probe_mapper_name(&self, mapper: &str) -> String {
            if mapper == "braid-disk2" {
                self.added_mapper_drift
                    .clone()
                    .unwrap_or_else(|| mapper.to_owned())
            } else {
                mapper.to_owned()
            }
        }

        fn mapper_is_open(&self, mapper: &str) -> bool {
            let canonical = self.canonical_mapper(mapper);
            self.opened
                .lock()
                .unwrap()
                .iter()
                .any(|opened| opened == mapper || opened == &canonical)
        }

        fn mapper_devid(&self, mapper: &str) -> u64 {
            let canonical = self.canonical_mapper(mapper);
            match canonical.as_str() {
                "braid-disk1" => 1,
                "braid-disk2" => self.disk2_devid,
                "braid-disk3" => 3,
                other => panic!("unexpected mapper for devid mapping: {other}"),
            }
        }

        fn mapper_underlying(&self, mapper: &str) -> &'static str {
            let canonical = self.canonical_mapper(mapper);
            match canonical.as_str() {
                "braid-disk1" => "/dev/vdb",
                "braid-disk2" => "/dev/vdc",
                "braid-disk3" => "/dev/vdd",
                other => panic!("unexpected mapper for underlying mapping: {other}"),
            }
        }

        fn formatted_uuid_for_device(&self, device: &str) -> Option<String> {
            let formatted = self.formatted_uuids.lock().unwrap();
            if let Some(uuid) = formatted.get(device) {
                return Some(uuid.clone());
            }
            let by_id = match device {
                "/dev/vdb" => Some("/dev/disk/by-id/virtio-disk1"),
                "/dev/vdc" => Some("/dev/disk/by-id/virtio-disk2"),
                "/dev/vdd" => Some("/dev/disk/by-id/virtio-disk3"),
                _ => None,
            }?;
            formatted.get(by_id).cloned()
        }

        fn luks_uuid_for_device(&self, device: &str) -> Option<String> {
            if let Some(uuid) = self.formatted_uuid_for_device(device) {
                return Some(uuid);
            }
            match device {
                "/dev/vdb" => Some("11111111-1111-1111-1111-111111111111".to_owned()),
                "/dev/vdc" => Some("22222222-2222-2222-2222-222222222222".to_owned()),
                "/dev/vdd" => Some("33333333-3333-3333-3333-333333333333".to_owned()),
                _ => None,
            }
        }

        fn pool_show(&self) -> String {
            let mut mappers = vec!["braid-disk1".to_owned()];
            let added = self.added.lock().unwrap();
            if !self.omit_new_mapper_from_probe {
                mappers.extend(added.iter().cloned());
            }
            if let Some(vanished) = &self.vanished_after_later_add
                && added
                    .last()
                    .map(|mapper| mapper != vanished)
                    .unwrap_or(false)
            {
                mappers.retain(|mapper| mapper != vanished);
            }
            let missing_count = if self.degraded { 1 } else { 0 };
            let mut out = format!(
                "Label: none  uuid: {POOL_FSID}\n\
                 \tTotal devices {} FS bytes used 16.17MiB\n",
                mappers.len() + missing_count
            );
            let present_count = mappers.len();
            for mapper in mappers {
                let probe_mapper = self.probe_mapper_name(&mapper);
                let devid = self.mapper_devid(&probe_mapper);
                out.push_str(&format!(
                    "\tdevid    {devid} size 496.00MiB used 121.56MiB path /dev/mapper/{probe_mapper}\n"
                ));
            }
            if self.degraded {
                let missing_devid = present_count + 1;
                out.push_str(&format!(
                    "\tdevid    {missing_devid} size 0 used 0 path MISSING\n"
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

        fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
            Ok(())
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
                    if self.malformed_bootstrap_post_mount_probe
                        && self.mounted.load(Ordering::SeqCst)
                        && !has_added
                    {
                        return Ok(RawCommandOutput {
                            cmd: format!("btrfs filesystem show {mount_point}"),
                            stdout: "This is not btrfs output at all\nrandom garbage data".into(),
                            stderr: String::new(),
                            exit_status: 0,
                        });
                    }
                    Ok(mock_ok(
                        &format!("btrfs filesystem show {mount_point}"),
                        &self.pool_show(),
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => {
                    if self.mapper_is_open(mapper.as_str()) {
                        Ok(mock_ok(
                            &format!("cryptsetup status {mapper}"),
                            &format!(
                                "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {}\n  mode:    read/write\n",
                                self.mapper_underlying(mapper.as_str())
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
                    if let Some(uuid) = self.luks_uuid_for_device(device) {
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
                CmdRequest::CryptsetupLuksFormat { device, uuid, .. } => {
                    self.formatted_uuids
                        .lock()
                        .unwrap()
                        .insert(device.clone(), uuid.as_str().to_owned());
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
                    self.opened.lock().unwrap().push(mapper.as_str().to_owned());
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
        let confirm = crate::confirm::RecordingConfirm::new();

        cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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

    // Intent: a plan-degraded real `cmd_add` emits the degraded balance-skip
    //   note once, from the plan notes, and never again from execute.
    // Why it exists: the execute-time newly-degraded branch must stay mutually
    //   exclusive with the plan-degraded branch or operators see duplicate
    //   `[skip]` explanations.
    // Scenario: the mounted pool has disk1 present and one missing member at
    //   planning time; disk2 is added successfully, the pool remains degraded,
    //   and the hard RAID1 balance is skipped.
    #[test]
    fn cmd_add_plan_degraded_emits_balance_skip_once() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let base_runner = AddFullPathRunner::live().degraded();
        let fs = base_runner.fs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = RequestRecordingRunner::new(base_runner);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_add(
                &runner,
                &fs,
                &AddParams {
                    config: &read_test_config(&config_path),
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
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver:
                        crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                },
            ));
        });

        result
            .expect("cmd_add should run")
            .expect("plan-degraded add should still succeed");
        let requests = runner.requests();
        let balance_count = requests
            .iter()
            .filter(|request| matches!(request, CmdRequest::BtrfsBalanceRaid1 { .. }))
            .count();
        assert_eq!(
            balance_count, 0,
            "plan-degraded add must issue NO RAID1 balance: {requests:?}"
        );
        let skip_body = format_add_degraded_balance_skip();
        assert_eq!(
            captured.matches(skip_body.as_str()).count(),
            1,
            "plan-degraded add must emit one balance-skip note; got: {captured:?}"
        );
    }

    // Intent: existing-pool add succeeds when the post-add probe reports the
    // new device under a drifted mapper but the journaled LUKS UUID is present
    // in the live pool.
    //
    // Why it exists: post-add membership correlation must be UUID-keyed per
    // decision 024. A reverted-to-mapper-keyed implementation must fail this
    // test even if helper-level unit tests still pass.
    //
    // Scenario: `braid add disk2=...` completes `btrfs device add`; the
    // post-add probe reports disk2 as `braid-WRONG` with the correct LUKS UUID.
    // Add still persists membership, clears the journal, and drops the ghost.
    #[test]
    fn cmd_add_succeeds_when_post_add_mapper_drifted() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        save_test_acked(&paths, &[("2", test_acked_disk(true, 22))]);
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddFullPathRunner::live().with_added_mapper_drifted("braid-WRONG");
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        );

        assert!(
            result.is_ok(),
            "expected add to tolerate mapper drift, got {result:?}"
        );
        assert_eq!(runner.added_mappers(), vec!["braid-disk2"]);
        let membership = membership::load_membership(&paths).expect("membership should persist");
        let disk2_name = DiskName::parse("disk2").unwrap();
        let (_, disk2) = membership
            .by_name(&disk2_name)
            .expect("disk2 membership should be saved");
        assert_eq!(disk2.devid, Some(Devid::new(2)));
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "pending-op.json should be cleared after successful add"
        );
        assert!(
            !alert::load_acked_stats(&paths).0.contains_key("2"),
            "drifted mapper must still resolve the assigned devid for cleanup"
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
        let confirm = crate::confirm::RecordingConfirm::new();

        cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        )
        .expect("bootstrap should succeed even when post-mount enrichment probe fails");

        assert!(
            !paths.acked_stats_json().exists(),
            "bootstrap cleanup must delete every stale acked baseline before enrichment"
        );
    }

    // Intent: bootstrap `cmd_add` warns when its best-effort post-mount
    //   `probe_pool` returns an error, while still succeeding.
    // Why it exists: a committed first-disk bootstrap must not hide that
    //   optional pool.json metadata enrichment was skipped.
    // Scenario: first-disk bootstrap succeeds through LUKS format, open, mkfs,
    //   and mount. The post-mount pool probe returns a command error, so add
    //   saves the unenriched target membership and clears the journal.
    #[test]
    fn cmd_add_bootstrap_warns_when_post_mount_probe_errors() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = fresh_add_setup();
        let runner = AddFullPathRunner::bootstrap().with_bootstrap_post_mount_probe_failure();
        let fs = runner.fs(vec!["/dev/disk/by-id/virtio-disk1".into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_add(
                &runner,
                &fs,
                &AddParams {
                    config: &read_test_config(&config_path),
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
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver:
                        crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                },
            ));
        });

        result
            .expect("cmd_add should run")
            .expect("bootstrap should tolerate post-mount probe errors");
        assert_eq!(
            captured
                .matches("Warning: failed to probe pool for metadata refresh: ")
                .count(),
            1,
            "expected one metadata-refresh warning, got: {captured:?}"
        );
        assert!(
            captured.contains("post-mount probe failed"),
            "warning should include the probe error detail, got: {captured:?}"
        );
        assert!(
            membership::load_membership(&paths).is_ok(),
            "pool.json should still be persisted when enrichment is skipped"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "pending-op.json should be cleared after successful bootstrap"
        );
    }

    // Intent: bootstrap `cmd_add` must tolerate a post-mount enrichment
    //   `probe_pool` that returns `Err(ProbeError::Parse(_))`, persist the
    //   target membership, and clear the journal.
    // Why it exists: bootstrap enrichment after mkfs+mount is best-effort.
    //   A parser drift in `btrfs filesystem show` must not turn a committed
    //   fresh-pool add into a hard failure or leave recovery work pending.
    // Scenario: first-disk bootstrap succeeds through LUKS format, open, mkfs,
    //   and mount. Mountinfo then declares `/mnt/storage` as btrfs, but
    //   `BtrfsFilesystemShow` returns malformed stdout lacking `Total
    //   devices`, so the best-effort post-mount probe yields a parse error.
    #[test]
    fn cmd_add_bootstrap_tolerates_post_mount_probe_err() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = fresh_add_setup();
        let runner = AddFullPathRunner::bootstrap().with_malformed_bootstrap_post_mount_probe();
        let fs = runner.fs(vec!["/dev/disk/by-id/virtio-disk1".into()]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();

        cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        )
        .expect("bootstrap should tolerate post-mount probe parse errors");

        let membership =
            membership::load_membership(&paths).expect("pool.json should be persisted");
        let disk1_name = DiskName::parse("disk1").unwrap();
        let (_uuid, disk1) = membership
            .by_name(&disk1_name)
            .expect("disk1 membership should be saved");
        assert!(
            disk1.devid.is_none(),
            "disk1.devid must remain None when post-mount probe returns Err, got: {:?}",
            disk1.devid
        );
        assert!(
            disk1.added_at.is_none(),
            "disk1.added_at must remain None when post-mount probe returns Err, got: {:?}",
            disk1.added_at
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "pending-op.json should be cleared after successful bootstrap"
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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

    // Intent: a partial multi-add leaves pending-op.json populated with
    //   every originally-requested target so `braid recover` can finish
    //   the work the loop interrupted.
    //
    // Why it exists: ADR-017 makes target_membership a write-once,
    //   before-the-irreversible-loop snapshot; the live-pool add loop in
    //   `AddPlan::execute` never touches the journal mid-loop. A
    //   future refactor that pruned journaled targets to "match recovery
    //   to live state" on partial failure would silently change what
    //   recover replays without breaking any existing test.
    //
    // Scenario: cmd_add disk2,disk3 against a mounted pool seeded with
    //   disk1 by add_test_setup, with disk3's btrfs device add forced to
    //   fail. Assert pending-op.json carries OpKind::Add {
    //   phase: PoolMutation, targets: {disk2, disk3} } with
    //   pre_membership = {disk1}, target_membership = {disk1, disk2, disk3},
    //   and per-target by-id paths intact.
    #[test]
    fn cmd_add_partial_multi_add_journal_carries_all_targets() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let fs = AddMockFs(vec![
            "/dev/disk/by-id/virtio-disk2".into(),
            "/dev/disk/by-id/virtio-disk3".into(),
        ]);
        let runner = AddFullPathRunner::live().with_second_add_failure();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        );

        assert!(result.is_err(), "second device add should fail");
        assert_eq!(runner.added_mappers(), vec!["braid-disk2"]);
        let journal = journal::load_journal(&paths)
            .unwrap()
            .expect("journal must survive partial failure");
        let journal::OpKind::Add { phase, targets } = &journal.op else {
            panic!("expected add journal, got: {:?}", journal.op);
        };
        assert_eq!(phase, &journal::AddPhase::PoolMutation);

        let target_names: std::collections::BTreeSet<_> =
            targets.iter().map(|(_, t)| t.name.as_str()).collect();
        assert_eq!(
            target_names,
            std::collections::BTreeSet::from(["disk2", "disk3"])
        );

        let pre_membership_names: std::collections::BTreeSet<_> = journal
            .pre_membership
            .iter()
            .map(|(_, m)| m.name.as_str())
            .collect();
        assert_eq!(
            pre_membership_names,
            std::collections::BTreeSet::from(["disk1"])
        );

        let target_membership_names: std::collections::BTreeSet<_> = journal
            .target_membership
            .iter()
            .map(|(_, m)| m.name.as_str())
            .collect();
        assert_eq!(
            target_membership_names,
            std::collections::BTreeSet::from(["disk1", "disk2", "disk3"])
        );

        let disk3 = targets
            .iter()
            .find(|(_, t)| t.name.as_str() == "disk3")
            .map(|(_, t)| t)
            .expect("disk3 target");
        assert_eq!(disk3.by_id.as_str(), "/dev/disk/by-id/virtio-disk3");
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
     * `PostAddProbeFailed` variant, both when the probe command itself fails
     * and when the freshly added mapper is absent from the successful probe
     * result. The variant directs the operator to `braid recover` (the
     * journal is still pending and pool.json was never saved), not to acked-
     * stats deletion -- the acked-stats cleanup was never reached.
     *
     * Why it exists: live-add cleanup needs the assigned btrfs devid. If braid
     * cannot prove which devid was assigned, it must fail closed instead of
     * guessing or skipping cleanup. The journal must survive so `braid recover`
     * can replay the still-pending PoolMutation.
     *
     * Scenario: disk2's `btrfs device add` succeeds. In one case the
     * following pool probe fails; in the other it succeeds but omits
     * /dev/mapper/braid-disk2. Both must stop at the post-add probe boundary
     * with the journal intact and pool.json not yet listing disk2.
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
            let confirm = crate::confirm::RecordingConfirm::new();

            let result = cmd_add(
                &runner,
                &fs,
                &AddParams {
                    config: &read_test_config(&config_path),
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
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(
                    ),
                },
            );

            match result {
                Err(AddError::PostAddProbeFailed { .. }) => {}
                other => panic!("{label}: expected PostAddProbeFailed, got {other:?}"),
            }
            assert_eq!(
                runner.added_mappers(),
                vec!["braid-disk2"],
                "{label}: failure must happen after device add commits"
            );
            // The recover-pointing contract: the PoolMutation journal survives
            // the error, so `braid recover` has the still-pending op to replay.
            assert!(
                journal::load_journal(&paths).unwrap().is_some(),
                "{label}: journal must survive so `braid recover` can replay it"
            );
            // pool.json was never persisted (save_membership runs only after the
            // per-target loop completes), so disk2 must not yet be a member.
            assert!(
                membership::load_membership(&paths)
                    .unwrap()
                    .by_name(&DiskName::parse("disk2").unwrap())
                    .is_none(),
                "{label}: pool.json must not list disk2 -- save_membership was never reached"
            );
        }
    }

    // Intent: the live-pool add end-state sweep is fatal when an earlier
    //   successfully-added disk vanishes before the final pool probe.
    // Why it exists: the per-target probe only proves each disk was present
    //   immediately after its own `btrfs device add`; without the final sweep,
    //   a later disappearance could be persisted to pool.json with no devid.
    // Scenario: disk2 and disk3 are added to an existing disk1 pool. Disk2 is
    //   present after its own add, then disappears from the final probe after
    //   disk3 is added. The command must stop with recovery guidance, keep the
    //   journal, and avoid saving either new disk to pool.json.
    #[test]
    fn cmd_add_earlier_disk_vanishing_before_final_probe_is_fatal() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let fs = AddMockFs(vec![
            "/dev/disk/by-id/virtio-disk2".into(),
            "/dev/disk/by-id/virtio-disk3".into(),
        ]);
        let runner = AddFullPathRunner::live().with_mapper_vanished_after_later_add("braid-disk2");
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
            },
        );

        let err = result.expect_err("final probe disappearance should fail closed");
        let rendered = err.to_string();
        match err {
            AddError::PostAddProbeFailed { .. } => {}
            other => panic!("expected PostAddProbeFailed, got {other:?}"),
        }
        assert!(
            rendered.contains("braid recover"),
            "post-add failure must point at recovery, got: {rendered}"
        );
        assert_eq!(
            runner.added_mappers(),
            vec!["braid-disk2", "braid-disk3"],
            "both device adds must commit before the end-state sweep fails"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_some(),
            "journal must survive so `braid recover` can replay it"
        );
        let membership = membership::load_membership(&paths).unwrap();
        assert!(
            membership
                .by_name(&DiskName::parse("disk2").unwrap())
                .is_none(),
            "pool.json must not list disk2 before save_membership"
        );
        assert!(
            membership
                .by_name(&DiskName::parse("disk3").unwrap())
                .is_none(),
            "pool.json must not list disk3 before save_membership"
        );
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
        let confirm = crate::confirm::RecordingConfirm::new();

        cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
        // Identity is the LuksUuidMap key. There is exactly one target
        // in this single-disk fresh add; recover its UUID by iterating.
        assert_eq!(targets.len(), 1);
        let (_uuid, target) = targets.iter().next().expect("one fresh target");
        assert_eq!(target.name.as_str(), "disk2");
        assert_eq!(target.by_id.as_str(), "/dev/disk/by-id/virtio-disk2");
        let journal::AddJournalMode::FreshLuks {
            extra_opts,
            enroll_key_file,
        } = &target.mode
        else {
            panic!("expected fresh LUKS target, got: {:?}", target.mode);
        };
        // The structured `extra_opts` carries user-supplied
        // non-managed tokens unchanged. Managed flags (`--uuid`,
        // `--label`) are journaled at the op level (UUID is the map
        // key; the label is derived from `name` at the format call
        // site), so they MUST NOT appear here.
        assert_eq!(
            extra_opts.as_slice(),
            &[
                "--pbkdf".to_owned(),
                "pbkdf2".to_owned(),
                "--iter-time".to_owned(),
                "1".to_owned(),
            ]
        );
        assert!(enroll_key_file.is_none());
        assert_eq!(inhibitor.acquire_count(), 1);
    }

    #[test]
    // Intent: no journal is written when a PresentLuks disk fails identity
    //   validation (NoBtrfs).
    //
    // Why it exists: the journal was previously written before identity checks,
    //   so a NoBtrfs refusal left a stale pending-op.json that
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        );

        assert!(result.is_err(), "add should fail on NoBtrfs");
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
    //   NoBtrfs before pending-op.json can be stranded.
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let mut result = Ok(());
        crate::status_tag::testing::capture_with_color(false, || {
            result = cmd_add(
                &runner,
                &fs,
                &AddParams {
                    config: &read_test_config(&config_path),
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
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(
                    ),
                },
            );
        });

        let err = result.expect_err("closed identity failure should abort");
        assert!(
            err.to_string().contains("contains no btrfs superblock"),
            "expected NoBtrfs refusal, got: {err}"
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
    // Intent: a disk spec whose name matches an existing pool member but whose
    //   by-id differs is rejected at the fail-fast gate -- before any probe,
    //   passphrase read, inhibitor acquisition, or journal write -- and the
    //   operator message names the colliding name without leaking a synthetic
    //   placeholder UUID.
    //
    // Why it exists: the name/by-id conflict gate previously drove
    //   PoolMembership::insert on a throwaway clone seeded with sentinel UUIDs
    //   (fffffff...), which leaked into operator output and left the
    //   name-collision arm untested. This pins the operator contract so the
    //   simplified direct-lookup gate (and the sentinel removal) cannot
    //   silently regress.
    //
    // Scenario: operator reuses an in-pool logical name (disk1) for a
    //   brand-new physical disk (virtio-disk9) -- a copy-paste slip -- and runs
    //   a non-dry-run `braid add`. The command must refuse with disk1 named and
    //   zero side effects.
    fn add_rejects_name_collision_with_existing_member() {
        let (_state_tmp, paths, _tmp, config_path, _pass_path) = add_test_setup();
        // fs lookups never happen -- the conflict gate fires before any probe.
        let fs = AddMockFs(vec![]);
        // Empty MockRunner: the gate fires before the first runner.run() at
        // probe_pool, so an empty runner never returns MissingMock. Asserting
        // the conflict error text indirectly pins "nothing executed".
        let runner = MockRunner::default();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        // Scripted reader pins "refused before credentials are read": a valid
        // passphrase_file would not, since a file read is silent and a gate
        // moved after credential reading would still error and still pass. The
        // unpopped queue makes "never read" an explicit assertion.
        let tty = ScriptedPassphraseReader::new(["SENTINEL"]);

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
                disk_specs: &["disk1=/dev/disk/by-id/virtio-disk9".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                confirm: &confirm,
                passphrase_reader: &tty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        );

        let err = result
            .expect_err("name collision must be rejected")
            .to_string();
        assert!(
            err.contains("disk1") && err.contains("in use"),
            "error must name the colliding disk name, got: {err}"
        );
        assert!(
            !err.contains("fffffff"),
            "operator output must not leak a synthetic placeholder UUID, got: {err}"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal after pre-probe validation failure"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "validation failure must NOT acquire the sleep inhibitor"
        );
        assert_eq!(
            tty.remaining(),
            1,
            "conflict refused before passphrase read"
        );
    }

    #[test]
    // Intent: a disk spec whose by-id matches an existing pool member but whose
    //   name differs is rejected at the fail-fast gate -- before any probe,
    //   passphrase read, inhibitor acquisition, or journal write -- and the
    //   operator message names the colliding by-id path without leaking a
    //   synthetic placeholder UUID.
    //
    // Why it exists: same gate, by-id arm. Neither conflict arm had add-layer
    //   coverage before; the by-id arm in particular must not be shadowed by
    //   the exact-re-add skip (the `continue` fires only when name AND by-id
    //   both match an existing member).
    //
    // Scenario: operator gives a new logical name (disk9) to a
    //   /dev/disk/by-id path (virtio-disk1) that already belongs to disk1 --
    //   e.g. renaming a disk already in the pool -- and runs a non-dry-run
    //   `braid add`. The command must refuse with the by-id path named and zero
    //   side effects.
    fn add_rejects_by_id_collision_with_existing_member() {
        let (_state_tmp, paths, _tmp, config_path, _pass_path) = add_test_setup();
        // fs lookups never happen -- the conflict gate fires before any probe.
        let fs = AddMockFs(vec![]);
        // Empty MockRunner: the gate fires before the first runner.run() at
        // probe_pool, so an empty runner never returns MissingMock. Asserting
        // the conflict error text indirectly pins "nothing executed".
        let runner = MockRunner::default();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        // Scripted reader pins "refused before credentials are read": a valid
        // passphrase_file would not, since a file read is silent and a gate
        // moved after credential reading would still error and still pass. The
        // unpopped queue makes "never read" an explicit assertion.
        let tty = ScriptedPassphraseReader::new(["SENTINEL"]);

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
                disk_specs: &["disk9=/dev/disk/by-id/virtio-disk1".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                confirm: &confirm,
                passphrase_reader: &tty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        );

        let err = result
            .expect_err("by_id collision must be rejected")
            .to_string();
        assert!(
            err.contains("/dev/disk/by-id/virtio-disk1") && err.contains("in use"),
            "error must name the colliding by-id path, got: {err}"
        );
        assert!(
            !err.contains("fffffff"),
            "operator output must not leak a synthetic placeholder UUID, got: {err}"
        );
        assert!(
            journal::load_journal(&paths).unwrap().is_none(),
            "no journal after pre-probe validation failure"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "validation failure must NOT acquire the sleep inhibitor"
        );
        assert_eq!(
            tty.remaining(),
            1,
            "conflict refused before passphrase read"
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
        let confirm = crate::confirm::RecordingConfirm::new();
        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
        let backup_tmp = paths
            .luks_headers_dir()
            .join("braid-disk1.luksheader.tmp")
            .display()
            .to_string();
        // CryptsetupLuksFormat carries a randomly generated LuksUuid at
        // runtime (Phase 2's t=0 identity), so the runner matches it via
        // a handler keyed on `device` instead of by full-value equality.
        let runner = MockRunner::default()
            .with_output_sequence(
                CmdRequest::CryptsetupLuksUuid {
                    device: by_id.into(),
                },
                vec![
                    mock_not_luks(&format!("cryptsetup luksUUID {by_id}")),
                    mock_luks_uuid(by_id, "11111111-1111-1111-1111-111111111111"),
                ],
            )
            .with_handler({
                let by_id = by_id.to_owned();
                move |req| {
                    if let CmdRequest::CryptsetupLuksFormat { device, .. } = req
                        && *device == by_id
                    {
                        Some(Ok(mock_ok("cryptsetup luksFormat", "")))
                    } else {
                        None
                    }
                }
            })
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: by_id.into(),
                    backup_path: backup_tmp,
                },
                mock_ok("cryptsetup luksHeaderBackup", ""),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        );

        match result {
            Err(AddError::Luks(crate::luks::LuksError::MapperBackingMismatch {
                expected_path,
                found_path,
                ..
            })) => {
                assert_eq!(expected_path, "/dev/vdb");
                assert_eq!(found_path, "/dev/vdz");
            }
            other => panic!("expected MapperBackingMismatch, got {other:?}"),
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
                    mapper: MapperName::from_basename("braid-disk2".into()),
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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

    // Intent: dry-run for fresh single-disk bootstrap shows LUKS init + mkfs + mount commands.
    // Why it exists: verifies header backup and mount commands appear, and
    // pins the single-profile side of the bootstrap mkfs boundary.
    // Scenario: first disk added to an empty pool (no pool mounted yet).
    #[test]
    fn dry_run_render_fresh_single_disk_bootstrap() {
        let runner = MockRunner::default();
        let probed = vec![PresentConfigDisk {
            name: DiskName::parse("disk1").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/disk1").unwrap(),
            state: PresentConfigDiskState::PresentNotLuks,
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
                names: &[DiskName::parse("disk1").unwrap()],
                by_ids: &[&ByIdPath::parse("/dev/disk/by-id/disk1").unwrap()],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::parse(&luks_format_extra_opts)
                    .unwrap(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
            },
        )
        .unwrap()
        .render_steps();
        let output = Step::render_dry_run(&steps);
        assert_eq!(
            steps.len(),
            5,
            "fresh single-disk bootstrap must emit format + backup + open + mkfs + mount; got {:?}",
            steps
        );
        assert_lines_in_order(
            &output,
            &[
                "[destructive]",
                "$ cryptsetup luksFormat",
                "LUKS header backup",
                "$ cryptsetup luksHeaderBackup",
                "LUKS open",
                "$ cryptsetup open --type luks",
                "mkfs.btrfs",
                "$ mkfs.btrfs",
                "mount",
                "$ mount",
            ],
        );

        let format_line = output
            .lines()
            .nth(line_index(&output, "$ cryptsetup luksFormat"))
            .expect("format line present");
        assert!(format_line.contains("--pbkdf pbkdf2 --iter-time 1"));
        assert!(format_line.contains("--label braid-disk1"));

        let mkfs_line = output
            .lines()
            .nth(line_index(&output, "$ mkfs.btrfs"))
            .expect("mkfs line present");
        assert!(mkfs_line.contains("-d single -m dup"));
        assert!(!mkfs_line.contains("raid1"));

        let mount_line = output
            .lines()
            .nth(line_index(&output, "$ mount"))
            .expect("mount line present");
        assert!(mount_line.contains("/mnt/storage"));
    }

    // Intent: under mapper drift (a live member open as braid-WRONG), the add
    //   credential-verify prelude names the existing member through membership
    //   ('disk1'), not the drifted mapper basename.
    // Why it exists: this site used to parse the mapper basename, so a drifted
    //   member surfaced as 'WRONG' in the `passphrase: checking against ...`
    //   line; decision 024 requires the live-UUID->DiskName join here.
    // Scenario: an operator adds disk2 while disk1 is open under a stale
    //   'braid-WRONG' mapper; the verify prelude must still read 'disk1'.
    #[test]
    fn add_credential_prelude_names_drifted_member_via_membership() {
        let drifted_uuid = LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap();
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-WRONG".into()),
                luks_uuid: drifted_uuid.clone(),
                devid: Devid::new(1),
                underlying: "/dev/vda".into(),
            }],
            missing_count: 0,
            total_devices: 1,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        };
        let mut membership = PoolMembership::empty();
        membership
            .insert(
                drifted_uuid,
                DiskMember::new(
                    DiskName::parse("disk1").unwrap(),
                    ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
                ),
            )
            .unwrap();
        // New disk being added is PresentNotLuks, so it contributes no verify
        // target; the only verify target is the live (drifted) member.
        let names = [DiskName::parse("disk2").unwrap()];
        let by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap();
        let by_ids = [&by_id];
        let probed = vec![PresentConfigDisk {
            name: DiskName::parse("disk2").unwrap(),
            by_id_path: by_id.clone(),
            state: PresentConfigDiskState::PresentNotLuks,
        }];
        let mount_point = MountPoint::new("/mnt/storage".into());
        let extra_opts = LuksFormatExtraOpts::default();
        let (_tmp, paths) = test_paths();
        let input = AddStepsInput {
            names: &names,
            by_ids: &by_ids,
            probed: &probed,
            pool: &pool,
            mount_point: &mount_point,
            paths: &paths,
            enroll_key_file: None,
            luks_format_extra_opts: &extra_opts,
            backing_path_resolver: crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            pool_membership: &membership,
        };

        let prelude = build_add_credential_prelude(&input, &[]);

        assert_eq!(
            prelude.verify_targets.len(),
            1,
            "only the live pool member should be a verify target"
        );
        assert_eq!(
            prelude.verify_targets[0].name(),
            "disk1",
            "drifted-mapper member must resolve to 'disk1' via membership, not 'braid-WRONG'"
        );
        assert_eq!(prelude.verify_targets[0].device(), "/dev/vda");
    }

    #[test]
    // Intent: two consecutive dry-run renders of the same fresh-disk `add` are
    //   byte-identical, and the format line shows the fixed
    //   `<generated-at-format-time>` placeholder, not a per-invocation random
    //   UUID.
    // Why it exists: a fresh (PresentNotLuks) target mints a random LuksUuid at
    //   plan time (ADR-024). Before the preview-variant fix that real UUID
    //   flowed into the rendered `--uuid`, so two dry-runs of the identical
    //   command printed different output. A single StatePaths isolates the
    //   minted UUID as the only variable -- the header-backup path flows into
    //   both the step description and `--header-backup-file`, so two tempdirs
    //   would diverge even after the fix. Fails pre-fix (random `--uuid`),
    //   passes post-fix.
    // Scenario: an operator runs `braid add --dry-run` twice against the same
    //   fresh disk and expects identical, honest preview output.
    fn dry_run_render_fresh_disk_uuid_is_reproducible_across_invocations() {
        let runner = MockRunner::default();
        let probed = vec![PresentConfigDisk {
            name: DiskName::parse("disk1").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/disk1").unwrap(),
            state: PresentConfigDiskState::PresentNotLuks,
        }];
        let pool = pool_unmounted();
        let names = [DiskName::parse("disk1").unwrap()];
        let by_id = ByIdPath::parse("/dev/disk/by-id/disk1").unwrap();
        let by_ids = [&by_id];
        let mount_point = MountPoint::new("/mnt/storage".into());
        let extra_opts = LuksFormatExtraOpts::default();
        let membership = PoolMembership::empty();
        // Bind ONE StatePaths so the header-backup path is fixed across both
        // builder calls; the minted LuksUuid is then the only variable.
        let (_tmp, paths) = test_paths();

        let input = AddStepsInput {
            names: &names,
            by_ids: &by_ids,
            probed: &probed,
            pool: &pool,
            mount_point: &mount_point,
            paths: &paths,
            enroll_key_file: None,
            luks_format_extra_opts: &extra_opts,
            backing_path_resolver: crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            pool_membership: &membership,
        };

        // Each call mints a fresh `LuksUuid::new_v4()` internally.
        let first =
            Step::render_dry_run(&build_add_work_plan(&runner, &input).unwrap().render_steps());
        let second =
            Step::render_dry_run(&build_add_work_plan(&runner, &input).unwrap().render_steps());

        assert_eq!(
            first, second,
            "two dry-run renders of the same fresh add must be byte-identical"
        );
        assert!(
            first.contains("<generated-at-format-time>"),
            "fresh-format preview must show the placeholder token, got:\n{first}"
        );
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
        let probed = vec![PresentConfigDisk {
            name: DiskName::parse("disk1").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/disk1").unwrap(),
            state: PresentConfigDiskState::PresentNotLuks,
        }];
        let pool = pool_unmounted();
        let kf = std::path::Path::new("/mnt/usb/braid.key");

        let steps = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &[DiskName::parse("disk1").unwrap()],
                by_ids: &[&ByIdPath::parse("/dev/disk/by-id/disk1").unwrap()],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: Some(kf),
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
            },
        )
        .unwrap()
        .render_steps();
        let output = Step::render_dry_run(&steps);
        assert_lines_in_order(
            &output,
            &[
                "$ cryptsetup luksFormat",
                "$ cryptsetup luksAddKey",
                "$ cryptsetup luksHeaderBackup",
                "$ cryptsetup open --type luks",
            ],
        );
        let addkey = line_index(&output, "$ cryptsetup luksAddKey");
        let backup = line_index(&output, "$ cryptsetup luksHeaderBackup");
        let lines: Vec<&str> = output.lines().collect();
        // Pin BOTH stringly fields with distinct keyfile and header paths
        // so a transposition at the terminal render (keyfile string into
        // HeaderBackup.backup_path, or the reverse) fails here even though
        // the newtypes already guard the function boundary.
        assert!(
            lines[addkey].contains("/mnt/usb/braid.key")
                && !lines[addkey].contains("braid-disk1.luksheader"),
            "luksAddKey line must carry the keyfile, not the header path; got: {}",
            lines[addkey]
        );
        assert!(
            lines[backup].contains("braid-disk1.luksheader")
                && !lines[backup].contains("/mnt/usb/braid.key"),
            "luksHeaderBackup line must carry the header path, not the keyfile; got: {}",
            lines[backup]
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
                names: &[DiskName::parse("disk1").unwrap()],
                by_ids: &[&ByIdPath::parse("/dev/disk/by-id/disk1").unwrap()],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: Some(kf),
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
            },
        )
        .unwrap()
        .render_steps();
        let output = Step::render_dry_run(&steps);
        assert_lines_in_order(
            &output,
            &[
                "$ cryptsetup open --type luks",
                "$ cryptsetup luksAddKey",
                "$ cryptsetup luksHeaderBackup",
                "$ btrfs device add",
            ],
        );
        let addkey = line_index(&output, "$ cryptsetup luksAddKey");
        let backup = line_index(&output, "$ cryptsetup luksHeaderBackup");
        let lines: Vec<&str> = output.lines().collect();
        // Pin both stringly fields with distinct paths so a keyfile/header
        // transposition at the returned-disk render boundary fails here.
        assert!(
            lines[addkey].contains("/mnt/usb/braid.key")
                && !lines[addkey].contains("braid-disk1.luksheader"),
            "addKey command must carry the keyfile, not the header path: {}",
            lines[addkey]
        );
        assert!(
            lines[backup].contains("braid-disk1.luksheader")
                && !lines[backup].contains("/mnt/usb/braid.key"),
            "headerBackup command must carry the header path, not the keyfile: {}",
            lines[backup]
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
                names: &[DiskName::parse("disk1").unwrap()],
                by_ids: &[&ByIdPath::parse("/dev/disk/by-id/disk1").unwrap()],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: Some(kf),
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
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

    // Intent: dry-run for two fresh bootstrap disks renders RAID1 mkfs, not a
    // post-mkfs balance.
    // Why it exists: pins the two-disk side of the bootstrap mkfs boundary so
    // two fresh devices are not accidentally created as a single-profile pool.
    // Scenario: first pool creation receives disk1 and disk2 together while
    // no pool is mounted yet.
    #[test]
    fn dry_run_render_fresh_two_disk_bootstrap_uses_raid1_mkfs() {
        let runner = MockRunner::default();
        let by_id_disk1 = ByIdPath::parse("/dev/disk/by-id/disk1").unwrap();
        let by_id_disk2 = ByIdPath::parse("/dev/disk/by-id/disk2").unwrap();
        let names = vec![
            DiskName::parse("disk1").unwrap(),
            DiskName::parse("disk2").unwrap(),
        ];
        let by_ids = vec![&by_id_disk1, &by_id_disk2];
        let probed = vec![
            PresentConfigDisk {
                name: names[0].clone(),
                by_id_path: by_id_disk1.clone(),
                state: PresentConfigDiskState::PresentNotLuks,
            },
            PresentConfigDisk {
                name: names[1].clone(),
                by_id_path: by_id_disk2.clone(),
                state: PresentConfigDiskState::PresentNotLuks,
            },
        ];
        let pool = pool_unmounted();

        let steps = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &names,
                by_ids: &by_ids,
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
            },
        )
        .unwrap()
        .render_steps();
        let output = Step::render_dry_run(&steps);

        assert!(
            output.contains("mkfs.btrfs RAID1"),
            "missing RAID1 mkfs step description: {output}"
        );
        assert!(
            output.contains("$ mkfs.btrfs -d raid1 -m raid1"),
            "missing RAID1 mkfs command: {output}"
        );
        assert!(
            !output.contains("btrfs balance to RAID1"),
            "bootstrap RAID1 mkfs must not render a balance step: {output}"
        );
    }

    #[test]
    // Intent: dry-run for adding to existing pool shows device add + balance commands.
    // Why: verifies the pool-mounted path includes balance to RAID1.
    // Scenario: adding a fresh disk to a 1-disk pool (pool already mounted).
    fn dry_run_render_add_to_existing_pool_with_balance() {
        let runner = MockRunner::default();
        let probed = vec![PresentConfigDisk {
            name: DiskName::parse("disk2").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/disk2").unwrap(),
            state: PresentConfigDiskState::PresentNotLuks,
        }];
        let pool = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");

        let steps = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &[DiskName::parse("disk2").unwrap()],
                by_ids: &[&ByIdPath::parse("/dev/disk/by-id/disk2").unwrap()],
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
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
        disk1_present_luks_member: bool,
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
                disk1_present_luks_member: false,
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
        fn with_disk1_present_luks_member(mut self) -> Self {
            self.disk1_present_luks_member = true;
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
                    if self
                        .opened
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|m| m == mapper.as_str())
                    {
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
                CmdRequest::CryptsetupLuksUuid { device }
                    if self.disk1_present_luks_member
                        && device == "/dev/disk/by-id/virtio-disk1" =>
                {
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
                CmdRequest::CryptsetupLuksDumpText { device }
                    if self.disk1_present_luks_member
                        && device == "/dev/disk/by-id/virtio-disk1" =>
                {
                    Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksDump {device}"),
                        stdout: "LUKS header information\n\
                                 Version:       \t2\n\
                                 Label:         \tbraid-disk1\n\
                                 Subsystem:     \t(no subsystem)\n"
                            .into(),
                        stderr: String::new(),
                        exit_status: 0,
                    })
                }
                CmdRequest::BtrfsFilesystemShowTarget { target }
                    if self.disk1_present_luks_member && target == "/dev/mapper/braid-disk1" =>
                {
                    Ok(btrfs_show_with_uuid(LIVE_POOL_FSID))
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

        fn create_dir_all(&self, path: &str) -> Result<(), std::io::Error> {
            panic!("planner-boundary test: fs.create_dir_all must not be called; got: {path}");
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let failure = match plan_add(
            &PanicRunner,
            &PanicFilesystem,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &crate::luks::RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let failure = match plan_add(
            &PanicRunner,
            &PanicFilesystem,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &crate::luks::RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
        let confirm = crate::confirm::RecordingConfirm::new();
        let tty = ScriptedPassphraseReader::new(["typo-one", "typo-two"]);

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &tty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
        let confirm = crate::confirm::RecordingConfirm::new();
        let tty = ScriptedPassphraseReader::new(["ok", "ok"]);

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &tty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
        let confirm = crate::confirm::RecordingConfirm::new();
        let tty = ScriptedPassphraseReader::new(["ok", "ok"]);

        let err = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &tty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
        let confirm = crate::confirm::RecordingConfirm::new();
        let tty = ScriptedPassphraseReader::new(["ok", "ok"]);
        let kf_dir = tempfile::tempdir().unwrap();
        let kf_path = kf_dir.path().join("braid.key");
        std::fs::write(&kf_path, [0u8; crate::luks::KEYFILE_SIZE]).unwrap();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &tty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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

        // Pin BOTH stringly fields with distinct keyfile and header paths so
        // a transposition at the execute-path render (keyfile bytes into the
        // header-backup destination, or the reverse) fails here.
        let CmdRequest::CryptsetupLuksAddKeyFile { key_file_path, .. } = &log[addkey] else {
            unreachable!("indexed the luksAddKey request above");
        };
        let CmdRequest::CryptsetupLuksHeaderBackup { backup_path, .. } = &log[backup] else {
            unreachable!("indexed the luksHeaderBackup request above");
        };
        assert_eq!(
            key_file_path,
            &kf_path.display().to_string(),
            "luksAddKey must enroll the operator keyfile, not the header path"
        );
        assert!(
            backup_path.contains("braid-disk1.luksheader")
                && backup_path != &kf_path.display().to_string(),
            "luksHeaderBackup must target the header path, not the keyfile; got: {backup_path}"
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
        let confirm = crate::confirm::RecordingConfirm::new();
        let tty = ScriptedPassphraseReader::new(["SENTINEL"]);

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &tty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
        let confirm = crate::confirm::RecordingConfirm::new();
        let tty = ScriptedPassphraseReader::new(["pw", "SENTINEL"]);

        let _ = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &tty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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

    /*
     * Intent: when the operator passes a disk already in the pool alongside
     *   a fresh disk, `braid add` verifies the in-pool disk only through the
     *   live pool-member credential target.
     *
     * Why it exists: each verify target costs a full Argon2 round and emits
     *   an operator-visible wait/ok row. Re-verifying the same LUKS UUID via
     *   the candidate side wastes time and renders a duplicate progress row.
     *
     * Scenario: pool has disk1 already open as a live member; operator runs
     *   `braid add disk1=... disk2=...`, where disk2 is fresh. The command
     *   reaches the credential prelude, then later aborts at the forced
     *   header-backup failure for disk2, leaving the request log behind.
     */
    #[test]
    fn cmd_add_mixed_already_in_pool_and_fresh_verifies_each_disk_once() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let fs = AddMockFs(vec![
            "/dev/disk/by-id/virtio-disk1".into(),
            "/dev/disk/by-id/virtio-disk2".into(),
        ]);
        let runner = AddRecordingRunner::new(true).with_disk1_present_luks_member();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        );

        assert!(
            result.is_err(),
            "cmd_add must abort at forced header-backup failure"
        );

        let log = runner.log();
        let verify_devices: Vec<&str> = log
            .iter()
            .filter_map(|request| match request {
                CmdRequest::CryptsetupTestPassphrase { device } => Some(device.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            verify_devices,
            vec!["/dev/vdb"],
            "must verify the live pool member once, got log: {log:?}"
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

    #[derive(Clone, Copy)]
    enum AddPlanTargetProbe {
        ClosedSlot1Empty,
        ClosedSlot1Occupied,
        ClosedDumpFails,
        OpenRecoverableSlot1Empty,
        AlreadyInPoolSlot1Empty,
    }

    struct AddPlanTestRunner {
        missing_count: u64,
        keyfile_probes: Vec<AddPlanKeyfileProbe>,
        target_probes: HashMap<String, AddPlanTargetProbe>,
    }

    impl AddPlanTestRunner {
        fn new() -> Self {
            Self {
                missing_count: 0,
                keyfile_probes: vec![AddPlanKeyfileProbe::Empty],
                target_probes: HashMap::new(),
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

        fn with_target_probe(mut self, by_id: &str, probe: AddPlanTargetProbe) -> Self {
            self.target_probes.insert(by_id.to_owned(), probe);
            self
        }

        fn pool_underlying(index: usize) -> String {
            format!("/dev/vd{}", (b'b' + index as u8) as char)
        }

        fn pool_uuid(index: usize) -> String {
            format!("11111111-1111-1111-1111-11111111111{index}")
        }

        fn disk_index_from_by_id(by_id: &str) -> Option<usize> {
            by_id
                .strip_prefix("/dev/disk/by-id/virtio-disk")?
                .parse::<usize>()
                .ok()?
                .checked_sub(1)
        }

        fn disk_index_from_mapper(mapper: &str) -> Option<usize> {
            mapper
                .strip_prefix("braid-disk")?
                .parse::<usize>()
                .ok()?
                .checked_sub(1)
        }

        fn by_id_for_index(index: usize) -> String {
            format!("/dev/disk/by-id/virtio-disk{}", index + 1)
        }

        fn target_probe_by_index(&self, index: usize) -> Option<AddPlanTargetProbe> {
            let by_id = Self::by_id_for_index(index);
            self.target_probes.get(&by_id).copied()
        }

        fn target_probe_for_device(&self, device: &str) -> Option<(usize, AddPlanTargetProbe)> {
            self.target_probes.iter().find_map(|(by_id, probe)| {
                let index = Self::disk_index_from_by_id(by_id)?;
                (device == by_id || device == Self::pool_underlying(index))
                    .then_some((index, *probe))
            })
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
                    let Some(suffix) = mapper.as_str().strip_prefix("braid-disk") else {
                        return Err(CmdError::MissingMock);
                    };
                    let index = suffix
                        .parse::<usize>()
                        .map_err(|_| CmdError::MissingMock)?
                        .checked_sub(1)
                        .ok_or(CmdError::MissingMock)?;
                    if index >= self.keyfile_probes.len() {
                        if let Some(probe) = self.target_probe_by_index(index) {
                            match probe {
                                AddPlanTargetProbe::ClosedSlot1Empty
                                | AddPlanTargetProbe::ClosedSlot1Occupied
                                | AddPlanTargetProbe::ClosedDumpFails => {
                                    return Ok(mock_status_inactive(mapper.as_str()));
                                }
                                AddPlanTargetProbe::OpenRecoverableSlot1Empty
                                | AddPlanTargetProbe::AlreadyInPoolSlot1Empty => {}
                            }
                        } else {
                            return Err(CmdError::MissingMock);
                        }
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
                                let underlying = Self::pool_underlying(index);
                                let by_id = Self::by_id_for_index(index);
                                (device == &underlying || device == &by_id).then_some(index)
                            })
                    {
                        Ok(mock_ok(
                            &format!("cryptsetup luksUUID {device}"),
                            &format!("{}\n", Self::pool_uuid(index)),
                        ))
                    } else if let Some((index, probe)) = self.target_probe_for_device(device) {
                        let uuid = match probe {
                            AddPlanTargetProbe::ClosedSlot1Empty
                            | AddPlanTargetProbe::ClosedSlot1Occupied
                            | AddPlanTargetProbe::ClosedDumpFails
                            | AddPlanTargetProbe::OpenRecoverableSlot1Empty => {
                                "aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa".to_owned()
                            }
                            AddPlanTargetProbe::AlreadyInPoolSlot1Empty => Self::pool_uuid(index),
                        };
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
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                )),
                CmdRequest::CryptsetupLuksDumpText { device } => {
                    let Some(index) = Self::disk_index_from_by_id(device) else {
                        return Err(CmdError::MissingMock);
                    };
                    if self.target_probe_by_index(index).is_none() {
                        return Err(CmdError::MissingMock);
                    }
                    Ok(mock_ok(
                        &format!("cryptsetup luksDump {device}"),
                        &format!(
                            "LUKS header information\nVersion:\t2\nLabel:\tbraid-disk{}\n",
                            index + 1
                        ),
                    ))
                }
                CmdRequest::BtrfsFilesystemShowTarget { target } => {
                    let mapper = target
                        .strip_prefix("/dev/mapper/")
                        .ok_or(CmdError::MissingMock)?;
                    let Some(index) = Self::disk_index_from_mapper(mapper) else {
                        return Err(CmdError::MissingMock);
                    };
                    match self.target_probe_by_index(index) {
                        Some(
                            AddPlanTargetProbe::OpenRecoverableSlot1Empty
                            | AddPlanTargetProbe::AlreadyInPoolSlot1Empty,
                        ) => Ok(mock_ok(
                            &format!("btrfs filesystem show {target}"),
                            &format!(
                                "Label: none  uuid: {POOL_FSID}\n\tTotal devices 1 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path {target}\n"
                            ),
                        )),
                        Some(_) | None => Err(CmdError::MissingMock),
                    }
                }
                CmdRequest::CryptsetupLuksDump { .. } => {
                    let CmdRequest::CryptsetupLuksDump { device } = request else {
                        unreachable!();
                    };
                    if let Some((_, probe)) = self.target_probe_for_device(device) {
                        return match probe {
                            AddPlanTargetProbe::ClosedSlot1Empty
                            | AddPlanTargetProbe::OpenRecoverableSlot1Empty
                            | AddPlanTargetProbe::AlreadyInPoolSlot1Empty => Ok(mock_ok(
                                "cryptsetup luksDump --dump-json-metadata",
                                r#"{"keyslots":{"0":{"type":"luks2"}}}"#,
                            )),
                            AddPlanTargetProbe::ClosedSlot1Occupied => Ok(mock_ok(
                                "cryptsetup luksDump --dump-json-metadata",
                                r#"{"keyslots":{"0":{"type":"luks2"},"1":{"type":"luks2"}}}"#,
                            )),
                            AddPlanTargetProbe::ClosedDumpFails => Ok(RawCommandOutput {
                                cmd: format!("cryptsetup luksDump --dump-json-metadata {device}"),
                                stdout: String::new(),
                                stderr: format!("forced luksDump failure on target {device}"),
                                exit_status: 5,
                            }),
                        };
                    }
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
    /// config, paths, inhibitor, and confirm lifetimes so each test stays small.
    struct PlanAddFixture {
        _state_tmp: tempfile::TempDir,
        paths: StatePaths,
        _tmp: tempfile::TempDir,
        config: Config,
        pass_path: std::path::PathBuf,
        inhibitor: crate::inhibit::RecordingInhibitor,
        confirm: crate::confirm::RecordingConfirm,
    }

    fn plan_add_fixture() -> PlanAddFixture {
        let (state_tmp, paths, tmp, config_path, pass_path) = add_test_setup();
        let config = read_test_config(&config_path);
        PlanAddFixture {
            _state_tmp: state_tmp,
            paths,
            _tmp: tmp,
            config,
            pass_path,
            inhibitor: crate::inhibit::RecordingInhibitor::new(),
            confirm: crate::confirm::RecordingConfirm::new(),
        }
    }

    impl PlanAddFixture {
        fn params<'a>(&'a self, disk_specs: &'a [String], dry_run: bool) -> AddParams<'a> {
            self.params_with_config(disk_specs, dry_run, &self.config)
        }

        fn params_with_config<'a>(
            &'a self,
            disk_specs: &'a [String],
            dry_run: bool,
            config: &'a Config,
        ) -> AddParams<'a> {
            AddParams {
                config,
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
                confirm: &self.confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
     * insertion order to preserve today's eprintln! sequence
     * (missing-devices warning first, keyfile-asymmetry warning second).
     * Swapping them would change the stderr order a user sees.
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

    // Intent: returning closed LUKS add targets with empty slot 1 emit the
    // keyfile-asymmetry warning when the existing pool has keyfile enrollment.
    // Why it exists: fresh-format gating missed returning disks that predate
    // the pool's keyfile rollout.
    // Scenario: disk2 is a closed braid-labeled LUKS disk with slot 0 only,
    // and the operator re-adds it without --enroll.
    #[test]
    fn plan_add_keyfile_asymmetry_emits_warn_for_returning_disk_with_empty_slot_1() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new().with_keyfile().with_target_probe(
            "/dev/disk/by-id/virtio-disk2",
            AddPlanTargetProbe::ClosedSlot1Empty,
        );

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let plan = plan_add(&runner, &fs, &fixture.params(&disk_specs, true))
            .expect("plan_add should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(warns.len(), 1, "expected one Warn note, got {warns:?}");
        assert_eq!(warns[0], &format_keyfile_asymmetry_warning());
    }

    // Intent: returning closed LUKS add targets with occupied slot 1 do not
    // emit the keyfile-asymmetry warning.
    // Why it exists: the returning-disk check must distinguish already
    // enrolled disks from asymmetric disks.
    // Scenario: disk2 already carries slot 1, so re-adding it without
    // --enroll does not create keyfile asymmetry.
    #[test]
    fn plan_add_keyfile_no_warn_for_returning_disk_with_occupied_slot_1() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new().with_keyfile().with_target_probe(
            "/dev/disk/by-id/virtio-disk2",
            AddPlanTargetProbe::ClosedSlot1Occupied,
        );

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let plan = plan_add(&runner, &fs, &fixture.params(&disk_specs, true))
            .expect("plan_add should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert!(warns.is_empty(), "expected no Warn notes, got {warns:?}");
    }

    // Intent: returning LUKS target slot-probe failures surface as
    // target-specific PreviewNote::Warn diagnostics.
    // Why it exists: target-side uncertainty should not be confused with
    // pool-member enrollment uncertainty.
    // Scenario: disk2 is returning LUKS, but its JSON luksDump fails while
    // previewing add without --enroll.
    #[test]
    fn plan_add_keyfile_emits_target_probe_failure_for_returning_disk_dump_error() {
        let fixture = plan_add_fixture();
        let target = ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap();
        let fs = AddMockFs(vec![target.as_str().to_owned()]);
        let runner = AddPlanTestRunner::new()
            .with_keyfile()
            .with_target_probe(target.as_str(), AddPlanTargetProbe::ClosedDumpFails);

        let disk_specs = [format!("disk2={target}")];
        let plan = plan_add(&runner, &fs, &fixture.params(&disk_specs, true))
            .expect("plan_add should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        let err = crate::luks::LuksError::Validation(format!(
            "cryptsetup luksDump failed (exit 5): forced luksDump failure on target {target}"
        ));
        assert_eq!(warns.len(), 1, "expected one Warn note, got {warns:?}");
        assert_eq!(
            warns[0],
            &format_target_keyfile_probe_failure(&target, &err)
        );
    }

    // Intent: pool-side keyfile probe failures still surface when a returning
    // target lacks slot 1 and the pool enrollment state is uncertain.
    // Why it exists: moving the add warning gate after work-plan construction
    // must not make the existing pool-side uncertainty channel unreachable.
    // Scenario: disk2 has slot 1 empty, and the only pool member's luksDump
    // fails while previewing add without --enroll.
    #[test]
    fn plan_add_keyfile_pool_probe_failure_for_returning_disk_with_empty_slot_1() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new()
            .with_keyfile_probe_failure()
            .with_target_probe(
                "/dev/disk/by-id/virtio-disk2",
                AddPlanTargetProbe::ClosedSlot1Empty,
            );

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let plan = plan_add(&runner, &fs, &fixture.params(&disk_specs, true))
            .expect("plan_add should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        let failure = crate::luks::KeyfileEnrollmentProbeFailure {
            device: "/dev/vdb".to_owned(),
            error:
                "cryptsetup luksDump failed (exit 5): forced luksDump failure on existing disk 1"
                    .to_owned(),
        };
        assert_eq!(warns.len(), 1, "expected one Warn note, got {warns:?}");
        assert_eq!(warns[0], &format_keyfile_enrollment_probe_failure(&failure));
    }

    // Intent: already-in-pool add targets do not emit keyfile-asymmetry
    // warnings even when that disk has empty slot 1.
    // Why it exists: the warning decision must derive from work_plan.targets,
    // after SameBacking no-op targets have been filtered out.
    // Scenario: disk2 is already a live pool member, the pool proves keyfile
    // enrollment via disk1, and the operator runs a no-op `braid add disk2`.
    #[test]
    fn plan_add_keyfile_no_warn_when_target_already_in_pool_with_empty_slot_1() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new()
            .with_keyfile_probes(vec![
                AddPlanKeyfileProbe::Occupied,
                AddPlanKeyfileProbe::Empty,
            ])
            .with_target_probe(
                "/dev/disk/by-id/virtio-disk2",
                AddPlanTargetProbe::AlreadyInPoolSlot1Empty,
            );

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let plan = plan_add(&runner, &fs, &fixture.params(&disk_specs, true))
            .expect("plan_add should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        let infos: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Info(b) => Some(b),
                _ => None,
            })
            .collect();
        assert!(warns.is_empty(), "expected no Warn notes, got {warns:?}");
        assert_eq!(infos.len(), 1, "expected one Info note, got {infos:?}");
        assert_eq!(
            infos[0],
            &format_add_noop(&[DiskName::parse("disk2").unwrap()])
        );
    }

    // Intent: open recoverable returning add targets with empty slot 1 emit
    // the keyfile-asymmetry warning.
    // Why it exists: the warning gate must cover both ClosedPresentLuks and
    // OpenRecoverable work-plan variants.
    // Scenario: disk2's mapper is already open, belongs to the same btrfs
    // FSID, is not a live pool member, and lacks keyfile slot 1.
    #[test]
    fn plan_add_keyfile_asymmetry_emits_warn_for_open_returning_disk_with_empty_slot_1() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new().with_keyfile().with_target_probe(
            "/dev/disk/by-id/virtio-disk2",
            AddPlanTargetProbe::OpenRecoverableSlot1Empty,
        );

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let plan = plan_add(&runner, &fs, &fixture.params(&disk_specs, true))
            .expect("plan_add should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(warns.len(), 1, "expected one Warn note, got {warns:?}");
        assert_eq!(warns[0], &format_keyfile_asymmetry_warning());
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
        let single = vec![DiskName::parse("disk2").unwrap()];
        let multi = vec![
            DiskName::parse("disk1").unwrap(),
            DiskName::parse("disk2").unwrap(),
        ];

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
        let confirm = crate::confirm::RecordingConfirm::new();

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let report = plan_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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

    // Intent: a mixed add lists only the fresh disk in the confirmation plan.
    //
    // Why it exists: guards the `confirm_disks`-from-targets invariant
    //   against regressing back to over-listing already-in-pool disks.
    //
    // Scenario: the operator re-passes an in-pool disk alongside a new one,
    //   and the confirmation must not imply the in-pool disk is being re-added.
    #[test]
    fn plan_add_mixed_already_in_pool_and_fresh_confirms_only_fresh_disk() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let fs = AddMockFs(vec![
            "/dev/disk/by-id/virtio-disk1".into(),
            "/dev/disk/by-id/virtio-disk2".into(),
        ]);
        let runner = AddRecordingRunner::new(true).with_disk1_present_luks_member();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        let disk_specs = [
            "disk1=/dev/disk/by-id/virtio-disk1".to_string(),
            "disk2=/dev/disk/by-id/virtio-disk2".to_string(),
        ];

        let plan = plan_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        )
        .expect("plan_add should succeed for mixed already-in-pool + fresh add");

        let confirm_names: Vec<&str> = plan
            .work_plan
            .prelude
            .confirm_disks
            .iter()
            .map(|d| d.name.as_str())
            .collect();
        assert_eq!(
            confirm_names,
            vec!["disk2"],
            "confirmation must list only the fresh disk, not the already-in-pool disk1"
        );
        assert_eq!(
            plan.work_plan.targets.len(),
            1,
            "only the fresh disk is real work"
        );
        assert!(
            plan.work_plan.prelude.confirm_disks[0].needs_luks_format,
            "the fresh disk must be flagged for LUKS format"
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
        let confirm = crate::confirm::RecordingConfirm::new();

        let disk_specs = ["disk1=/dev/disk/by-id/virtio-disk1".to_string()];
        let report = plan_add(
            &runner,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
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
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
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
     * returns the `btrfs device add` step (the RAID1 balance is skipped
     * here because the pool is degraded).
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

    /* Intent: add_preview_phase encodes the full preview-side balance
     * decision table used by dry-run steps and the degraded-add skip note.
     * Why it exists: the two preview readers must keep sharing one
     * typed prediction instead of reintroducing separate mounted/count/
     * missing-count math.
     * Scenario: exercise bootstrap, live whole, live degraded, and the
     * defensive live lower-bound branch.
     */
    #[test]
    fn add_preview_phase_decision_table() {
        let whole = pool_mounted_with_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee");
        assert_eq!(
            add_preview_phase(&pool_unmounted(), 1),
            AddPreviewPhase::Bootstrap
        );
        assert_eq!(
            add_preview_phase(&whole, 1),
            AddPreviewPhase::LiveAdd(PreviewedBalance::Run)
        );

        let mut degraded = whole.clone();
        degraded.missing_count = 1;
        degraded.missing_devids = vec![Devid::new(2)];
        degraded.total_devices = 2;
        assert_eq!(
            add_preview_phase(&degraded, 1),
            AddPreviewPhase::LiveAdd(PreviewedBalance::SkipDegraded)
        );
        assert_eq!(
            add_preview_phase(&whole, 0),
            AddPreviewPhase::LiveAdd(PreviewedBalance::NotApplicable)
        );
    }

    /* Intent: on a degraded pool (a member missing), the dry-run preview
     * adds the disk but OMITS the `btrfs balance to RAID1` step and
     * surfaces exactly one `[skip]` note explaining redundancy is deferred.
     * Why it exists: the degraded add must not run the hard RAID1 convert
     * (it would rewrite all chunks with no redundancy); restoration is
     * deferred to `remove-missing`/`replace`. This pins both the preview
     * step gate and the single-emit `PreviewNote::Skip`, guarding against
     * a regression that re-adds the balance step or double-emits the note.
     * Scenario: a 1-present-device pool with one MISSING placeholder; the
     * operator adds a fresh disk2.
     */
    #[test]
    fn plan_add_degraded_preview_omits_balance_step() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new().with_missing(1);

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let report = plan_add(&runner, &fs, &fixture.params(&disk_specs, true));
        let plan = report.expect("plan_add should succeed on a degraded pool");

        let rendered = plan.preview().render();
        assert!(
            rendered.contains("btrfs device add"),
            "device-add step must still appear; got:\n{rendered}"
        );
        assert!(
            !rendered.contains("btrfs balance to RAID1"),
            "degraded preview must omit the RAID1 balance step; got:\n{rendered}"
        );
        let skip_body = format_add_degraded_balance_skip();
        assert_eq!(
            rendered.matches(skip_body.as_str()).count(),
            1,
            "degraded preview must surface the balance-skip note exactly once; got:\n{rendered}"
        );
    }

    // Intent: a degraded no-op `braid add` surfaces the pool-health
    // missing-devices warning and the no-op Info line, but omits the
    // work-only balance-skip note.
    // Why it exists: guards the deliberate "health warning fires on no-op,
    // work notes do not" split. A regression that `is_noop`-gates the
    // missing-devices warning, or a note-ordering refactor that drops it,
    // must consciously update this test rather than silently going quiet
    // about a degraded pool.
    // Scenario: disk2 is already a live pool member, the pool has one missing
    // member, and the operator runs a no-op `braid add disk2`.
    #[test]
    fn plan_add_degraded_noop_keeps_missing_warning() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let runner = AddPlanTestRunner::new()
            .with_keyfile_probes(vec![
                AddPlanKeyfileProbe::Occupied,
                AddPlanKeyfileProbe::Empty,
            ])
            .with_missing(1)
            .with_target_probe(
                "/dev/disk/by-id/virtio-disk2",
                AddPlanTargetProbe::AlreadyInPoolSlot1Empty,
            );

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let plan = plan_add(&runner, &fs, &fixture.params(&disk_specs, true))
            .expect("plan_add should succeed on a degraded no-op re-add");

        assert_eq!(
            plan.pool.missing_count, 1,
            "test must exercise a degraded pool"
        );

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        let infos: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Info(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(warns.len(), 1, "expected one Warn note, got {warns:?}");
        assert_eq!(warns[0], &format_add_missing_devices_warning(1));
        assert_eq!(infos.len(), 1, "expected one Info note, got {infos:?}");
        assert_eq!(
            infos[0],
            &format_add_noop(&[DiskName::parse("disk2").unwrap()])
        );

        let preview = plan.preview();
        assert!(
            preview.steps.is_empty(),
            "no-op must have zero steps, got: {:?}",
            preview.steps
        );

        let rendered = preview.render();
        assert!(
            rendered.contains("[warn] pool has 1 missing device"),
            "degraded no-op must render the pool-health warning; got:\n{rendered}"
        );
        assert!(
            rendered.contains("Nothing to do -- disk2 already in pool."),
            "degraded no-op must render the no-op Info line; got:\n{rendered}"
        );
        assert!(
            !rendered.contains(&format_add_degraded_balance_skip()),
            "degraded no-op must omit the work-only balance-skip note; got:\n{rendered}"
        );
        assert!(
            !rendered.contains("nothing to do."),
            "generic `nothing to do.` fallback must NOT appear alongside the Info note"
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
     * `PerDiskStyle::Bracketed`.
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
        let rendered = preview::render_notes_for_stderr(&notes, PerDiskStyle::Bracketed);
        let expected = concat!(
            "Nothing to do -- disk2 already in pool.\n",
            "[warn] pool has 1 missing device. Consider repairing with",
            " `braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>` first.",
            " Use `braid status` to see the missing disk's name.\n",
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
     * add work-plan rendering (e.g. NoBtrfs identity), the
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
            Ok(_) => panic!("plan_add must fail on NoBtrfs identity"),
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
        fn create_dir_all(&self, path: &str) -> Result<(), std::io::Error> {
            self.inner.create_dir_all(path)
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

        // Build a config with a UPS named "ups".
        let config_json = serde_json::json!({
            "mount_point": "/mnt/storage",
            "ups": { "name": "ups" },
        });
        let config = serde_json::from_value(config_json).expect("valid UPS test config");

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let params = fixture.params_with_config(&disk_specs, true, &config);
        let failure = match plan_add(&runner, &fs, &params) {
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

    // Intent: plan_add rejects an absent requested disk with the exact
    //   validation text and requested disk identity.
    // Why it exists: the absent-disk rejection moved from the work-plan
    //   builder to the probe boundary and must keep the prior message.
    // Scenario: a mounted pool exists, but the new disk's by-id path is
    //   not present in the filesystem probe.
    #[test]
    fn plan_add_rejects_absent_new_disk_with_exact_message() {
        let fixture = plan_add_fixture();
        let fs = AddMockFs(vec![]);
        let runner = AddPlanTestRunner::new();

        let disk_specs = ["disk2=/dev/disk/by-id/virtio-disk2".to_string()];
        let failure = match plan_add(&runner, &fs, &fixture.params(&disk_specs, true)) {
            Ok(_) => panic!("expected absent new disk to fail planning"),
            Err(failure) => failure,
        };

        match &failure.error {
            AddError::Validation(body) => {
                assert_eq!(
                    body,
                    "disk 'disk2' (/dev/disk/by-id/virtio-disk2) is not present. Is it plugged in?"
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert!(
            failure.notes.is_empty(),
            "absent add rejection must preserve the empty-notes contract: {:?}",
            failure.notes,
        );
    }

    // -----------------------------------------------------------------------
    // Phase 3b: LUKS UUID identity migration test pins
    //
    // Test-module seed allocation: cli/src/add.rs uses 500-599. See
    // cli/src/test_fixtures/shared.rs::test_uuid for the cross-module
    // seed-allocation table (membership 100-199, luks 200,
    // journal 201-299, cmd 300-399, remove 400-449,
    // remove_missing 450-499, add 500-599).
    // -----------------------------------------------------------------------

    /// Build a recording runner that resolves `cryptsetup luksDump`
    /// label probes for one or more by-id paths to a fixed
    /// `braid-<name>` label, so PresentLuks adoption planning resolves
    /// for tests that drive multiple cloned-disk targets.
    fn cloned_disk_runner(devices: &[(&'static str, &'static str)]) -> MockRunner {
        let devices: Vec<(String, String)> = devices
            .iter()
            .map(|(by_id, uuid)| ((*by_id).to_owned(), (*uuid).to_owned()))
            .collect();
        MockRunner::default().with_handler(move |req| match req {
            CmdRequest::CryptsetupLuksUuid { device } => devices
                .iter()
                .find(|(by_id, _)| by_id == device)
                .map(|(_, uuid)| Ok(mock_ok("cryptsetup luksUUID", &format!("{uuid}\n")))),
            CmdRequest::BtrfsFilesystemShowTarget { .. } => Some(Ok(btrfs_show_with_uuid(
                "cc86845b-aec3-408e-bef5-553affc1f2b1",
            ))),
            _ => None,
        })
    }

    /// Build a `PresentConfigDisk` matching an already-open LUKS disk
    /// under the given by-id path and probed LUKS UUID. The
    /// label is `braid-<name>` so the precondition gate accepts it.
    fn cloned_disk_probed(name: &str, by_id: &str, uuid: &str) -> PresentConfigDisk {
        cloned_disk_probed_with_mapper_state(name, by_id, uuid, true)
    }

    fn cloned_disk_probed_with_mapper_state(
        name: &str,
        by_id: &str,
        uuid: &str,
        mapper_open: bool,
    ) -> PresentConfigDisk {
        PresentConfigDisk {
            name: DiskName::parse(name).expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse(by_id).unwrap(),
            state: PresentConfigDiskState::PresentLuks {
                uuid: LuksUuid::parse(uuid).unwrap(),
                label: Some(luks_label_for(&disk(name)).as_str().to_owned()),
                mapper_open,
            },
        }
    }

    /* Intent: two PresentLuks adoption targets in a single `braid add`
     * invocation that point at distinct by-ids and distinct disk names
     * but probe to the same LUKS UUID (the dd-cloned-disk case) fail
     * planning with `AddError::DuplicateUuid` before any journal write,
     * before any `LuksUuidMap::insert`, and before any
     * `CryptsetupLuksFormat` or `BtrfsDeviceAdd`.
     *
     * Why it exists: discover already closes this case with
     * `DiscoverError::DuplicateUuid`; falling through to a generic
     * `LuksUuidMapConflict` or `MembershipError::Conflict` would not
     * name both by-id paths and would not flag the cloned-disk
     * diagnosis. The structured error is the pre-write refusal; the
     * map/membership insert paths remain the defense-in-depth backstop.
     *
     * Scenario: operator clones disk2 to disk3 with `dd`, then runs
     * `braid add disk3=... disk4=...` against an empty membership.
     * Both targets probe to the same LUKS UUID. The shared-UUID
     * refusal fires inside `build_add_work_plan` BEFORE the second
     * target's journal-targets insertion.
     */
    #[test]
    fn add_cloned_disk_duplicate_uuid_refusal() {
        use crate::cmd::CmdRequest;
        let runner = cloned_disk_runner(&[
            (
                "/dev/disk/by-id/usb-CLONE-AAAA",
                "55555555-5555-5555-5555-555555555555",
            ),
            (
                "/dev/disk/by-id/usb-CLONE-BBBB",
                "55555555-5555-5555-5555-555555555555",
            ),
        ]);
        let by_id_a = ByIdPath::parse("/dev/disk/by-id/usb-CLONE-AAAA").unwrap();
        let by_id_b = ByIdPath::parse("/dev/disk/by-id/usb-CLONE-BBBB").unwrap();
        let probed = vec![
            cloned_disk_probed(
                "diska",
                "/dev/disk/by-id/usb-CLONE-AAAA",
                "55555555-5555-5555-5555-555555555555",
            ),
            cloned_disk_probed(
                "diskb",
                "/dev/disk/by-id/usb-CLONE-BBBB",
                "55555555-5555-5555-5555-555555555555",
            ),
        ];
        let pool = pool_mounted_with_fsid("cc86845b-aec3-408e-bef5-553affc1f2b1");
        let names = vec![
            DiskName::parse("diska").unwrap(),
            DiskName::parse("diskb").unwrap(),
        ];
        let by_ids_refs: Vec<&ByIdPath> = vec![&by_id_a, &by_id_b];

        let recording = RequestRecordingRunner::new(runner);
        let result = build_add_work_plan(
            &recording,
            &AddStepsInput {
                names: &names,
                by_ids: &by_ids_refs,
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
            },
        );
        match result {
            Err(AddError::DuplicateUuid {
                uuid,
                name1,
                by_id1,
                name2,
                by_id2,
            }) => {
                assert_eq!(uuid.as_str(), "55555555-5555-5555-5555-555555555555");
                // Sorted lexicographically by by_id (matches discover's
                // label_collision ordering).
                assert_eq!(by_id1.as_str(), "/dev/disk/by-id/usb-CLONE-AAAA");
                assert_eq!(by_id2.as_str(), "/dev/disk/by-id/usb-CLONE-BBBB");
                assert_eq!(name1.as_str(), "diska");
                assert_eq!(name2.as_str(), "diskb");
                // The Display body is the pinned operator-facing string.
                let body = AddError::DuplicateUuid {
                    uuid,
                    name1,
                    by_id1,
                    name2,
                    by_id2,
                }
                .to_string();
                assert!(
                    body.contains(
                        "duplicate LUKS UUID: braid-diska (/dev/disk/by-id/usb-CLONE-AAAA) and braid-diskb (/dev/disk/by-id/usb-CLONE-BBBB) share UUID 55555555-5555-5555-5555-555555555555"
                    ),
                    "Display must match the pinned wording: {body}"
                );
                assert!(
                    body.contains("detach the cloned or unintended disk before retrying"),
                    "missing detach remediation clause: {body}"
                );
            }
            other => panic!("expected AddError::DuplicateUuid, got: {other:?}"),
        }
        // Defense-in-depth: NO CryptsetupLuksFormat or BtrfsDeviceAdd
        // reached the recording runner. The pre-write refusal fires
        // inside the planner BEFORE the insertion into journal
        // targets / membership, and BEFORE any executor step.
        let requests = recording.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksFormat { .. } | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "no CryptsetupLuksFormat or BtrfsDeviceAdd may run on refusal path: {requests:?}"
        );
    }

    // Intent: an open PresentLuks target with the same UUID and same
    // canonical backing as a live pool row is a no-op even when mapper names
    // drift.
    // Why it exists: mapper-name equality must not decide live-pool identity;
    // UUID plus backing-path proof is the safe already-in-pool check.
    // Scenario: the live pool reports mapper braid-drifted, while the
    // candidate disk is named clone and resolves to the same kernel path.
    #[test]
    fn add_open_present_luks_same_uuid_same_backing_drift_noops() {
        let collision_uuid = "55555555-5555-5555-5555-555555555555";
        const BY_ID: &str = "/dev/disk/by-id/usb-CLONE";
        let uuid = LuksUuid::parse(collision_uuid).unwrap();
        let by_id = ByIdPath::parse(BY_ID).unwrap();
        let mut pool =
            pool_with_live_devices(vec![live_pool_device("braid-drifted", &uuid, "/dev/vdb")]);
        pool.fsid = Some(Fsid::parse("cc86845b-aec3-408e-bef5-553affc1f2b1").unwrap());
        let runner = cloned_disk_runner(&[(BY_ID, collision_uuid)]);
        let recording = RequestRecordingRunner::new(runner);
        let probed = vec![cloned_disk_probed("clone", by_id.as_str(), collision_uuid)];
        let names = vec![DiskName::parse("clone").unwrap()];
        let by_ids_refs: Vec<&ByIdPath> = vec![&by_id];
        let resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path(by_id.as_str(), "/dev/vdb");

        let work_plan = build_add_work_plan(
            &recording,
            &AddStepsInput {
                names: &names,
                by_ids: &by_ids_refs,
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver: &resolver,
                pool_membership: &PoolMembership::empty(),
            },
        )
        .expect("same UUID and same backing should be a no-op");

        assert!(work_plan.is_noop(), "expected no-op plan: {work_plan:?}");
        assert!(
            work_plan.initial_journal_targets.iter().next().is_none(),
            "already-in-pool no-op must not journal a target"
        );
        let requests = recording.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksFormat { .. } | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "no format or device-add may run on no-op planning path: {requests:?}"
        );
    }

    // Intent: an open PresentLuks target with a live UUID match but different
    // backing is refused as a duplicate UUID.
    // Why it exists: replacing the old mapper-name check with UUID-only
    // membership would allow cloned LUKS headers to look like no-ops.
    // Scenario: the candidate by-id resolves to /dev/vdb, but the live pool
    // row with the same UUID resolves to /dev/vdc.
    #[test]
    fn add_open_present_luks_same_uuid_different_backing_rejects_clone() {
        let collision_uuid = "55555555-5555-5555-5555-555555555555";
        const BY_ID: &str = "/dev/disk/by-id/usb-CLONE";
        let uuid = LuksUuid::parse(collision_uuid).unwrap();
        let by_id = ByIdPath::parse(BY_ID).unwrap();
        let mut pool =
            pool_with_live_devices(vec![live_pool_device("braid-foreign", &uuid, "/dev/vdc")]);
        pool.fsid = Some(Fsid::parse("cc86845b-aec3-408e-bef5-553affc1f2b1").unwrap());
        let runner = cloned_disk_runner(&[(BY_ID, collision_uuid)]);
        let recording = RequestRecordingRunner::new(runner);
        let probed = vec![cloned_disk_probed("clone", by_id.as_str(), collision_uuid)];
        let names = vec![DiskName::parse("clone").unwrap()];
        let by_ids_refs: Vec<&ByIdPath> = vec![&by_id];
        let resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path(by_id.as_str(), "/dev/vdb");

        let result = build_add_work_plan(
            &recording,
            &AddStepsInput {
                names: &names,
                by_ids: &by_ids_refs,
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver: &resolver,
                pool_membership: &PoolMembership::empty(),
            },
        );

        match result {
            Err(AddError::DuplicateUuidLivePool { uuid, by_id, .. }) => {
                assert_eq!(uuid.as_str(), collision_uuid);
                assert_eq!(
                    by_id.as_str(),
                    BY_ID,
                    "live-pool refusal must name the candidate add target's by-id"
                );
            }
            other => panic!("expected DuplicateUuidLivePool for open clone, got: {other:?}"),
        }
    }

    // Intent: a closed PresentLuks target with the same UUID and same backing
    // as a live pool row is a no-op before UUID uniqueness assertions run.
    // Why it exists: closed returned-disk planning must tolerate benign
    // mapper drift instead of rejecting the UUID as a live-pool collision.
    // Scenario: the candidate mapper is closed, but its by-id resolves to the
    // same backing path as a live row with the same LUKS UUID.
    #[test]
    fn add_closed_present_luks_same_uuid_same_backing_drift_noops() {
        let collision_uuid = "55555555-5555-5555-5555-555555555555";
        let uuid = LuksUuid::parse(collision_uuid).unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/usb-CLONE").unwrap();
        let mut pool =
            pool_with_live_devices(vec![live_pool_device("braid-drifted", &uuid, "/dev/vdb")]);
        pool.fsid = Some(Fsid::parse("cc86845b-aec3-408e-bef5-553affc1f2b1").unwrap());
        let runner = RequestRecordingRunner::new(MockRunner::default());
        let probed = vec![cloned_disk_probed_with_mapper_state(
            "clone",
            by_id.as_str(),
            collision_uuid,
            false,
        )];
        let names = vec![DiskName::parse("clone").unwrap()];
        let by_ids_refs: Vec<&ByIdPath> = vec![&by_id];
        let resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path(by_id.as_str(), "/dev/vdb");

        let work_plan = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &names,
                by_ids: &by_ids_refs,
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver: &resolver,
                pool_membership: &PoolMembership::empty(),
            },
        )
        .expect("same UUID and same backing should be a no-op");

        assert!(work_plan.is_noop(), "expected no-op plan: {work_plan:?}");
        assert!(
            work_plan.initial_journal_targets.iter().next().is_none(),
            "closed already-in-pool no-op must not journal a target"
        );
        assert!(
            runner.requests().is_empty(),
            "closed no-op classification should not need command probes: {:?}",
            runner.requests()
        );
    }

    // Intent: mixed no-op and fresh-disk planning derives confirm and done
    // target names from disks that survived classification.
    // Why it exists: `braid add disk1=... disk2=...` must not ask the
    // operator to confirm or later report adding a SameBacking no-op disk.
    // Scenario: clone is a closed PresentLuks disk with the same UUID and
    // backing as a live pool member, while fresh is a new PresentNotLuks disk.
    #[test]
    fn add_mixed_noop_and_fresh_excludes_noop_from_workplan() {
        let collision_uuid = "55555555-5555-5555-5555-555555555555";
        let uuid = LuksUuid::parse(collision_uuid).unwrap();
        let clone_by_id = ByIdPath::parse("/dev/disk/by-id/usb-CLONE").unwrap();
        let fresh_by_id = ByIdPath::parse("/dev/disk/by-id/usb-FRESH").unwrap();
        let mut pool =
            pool_with_live_devices(vec![live_pool_device("braid-drifted", &uuid, "/dev/vdb")]);
        pool.fsid = Some(Fsid::parse("cc86845b-aec3-408e-bef5-553affc1f2b1").unwrap());
        let runner = RequestRecordingRunner::new(MockRunner::default());
        let probed = vec![
            cloned_disk_probed_with_mapper_state(
                "clone",
                clone_by_id.as_str(),
                collision_uuid,
                false,
            ),
            PresentConfigDisk {
                name: DiskName::parse("fresh").unwrap(),
                by_id_path: fresh_by_id.clone(),
                state: PresentConfigDiskState::PresentNotLuks,
            },
        ];
        let names = vec![
            DiskName::parse("clone").unwrap(),
            DiskName::parse("fresh").unwrap(),
        ];
        let by_ids_refs: Vec<&ByIdPath> = vec![&clone_by_id, &fresh_by_id];
        let resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path(clone_by_id.as_str(), "/dev/vdb");

        let work_plan = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &names,
                by_ids: &by_ids_refs,
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver: &resolver,
                pool_membership: &PoolMembership::empty(),
            },
        )
        .expect("fresh target should keep mixed add from being a no-op");

        assert!(!work_plan.is_noop(), "expected mixed add to do work");
        assert_eq!(work_plan.targets.len(), 1);
        let confirm_disks = &work_plan.prelude.confirm_disks;
        assert_eq!(
            confirm_disks.len(),
            1,
            "confirm prompt should include only surviving targets: {confirm_disks:?}"
        );
        assert_eq!(confirm_disks[0].name.as_str(), "fresh");
        assert!(confirm_disks[0].needs_luks_format);
        assert!(
            confirm_disks
                .iter()
                .all(|disk| disk.name.as_str() != "clone"),
            "SameBacking no-op must be absent from confirm disks: {confirm_disks:?}"
        );
        assert_eq!(
            work_plan.target_names(),
            vec![DiskName::parse("fresh").unwrap()]
        );
        assert!(
            runner.requests().is_empty(),
            "closed no-op plus fresh planning should not need command probes: {:?}",
            runner.requests()
        );
    }

    // Intent: a closed PresentLuks target with a live UUID match but different
    // backing is refused as a duplicate UUID.
    // Why it exists: the closed-mapper path must fail closed for cloned LUKS
    // headers instead of skipping the target as already in pool.
    // Scenario: the candidate mapper is closed and resolves to /dev/vdb, while
    // a live pool row with the same UUID resolves to /dev/vdc.
    #[test]
    fn add_closed_present_luks_same_uuid_different_backing_rejects_clone() {
        let collision_uuid = "55555555-5555-5555-5555-555555555555";
        let uuid = LuksUuid::parse(collision_uuid).unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/usb-CLONE").unwrap();
        let mut pool =
            pool_with_live_devices(vec![live_pool_device("braid-foreign", &uuid, "/dev/vdc")]);
        pool.fsid = Some(Fsid::parse("cc86845b-aec3-408e-bef5-553affc1f2b1").unwrap());
        let runner = RequestRecordingRunner::new(MockRunner::default());
        let probed = vec![cloned_disk_probed_with_mapper_state(
            "clone",
            by_id.as_str(),
            collision_uuid,
            false,
        )];
        let names = vec![DiskName::parse("clone").unwrap()];
        let by_ids_refs: Vec<&ByIdPath> = vec![&by_id];
        let resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path(by_id.as_str(), "/dev/vdb");

        let result = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &names,
                by_ids: &by_ids_refs,
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver: &resolver,
                pool_membership: &PoolMembership::empty(),
            },
        );

        match result {
            Err(AddError::DuplicateUuidLivePool { uuid, by_id, .. }) => {
                assert_eq!(uuid.as_str(), collision_uuid);
                assert_eq!(
                    by_id.as_str(),
                    "/dev/disk/by-id/usb-CLONE",
                    "live-pool refusal must name the candidate add target's by-id"
                );
            }
            other => panic!("expected DuplicateUuidLivePool for closed clone, got: {other:?}"),
        }
        assert!(
            runner.requests().is_empty(),
            "closed clone classification should not need command probes: {:?}",
            runner.requests()
        );
    }

    /* Intent: planning-time progress messages and dry-run preview
     * iterate `OpKind::Add.targets` sorted by `DiskName`, NOT by the
     * (UUID-lex) key order of the underlying `LuksUuidMap`. With three
     * fresh-format targets whose generated UUIDs are randomized, the
     * preview step iteration must still come out alphabetical by name.
     *
     * Why it exists: UUID-keyed iteration is effectively random per
     * invocation (each fresh disk gets a fresh v4); without the sort
     * helper the operator-visible target order would reorder on every
     * fresh `braid add`. The TUI/status/doctor/preflight fixtures
     * already pin this ordering for other surfaces; this is the
     * symmetric pin for the add path.
     *
     * Scenario: three fresh PresentNotLuks disks named a-disk, m-disk,
     * z-disk. Build the preview and assert the rendered step block
     * names them in that order regardless of the UUID-lex order of
     * their `LuksUuid::new_v4()` keys.
     */
    #[test]
    fn add_preview_iteration_sorts_by_disk_name() {
        let runner = MockRunner::default();
        let probed = vec![
            PresentConfigDisk {
                name: DiskName::parse("a-disk").expect("valid disk name in test fixture"),
                by_id_path: ByIdPath::parse("/dev/disk/by-id/usb-A").unwrap(),
                state: PresentConfigDiskState::PresentNotLuks,
            },
            PresentConfigDisk {
                name: DiskName::parse("m-disk").expect("valid disk name in test fixture"),
                by_id_path: ByIdPath::parse("/dev/disk/by-id/usb-M").unwrap(),
                state: PresentConfigDiskState::PresentNotLuks,
            },
            PresentConfigDisk {
                name: DiskName::parse("z-disk").expect("valid disk name in test fixture"),
                by_id_path: ByIdPath::parse("/dev/disk/by-id/usb-Z").unwrap(),
                state: PresentConfigDiskState::PresentNotLuks,
            },
        ];
        let pool = pool_unmounted();
        let names = vec![
            DiskName::parse("a-disk").unwrap(),
            DiskName::parse("m-disk").unwrap(),
            DiskName::parse("z-disk").unwrap(),
        ];
        let by_a = ByIdPath::parse("/dev/disk/by-id/usb-A").unwrap();
        let by_m = ByIdPath::parse("/dev/disk/by-id/usb-M").unwrap();
        let by_z = ByIdPath::parse("/dev/disk/by-id/usb-Z").unwrap();
        let by_ids_refs: Vec<&ByIdPath> = vec![&by_a, &by_m, &by_z];

        let work_plan = build_add_work_plan(
            &runner,
            &AddStepsInput {
                names: &names,
                by_ids: &by_ids_refs,
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
            },
        )
        .expect("planning should succeed for three fresh disks");
        let steps = work_plan.render_steps();
        let rendered = Step::render_dry_run(&steps);

        // Each target produces a "LUKS format <by_id>" step. The
        // ordering must be alphabetical by DiskName -- a-disk, m-disk,
        // z-disk -- regardless of the UUID-lex order of the
        // freshly-generated UUIDs that key the underlying
        // `LuksUuidMap`.
        let a_pos = rendered
            .find("LUKS format /dev/disk/by-id/usb-A")
            .expect("a-disk LUKS format step must appear");
        let m_pos = rendered
            .find("LUKS format /dev/disk/by-id/usb-M")
            .expect("m-disk LUKS format step must appear");
        let z_pos = rendered
            .find("LUKS format /dev/disk/by-id/usb-Z")
            .expect("z-disk LUKS format step must appear");
        assert!(
            a_pos < m_pos && m_pos < z_pos,
            "preview iteration must be DiskName-sorted (a < m < z), got positions a={a_pos} m={m_pos} z={z_pos}\nrendered:\n{rendered}"
        );
    }

    /* Intent: `--luks-format-arg` carrying a braid-managed identity
     * or storage-model-breaking cryptsetup flag is rejected BEFORE any
     * probing, journal write, inhibitor acquisition, or
     * `CryptsetupLuksFormat` request. The refusal surfaces as
     * `AddError::ManagedFormatFlag(_)`.
     *
     * Why it exists: braid owns the LUKS identity, passphrase path,
     * keyslot layout, header placement, LUKS type, and modeled
     * integrity mode. User-supplied overrides would bypass those
     * invariants; refusing at the parse boundary keeps them
     * load-bearing.
     *
     * Scenario: operator types `braid add disk1=... --luks-format-arg
     * --header=/tmp/header`. The planner returns the structured rejection
     * with the offending token named, having executed zero shell
     * commands.
     */
    #[test]
    fn add_rejects_managed_luks_format_args() {
        for token in [
            "--uuid=DEADBEEF-DEAD-BEEF-DEAD-BEEFDEADBEEF",
            "--uuid",
            "--label=foo",
            "--label",
            "--header",
            "--header=/tmp/x",
            "--type=luks1",
            "--key-file=/dev/null",
            "--key-slot=2",
            "--integrity=hmac-sha256",
            "--keyfile-offset=64",
            "--keyfile-size=16",
            "-M",
            "-qMluks1",
        ] {
            let (_state_tmp, paths, _tmp, config_path, pass_path) = fresh_add_setup();
            let recording = RequestRecordingRunner::new(MockRunner::default());
            let fs = AddOfflineMockFs(vec!["/dev/disk/by-id/virtio-disk1".into()]);
            let inhibitor = crate::inhibit::RecordingInhibitor::new();
            let confirm = crate::confirm::RecordingConfirm::new();
            let extras = vec![token.to_owned()];

            let result = cmd_add(
                &recording,
                &fs,
                &AddParams {
                    config: &read_test_config(&config_path),
                    disk_specs: &["disk1=/dev/disk/by-id/virtio-disk1".into()],
                    dry_run: false,
                    yes: true,
                    passphrase_stdin: false,
                    passphrase_file: Some(pass_path.as_path()),
                    enroll_key_file: None,
                    luks_format_extra_opts: &extras,
                    progress: ProgressOutput::Off,
                    paths: &paths,
                    sleep_inhibitor: &inhibitor,
                    confirm: &confirm,
                    passphrase_reader: &RealTty,
                    backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(
                    ),
                },
            );
            match result {
                Err(AddError::ManagedFormatFlag(
                    crate::types::LuksFormatExtraOptsError::ManagedFormatFlag { token: t },
                )) => {
                    assert_eq!(t, token, "token must be the offending input verbatim");
                }
                other => panic!("expected ManagedFormatFlag refusal for {token:?}, got: {other:?}"),
            }
            // Pre-parse rejection: nothing reached the runner, no
            // journal, no inhibitor.
            assert!(
                recording.requests().is_empty(),
                "managed-flag refusal must not invoke the runner for {token:?}: {:?}",
                recording.requests()
            );
            assert!(
                journal::load_journal(&paths).unwrap().is_none(),
                "managed-flag refusal must not write a journal for {token:?}"
            );
            assert_eq!(
                inhibitor.acquire_count(),
                0,
                "managed-flag refusal must not acquire the sleep inhibitor for {token:?}"
            );
        }
    }

    /* Intent: a successful fresh add issues `CryptsetupLuksFormat`
     * with structured `uuid`, `label = "braid-<name>"`, and
     * user-supplied extras (`--use-random` here) unchanged in
     * `extra_opts`. Managed tokens MUST NOT appear inside
     * `extra_opts`.
     *
     * Why it exists: the structured-format-fields contract is
     * load-bearing for the t=0 journaled identity. A regression that
     * dropped or shadowed the structured fields (e.g. with a stray
     * `--uuid` token inside extras) would either fail this test or
     * fail the rejection suite.
     *
     * Scenario: bootstrap `braid add disk1=...
     * --luks-format-arg=--use-random`. The journal is forced to
     * survive by failing `luksFormat`, and the recorded
     * `CryptsetupLuksFormat` request is inspected.
     */
    #[test]
    fn add_fresh_records_structured_luks_format_request() {
        let (_state_tmp, paths, _tmp, config_path, pass_path) = add_test_setup();
        let runner = AddFullPathRunner::live().with_luks_format_failure();
        let fs = runner.fs(vec!["/dev/disk/by-id/virtio-disk2".into()]);
        let recording = RequestRecordingRunner::new(runner);
        let extras = vec!["--use-random".to_owned()];
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();

        let result = cmd_add(
            &recording,
            &fs,
            &AddParams {
                config: &read_test_config(&config_path),
                disk_specs: &["disk2=/dev/disk/by-id/virtio-disk2".into()],
                dry_run: false,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: Some(pass_path.as_path()),
                enroll_key_file: None,
                luks_format_extra_opts: &extras,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                confirm: &confirm,
                passphrase_reader: &RealTty,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        );
        assert!(
            result.is_err(),
            "forced luksFormat failure should abort add"
        );

        // Inspect the recorded CryptsetupLuksFormat: structured
        // uuid + label + extras.
        let requests = recording.requests();
        let format = requests
            .iter()
            .find_map(|r| match r {
                CmdRequest::CryptsetupLuksFormat {
                    device,
                    uuid,
                    label,
                    extra_opts,
                } if device == "/dev/disk/by-id/virtio-disk2" => {
                    Some((uuid.clone(), label.clone(), extra_opts.clone()))
                }
                _ => None,
            })
            .expect("CryptsetupLuksFormat must reach the runner");
        let (uuid, label, extra_opts) = format;
        // The label is derived from the DiskName at the call site.
        assert_eq!(label.as_str(), "braid-disk2");
        // UUID is a generated v4 -- non-nil, canonical hyphenated form.
        assert_ne!(uuid.as_str(), "00000000-0000-0000-0000-000000000000");
        assert_eq!(uuid.as_str().len(), 36);
        // The user-supplied non-managed token reaches the structured
        // extras slice in argv order, unchanged. No managed token
        // leaked into `extra_opts`.
        assert_eq!(extra_opts.as_slice(), &["--use-random".to_owned()]);
        // The journal records the same uuid and the structured extras.
        let journal = journal::load_journal(&paths)
            .unwrap()
            .expect("journal must survive forced luksFormat failure");
        let journal::OpKind::Add { targets, .. } = journal.op else {
            panic!("expected Add op kind");
        };
        let (journaled_uuid, target) = targets.iter().next().expect("one target journaled");
        assert_eq!(journaled_uuid.as_str(), uuid.as_str());
        let journal::AddJournalMode::FreshLuks {
            extra_opts: journaled_extras,
            ..
        } = &target.mode
        else {
            panic!("expected FreshLuks mode");
        };
        assert_eq!(journaled_extras.as_slice(), &["--use-random".to_owned()]);
    }

    /* Intent: pre-journal-write UUID uniqueness assert refuses a
     * generated/probed target UUID that collides with an existing
     * `PoolMembership` UUID key BEFORE any journal write or
     * `CryptsetupLuksFormat` request.
     *
     * Why it exists: defense-in-depth backstops live at
     * `LuksUuidMap::insert` and `PoolMembership::insert`. The
     * pre-write gate exists so the operator-facing message names both
     * by-id pairs (the in-flight target + the colliding existing
     * member) rather than falling through to a generic
     * conflict error.
     *
     * Scenario: a returning braid-labeled disk probes to the same UUID
     * as an existing pool member (e.g. the operator cloned a current
     * pool member's disk and is trying to add the clone). Planning
     * refuses with `AddError::DuplicateUuid`.
     */
    #[test]
    fn add_pre_write_uniqueness_assert_membership_collision() {
        let collision_uuid = "33333333-3333-3333-3333-333333333333";
        let mut membership = PoolMembership::empty();
        membership
            .insert(
                LuksUuid::parse(collision_uuid).unwrap(),
                DiskMember {
                    name: DiskName::parse("existing").unwrap(),
                    by_id: ByIdPath::parse("/dev/disk/by-id/ata-EXISTING").unwrap(),
                    devid: Some(Devid::new(1)),
                    added_at: None,
                },
            )
            .expect("seed membership");
        let runner = cloned_disk_runner(&[("/dev/disk/by-id/usb-CLONE", collision_uuid)]);
        let recording = RequestRecordingRunner::new(runner);
        let probed = vec![cloned_disk_probed(
            "clone",
            "/dev/disk/by-id/usb-CLONE",
            collision_uuid,
        )];
        let pool = pool_mounted_with_fsid("cc86845b-aec3-408e-bef5-553affc1f2b1");
        let names = vec![DiskName::parse("clone").unwrap()];
        let by_id = ByIdPath::parse("/dev/disk/by-id/usb-CLONE").unwrap();
        let by_ids_refs: Vec<&ByIdPath> = vec![&by_id];

        let result = build_add_work_plan(
            &recording,
            &AddStepsInput {
                names: &names,
                by_ids: &by_ids_refs,
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &membership,
            },
        );
        match result {
            Err(AddError::DuplicateUuid {
                uuid,
                by_id1,
                by_id2,
                ..
            }) => {
                assert_eq!(uuid.as_str(), collision_uuid);
                // by-id pairs sorted lexicographically.
                assert_eq!(by_id1.as_str(), "/dev/disk/by-id/ata-EXISTING");
                assert_eq!(by_id2.as_str(), "/dev/disk/by-id/usb-CLONE");
            }
            other => panic!("expected DuplicateUuid (membership collision), got: {other:?}"),
        }
        // No CryptsetupLuksFormat issued on the refusal path.
        let requests = recording.requests();
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksFormat { .. })),
            "membership-collision refusal must not invoke luksFormat: {requests:?}"
        );
    }

    // Intent: at the `build_add_work_plan` integration level, a live-pool
    // UUID collision against a `braid-`-prefixed foreign mapper is refused
    // with `DuplicateUuidLivePool` whose rendered message names only the add
    // target and surfaces nothing derived from the foreign mapper -- no
    // `braid-braid` double-prefix, no `braid-foreign`, no luksFormat issued.
    // Why it exists: a `braid-`-prefixed live mapper is the canonical
    // double-prefix regression -- the only input that could have rendered
    // `braid-braid-foreign` under the old mapper-synthesis path. The refusal
    // here fires at the open branch's `classify_live_pool_match`
    // `DifferentBacking` arm (the candidate by-id resolves to a different
    // backing than the live row), the `PresentLuks`-owned live-pool gate.
    // The complementary FreshLuks-owned gate
    // (`assert_fresh_uuid_absent_from_live_pool`) gets direct unit coverage in
    // `fresh_uuid_live_pool_collision_omits_foreign_mapper`. Both gates render
    // via the same `duplicate_live_pool_uuid_error`, so this pins the message
    // contract at the planner-integration level.
    // Scenario: an open braid-labeled disk probes to the same LUKS UUID as an
    // unrecognized live `PoolDevice` whose mapper is `braid-foreign`, at a
    // different backing path. `build_add_work_plan` refuses with
    // `DuplicateUuidLivePool`, naming only the real add target and reporting
    // the colliding side by scope ("live pool").
    #[test]
    fn add_live_pool_collision_omits_braid_prefixed_mapper() {
        let collision_uuid = "44444444-4444-4444-4444-444444444444";
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-foreign".into()),
                luks_uuid: LuksUuid::parse(collision_uuid).unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vdb".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: Some(Fsid::parse("cc86845b-aec3-408e-bef5-553affc1f2b1").unwrap()),
            null_underlying: vec![],
        };
        let runner = cloned_disk_runner(&[("/dev/disk/by-id/usb-CLONE", collision_uuid)]);
        let recording = RequestRecordingRunner::new(runner);
        let probed = vec![cloned_disk_probed(
            "clone",
            "/dev/disk/by-id/usb-CLONE",
            collision_uuid,
        )];
        let names = vec![DiskName::parse("clone").unwrap()];
        let by_id = ByIdPath::parse("/dev/disk/by-id/usb-CLONE").unwrap();
        let by_ids_refs: Vec<&ByIdPath> = vec![&by_id];

        let result = build_add_work_plan(
            &recording,
            &AddStepsInput {
                names: &names,
                by_ids: &by_ids_refs,
                probed: &probed,
                pool: &pool,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                paths: &test_paths().1,
                enroll_key_file: None,
                luks_format_extra_opts: &LuksFormatExtraOpts::default(),
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                pool_membership: &PoolMembership::empty(),
            },
        );
        let body = result
            .as_ref()
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default();
        match result {
            Err(AddError::DuplicateUuidLivePool { uuid, name, by_id }) => {
                assert_eq!(uuid.as_str(), collision_uuid);
                assert_eq!(name.as_str(), "clone");
                assert_eq!(by_id.as_str(), "/dev/disk/by-id/usb-CLONE");
            }
            other => {
                panic!("expected DuplicateUuidLivePool (live-pool collision), got: {other:?}")
            }
        }
        assert!(
            body.contains("add target braid-clone (/dev/disk/by-id/usb-CLONE)"),
            "live-pool refusal must name the real add target: {body}"
        );
        assert!(
            body.contains("live pool"),
            "live-pool refusal must report the colliding side by scope: {body}"
        );
        // Canonical double-prefix regression: a `braid-`-prefixed live mapper
        // (braid-foreign) is the only input that could render `braid-braid-...`
        // under the old mapper-synthesis path.
        assert!(
            !body.contains("braid-braid"),
            "live-pool refusal must not double-prefix: {body}"
        );
        assert!(
            !body.contains("braid-foreign"),
            "live-pool refusal must surface nothing derived from the foreign mapper: {body}"
        );
        // No CryptsetupLuksFormat issued on the refusal path.
        let requests = recording.requests();
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksFormat { .. })),
            "live-pool collision refusal must not invoke luksFormat: {requests:?}"
        );
    }

    // Intent: FreshLuks's live-pool guard refuses a UUID colliding with a
    // device whose mapper is non-braid (`clone-foreign`) by naming only the
    // real add target and reporting the colliding side by scope -- nothing
    // derived from the foreign mapper.
    // Why it exists: this guard (`assert_fresh_uuid_absent_from_live_pool`)
    // owns the FreshLuks live-pool refusal -- the only plan-time caller that
    // reaches a `pool.devices` scan, since the `PresentLuks` arms are
    // intercepted upstream by the backing-aware `classify_live_pool_match`.
    // The prior code synthesized a `DiskName` from the live device's mapper,
    // leaking `braid-clone-foreign` into the message (and double-prefixing
    // `braid-`-prefixed mappers). ADR 024 forbids inventing an identity for
    // the clone; this pins that a non-`braid-` foreign mapper never reaches
    // the operator-facing text (the complement of the `braid-foreign`
    // double-prefix regression above).
    // Scenario: a freshly-formatted add target's minted LUKS UUID matches a
    // live `PoolDevice` an operator opened by hand under the mapper
    // `clone-foreign`, absent from membership.
    #[test]
    fn fresh_uuid_live_pool_collision_omits_foreign_mapper() {
        let uuid = LuksUuid::parse("66666666-6666-6666-6666-666666666666").unwrap();
        let name = DiskName::parse("disk2").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap();
        let live_pool =
            pool_with_live_devices(vec![live_pool_device("clone-foreign", &uuid, "/dev/vde")]);

        let err =
            assert_fresh_uuid_absent_from_live_pool(&uuid, &live_pool, &name, &by_id).unwrap_err();

        let body = err.to_string();
        match err {
            AddError::DuplicateUuidLivePool {
                uuid: collided,
                name: target_name,
                by_id: target_by_id,
            } => {
                assert_eq!(collided.as_str(), uuid.as_str());
                assert_eq!(target_name.as_str(), "disk2");
                assert_eq!(target_by_id.as_str(), by_id.as_str());
            }
            other => panic!("expected DuplicateUuidLivePool, got: {other:?}"),
        }
        assert!(
            body.contains("add target braid-disk2 (/dev/disk/by-id/virtio-disk2)"),
            "live-pool refusal must name the real add target: {body}"
        );
        assert!(
            body.contains("live pool"),
            "live-pool refusal must report the colliding side by scope: {body}"
        );
        assert!(
            !body.contains("clone-foreign"),
            "live-pool refusal must surface nothing derived from the foreign mapper: {body}"
        );
    }
}
