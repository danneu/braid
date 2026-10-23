use crate::alert;
use crate::by_id::{ByIdResolver, by_id_priority, is_partition_entry};
use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::{self, Config};
use crate::credential::{self, OpenCredential};
use crate::credential_verify::{Credential, CredentialVerifyTarget, verify_credential_for_targets};
use crate::filesystem::Filesystem;
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal::{self, Journal};
use crate::luks::{self, BackingPathResolver, VerifyOutcome};
use crate::mapper_close::{
    CloseContext, TrackedMapper, close_mapper_best_effort, emit_close_progress,
};
use crate::membership::{
    self, DiskMember, LiveBinding, LiveBindingError, LuksUuidMap, MemberBinding, PoolMembership,
};
use crate::mount::{self, MountError, OpenPlan};
use crate::mount_check;
use crate::parse::btrfs_filesystem_show::{DeviceBtrfsProbe, classify_btrfs_probe};
use crate::parse::{ReplaceState, parse_btrfs_replace_status};
use crate::pool::{self, OwedRaid1Balance, Raid1BalanceOutcome, Raid1HostRefusal};
use crate::pool_mount::{self, MountedAt};
use crate::preview::{self, PerDiskStyle, PlanFailure, Preview, PreviewNote};
use crate::probe::{self, ProbeError};
use crate::probe_mapper_uuid::{MapperOwnership, probe_observed_mapper_uuid};
use crate::progress::{self, ProgressOutput, Sleeper};
use crate::repair_hint;
use crate::secret::Passphrase;
use crate::state_paths::StatePaths;
use crate::status::{BalanceReport, get_balance_report};
use crate::status_tag::{StatusTag, color_enabled_for_stderr, emit_status, status_line};
use crate::types::{
    ByIdPath, ConfigDiskState, Devid, DiskName, Fsid, KeyFilePath, LuksUuid, MapperName,
    MountPoint, PoolState, PresentConfigDisk, PresentConfigDiskState, format_uuid_list,
};
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RecoverError {
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] crate::parse::ParseError),
    #[error("journal error: {0}")]
    Journal(String),
    #[error("membership error: {0}")]
    Membership(#[from] membership::MembershipError),
    #[error("{0}")]
    Mount(#[from] MountError),
    #[error("luks error: {0}")]
    Luks(#[from] crate::luks::LuksError),
    #[error("{0}")]
    Failed(String),
    /// Command-boundary wrapper that preserves the typed recovery failure while
    /// appending remediation from the final authoritative journal observation.
    #[error("{source}\n{advice}")]
    JournalLifecycle {
        #[source]
        source: Box<RecoverError>,
        advice: String,
    },
    /// Recovery changed the pool, then dropping alert state failed (ADR 014).
    /// It holds the alert error itself, not free text, so only an alert-state
    /// failure can carry the delete-the-alert-files remedy; a pool or probe
    /// failure cannot be mislabeled with it. `devids` is empty for a drop
    /// that covers every disk.
    #[error(
        "pool was modified by recovery, but alert baseline cleanup failed at {stage}{}: {source}\n\
         pending-op.json is preserved; rm /var/lib/braid/acked-stats.json and \
         /var/lib/braid/alert-latch.json before trusting `braid monitor`.",
        alert::ghost_drop_devid_context(.devids)
    )]
    AckCleanupFailed {
        stage: &'static str,
        devids: Vec<Devid>,
        #[source]
        source: alert::AlertStateError,
    },
    /// Journaled-snapshot corruption: the same `devid` resolves to two or
    /// more UUIDs inside `journal.pre_membership` / `journal.target_membership`.
    /// Surfaces in canonical lexicographic UUID order.
    #[error(
        "duplicate devid {devid} in journaled membership across UUIDs {}",
        format_uuid_list(.members)
    )]
    DuplicateDevidDuringReplay {
        devid: Devid,
        members: Vec<LuksUuid>,
    },
    /// A btrfs-MISSING devid that no journaled member records sits beside
    /// journaled members with no recorded devid, so any of them could be the
    /// missing device (decision 024 rule 9). Recovery refuses rather than
    /// guess which member btrfs still holds.
    #[error("{}", ambiguous_missing_devid_message(.devids, .members))]
    AmbiguousMissingDevid {
        devids: Vec<Devid>,
        members: Vec<DiskName>,
    },
}

impl RecoverError {
    /// Preserve the dedicated degraded-mount exit status through the
    /// command-boundary lifecycle wrapper.
    pub fn is_degraded_refusal(&self) -> bool {
        match self {
            Self::Mount(MountError::DegradedRefused(_)) => true,
            Self::JournalLifecycle { source, .. } => source.is_degraded_refusal(),
            _ => false,
        }
    }
}

/// Operator wording for `RecoverError::AmbiguousMissingDevid`. It names every
/// suspect member and devid, since the operator must reconcile each by hand.
fn ambiguous_missing_devid_message(devids: &[Devid], members: &[DiskName]) -> String {
    let members = members
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let devids = devids
        .iter()
        .map(Devid::to_string)
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "recovery cannot safely correlate journaled member(s) {members} with btrfs-MISSING \
         devid(s) {devids}: the journal records no btrfs devid for those members and live \
         btrfs reports the missing devid(s) without observable LUKS UUIDs, so any of those \
         members could be a missing device. Preserve pending-op.json and rebuild membership \
         with braid discover --write only after manual reconciliation -- see \
         docs/guides/recovery-scenarios.md and docs/internals/luks-unlock.md."
    )
}

impl From<membership::BindingProjectionError> for RecoverError {
    fn from(value: membership::BindingProjectionError) -> Self {
        match value {
            membership::BindingProjectionError::AmbiguousMissingDevid { devids, members } => {
                RecoverError::AmbiguousMissingDevid { devids, members }
            }
            membership::BindingProjectionError::Membership(e) => RecoverError::Membership(e),
        }
    }
}

/// Recovery-local passphrase holder that preserves zeroizing ownership.
///
/// Borrowed values refer to the already-resolved `OpenCredential`; owned values
/// are freshly read for recovery and must still zeroize on drop.
enum RecoverPassphrase<'a> {
    Borrowed(&'a Passphrase),
    Owned(Passphrase),
}

impl RecoverPassphrase<'_> {
    /// Return the passphrase boundary while preserving the owned/borrowed
    /// recovery distinction.
    fn expose_secret(&self) -> &Passphrase {
        match self {
            Self::Borrowed(z) => z,
            Self::Owned(z) => z,
        }
    }
}

/// Find the `/dev/disk/by-id/` symlink whose canonical target matches `underlying`.
///
/// `underlying` is a live pool device's backing kernel path (from `cryptsetup status`).
/// We pick the highest-priority match by `by_id::by_id_priority` so the recorded
/// `by_id` is the most stable identifier the kernel exposes for this device.
///
/// Hard-fails if no by-id symlink resolves to `underlying` — recovery refuses to
/// guess a stable identifier when none exists.
fn resolve_by_id_for_underlying(
    resolver: &dyn ByIdResolver,
    underlying: &str,
) -> Result<ByIdPath, RecoverError> {
    let by_id_dir = "/dev/disk/by-id";

    // Canonical kernel path of the live pool device, used as the join key.
    let target = resolver.canonicalize(underlying).map_err(|e| {
        RecoverError::Failed(format!(
            "cannot canonicalize live pool device {underlying}: {e}"
        ))
    })?;

    let entries = resolver
        .list_by_id_entries()
        .map_err(|e| RecoverError::Failed(format!("cannot read {by_id_dir}: {e}")))?;

    // (priority, filename, full_path) for every by-id entry that resolves to `target`.
    let mut matches: Vec<(u8, String, String)> = Vec::new();
    for name in entries {
        if is_partition_entry(&name) {
            continue;
        }
        let full = format!("{by_id_dir}/{name}");
        // Skip dangling/broken symlinks silently — they cannot match anything.
        let Ok(resolved) = resolver.canonicalize(&full) else {
            continue;
        };
        if resolved == target {
            matches.push((by_id_priority(&name), name, full));
        }
    }

    if matches.is_empty() {
        return Err(RecoverError::Failed(format!(
            "live pool device '{underlying}' has no /dev/disk/by-id/ symlink \
             resolving to it. Recovery cannot persist a stable identifier for \
             this device.\n\
             To inspect the udev-created symlinks for this device, run:\n  \
             udevadm info --query=symlink --name {underlying}\n\
             If the output contains no `disk/by-id/...` entries, ensure udev \
             is running and the device's hardware identifiers are exposed by \
             the kernel. If by-id entries exist \
             but none match this device's canonical path, file a braid bug \
             with the udevadm output."
        )));
    }

    // Stable highest-priority pick: lowest by_id_priority wins, ties broken by filename.
    matches.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    let selected = matches.into_iter().next().unwrap().2;
    ByIdPath::parse(&selected).map_err(|e| RecoverError::Failed(e.to_string()))
}

/// Decision 017 `added_at` precedence for recover's pool.json rebuild.
///
/// Existing pool.json wins, then the journal snapshot member, then a fresh
/// stamp. Keep this local because recover owns the multi-source replay rule.
fn resolve_added_at(
    prior: Option<&PoolMembership>,
    fallback: &DiskMember,
    uuid: &LuksUuid,
) -> Option<String> {
    prior
        .and_then(|p| p.by_uuid(uuid))
        .and_then(|m| m.added_at.clone())
        .or_else(|| fallback.added_at.clone())
        .or_else(|| Some(crate::util::now_iso()))
}

/// Rebuild pool.json from the live mounted pool and clear the pending-operation journal.
///
/// This is the only path out of recovery mode. It opens LUKS devices and mounts
/// the pool if needed, then probes the actual btrfs pool topology (not LUKS
/// labels) and builds membership from live state.
pub struct RecoverParams<'a> {
    pub config: &'a Config,
    pub paths: &'a StatePaths,
    pub passphrase_stdin: bool,
    pub passphrase_file: Option<&'a std::path::Path>,
    pub allow_degraded: bool,
    pub dry_run: bool,
    /// Progress output for post-mount maintenance that can run for a long time,
    /// such as replace resize replay or owed RAID1 balance replay.
    pub progress: ProgressOutput,
    pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
    /// Sleeper seam for transient mapper-close retries and recovery
    /// polling paths so unit tests never depend on real wall-clock sleeps.
    pub sleeper: &'a dyn progress::Sleeper,
    /// TTY reader seam for interactive recover passphrase prompts.
    pub tty: &'a dyn luks::PassphraseReader,
    /// Seam for resolving by-id paths and mapper backings during recovery
    /// probes and already-open mapper checks.
    pub backing_path_resolver: &'a dyn BackingPathResolver,
}

/// Dry-run preview source of truth for `braid recover` plus the
/// execute inputs pre-computed during planning. `preview()` renders
/// accumulated notes plus steps from the semantic work plan; `execute()`
/// renders `notes` to stderr with `PerDiskStyle::Bracketed` before any mutation,
/// preserving today's "entry banner then probe context then work"
/// real-run sequence.
///
/// `open_plan` is `None` when the pool was already mounted at probe
/// time. `notes` carries the entry-banner `Info` note first, then the
/// `ProbeEvent`-derived notes (including `AlreadyMounted`) so both
/// `preview()` and `execute()` surface them in order.
#[derive(Debug)]
pub struct RecoverPlan {
    pub notes: Vec<PreviewNote>,
    work_plan: RecoverWorkPlan,
}

#[derive(Debug)]
struct RecoverWorkPlan {
    open_plan: Option<OpenPlan>,
    pre_resolved_credential: Option<OpenCredential>,
    journal: Journal,
    admission_membership: PoolMembership,
    mount_point: MountPoint,
    pool_json_path: PathBuf,
    pending_op_path: PathBuf,
    luks_headers_dir: PathBuf,
    actions: Vec<RecoverWorkAction>,
}

#[derive(Debug)]
enum RecoverWorkAction {
    /// A bootstrap add that stopped before mkfs.btrfs: there is no pool, so
    /// recover only clears the journal. `formatted` lists the disks
    /// luksFormat wrote, sorted by name, which the operator wipes before
    /// re-running add. Terminal; nothing is opened or mounted.
    ClearUncommittedBootstrap {
        formatted: Vec<(DiskName, ByIdPath)>,
    },
    InitialOpenPool,
    WaitForKernelReplace,
    RemountCycle {
        close_names: Vec<DiskName>,
        reopen_names: Vec<DiskName>,
        any_missing_member: bool,
    },
    // Boxed because a completion is far larger than every other action
    // (clippy::large_enum_variant).
    Complete(Box<RecoverCompletion>),
}

/// How recover finishes the journaled op once the pool is mounted. Each arm
/// carries the RAID1 balance the op still owes, set once in `plan_recover`,
/// so the dry-run rows and the replay read one value.
#[derive(Debug)]
enum RecoverCompletion {
    AddPoolMutation {
        targets: LuksUuidMap<journal::AddJournalTarget>,
        all_targets_already_live: bool,
        /// Targets the dry-run probe found committed, read from the live
        /// binding; `None` when the pool was not mounted to probe.
        committed_targets: Option<std::collections::BTreeSet<LuksUuid>>,
        /// Fresh targets whose disk read as not LUKS at plan time, which
        /// recover drops instead of replaying; see
        /// `drop_never_formatted_add_targets`.
        never_formatted: std::collections::BTreeSet<LuksUuid>,
        owed_balance: OwedRaid1Balance,
    },
    AddPostBalance {
        owed_balance: OwedRaid1Balance,
    },
    RemoveMissingPoolMutation {
        devid: Devid,
        owed_balance: Option<OwedRaid1Balance>,
    },
    RemoveMissingPostMaintenance {
        devid: Devid,
        owed_balance: Option<OwedRaid1Balance>,
    },
    ReplacePoolMutation {
        identity: journal::ReplaceIdentity,
        new_name: DiskName,
        new_target: journal::ReplaceJournalTarget,
        source: journal::ReplaceJournalSource,
        uncommitted_writes: Option<UncommittedReplaceWrites>,
        owed_balance: Option<OwedRaid1Balance>,
    },
    ReplacePostMaintenance {
        new_uuid: LuksUuid,
        new_name: DiskName,
        source: journal::ReplaceJournalSource,
        owed_balance: Option<OwedRaid1Balance>,
    },
    GenericLivePool {
        owed_balance: Option<OwedRaid1Balance>,
    },
}

struct RecoverExecutionState {
    credential: Option<OpenCredential>,
    just_mounted: bool,
}

impl RecoverWorkPlan {
    fn render_steps(&self) -> Vec<Step> {
        let mut steps = Vec::new();
        for action in &self.actions {
            action.render_into(self, &mut steps);
        }
        steps
    }

    fn execute<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
        mut self,
        runner: &R,
        fs: &F,
        by_id_resolver: &dyn ByIdResolver,
        params: &RecoverParams<'_>,
    ) -> Result<(), RecoverError> {
        let mut state = RecoverExecutionState {
            credential: self.pre_resolved_credential.take(),
            just_mounted: false,
        };
        for action in &self.actions {
            if action.execute(&self, &mut state, runner, fs, by_id_resolver, params)? {
                return Ok(());
            }
        }
        Err(RecoverError::Failed(
            "internal error: recover work plan had no terminal action".into(),
        ))
    }
}

impl RecoverWorkAction {
    fn render_into(&self, plan: &RecoverWorkPlan, steps: &mut Vec<Step>) {
        match self {
            RecoverWorkAction::ClearUncommittedBootstrap { formatted } => {
                let detail = if formatted.is_empty() {
                    "no disk was formatted".to_owned()
                } else {
                    format!(
                        "wipe the formatted disks before re-running braid add: {}",
                        formatted
                            .iter()
                            .map(|(name, _)| name.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                steps.push(Step {
                    risk: "safe",
                    description: format!(
                        "clear pending-op.json -> {} (bootstrap add stopped before mkfs.btrfs; {detail})",
                        plan.pending_op_path.display()
                    ),
                    commands: vec![],
                });
            }
            RecoverWorkAction::InitialOpenPool => {
                if let Some(open_plan) = &plan.open_plan {
                    steps.extend(mount::compile_open_steps(
                        open_plan,
                        &plan.mount_point,
                        None,
                    ));
                }
            }
            RecoverWorkAction::WaitForKernelReplace => {
                steps.push(Step {
                    risk: "long",
                    description:
                        "wait for kernel dev_replace to finish (skipped if no running replace)"
                            .into(),
                    commands: vec![],
                });
            }
            RecoverWorkAction::RemountCycle {
                close_names,
                reopen_names,
                any_missing_member,
            } => {
                steps.push(Step {
                    risk: "safe",
                    description: format!("unmount {} (recover remount cycle)", plan.mount_point),
                    commands: vec![CmdRequest::Umount {
                        mount_point: plan.mount_point.clone(),
                    }],
                });

                let forget_devs: Vec<String> = close_names
                    .iter()
                    .map(|name| config::mapper_name(name).dev_path())
                    .collect();
                if !forget_devs.is_empty() {
                    steps.push(Step {
                        risk: "safe",
                        description: "btrfs device scan --forget (recover remount cycle)".into(),
                        commands: vec![CmdRequest::BtrfsDeviceScanForget {
                            devices: forget_devs,
                        }],
                    });
                }

                for name in close_names {
                    let mn = config::mapper_name(name);
                    steps.push(Step {
                        risk: "safe",
                        description: format!("close LUKS mapper {} (recover remount cycle)", mn),
                        commands: vec![CmdRequest::CryptsetupClose { mapper: mn.clone() }],
                    });
                }

                for name in reopen_names {
                    let member = plan
                        .admission_membership
                        .by_name(name)
                        .map(|(_, m)| m)
                        .expect("remount-cycle reopen target validated during planning");
                    let mn = config::mapper_name(name);
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "LUKS open {} -> {} (recover remount cycle)",
                            member.by_id, mn,
                        ),
                        commands: vec![CmdRequest::CryptsetupLuksOpen {
                            device: member.by_id.as_str().to_owned(),
                            mapper: mn.clone(),
                        }],
                    });
                }

                steps.push(Step {
                    risk: "safe",
                    description: "btrfs device scan (recover remount cycle)".into(),
                    commands: vec![CmdRequest::BtrfsDeviceScanAll],
                });

                let first_reopen_name = reopen_names
                    .first()
                    .expect("remount-cycle mount target validated during planning");
                let mount_device = config::mapper_name(first_reopen_name).dev_path();
                if *any_missing_member {
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "mount -> {} (recover remount cycle, degraded)",
                            plan.mount_point
                        ),
                        commands: vec![CmdRequest::MountWithOptions {
                            device: mount_device,
                            mount_point: plan.mount_point.clone(),
                            options: vec!["degraded".to_owned()],
                        }],
                    });
                } else {
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "mount -> {} (recover remount cycle)",
                            plan.mount_point
                        ),
                        commands: vec![CmdRequest::Mount {
                            device: mount_device,
                            mount_point: plan.mount_point.clone(),
                        }],
                    });
                }
            }
            RecoverWorkAction::Complete(completion) => completion.render_into(plan, steps),
        }
    }

    fn execute<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
        &self,
        plan: &RecoverWorkPlan,
        state: &mut RecoverExecutionState,
        runner: &R,
        fs: &F,
        by_id_resolver: &dyn ByIdResolver,
        params: &RecoverParams<'_>,
    ) -> Result<bool, RecoverError> {
        match self {
            RecoverWorkAction::ClearUncommittedBootstrap { formatted } => {
                clear_uncommitted_bootstrap(params.paths, formatted)?;
                Ok(true)
            }
            RecoverWorkAction::InitialOpenPool => {
                execute_recover_initial_open(plan, state, runner, fs, params)?;
                Ok(false)
            }
            RecoverWorkAction::WaitForKernelReplace => {
                if state.just_mounted {
                    wait_for_kernel_replace_to_finish(
                        runner,
                        &plan.mount_point,
                        params.sleeper,
                        color_enabled_for_stderr(),
                    )?;
                }
                Ok(false)
            }
            RecoverWorkAction::RemountCycle { close_names, .. } => {
                if state.just_mounted {
                    let recovery_mount_membership =
                        mount_membership_for_recover(&plan.journal, &plan.admission_membership)
                            .clone();
                    let cred = state.credential.as_ref().expect(
                        "just_mounted implies open_plan was Some and credential was resolved",
                    );
                    relock_and_remount(
                        runner,
                        fs,
                        RelockAndRemountCtx {
                            sleeper: params.sleeper,
                            config: params.config,
                            paths: params.paths,
                            membership: &recovery_mount_membership,
                            backing_path_resolver: params.backing_path_resolver,
                            allow_degraded: params.allow_degraded,
                            credential: cred,
                            close_names,
                        },
                    )?;
                }
                Ok(false)
            }
            RecoverWorkAction::Complete(completion) => {
                completion.execute(plan, state, runner, fs, by_id_resolver, params)?;
                Ok(true)
            }
        }
    }
}

impl RecoverCompletion {
    fn render_into(&self, plan: &RecoverWorkPlan, steps: &mut Vec<Step>) {
        match self {
            RecoverCompletion::AddPoolMutation {
                targets,
                all_targets_already_live,
                committed_targets,
                never_formatted,
                owed_balance,
            } => {
                render_add_pool_mutation_recovery_steps(
                    plan,
                    steps,
                    targets,
                    *all_targets_already_live,
                    committed_targets.as_ref(),
                    never_formatted,
                );
                let all_dropped = targets.keys().all(|uuid| {
                    never_formatted.contains(uuid)
                        && !committed_targets
                            .as_ref()
                            .is_some_and(|committed| committed.contains(uuid))
                });
                if all_dropped {
                    render_uncommitted_add_tail(plan, steps);
                } else {
                    render_recovery_tail(plan, steps, None, Some(*owed_balance));
                }
            }
            RecoverCompletion::AddPostBalance { owed_balance } => {
                render_recovery_tail(plan, steps, None, Some(*owed_balance));
            }
            RecoverCompletion::RemoveMissingPoolMutation { owed_balance, .. }
            | RecoverCompletion::RemoveMissingPostMaintenance { owed_balance, .. } => {
                render_recovery_tail(plan, steps, None, *owed_balance);
            }
            RecoverCompletion::ReplacePoolMutation {
                new_name,
                uncommitted_writes,
                owed_balance,
                ..
            } => {
                if let Some(writes) = uncommitted_writes {
                    writes.render_into(&plan.luks_headers_dir, steps);
                }
                render_recovery_tail(
                    plan,
                    steps,
                    Some(ReplaceResizePreview {
                        new_name,
                        skipped_if_replacement_not_committed: true,
                    }),
                    *owed_balance,
                );
            }
            RecoverCompletion::ReplacePostMaintenance {
                new_name,
                owed_balance,
                ..
            } => {
                render_recovery_tail(
                    plan,
                    steps,
                    Some(ReplaceResizePreview {
                        new_name,
                        skipped_if_replacement_not_committed: false,
                    }),
                    *owed_balance,
                );
            }
            RecoverCompletion::GenericLivePool { owed_balance } => {
                render_recovery_tail(plan, steps, None, *owed_balance);
            }
        }
    }

    fn execute<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
        &self,
        plan: &RecoverWorkPlan,
        state: &RecoverExecutionState,
        runner: &R,
        fs: &F,
        by_id_resolver: &dyn ByIdResolver,
        params: &RecoverParams<'_>,
    ) -> Result<(), RecoverError> {
        let pool = probe::probe_pool(runner, fs, &plan.mount_point)?;
        match mount_check::mount_entry_at_via_fs(fs, plan.mount_point.as_str()) {
            Ok(Some(entry)) if mount_check::entry_is_read_only(&entry) => {
                return Err(RecoverError::Failed(format!(
                    "recovery aborted: pool at {mp} is mounted read-only \
                     (vfs_options={:?}, fs_options={:?}). pool.json was not \
                     written and the pending-op journal is preserved.\n{hint}",
                    entry.vfs_options,
                    entry.fs_options,
                    mp = plan.mount_point,
                    hint = repair_hint::read_only_pool(
                        &plan.mount_point,
                        repair_hint::ReopenWith::Recover
                    ),
                )));
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(e) => return Err(RecoverError::Probe(ProbeError::MountInfo(e))),
        }
        // Completion runs only after recovery either opened the pool or
        // observed it already mounted. If this fresh post-mount probe no
        // longer sees a mounted pool with members, preserve pool.json and
        // the pending journal so the operator can fix the mount state and
        // retry.
        if !pool.mounted || (pool.devices.is_empty() && !plan.admission_membership.is_empty()) {
            let probe_state = if !pool.mounted {
                "no btrfs mount"
            } else {
                "zero btrfs devices"
            };
            return Err(RecoverError::Failed(format!(
                "recovery aborted: post-mount probe at {} reports {} -- \
                 expected a mounted pool with members. pool.json was not \
                 written and the pending-op journal is preserved. Investigate \
                 (external umount? btrfs auto-remount-ro? mount_point \
                 mismatch?).",
                plan.mount_point, probe_state
            )));
        }
        match self {
            RecoverCompletion::AddPoolMutation {
                targets,
                never_formatted,
                owed_balance,
                ..
            } => execute_add_pool_mutation_recovery(
                runner,
                fs,
                by_id_resolver,
                params,
                AddPoolReplayCtx {
                    credential: state.credential.as_ref(),
                    journal: &plan.journal,
                    union: &plan.admission_membership,
                    targets,
                    pool,
                    owed_balance: *owed_balance,
                },
                never_formatted,
            ),
            RecoverCompletion::AddPostBalance { owed_balance } => {
                execute_add_post_balance_recovery(
                    runner,
                    fs,
                    by_id_resolver,
                    params,
                    &plan.journal,
                    pool,
                    *owed_balance,
                    false,
                )
            }
            RecoverCompletion::RemoveMissingPoolMutation {
                devid,
                owed_balance,
            } => execute_remove_missing_pool_mutation_recovery(
                runner,
                fs,
                by_id_resolver,
                params,
                &plan.journal,
                pool,
                *devid,
                *owed_balance,
            ),
            RecoverCompletion::RemoveMissingPostMaintenance {
                devid,
                owed_balance,
            } => execute_remove_missing_post_maintenance_recovery(
                runner,
                fs,
                by_id_resolver,
                params,
                RemoveMissingPostCtx {
                    journal: &plan.journal,
                    pool,
                    devid: *devid,
                    owed_balance: *owed_balance,
                    inhibitor_already_held: false,
                },
            ),
            RecoverCompletion::ReplacePoolMutation {
                identity,
                new_name,
                new_target,
                source,
                uncommitted_writes,
                owed_balance,
            } => execute_replace_pool_mutation_recovery(
                runner,
                fs,
                by_id_resolver,
                params,
                state.credential.as_ref(),
                &plan.journal,
                &plan.admission_membership,
                pool,
                identity,
                new_name,
                new_target,
                source,
                uncommitted_writes.as_ref(),
                *owed_balance,
            ),
            RecoverCompletion::ReplacePostMaintenance {
                new_uuid,
                new_name,
                source,
                owed_balance,
            } => execute_replace_post_maintenance_recovery(
                runner,
                fs,
                params.sleeper,
                by_id_resolver,
                params,
                &plan.journal,
                pool,
                new_uuid,
                new_name,
                source,
                *owed_balance,
                false,
            ),
            RecoverCompletion::GenericLivePool { owed_balance } => {
                execute_generic_live_pool_recovery(
                    runner,
                    fs,
                    by_id_resolver,
                    params,
                    plan,
                    pool,
                    *owed_balance,
                )
            }
        }
    }
}

struct ReplaceResizePreview<'a> {
    new_name: &'a DiskName,
    skipped_if_replacement_not_committed: bool,
}

/// The LUKS writes recover owes the new disk when a replace did not commit,
/// chosen once in `plan_recover` from the journaled target mode. The dry-run
/// rows and `finish_uncommitted_replace_recovery` both read this one value,
/// so the preview cannot leave out a write the replay runs (ADR 022).
#[derive(Debug)]
struct UncommittedReplaceWrites {
    by_id: ByIdPath,
    mapper: MapperName,
    enroll_key_file: Option<KeyFilePath>,
    /// A fresh disk with no LUKS header never reached luksFormat, so the
    /// replay skips these writes for it.
    skipped_without_luks_header: bool,
}

impl UncommittedReplaceWrites {
    /// `None` for an existing-LUKS disk with no keyfile to enroll: replace
    /// planned no LUKS write on it, so recover owes none and asks for no
    /// passphrase. A fresh disk always owes its header backup.
    fn plan(new_name: &DiskName, new_target: &journal::ReplaceJournalTarget) -> Option<Self> {
        let (enroll_key_file, skipped_without_luks_header) = match &new_target.mode {
            journal::ReplaceJournalMode::ExistingLuks {
                enroll_key_file: None,
            } => return None,
            journal::ReplaceJournalMode::ExistingLuks {
                enroll_key_file: Some(key_file),
            } => (Some(key_file.clone()), false),
            // The uncommitted branch never reformats, so `extra_opts` plays
            // no part in its writes.
            journal::ReplaceJournalMode::FreshLuks {
                extra_opts: _,
                enroll_key_file,
            } => (enroll_key_file.clone(), true),
        };
        Some(Self {
            by_id: new_target.by_id.clone(),
            mapper: config::mapper_name(new_name),
            enroll_key_file,
            skipped_without_luks_header,
        })
    }

    fn render_into(&self, luks_headers_dir: &std::path::Path, steps: &mut Vec<Step>) {
        let suffix = if self.skipped_without_luks_header {
            " (skipped if replacement committed, or if the new disk shows no LUKS header)"
        } else {
            " (skipped if replacement committed)"
        };
        luks::push_enrollment_preview_steps(
            steps,
            &self.by_id,
            self.enroll_key_file.as_ref(),
            &luks::luks_header_backup_path(luks_headers_dir, &self.mapper),
            Some(suffix),
        );
    }

    /// Runs the writes in the order `render_into` shows them. The caller has
    /// already checked the new disk's identity and the passphrase, and holds
    /// the sleep inhibitor.
    fn execute<R: CommandRunner>(
        &self,
        runner: &R,
        passphrase: &Passphrase,
        paths: &StatePaths,
    ) -> Result<(), RecoverError> {
        if let Some(key_file) = &self.enroll_key_file {
            ensure_keyfile_enrolled(runner, self.by_id.as_str(), passphrase, key_file)?;
        }
        luks::backup_luks_header(runner, self.by_id.as_str(), &self.mapper, paths)?;
        Ok(())
    }
}

/// Render add-recovery replay steps in DiskName-sorted order.
///
/// The journal's `targets` map is UUID-keyed (random per-disk under v4),
/// so iterating it in map order would surface "replay fresh add target ..."
/// rows in a different order every recover. Sort by `target.name` first so
/// operator-visible preview output stays alphabetical-by-name -- the same
/// order the live `add` command renders.
fn render_add_pool_mutation_recovery_steps(
    plan: &RecoverWorkPlan,
    steps: &mut Vec<Step>,
    targets: &LuksUuidMap<journal::AddJournalTarget>,
    all_targets_already_live: bool,
    committed_targets: Option<&std::collections::BTreeSet<LuksUuid>>,
    never_formatted: &std::collections::BTreeSet<LuksUuid>,
) {
    steps.push(Step {
        risk: "safe",
        description: if all_targets_already_live {
            "reconcile journaled add targets against live pool (no replay needed: all targets already live)"
                .into()
        } else {
            "reconcile journaled add targets against live pool".into()
        },
        commands: vec![],
    });
    let conditional_suffix =
        " (skipped at runtime if open/scan reconciliation makes target live before replay)";

    // Sort by DiskName so operator-visible iteration order is stable
    // across recover runs, matching the live add command.
    let mut sorted: Vec<(&LuksUuid, &journal::AddJournalTarget)> = targets.iter().collect();
    sorted.sort_by(|a, b| a.1.name.cmp(&b.1.name));

    for (uuid, target) in sorted {
        let mapper = config::mapper_name(&target.name);
        let mapper_path = mapper.dev_path();
        if committed_targets.is_some_and(|committed| committed.contains(uuid)) {
            let (kind, label) = match &target.mode {
                journal::AddJournalMode::RecoverableBraidLabeled { .. } => {
                    ("verified returned-disk add", mapper_path.clone())
                }
                journal::AddJournalMode::FreshLuks { .. } => {
                    ("fresh add target", target.by_id.as_str().to_owned())
                }
            };
            steps.push(Step {
                risk: "safe",
                description: format!(
                    "replay {kind} {label} (skipped: target already live in pool)"
                ),
                commands: vec![],
            });
            continue;
        }
        if never_formatted.contains(uuid) {
            steps.push(Step {
                risk: "safe",
                description: format!(
                    "drop fresh add target {} from pending-op.json (luksFormat never wrote it; \
                     refused if btrfs reports a missing devid no pre-operation member records)",
                    target.by_id
                ),
                commands: vec![],
            });
            continue;
        }
        match &target.mode {
            journal::AddJournalMode::RecoverableBraidLabeled {
                enroll_key_file, ..
            } => {
                // Mirror the executor's order: when `enroll_key_file:
                // Some(kf)`, addKey + luksHeaderBackup run BEFORE
                // pool_add_device (which expands to scanForget +
                // wipefs + btrfs device add). Putting them ahead of
                // scanForget keeps `recover --dry-run` byte-aligned
                // with replay; placing them between wipefs and
                // btrfs-add would falsely show wipefs running before
                // the slot 1 mutation. None branch is unchanged.
                let mut commands = Vec::new();
                if let Some(key_file) = enroll_key_file {
                    commands.push(CmdRequest::CryptsetupLuksAddKeyFile {
                        device: target.by_id.as_str().to_owned(),
                        key_file_path: key_file.as_path().display().to_string(),
                    });
                    commands.push(CmdRequest::CryptsetupLuksHeaderBackup {
                        device: target.by_id.as_str().to_owned(),
                        backup_path: luks::luks_header_backup_path(&plan.luks_headers_dir, &mapper)
                            .as_path()
                            .display()
                            .to_string(),
                    });
                }
                commands.push(CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![mapper_path.clone()],
                });
                commands.push(CmdRequest::WipefsBtrfs {
                    device: mapper_path.clone(),
                });
                commands.push(CmdRequest::BtrfsDeviceAdd {
                    device: mapper_path,
                    mount_point: plan.mount_point.clone(),
                    force: true,
                });
                let mapper_path_for_description = mapper.dev_path();
                steps.push(Step {
                    risk: "safe",
                    description: format!(
                        "replay verified returned-disk add {mapper_path_for_description}{conditional_suffix}"
                    ),
                    commands,
                });
            }
            // `extra_opts` is unread: recover never formats a disk.
            journal::AddJournalMode::FreshLuks {
                enroll_key_file, ..
            } => {
                let mut commands = Vec::new();
                if let Some(key_file) = enroll_key_file {
                    commands.push(CmdRequest::CryptsetupLuksAddKeyFile {
                        device: target.by_id.as_str().to_owned(),
                        key_file_path: key_file.as_path().display().to_string(),
                    });
                }
                commands.push(CmdRequest::CryptsetupLuksHeaderBackup {
                    device: target.by_id.as_str().to_owned(),
                    backup_path: luks::luks_header_backup_path(&plan.luks_headers_dir, &mapper)
                        .as_path()
                        .display()
                        .to_string(),
                });
                commands.push(CmdRequest::CryptsetupLuksOpen {
                    device: target.by_id.as_str().to_owned(),
                    mapper: mapper.clone(),
                });
                commands.push(CmdRequest::BtrfsDeviceAdd {
                    device: mapper_path,
                    mount_point: plan.mount_point.clone(),
                    force: false,
                });
                let expected_label = config::luks_label_for(&target.name);
                let fresh_conditional_suffix = format!(
                    "{conditional_suffix} (refused at runtime unless the disk shows a LUKS header with the journaled UUID and the '{expected_label}' label)"
                );
                steps.push(Step {
                    risk: "safe",
                    description: format!(
                        "replay fresh add target {}{fresh_conditional_suffix}",
                        target.by_id
                    ),
                    commands,
                });
            }
        }
    }
}

/// Dry-run tail for a live add whose every target luksFormat never wrote: the
/// rows `drop_never_formatted_add_targets` runs when it rolls the add back.
fn render_uncommitted_add_tail(plan: &RecoverWorkPlan, steps: &mut Vec<Step>) {
    steps.push(Step {
        risk: "safe",
        description: format!(
            "write pre-operation pool.json -> {} (add did not commit)",
            plan.pool_json_path.display()
        ),
        commands: vec![],
    });
    steps.push(Step {
        risk: "safe",
        description: format!(
            "clear pending-op.json -> {}",
            plan.pending_op_path.display()
        ),
        commands: vec![],
    });
}

/// Finish a bootstrap add that stopped before mkfs.btrfs: clear the journal
/// and name the disks luksFormat wrote. pool.json stays as it is, since
/// bootstrap add writes it only after mkfs.
fn clear_uncommitted_bootstrap(
    paths: &StatePaths,
    formatted: &[(DiskName, ByIdPath)],
) -> Result<(), RecoverError> {
    journal::clear_journal(paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
    eprintln!("{}", uncommitted_bootstrap_message(formatted));
    Ok(())
}

/// What recover tells the operator after it clears an uncommitted bootstrap
/// journal: either nothing changed, or which disks still carry the LUKS
/// container braid wrote and how to wipe them.
fn uncommitted_bootstrap_message(formatted: &[(DiskName, ByIdPath)]) -> String {
    if formatted.is_empty() {
        return "bootstrap add stopped before luksFormat wrote any disk, so nothing changed. \
                pending-op.json cleared. Re-run braid add to retry."
            .to_owned();
    }
    let disks: Vec<String> = formatted
        .iter()
        .map(|(name, by_id)| format!("  {name} ({by_id})"))
        .collect();
    format!(
        "bootstrap add stopped before mkfs.btrfs created the pool, so there is no pool to \
         recover. pending-op.json cleared.\n\
         braid add refuses a braid-labeled disk with no btrfs, so wipe the LUKS container \
         braid wrote on each of these disks before re-running braid add:\n{}\n  \
         e.g.: sudo wipefs -a /dev/disk/by-id/<device>",
        disks.join("\n")
    )
}

fn render_recovery_tail(
    plan: &RecoverWorkPlan,
    steps: &mut Vec<Step>,
    resize: Option<ReplaceResizePreview<'_>>,
    owed_balance: Option<OwedRaid1Balance>,
) {
    steps.push(Step {
        risk: "safe",
        description: format!(
            "write recovered pool.json -> {}",
            plan.pool_json_path.display()
        ),
        commands: vec![],
    });

    // On a replace journaled before it committed, the resize and the owed
    // balance both run only if the replacement committed, so one fact marks
    // both rows.
    let balance_condition = resize
        .as_ref()
        .is_some_and(|resize| resize.skipped_if_replacement_not_committed)
        .then_some("if replacement did not commit");
    if let Some(resize) = resize {
        let suffix = if resize.skipped_if_replacement_not_committed {
            format!(
                " (skipped if replacement did not commit; devid for '{}' resolved post-mount)",
                resize.new_name
            )
        } else {
            format!(" (devid for '{}' resolved post-mount)", resize.new_name)
        };
        steps.push(Step {
            risk: "safe",
            description: format!(
                "btrfs filesystem resize <devid>:max {}{}",
                plan.mount_point, suffix
            ),
            commands: vec![],
        });
    }

    if let Some(owed_balance) = owed_balance {
        steps.push(Step {
            risk: "safe",
            description: format!(
                "check btrfs balance status {} is idle before RAID1 replay",
                plan.mount_point
            ),
            commands: vec![],
        });
        steps.push(owed_balance.replay_preview_step(&plan.mount_point, balance_condition));
    }

    steps.push(Step {
        risk: "safe",
        description: format!(
            "clear pending-op.json -> {}",
            plan.pending_op_path.display()
        ),
        commands: vec![],
    });
}

fn execute_recover_initial_open<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    plan: &RecoverWorkPlan,
    state: &mut RecoverExecutionState,
    runner: &R,
    fs: &F,
    params: &RecoverParams<'_>,
) -> Result<(), RecoverError> {
    let Some(open_plan) = plan.open_plan.as_ref() else {
        state.just_mounted = false;
        return Ok(());
    };

    // Recover-specific eager resolve: only Replace::PoolMutation needs the
    // credential up front, because its post-mount RemountCycle action closes
    // every mapper and must reopen them with the same credential. Every other
    // op kind either has no post-mount credential consumer at all, or covers
    // its closed-mapper / replay-verify cases via the lazy seam in
    // recover_passphrase (single-passphrase principle preserved).
    if is_replace_pool_mutation(&plan.journal.op) && state.credential.is_none() {
        state.credential = Some(
            credential::resolve_credential(
                params.passphrase_stdin,
                params.passphrase_file,
                None, // recover does not expose --key-file today
            )
            .map_err(|e| RecoverError::Failed(format!("recover: {e}")))?,
        );
    }

    enum InitialOpenFailure {
        MountOnly(MountError),
        Unlock(mount::UnlockAndMountFailure),
    }

    let res: Result<bool, InitialOpenFailure> = if open_plan.to_unlock.is_empty() {
        mount::execute_mount_only(runner, fs, params.config, open_plan)
            .map_err(InitialOpenFailure::MountOnly)
    } else {
        if state.credential.is_none() {
            state.credential = Some(
                credential::resolve_credential(
                    params.passphrase_stdin,
                    params.passphrase_file,
                    None, // recover does not expose --key-file today
                )
                .map_err(|e| RecoverError::Failed(format!("recover: {e}")))?,
            );
        }
        let cred = state
            .credential
            .as_ref()
            .expect("credential resolved above for this branch");
        mount::execute_unlock_and_mount(
            runner,
            fs,
            params.config,
            open_plan,
            params.backing_path_resolver,
            cred,
        )
        .map_err(InitialOpenFailure::Unlock)
    };

    state.just_mounted = match res {
        Ok(just_mounted) => just_mounted,
        Err(InitialOpenFailure::MountOnly(e)) => {
            return Err(e.into());
        }
        Err(InitialOpenFailure::Unlock(failure)) => {
            // A bootstrap that never reached mkfs.btrfs was cleared in
            // `plan_bootstrap_recovery` before this mount, so a mount failure
            // here is a real one.
            let _ = mount::close_opened_mappers(
                runner,
                params.sleeper,
                fs,
                &failure.opened_mappers,
                color_enabled_for_stderr(),
            );
            return Err(failure.error.into());
        }
    };
    Ok(())
}

fn execute_generic_live_pool_recovery<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    plan: &RecoverWorkPlan,
    pool: PoolState,
    owed_balance: Option<OwedRaid1Balance>,
) -> Result<(), RecoverError> {
    // One binding for both journal kinds (decision 024 rule 9): a member
    // btrfs still holds -- present, null-backed, or MISSING at the devid the
    // journal records for it -- stays in pool.json. An absent member with a
    // recorded devid that btrfs no longer reports is gone, as a committed
    // remove leaves its target. An absent member with no recorded devid
    // beside a MISSING devid nobody records refuses: either could be the
    // other.
    let null_backed_scope = if matches!(&plan.journal.op, journal::OpKind::Remove { .. }) {
        "pre-operation membership"
    } else {
        ADMISSION_SCOPE
    };
    let binding = membership::bind_live_pool(&pool, &plan.admission_membership)
        .map_err(|e| live_binding_error(e, null_backed_scope))?;
    let projected = binding.bound_membership()?;
    let prior = membership::load_membership(params.paths).ok();
    let recovered =
        materialize_recovered_membership(&binding, &projected, prior.as_ref(), by_id_resolver)?;

    let pre_names: std::collections::BTreeSet<_> = plan
        .journal
        .pre_membership
        .names()
        .map(|n| n.as_str().to_owned())
        .collect();
    let target_names: std::collections::BTreeSet<_> = plan
        .journal
        .target_membership
        .names()
        .map(|n| n.as_str().to_owned())
        .collect();
    let recovered_names: std::collections::BTreeSet<_> =
        recovered.names().map(|n| n.as_str().to_owned()).collect();

    eprintln!("  pre-operation membership:  {:?}", pre_names);
    eprintln!("  target membership:         {:?}", target_names);
    eprintln!("  recovered (live pool):     {:?}", recovered_names);

    eprintln!(
        "note: {}",
        recovery_guidance(
            &plan.journal.op,
            &pre_names,
            &target_names,
            &recovered_names
        )
    );

    membership::save_membership(&recovered, params.paths)?;
    eprintln!("pool.json written from live pool state.");

    if plan.journal.is_bootstrap_add() {
        alert::drop_all_ghost_alert_state(params.paths).map_err(|source| {
            RecoverError::AckCleanupFailed {
                stage: "bootstrap-recovery",
                devids: Vec::new(),
                source,
            }
        })?;
    }

    let _guard = if owed_balance.is_some() {
        Some(
            params
                .sleep_inhibitor
                .acquire("finishing interrupted add balance")
                .map_err(|e| {
                    RecoverError::Failed(format!("could not acquire sleep inhibitor: {e}"))
                })?,
        )
    } else {
        None
    };

    if let Some(owed_balance) = owed_balance {
        replay_owed_raid1_maintenance(
            runner,
            fs,
            &plan.mount_point,
            owed_balance,
            "add",
            params.progress,
        )?;
    }

    if let journal::OpKind::Remove { luks_uuid, .. } = &plan.journal.op
        && recovered.by_uuid(luks_uuid).is_none()
        && let Some(devid) = plan
            .journal
            .pre_membership
            .by_uuid(luks_uuid)
            .and_then(|m| m.devid)
        && let Err(e) = alert::drop_ghost_alert_state_for_devids(params.paths, &[devid])
    {
        eprintln!("Warning: failed to update alert baselines: {e}");
    }

    journal::clear_journal(params.paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
    eprintln!("pending-op.json cleared. Recovery complete.");
    Ok(())
}

/// Format the entry-banner line that both dry-run preview and real-run
/// stderr share. Wording is pinned byte-for-byte against the pre-PR 6
/// `eprintln!` that announced the start of recovery.
pub fn format_recover_entry(journal: &Journal) -> String {
    format!(
        "Recovering from interrupted {:?} operation (started {})...",
        journal_op_label(&journal.op),
        journal.started_at
    )
}

impl RecoverPlan {
    pub fn preview(&self) -> Preview {
        Preview {
            notes: self.notes.clone(),
            steps: self.work_plan.render_steps(),
        }
    }

    pub fn execute<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
        self,
        runner: &R,
        fs: &F,
        by_id_resolver: &dyn ByIdResolver,
        params: &RecoverParams<'_>,
    ) -> Result<(), RecoverError> {
        // Render accumulated notes (entry banner + probe events) to
        // stderr before any mutation. This replaces today's pair of
        // `eprintln!(entry)` + `mount::print_probe_events(&events)`
        // calls with byte-identical output in the same order.
        preview::emit_notes_to_stderr(&self.notes, PerDiskStyle::Bracketed);

        let RecoverPlan {
            notes: _,
            work_plan,
        } = self;
        work_plan.execute(runner, fs, by_id_resolver, params)
    }
}

/// Plan a `braid recover` run. Owns everything above today's real-run
/// mutation body: journal load, admission membership construction,
/// `mount::plan_open_pool`, ProbeEvent-to-PreviewNote conversion,
/// dry-run already-mounted reconciliation, and dry-run step
/// construction (write pool.json / clear pending-op.json, plus
/// compile_open_steps when an initial mount is required).
///
/// On success, accumulated notes (entry banner + probe events) live
/// on `plan.notes` (the single render source for both preview and
/// execute). On failure after accumulating notes, those notes move
/// to `PlanFailure::notes` so the caller can render them to stderr
/// before the error.
pub fn plan_recover<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RecoverParams<'_>,
) -> Result<RecoverPlan, PlanFailure<RecoverError>> {
    // 1. Load journal (required -- nothing to recover if absent). The
    // no-journal failure is a no-context failure by design: nothing has
    // been probed or accumulated yet, so `PlanFailure::notes` stays empty.
    let journal = match journal::load_journal(params.paths) {
        Ok(Some(j)) => j,
        Ok(None) => {
            return Err(PlanFailure::empty(RecoverError::Failed(
                "no pending operation journal found -- nothing to recover".into(),
            )));
        }
        Err(e) => {
            return Err(PlanFailure::empty(RecoverError::Journal(e.to_string())));
        }
    };

    // Entry banner always comes first -- whether the run succeeds, fails at
    // plan_open_pool, or fails at the already-mounted reconciliation.
    let mut notes = vec![PreviewNote::Info(format_recover_entry(&journal))];

    let admission_membership = match recovery_admission_membership(&journal) {
        Ok(membership) => membership,
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, RecoverError::Membership(e)));
        }
    };
    let mut pre_resolved_credential = None;

    // A bootstrap add that never reached mkfs.btrfs has no pool to mount, so
    // it is decided before mount planning, which would refuse it.
    if let journal::OpKind::Add {
        phase: journal::AddPhase::PoolMutation,
        targets,
    } = &journal.op
        && journal.is_bootstrap_add()
    {
        match plan_bootstrap_recovery(runner, fs, params, targets) {
            Ok(BootstrapRecovery::Uncommitted { formatted }) => {
                let work_plan = RecoverWorkPlan {
                    open_plan: None,
                    pre_resolved_credential: None,
                    journal,
                    admission_membership,
                    mount_point: params.config.mount_point().clone(),
                    pool_json_path: params.paths.pool_json(),
                    pending_op_path: params.paths.pending_op_json(),
                    luks_headers_dir: params.paths.luks_headers_dir(),
                    actions: vec![RecoverWorkAction::ClearUncommittedBootstrap { formatted }],
                };
                return Ok(RecoverPlan { notes, work_plan });
            }
            Ok(BootstrapRecovery::Mount { credential, note }) => {
                pre_resolved_credential = credential;
                notes.extend(note);
            }
            Err(e) => return Err(PlanFailure::with_notes(notes, e)),
        }
    }
    let mount_membership = mount_membership_for_recover(&journal, &admission_membership);

    if let journal::OpKind::Add {
        phase: journal::AddPhase::PoolMutation,
        targets,
    } = &journal.op
        && !journal.is_bootstrap_add()
        && !params.dry_run
    {
        // Preflight add-target reconciliation -- one of the planner's two
        // deliberate mutations (ADR 022). `plan_bootstrap_recovery` above is
        // the other; it runs only for a bootstrap journal, so at most one of
        // them runs per recover. Gated by `!dry_run` so preview stays
        // side-effect-free; see `discover_add_targets_before_mount` for why it
        // must precede the mount.
        match discover_add_targets_before_mount(runner, fs, params, &journal, targets) {
            Ok(credential) => pre_resolved_credential = credential,
            Err(e) => {
                return Err(PlanFailure::with_notes(notes, e));
            }
        }
    }

    let report = mount::plan_open_pool(
        runner,
        fs,
        &mount::PlanOpenPoolParams {
            config: params.config,
            paths: params.paths,
            membership: mount_membership,
            backing_path_resolver: params.backing_path_resolver,
            allow_degraded: params.allow_degraded,
            command_hint: "recover",
        },
    );
    for event in &report.events {
        notes.push(event.to_preview_note());
    }

    let open_plan = match report.result {
        Ok(op) => op,
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, recover_mount_error(e)));
        }
    };
    let replace_pool_mutation = is_replace_pool_mutation(&journal.op);

    // Refuse Replace recovery on an already-mounted pool. The cycle that
    // scrubs stale in-memory btrfs_fs_devices after a kernel-resumed
    // dev_replace requires a clean umount-and-remount that we cannot safely
    // perform when an external process holds the mount (EBUSY risk). The
    // staleness is also undetectable from userspace post-resume: btrfs
    // replace status reports `Finished` for both a normally-completed
    // replace and a kernel-resumed replace, so we cannot use it to tell
    // whether the in-memory fs_devices view is fresh.
    //
    // Operator's recovery path: `braid lock` (works with a journal present
    // -- no pending-op preflight in lock.rs) then `braid recover`, which
    // opens its own mount and takes the just_mounted == true cycle path.
    if open_plan.is_none() && replace_pool_mutation {
        return Err(PlanFailure::with_notes(
            notes,
            RecoverError::Failed(
                "recover refuses to probe an already-mounted pool when the journal \
                 records a replace -- the kernel may have resumed an interrupted \
                 dev_replace on this mount session, leaving stale in-memory device \
                 state that probe_pool cannot distinguish from real topology.\n\n\
                 To recover safely, fully cycle the mount yourself first:\n  \
                 sudo braid lock\n\n\
                 braid lock works with a pending-operation journal and unmounts + \
                 closes LUKS so the next recovery attempt can open a fresh mount \
                 session and clear the staleness via the relock cycle."
                    .to_owned(),
            ),
        ));
    }

    let probed_live_pool = if open_plan.is_none() && params.dry_run {
        // Pool is already mounted -- run the same read-only reconciliation
        // validation that execution's later probe_pool loop does. This
        // catches errors like "device X is outside the admission membership"
        // before claiming recovery is ready. Kept dry-run only
        // to preserve today's asymmetry: real-run already-mounted skips
        // this check because it happens implicitly downstream in
        // `execute()` when it walks the probed pool devices.
        let mount_point = params.config.mount_point();
        let pool = match probe::probe_pool(runner, fs, mount_point) {
            Ok(p) => p,
            Err(e) => {
                return Err(PlanFailure::with_notes(notes, e.into()));
            }
        };
        match mount_check::mount_entry_at_via_fs(fs, mount_point.as_str()) {
            Ok(Some(entry)) if mount_check::entry_is_read_only(&entry) => {
                return Err(PlanFailure::with_notes(
                    notes,
                    RecoverError::Failed(format!(
                        "recover dry-run: pool at {mp} is mounted read-only \
                         (vfs_options={:?}, fs_options={:?}) -- execute \
                         would refuse. pool.json and the pending-op journal \
                         are unchanged.\n{hint}",
                        entry.vfs_options,
                        entry.fs_options,
                        mp = mount_point,
                        hint = repair_hint::read_only_pool(
                            mount_point,
                            repair_hint::ReopenWith::Recover
                        ),
                    )),
                ));
            }
            Ok(Some(_)) | Ok(None) => {}
            Err(e) => {
                return Err(PlanFailure::with_notes(
                    notes,
                    RecoverError::Probe(ProbeError::MountInfo(e)),
                ));
            }
        }
        if let Err(e) = validate_live_members_allowed(&pool, &admission_membership) {
            return Err(PlanFailure::with_notes(notes, e));
        }
        Some(pool)
    } else {
        None
    };

    let mut actions = Vec::new();
    if let Some(initial_open_plan) = &open_plan {
        actions.push(RecoverWorkAction::InitialOpenPool);

        if replace_pool_mutation {
            actions.push(RecoverWorkAction::WaitForKernelReplace);
            let mut cycle_reopen_names: Vec<DiskName> = Vec::new();
            for event in &report.events {
                let Some(name) = (match event {
                    mount::ProbeEvent::DiskAvailable { name }
                    | mount::ProbeEvent::DiskAlreadyOpen { name } => Some(name),
                    _ => None,
                }) else {
                    continue;
                };
                let parsed = DiskName::parse(name).map_err(|e| {
                PlanFailure::with_notes(
                    notes.clone(),
                    RecoverError::Failed(format!(
                        "recover remount cycle preview: invalid disk name from mount planner '{name}': {e}"
                    )),
                )
            })?;
                cycle_reopen_names.push(parsed);
            }
            let cycle_close_names: Vec<DiskName> = admission_membership
                .iter()
                .filter_map(|(_, member)| {
                    let name = &member.name;
                    if cycle_reopen_names.contains(name) {
                        return Some(name.clone());
                    }
                    let mapper_path = config::mapper_name(name).dev_path();
                    fs.exists(&mapper_path).then(|| name.clone())
                })
                .collect();

            for name in &cycle_reopen_names {
                if admission_membership.by_name(name).is_none() {
                    return Err(PlanFailure::with_notes(
                        notes,
                        RecoverError::Failed(format!(
                            "recover remount cycle preview: disk '{name}' missing from recovery admission membership"
                        )),
                    ));
                }
            }
            if cycle_reopen_names.is_empty() {
                return Err(PlanFailure::with_notes(
                    notes,
                    RecoverError::Failed(
                        "recover remount cycle preview: no disks available to reopen".into(),
                    ),
                ));
            }
            actions.push(RecoverWorkAction::RemountCycle {
                close_names: cycle_close_names,
                reopen_names: cycle_reopen_names,
                any_missing_member: initial_open_plan.any_missing_member,
            });
        }
    }

    // Each arm names the RAID1 balance its op still owes: live add owes the
    // hard convert it runs after btrfs device add, and remove-missing and
    // replace owe the soft restore only when their journal records one.
    let completion = match &journal.op {
        journal::OpKind::Add {
            phase: journal::AddPhase::PoolMutation,
            targets,
        } if !journal.is_bootstrap_add() => {
            let never_formatted = match never_formatted_add_targets(
                runner,
                fs,
                params.backing_path_resolver,
                targets,
            ) {
                Ok(never_formatted) => never_formatted,
                Err(e) => return Err(PlanFailure::with_notes(notes, e)),
            };
            let committed_targets = match probed_live_pool.as_ref() {
                Some(pool) => match committed_add_targets(pool, &admission_membership, targets) {
                    Ok(committed) => Some(committed),
                    Err(e) => return Err(PlanFailure::with_notes(notes, e)),
                },
                None => None,
            };
            let all_targets_already_live = committed_targets
                .as_ref()
                .is_some_and(|committed| committed.len() == targets.len());
            RecoverCompletion::AddPoolMutation {
                targets: targets.clone(),
                all_targets_already_live,
                committed_targets,
                never_formatted,
                owed_balance: OwedRaid1Balance::Grow,
            }
        }
        journal::OpKind::Add {
            phase: journal::AddPhase::PostAddBalanceRaid1,
            ..
        } => RecoverCompletion::AddPostBalance {
            owed_balance: OwedRaid1Balance::Grow,
        },
        journal::OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PoolMutation,
            devid,
            restore_raid1_after_commit,
        } => RecoverCompletion::RemoveMissingPoolMutation {
            devid: *devid,
            owed_balance: restore_raid1_after_commit.then_some(OwedRaid1Balance::RestoreSingle),
        },
        journal::OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
            devid,
            restore_raid1_after_commit,
        } => RecoverCompletion::RemoveMissingPostMaintenance {
            devid: *devid,
            owed_balance: restore_raid1_after_commit.then_some(OwedRaid1Balance::RestoreSingle),
        },
        journal::OpKind::Replace(journal::ReplaceOp {
            phase: journal::ReplacePhase::PoolMutation,
            identity,
            new_name,
            new_target,
            source,
            restore_raid1_after_commit,
            ..
        }) => RecoverCompletion::ReplacePoolMutation {
            identity: identity.clone(),
            new_name: new_name.clone(),
            new_target: new_target.clone(),
            source: source.clone(),
            uncommitted_writes: UncommittedReplaceWrites::plan(new_name, new_target),
            owed_balance: restore_raid1_after_commit.then_some(OwedRaid1Balance::RestoreSingle),
        },
        journal::OpKind::Replace(journal::ReplaceOp {
            phase: journal::ReplacePhase::PostReplaceMaintenance,
            identity,
            new_name,
            source,
            restore_raid1_after_commit,
            ..
        }) => RecoverCompletion::ReplacePostMaintenance {
            new_uuid: identity.new_uuid().clone(),
            new_name: new_name.clone(),
            source: source.clone(),
            owed_balance: restore_raid1_after_commit.then_some(OwedRaid1Balance::RestoreSingle),
        },
        journal::OpKind::Add { targets, .. } => RecoverCompletion::GenericLivePool {
            // Reached only by bootstrap add -- live PoolMutation routes to
            // AddPoolMutation and PostAddBalanceRaid1 to AddPostBalance, so the
            // grow convert those paths own never applies here. A 2+-disk
            // bootstrap already ran `mkfs.btrfs -d raid1 -m raid1`, so this soft
            // replay is an idempotent near-no-op kept as cheap defense-in-depth;
            // do not assume mkfs leaves zero convertible chunks, and do not drop
            // it. A 1-disk bootstrap ran plain mkfs, which cannot make RAID1, so
            // it owes nothing.
            owed_balance: (targets.len() >= 2).then_some(OwedRaid1Balance::RestoreSingle),
        },
        journal::OpKind::Remove { .. } => RecoverCompletion::GenericLivePool {
            // No resume, no replay. A 2->1 `braid remove` converts to
            // `ONE_DISK_PROFILES` before its device remove (as does the
            // remove-missing collapse, whose arm above also never resumes
            // it), so a paused balance observed here may belong to an
            // unfinished pre-remove rather than to a post-mutation rebalance. Resuming it would complete the
            // conversion-to-single without removing the device, then journal
            // clear would silently halve redundancy. The recovery_guidance
            // message directs the operator to re-run `braid remove` instead,
            // which handles every shape (2->1 pre-balance, 3->2 / 4->3 with no
            // pre-balance) correctly.
            owed_balance: None,
        },
    };
    actions.push(RecoverWorkAction::Complete(Box::new(completion)));

    let work_plan = RecoverWorkPlan {
        open_plan,
        pre_resolved_credential,
        journal,
        admission_membership,
        mount_point: params.config.mount_point().clone(),
        pool_json_path: params.paths.pool_json(),
        pending_op_path: params.paths.pending_op_json(),
        luks_headers_dir: params.paths.luks_headers_dir(),
        actions,
    };
    Ok(RecoverPlan { notes, work_plan })
}

/// Plan-then-execute interrupted-operation recovery; dry-run renders the plan.
/// Planning loads the journal, resolves admission membership, and plans mount/open;
/// execute verifies credentials and replays journal actions.
pub fn cmd_recover<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
) -> Result<(), RecoverError> {
    cmd_recover_inner(runner, fs, by_id_resolver, params).map_err(|error| {
        RecoverError::JournalLifecycle {
            source: Box::new(error),
            advice: journal::recovery_error_advice(params.paths),
        }
    })
}

fn cmd_recover_inner<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
) -> Result<(), RecoverError> {
    let plan = match plan_recover(runner, fs, params) {
        Ok(p) => p,
        Err(PlanFailure { notes, error }) => {
            // Preserved-context failure: any accumulated notes (entry
            // banner + per-disk probe events) render to stderr before
            // the error, mirroring today's `eprintln!(entry)` +
            // `mount::print_probe_events` + `?` sequence.
            preview::emit_notes_to_stderr(&notes, PerDiskStyle::Bracketed);
            return Err(error);
        }
    };

    if params.dry_run {
        plan.preview().print_colored();
        return Ok(());
    }

    plan.execute(runner, fs, by_id_resolver, params)
}

/// Preserve recovery context in mount-time identity failures without changing
/// the shared mount planner's messages for normal unlock/status callers.
fn recover_mount_error(error: MountError) -> RecoverError {
    match error {
        MountError::Failed(message)
            if message.contains("LUKS UUID mismatch")
                && !message.contains("preserving pending-op.json") =>
        {
            RecoverError::Failed(format!(
                "{message}\nrecover aborted -- preserving pending-op.json"
            ))
        }
        other => RecoverError::Mount(other),
    }
}

fn is_replace_pool_mutation(op: &journal::OpKind) -> bool {
    matches!(
        op,
        journal::OpKind::Replace(journal::ReplaceOp {
            phase: journal::ReplacePhase::PoolMutation,
            ..
        })
    )
}

/// Bind `pool` for a phased topology check. A foreign live device means the
/// pool shows some other topology, so it reads as `None` and each caller
/// reports the mismatch in its own wording; contradictions still refuse.
fn bind_for_topology<'a>(
    pool: &'a PoolState,
    reference: &'a PoolMembership,
) -> Result<Option<LiveBinding<'a>>, RecoverError> {
    match membership::bind_live_pool(pool, reference) {
        Ok(binding) => Ok(Some(binding)),
        Err(LiveBindingError::Foreign { .. }) => Ok(None),
        Err(e) => Err(live_binding_error(e, ADMISSION_SCOPE)),
    }
}

/// The phased reading of a binding: its projection when every reference
/// member is bound, else `None`. The projection runs first, so an ambiguous
/// MISSING devid refuses as such rather than as a topology mismatch.
fn fully_bound_membership(
    binding: &LiveBinding<'_>,
) -> Result<Option<PoolMembership>, RecoverError> {
    let projected = binding.bound_membership()?;
    Ok(binding.all_bound().then_some(projected))
}

/// Yes/no topology check for phases that only need to know which topology
/// the live pool shows (decision 024 rule 9 binding).
fn live_pool_matches_membership(
    pool: &PoolState,
    membership: &PoolMembership,
) -> Result<bool, RecoverError> {
    let Some(binding) = bind_for_topology(pool, membership)? else {
        return Ok(false);
    };
    Ok(fully_bound_membership(&binding)?.is_some())
}

/// The membership recovery writes for a phase whose live pool must show
/// exactly `expected`, or `None` when it shows some other topology.
fn recover_membership_matching_expected(
    pool: &PoolState,
    expected: &PoolMembership,
    prior: Option<&PoolMembership>,
    by_id_resolver: &dyn ByIdResolver,
) -> Result<Option<PoolMembership>, RecoverError> {
    let Some(binding) = bind_for_topology(pool, expected)? else {
        return Ok(None);
    };
    let Some(projected) = fully_bound_membership(&binding)? else {
        return Ok(None);
    };
    materialize_recovered_membership(&binding, &projected, prior, by_id_resolver).map(Some)
}

/// Turn a binding's projection into the `pool.json` recovery writes. A
/// present member takes the by-id path its live backing disk resolves to; a
/// null-backed or MISSING member has no live backing disk, so it keeps the
/// journaled path. `added_at` follows `resolve_added_at`.
fn materialize_recovered_membership(
    binding: &LiveBinding<'_>,
    projected: &PoolMembership,
    prior: Option<&PoolMembership>,
    by_id_resolver: &dyn ByIdResolver,
) -> Result<PoolMembership, RecoverError> {
    let mut recovered = PoolMembership::empty();
    for (uuid, member) in projected.iter() {
        let by_id = match binding.member(uuid) {
            Some(MemberBinding::Present(dev)) => {
                resolve_by_id_for_underlying(by_id_resolver, &dev.underlying)?
            }
            _ => member.by_id.clone(),
        };
        recovered.insert(
            uuid.clone(),
            DiskMember {
                by_id,
                added_at: resolve_added_at(prior, member, uuid),
                ..member.clone()
            },
        )?;
    }
    Ok(recovered)
}

/// Scope named when a null-backed mapper records a UUID the recovery
/// admission membership lacks.
const ADMISSION_SCOPE: &str = "recovery admission membership for this phase";

/// Map a binding refusal to recover's wording. `null_backed_scope` names the
/// membership a foreign null-backed mapper is missing from; a foreign present
/// device keeps the admission wording every recovery path shares.
fn live_binding_error(err: LiveBindingError, null_backed_scope: &str) -> RecoverError {
    match err {
        LiveBindingError::Foreign {
            mapper,
            luks_uuid,
            null_backed: false,
        } => foreign_live_device_not_admitted(&mapper, &luks_uuid),
        LiveBindingError::Foreign {
            mapper,
            luks_uuid,
            null_backed: true,
        } => RecoverError::Failed(format!(
            "device {mapper} (LUKS UUID {luks_uuid}) is in the live pool with no backing disk \
             but is not part of the {null_backed_scope}."
        )),
        LiveBindingError::DuplicateDevid { devid, members } => {
            RecoverError::DuplicateDevidDuringReplay { devid, members }
        }
        e @ (LiveBindingError::DuplicateLiveUuid { .. }
        | LiveBindingError::DuplicateLiveDevid { .. }) => RecoverError::Failed(format!(
            "recovery refuses a contradictory live pool: {e}; preserving pending-op.json"
        )),
    }
}

fn write_remove_missing_phase(
    paths: &StatePaths,
    journal: &Journal,
    phase: journal::RemoveMissingPhase,
    target_membership: Option<PoolMembership>,
) -> Result<Journal, RecoverError> {
    let journal::OpKind::RemoveMissing {
        devid,
        restore_raid1_after_commit,
        ..
    } = &journal.op
    else {
        return Err(RecoverError::Journal(
            "cannot advance non-remove-missing journal".into(),
        ));
    };
    journal::rewrite_journal(
        paths,
        journal,
        journal::OpKind::RemoveMissing {
            phase,
            devid: *devid,
            restore_raid1_after_commit: *restore_raid1_after_commit,
        },
        target_membership,
    )
    .map_err(|e| RecoverError::Journal(e.to_string()))
}

fn write_replace_phase(
    paths: &StatePaths,
    journal: &Journal,
    phase: journal::ReplacePhase,
    target_membership: Option<PoolMembership>,
) -> Result<Journal, RecoverError> {
    let journal::OpKind::Replace(op) = &journal.op else {
        return Err(RecoverError::Journal(
            "cannot advance non-replace journal".into(),
        ));
    };
    journal::rewrite_journal(
        paths,
        journal,
        journal::OpKind::Replace(journal::ReplaceOp {
            phase,
            ..op.clone()
        }),
        target_membership,
    )
    .map_err(|e| RecoverError::Journal(e.to_string()))
}

/// Replay the RAID1 balance `owed_balance` that the interrupted `label` op
/// still owes. An idle btrfs balance is a precondition: anything else fails
/// closed with the journal kept. The balance itself runs behind the live
/// restore's gated step, so recover balances only when the pool can host
/// RAID1 now, and on a skip the caller still clears the journal, as the live
/// path does.
fn replay_owed_raid1_maintenance<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    owed_balance: OwedRaid1Balance,
    label: &str,
    progress: ProgressOutput,
) -> Result<(), RecoverError> {
    let color_enabled = color_enabled_for_stderr();
    match get_balance_report(runner, mount_point) {
        BalanceReport::Idle => {}
        BalanceReport::Paused { .. } => {
            return Err(RecoverError::Failed(format!(
                "recover found a paused btrfs balance at {mount_point} before post-{label} \
                 RAID1 replay. Automatic recovery is unsafe for crash-paused owed RAID1 \
                 maintenance; preserving pending-op.json. Inspect btrfs manually before \
                 clearing recovery state."
            )));
        }
        BalanceReport::Running { .. } => {
            return Err(RecoverError::Failed(format!(
                "recover found a running btrfs balance at {mount_point} before post-{label} \
                 RAID1 replay. Automatic recovery requires an idle btrfs balance before \
                 owed RAID1 replay; preserving pending-op.json. Inspect btrfs manually \
                 before clearing recovery state."
            )));
        }
        BalanceReport::Unknown => {
            return Err(RecoverError::Failed(format!(
                "recover could not determine btrfs balance state at {mount_point} before \
                 post-{label} RAID1 replay. Automatic recovery requires an idle btrfs \
                 balance before owed RAID1 replay; preserving pending-op.json. Inspect \
                 btrfs manually before clearing recovery state."
            )));
        }
    }

    let (balance_name, chunk_rule) = match owed_balance {
        OwedRaid1Balance::Grow => ("RAID1 balance", "rewrite every chunk"),
        OwedRaid1Balance::RestoreSingle => ("RAID1 soft balance", "skip already-RAID1 chunks"),
    };
    let outcome = pool::balance_if_pool_hosts_raid1(runner, fs, mount_point, |host| {
        eprint!(
            "{}",
            status_line(
                StatusTag::Wait,
                color_enabled,
                &format!("pool: replaying post-{label} {balance_name} ({chunk_rule})..."),
            )
        );
        owed_balance.run(runner, host, progress)?;
        eprint!(
            "{}",
            status_line(
                StatusTag::Ok,
                color_enabled,
                &format!("pool: {balance_name} replay complete"),
            )
        );
        Ok(())
    })
    .map_err(|e| RecoverError::Failed(format!("recover balance replay: {e}")))?;
    match outcome {
        Raid1BalanceOutcome::Balanced => {}
        Raid1BalanceOutcome::Skipped(Raid1HostRefusal::Degraded) => {
            owed_balance.emit_skipped();
        }
        // A whole one-device pool has no second disk to mirror onto, so the
        // replay skips it silently.
        Raid1BalanceOutcome::Skipped(Raid1HostRefusal::TooFewDevices) => {}
    }
    Ok(())
}

fn journal_op_label(op: &journal::OpKind) -> &'static str {
    match op {
        journal::OpKind::Add { .. } => "add",
        journal::OpKind::Remove { .. } => "remove",
        journal::OpKind::RemoveMissing { .. } => "remove-missing",
        journal::OpKind::Replace(_) => "replace",
    }
}

fn live_member_uuids(pool: &PoolState) -> std::collections::BTreeSet<LuksUuid> {
    pool.devices
        .iter()
        .map(|dev| dev.luks_uuid.clone())
        .collect()
}

/// Foreign-device rejection for a present device whose LUKS UUID the
/// phase-aware admission membership lacks. Every recovery path that refuses a
/// foreign present device uses this wording, since recover admits different
/// journal snapshots in different phases.
fn foreign_live_device_not_admitted(mapper: &MapperName, uuid: &LuksUuid) -> RecoverError {
    RecoverError::Failed(format!(
        "device {mapper} (LUKS UUID {uuid}) is in the live pool but has no journaled by-id \
         binding in the recovery admission membership for this phase.\n\
         This must be resolved manually -- provide the correct \
         /dev/disk/by-id/ path."
    ))
}

/// Present-device admission check, run before recovery mutates anything.
/// It stays present-only: binding the replace union here could refuse a
/// sound pool, because the old and new disk may both record the old devid.
fn validate_live_members_allowed(
    pool: &PoolState,
    allowed: &PoolMembership,
) -> Result<(), RecoverError> {
    match pool
        .devices
        .iter()
        .find(|dev| allowed.by_uuid(&dev.luks_uuid).is_none())
    {
        Some(dev) => Err(foreign_live_device_not_admitted(
            &dev.mapper,
            &dev.luks_uuid,
        )),
        None => Ok(()),
    }
}

/// The journaled add targets the live pool already holds, read from the one
/// binding (decision 024 rule 9): a target whose LUKS UUID an open mapper
/// records, present or null-backed, is committed and must not be replayed.
/// Only `PoolMutation` calls this, and
/// `cli/src/add.rs#AddPlan::execute_inner` journals every target there with
/// no devid. So a committed target that btrfs reports only as a MISSING
/// devid never binds here: its replay refuses at the absent disk until it
/// returns. Each probe of an add recovery goes through here, so it is also
/// the per-probe foreign-device gate.
fn committed_add_targets(
    pool: &PoolState,
    union: &PoolMembership,
    targets: &LuksUuidMap<journal::AddJournalTarget>,
) -> Result<std::collections::BTreeSet<LuksUuid>, RecoverError> {
    let binding = membership::bind_live_pool(pool, union)
        .map_err(|e| live_binding_error(e, ADMISSION_SCOPE))?;
    Ok(targets
        .keys()
        .filter(|uuid| binding.member(uuid).is_some_and(|b| b.is_bound()))
        .cloned()
        .collect())
}

/// The devid btrfs holds each journaled add target at, or `None` when some
/// target is not bound.
fn committed_add_target_devids(
    binding: &LiveBinding<'_>,
    targets: &LuksUuidMap<journal::AddJournalTarget>,
) -> Option<Vec<Devid>> {
    targets
        .keys()
        .map(|uuid| binding.member(uuid).and_then(|b| b.devid()))
        .collect()
}

/// Refusal when the live pool does not show the committed add membership.
/// `stage` keeps the post-add wording operators already know.
fn add_membership_mismatch(
    stage: &str,
    expected: &PoolMembership,
    pool: &PoolState,
    binding: Option<&LiveBinding<'_>>,
) -> RecoverError {
    let expected: std::collections::BTreeSet<&LuksUuid> = expected.iter().map(|(u, _)| u).collect();
    let found: std::collections::BTreeSet<&LuksUuid> = match binding {
        Some(binding) => expected
            .iter()
            .copied()
            .filter(|uuid| binding.member(uuid).is_some_and(|b| b.is_bound()))
            .collect(),
        None => pool.devices.iter().map(|dev| &dev.luks_uuid).collect(),
    };
    let live_devices = pool
        .devices
        .iter()
        .map(|dev| format!("{} ({})", dev.mapper, dev.luks_uuid))
        .chain(
            pool.null_underlying
                .iter()
                .map(|dev| format!("{} ({}, no backing disk)", dev.mapper, dev.luks_uuid)),
        )
        .chain(
            pool.devid_only_missing()
                .map(|devid| format!("devid {devid} (MISSING)")),
        )
        .collect::<Vec<_>>()
        .join(", ");
    RecoverError::Failed(format!(
        "{stage} expected live pool membership {expected:?}, found {found:?}; live devices: \
         {live_devices}"
    ))
}

/// The membership an add recovery writes once the add has committed: every
/// member of `committed` bound to the live pool. Anything else refuses with
/// the journal kept.
fn committed_add_membership(
    stage: &str,
    binding: &LiveBinding<'_>,
    pool: &PoolState,
    committed: &PoolMembership,
    prior: Option<&PoolMembership>,
    by_id_resolver: &dyn ByIdResolver,
) -> Result<PoolMembership, RecoverError> {
    let Some(projected) = fully_bound_membership(binding)? else {
        return Err(add_membership_mismatch(
            stage,
            committed,
            pool,
            Some(binding),
        ));
    };
    materialize_recovered_membership(binding, &projected, prior, by_id_resolver)
}

/// Advance an add journal to `phase` with `target` as its target snapshot.
/// At the PostAddBalanceRaid1 advance `target` is the membership just saved
/// to `pool.json`, so a later rerun binds members at the devids btrfs gave
/// them, not the devid-less snapshot the add started from.
fn write_add_phase(
    paths: &StatePaths,
    journal: &Journal,
    phase: journal::AddPhase,
    target: PoolMembership,
) -> Result<Journal, RecoverError> {
    let journal::OpKind::Add { targets, .. } = &journal.op else {
        return Err(RecoverError::Journal(
            "cannot advance non-add journal".into(),
        ));
    };
    journal::rewrite_journal(
        paths,
        journal,
        journal::OpKind::Add {
            phase,
            targets: targets.clone(),
        },
        Some(target),
    )
    .map_err(|e| RecoverError::Journal(e.to_string()))
}

/// The single place recover refuses a key-file credential; recover has not
/// exposed `--key-file` yet, so every no-prompt passphrase boundary funnels
/// through this guard until that policy changes.
fn open_credential_passphrase<'a>(
    credential: &'a OpenCredential,
    context: &str,
) -> Result<&'a Passphrase, RecoverError> {
    match credential {
        OpenCredential::Passphrase(passphrase) => Ok(passphrase),
        OpenCredential::KeyFile(_) => Err(RecoverError::Failed(format!(
            "{context} requires a passphrase"
        ))),
    }
}

/// Resolves the passphrase recover drives `cryptsetup` with, preserving the
/// single-passphrase invariant across already-open credentials and delayed
/// prompt paths.
fn recover_passphrase<'a>(
    existing: Option<&'a OpenCredential>,
    params: &RecoverParams<'_>,
    context: &str,
) -> Result<RecoverPassphrase<'a>, RecoverError> {
    match existing {
        Some(credential) => Ok(RecoverPassphrase::Borrowed(open_credential_passphrase(
            credential, context,
        )?)),
        None => Ok(RecoverPassphrase::Owned(luks::read_passphrase_with(
            params.passphrase_file,
            params.passphrase_stdin,
            false,
            params.tty,
        )?)),
    }
}

fn add_recovery_uuid_mismatch_message(
    by_id: &ByIdPath,
    expected_uuid: &LuksUuid,
    observed_uuid: &LuksUuid,
    target_state: &str,
) -> String {
    format!(
        "add recovery aborted: target {by_id} LUKS UUID mismatch -- journaled {expected_uuid}, observed {observed_uuid} ({target_state}); the disk at this by-id was reformatted out-of-band between crash and recovery (see docs/guides/recovery-scenarios.md)"
    )
}

/// The journaled operation a recover target belongs to, so the shared
/// absent-target refusal names the operation the operator ran.
#[derive(Debug, Clone, Copy)]
enum RecoverTargetOp {
    Add,
    Replace,
}

impl RecoverTargetOp {
    fn as_str(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Replace => "replace",
        }
    }
}

/// Presence boundary for the replay sites that refuse a journaled target
/// instead of skipping it: each one refuses an unplugged disk with this one
/// message and never reports it as a non-LUKS disk. The discovery passes call
/// `probe::probe_config_disk` directly because they skip absent targets on
/// purpose.
fn probe_present_recover_target<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    op: RecoverTargetOp,
    name: &DiskName,
    by_id: &ByIdPath,
    backing_path_resolver: &dyn BackingPathResolver,
) -> Result<PresentConfigDiskState, RecoverError> {
    let probed = probe::probe_config_disk(runner, fs, name, by_id, backing_path_resolver)?;
    match PresentConfigDisk::try_from(probed) {
        Ok(present) => Ok(present.state),
        // No rerun step here: `cmd_recover` appends one from a fresh read of
        // the journal.
        Err(absent) => Err(RecoverError::Failed(format!(
            "recover {} target '{}' ({}) is not present; reconnect it",
            op.as_str(),
            absent.name,
            absent.by_id_path
        ))),
    }
}

/// How recover finishes an interrupted bootstrap add, decided before mount
/// planning by `plan_bootstrap_recovery`.
enum BootstrapRecovery {
    /// mkfs.btrfs never ran, so there is no pool: clear the journal.
    /// `formatted` names the disks luksFormat wrote, sorted by name.
    Uncommitted {
        formatted: Vec<(DiskName, ByIdPath)>,
    },
    /// mkfs.btrfs may have run: open and mount the pool. `credential` is the
    /// one the btrfs check resolved, so the mount does not prompt again.
    Mount {
        credential: Option<OpenCredential>,
        note: Option<PreviewNote>,
    },
}

/// The bootstrap rule. mkfs.btrfs is the bootstrap commit point, and add runs
/// it only after it formats every target, so a target with no LUKS header, or
/// formatted targets with no btrfs among them, prove mkfs never ran. A pool
/// mounted at the mount point proves it did. Every target is probed: an
/// absent one, or a LUKS one without the braid label and journaled UUID,
/// refuses with the journal kept. Telling formatted targets from a pool needs
/// them open, so outside --dry-run this opens each closed formatted target,
/// checks its mapper for btrfs, and closes what it opened. Like
/// `discover_add_targets_before_mount`, that is a deliberate planner-time
/// mutation (ADR 022).
fn plan_bootstrap_recovery<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RecoverParams<'_>,
    targets: &LuksUuidMap<journal::AddJournalTarget>,
) -> Result<BootstrapRecovery, RecoverError> {
    let mount_path = BootstrapRecovery::Mount {
        credential: None,
        note: None,
    };
    if pool_mount::confirmed_mount(runner, params.config.mount_point())?.is_some() {
        return Ok(mount_path);
    }
    // Bootstrap add formats every target; a journal that says otherwise is
    // not one this rule can read, so it takes the mount path unchanged.
    if targets
        .iter()
        .any(|(_, target)| !matches!(target.mode, journal::AddJournalMode::FreshLuks { .. }))
    {
        return Ok(mount_path);
    }

    let mut sorted: Vec<(&LuksUuid, &journal::AddJournalTarget)> = targets.iter().collect();
    sorted.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    let mut formatted: Vec<(&journal::AddJournalTarget, bool)> = Vec::new();
    for (target_uuid, target) in sorted {
        let state = probe_present_recover_target(
            runner,
            fs,
            RecoverTargetOp::Add,
            &target.name,
            &target.by_id,
            params.backing_path_resolver,
        )?;
        let PresentConfigDiskState::PresentLuks {
            uuid,
            label,
            mapper_open,
        } = state
        else {
            continue;
        };
        let expected_label = config::luks_label_for(&target.name);
        if label.as_deref() != Some(expected_label.as_str()) {
            return Err(RecoverError::Failed(format!(
                "recover add target '{}' has unexpected LUKS label",
                target.name
            )));
        }
        if &uuid != target_uuid {
            return Err(RecoverError::Failed(add_recovery_uuid_mismatch_message(
                &target.by_id,
                target_uuid,
                &uuid,
                "fresh-luks",
            )));
        }
        formatted.push((target, mapper_open));
    }
    let formatted_names = |formatted: &[(&journal::AddJournalTarget, bool)]| {
        formatted
            .iter()
            .map(|(target, _)| (target.name.clone(), target.by_id.clone()))
            .collect()
    };
    if formatted.is_empty() {
        return Ok(BootstrapRecovery::Uncommitted {
            formatted: Vec::new(),
        });
    }
    if params.dry_run {
        return Ok(BootstrapRecovery::Mount {
            credential: None,
            note: Some(PreviewNote::Info(
                "dry-run does not open the formatted bootstrap disks; if none of them \
                 carries btrfs, recover clears pending-op.json instead of mounting"
                    .to_owned(),
            )),
        });
    }

    let mut credential: Option<OpenCredential> = None;
    let mut opened: Vec<TrackedMapper> = Vec::new();
    let check = (|| -> Result<bool, RecoverError> {
        let mut any_btrfs = false;
        for (target, mapper_open) in &formatted {
            let mapper = config::mapper_name(&target.name);
            if !mapper_open {
                if credential.is_none() {
                    credential = Some(
                        credential::resolve_credential(
                            params.passphrase_stdin,
                            params.passphrase_file,
                            None,
                        )
                        .map_err(|e| RecoverError::Failed(format!("recover: {e}")))?,
                    );
                }
                let passphrase = open_credential_passphrase(
                    credential.as_ref().expect("credential was resolved above"),
                    "bootstrap add recovery",
                )?;
                if luks::ensure_luks_open(
                    runner,
                    fs,
                    &target.name,
                    &target.by_id,
                    params.backing_path_resolver,
                    passphrase,
                )? == luks::OpenOutcome::Opened
                {
                    opened.push(TrackedMapper {
                        name: target.name.clone(),
                        mapper: mapper.clone(),
                    });
                }
            }
            // Only a clear "no btrfs" counts as unwritten; a btrfs signature,
            // an unclear probe, or a probe failure takes the mount path.
            let no_btrfs = runner
                .run(&CmdRequest::BtrfsFilesystemShowTarget {
                    target: mapper.dev_path(),
                })
                .is_ok_and(|raw| matches!(classify_btrfs_probe(&raw), DeviceBtrfsProbe::NoBtrfs));
            if !no_btrfs {
                any_btrfs = true;
            }
        }
        Ok(any_btrfs)
    })();
    let closed = mount::close_opened_mappers(
        runner,
        params.sleeper,
        fs,
        &opened,
        color_enabled_for_stderr(),
    );
    if check? {
        return Ok(BootstrapRecovery::Mount {
            credential,
            note: None,
        });
    }
    // The journal is cleared next, so a mapper left open here would sit on a
    // disk the operator is told to wipe.
    closed.map_err(|e| {
        RecoverError::Failed(format!(
            "bootstrap add recovery could not close a mapper it opened to check for btrfs: {e}"
        ))
    })?;
    Ok(BootstrapRecovery::Uncommitted {
        formatted: formatted_names(&formatted),
    })
}

/// The fresh add targets whose disk reads as not LUKS now: luksFormat never
/// wrote them. `plan_recover` records them so the dry-run shows each drop,
/// and `drop_never_formatted_add_targets` probes each again before dropping.
fn never_formatted_add_targets<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    backing_path_resolver: &dyn BackingPathResolver,
    targets: &LuksUuidMap<journal::AddJournalTarget>,
) -> Result<std::collections::BTreeSet<LuksUuid>, RecoverError> {
    let mut never_formatted = std::collections::BTreeSet::new();
    for (uuid, target) in targets {
        if !matches!(target.mode, journal::AddJournalMode::FreshLuks { .. }) {
            continue;
        }
        let probed = probe::probe_config_disk(
            runner,
            fs,
            &target.name,
            &target.by_id,
            backing_path_resolver,
        )?;
        if matches!(probed.state, ConfigDiskState::PresentNotLuks) {
            never_formatted.insert(uuid.clone());
        }
    }
    Ok(never_formatted)
}

/// Preflight for an interrupted existing-pool add: may open closed validated
/// add-targets -- resolving the unlock credential once when it does -- and scan
/// them before mount. One of the two `!dry_run`-gated exceptions to Decision
/// 022; `plan_bootstrap_recovery` is the other.
fn discover_add_targets_before_mount<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RecoverParams<'_>,
    journal: &Journal,
    targets: &LuksUuidMap<journal::AddJournalTarget>,
) -> Result<Option<OpenCredential>, RecoverError> {
    // An already-mounted pool needs no pre-mount discovery. Any other mount
    // at the mount point refuses here, before a target opens.
    if let Some(mount) = pool_mount::confirmed_mount(runner, params.config.mount_point())? {
        return match mount.classify(fs, params.paths) {
            MountedAt::Pool(_) => Ok(None),
            MountedAt::Foreign(foreign) => Err(recover_mount_error(foreign.into())),
            MountedAt::Unknown(unproven) => Err(recover_mount_error(unproven.into())),
        };
    }

    let mut credential: Option<OpenCredential> = None;
    for (target_uuid, target) in targets {
        if journal.pre_membership.by_uuid(target_uuid).is_some() {
            continue;
        }

        let probed = probe::probe_config_disk(
            runner,
            fs,
            &target.name,
            &target.by_id,
            params.backing_path_resolver,
        )?;
        let ConfigDiskState::PresentLuks {
            uuid,
            label,
            mapper_open,
        } = probed.state
        else {
            continue;
        };

        match &target.mode {
            journal::AddJournalMode::RecoverableBraidLabeled { .. } => {
                if &uuid != target_uuid {
                    continue;
                }
            }
            journal::AddJournalMode::FreshLuks { .. } => {
                let expected_label = config::luks_label_for(&target.name);
                if label.as_deref() != Some(expected_label.as_str()) {
                    continue;
                }
                if &uuid != target_uuid {
                    return Err(RecoverError::Failed(add_recovery_uuid_mismatch_message(
                        &target.by_id,
                        target_uuid,
                        &uuid,
                        "fresh-luks",
                    )));
                }
            }
        }

        if !mapper_open {
            if credential.is_none() {
                credential = Some(
                    credential::resolve_credential(
                        params.passphrase_stdin,
                        params.passphrase_file,
                        None,
                    )
                    .map_err(|e| RecoverError::Failed(format!("recover: {e}")))?,
                );
            }
            let passphrase = open_credential_passphrase(
                credential.as_ref().expect("credential was resolved above"),
                "add recovery pre-mount discovery",
            )?;
            luks::ensure_luks_open(
                runner,
                fs,
                &target.name,
                &target.by_id,
                params.backing_path_resolver,
                passphrase,
            )?;
        }

        let mapper = config::mapper_name(&target.name);
        scan_mapper_if_btrfs_visible(runner, &mapper.dev_path())?;
    }

    Ok(credential)
}

/// Verify the recovery passphrase against the live members and every add
/// target the replay will touch, before any replay mutation. `pending` holds
/// only the targets the live binding found uncommitted.
fn verify_recover_passphrase_for_add_replay<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    pool: &PoolState,
    membership: &PoolMembership,
    pending: &[(&LuksUuid, &journal::AddJournalTarget)],
    backing_path_resolver: &dyn BackingPathResolver,
    passphrase: &Passphrase,
) -> Result<(), RecoverError> {
    let mut verify_targets: Vec<_> = pool
        .devices
        .iter()
        .map(|device| CredentialVerifyTarget::existing_pool_member(membership, device))
        .collect();
    if verify_targets.is_empty() {
        return Err(RecoverError::Failed(
            "cannot verify add recovery passphrase because no live pool members were found".into(),
        ));
    }

    for &(target_uuid, target) in pending {
        let probed = probe::probe_config_disk(
            runner,
            fs,
            &target.name,
            &target.by_id,
            backing_path_resolver,
        )?;
        let ConfigDiskState::PresentLuks { uuid, label, .. } = probed.state else {
            continue;
        };
        match &target.mode {
            journal::AddJournalMode::RecoverableBraidLabeled { .. } => {
                if &uuid != target_uuid {
                    return Err(RecoverError::Failed(format!(
                        "recover add target '{}' LUKS UUID mismatch: expected {}, found {}",
                        target.name, target_uuid, uuid
                    )));
                }
            }
            journal::AddJournalMode::FreshLuks { .. } => {
                let expected_label = config::luks_label_for(&target.name);
                if label.as_deref() != Some(expected_label.as_str()) {
                    return Err(RecoverError::Failed(format!(
                        "recover add target '{}' has unexpected LUKS label",
                        target.name
                    )));
                }
                if &uuid != target_uuid {
                    return Err(RecoverError::Failed(add_recovery_uuid_mismatch_message(
                        &target.by_id,
                        target_uuid,
                        &uuid,
                        "fresh-luks",
                    )));
                }
            }
        }
        verify_targets.push(CredentialVerifyTarget::named_candidate(
            &target.name,
            &target.by_id,
        ));
    }

    verify_credential_for_targets(
        runner,
        &verify_targets,
        Credential::Passphrase(passphrase),
        color_enabled_for_stderr(),
        |line| eprint!("{line}"),
    )
    .map_err(|e| match e {
        crate::credential_verify::CredentialVerifyError::Rejected { target } => {
            RecoverError::Failed(format!(
                "recover add passphrase was rejected by '{}'",
                target.name()
            ))
        }
        crate::credential_verify::CredentialVerifyError::Luks { target, source } => {
            RecoverError::Failed(format!(
                "recover add credential verification failed on '{}': {source}",
                target.name()
            ))
        }
    })
}

fn visible_btrfs_fsid<R: CommandRunner>(
    runner: &R,
    mapper_path: &str,
) -> Result<Option<Fsid>, RecoverError> {
    let show_raw = runner.run(&CmdRequest::BtrfsFilesystemShowTarget {
        target: mapper_path.to_owned(),
    })?;
    match classify_btrfs_probe(&show_raw) {
        DeviceBtrfsProbe::NoBtrfs => Ok(None),
        DeviceBtrfsProbe::Unknown(msg) => Err(RecoverError::Failed(msg)),
        DeviceBtrfsProbe::HasBtrfs => {
            let show = crate::parse::parse_btrfs_filesystem_show(&show_raw)?;
            Ok(show.uuid)
        }
    }
}

fn scan_mapper<R: CommandRunner>(runner: &R, mapper_path: &str) -> Result<(), RecoverError> {
    let result = runner.run(&CmdRequest::BtrfsDeviceScan {
        device: mapper_path.to_owned(),
    })?;
    if result.exit_status != 0 {
        return Err(RecoverError::Failed(format!(
            "btrfs device scan failed for {} (exit {}): {}",
            mapper_path,
            result.exit_status,
            result.stderr.trim()
        )));
    }
    Ok(())
}

fn scan_mapper_if_btrfs_visible<R: CommandRunner>(
    runner: &R,
    mapper_path: &str,
) -> Result<bool, RecoverError> {
    if visible_btrfs_fsid(runner, mapper_path)?.is_none() {
        return Ok(false);
    }
    scan_mapper(runner, mapper_path)?;
    Ok(true)
}

fn ensure_keyfile_enrolled<R: CommandRunner>(
    runner: &R,
    device: &str,
    passphrase: &Passphrase,
    key_file: &KeyFilePath,
) -> Result<(), RecoverError> {
    luks::validate_user_keyfile_path(key_file.as_path())?;
    match luks::verify_key_file(runner, device, key_file.as_path())? {
        VerifyOutcome::Authenticated => Ok(()),
        VerifyOutcome::Rejected => {
            luks::enroll_key_file(runner, device, passphrase, key_file)?;
            Ok(())
        }
    }
}

/// Finish an add journal at PostAddBalanceRaid1. The journal target is the
/// committed membership, so the live pool must bind every member of it.
#[allow(clippy::too_many_arguments)]
fn execute_add_post_balance_recovery<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    journal: &Journal,
    pool: PoolState,
    owed_balance: OwedRaid1Balance,
    inhibitor_already_held: bool,
) -> Result<(), RecoverError> {
    const STAGE: &str = "post-add recovery";
    let committed = &journal.target_membership;
    let binding = match membership::bind_live_pool(&pool, committed) {
        Ok(binding) => binding,
        Err(LiveBindingError::Foreign { .. }) => {
            return Err(add_membership_mismatch(STAGE, committed, &pool, None));
        }
        Err(e) => return Err(live_binding_error(e, ADMISSION_SCOPE)),
    };
    let prior = membership::load_membership(params.paths).ok();
    let recovered = committed_add_membership(
        STAGE,
        &binding,
        &pool,
        committed,
        prior.as_ref(),
        by_id_resolver,
    )?;
    membership::save_membership(&recovered, params.paths)?;
    eprintln!("pool.json written from committed add membership.");

    let _guard = if inhibitor_already_held {
        None
    } else {
        Some(
            params
                .sleep_inhibitor
                .acquire("finishing interrupted add balance")
                .map_err(|e| {
                    RecoverError::Failed(format!("could not acquire sleep inhibitor: {e}"))
                })?,
        )
    };
    replay_owed_raid1_maintenance(
        runner,
        fs,
        params.config.mount_point(),
        owed_balance,
        "add",
        params.progress,
    )?;
    journal::clear_journal(params.paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
    eprintln!("pending-op.json cleared. Recovery complete.");
    Ok(())
}

/// Per-replay state for the `add` PoolMutation recovery path: keeps the
/// replay-time inputs (credential, journal slice, admission membership,
/// per-disk targets) and the live `PoolState` that the helper rebuilds
/// after opening any returned disks.
struct AddPoolReplayCtx<'a> {
    credential: Option<&'a OpenCredential>,
    journal: &'a Journal,
    union: &'a PoolMembership,
    targets: &'a LuksUuidMap<journal::AddJournalTarget>,
    pool: PoolState,
    owed_balance: OwedRaid1Balance,
}

/// Finish an interrupted live add. Its first step drops each fresh target
/// luksFormat never wrote, before any passphrase, inhibitor, or replay; the
/// replay then reads the journal that step leaves.
fn execute_add_pool_mutation_recovery<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    ctx: AddPoolReplayCtx<'_>,
    never_formatted: &std::collections::BTreeSet<LuksUuid>,
) -> Result<(), RecoverError> {
    let rewritten =
        match drop_never_formatted_add_targets(runner, fs, params, &ctx, never_formatted)? {
            AddTargetDrop::Kept => None,
            AddTargetDrop::RolledBack => return Ok(()),
            AddTargetDrop::Rewritten(journal) => Some(*journal),
        };
    let Some(journal) = &rewritten else {
        return replay_add_pool_mutation(runner, fs, by_id_resolver, params, ctx);
    };
    // Every later gate binds the live pool to the admission membership, so it
    // must come from the rewritten journal: a dropped target left in it would
    // never bind, and recover would refuse on every run.
    let union = recovery_admission_membership(journal)?;
    let journal::OpKind::Add { targets, .. } = &journal.op else {
        return Err(RecoverError::Journal(
            "rewritten add journal is not an add".into(),
        ));
    };
    replay_add_pool_mutation(
        runner,
        fs,
        by_id_resolver,
        params,
        AddPoolReplayCtx {
            credential: ctx.credential,
            journal,
            union: &union,
            targets,
            pool: ctx.pool,
            owed_balance: ctx.owed_balance,
        },
    )
}

/// What `drop_never_formatted_add_targets` did to an interrupted live add.
enum AddTargetDrop {
    /// No target to drop; the journal stands.
    Kept,
    /// Some targets dropped; recovery goes on with this journal. Boxed
    /// because a journal dwarfs the other variants.
    Rewritten(Box<Journal>),
    /// Every target dropped: pool.json holds the pre-add membership and the
    /// journal is cleared.
    RolledBack,
}

/// Drop each fresh add target luksFormat never wrote. Recover never formats
/// a disk, so a format that failed cannot fail again on every run; the
/// operator re-runs `braid add` for a dropped disk. A drop needs proof that
/// btrfs does not hold the disk: it is not committed, it still reads as not
/// LUKS, and every btrfs-MISSING devid with no open mapper is one a
/// pre-operation member records (`check_drop_orphans_no_device`). Otherwise
/// the disk may be one btrfs already added whose header now reads badly, so
/// recover refuses with the journal kept. The drop rewrites both the add
/// targets and target_membership, so the later gates see the reduced set and
/// a crash after the rewrite reruns cleanly. When no target remains nothing
/// committed, so this saves pre_membership and clears the journal, as
/// `finish_uncommitted_replace_recovery` does.
fn drop_never_formatted_add_targets<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RecoverParams<'_>,
    ctx: &AddPoolReplayCtx<'_>,
    never_formatted: &std::collections::BTreeSet<LuksUuid>,
) -> Result<AddTargetDrop, RecoverError> {
    let committed = committed_add_targets(&ctx.pool, ctx.union, ctx.targets)?;
    let mut dropped: Vec<(&LuksUuid, &journal::AddJournalTarget)> = Vec::new();
    for (uuid, target) in ctx.targets {
        if !never_formatted.contains(uuid)
            || committed.contains(uuid)
            || !matches!(target.mode, journal::AddJournalMode::FreshLuks { .. })
        {
            continue;
        }
        let probed = probe::probe_config_disk(
            runner,
            fs,
            &target.name,
            &target.by_id,
            params.backing_path_resolver,
        )?;
        if matches!(probed.state, ConfigDiskState::PresentNotLuks) {
            dropped.push((uuid, target));
        }
    }
    if dropped.is_empty() {
        return Ok(AddTargetDrop::Kept);
    }
    dropped.sort_by(|a, b| a.1.name.cmp(&b.1.name));
    check_drop_orphans_no_device(&ctx.pool, ctx.union, &ctx.journal.pre_membership, &dropped)?;

    let mut targets = ctx.targets.clone();
    let mut target_membership = ctx.journal.target_membership.clone();
    for (uuid, target) in &dropped {
        targets.remove(uuid);
        target_membership.remove_by_uuid(uuid);
        eprintln!(
            "note: add target '{}' ({}) was never formatted; dropping it from the \
             interrupted add. Re-run braid add to add it.",
            target.name, target.by_id
        );
    }
    if targets.is_empty() {
        membership::save_membership(&ctx.journal.pre_membership, params.paths)?;
        journal::clear_journal(params.paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
        eprintln!(
            "add did not complete -- luksFormat wrote no target, so the pool keeps its \
             pre-add membership. pending-op.json cleared. Re-run braid add to retry."
        );
        return Ok(AddTargetDrop::RolledBack);
    }
    let journal = journal::rewrite_journal(
        params.paths,
        ctx.journal,
        journal::OpKind::Add {
            phase: journal::AddPhase::PoolMutation,
            targets,
        },
        Some(target_membership),
    )
    .map_err(|e| RecoverError::Journal(e.to_string()))?;
    Ok(AddTargetDrop::Rewritten(Box::new(journal)))
}

/// Refuses a drop when btrfs may hold a dropped disk under a devid braid
/// cannot name: a btrfs-MISSING devid with no open mapper that no
/// pre-operation member records, or a device btrfs counts that the probe
/// does not show. A degraded pool whose missing member has no recorded devid
/// refuses here, which fails closed.
fn check_drop_orphans_no_device(
    pool: &PoolState,
    union: &PoolMembership,
    pre_membership: &PoolMembership,
    dropped: &[(&LuksUuid, &journal::AddJournalTarget)],
) -> Result<(), RecoverError> {
    let binding = membership::bind_live_pool(pool, union)
        .map_err(|e| live_binding_error(e, ADMISSION_SCOPE))?;
    let unnamed: Vec<Devid> = pool
        .devid_only_missing()
        .filter(|devid| {
            !pre_membership.iter().any(|(uuid, _)| {
                binding.member(uuid) == Some(MemberBinding::MissingByDevid(*devid))
            })
        })
        .collect();
    let shown = pool.devices.len() + pool.null_underlying.len() + pool.devid_only_missing().count();
    let detail = if !unnamed.is_empty() {
        format!(
            "missing devid(s) {} that no pre-operation member records",
            unnamed
                .iter()
                .map(|devid| devid.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else if (shown as u64) < pool.total_devices {
        format!(
            "{} devices, but braid sees only {shown} of them",
            pool.total_devices
        )
    } else {
        return Ok(());
    };
    let names = dropped
        .iter()
        .map(|(_, target)| format!("'{}'", target.name))
        .collect::<Vec<_>>()
        .join(", ");
    Err(RecoverError::Failed(format!(
        "recover cannot drop never-formatted add target(s) {names}: btrfs reports {detail}, \
         so one of them could be a disk btrfs already holds. Reconnect the missing disk and \
         re-run braid recover."
    )))
}

/// Replay an interrupted live add from its journal: open and scan the
/// formatted targets, add what btrfs does not hold yet, then hand off to the
/// committed tail. Never formats a disk.
fn replay_add_pool_mutation<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    ctx: AddPoolReplayCtx<'_>,
) -> Result<(), RecoverError> {
    let AddPoolReplayCtx {
        credential,
        journal,
        union,
        targets,
        mut pool,
        owed_balance,
    } = ctx;
    let mut committed = committed_add_targets(&pool, union, targets)?;
    let mount_point = params.config.mount_point();
    let mut passphrase: Option<RecoverPassphrase<'_>> = None;

    if committed.len() < targets.len() {
        let mut opened_or_scanned = false;
        for (target_uuid, target) in targets {
            if committed.contains(target_uuid) {
                continue;
            }
            let probed = probe::probe_config_disk(
                runner,
                fs,
                &target.name,
                &target.by_id,
                params.backing_path_resolver,
            )?;
            let ConfigDiskState::PresentLuks {
                uuid,
                label,
                mapper_open,
            } = probed.state
            else {
                continue;
            };

            match &target.mode {
                journal::AddJournalMode::RecoverableBraidLabeled { .. } if &uuid != target_uuid => {
                    return Err(RecoverError::Failed(format!(
                        "recover add target '{}' LUKS UUID mismatch: expected {}, found {}",
                        target.name, target_uuid, uuid
                    )));
                }
                journal::AddJournalMode::RecoverableBraidLabeled { .. } => {}
                journal::AddJournalMode::FreshLuks { .. } => {
                    let expected_label = config::luks_label_for(&target.name);
                    if label.as_deref() != Some(expected_label.as_str()) {
                        continue;
                    }
                    if &uuid != target_uuid {
                        return Err(RecoverError::Failed(add_recovery_uuid_mismatch_message(
                            &target.by_id,
                            target_uuid,
                            &uuid,
                            "fresh-luks",
                        )));
                    }
                }
            }

            if !mapper_open {
                if passphrase.is_none() {
                    passphrase = Some(recover_passphrase(credential, params, "add recovery")?);
                }
                let passphrase = passphrase
                    .as_ref()
                    .map(|p| p.expose_secret())
                    .expect("passphrase was resolved above");
                luks::ensure_luks_open(
                    runner,
                    fs,
                    &target.name,
                    &target.by_id,
                    params.backing_path_resolver,
                    passphrase,
                )?;
            }
            let mapper = config::mapper_name(&target.name);
            if scan_mapper_if_btrfs_visible(runner, &mapper.dev_path())? {
                opened_or_scanned = true;
            }
        }

        if opened_or_scanned {
            pool = probe::probe_pool(runner, fs, mount_point)?;
            committed = committed_add_targets(&pool, union, targets)?;
        }
    }

    if committed.len() < targets.len() {
        if passphrase.is_none() {
            passphrase = Some(recover_passphrase(credential, params, "add recovery")?);
        }
        let passphrase = passphrase
            .as_ref()
            .expect("passphrase was resolved above")
            .expose_secret();
        verify_recover_passphrase_for_add_replay(
            runner,
            fs,
            &pool,
            union,
            &targets
                .iter()
                .filter(|(uuid, _)| !committed.contains(*uuid))
                .collect::<Vec<_>>(),
            params.backing_path_resolver,
            passphrase,
        )?;
        let _guard = params
            .sleep_inhibitor
            .acquire("replaying interrupted add")
            .map_err(|e| RecoverError::Failed(format!("could not acquire sleep inhibitor: {e}")))?;

        for (target_uuid, target) in targets {
            if committed.contains(target_uuid) {
                continue;
            }
            let mapper = config::mapper_name(&target.name);
            let mapper_path = mapper.dev_path();
            match &target.mode {
                journal::AddJournalMode::RecoverableBraidLabeled {
                    verified_pool_fsid,
                    enroll_key_file,
                } => {
                    let (uuid, mapper_open) = match probe_present_recover_target(
                        runner,
                        fs,
                        RecoverTargetOp::Add,
                        &target.name,
                        &target.by_id,
                        params.backing_path_resolver,
                    )? {
                        PresentConfigDiskState::PresentLuks {
                            uuid, mapper_open, ..
                        } => (uuid, mapper_open),
                        PresentConfigDiskState::PresentNotLuks => {
                            return Err(RecoverError::Failed(format!(
                                "recover add target '{}' is not a LUKS device",
                                target.name
                            )));
                        }
                    };
                    if &uuid != target_uuid {
                        return Err(RecoverError::Failed(format!(
                            "recover add target '{}' LUKS UUID mismatch: expected {}, found {}",
                            target.name, target_uuid, uuid
                        )));
                    }
                    if !mapper_open {
                        luks::ensure_luks_open(
                            runner,
                            fs,
                            &target.name,
                            &target.by_id,
                            params.backing_path_resolver,
                            passphrase,
                        )?;
                    }
                    if let Some(fsid) = visible_btrfs_fsid(runner, &mapper_path)?
                        && &fsid != verified_pool_fsid
                    {
                        return Err(RecoverError::Failed(format!(
                            "recover add target '{}' btrfs FSID mismatch: expected {}, found {}",
                            target.name, verified_pool_fsid, fsid
                        )));
                    }
                    // Crashed mid-add: if the journaled plan called for keyfile
                    // enrollment on this returning disk, replay it before the
                    // pool_add_device. Mirrors the FreshLuks branch above and
                    // the live add execute path. ensure_keyfile_enrolled is
                    // idempotent (skips if slot 1 already authenticates), so a
                    // partially-completed pre-crash enrollment is safe to replay.
                    if let Some(key_file) = enroll_key_file {
                        ensure_keyfile_enrolled(
                            runner,
                            target.by_id.as_str(),
                            passphrase,
                            key_file,
                        )?;
                        luks::backup_luks_header(
                            runner,
                            target.by_id.as_str(),
                            &mapper,
                            params.paths,
                        )?;
                    }
                    crate::pool::pool_add_device(runner, &mapper_path, mount_point, true)
                        .map_err(|e| RecoverError::Failed(format!("recover add replay: {e}")))?;
                }
                // `extra_opts` is unread: recover never formats a disk.
                journal::AddJournalMode::FreshLuks {
                    enroll_key_file, ..
                } => {
                    let state = probe_present_recover_target(
                        runner,
                        fs,
                        RecoverTargetOp::Add,
                        &target.name,
                        &target.by_id,
                        params.backing_path_resolver,
                    )?;
                    let expected_label = config::luks_label_for(&target.name);
                    match state {
                        // Read as LUKS when recover planned, so it was not
                        // dropped. A rerun probes again and drops it.
                        PresentConfigDiskState::PresentNotLuks => {
                            return Err(RecoverError::Failed(format!(
                                "recover add target '{}' is no longer a LUKS device; recover \
                                 never formats a disk, so it stops here",
                                target.name
                            )));
                        }
                        PresentConfigDiskState::PresentLuks { uuid, label, .. } => {
                            if label.as_deref() != Some(expected_label.as_str()) {
                                return Err(RecoverError::Failed(format!(
                                    "recover add target '{}' has unexpected LUKS label",
                                    target.name
                                )));
                            }
                            if &uuid != target_uuid {
                                return Err(RecoverError::Failed(
                                    add_recovery_uuid_mismatch_message(
                                        &target.by_id,
                                        target_uuid,
                                        &uuid,
                                        "fresh-luks",
                                    ),
                                ));
                            }
                        }
                    }

                    if let Some(key_file) = enroll_key_file {
                        ensure_keyfile_enrolled(
                            runner,
                            target.by_id.as_str(),
                            passphrase,
                            key_file,
                        )?;
                    }
                    luks::backup_luks_header(runner, target.by_id.as_str(), &mapper, params.paths)?;
                    luks::ensure_luks_open(
                        runner,
                        fs,
                        &target.name,
                        &target.by_id,
                        params.backing_path_resolver,
                        passphrase,
                    )?;
                    crate::pool::pool_add_device(runner, &mapper_path, mount_point, false)
                        .map_err(|e| RecoverError::Failed(format!("recover add replay: {e}")))?;
                }
            }
            pool = probe::probe_pool(runner, fs, mount_point)?;
            // The disk was just added through an open mapper. Unlike the
            // committed check, which accepts a null-backed target, this lookup
            // requires btrfs to list the target as present. A miss is a btrfs
            // anomaly, so fail closed with the journal kept and let a rerun
            // probe again. Alert state is untouched at this point.
            let dev = pool.member_by_uuid(target_uuid).live().ok_or_else(|| {
                RecoverError::Failed(format!(
                    "recover add replay: target '{}' is not in the live pool after btrfs device add",
                    target.name
                ))
            })?;
            alert::drop_ghost_alert_state_for_devids(params.paths, &[dev.devid]).map_err(
                |source| RecoverError::AckCleanupFailed {
                    stage: "live-pool add recovery",
                    devids: vec![dev.devid],
                    source,
                },
            )?;
            // Per-target fail-closed gate: a foreign device surfacing mid-batch
            // stops further pool_add_device here rather than at the terminal
            // binding, which remains the final admission gate.
            committed = committed_add_targets(&pool, union, targets)?;
        }

        return finish_committed_add_recovery(
            runner,
            fs,
            by_id_resolver,
            params,
            AddPoolReplayCtx {
                credential,
                journal,
                union,
                targets,
                pool,
                owed_balance,
            },
            true,
        );
    }

    finish_committed_add_recovery(
        runner,
        fs,
        by_id_resolver,
        params,
        AddPoolReplayCtx {
            credential,
            journal,
            union,
            targets,
            pool,
            owed_balance,
        },
        false,
    )
}

/// The shared tail once every journaled add target should be committed:
/// bind the live pool once, sweep the targets' acked-stats, save the
/// committed membership, and advance the journal with that membership as its
/// target before the post-balance tail runs.
fn finish_committed_add_recovery<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    ctx: AddPoolReplayCtx<'_>,
    inhibitor_already_held: bool,
) -> Result<(), RecoverError> {
    let AddPoolReplayCtx {
        journal,
        union,
        targets,
        pool,
        owed_balance,
        ..
    } = ctx;
    let binding = membership::bind_live_pool(&pool, union)
        .map_err(|e| live_binding_error(e, ADMISSION_SCOPE))?;
    let Some(target_devids) = committed_add_target_devids(&binding, targets) else {
        return Err(RecoverError::Failed(
            "recover add replay finished but not every journaled target is live".into(),
        ));
    };
    let prior = membership::load_membership(params.paths).ok();
    let recovered = committed_add_membership(
        "add recovery",
        &binding,
        &pool,
        union,
        prior.as_ref(),
        by_id_resolver,
    )?;
    sweep_recovered_add_acked_stats(params.paths, &target_devids)?;
    membership::save_membership(&recovered, params.paths)?;
    eprintln!("pool.json written from completed add membership.");
    let journal = write_add_phase(
        params.paths,
        journal,
        journal::AddPhase::PostAddBalanceRaid1,
        recovered,
    )?;
    execute_add_post_balance_recovery(
        runner,
        fs,
        by_id_resolver,
        params,
        &journal,
        pool,
        owed_balance,
        inhibitor_already_held,
    )
}

/// Drop acked-stats ghosts for every journaled add target before phase handoff.
///
/// Recovery can enter with targets already committed, or skip individual
/// committed targets during replay; the sweep makes those committed-but-closed
/// windows obey the same reused-devid boundary as the replayed add arm. The
/// devids come from the live binding, so a target btrfs holds null-backed is
/// swept at the devid its mapper holds.
fn sweep_recovered_add_acked_stats(
    paths: &StatePaths,
    target_devids: &[Devid],
) -> Result<(), RecoverError> {
    alert::drop_ghost_alert_state_for_devids(paths, target_devids).map_err(|source| {
        RecoverError::AckCleanupFailed {
            stage: "live-pool add recovery (target sweep)",
            devids: target_devids.to_vec(),
            source,
        }
    })?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute_remove_missing_pool_mutation_recovery<
    R: CommandRunner + Sync,
    F: Filesystem + ?Sized,
>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    journal: &Journal,
    pool: PoolState,
    devid: Devid,
    owed_balance: Option<OwedRaid1Balance>,
) -> Result<(), RecoverError> {
    if pool.missing_devids.contains(&devid) {
        if !live_pool_matches_membership(&pool, &journal.pre_membership)? {
            return Err(RecoverError::Failed(format!(
                "remove-missing recovery found devid {devid} still missing, but live pool \
                 topology does not match the pre-operation membership"
            )));
        }
        membership::save_membership(&journal.pre_membership, params.paths)?;
        journal::clear_journal(params.paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
        eprintln!(
            "{}",
            remove_missing_not_committed_guidance(&pool, devid, params.config.mount_point())
        );
        return Ok(());
    }

    let Some(recovered) = recover_membership_matching_expected(
        &pool,
        &journal.target_membership,
        membership::load_membership(params.paths).ok().as_ref(),
        by_id_resolver,
    )?
    else {
        return Err(RecoverError::Failed(format!(
            "remove-missing recovery found devid {devid} gone, but live pool topology \
             does not match the target membership"
        )));
    };
    let journal = write_remove_missing_phase(
        params.paths,
        journal,
        journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
        Some(recovered),
    )?;
    execute_remove_missing_post_maintenance_recovery(
        runner,
        fs,
        by_id_resolver,
        params,
        RemoveMissingPostCtx {
            journal: &journal,
            pool,
            devid,
            owed_balance,
            inhibitor_already_held: false,
        },
    )
}

/// What recover says after an uncommitted remove-missing. A collapse, which
/// `cli/src/remove_missing.rs#classify_removal_outcome` picks from the live
/// pool's shape, converts before its device remove, and every braid mount
/// uses `skip_balance`, so a crash can leave that convert paused. Recover
/// leaves it alone, like a paused `braid remove` convert, but the re-run
/// refuses while it is paused, so the guidance names both ways out.
fn remove_missing_not_committed_guidance(
    pool: &PoolState,
    devid: Devid,
    mount_point: &MountPoint,
) -> String {
    let retry = format!(
        "remove-missing did not complete -- missing devid {devid} is still recorded. \
         Re-run braid remove-missing to retry."
    );
    let collapse = std::num::NonZeroU64::new(pool.missing_count)
        .and_then(|missing| {
            crate::remove_missing::classify_removal_outcome(pool.devices.len(), missing)
        })
        .is_some_and(|outcome| outcome.collapses());
    if !collapse {
        return retry;
    }
    format!(
        "{retry} If `braid status` shows a paused balance, the interrupted collapse left \
         its convert paused, and remove-missing refuses until it ends: run \
         `btrfs balance resume {mount_point}` to finish the convert, or \
         `btrfs balance cancel {mount_point}`, then re-run braid remove-missing."
    )
}

/// Per-replay state for the post-remove-missing recovery path: bundles the
/// journal slice, the resumed `PoolState`, the devid invariants the helper
/// asserts, and the inhibitor coordination flag set by the caller chain.
struct RemoveMissingPostCtx<'a> {
    journal: &'a Journal,
    pool: PoolState,
    devid: Devid,
    owed_balance: Option<OwedRaid1Balance>,
    inhibitor_already_held: bool,
}

fn execute_remove_missing_post_maintenance_recovery<
    R: CommandRunner + Sync,
    F: Filesystem + ?Sized,
>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    ctx: RemoveMissingPostCtx<'_>,
) -> Result<(), RecoverError> {
    let RemoveMissingPostCtx {
        journal,
        pool,
        devid,
        owed_balance,
        inhibitor_already_held,
    } = ctx;
    if pool.missing_devids.contains(&devid) {
        return Err(RecoverError::Failed(format!(
            "post-remove-missing recovery expected devid {devid} to be gone, \
             but btrfs still reports it missing"
        )));
    }
    let prior = membership::load_membership(params.paths).ok();
    let Some(recovered) = recover_membership_matching_expected(
        &pool,
        &journal.target_membership,
        prior.as_ref(),
        by_id_resolver,
    )?
    else {
        return Err(RecoverError::Failed(
            "post-remove-missing recovery live pool does not match target membership".into(),
        ));
    };
    membership::save_membership(&recovered, params.paths)?;
    eprintln!("pool.json written from committed remove-missing membership.");

    let _guard = if inhibitor_already_held || owed_balance.is_none() {
        None
    } else {
        Some(
            params
                .sleep_inhibitor
                .acquire("finishing interrupted remove-missing maintenance")
                .map_err(|e| {
                    RecoverError::Failed(format!("could not acquire sleep inhibitor: {e}"))
                })?,
        )
    };
    if let Some(owed_balance) = owed_balance {
        replay_owed_raid1_maintenance(
            runner,
            fs,
            params.config.mount_point(),
            owed_balance,
            "remove-missing",
            params.progress,
        )?;
    }
    journal::clear_journal(params.paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
    if let Err(e) = alert::drop_ghost_alert_state_for_devids(params.paths, &[devid]) {
        eprintln!("Warning: failed to update alert baselines: {e}");
    }
    eprintln!("pending-op.json cleared. Recovery complete.");
    Ok(())
}

fn verify_replace_fresh_prep_passphrase<R: CommandRunner>(
    runner: &R,
    pool: &PoolState,
    membership: &PoolMembership,
    new_name: &DiskName,
    new_by_id: &ByIdPath,
    passphrase: &Passphrase,
) -> Result<(), RecoverError> {
    let mut targets: Vec<_> = pool
        .devices
        .iter()
        .map(|device| CredentialVerifyTarget::existing_pool_member(membership, device))
        .collect();
    targets.push(CredentialVerifyTarget::named_candidate(new_name, new_by_id));
    verify_credential_for_targets(
        runner,
        &targets,
        Credential::Passphrase(passphrase),
        color_enabled_for_stderr(),
        |line| eprint!("{line}"),
    )
    .map_err(|e| match e {
        crate::credential_verify::CredentialVerifyError::Rejected { target } => {
            RecoverError::Failed(format!(
                "recover replace passphrase was rejected by '{}'",
                target.name()
            ))
        }
        crate::credential_verify::CredentialVerifyError::Luks { target, source } => {
            RecoverError::Failed(format!(
                "recover replace credential verification failed on '{}': {source}",
                target.name()
            ))
        }
    })
}

/// Finish-time inputs for the uncommitted replace recovery branch: bundles
/// the credential, journal slice, pool snapshot, incoming-disk identifiers,
/// and the planned LUKS writes needed to retry the prep-only steps without
/// touching the live pool.
struct ReplaceFinishCtx<'a> {
    credential: Option<&'a OpenCredential>,
    journal: &'a Journal,
    pool: &'a PoolState,
    new_uuid: &'a LuksUuid,
    new_name: &'a DiskName,
    new_target: &'a journal::ReplaceJournalTarget,
    writes: Option<&'a UncommittedReplaceWrites>,
}

fn finish_uncommitted_replace_recovery<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RecoverParams<'_>,
    ctx: ReplaceFinishCtx<'_>,
) -> Result<(), RecoverError> {
    let ReplaceFinishCtx {
        credential,
        journal,
        pool,
        new_uuid,
        new_name,
        new_target,
        writes,
    } = ctx;
    // Identity probe before any write. Disk unplugged, wrong disk replugged,
    // or header zeroed -> preserve the journal.
    let state = probe_present_recover_target(
        runner,
        fs,
        RecoverTargetOp::Replace,
        new_name,
        &new_target.by_id,
        params.backing_path_resolver,
    )?;
    let luks_header_present = match &new_target.mode {
        journal::ReplaceJournalMode::ExistingLuks { .. } => {
            // We use LUKS UUID (not a braid label) because ExistingLuks
            // targets carry no braid label by definition.
            match &state {
                PresentConfigDiskState::PresentLuks { uuid, .. } if uuid == new_uuid => true,
                PresentConfigDiskState::PresentLuks { uuid, .. } => {
                    return Err(RecoverError::Failed(format!(
                        "recover replace target '{}' LUKS UUID mismatch: \
                         expected {}, found {} -- preserving pending-op.json",
                        new_name, new_uuid, uuid,
                    )));
                }
                PresentConfigDiskState::PresentNotLuks => {
                    return Err(RecoverError::Failed(format!(
                        "recover replace target '{}' is no longer LUKS-formatted; \
                         preserving pending-op.json",
                        new_name,
                    )));
                }
            }
        }
        journal::ReplaceJournalMode::FreshLuks { .. } => {
            let expected_label = config::luks_label_for(new_name);
            match state {
                // Replace stopped before luksFormat: the disk holds nothing
                // braid wrote, so there is no prep to finish.
                PresentConfigDiskState::PresentNotLuks => false,
                PresentConfigDiskState::PresentLuks { uuid, label, .. } => {
                    if label.as_deref() != Some(expected_label.as_str()) {
                        return Err(RecoverError::Failed(format!(
                            "recover replace target '{}' has unexpected LUKS label",
                            new_name
                        )));
                    }
                    if &uuid != new_uuid {
                        return Err(RecoverError::Failed(format!(
                            "recover replace target '{}' LUKS UUID mismatch: expected {}, found {}",
                            new_target.by_id, new_uuid, uuid
                        )));
                    }
                    true
                }
            }
        }
    };

    if luks_header_present && let Some(writes) = writes {
        // The passphrase is verified against the existing pool members AND
        // the new disk before any LUKS write, so a wrong passphrase aborts
        // with the journal preserved.
        let passphrase = recover_passphrase(credential, params, "replace recovery")?;
        verify_replace_fresh_prep_passphrase(
            runner,
            pool,
            &journal.pre_membership,
            new_name,
            &new_target.by_id,
            passphrase.expose_secret(),
        )?;
        let _guard = params
            .sleep_inhibitor
            .acquire("finishing interrupted replace preparation")
            .map_err(|e| RecoverError::Failed(format!("could not acquire sleep inhibitor: {e}")))?;
        writes.execute(runner, passphrase.expose_secret(), params.paths)?;
    }

    membership::save_membership(&journal.pre_membership, params.paths)?;
    journal::clear_journal(params.paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
    eprintln!(
        "replace did not complete -- pool still has the pre-replace topology. \
         Re-run braid replace to retry."
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn execute_replace_pool_mutation_recovery<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    credential: Option<&OpenCredential>,
    journal: &Journal,
    union: &PoolMembership,
    pool: PoolState,
    identity: &journal::ReplaceIdentity,
    new_name: &DiskName,
    new_target: &journal::ReplaceJournalTarget,
    source: &journal::ReplaceJournalSource,
    uncommitted_writes: Option<&UncommittedReplaceWrites>,
    owed_balance: Option<OwedRaid1Balance>,
) -> Result<(), RecoverError> {
    validate_live_members_allowed(&pool, union)?;
    let live = live_member_uuids(&pool);
    // Tells the committed topology from the pre-replace one by which UUID
    // is live, which works only because `ReplaceIdentity` keeps them apart.
    let (old_uuid, new_uuid) = (identity.old_uuid(), identity.new_uuid());
    let committed = live.contains(new_uuid) && !live.contains(old_uuid);
    let pre_topology =
        live_pool_matches_membership(&pool, &journal.pre_membership)? && !live.contains(new_uuid);

    if committed {
        let Some(recovered) = recover_membership_matching_expected(
            &pool,
            &journal.target_membership,
            membership::load_membership(params.paths).ok().as_ref(),
            by_id_resolver,
        )?
        else {
            return Err(RecoverError::Failed(
                "replace recovery found the new disk live, but live pool topology \
                 does not match the target membership"
                    .into(),
            ));
        };
        let journal = write_replace_phase(
            params.paths,
            journal,
            journal::ReplacePhase::PostReplaceMaintenance,
            Some(recovered),
        )?;
        return execute_replace_post_maintenance_recovery(
            runner,
            fs,
            params.sleeper,
            by_id_resolver,
            params,
            &journal,
            pool,
            new_uuid,
            new_name,
            source,
            owed_balance,
            false,
        );
    }

    if pre_topology {
        return finish_uncommitted_replace_recovery(
            runner,
            fs,
            params,
            ReplaceFinishCtx {
                credential,
                journal,
                pool: &pool,
                new_uuid,
                new_name,
                new_target,
                writes: uncommitted_writes,
            },
        );
    }

    Err(RecoverError::Failed(
        "replace recovery live pool does not match either the pre-replace or \
         committed target topology; preserving pending-op.json"
            .into(),
    ))
}

fn close_old_mapper_best_effort<R, F, S>(
    runner: &R,
    fs: &F,
    sleeper: &S,
    mapper: &crate::types::MapperName,
    disk_label: &DiskName,
    old_uuid: &LuksUuid,
) where
    R: CommandRunner,
    F: Filesystem + ?Sized,
    S: Sleeper + ?Sized,
{
    let color_enabled = color_enabled_for_stderr();
    // Recovery mirrors the execute path's UUID authority. A transient probe
    // failure can leak this old dm slot until `braid lock` or reboot, but it
    // does not block later resize and journal-clear steps.
    match probe_observed_mapper_uuid(fs, mapper, old_uuid) {
        MapperOwnership::Owned => {
            if close_mapper_best_effort(
                runner,
                sleeper,
                mapper,
                disk_label,
                CloseContext::Normal,
                color_enabled,
            ) {
                eprintln!(
                    "Old device closed. If repurposing the physical disk, wipe it separately."
                );
            }
        }
        // Already closed is the normal post-crash / post-remount-cycle state.
        MapperOwnership::Inactive => {}
        // Foreign or unverifiable mapper; the probe helper already warned.
        MapperOwnership::Unverified => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_replace_post_maintenance_recovery<R, F, S>(
    runner: &R,
    fs: &F,
    sleeper: &S,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    journal: &Journal,
    pool: PoolState,
    new_uuid: &LuksUuid,
    new_name: &DiskName,
    source: &journal::ReplaceJournalSource,
    owed_balance: Option<OwedRaid1Balance>,
    inhibitor_already_held: bool,
) -> Result<(), RecoverError>
where
    R: CommandRunner + Sync,
    F: Filesystem + ?Sized,
    S: Sleeper + ?Sized,
{
    let prior = membership::load_membership(params.paths).ok();
    let Some(recovered) = recover_membership_matching_expected(
        &pool,
        &journal.target_membership,
        prior.as_ref(),
        by_id_resolver,
    )?
    else {
        return Err(RecoverError::Failed(
            "post-replace recovery live pool does not match target membership".into(),
        ));
    };
    let pre_names: std::collections::BTreeSet<_> = journal
        .pre_membership
        .names()
        .map(|n| n.as_str().to_owned())
        .collect();
    let target_names: std::collections::BTreeSet<_> = journal
        .target_membership
        .names()
        .map(|n| n.as_str().to_owned())
        .collect();
    let recovered_names: std::collections::BTreeSet<_> =
        recovered.names().map(|n| n.as_str().to_owned()).collect();
    eprintln!(
        "note: {}",
        recovery_guidance(&journal.op, &pre_names, &target_names, &recovered_names)
    );
    membership::save_membership(&recovered, params.paths)?;
    eprintln!("pool.json written from committed replace membership.");

    let _guard = if inhibitor_already_held {
        None
    } else {
        Some(
            params
                .sleep_inhibitor
                .acquire("finishing interrupted replace maintenance")
                .map_err(|e| {
                    RecoverError::Failed(format!("could not acquire sleep inhibitor: {e}"))
                })?,
        )
    };

    if let journal::ReplaceJournalSource::Live { old_mapper, .. } = source {
        let journal::OpKind::Replace(journal::ReplaceOp {
            identity, old_name, ..
        }) = &journal.op
        else {
            unreachable!("post-maintenance recovery runs only for Replace journals");
        };
        close_old_mapper_best_effort(
            runner,
            fs,
            sleeper,
            old_mapper,
            old_name,
            identity.old_uuid(),
        );
    }

    let Some(dev) = pool.member_by_uuid(new_uuid).live() else {
        return Err(RecoverError::Failed(format!(
            "post-replace recovery could not find new disk '{}' in the live pool",
            new_name
        )));
    };
    // btrfs handed the replaced devid to the new disk: replay replace's
    // fail-closed drop of the old disk's alert state, which a crash may have
    // cut off (ADR 014's acked-stats hygiene).
    alert::drop_ghost_alert_state_for_devids(params.paths, &[dev.devid]).map_err(|source| {
        RecoverError::AckCleanupFailed {
            stage: "replace recovery",
            devids: vec![dev.devid],
            source,
        }
    })?;
    eprintln!(
        "Replaying post-replace resize on devid {} (new disk '{}')...",
        dev.devid, new_name
    );
    crate::pool::pool_resize_device(runner, dev.devid, params.config.mount_point())
        .map_err(|e| RecoverError::Failed(format!("recover replace resize: {e}")))?;

    if let Some(owed_balance) = owed_balance {
        replay_owed_raid1_maintenance(
            runner,
            fs,
            params.config.mount_point(),
            owed_balance,
            "replace",
            params.progress,
        )?;
    }
    journal::clear_journal(params.paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
    eprintln!("pending-op.json cleared. Recovery complete.");
    Ok(())
}

const REPLACE_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const REPLACE_WAIT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Block until any kernel-resumed btrfs dev_replace on `mount_point` finishes.
///
/// `btrfs_resume_dev_replace_async` runs as an unrelated kthread and is NOT
/// waited on by umount, so without this wait the relock_and_remount cycle can
/// race the resume worker and the second mount sees the same in-flight state.
///
/// Recoverable status problems emit `[warn]` and proceed: a subprocess error
/// from `runner.run` (transient races, ENOMEM, signals) or `Cancelled`, where
/// the kernel has reverted the topology and downstream recovery can clean up
/// the journal. Hard stops emit `[fail]` and return `RecoverError::Failed`:
/// `Suspended`, because the kernel still treats the replace as ongoing, or
/// any parser `Err`, including a non-zero `btrfs replace status` exit
/// classified by the parser and unrecognised zero-exit stdout such as an
/// upstream wording change. In this stream, `[fail]` always pairs with
/// `RecoverError::Failed`.
///
/// The `Running` arm is intentionally unbounded. Proceeding past it would
/// race the resume kthread this barrier exists to close; a fail-returning
/// timeout would preserve the journal, but only re-hit the same kernel state
/// on the next recover. Stalls surface as elapsed-time heartbeats; SIGINT is
/// the operator escape.
fn wait_for_kernel_replace_to_finish<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    sleeper: &dyn Sleeper,
    color_enabled: bool,
) -> Result<(), RecoverError> {
    let mut last_pct: Option<f64> = None;
    let mut total_elapsed = Duration::ZERO;
    let mut since_last_emit = Duration::ZERO;
    let mut wait_emitted = false;
    loop {
        let raw = match runner.run(&CmdRequest::BtrfsReplaceStatus {
            mount_point: mount_point.clone(),
        }) {
            Ok(r) => r,
            Err(_) => {
                emit_status(&status_line(
                    StatusTag::Warn,
                    color_enabled,
                    "pool: kernel dev_replace status check failed -- proceeding",
                ));
                return Ok(());
            }
        };
        let parsed = match parse_btrfs_replace_status(&raw) {
            Ok(p) => p,
            Err(_) => {
                let stdout = raw.stdout.clone();
                let message = format!(
                    "pool: kernel dev_replace status returned unrecognised output (preserving journal; report upstream wording change). Re-run `braid recover` after upgrading braid. stdout: {stdout:?}"
                );
                emit_status(&status_line(StatusTag::Fail, color_enabled, &message));
                return Err(RecoverError::Failed(message));
            }
        };
        match parsed {
            ReplaceState::Finished | ReplaceState::NotStarted => {
                if wait_emitted {
                    emit_status(&status_line(
                        StatusTag::Ok,
                        color_enabled,
                        "pool: kernel dev_replace finished",
                    ));
                }
                return Ok(());
            }
            ReplaceState::Cancelled => {
                emit_status(&status_line(
                    StatusTag::Warn,
                    color_enabled,
                    "pool: kernel dev_replace canceled -- proceeding",
                ));
                return Ok(());
            }
            ReplaceState::Suspended { pct } => {
                let message = format!(
                    "pool: kernel dev_replace is suspended at {pct:.1}% (target device unavailable). Run `btrfs replace cancel {mount_point}` to clear it."
                );
                emit_status(&status_line(StatusTag::Fail, color_enabled, &message));
                return Err(RecoverError::Failed(message));
            }
            ReplaceState::Running { pct } => {
                if !wait_emitted {
                    emit_status(&status_line(
                        StatusTag::Wait,
                        color_enabled,
                        "pool: waiting for kernel dev_replace to finish...",
                    ));
                    emit_status(&format!("  ... {pct:.1}%\n"));
                    wait_emitted = true;
                    last_pct = Some(pct);
                    since_last_emit = Duration::ZERO;
                } else if last_pct != Some(pct) {
                    emit_status(&format!("  ... {pct:.1}%\n"));
                    last_pct = Some(pct);
                    since_last_emit = Duration::ZERO;
                } else if since_last_emit >= REPLACE_WAIT_HEARTBEAT_INTERVAL {
                    emit_status(&format!(
                        "  ... {pct:.1}% ({}s elapsed)\n",
                        total_elapsed.as_secs()
                    ));
                    since_last_emit = Duration::ZERO;
                }
            }
        }
        sleeper.sleep(REPLACE_WAIT_POLL_INTERVAL);
        total_elapsed += REPLACE_WAIT_POLL_INTERVAL;
        since_last_emit += REPLACE_WAIT_POLL_INTERVAL;
    }
}

/// Bundles the call-specific inputs for `relock_and_remount` so the recovery
/// remount cycle keeps the `runner, fs, ctx` positional shape shared with
/// sibling recovery phases and stays under clippy's argument-count threshold.
struct RelockAndRemountCtx<'a> {
    sleeper: &'a dyn Sleeper,
    config: &'a Config,
    paths: &'a StatePaths,
    membership: &'a PoolMembership,
    backing_path_resolver: &'a dyn BackingPathResolver,
    allow_degraded: bool,
    credential: &'a OpenCredential,
    close_names: &'a [DiskName],
}

/// Drop all kernel state for the recovery mount and re-establish it from
/// scratch, so a subsequent probe_pool reads the post-resume on-disk topology
/// instead of the stale in-memory btrfs_fs_devices the kernel can carry
/// across a resumed dev_replace.
///
/// This mirrors what `braid lock; braid unlock` does end-to-end: umount,
/// `btrfs device scan --forget` (drop cached fs_devices), close the planned
/// LUKS mapper set, then re-plan + re-open + remount via the standard
/// `plan_open_pool` + `execute_unlock_and_mount` flow.
///
/// The LUKS close+reopen is load-bearing: empirically, an `umount + scan
/// --forget + remount` cycle that leaves the dm devices alive does NOT clear
/// the staleness — see docs/internals/btrfs/dev-replace-resume.md.
fn relock_and_remount<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    ctx: RelockAndRemountCtx<'_>,
) -> Result<(), RecoverError> {
    let RelockAndRemountCtx {
        sleeper,
        config,
        paths,
        membership,
        backing_path_resolver,
        allow_degraded,
        credential,
        close_names,
    } = ctx;
    let color_enabled = color_enabled_for_stderr();
    let mount_point = config.mount_point();

    // 1. Umount. The kernel waits for in-flight operations (including the
    //    dev_replace resume worker) to drain before releasing the mount.
    eprint!(
        "{}",
        status_line(
            StatusTag::Wait,
            color_enabled,
            &format!("pool: unmounting {mount_point} (recover remount cycle)..."),
        )
    );
    let umount = runner
        .run(&CmdRequest::Umount {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| RecoverError::Failed(format!("recover remount cycle: umount: {e}")))?;
    if umount.exit_status != 0 {
        return Err(RecoverError::Failed(format!(
            "recover remount cycle: umount {} failed (exit {}): {}",
            mount_point,
            umount.exit_status,
            umount.stderr.trim()
        )));
    }
    eprint!(
        "{}",
        status_line(
            StatusTag::Ok,
            color_enabled,
            &format!("pool: unmounted {mount_point} (recover remount cycle)"),
        )
    );

    // 2. Drop cached btrfs_fs_devices. Without this, the kernel may
    //    re-attach the next mount to a still-cached structure that retains
    //    the stale post-resume topology, defeating the cycle.
    //
    //    Scope to the planned mapper close set for this cycle. The no-arg
    //    form is kernel-global and would invalidate unrelated btrfs scan
    //    entries; per-device forget is sufficient here because it only
    //    needs to cover the mappers this cycle will close.
    let forget_devs: Vec<String> = close_names
        .iter()
        .map(|name| config::mapper_name(name).dev_path())
        .filter(|p| fs.exists(p))
        .collect();
    if !forget_devs.is_empty() {
        let forget = runner
            .run(&CmdRequest::BtrfsDeviceScanForget {
                devices: forget_devs,
            })
            .map_err(|e| {
                RecoverError::Failed(format!("recover remount cycle: scan --forget: {e}"))
            })?;
        if forget.exit_status != 0 {
            return Err(RecoverError::Failed(format!(
                "recover remount cycle: btrfs device scan --forget failed (exit {}): {}",
                forget.exit_status,
                forget.stderr.trim()
            )));
        }
    }

    // 3. Close every planned LUKS mapper. The dm devices must be destroyed
    //    (not just unmounted) for the next mount to bypass the kernel's
    //    stale fs_devices cache.
    for name in close_names {
        let mn = config::mapper_name(name);
        let mapper_path = mn.dev_path();
        if !fs.exists(&mapper_path) {
            continue;
        }
        // Route through the shared core so this close gets the same busy-retry
        // as every other close path; failure still hard-aborts the cycle (no
        // closing row) by mapping the error into RecoverError.
        emit_close_progress(
            runner,
            sleeper,
            &mn,
            name,
            CloseContext::Normal,
            color_enabled,
        )
        .map_err(|e| {
            RecoverError::Failed(format!("recover remount cycle: cryptsetup close {mn}: {e}"))
        })?;
    }

    // 4. Re-open LUKS and mount via the standard helper. With the dm
    //    devices freshly recreated and the cached fs_devices dropped, the
    //    kernel reads the chunk tree from disk and rebuilds a fresh
    //    fs_devices reflecting the post-resume on-disk state.
    //
    // The cycle just closed planned mappers, so the cycle's plan ALWAYS has
    // `to_unlock` non-empty — we always pass the credential. (If somehow
    // plan_open_pool returns None here it means another mounter raced us.)
    let cycle_report = mount::plan_open_pool(
        runner,
        fs,
        &mount::PlanOpenPoolParams {
            config,
            paths,
            membership,
            backing_path_resolver,
            allow_degraded,
            command_hint: "recover",
        },
    );
    mount::print_probe_events(&cycle_report.events);
    let cycle_plan = cycle_report
        .result
        .map_err(|e| RecoverError::Failed(format!("recover remount cycle: plan: {e}")))?
        .ok_or_else(|| {
            RecoverError::Failed("recover remount cycle: pool already mounted after umount?".into())
        })?;
    match mount::execute_unlock_and_mount(
        runner,
        fs,
        config,
        &cycle_plan,
        backing_path_resolver,
        credential,
    ) {
        Ok(_) => {}
        Err(failure) => {
            let _ = mount::close_opened_mappers(
                runner,
                sleeper,
                fs,
                &failure.opened_mappers,
                color_enabled,
            );
            return Err(RecoverError::Failed(format!(
                "recover remount cycle: re-mount: {}",
                failure.error
            )));
        }
    }

    Ok(())
}

/// Compare recovered membership against pre/target to produce a one-sentence guidance message.
fn recovery_guidance(
    op: &journal::OpKind,
    pre_names: &std::collections::BTreeSet<String>,
    target_names: &std::collections::BTreeSet<String>,
    recovered_names: &std::collections::BTreeSet<String>,
) -> String {
    if recovered_names == target_names {
        match op {
            journal::OpKind::Add { targets, .. } => {
                let mut names: Vec<_> = targets.iter().map(|(_, t)| &t.name).collect();
                names.sort();
                let names: Vec<_> = names.into_iter().map(|n| format!("'{n}'")).collect();
                format!("add completed -- {} now in the pool.", names.join(", "))
            }
            journal::OpKind::Remove { name, .. } => {
                format!("remove completed -- '{name}' is no longer in the pool.")
            }
            journal::OpKind::RemoveMissing { .. } => {
                "remove-missing completed -- missing device removed from the pool.".to_owned()
            }
            journal::OpKind::Replace(journal::ReplaceOp {
                old_name, new_name, ..
            }) => {
                format!("replace completed -- '{old_name}' replaced by '{new_name}'.")
            }
        }
    } else if recovered_names == pre_names {
        match op {
            journal::OpKind::Add { targets, .. } => {
                let mut names: Vec<_> = targets.iter().map(|(_, t)| &t.name).collect();
                names.sort();
                let names: Vec<_> = names.into_iter().map(|n| format!("'{n}'")).collect();
                format!(
                    "add did not complete -- {} not in the pool. Re-run braid add to retry.",
                    names.join(", ")
                )
            }
            journal::OpKind::Remove { name, .. } => {
                format!(
                    "remove did not complete -- '{name}' is still in the pool. \
                     Re-run braid remove to retry."
                )
            }
            journal::OpKind::RemoveMissing { .. } => {
                "remove-missing did not complete -- device still in the pool. \
                 Re-run braid remove-missing to retry."
                    .to_owned()
            }
            journal::OpKind::Replace(journal::ReplaceOp { old_name, .. }) => {
                format!(
                    "replace did not complete -- pool still has '{old_name}'. \
                     Re-run braid replace to retry."
                )
            }
        }
    } else {
        "pool membership does not match the pre-operation or target state. \
         Run braid status and decide whether to re-run the operation."
            .to_owned()
    }
}

/// Build the phase-specific journal membership that recovery may admit live.
///
/// Replace post-maintenance uses the committed target snapshot only because
/// btrfs preserves the old device's devid on the replacement after commit.
/// Other phases admit the pre-operation snapshot plus target-only UUIDs so
/// interrupted mutations can observe either side without accepting unrelated
/// live devices.
fn recovery_admission_membership(
    journal: &Journal,
) -> Result<PoolMembership, membership::MembershipError> {
    if matches!(
        &journal.op,
        journal::OpKind::Replace(journal::ReplaceOp {
            phase: journal::ReplacePhase::PostReplaceMaintenance,
            ..
        })
    ) {
        return Ok(journal.target_membership.clone());
    }

    let mut membership = journal.pre_membership.clone();
    for (uuid, member) in journal.target_membership.iter() {
        if membership.by_uuid(uuid).is_none() {
            membership.insert(uuid.clone(), member.clone())?;
        }
    }
    Ok(membership)
}

/// Phase-specific mount source: which membership recover opens and mounts
/// before probing live topology. Three sources, so an interrupted mutation
/// mounts the set still observable at its journal phase.
///
/// - Pre-operation membership: existing-pool `Add::PoolMutation`,
///   `RemoveMissing::PoolMutation`.
/// - Committed target membership: every post-maintenance phase
///   (`PostAddBalanceRaid1`, `PostRemoveMissingMaintenance`,
///   `PostReplaceMaintenance`).
/// - Admission membership (pre + target-only; see
///   `recovery_admission_membership`): `Replace::PoolMutation` (kernel may
///   still be finishing `dev_replace`), bootstrap `Add::PoolMutation` (pre is
///   empty, so this is the new disk), and plain `Remove` (target is a subset
///   of pre, so this is the pre-removal set).
fn mount_membership_for_recover<'a>(
    journal: &'a Journal,
    admission_membership: &'a PoolMembership,
) -> &'a PoolMembership {
    match &journal.op {
        journal::OpKind::Add {
            phase: journal::AddPhase::PoolMutation,
            ..
        } => {
            if journal.is_bootstrap_add() {
                admission_membership
            } else {
                &journal.pre_membership
            }
        }
        journal::OpKind::Add {
            phase: journal::AddPhase::PostAddBalanceRaid1,
            ..
        } => &journal.target_membership,
        journal::OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PoolMutation,
            ..
        } => &journal.pre_membership,
        journal::OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
            ..
        } => &journal.target_membership,
        journal::OpKind::Replace(journal::ReplaceOp {
            phase: journal::ReplacePhase::PoolMutation,
            ..
        }) => admission_membership,
        journal::OpKind::Replace(journal::ReplaceOp {
            phase: journal::ReplacePhase::PostReplaceMaintenance,
            ..
        }) => &journal.target_membership,
        journal::OpKind::Remove { .. } => admission_membership,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::by_id::test_helpers::{MockByIdResolver, resolver_for};
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, MockRunner, RawCommandOutput};
    use crate::filesystem::Filesystem;
    use crate::journal::{self, OpKind};
    use crate::luks::ScriptedPassphraseReader;
    use crate::mapper_close::{CLOSE_RETRY_ATTEMPTS, CLOSE_RETRY_DELAY};
    use crate::mount::MountError;
    use crate::preview::NoteLevel;
    use crate::test_fixtures::{
        MockDmTable, PoolFixture, RecordingSleeper, RemountHarness, TEST_PASSPHRASE_BYTES,
        test_uuid,
    };
    use crate::types::{
        ByIdPath, DiskName, LuksFormatExtraOpts, LuksUuid, MapperName, MountPoint,
        NullUnderlyingDevice, PoolDevice,
    };
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::sync::{Arc, Mutex};

    fn passphrase(s: &str) -> Passphrase {
        Passphrase::from_zeroizing(zeroize::Zeroizing::new(s.to_owned()))
    }

    fn write_valid_keyfile(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
        let key_file = dir.path().join(name);
        std::fs::write(&key_file, vec![0u8; luks::KEYFILE_SIZE]).unwrap();
        key_file
    }

    // Intent: a key-file credential reaching recover's passphrase boundary is
    //   refused with a "requires a passphrase" error, never silently accepted.
    // Why it exists: this OpenCredential::KeyFile arm is a fail-closed guard on
    //   a branch unreachable through today's CLI, so a unit test is its only
    //   behavioral guard.
    // Scenario: a future recover --key-file path hands recover a resolved
    //   key-file credential; the guard must still fire with the passphrase
    //   hint, not proceed.
    #[test]
    fn key_file_credential_is_rejected_at_recover_passphrase_boundary() {
        let cred = OpenCredential::KeyFile(std::path::PathBuf::from("/dev/null"));
        let err = open_credential_passphrase(&cred, "add recovery").unwrap_err();

        assert!(
            matches!(err, RecoverError::Failed(ref msg) if msg.contains("requires a passphrase")),
            "key-file credential must be refused with a passphrase hint, got {err:?}"
        );
    }

    struct MockFs {
        paths: Vec<String>,
        mountinfo: String,
        /// Mountinfo for the first read only; later reads see `mountinfo`.
        /// Models a mount that changes between planning and execution.
        first_mountinfo: Mutex<Option<String>>,
        dm: MockDmTable,
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                mountinfo:
                    "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                        .to_owned(),
                first_mountinfo: Mutex::new(None),
                dm: MockDmTable::default(),
            }
        }

        fn without_mounted_pool(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                mountinfo: "26 25 0:23 / / rw shared:1 - ext4 /dev/sda1 rw\n".to_owned(),
                first_mountinfo: Mutex::new(None),
                dm: MockDmTable::default(),
            }
        }

        fn with_mounted_pool_ro_vfs(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                mountinfo:
                    "36 35 0:32 / /mnt/storage ro shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                        .to_owned(),
                first_mountinfo: Mutex::new(None),
                dm: MockDmTable::default(),
            }
        }

        fn with_mounted_pool_ro_fs(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                mountinfo:
                    "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 ro,space_cache=v2\n"
                        .to_owned(),
                first_mountinfo: Mutex::new(None),
                dm: MockDmTable::default(),
            }
        }

        /// Add an active LUKS2 mapping to the sysfs device-mapper table.
        fn with_luks_dm(mut self, mapper: &str, luks_uuid: &str) -> Self {
            self.dm = self.dm.with_luks(mapper, luks_uuid);
            self
        }

        /// Name the mapper mountinfo reports as the pool's device, the
        /// device the mount classifier reads the LUKS identity from.
        fn mounted_from(mut self, mapper: &str) -> Self {
            self.mountinfo = self
                .mountinfo
                .replace("/dev/mapper/braid-disk1", &format!("/dev/mapper/{mapper}"));
            self
        }

        /// Show `mountinfo` on the first read only, then the current one.
        fn first_reading(self, mountinfo: &str) -> Self {
            *self.first_mountinfo.lock().unwrap() = Some(mountinfo.to_owned());
            self
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path.to_string())
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }

        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path == "/proc/self/mountinfo" {
                if let Some(first) = self.first_mountinfo.lock().unwrap().take() {
                    return Ok(first);
                }
                return Ok(self.mountinfo.clone());
            }
            self.dm
                .read_to_string(path)
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }

        fn list_dir(&self, path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(self.dm.list_dir(path).unwrap_or_default())
        }

        fn claim_exclusive(&self, _path: &str) -> Result<(), std::io::Error> {
            Ok(())
        }

        fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
            Ok(())
        }
    }

    // Intent: every recovery failure is wrapped at the command boundary with
    // advice derived from the journal state visible after that failure.
    // Why it exists: lower-level recovery branches must not independently
    // decide whether the operator can rerun recovery.
    // Scenario: recovery fails with a valid visible journal and preserves the
    // typed source error before one shared rerun instruction.
    #[test]
    fn cmd_recover_classifies_failure_with_visible_journal() {
        let f = PoolFixture::empty();
        let pending = journal::build_journal(
            PoolMembership::empty(),
            PoolMembership::empty(),
            OpKind::Remove {
                luks_uuid: test_uuid(975),
                name: DiskName::parse("disk1").unwrap(),
            },
        );
        journal::write_journal(&f.paths, &pending).unwrap();

        let err = cmd_recover(
            &MockRunner::default(),
            &MockFs::without_mounted_pool(&[]),
            &MockByIdResolver::default(),
            &f.recover_params().passphrase_file(None).build(),
        )
        .unwrap_err();

        match err {
            RecoverError::JournalLifecycle { source, advice } => {
                assert!(!matches!(*source, RecoverError::JournalLifecycle { .. }));
                assert!(advice.contains("re-run `braid recover`"));
                assert_eq!(advice.matches("re-run `braid recover`").count(), 1);
            }
            other => panic!("expected journal lifecycle wrapper, got {other:?}"),
        }
    }

    // Intent: an absent journal after a recovery error renders uncertain
    // deletion guidance without directing the operator into recovery.
    // Why it exists: every journal-clear branch returns through this same
    // boundary, so an absent final observation must not claim a rerun is safe.
    // Scenario: the command starts with no journal, preserving its original
    // refusal as the typed source while classifying the absent final state.
    #[test]
    fn cmd_recover_classifies_failure_with_absent_journal() {
        let f = PoolFixture::empty();
        let err = cmd_recover(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &MockByIdResolver::default(),
            &f.recover_params().passphrase_file(None).build(),
        )
        .unwrap_err();

        match err {
            RecoverError::JournalLifecycle { source, advice } => {
                assert!(matches!(*source, RecoverError::Failed(_)));
                assert!(advice.contains("deletion durability was not confirmed"));
                assert!(!advice.contains("re-run `braid recover`"));
            }
            other => panic!("expected journal lifecycle wrapper, got {other:?}"),
        }
    }

    // Intent: journals that recovery cannot consume render storage/manual
    // remediation instead of a contradictory recovery retry.
    // Why it exists: the classifier must use `load_journal`, including its
    // parse and I/O distinctions, rather than a path-existence check.
    // Scenario: pending-op.json is first invalid JSON and then an unreadable
    // filesystem object at the recovery command boundary.
    #[test]
    fn cmd_recover_classifies_unparseable_and_unreadable_journals() {
        let f = PoolFixture::empty();
        std::fs::write(f.paths.pending_op_json(), b"not json").unwrap();
        let err = cmd_recover(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &MockByIdResolver::default(),
            &f.recover_params().passphrase_file(None).build(),
        )
        .unwrap_err();
        let RecoverError::JournalLifecycle { advice, .. } = err else {
            panic!("expected journal lifecycle wrapper");
        };
        assert!(advice.contains("observed journal error"));
        assert!(!advice.contains("re-run `braid recover`"));

        std::fs::remove_file(f.paths.pending_op_json()).unwrap();
        std::fs::create_dir(f.paths.pending_op_json()).unwrap();
        let err = cmd_recover(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &MockByIdResolver::default(),
            &f.recover_params().passphrase_file(None).build(),
        )
        .unwrap_err();
        let RecoverError::JournalLifecycle { advice, .. } = err else {
            panic!("expected journal lifecycle wrapper");
        };
        assert!(advice.contains("observed journal error"));
        assert!(advice.contains("after manual reconciliation"));
        assert!(!advice.contains("re-run `braid recover`"));
    }

    // Intent: recovery lifecycle classification does not change the dedicated
    // degraded-mount exit-code signal consumed by the CLI boundary.
    // Why it exists: wrapping all recovery errors would otherwise hide the
    // typed refusal that main maps to exit status 2.
    // Scenario: a degraded refusal is nested under lifecycle advice.
    #[test]
    fn recovery_wrapper_preserves_degraded_refusal() {
        let err = RecoverError::JournalLifecycle {
            source: Box::new(RecoverError::Mount(MountError::DegradedRefused(
                "degraded".into(),
            ))),
            advice: "advice".into(),
        };
        assert!(err.is_degraded_refusal());
        assert_eq!(err.to_string(), "degraded\nadvice");
    }

    struct FailingInhibitor;

    impl AcquireSleepInhibitor for FailingInhibitor {
        fn acquire(&self, _why: &str) -> std::io::Result<Box<dyn crate::inhibit::SleepGuard>> {
            Err(std::io::Error::other("forced inhibitor failure"))
        }
    }

    struct RequestCountInhibitor {
        runner: MockRunner,
        drop_request_count: std::rc::Rc<std::cell::Cell<Option<usize>>>,
        first_acquire_request_count: std::cell::Cell<Option<usize>>,
        acquire_count: std::cell::Cell<usize>,
    }

    struct RequestCountSleepGuard {
        runner: MockRunner,
        drop_request_count: std::rc::Rc<std::cell::Cell<Option<usize>>>,
    }

    impl Drop for RequestCountSleepGuard {
        fn drop(&mut self) {
            if self.drop_request_count.get().is_none() {
                self.drop_request_count
                    .set(Some(self.runner.requests().len()));
            }
        }
    }

    impl RequestCountInhibitor {
        fn new(runner: MockRunner) -> Self {
            Self {
                runner,
                drop_request_count: std::rc::Rc::new(std::cell::Cell::new(None)),
                first_acquire_request_count: std::cell::Cell::new(None),
                acquire_count: std::cell::Cell::new(0),
            }
        }

        fn first_acquire_request_count(&self) -> Option<usize> {
            self.first_acquire_request_count.get()
        }

        fn acquire_count(&self) -> usize {
            self.acquire_count.get()
        }

        fn drop_request_count(&self) -> Option<usize> {
            self.drop_request_count.get()
        }
    }

    impl AcquireSleepInhibitor for RequestCountInhibitor {
        fn acquire(&self, _why: &str) -> std::io::Result<Box<dyn crate::inhibit::SleepGuard>> {
            self.acquire_count.set(self.acquire_count.get() + 1);
            if self.first_acquire_request_count.get().is_none() {
                self.first_acquire_request_count
                    .set(Some(self.runner.requests().len()));
            }
            Ok(Box::new(RequestCountSleepGuard {
                runner: self.runner.clone(),
                drop_request_count: std::rc::Rc::clone(&self.drop_request_count),
            }))
        }
    }

    /// `cryptsetup status` output for a mapper that is currently closed.
    /// Matches the exit-status / stderr shape that recover's status parser
    /// classifies as inactive.
    fn inactive_mapper_status(mapper: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup status {mapper}"),
            stdout: String::new(),
            stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
            exit_status: 4,
        }
    }

    fn ok_raw(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn ok_raw_empty(cmd: &str) -> RawCommandOutput {
        ok_raw(cmd, "")
    }

    enum ReplaceStatusItem {
        Output(RawCommandOutput),
        Error(&'static str),
    }

    struct ReplaceStatusSequenceRunner {
        items: Mutex<VecDeque<ReplaceStatusItem>>,
    }

    impl ReplaceStatusSequenceRunner {
        fn new(items: Vec<ReplaceStatusItem>) -> Self {
            Self {
                items: Mutex::new(VecDeque::from(items)),
            }
        }
    }

    impl CommandRunner for ReplaceStatusSequenceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            assert!(
                matches!(request, CmdRequest::BtrfsReplaceStatus { .. }),
                "unexpected request: {request:?}"
            );
            match self.items.lock().unwrap().pop_front() {
                Some(ReplaceStatusItem::Output(output)) => Ok(output),
                Some(ReplaceStatusItem::Error(message)) => Err(CmdError::Failed(message.into())),
                None => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            Err(CmdError::Failed(format!(
                "unexpected stdin request: {request:?}"
            )))
        }
    }

    fn err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
            exit_status: exit_code,
        }
    }

    fn assert_close_retry_sleeps(calls: Vec<std::time::Duration>) {
        assert_eq!(
            calls,
            vec![CLOSE_RETRY_DELAY; (CLOSE_RETRY_ATTEMPTS - 1) as usize],
            "close retry sleeps must use the injected sleeper"
        );
    }

    fn running_runs(n: usize, pct: &str) -> Vec<ReplaceStatusItem> {
        let cmd = "btrfs replace status -1 /mnt/storage";
        let body = format!("{pct} done, 0 write errs, 0 uncorr. read errs\n");
        (0..n)
            .map(|_| ReplaceStatusItem::Output(ok_raw(cmd, &body)))
            .collect()
    }

    #[test]
    fn wait_for_kernel_replace_emits_canonical_rows_on_running_then_finished() {
        /*
        Intent: a real wait on kernel-resumed dev_replace is announced and closed.
        Why it exists: percentage progress only appears when the percentage changes; a
        slow worker still needs an upfront canonical wait row.
        Scenario: recover observes one running poll, then a finished poll.
        */
        let runner = ReplaceStatusSequenceRunner::new(vec![
            ReplaceStatusItem::Output(ok_raw(
                "btrfs replace status -1 /mnt/storage",
                "5.0% done, 0 write errs, 0 uncorr. read errs\n",
            )),
            ReplaceStatusItem::Output(ok_raw(
                "btrfs replace status -1 /mnt/storage",
                "Started on 27.Feb 10:30:00, finished on 27.Feb 10:35:00, 0 write errs, 0 uncorr. read errs\n",
            )),
        ]);
        let mount_point = MountPoint::new("/mnt/storage".into());
        let captured = crate::status_tag::testing::capture_with(|| {
            wait_for_kernel_replace_to_finish(&runner, &mount_point, &progress::NoopSleeper, false)
                .unwrap();
        });
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            vec![
                "[wait] pool: waiting for kernel dev_replace to finish...",
                "  ... 5.0%",
                "[ok]   pool: kernel dev_replace finished",
            ],
        );
    }

    #[test]
    // Intent: canceled kernel dev_replace reports a warn row but does not abort
    // recovery after an observed in-flight wait.
    // Why it exists: canceled means the kernel rolled topology back; downstream
    // replace recovery can still classify and clean up the journal safely.
    // Scenario: recover observes one running poll, then the kernel reports
    // "canceled on" for the same replace.
    fn wait_for_kernel_replace_emits_warn_on_canceled_returns_ok() {
        let runner = ReplaceStatusSequenceRunner::new(vec![
            ReplaceStatusItem::Output(ok_raw(
                "btrfs replace status -1 /mnt/storage",
                "5.0% done, 0 write errs, 0 uncorr. read errs\n",
            )),
            ReplaceStatusItem::Output(ok_raw(
                "btrfs replace status -1 /mnt/storage",
                "Started on 27.Feb 10:30:00, canceled on 27.Feb 10:35:00 at 0.0%, 0 write errs, 0 uncorr. read errs\n",
            )),
        ]);
        let mount_point = MountPoint::new("/mnt/storage".into());
        let mut result = None;
        let captured = crate::status_tag::testing::capture_with(|| {
            result = Some(wait_for_kernel_replace_to_finish(
                &runner,
                &mount_point,
                &progress::NoopSleeper,
                false,
            ));
        });
        assert!(result.unwrap().is_ok(), "canceled replace should proceed");
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            vec![
                "[wait] pool: waiting for kernel dev_replace to finish...",
                "  ... 5.0%",
                "[warn] pool: kernel dev_replace canceled -- proceeding",
            ],
        );
    }

    #[test]
    // Intent: suspended kernel dev_replace reports an actionable fail row and
    // returns a recover-blocking error.
    // Why it exists: the kernel treats suspended replace as ongoing, so
    // continuing would clear braid's journal while a retry is still blocked.
    // Scenario: recover observes one running poll, then the target disappears
    // and status reports "suspended on" with real progress.
    fn wait_for_kernel_replace_emits_fail_on_suspended_returns_err() {
        let runner = ReplaceStatusSequenceRunner::new(vec![
            ReplaceStatusItem::Output(ok_raw(
                "btrfs replace status -1 /mnt/storage",
                "5.0% done, 0 write errs, 0 uncorr. read errs\n",
            )),
            ReplaceStatusItem::Output(ok_raw(
                "btrfs replace status -1 /mnt/storage",
                "Started on 27.Feb 10:30:00, suspended on 27.Feb 10:35:00 at 12.5%, 0 write errs, 0 uncorr. read errs\n",
            )),
        ]);
        let mount_point = MountPoint::new("/mnt/storage".into());
        let mut result = None;
        let captured = crate::status_tag::testing::capture_with(|| {
            result = Some(wait_for_kernel_replace_to_finish(
                &runner,
                &mount_point,
                &progress::NoopSleeper,
                false,
            ));
        });
        let err = result.unwrap().expect_err("suspended replace should abort");
        let RecoverError::Failed(msg) = err else {
            panic!("expected RecoverError::Failed, got {err:?}");
        };
        assert!(
            msg.contains("suspended at 12.5%"),
            "error should carry suspended percentage, got: {msg}"
        );
        assert!(
            msg.contains("btrfs replace cancel /mnt/storage"),
            "error should give the manual cancel command, got: {msg}"
        );
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            vec![
                "[wait] pool: waiting for kernel dev_replace to finish...",
                "  ... 5.0%",
                "[fail] pool: kernel dev_replace is suspended at 12.5% (target device unavailable). Run `btrfs replace cancel /mnt/storage` to clear it.",
            ],
        );
    }

    #[test]
    // Intent: canceled kernel dev_replace reports a warn row even when it is
    // the first status observed.
    // Why it exists: canceled is surfaced unconditionally, unlike the silent
    // Finished/NotStarted fast path that only emits [ok] after a wait row.
    // Scenario: recover mounts after the kernel already transitioned the
    // resumed replace to CANCELED.
    fn wait_for_kernel_replace_emits_warn_on_canceled_first_poll() {
        let runner = ReplaceStatusSequenceRunner::new(vec![ReplaceStatusItem::Output(ok_raw(
            "btrfs replace status -1 /mnt/storage",
            "Started on 27.Feb 10:30:00, canceled on 27.Feb 10:35:00 at 0.0%, 0 write errs, 0 uncorr. read errs\n",
        ))]);
        let mount_point = MountPoint::new("/mnt/storage".into());
        let mut result = None;
        let captured = crate::status_tag::testing::capture_with(|| {
            result = Some(wait_for_kernel_replace_to_finish(
                &runner,
                &mount_point,
                &progress::NoopSleeper,
                false,
            ));
        });
        assert!(result.unwrap().is_ok(), "canceled replace should proceed");
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            vec!["[warn] pool: kernel dev_replace canceled -- proceeding"],
        );
    }

    #[test]
    fn wait_for_kernel_replace_emits_warn_on_status_error_after_wait() {
        /*
        Intent: status-poll failure after an observed wait closes the row with [warn].
        Why it exists: recover continues on this best-effort barrier, so a warning
        row is the only terminal row for the announced wait window.
        Scenario: recover observes a running dev_replace, then the next status
        subprocess fails.
        */
        let runner = ReplaceStatusSequenceRunner::new(vec![
            ReplaceStatusItem::Output(ok_raw(
                "btrfs replace status -1 /mnt/storage",
                "5.0% done, 0 write errs, 0 uncorr. read errs\n",
            )),
            ReplaceStatusItem::Error("status failed"),
        ]);
        let mount_point = MountPoint::new("/mnt/storage".into());
        let captured = crate::status_tag::testing::capture_with(|| {
            wait_for_kernel_replace_to_finish(&runner, &mount_point, &progress::NoopSleeper, false)
                .unwrap();
        });
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            vec![
                "[wait] pool: waiting for kernel dev_replace to finish...",
                "  ... 5.0%",
                "[warn] pool: kernel dev_replace status check failed -- proceeding",
            ],
        );
    }

    #[test]
    // Intent: a runner subprocess failure on the very first poll still emits a
    //   [warn] row and returns Ok so recover proceeds on the best-effort
    //   barrier.
    // Why it exists: a transient runner Err on a never-replaced pool would
    //   otherwise force-fail every recover; the wait is best-effort against
    //   subprocess failures specifically. Pins the "subprocess failure is
    //   best-effort" branch of the split between runner-Err (warn + proceed)
    //   and parser-Err (fail + preserve journal).
    // Scenario: the very first `btrfs replace status` call fails to spawn
    //   before any wait row has been emitted.
    fn wait_for_kernel_replace_emits_warn_on_status_error_first_poll() {
        let runner =
            ReplaceStatusSequenceRunner::new(vec![ReplaceStatusItem::Error("status failed")]);
        let mount_point = MountPoint::new("/mnt/storage".into());
        let captured = crate::status_tag::testing::capture_with(|| {
            wait_for_kernel_replace_to_finish(&runner, &mount_point, &progress::NoopSleeper, false)
                .unwrap();
        });
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            vec!["[warn] pool: kernel dev_replace status check failed -- proceeding"],
        );
    }

    #[test]
    // Intent: unrecognised stdout on the very first poll emits a [fail] row
    //   and returns RecoverError::Failed, preserving the journal.
    // Why it exists: this is the regression commit b551555 was added to
    //   prevent. If a future btrfs-progs reworded "% done" to "% complete",
    //   the old silent NotStarted fallback would exit at the first poll, race
    //   the kernel resume worker through relock_and_remount, and let
    //   downstream replace recovery clear pending-op.json. Pinning this as a
    //   test makes the upstream wording change a clear failure rather than a
    //   silent skip.
    // Scenario: the first `btrfs replace status` call exits zero with a
    //   fictional reworded line; recover must abort and keep the journal.
    fn wait_for_kernel_replace_emits_fail_on_unrecognised_stdout_first_poll() {
        let runner = ReplaceStatusSequenceRunner::new(vec![ReplaceStatusItem::Output(ok_raw(
            "btrfs replace status -1 /mnt/storage",
            "75.0% complete, 0 write errs, 0 uncorr. read errs\n",
        ))]);
        let mount_point = MountPoint::new("/mnt/storage".into());
        let mut result = None;
        let captured = crate::status_tag::testing::capture_with(|| {
            result = Some(wait_for_kernel_replace_to_finish(
                &runner,
                &mount_point,
                &progress::NoopSleeper,
                false,
            ));
        });
        let err = result
            .unwrap()
            .expect_err("unrecognised stdout should abort recover");
        let RecoverError::Failed(msg) = err else {
            panic!("expected RecoverError::Failed, got {err:?}");
        };
        assert!(
            msg.contains("75.0% complete"),
            "error should carry the offending stdout, got: {msg}"
        );
        let lines = captured.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "expected single fail row, got {lines:?}");
        assert!(
            lines[0]
                .starts_with("[fail] pool: kernel dev_replace status returned unrecognised output"),
            "unexpected fail row: {}",
            lines[0]
        );
        assert!(
            lines[0].contains("75.0% complete"),
            "fail row should echo the offending stdout: {}",
            lines[0]
        );
    }

    #[test]
    // Intent: when the wait window has already been announced, an
    //   unrecognised reworded line on the next poll closes the window with a
    //   [fail] row and returns RecoverError::Failed.
    // Why it exists: closes the announced wait with the correct terminal row
    //   when the kernel transitions an in-flight replace into stdout we no
    //   longer recognise; same fail-closed contract as the first-poll case
    //   but exercises the post-wait path so we cannot regress only one
    //   branch.
    // Scenario: recover observes one running poll, then a fictional reworded
    //   line on the next poll.
    fn wait_for_kernel_replace_emits_fail_on_unrecognised_stdout_after_wait() {
        let runner = ReplaceStatusSequenceRunner::new(vec![
            ReplaceStatusItem::Output(ok_raw(
                "btrfs replace status -1 /mnt/storage",
                "5.0% done, 0 write errs, 0 uncorr. read errs\n",
            )),
            ReplaceStatusItem::Output(ok_raw(
                "btrfs replace status -1 /mnt/storage",
                "75.0% complete, 0 write errs, 0 uncorr. read errs\n",
            )),
        ]);
        let mount_point = MountPoint::new("/mnt/storage".into());
        let mut result = None;
        let captured = crate::status_tag::testing::capture_with(|| {
            result = Some(wait_for_kernel_replace_to_finish(
                &runner,
                &mount_point,
                &progress::NoopSleeper,
                false,
            ));
        });
        let err = result
            .unwrap()
            .expect_err("unrecognised stdout should abort recover");
        let RecoverError::Failed(msg) = err else {
            panic!("expected RecoverError::Failed, got {err:?}");
        };
        assert!(
            msg.contains("75.0% complete"),
            "error should carry the offending stdout, got: {msg}"
        );
        let lines = captured.lines().collect::<Vec<_>>();
        assert_eq!(
            lines.len(),
            3,
            "expected wait + progress + fail rows, got {lines:?}"
        );
        assert_eq!(
            lines[0],
            "[wait] pool: waiting for kernel dev_replace to finish..."
        );
        assert_eq!(lines[1], "  ... 5.0%");
        assert!(
            lines[2]
                .starts_with("[fail] pool: kernel dev_replace status returned unrecognised output"),
            "unexpected fail row: {}",
            lines[2]
        );
        assert!(
            lines[2].contains("75.0% complete"),
            "fail row should echo the offending stdout: {}",
            lines[2]
        );
    }

    #[test]
    fn wait_for_kernel_replace_emits_cumulative_heartbeats_when_pct_unchanged() {
        /*
        Intent: when the kernel-reported pct stalls, the wait loop emits heartbeat
        lines at the configured cadence with cumulative elapsed seconds.
        Why it exists: operator confidence depends on seeing monotonically advancing
        time during a stall; a stuck suffix would hide whether the wait loop is
        still looping.
        Scenario: recover observes 320 unchanged running polls, then a finished
        poll under a noop sleeper.
        */
        let mut items = running_runs(320, "50.0%");
        items.push(ReplaceStatusItem::Output(ok_raw(
            "btrfs replace status -1 /mnt/storage",
            "Started on 27.Feb 10:30:00, finished on 27.Feb 10:35:00, 0 write errs, 0 uncorr. read errs\n",
        )));
        let runner = ReplaceStatusSequenceRunner::new(items);
        let mount_point = MountPoint::new("/mnt/storage".into());
        let captured = crate::status_tag::testing::capture_with(|| {
            wait_for_kernel_replace_to_finish(&runner, &mount_point, &progress::NoopSleeper, false)
                .unwrap();
        });
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            vec![
                "[wait] pool: waiting for kernel dev_replace to finish...",
                "  ... 50.0%",
                "  ... 50.0% (30s elapsed)",
                "  ... 50.0% (60s elapsed)",
                "[ok]   pool: kernel dev_replace finished",
            ],
        );
    }

    #[test]
    fn wait_for_kernel_replace_does_not_emit_heartbeat_below_threshold() {
        /*
        Intent: when pct stays unchanged for fewer iterations than the heartbeat
        threshold requires, no heartbeat line is emitted.
        Why it exists: bounding silence is useful only if the threshold is honored;
        heartbeats that fire too often would spam stderr.
        Scenario: recover observes 100 unchanged running polls, then a finished poll
        under a noop sleeper.
        */
        let mut items = running_runs(100, "50.0%");
        items.push(ReplaceStatusItem::Output(ok_raw(
            "btrfs replace status -1 /mnt/storage",
            "Started on 27.Feb 10:30:00, finished on 27.Feb 10:35:00, 0 write errs, 0 uncorr. read errs\n",
        )));
        let runner = ReplaceStatusSequenceRunner::new(items);
        let mount_point = MountPoint::new("/mnt/storage".into());
        let captured = crate::status_tag::testing::capture_with(|| {
            wait_for_kernel_replace_to_finish(&runner, &mount_point, &progress::NoopSleeper, false)
                .unwrap();
        });
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            vec![
                "[wait] pool: waiting for kernel dev_replace to finish...",
                "  ... 50.0%",
                "[ok]   pool: kernel dev_replace finished",
            ],
        );
    }

    #[test]
    fn wait_for_kernel_replace_resets_heartbeat_clock_on_pct_change() {
        /*
        Intent: a pct change emits an unsuffixed progress line and resets the
        heartbeat clock.
        Why it exists: without the reset, a stalled-then-progressing replace could
        emit a stale heartbeat one poll after real progress.
        Scenario: recover observes 149 polls at 50.0%, two polls at 50.5%, then a
        finished poll under a noop sleeper.
        */
        let mut items = running_runs(149, "50.0%");
        items.extend(running_runs(2, "50.5%"));
        items.push(ReplaceStatusItem::Output(ok_raw(
            "btrfs replace status -1 /mnt/storage",
            "Started on 27.Feb 10:30:00, finished on 27.Feb 10:35:00, 0 write errs, 0 uncorr. read errs\n",
        )));
        let runner = ReplaceStatusSequenceRunner::new(items);
        let mount_point = MountPoint::new("/mnt/storage".into());
        let captured = crate::status_tag::testing::capture_with(|| {
            wait_for_kernel_replace_to_finish(&runner, &mount_point, &progress::NoopSleeper, false)
                .unwrap();
        });
        assert_eq!(
            captured.lines().collect::<Vec<_>>(),
            vec![
                "[wait] pool: waiting for kernel dev_replace to finish...",
                "  ... 50.0%",
                "  ... 50.5%",
                "[ok]   pool: kernel dev_replace finished",
            ],
        );
    }

    fn mountpoint_ok() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".into()).into(),
            },
            ok_raw_empty("mountpoint"),
        )
    }

    fn mountpoint_fail() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".into()).into(),
            },
            err_raw("mountpoint", 1, ""),
        )
    }

    fn btrfs_show_toshiba_and_mystery() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-mystery\n",
        )
    }

    fn btrfs_show_two_disks() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n",
        )
    }

    fn btrfs_show_two_disks_and_foreign() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n\
             \tdevid    3 size 10.00GiB used 2.00GiB path /dev/mapper/luks-foreign\n",
        )
    }

    fn btrfs_show_three_disks() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n\
             \tdevid    3 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk3\n",
        )
    }

    fn btrfs_show_disk1_and_disk2_devid4() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    4 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n",
        )
    }

    fn btrfs_show_disk1_and_drifted_disk2_devid4() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    4 size 10.00GiB used 2.00GiB path /dev/mapper/braid-WRONG\n",
        )
    }

    fn btrfs_show_disk1_disk2_devid4_disk3_devid5() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    4 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n\
             \tdevid    5 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk3\n",
        )
    }

    fn btrfs_show_disk1_disk2_devid4_and_foreign() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    4 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n\
             \tdevid    9 size 10.00GiB used 2.00GiB path /dev/mapper/luks-foreign\n",
        )
    }

    fn btrfs_show_disk1_disk2_devid4_disk3_devid5_and_foreign() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 4 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    4 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n\
             \tdevid    5 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk3\n\
             \tdevid    9 size 10.00GiB used 2.00GiB path /dev/mapper/luks-foreign\n",
        )
    }

    fn btrfs_show_one_disk() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 1 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n",
        )
    }

    fn btrfs_show_zero_devices() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n",
        )
    }

    fn cryptsetup_status_active(mapper: &str, device: &str) -> RawCommandOutput {
        ok_raw(
            &format!("cryptsetup status {mapper}"),
            &format!(
                "/dev/mapper/{mapper} is active and is in use.\n\
                 \ttype:    LUKS2\n\
                 \tcipher:  aes-xts-plain64\n\
                 \tdevice:  {device}\n\
                 \tsector size:  512\n"
            ),
        )
    }

    fn cryptsetup_uuid_ok(device: &str, uuid: &str) -> RawCommandOutput {
        ok_raw(
            &format!("cryptsetup luksUUID {device}"),
            &format!("{uuid}\n"),
        )
    }

    fn already_mounted_two_disks_and_foreign_runner() -> MockRunner {
        let (mp_req, mp_out) = mountpoint_ok();
        MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks_and_foreign(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("luks-foreign".into()),
                },
                cryptsetup_status_active("luks-foreign", "/dev/vdc"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into(),
                },
                cryptsetup_uuid_ok("/dev/vdc", "99999999-9999-9999-9999-999999999999"),
            )
    }

    const POOL_JSON_ADDED_AT: &str = "2024-06-15T12:34:56Z";
    const JOURNAL_ADDED_AT: &str = "2023-08-30T10:00:00Z";
    const LEGACY_JOURNAL_ADDED_AT: &str = "2023-01-01T00:00:00Z";

    fn disk_name(name: &str) -> DiskName {
        DiskName::parse(name).expect("valid fixture disk name")
    }

    fn by_id_path(path: &str) -> ByIdPath {
        ByIdPath::parse(path).expect("valid fixture by-id path")
    }

    fn uuid_raw(raw: &str) -> LuksUuid {
        LuksUuid::parse(raw).expect("valid fixture UUID")
    }

    fn uuid_for_name(name: &str) -> LuksUuid {
        match name {
            "disk1" | "toshiba" => uuid_raw("11111111-1111-1111-1111-111111111111"),
            "disk2" | "old" => uuid_raw("22222222-2222-2222-2222-222222222222"),
            "disk3" => uuid_raw("33333333-3333-3333-3333-333333333333"),
            "new" => uuid_raw("33333333-3333-3333-3333-333333333333"),
            "luks-foreign" => uuid_raw("99999999-9999-9999-9999-999999999999"),
            other => {
                let seed = other
                    .bytes()
                    .fold(0u64, |acc, b| acc.wrapping_mul(131).wrapping_add(b as u64));
                let seed = (seed & 0xffff_ffff_ffff).max(1);
                LuksUuid::parse(&format!("00000000-0000-0000-0000-{seed:012x}"))
                    .expect("derived fixture UUID")
            }
        }
    }

    fn disk_member_named(
        name: &str,
        by_id: &str,
        added_at: Option<&str>,
        devid: Option<Devid>,
    ) -> DiskMember {
        DiskMember {
            name: disk_name(name),
            by_id: by_id_path(by_id),
            devid,
            added_at: added_at.map(str::to_owned),
        }
    }

    fn membership_from(entries: Vec<(LuksUuid, DiskMember)>) -> PoolMembership {
        let mut membership = PoolMembership::empty();
        for (uuid, member) in entries {
            membership
                .insert(uuid, member)
                .expect("insert fixture member");
        }
        membership
    }

    fn membership_entry(
        name: &str,
        by_id: &str,
        added_at: Option<&str>,
        devid: Option<Devid>,
    ) -> (LuksUuid, DiskMember) {
        (
            uuid_for_name(name),
            disk_member_named(name, by_id, added_at, devid),
        )
    }

    fn membership_name_list(membership: &PoolMembership) -> Vec<String> {
        let mut names: Vec<String> = membership
            .names()
            .map(|name| name.as_str().to_owned())
            .collect();
        names.sort();
        names
    }

    fn add_target(
        name: &str,
        by_id: &str,
        mode: journal::AddJournalMode,
    ) -> journal::AddJournalTarget {
        journal::AddJournalTarget {
            name: disk_name(name),
            by_id: by_id_path(by_id),
            mode,
        }
    }

    fn add_targets(
        entries: Vec<(LuksUuid, journal::AddJournalTarget)>,
    ) -> crate::membership::LuksUuidMap<journal::AddJournalTarget> {
        let mut targets = crate::membership::LuksUuidMap::new();
        for (uuid, target) in entries {
            targets.insert(uuid, target).expect("insert add target");
        }
        targets
    }

    fn fresh_mode(
        extras: Vec<String>,
        enroll_key_file: Option<std::path::PathBuf>,
    ) -> journal::AddJournalMode {
        let extras = strip_legacy_managed_format_opts(extras);
        journal::AddJournalMode::FreshLuks {
            extra_opts: LuksFormatExtraOpts::parse(&extras).expect("valid fixture extra opts"),
            enroll_key_file: enroll_key_file.map(KeyFilePath::new),
        }
    }

    fn strip_legacy_managed_format_opts(extras: Vec<String>) -> Vec<String> {
        let mut out = Vec::new();
        let mut iter = extras.into_iter();
        while let Some(token) = iter.next() {
            if token == "--label" || token == "--uuid" {
                let _ = iter.next();
                continue;
            }
            if token.starts_with("--label=") || token.starts_with("--uuid=") {
                continue;
            }
            out.push(token);
        }
        out
    }

    fn one_disk_membership() -> PoolMembership {
        membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            None,
        )])
    }

    fn acked_disk(missing_acked: bool, read_io_errs: u64) -> alert::AckedDisk {
        alert::AckedDisk {
            missing_acked,
            device_stats: alert::DeviceErrorCounters {
                read_io_errs,
                ..Default::default()
            },
        }
    }

    fn seed_acked_stats(paths: &StatePaths, entries: &[(u64, alert::AckedDisk)]) {
        let map = entries
            .iter()
            .map(|(devid, disk)| (devid.to_string(), disk.clone()))
            .collect();
        alert::save_acked_stats(&alert::AckedStats(map), paths).unwrap();
    }

    fn bootstrap_pool_mutation_add_journal() -> journal::Journal {
        bootstrap_pool_mutation_add_journal_for(&["disk1", "disk2"])
    }

    fn bootstrap_pool_mutation_add_journal_for(names: &[&str]) -> journal::Journal {
        let pre = PoolMembership::empty();
        let target = membership_from(
            names
                .iter()
                .map(|name| {
                    membership_entry(name, &format!("/dev/disk/by-id/virtio-{name}"), None, None)
                })
                .collect(),
        );
        let targets = add_targets(
            names
                .iter()
                .copied()
                .map(|name| {
                    (
                        uuid_for_name(name),
                        add_target(
                            name,
                            &format!("/dev/disk/by-id/virtio-{name}"),
                            fresh_mode(Vec::new(), None),
                        ),
                    )
                })
                .collect(),
        );
        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Add {
                phase: journal::AddPhase::PoolMutation,
                targets,
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    fn test_recovery_admission_membership(journal: &journal::Journal) -> PoolMembership {
        recovery_admission_membership(journal)
            .expect("fixture admission membership should be valid")
    }

    fn recover_work_plan_for_journal(journal: journal::Journal) -> RecoverWorkPlan {
        let union = test_recovery_admission_membership(&journal);
        RecoverWorkPlan {
            open_plan: None,
            pre_resolved_credential: None,
            journal,
            admission_membership: union,
            mount_point: MountPoint::new("/mnt/storage".into()),
            pool_json_path: std::path::PathBuf::from("/var/lib/braid/pool.json"),
            pending_op_path: std::path::PathBuf::from("/var/lib/braid/pending-op.json"),
            luks_headers_dir: std::path::PathBuf::from("/var/lib/braid/luks-headers"),
            actions: Vec::new(),
        }
    }

    fn add_op_from_disks(disks: BTreeMap<String, ByIdPath>) -> OpKind {
        let mut targets = crate::membership::LuksUuidMap::new();
        for (name, by_id) in disks {
            targets
                .insert(
                    uuid_for_name(&name),
                    journal::AddJournalTarget {
                        name: disk_name(&name),
                        by_id,
                        mode: fresh_mode(Vec::new(), None),
                    },
                )
                .expect("insert add op target");
        }
        OpKind::Add {
            phase: journal::AddPhase::PostAddBalanceRaid1,
            targets,
        }
    }

    fn already_mounted_one_disk_runner() -> MockRunner {
        let (mp_req, mp_out) = mountpoint_ok();
        MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_one_disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
    }

    fn interrupted_remove_journal(disk1_added_at: Option<&str>) -> journal::Journal {
        let pre = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                disk1_added_at,
                None,
            ),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);
        let target = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            disk1_added_at,
            None,
        )]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Remove {
                luks_uuid: uuid_for_name("disk2"),
                name: disk_name("disk2"),
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    fn remove_missing_journal() -> journal::Journal {
        let pre = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
        ]);
        let target = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            Some(Devid::new(1)),
        )]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::RemoveMissing {
                phase: journal::RemoveMissingPhase::PoolMutation,
                devid: Devid::new(2),
                restore_raid1_after_commit: true,
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    fn remove_missing_journal_two_survivors() -> journal::Journal {
        let pre = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                None,
                Some(Devid::new(3)),
            ),
        ]);
        let target = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
        ]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::RemoveMissing {
                phase: journal::RemoveMissingPhase::PoolMutation,
                devid: Devid::new(3),
                restore_raid1_after_commit: true,
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    /// Two-disk journal for interrupted add: pre has disk1+disk2, target has disk1+disk2+disk3.
    fn two_disk_journal() -> journal::Journal {
        let pre = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);
        let target = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
            membership_entry("disk3", "/dev/disk/by-id/virtio-disk3", None, None),
        ]);

        let mut add_targets_by_name = BTreeMap::new();
        add_targets_by_name.insert(
            "disk3".to_owned(),
            ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: add_op_from_disks(add_targets_by_name),
            pre_membership: pre,
            target_membership: target,
        }
    }

    /// Add journal for the committed post-add phase: pre has disk1,
    /// target/live pool has disk1+disk2, and recovery only owes balance.
    fn committed_two_disk_add_journal() -> journal::Journal {
        let pre = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            None,
        )]);
        let target = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);

        let mut add_targets_by_name = BTreeMap::new();
        add_targets_by_name.insert(
            "disk2".to_owned(),
            ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: add_op_from_disks(add_targets_by_name),
            pre_membership: pre,
            target_membership: target,
        }
    }

    /// Variant of `recoverable_pool_mutation_add_journal` carrying an
    /// `enroll_key_file: Some(kf)` -- models a crash mid-`add --enroll
    /// DIR` where the journaled plan calls for keyfile enrollment on
    /// the returning braid disk.
    fn recoverable_pool_mutation_add_journal_with_enroll(
        enroll_key_file: std::path::PathBuf,
    ) -> journal::Journal {
        let mut journal = recoverable_pool_mutation_add_journal();
        if let OpKind::Add { targets, .. } = &mut journal.op {
            let uuids: Vec<LuksUuid> = targets.keys().cloned().collect();
            for uuid in uuids {
                let target = targets.get_mut(&uuid).expect("target still present");
                if let journal::AddJournalMode::RecoverableBraidLabeled {
                    enroll_key_file: stored,
                    ..
                } = &mut target.mode
                {
                    *stored = Some(KeyFilePath::new(enroll_key_file.clone()));
                }
            }
        } else {
            unreachable!("returns Add");
        }
        journal
    }

    fn recoverable_pool_mutation_add_journal() -> journal::Journal {
        let pre = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            None,
        )]);
        let target = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);

        let targets = add_targets(vec![(
            uuid_for_name("disk2"),
            add_target(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                journal::AddJournalMode::RecoverableBraidLabeled {
                    verified_pool_fsid: Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
                        .unwrap(),
                    enroll_key_file: None,
                },
            ),
        )]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Add {
                phase: journal::AddPhase::PoolMutation,
                targets,
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    fn two_pre_recoverable_add_disk3_journal() -> journal::Journal {
        let pre = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);
        let target = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
            membership_entry("disk3", "/dev/disk/by-id/virtio-disk3", None, None),
        ]);

        let targets = add_targets(vec![(
            uuid_for_name("disk3"),
            add_target(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                journal::AddJournalMode::RecoverableBraidLabeled {
                    verified_pool_fsid: Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
                        .unwrap(),
                    enroll_key_file: None,
                },
            ),
        )]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Add {
                phase: journal::AddPhase::PoolMutation,
                targets,
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    fn two_target_recoverable_pool_mutation_add_journal() -> journal::Journal {
        let pre = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            None,
        )]);
        let target = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
            membership_entry("disk3", "/dev/disk/by-id/virtio-disk3", None, None),
        ]);

        let targets = add_targets(
            ["disk2", "disk3"]
                .into_iter()
                .map(|name| {
                    (
                        uuid_for_name(name),
                        add_target(
                            name,
                            &format!("/dev/disk/by-id/virtio-{name}"),
                            journal::AddJournalMode::RecoverableBraidLabeled {
                                verified_pool_fsid: Fsid::parse(
                                    "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
                                )
                                .unwrap(),
                                enroll_key_file: None,
                            },
                        ),
                    )
                })
                .collect(),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Add {
                phase: journal::AddPhase::PoolMutation,
                targets,
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    fn mixed_pool_mutation_add_journal() -> journal::Journal {
        let pre = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            None,
        )]);
        let target = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
            membership_entry("disk3", "/dev/disk/by-id/virtio-disk3", None, None),
        ]);

        let targets = add_targets(vec![
            (
                uuid_for_name("disk2"),
                add_target(
                    "disk2",
                    "/dev/disk/by-id/virtio-disk2",
                    journal::AddJournalMode::RecoverableBraidLabeled {
                        verified_pool_fsid: Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
                            .unwrap(),
                        enroll_key_file: None,
                    },
                ),
            ),
            (
                uuid_for_name("disk3"),
                add_target(
                    "disk3",
                    "/dev/disk/by-id/virtio-disk3",
                    fresh_mode(
                        Vec::new(),
                        Some(std::path::PathBuf::from(
                            "/var/lib/braid/keyfiles/braid-disk3.key",
                        )),
                    ),
                ),
            ),
        ]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Add {
                phase: journal::AddPhase::PoolMutation,
                targets,
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    fn fresh_pool_mutation_add_journal(
        luks_format_extra_opts: Vec<String>,
        enroll_key_file: Option<std::path::PathBuf>,
    ) -> journal::Journal {
        let pre = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            None,
        )]);
        let target = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);
        let targets = add_targets(vec![(
            uuid_for_name("disk2"),
            add_target(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                fresh_mode(luks_format_extra_opts, enroll_key_file),
            ),
        )]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Add {
                phase: journal::AddPhase::PoolMutation,
                targets,
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    fn pool_state_one_disk() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-disk1".into()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vda".into(),
            }],
            missing_count: 0,
            total_devices: 1,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    fn pool_state_disk1_and_foreign() -> PoolState {
        let mut pool = pool_state_one_disk();
        pool.devices.push(PoolDevice {
            mapper: MapperName::from_basename("luks-foreign".into()),
            luks_uuid: LuksUuid::parse("99999999-9999-9999-9999-999999999999").unwrap(),
            devid: Devid::new(9),
            underlying: "/dev/vdz".into(),
        });
        pool.total_devices = 2;
        pool
    }

    fn pool_state_two_disks() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                    luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                    devid: Devid::new(1),
                    underlying: "/dev/vda".into(),
                },
                PoolDevice {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                    luks_uuid: LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap(),
                    devid: Devid::new(2),
                    underlying: "/dev/vdb".into(),
                },
            ],
            missing_count: 0,
            total_devices: 2,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    fn pool_state_disk1_and_disk2_devid4() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                    luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                    devid: Devid::new(1),
                    underlying: "/dev/vda".into(),
                },
                PoolDevice {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                    luks_uuid: LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap(),
                    devid: Devid::new(4),
                    underlying: "/dev/vdb".into(),
                },
            ],
            missing_count: 0,
            total_devices: 2,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    fn pool_state_disk1_and_drifted_disk2_devid4() -> PoolState {
        let mut pool = pool_state_disk1_and_disk2_devid4();
        pool.devices[1].mapper = MapperName::from_basename("braid-WRONG".into());
        pool
    }

    fn pool_state_disk1_with_null_underlying_disk2() -> PoolState {
        let mut pool = pool_state_one_disk();
        pool.total_devices = 2;
        pool.null_underlying.push(NullUnderlyingDevice {
            mapper: MapperName::from_basename("braid-disk2".into()),
            luks_uuid: uuid_for_name("disk2"),
            devid: Devid::new(2),
        });
        pool
    }

    fn pool_state_disk1_and_old() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                    luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                    devid: Devid::new(1),
                    underlying: "/dev/vda".into(),
                },
                PoolDevice {
                    mapper: MapperName::from_basename("braid-old".into()),
                    luks_uuid: LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap(),
                    devid: Devid::new(2),
                    underlying: "/dev/vdb".into(),
                },
            ],
            missing_count: 0,
            total_devices: 2,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    fn pool_state_disk1_and_new() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                    luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                    devid: Devid::new(1),
                    underlying: "/dev/vda".into(),
                },
                PoolDevice {
                    mapper: MapperName::from_basename("braid-new".into()),
                    luks_uuid: LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap(),
                    devid: Devid::new(2),
                    underlying: "/dev/vdc".into(),
                },
            ],
            missing_count: 0,
            total_devices: 2,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    fn pool_state_disk1_old_and_new() -> PoolState {
        let mut pool = pool_state_disk1_and_old();
        pool.devices.push(PoolDevice {
            mapper: MapperName::from_basename("braid-new".into()),
            luks_uuid: LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap(),
            devid: Devid::new(3),
            underlying: "/dev/vdc".into(),
        });
        pool.total_devices = 3;
        pool
    }

    fn pool_state_disk1_with_missing_devid2() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-disk1".into()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vda".into(),
            }],
            missing_count: 1,
            total_devices: 2,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![Devid::new(2)],
            null_underlying: vec![],
        }
    }

    fn pool_state_three_disks() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                    luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                    devid: Devid::new(1),
                    underlying: "/dev/vda".into(),
                },
                PoolDevice {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                    luks_uuid: LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap(),
                    devid: Devid::new(2),
                    underlying: "/dev/vdb".into(),
                },
                PoolDevice {
                    mapper: MapperName::from_basename("braid-disk3".into()),
                    luks_uuid: LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap(),
                    devid: Devid::new(3),
                    underlying: "/dev/vdc".into(),
                },
            ],
            missing_count: 0,
            total_devices: 3,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    fn luks_dump_label(label: &str) -> RawCommandOutput {
        ok_raw(
            "cryptsetup luksDump",
            &format!("LUKS header information\nVersion:       \t2\nLabel:         \t{label}\n"),
        )
    }

    fn luks_dump_text_request_count(requests: &[CmdRequest], device: &str) -> usize {
        requests
            .iter()
            .filter(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksDumpText { device: requested }
                        if requested == device
                )
            })
            .count()
    }

    fn btrfs_show_target_fsid(fsid: &str) -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show target",
            &format!(
                "Label: none  uuid: {fsid}\n\
                 \tTotal devices 1 FS bytes used 1.00GiB\n\
                 \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n"
            ),
        )
    }

    fn btrfs_show_target_no_btrfs(target: &str) -> RawCommandOutput {
        err_raw(
            "btrfs filesystem show target",
            1,
            &format!("not a valid btrfs filesystem on {target}"),
        )
    }

    fn with_one_disk_pool_probe(runner: MockRunner) -> MockRunner {
        runner
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_one_disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
    }

    fn with_two_disk_pool_probe(runner: MockRunner) -> MockRunner {
        runner
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
    }

    /// Device-mapper table for the pool `with_two_disk_pool_probe` reports,
    /// for tests that reach a fresh probe of the live pool.
    fn two_disk_pool_fs() -> MockFs {
        MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222")
    }

    fn with_disk1_disk2_devid4_pool_probe(runner: MockRunner) -> MockRunner {
        runner
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_disk1_and_disk2_devid4(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
    }

    fn with_disk1_drifted_disk2_devid4_pool_probe(runner: MockRunner) -> MockRunner {
        runner
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_disk1_and_drifted_disk2_devid4(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-WRONG".into()),
                },
                cryptsetup_status_active("braid-WRONG", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
    }

    fn with_disk1_disk2_devid4_disk3_devid5_pool_probe(runner: MockRunner) -> MockRunner {
        runner
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_disk1_disk2_devid4_disk3_devid5(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk3".into()),
                },
                cryptsetup_status_active("braid-disk3", "/dev/vdc"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into(),
                },
                cryptsetup_uuid_ok("/dev/vdc", "33333333-3333-3333-3333-333333333333"),
            )
    }

    fn with_three_disk_pool_probe(runner: MockRunner) -> MockRunner {
        runner
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_three_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk3".into()),
                },
                cryptsetup_status_active("braid-disk3", "/dev/vdc"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into(),
                },
                cryptsetup_uuid_ok("/dev/vdc", "33333333-3333-3333-3333-333333333333"),
            )
    }

    fn with_idle_balance_status(runner: MockRunner) -> MockRunner {
        runner.with_output(
            CmdRequest::BtrfsBalanceStatus {
                mount_point: MountPoint::new("/mnt/storage".into()),
            },
            ok_raw(
                "btrfs balance status",
                "No balance found on '/mnt/storage'\n",
            ),
        )
    }

    /// Idle balance plus the soft convert, for replays that owe
    /// `OwedRaid1Balance::RestoreSingle`. A replay that ran the hard convert
    /// instead fails on the missing mock.
    fn with_restore_balance_replay(runner: MockRunner) -> MockRunner {
        with_idle_balance_status(runner).with_output(
            CmdRequest::BtrfsBalanceRaid1Soft {
                mount_point: MountPoint::new("/mnt/storage".into()),
            },
            ok_raw_empty("btrfs balance start"),
        )
    }

    /// Idle balance plus the hard convert, for live-add replays that owe
    /// `OwedRaid1Balance::Grow`. A replay that ran the soft convert instead
    /// fails on the missing mock.
    fn with_grow_balance_replay(runner: MockRunner) -> MockRunner {
        with_idle_balance_status(runner).with_output(
            CmdRequest::BtrfsBalanceRaid1 {
                mount_point: MountPoint::new("/mnt/storage".into()),
            },
            ok_raw_empty("btrfs balance start"),
        )
    }

    // Intent
    // Bootstrap add recovery deletes every pre-existing acked-stats entry.
    //
    // Why it exists
    // A recovered bootstrap creates a new pool identity; old-pool devid
    // baselines must not attach to the new disks.
    //
    // Scenario
    // A pool bootstrap crashes after btrfs creates the filesystem but before
    // `cmd_add` clears acked-stats. Recovery completes the bootstrap.
    #[test]
    fn bootstrap_recovery_clears_acked_stats() {
        let f = PoolFixture::empty();
        let journal = bootstrap_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        seed_acked_stats(
            &f.paths,
            &[
                (1, acked_disk(false, 11)),
                (2, acked_disk(true, 22)),
                (7, acked_disk(false, 77)),
            ],
        );
        let runner = with_two_disk_pool_probe(with_restore_balance_replay(MockRunner::default()));
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().passphrase_file(None).build();
        let plan = recover_work_plan_for_journal(journal);

        execute_generic_live_pool_recovery(
            &runner,
            &two_disk_pool_fs(),
            &resolver,
            &params,
            &plan,
            pool_state_two_disks(),
            Some(OwedRaid1Balance::RestoreSingle),
        )
        .expect("bootstrap recovery should clear acked-stats and finish");

        assert!(
            !f.paths.acked_stats_json().exists(),
            "bootstrap recovery must delete stale acked-stats.json"
        );
    }

    // Intent
    // Bootstrap-add GenericLivePool recovery holds a sleep inhibitor while it
    // replays the owed RAID1 soft balance.
    //
    // Why it exists
    // The generic live-pool recovery path is easy to miss because the sibling
    // post-maintenance replay paths acquire their inhibitors at separate call
    // sites. This pins the direct helper contract instead of relying only on
    // cmd_recover entry-point coverage.
    //
    // Scenario
    // A 2-disk bootstrap add crashed after btrfs created the filesystem.
    // Recovery rebuilds membership from the live pool and replays the owed
    // post-add soft balance under a sleep inhibitor.
    #[test]
    fn bootstrap_recovery_holds_inhibitor_across_balance() {
        let f = PoolFixture::empty();
        let journal = bootstrap_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = with_two_disk_pool_probe(with_restore_balance_replay(MockRunner::default()));
        let inhibitor = RequestCountInhibitor::new(runner.clone());
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&inhibitor)
            .build();
        let plan = recover_work_plan_for_journal(journal);

        execute_generic_live_pool_recovery(
            &runner,
            &two_disk_pool_fs(),
            &resolver,
            &params,
            &plan,
            pool_state_two_disks(),
            Some(OwedRaid1Balance::RestoreSingle),
        )
        .expect("bootstrap recovery should replay owed maintenance under inhibitor");

        let requests = runner.requests();
        let balance_index = requests
            .iter()
            .position(|r| {
                matches!(
                    r,
                    CmdRequest::BtrfsBalanceRaid1Soft { mount_point }
                        if mount_point.as_str() == "/mnt/storage"
                )
            })
            .expect("generic bootstrap recovery must issue post-add soft RAID1 balance");
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "bootstrap recovery must acquire one sleep inhibitor"
        );
        assert!(
            inhibitor.first_acquire_request_count().unwrap() <= balance_index,
            "sleep inhibitor must be acquired before the soft balance; \
             balance_index={balance_index}, requests={requests:?}"
        );
        assert!(
            inhibitor.drop_request_count().unwrap() > balance_index,
            "sleep inhibitor guard must stay held across the soft balance; \
             balance_index={balance_index}, requests={requests:?}"
        );
        assert!(
            !f.paths.pending_op_json().exists(),
            "journal must clear after successful maintenance replay"
        );
    }

    // Intent
    // GenericLivePool Remove recovery does not acquire a sleep inhibitor when
    // it has no owed RAID1 maintenance to replay.
    //
    // Why it exists
    // The generic helper handles both bootstrap-add replay and remove cleanup.
    // A future unconditional acquire would expand inhibitor coverage onto a
    // local-file-only remove tail that performs no interruptible btrfs work.
    //
    // Scenario
    // A remove journal reaches the generic live-pool path with the live pool
    // still showing both disks. Recovery rebuilds membership and clears the
    // journal without replaying btrfs maintenance.
    #[test]
    fn generic_recovery_remove_arm_does_not_acquire_inhibitor() {
        let f = PoolFixture::empty();
        let journal = remove_2to1_journal_with_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default();
        let inhibitor = RequestCountInhibitor::new(runner.clone());
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&inhibitor)
            .build();
        let plan = recover_work_plan_for_journal(journal);

        execute_generic_live_pool_recovery(
            &runner,
            &MockFs::new(&[]),
            &resolver,
            &params,
            &plan,
            pool_state_two_disks(),
            None,
        )
        .expect("generic remove recovery should finish without inhibitor");

        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "remove recovery without owed RAID1 replay must not acquire a sleep inhibitor"
        );
        assert!(
            !f.paths.pending_op_json().exists(),
            "journal must clear after generic remove recovery"
        );
    }

    // Intent
    // Bootstrap-add GenericLivePool recovery aborts before balance replay when
    // sleep-inhibitor acquisition fails.
    //
    // Why it exists
    // Calling acquire is not enough: recovery must propagate acquisition
    // failure so it never runs interruptible btrfs work without a valid
    // inhibitor, and the journal must remain for retry.
    //
    // Scenario
    // A 2-disk bootstrap add crashed after btrfs created the filesystem.
    // Recovery rebuilds membership from the live pool, but logind refuses the
    // sleep inhibitor immediately before the owed soft balance would start.
    #[test]
    fn bootstrap_recovery_inhibitor_failure_aborts_before_balance() {
        let f = PoolFixture::empty();
        let journal = bootstrap_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = with_restore_balance_replay(MockRunner::default());
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let inhibitor = FailingInhibitor;
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&inhibitor)
            .build();
        let plan = recover_work_plan_for_journal(journal);

        let err = execute_generic_live_pool_recovery(
            &runner,
            &MockFs::new(&[]),
            &resolver,
            &params,
            &plan,
            pool_state_two_disks(),
            Some(OwedRaid1Balance::RestoreSingle),
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("could not acquire sleep inhibitor"),
            "expected sleep inhibitor failure, got: {err}",
        );
        assert!(
            !runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsBalanceStatus { .. } | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "inhibitor failure must stop before balance probing or replay"
        );
        assert!(
            f.paths.pending_op_json().exists(),
            "journal must survive for an idempotent retry"
        );
    }

    fn replay_returned_disk2_runner_base() -> MockRunner {
        MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
            .with_mapper_open(
                "braid-disk2",
                "/dev/vdb",
                "22222222-2222-2222-2222-222222222222",
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk2".into(),
                },
                btrfs_show_target_no_btrfs("/dev/mapper/braid-disk2"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec!["/dev/mapper/braid-disk2".into()],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::WipefsBtrfs {
                    device: "/dev/mapper/braid-disk2".into(),
                },
                ok_raw_empty("wipefs"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceAdd {
                    device: "/dev/mapper/braid-disk2".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                    force: true,
                },
                ok_raw_empty("btrfs device add"),
            )
    }

    fn replay_returned_disk2_runner_for_devid4() -> MockRunner {
        with_grow_balance_replay(with_disk1_disk2_devid4_pool_probe(
            replay_returned_disk2_runner_base(),
        ))
    }

    fn replay_returned_disk2_runner_closed_mapper_for_devid4() -> MockRunner {
        let inactive = inactive_mapper_status("braid-disk2");
        replay_returned_disk2_runner_for_devid4()
            .with_output_sequence(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                vec![inactive.clone(), inactive],
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
    }

    fn replay_returned_disk2_runner_for_drifted_devid4() -> MockRunner {
        with_grow_balance_replay(with_disk1_drifted_disk2_devid4_pool_probe(
            replay_returned_disk2_runner_base(),
        ))
    }

    // Intent
    // Live-add recovery on an already-mounted pool with a closed mapper
    // and a pending pool_add_device prompts for the LUKS passphrase
    // exactly once, not twice.
    //
    // Why it exists
    // Principle 4 (docs/design/decisions/004-single-passphrase.md) commits to
    // "one passphrase, all drives unlock". Independent recover_passphrase
    // calls in the discovery and replay blocks of
    // execute_add_pool_mutation_recovery used to prompt twice; a future
    // refactor could reintroduce the double prompt without this guard.
    //
    // Scenario
    // sudo braid recover on a mounted pool with an interrupted add: disk2
    // mapper is closed (discovery must open it) AND disk2 is not yet a
    // pool member (replay must run pool_add_device). The operator sees
    // one prompt, the replay reuses the cached passphrase, btrfs device
    // add commits the target.
    #[test]
    fn live_add_recovery_prompts_passphrase_once_when_mapper_closed() {
        let f = PoolFixture::empty();
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let runner = replay_returned_disk2_runner_closed_mapper_for_devid4();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let pass = std::str::from_utf8(TEST_PASSPHRASE_BYTES).unwrap();
        let reader = ScriptedPassphraseReader::new([pass, pass]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .tty(&reader)
            .build();

        execute_add_pool_mutation_recovery(
            &runner,
            &MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222"),
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("live-add replay should finish with one prompt");

        assert!(
            runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsDeviceAdd { device, .. }
                    if device == "/dev/mapper/braid-disk2"
            )),
            "replay block must reach pool_add_device for disk2"
        );
        assert_eq!(
            reader.remaining(),
            1,
            "passphrase must be prompted exactly once -- second prompt is a Principle-4 regression"
        );
    }

    // Intent
    // Live-add recovery drops the replayed target's assigned devid inside
    // the replay loop.
    //
    // Why it exists
    // If btrfs reuses a removed max devid, the fresh holder must not inherit
    // the old disk's acked baseline during partial add recovery.
    //
    // Scenario
    // Recovery replays `pool_add_device` for disk2 and btrfs assigns reused
    // devid 4 while unrelated devid 1 ack state already exists.
    #[test]
    fn live_add_recovery_drops_ghost_for_reused_devid_via_replay() {
        let f = PoolFixture::empty();
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let control = acked_disk(false, 11);
        seed_acked_stats(
            &f.paths,
            &[(1, control.clone()), (4, acked_disk(false, 44))],
        );
        let runner = replay_returned_disk2_runner_for_devid4();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().build();

        execute_add_pool_mutation_recovery(
            &runner,
            &MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222"),
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("live-add replay should finish");

        assert!(
            runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceAdd { device, .. } if device == "/dev/mapper/braid-disk2")),
            "recovery should replay the target add"
        );
        let reloaded = alert::load_acked_stats(&f.paths);
        assert_eq!(reloaded.0.get("1"), Some(&control));
        assert!(
            !reloaded.0.contains_key("4"),
            "replayed target's reused devid must be dropped"
        );
    }

    // Intent: live-add recovery's replay loop succeeds when the post-replay
    // probe reports the replayed target under a drifted mapper but the
    // journaled LUKS UUID is present in the live pool.
    //
    // Why it exists: recovery's ack-cleanup devid lookup must be UUID-keyed
    // per decision 024. A reverted-to-mapper-keyed replay loop would crash
    // recovery exactly when drift-tolerance matters most.
    //
    // Scenario: recovery replays `pool_add_device` for disk2; the post-replay
    // probe reports it as `braid-WRONG` carrying the journaled UUID. The
    // reused-devid ghost is still dropped.
    #[test]
    fn live_add_recovery_drops_ghost_under_drifted_mapper_via_replay() {
        let f = PoolFixture::empty();
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let control = acked_disk(false, 11);
        seed_acked_stats(
            &f.paths,
            &[(1, control.clone()), (4, acked_disk(false, 44))],
        );
        let runner = replay_returned_disk2_runner_for_drifted_devid4();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().build();

        execute_add_pool_mutation_recovery(
            &runner,
            &MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"])
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222")
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-WRONG", "22222222-2222-2222-2222-222222222222"),
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("live-add replay should tolerate mapper drift");

        assert!(
            runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceAdd { device, .. } if device == "/dev/mapper/braid-disk2")),
            "recovery should replay the target add"
        );
        let reloaded = alert::load_acked_stats(&f.paths);
        assert_eq!(reloaded.0.get("1"), Some(&control));
        assert!(
            !reloaded.0.contains_key("4"),
            "replayed target's reused devid must be dropped under mapper drift"
        );
    }

    // Intent
    // Live-add recovery sweeps ghosts for targets already live at recovery
    // entry when the replay loop is skipped entirely.
    //
    // Why it exists
    // Per-arm cleanup only runs inside replay; committed-but-closed targets
    // need a pre-save sweep to close the crash window.
    //
    // Scenario
    // Disk2 was added to btrfs at reused devid 4 before the crash, so
    // recovery sees all targets live and does not call `btrfs device add`.
    #[test]
    fn live_add_recovery_drops_ghost_for_committed_but_closed_target() {
        let f = PoolFixture::empty();
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let control = acked_disk(false, 11);
        seed_acked_stats(
            &f.paths,
            &[(1, control.clone()), (4, acked_disk(false, 44))],
        );
        let runner =
            with_disk1_disk2_devid4_pool_probe(with_grow_balance_replay(MockRunner::default()));
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_add_pool_mutation_recovery(
            &runner,
            &MockFs::new(&[])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222"),
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_disk1_and_disk2_devid4(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("all-live add recovery should finish");

        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceAdd { .. })),
            "all-live recovery must not replay btrfs device add"
        );
        let reloaded = alert::load_acked_stats(&f.paths);
        assert_eq!(reloaded.0.get("1"), Some(&control));
        assert!(
            !reloaded.0.contains_key("4"),
            "sweep must drop the committed target's reused devid"
        );
    }

    // Intent: live-add recovery's all-live sweep succeeds when the live pool
    // reports a journaled target under a drifted mapper but its UUID is present.
    //
    // Why it exists: `sweep_recovered_add_acked_stats` must resolve every
    // journaled target by UUID, not by reconstructed `braid-<name>` mapper.
    //
    // Scenario: disk2 was added to btrfs at reused devid 4 before the crash,
    // so recovery sees all targets live and skips replay. The live pool reports
    // disk2 under `braid-WRONG`; the sweep still drops the reused-devid ghost.
    #[test]
    fn live_add_recovery_drops_ghost_under_drifted_mapper_committed_but_closed() {
        let f = PoolFixture::empty();
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let control = acked_disk(false, 11);
        seed_acked_stats(
            &f.paths,
            &[(1, control.clone()), (4, acked_disk(false, 44))],
        );
        let runner = with_disk1_drifted_disk2_devid4_pool_probe(with_grow_balance_replay(
            MockRunner::default(),
        ));
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_add_pool_mutation_recovery(
            &runner,
            &MockFs::new(&[])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-WRONG", "22222222-2222-2222-2222-222222222222"),
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_disk1_and_drifted_disk2_devid4(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("all-live add recovery should tolerate mapper drift");

        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceAdd { .. })),
            "all-live recovery must not replay btrfs device add"
        );
        let reloaded = alert::load_acked_stats(&f.paths);
        assert_eq!(reloaded.0.get("1"), Some(&control));
        assert!(
            !reloaded.0.contains_key("4"),
            "sweep must drop the committed target's reused devid under mapper drift"
        );
    }

    // Intent
    // Live-add recovery sweeps ghosts for targets skipped by the per-target
    // live-member `continue` while other targets replay.
    //
    // Why it exists
    // Mixed batches can have one target already live and another still
    // missing; per-arm cleanup covers only the replayed target.
    //
    // Scenario
    // Disk2 is already live at reused devid 4, disk3 replays and gets devid
    // 5, and both stale ack entries must be removed.
    #[test]
    fn live_add_recovery_drops_ghosts_for_mixed_batch() {
        let f = PoolFixture::empty();
        let journal = two_target_recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let control = acked_disk(false, 11);
        seed_acked_stats(
            &f.paths,
            &[
                (1, control.clone()),
                (4, acked_disk(false, 44)),
                (5, acked_disk(false, 55)),
            ],
        );
        let runner = with_grow_balance_replay(with_disk1_disk2_devid4_disk3_devid5_pool_probe(
            MockRunner::default()
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk3".into(),
                    },
                    cryptsetup_uuid_ok(
                        "/dev/disk/by-id/virtio-disk3",
                        "33333333-3333-3333-3333-333333333333",
                    ),
                )
                .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk3")
                .with_mapper_open(
                    "braid-disk3",
                    "/dev/vdc",
                    "33333333-3333-3333-3333-333333333333",
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/vda".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/vdb".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/disk/by-id/virtio-disk3".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output(
                    CmdRequest::BtrfsFilesystemShowTarget {
                        target: "/dev/mapper/braid-disk3".into(),
                    },
                    btrfs_show_target_no_btrfs("/dev/mapper/braid-disk3"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceScanForget {
                        devices: vec!["/dev/mapper/braid-disk3".into()],
                    },
                    ok_raw_empty("btrfs device scan --forget"),
                )
                .with_output(
                    CmdRequest::WipefsBtrfs {
                        device: "/dev/mapper/braid-disk3".into(),
                    },
                    ok_raw_empty("wipefs"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceAdd {
                        device: "/dev/mapper/braid-disk3".into(),
                        mount_point: MountPoint::new("/mnt/storage".into()),
                        force: true,
                    },
                    ok_raw_empty("btrfs device add"),
                ),
        ));
        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk2"),
            ("/dev/vdc", "virtio-disk3"),
        ]);
        let params = f.recover_params().build();

        execute_add_pool_mutation_recovery(
            &runner,
            &MockFs::new(&["/dev/disk/by-id/virtio-disk3", "/dev/mapper/braid-disk3"])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222")
                .with_luks_dm("braid-disk3", "33333333-3333-3333-3333-333333333333"),
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_disk1_and_disk2_devid4(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("mixed add recovery should finish");

        assert!(
            runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceAdd { device, .. } if device == "/dev/mapper/braid-disk3")),
            "recovery should replay only the missing disk3 target"
        );
        let reloaded = alert::load_acked_stats(&f.paths);
        assert_eq!(reloaded.0.get("1"), Some(&control));
        assert!(!reloaded.0.contains_key("4"));
        assert!(!reloaded.0.contains_key("5"));
    }

    // Intent: add PoolMutation recovery stops replaying a multi-target batch
    // when a foreign live member appears after one target is re-added.
    // Why it exists: the per-target fail-closed gate must stop further
    // pool_add_device mutations before the terminal membership builder fails.
    // Scenario: disk2 is replayed, a stray LUKS mapper joins the live btrfs
    // pool during the re-probe, and recovery must not add disk3 afterward.
    #[test]
    fn live_add_recovery_stops_mid_batch_when_foreign_member_surfaces() {
        let f = PoolFixture::empty();
        let journal = two_target_recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let mount_point = MountPoint::new("/mnt/storage".into());
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
            .with_mapper_open(
                "braid-disk2",
                "/dev/vdb",
                "22222222-2222-2222-2222-222222222222",
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk2".into(),
                },
                btrfs_show_target_no_btrfs("/dev/mapper/braid-disk2"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec!["/dev/mapper/braid-disk2".into()],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::WipefsBtrfs {
                    device: "/dev/mapper/braid-disk2".into(),
                },
                ok_raw_empty("wipefs"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceAdd {
                    device: "/dev/mapper/braid-disk2".into(),
                    mount_point: mount_point.clone(),
                    force: true,
                },
                ok_raw_empty("btrfs device add"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk3".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk3",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk3")
            .with_mapper_open(
                "braid-disk3",
                "/dev/vdc",
                "33333333-3333-3333-3333-333333333333",
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk3".into(),
                },
                btrfs_show_target_no_btrfs("/dev/mapper/braid-disk3"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk3".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec!["/dev/mapper/braid-disk3".into()],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::WipefsBtrfs {
                    device: "/dev/mapper/braid-disk3".into(),
                },
                ok_raw_empty("wipefs"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceAdd {
                    device: "/dev/mapper/braid-disk3".into(),
                    mount_point: mount_point.clone(),
                    force: true,
                },
                ok_raw_empty("btrfs device add"),
            )
            .with_output_sequence(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: mount_point.clone(),
                },
                vec![
                    btrfs_show_disk1_disk2_devid4_and_foreign(),
                    btrfs_show_disk1_disk2_devid4_disk3_devid5_and_foreign(),
                ],
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: mount_point.clone(),
                },
                btrfs_show_disk1_disk2_devid4_disk3_devid5_and_foreign(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("luks-foreign".into()),
                },
                cryptsetup_status_active("luks-foreign", "/dev/vdz"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdz".into(),
                },
                cryptsetup_uuid_ok("/dev/vdz", "99999999-9999-9999-9999-999999999999"),
            );
        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk2"),
            ("/dev/vdc", "virtio-disk3"),
        ]);
        let params = f.recover_params().build();

        let err = execute_add_pool_mutation_recovery(
            &runner,
            &MockFs::new(&[
                "/dev/disk/by-id/virtio-disk2",
                "/dev/mapper/braid-disk2",
                "/dev/disk/by-id/virtio-disk3",
                "/dev/mapper/braid-disk3",
            ])
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222")
            .with_luks_dm("braid-disk3", "33333333-3333-3333-3333-333333333333")
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("luks-foreign", "99999999-9999-9999-9999-999999999999"),
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("recovery admission membership"),
            "foreign member must fail through the recovery admission gate, got: {msg}"
        );
        assert!(
            f.paths.pending_op_json().exists(),
            "journal must remain for operator recovery after the fail-closed stop"
        );
        let requests = runner.requests();
        let device_adds: Vec<&CmdRequest> = requests
            .iter()
            .filter(|request| matches!(request, CmdRequest::BtrfsDeviceAdd { .. }))
            .collect();
        assert_eq!(
            device_adds.len(),
            1,
            "foreign member surfacing mid-batch must stop before adding disk3: {device_adds:?}"
        );
        assert!(
            device_adds.iter().any(|request| matches!(
                request,
                CmdRequest::BtrfsDeviceAdd { device, .. }
                    if device == "/dev/mapper/braid-disk2"
            )),
            "disk2 is the only target that should be replayed: {device_adds:?}"
        );
    }

    // Intent
    // Committed Remove recovery drops the removed target's acked devid.
    //
    // Why it exists
    // Recovery must mirror the live `cmd_remove` hygiene path after btrfs
    // eviction has committed.
    //
    // Scenario
    // Disk2 was removed from a disk1+disk2 pool but recovery is finishing the
    // bookkeeping after a crash.
    #[test]
    fn remove_recovery_drops_target_devid_when_eviction_committed() {
        let f = PoolFixture::empty();
        let journal = remove_2to1_journal_with_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();
        let plan = recover_work_plan_for_journal(journal);
        let control = acked_disk(false, 11);
        seed_acked_stats(
            &f.paths,
            &[(1, control.clone()), (2, acked_disk(false, 22))],
        );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_generic_live_pool_recovery(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &resolver,
            &params,
            &plan,
            pool_state_one_disk(),
            None,
        )
        .expect("committed remove recovery should finish");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk2")).is_none());
        let reloaded = alert::load_acked_stats(&f.paths);
        assert_eq!(reloaded.0.get("1"), Some(&control));
        assert!(!reloaded.0.contains_key("2"));
    }

    // Intent
    // Uncommitted Remove recovery preserves the target's acked-stats entry.
    //
    // Why it exists
    // Recovery restores targets still owned by btrfs; dropping their acked
    // state would erase a legitimate live-disk baseline.
    //
    // Scenario
    // Disk2's remove did not commit and the mapper is now null-underlying, so
    // recovery keeps disk2 in pool.json.
    #[test]
    fn remove_recovery_preserves_target_devid_when_eviction_uncommitted() {
        let f = PoolFixture::empty();
        let journal = remove_2to1_journal_with_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();
        let plan = recover_work_plan_for_journal(journal);
        let target = acked_disk(false, 22);
        seed_acked_stats(&f.paths, &[(2, target.clone())]);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_generic_live_pool_recovery(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &resolver,
            &params,
            &plan,
            pool_state_disk1_with_null_underlying_disk2(),
            None,
        )
        .expect("uncommitted remove recovery should finish");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk2")).is_some());
        let reloaded = alert::load_acked_stats(&f.paths);
        assert_eq!(reloaded.0.get("2"), Some(&target));
    }

    // Intent
    // Remove recovery with no target devid in the journal skips cleanup.
    //
    // Why it exists
    // Recovery should tolerate older or externally written journals that lack
    // the enrichment added to `cmd_remove`.
    //
    // Scenario
    // The committed remove state is clear, but `pre_membership.disk2.devid`
    // is absent, so there is no safe acked-stats key to drop.
    #[test]
    fn remove_recovery_with_no_devid_journal_skips_cleanup_with_warning() {
        let f = PoolFixture::empty();
        let journal = remove_2to1_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let plan = recover_work_plan_for_journal(journal);
        let target = acked_disk(false, 22);
        seed_acked_stats(&f.paths, &[(2, target.clone())]);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_generic_live_pool_recovery(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &resolver,
            &params,
            &plan,
            pool_state_one_disk(),
            None,
        )
        .expect("remove recovery should tolerate missing target devid");

        let reloaded = alert::load_acked_stats(&f.paths);
        assert_eq!(reloaded.0.get("2"), Some(&target));
    }

    // Intent
    // Remove recovery treats corrupt acked-stats cleanup as warning-only.
    //
    // Why it exists
    // Remove cleanup is hygiene; a corrupt ack file must not strand recovery
    // with a completed btrfs eviction and a preserved journal.
    //
    // Scenario
    // Committed Remove recovery finds non-JSON acked-stats bytes while
    // clearing the journal.
    #[test]
    fn remove_recovery_warning_only_on_corrupt_acked_stats() {
        let f = PoolFixture::empty();
        let journal = remove_2to1_journal_with_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();
        let plan = recover_work_plan_for_journal(journal);
        std::fs::write(f.paths.acked_stats_json(), b"corrupt").unwrap();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_generic_live_pool_recovery(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &resolver,
            &params,
            &plan,
            pool_state_one_disk(),
            None,
        )
        .expect("corrupt acked-stats should warn, not fail remove recovery");

        assert!(!f.paths.pending_op_json().exists());
        assert_eq!(
            std::fs::read(f.paths.acked_stats_json()).unwrap(),
            b"corrupt"
        );
    }

    // Intent
    // RemoveMissing PostMaintenance recovery drops the removed devid's acked
    // entry.
    //
    // Why it exists
    // Recovery must mirror the live `cmd_remove_missing` hygiene path after
    // the remove-missing operation has committed.
    //
    // Scenario
    // `braid remove-missing` crashed after mutation and recovery resumes at
    // PostRemoveMissingMaintenance for devid 2.
    #[test]
    fn remove_missing_post_maintenance_recovery_drops_devid() {
        let f = PoolFixture::empty();
        let mut journal = remove_missing_journal();
        journal.op = OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
            devid: Devid::new(2),
            restore_raid1_after_commit: false,
        };
        journal::write_journal(&f.paths, &journal).unwrap();
        let control = acked_disk(false, 11);
        seed_acked_stats(
            &f.paths,
            &[(1, control.clone()), (2, acked_disk(false, 22))],
        );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_remove_missing_post_maintenance_recovery(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &resolver,
            &params,
            RemoveMissingPostCtx {
                journal: &journal,
                pool: pool_state_one_disk(),
                devid: Devid::new(2),
                owed_balance: None,
                inhibitor_already_held: false,
            },
        )
        .expect("post-maintenance remove-missing recovery should finish");

        let reloaded = alert::load_acked_stats(&f.paths);
        assert_eq!(reloaded.0.get("1"), Some(&control));
        assert!(!reloaded.0.contains_key("2"));
    }

    // Intent
    // RemoveMissing PostMaintenance recovery treats corrupt acked-stats
    // cleanup as warning-only.
    //
    // Why it exists
    // The remove-missing hygiene path should not turn corrupt alert state into
    // a stuck recovery journal.
    //
    // Scenario
    // The post-maintenance journal is ready to clear, but acked-stats.json is
    // non-JSON bytes.
    #[test]
    fn remove_missing_post_maintenance_recovery_warning_only_on_corrupt_acked_stats() {
        let f = PoolFixture::empty();
        let mut journal = remove_missing_journal();
        journal.op = OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
            devid: Devid::new(2),
            restore_raid1_after_commit: false,
        };
        journal::write_journal(&f.paths, &journal).unwrap();
        std::fs::write(f.paths.acked_stats_json(), b"corrupt").unwrap();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_remove_missing_post_maintenance_recovery(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &resolver,
            &params,
            RemoveMissingPostCtx {
                journal: &journal,
                pool: pool_state_one_disk(),
                devid: Devid::new(2),
                owed_balance: None,
                inhibitor_already_held: false,
            },
        )
        .expect("corrupt acked-stats should warn, not fail remove-missing recovery");

        assert!(!f.paths.pending_op_json().exists());
        assert_eq!(
            std::fs::read(f.paths.acked_stats_json()).unwrap(),
            b"corrupt"
        );
    }

    // Intent
    // Bootstrap recovery returns a typed ack cleanup error and preserves the
    // journal when `drop_all_ghost_alert_state` fails.
    //
    // Why it exists
    // The bootstrap add boundary must fail closed so a future recover can
    // retry cleanup instead of silently trusting stale alert baselines.
    //
    // Scenario
    // `acked-stats.json` is a directory, causing the cleanup removal to fail.
    #[test]
    fn bootstrap_recovery_ack_cleanup_failure_returns_typed_error_and_preserves_journal() {
        let f = PoolFixture::empty();
        let journal = bootstrap_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        std::fs::create_dir_all(f.paths.acked_stats_json()).unwrap();
        let plan = recover_work_plan_for_journal(journal);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().passphrase_file(None).build();

        let err = execute_generic_live_pool_recovery(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &resolver,
            &params,
            &plan,
            pool_state_two_disks(),
            Some(OwedRaid1Balance::RestoreSingle),
        )
        .unwrap_err();

        assert!(matches!(
            err,
            RecoverError::AckCleanupFailed {
                stage: "bootstrap-recovery",
                ..
            }
        ));
        assert!(f.paths.pending_op_json().exists());
    }

    // Intent
    // Live-add recovery returns a typed ack cleanup error and preserves the
    // journal when `drop_ghost_alert_state_for_devids` fails.
    //
    // Why it exists
    // Reused-devid cleanup is the add correctness boundary; corrupt
    // acked-stats must abort before the recovery phase handoff.
    //
    // Scenario
    // Recovery replays disk2's add, then the fallible acked-stats loader sees
    // non-JSON bytes.
    #[test]
    fn live_add_recovery_ack_cleanup_failure_returns_typed_error_and_preserves_journal() {
        let f = PoolFixture::empty();
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        std::fs::write(f.paths.acked_stats_json(), b"corrupt").unwrap();
        let runner = replay_returned_disk2_runner_for_devid4();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().build();

        let err = execute_add_pool_mutation_recovery(
            &runner,
            &MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222"),
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .unwrap_err();

        match &err {
            RecoverError::AckCleanupFailed { stage, .. } => {
                assert!(stage.starts_with("live-pool add recovery"));
            }
            other => panic!("expected AckCleanupFailed, got {other:?}"),
        }
        // The operator still sees which devid and which alert file failed.
        let rendered = err.to_string();
        assert!(
            rendered.contains("devid 4: failed to parse")
                && rendered.contains(&f.paths.acked_stats_json().display().to_string()),
            "cleanup failure must name the devid and the failing file: {rendered}"
        );
        assert!(f.paths.pending_op_json().exists());
    }

    // Intent: when the probe after a replayed `btrfs device add` does not
    //   list the target, add recovery fails closed with a pool error that
    //   names the target, keeps pending-op.json, and gives no alert-file
    //   remedy.
    // Why it exists: this branch used to return `AckCleanupFailed`, which
    //   tells the operator to delete acked-stats.json and alert-latch.json
    //   although alert state was never touched (hunt finding
    //   261022-1230-010). Live add calls the same case `PostAddProbeFailed`.
    // Scenario: recovery replays disk2's add and btrfs accepts it, but the
    //   next `btrfs filesystem show` lists only disk1.
    #[test]
    fn live_add_recovery_replayed_target_missing_from_probe_fails_without_alert_remedy() {
        let f = PoolFixture::empty();
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let runner = with_one_disk_pool_probe(replay_returned_disk2_runner_base());
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().build();

        let err = execute_add_pool_mutation_recovery(
            &runner,
            &MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222"),
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .unwrap_err();

        assert!(
            runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsDeviceAdd { device, .. }
                    if device == "/dev/mapper/braid-disk2"
            )),
            "replay must reach pool_add_device for disk2 before the probe misses it"
        );
        match &err {
            RecoverError::Failed(msg) => assert!(
                msg.contains("'disk2'") && msg.contains("not in the live pool after"),
                "expected the replay miss to name disk2, got {msg}"
            ),
            other => panic!("expected RecoverError::Failed for the replay miss, got {other:?}"),
        }
        assert!(
            !err.to_string().contains("acked-stats.json"),
            "a replay miss must not tell the operator to delete alert state: {err}"
        );
        assert!(f.paths.pending_op_json().exists());
    }

    // Intent
    // RemoveMissing::PoolMutation recovery bridges duplicate-devid journal
    // corruption to RecoverError::DuplicateDevidDuringReplay.
    //
    // Why it exists
    // The corruption signal must stay typed after crossing the
    // live_pool_matches_membership call site, not collapse to
    // RecoverError::Failed.
    //
    // Scenario
    // Recovery sees the removed devid still missing while the journaled
    // pre-membership snapshot binds that devid to two members.
    #[test]
    fn bridges_duplicate_devid_corruption_to_typed_recover_error() {
        let f = PoolFixture::empty();
        let mut journal = remove_missing_journal();
        journal.pre_membership = PoolMembership::for_corruption_tests(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
            membership_entry(
                "disk4",
                "/dev/disk/by-id/virtio-disk4",
                None,
                Some(Devid::new(2)),
            ),
        ]);
        journal::write_journal(&f.paths, &journal).unwrap();
        let params = f.recover_params().build();

        let err = execute_remove_missing_pool_mutation_recovery(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &MockByIdResolver::default(),
            &params,
            &journal,
            pool_state_disk1_with_missing_devid2(),
            Devid::new(2),
            Some(OwedRaid1Balance::RestoreSingle),
        )
        .unwrap_err();

        match err {
            RecoverError::DuplicateDevidDuringReplay { devid, members } => {
                assert_eq!(devid, Devid::new(2));
                assert_eq!(members.len(), 2);
            }
            RecoverError::Failed(message) => {
                panic!("expected DuplicateDevidDuringReplay, got Failed({message:?})");
            }
            other => panic!("expected DuplicateDevidDuringReplay, got {other:?}"),
        }
        assert!(f.paths.pending_op_json().exists());
    }

    // Intent
    // RemoveMissing::PoolMutation recovery refuses with the typed
    // RecoverError::AmbiguousMissingDevid when a btrfs-MISSING devid that no
    // journaled member records sits beside a member with no recorded devid.
    //
    // Why it exists
    // Either could be the other, so binding the devid to that member or
    // dropping the member would be a guess (decision 024 rule 9). The signal
    // must stay typed after crossing the binding, not collapse to
    // RecoverError::Failed.
    //
    // Scenario
    // remove-missing removed devid 2 and crashed. The journaled target
    // records disk3 with no devid, and the live pool shows disk1 plus a
    // MISSING devid 5.
    #[test]
    fn remove_missing_recovery_refuses_ambiguous_missing_devid() {
        let f = PoolFixture::empty();
        let mut journal = remove_missing_journal();
        journal.target_membership = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry("disk3", "/dev/disk/by-id/virtio-disk3", None, None),
        ]);
        journal::write_journal(&f.paths, &journal).unwrap();
        let mut pool = pool_state_disk1_with_missing_devid2();
        pool.missing_devids = vec![Devid::new(5)];
        let params = f.recover_params().build();

        let err = execute_remove_missing_pool_mutation_recovery(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &MockByIdResolver::default(),
            &params,
            &journal,
            pool,
            Devid::new(2),
            Some(OwedRaid1Balance::RestoreSingle),
        )
        .unwrap_err();

        match &err {
            RecoverError::AmbiguousMissingDevid { devids, members } => {
                assert_eq!(devids, &vec![Devid::new(5)]);
                assert_eq!(members, &vec![disk_name("disk3")]);
            }
            other => panic!("expected AmbiguousMissingDevid, got {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("'disk3'") && msg.contains("devid(s) 5"),
            "the refusal must name the member and the devid, got: {msg}"
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    #[test]
    fn plan_recover_discovers_add_targets_before_mount_planning() {
        let f = PoolFixture::empty();
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let inner = MockRunner::default()
            .with_output(mountpoint_fail().0, mountpoint_fail().1)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk2".into(),
                },
                btrfs_show_target_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScan {
                    device: "/dev/mapper/braid-disk2".into(),
                },
                ok_raw_empty("btrfs device scan"),
            );
        let harness = RemountHarness::new(
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
            inner,
            &["braid-disk1", "braid-disk2"],
        );

        let params = f.recover_params().build();

        let plan = plan_recover(&harness.runner, &harness.fs, &params)
            .expect("planner should discover add target, then plan from pre-membership");
        assert!(
            plan.work_plan.pre_resolved_credential.is_some(),
            "pre-mount discovery should carry the resolved passphrase into execution"
        );
        let open_plan = plan
            .work_plan
            .open_plan
            .expect("pool should still need initial mount");
        assert_eq!(
            open_plan.to_unlock,
            vec![(
                disk_name("disk1"),
                ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap()
            )]
        );

        let requests = harness.requests();
        assert_eq!(
            luks_dump_text_request_count(&requests, "/dev/disk/by-id/virtio-disk2"),
            1,
            "pre-mount FreshLuks discovery must use the label captured by probe_config_disk"
        );
        let disk2_open = requests
            .iter()
            .position(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksOpen { device, mapper }
                        if device == "/dev/disk/by-id/virtio-disk2" && mapper.as_str() == "braid-disk2"
                )
            })
            .expect("pre-mount discovery should open the committed add target");
        let disk2_scan = requests
            .iter()
            .position(|r| {
                matches!(
                    r,
                    CmdRequest::BtrfsDeviceScan { device }
                        if device == "/dev/mapper/braid-disk2"
                )
            })
            .expect("pre-mount discovery should scan the committed add target");
        let disk1_probe = requests
            .iter()
            .position(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksUuid { device }
                        if device == "/dev/disk/by-id/virtio-disk1"
                )
            })
            .expect("mount planning should probe pre-membership disk1");
        assert!(
            disk2_open < disk1_probe && disk2_scan < disk1_probe,
            "add target discovery must run before mount planning chooses from pre-membership"
        );
    }

    #[test]
    fn plan_recover_discovers_fresh_add_targets_before_mount_planning() {
        let f = PoolFixture::empty();

        let journal =
            fresh_pool_mutation_add_journal(vec!["--label".into(), "braid-disk2".into()], None);
        journal::write_journal(&f.paths, &journal).unwrap();

        let inner = MockRunner::default()
            .with_output(mountpoint_fail().0, mountpoint_fail().1)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                luks_dump_label("braid-disk2"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk2".into(),
                },
                btrfs_show_target_no_btrfs("/dev/mapper/braid-disk2"),
            );
        let harness = RemountHarness::new(
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
            inner,
            &["braid-disk1", "braid-disk2"],
        );

        let params = f.recover_params().build();
        let plan = plan_recover(&harness.runner, &harness.fs, &params)
            .expect("planner should discover fresh add target, then plan from pre-membership");
        let open_plan = plan
            .work_plan
            .open_plan
            .expect("pool should still need initial mount");
        assert_eq!(
            open_plan.to_unlock,
            vec![(
                disk_name("disk1"),
                ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap()
            )]
        );

        let requests = harness.requests();
        let disk2_open = requests
            .iter()
            .position(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksOpen { device, mapper }
                        if device == "/dev/disk/by-id/virtio-disk2" && mapper.as_str() == "braid-disk2"
                )
            })
            .expect("pre-mount discovery should open the committed fresh target");
        let disk2_btrfs_probe = requests
            .iter()
            .position(|r| {
                matches!(
                    r,
                    CmdRequest::BtrfsFilesystemShowTarget { target }
                        if target == "/dev/mapper/braid-disk2"
                )
            })
            .expect("pre-mount discovery should probe the committed fresh target for btrfs");
        let disk1_probe = requests
            .iter()
            .position(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksUuid { device }
                        if device == "/dev/disk/by-id/virtio-disk1"
                )
            })
            .expect("mount planning should probe pre-membership disk1");
        assert!(
            disk2_open < disk1_probe && disk2_btrfs_probe < disk1_probe,
            "fresh target discovery must run before mount planning chooses from pre-membership"
        );
        assert!(
            !requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::BtrfsDeviceScan { device }
                        if device == "/dev/mapper/braid-disk2"
                )
            }),
            "fresh target without btrfs signature must not be scanned before mount"
        );
    }

    // Intent: Pre-mount discovery skips a journaled add target whose live
    // LUKS UUID does not match the journaled identity.
    // Why it exists: Hard-failing during this non-destructive scan prevents
    // recovery from mounting the pre-operation pool and reaching the replay
    // gate that performs the real identity check.
    // Scenario: The journal records disk2 as an add target, but the physical
    // device at disk2's by-id path has been replaced with a different LUKS
    // container. Recovery should still plan a mount from disk1.
    #[test]
    fn plan_recover_skips_pre_mount_discovery_on_uuid_mismatch() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let runner = MockRunner::default()
            .with_output(mountpoint_fail().0, mountpoint_fail().1)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "99999999-9999-9999-9999-999999999999",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                inactive_mapper_status("braid-disk2"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                inactive_mapper_status("braid-disk1"),
            );
        let request_log = runner.clone();

        let params = f.recover_params().passphrase_file(None).build();
        let plan = plan_recover(&runner, &fs, &params)
            .expect("planner should skip mismatched add target and mount from pre-membership");
        assert!(
            plan.work_plan.pre_resolved_credential.is_none(),
            "skip must not resolve a credential during pre-mount discovery"
        );
        let open_plan = plan
            .work_plan
            .open_plan
            .expect("pool should still need initial mount");
        assert_eq!(
            open_plan.to_unlock,
            vec![(
                disk_name("disk1"),
                ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap()
            )]
        );

        let requests = request_log.requests();
        assert!(
            !requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksOpen { mapper, .. }
                        if mapper.as_str() == "braid-disk2"
                )
            }),
            "mismatched add target must not be opened during pre-mount discovery"
        );
        assert!(
            !requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::BtrfsDeviceScan { device }
                        if device == "/dev/mapper/braid-disk2"
                )
            }),
            "mismatched add target must not be scanned during pre-mount discovery"
        );
    }

    // Intent: Pre-mount discovery skips a fresh add target whose live LUKS
    // label does not match the journaled label.
    // Why it exists: The pre-mount scan is only a visibility helper; a stale
    // by-id path must not abort recovery before the pre-operation pool can
    // mount.
    // Scenario: The journal records fresh disk2 with label "braid-disk2",
    // but luksDump reports a different label on the live device.
    #[test]
    fn plan_recover_skips_pre_mount_discovery_on_fresh_label_mismatch() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal =
            fresh_pool_mutation_add_journal(vec!["--label".into(), "braid-disk2".into()], None);
        journal::write_journal(&f.paths, &journal).unwrap();

        let runner = MockRunner::default()
            .with_output(mountpoint_fail().0, mountpoint_fail().1)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                luks_dump_label("not-braid-disk2"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                inactive_mapper_status("braid-disk2"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                inactive_mapper_status("braid-disk1"),
            );
        let request_log = runner.clone();

        let params = f.recover_params().passphrase_file(None).build();
        let plan = plan_recover(&runner, &fs, &params)
            .expect("planner should skip mislabeled fresh target and mount from pre-membership");
        assert!(
            plan.work_plan.pre_resolved_credential.is_none(),
            "skip must not resolve a credential during pre-mount discovery"
        );
        let open_plan = plan
            .work_plan
            .open_plan
            .expect("pool should still need initial mount");
        assert_eq!(
            open_plan.to_unlock,
            vec![(
                disk_name("disk1"),
                ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap()
            )]
        );

        let requests = request_log.requests();
        assert!(
            !requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksOpen { mapper, .. }
                        if mapper.as_str() == "braid-disk2"
                )
            }),
            "mislabeled fresh target must not be opened during pre-mount discovery"
        );
        assert!(
            !requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::BtrfsDeviceScan { device }
                        if device == "/dev/mapper/braid-disk2"
                )
            }),
            "mislabeled fresh target must not be scanned during pre-mount discovery"
        );
    }

    // Intent: Pre-mount discovery continues after a mismatched journaled add
    // target and still opens/scans a later valid target.
    // Why it exists: A `continue` regression to `break` would pass the
    // single-target mismatch tests while losing discovery help for multi-disk
    // adds where one target is bad and another is already committed.
    // Scenario: `disk2` sorts first and has a replaced LUKS UUID; `disk3`
    // matches the journal and has a visible btrfs signature.
    #[test]
    fn plan_recover_continues_pre_mount_discovery_after_mismatched_target() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let journal = two_target_recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let runner = MockRunner::default()
            .with_output(mountpoint_fail().0, mountpoint_fail().1)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "99999999-9999-9999-9999-999999999999",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                inactive_mapper_status("braid-disk2"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk3".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk3",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk3")
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk3".into()),
                },
                inactive_mapper_status("braid-disk3"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk3".into(),
                    mapper: MapperName::from_basename("braid-disk3".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk3".into(),
                },
                btrfs_show_target_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScan {
                    device: "/dev/mapper/braid-disk3".into(),
                },
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                inactive_mapper_status("braid-disk1"),
            );
        let request_log = runner.clone();

        let params = f.recover_params().build();
        let plan = plan_recover(&runner, &fs, &params)
            .expect("planner should skip disk2 and continue discovering disk3");
        assert!(
            plan.work_plan.pre_resolved_credential.is_some(),
            "valid later target should resolve a credential during discovery"
        );

        let requests = request_log.requests();
        assert!(
            !requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksOpen { mapper, .. }
                        if mapper.as_str() == "braid-disk2"
                )
            }),
            "mismatched first target must not be opened"
        );
        assert!(
            requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksOpen { device, mapper }
                        if device == "/dev/disk/by-id/virtio-disk3" && mapper.as_str() == "braid-disk3"
                )
            }),
            "matching later target must be opened"
        );
        assert!(
            requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::BtrfsDeviceScan { device }
                        if device == "/dev/mapper/braid-disk3"
                )
            }),
            "matching later target must be scanned"
        );
    }

    // Intent: under mapper drift (a live member open as braid-WRONG), the
    //   add-replay passphrase rejection names the operator disk name resolved
    //   through membership ('disk1'), not the drifted mapper basename.
    // Why it exists: the credential-verify display used to parse the mapper
    //   basename, so a drifted member surfaced as 'WRONG'; decision 024 requires
    //   the live-UUID->DiskName join here as on every sibling surface.
    // Scenario: an interrupted add is replayed while disk1 is open under a stale
    //   'braid-WRONG' mapper and its passphrase no longer matches.
    #[test]
    fn add_replay_passphrase_rejection_names_drifted_member_via_membership() {
        let drifted_uuid = uuid_for_name("disk1");
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
                disk_member_named(
                    "disk1",
                    "/dev/disk/by-id/virtio-disk1",
                    None,
                    Some(Devid::new(1)),
                ),
            )
            .unwrap();
        // No pending targets: the verify runs against the live (drifted)
        // member only, which is exactly the membership join under test.
        let runner = MockRunner::default().with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/vda".into(),
            },
            b"wrongpass".to_vec(),
            err_raw("cryptsetup open --test-passphrase", 2, "No key available"),
        );
        let fs = MockFs::new(&[]);

        let err = verify_recover_passphrase_for_add_replay(
            &runner,
            &fs,
            &pool,
            &membership,
            &[],
            &crate::test_fixtures::MockBackingPathResolver::default(),
            &passphrase("wrongpass"),
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("recover add passphrase was rejected by 'disk1'"),
            "drifted member must resolve to 'disk1' via membership, got: {msg}"
        );
        assert!(
            !msg.contains("WRONG"),
            "must not surface the drifted mapper basename, got: {msg}"
        );
    }

    // Intent: under mapper drift, the replace fresh-prep passphrase rejection
    //   names the existing member's operator name resolved through membership
    //   ('disk1'), not the drifted mapper basename.
    // Why it exists: same decision-024 join as the add-replay path; the replace
    //   recovery prep verifies existing members before any LUKS mutation.
    // Scenario: an interrupted replace prep is finished while an existing member
    //   is open under a stale 'braid-WRONG' mapper and its passphrase no longer
    //   matches.
    #[test]
    fn replace_prep_passphrase_rejection_names_drifted_member_via_membership() {
        let drifted_uuid = uuid_for_name("disk1");
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
                disk_member_named(
                    "disk1",
                    "/dev/disk/by-id/virtio-disk1",
                    None,
                    Some(Devid::new(1)),
                ),
            )
            .unwrap();
        // Existing member (/dev/vda) is verified before the new disk, so its
        // rejection is what the assertion observes.
        let runner = MockRunner::default().with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/vda".into(),
            },
            b"wrongpass".to_vec(),
            err_raw("cryptsetup open --test-passphrase", 2, "No key available"),
        );

        let err = verify_replace_fresh_prep_passphrase(
            &runner,
            &pool,
            &membership,
            &disk_name("new"),
            &by_id_path("/dev/disk/by-id/virtio-new"),
            &passphrase("wrongpass"),
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("recover replace passphrase was rejected by 'disk1'"),
            "drifted member must resolve to 'disk1' via membership, got: {msg}"
        );
        assert!(
            !msg.contains("WRONG"),
            "must not surface the drifted mapper basename, got: {msg}"
        );
    }

    #[test]
    fn add_pool_mutation_replay_verifies_open_journaled_target_passphrase() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"])
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222")
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let journal = recoverable_pool_mutation_add_journal();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
            .with_mapper_open(
                "braid-disk2",
                "/dev/vdb",
                "22222222-2222-2222-2222-222222222222",
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk2".into(),
                },
                btrfs_show_target_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScan {
                    device: "/dev/mapper/braid-disk2".into(),
                },
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_one_disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                err_raw("cryptsetup open --test-passphrase", 2, "No key available"),
            );

        let resolver = MockByIdResolver::default();
        let params = f.recover_params().build();
        let err = execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(
            msg.contains("recover add passphrase was rejected by 'disk2'"),
            "error should name the journaled target, got: {msg}"
        );
        let requests = runner.requests();
        assert!(
            requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupTestPassphrase { device }
                    if device == "/dev/disk/by-id/virtio-disk2"
            )),
            "replay must verify the passphrase against the already-open add target"
        );
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::WipefsBtrfs { .. } | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "credential rejection must stop before destructive returned-disk replay"
        );
    }

    // Intent
    // Verify add PoolMutation recovery rejects an unjournaled live member
    // before reconciliation or replay.
    //
    // Why it exists
    // A foreign live member must not be adopted or mutated just because a
    // journaled add is resumable.
    //
    // Scenario
    // A pre-membership disk1+disk2 pool is recovering a returned disk3 add,
    // but the live pool already contains an unrelated braid-mystery mapper.
    #[test]
    fn pre_replay_unknown_live_member_aborts_before_mutation() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[]);
        let journal = two_pre_recoverable_add_disk3_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };

        let mut pool = pool_state_two_disks();
        pool.devices.push(PoolDevice {
            mapper: MapperName::from_basename("braid-mystery".into()),
            luks_uuid: LuksUuid::parse("99999999-9999-9999-9999-999999999999").unwrap(),
            devid: Devid::new(3),
            underlying: "/dev/vdz".into(),
        });
        pool.total_devices = 3;
        let runner = MockRunner::default();
        let params = f.recover_params().passphrase_file(None).build();

        let err = execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool,
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("braid-mystery"), "{msg}");
        let requests = runner.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsDeviceScanForget { .. }
                    | CmdRequest::WipefsBtrfs { .. }
                    | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "unknown live member must abort before destructive add recovery"
        );
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksOpen { mapper, .. } if mapper.as_str() == "braid-disk3"
            )),
            "unknown live member must abort before opening the journaled target"
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    // Intent
    // Verify a returned add target that is absent, or present but not LUKS,
    // preserves the recovery journal, does not write pool.json, and names
    // which of the two it is.
    //
    // Why it exists
    // Operators need to reattach the returned disk and rerun recover; clearing
    // or rewriting state on either path would strand the operation. An absent
    // disk was once reported as "not a LUKS device", which reads as a wiped
    // disk instead of an unplugged one.
    //
    // Scenario
    // The pool has disk1 mounted after an interrupted returned-disk add, and
    // the journaled disk2 by-id path is either physically absent or holds a
    // disk whose LUKS header cryptsetup cannot read.
    #[test]
    fn returned_target_absent_or_not_luks_preserves_journal_and_pool_json() {
        for absent in [true, false] {
            let f = PoolFixture::empty();
            let fs = if absent {
                MockFs::new(&["/dev/mapper/braid-disk1"])
            } else {
                MockFs::new(&["/dev/mapper/braid-disk1", "/dev/disk/by-id/virtio-disk2"])
            };
            let journal = recoverable_pool_mutation_add_journal();
            journal::write_journal(&f.paths, &journal).unwrap();
            let union = test_recovery_admission_membership(&journal);
            let targets = match &journal.op {
                OpKind::Add { targets, .. } => targets,
                _ => unreachable!("test journal is Add"),
            };
            let mut runner = MockRunner::default().with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            );
            if !absent {
                runner = runner.with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    err_raw(
                        "cryptsetup luksUUID",
                        1,
                        "Device is not a valid LUKS device.",
                    ),
                );
            }
            let params = f.recover_params().build();

            let err = execute_add_pool_mutation_recovery(
                &runner,
                &fs,
                &MockByIdResolver::default(),
                &params,
                AddPoolReplayCtx {
                    credential: None,
                    journal: &journal,
                    union: &union,
                    targets,
                    pool: pool_state_one_disk(),
                    owed_balance: OwedRaid1Balance::Grow,
                },
                &std::collections::BTreeSet::new(),
            )
            .unwrap_err();

            let msg = err.to_string();
            if absent {
                assert!(
                    msg.contains(
                        "recover add target 'disk2' (/dev/disk/by-id/virtio-disk2) is not present; reconnect it"
                    ),
                    "{msg}"
                );
                assert!(!msg.contains("is not a LUKS device"), "{msg}");
            } else {
                assert!(
                    msg.contains("recover add target 'disk2' is not a LUKS device"),
                    "{msg}"
                );
                assert!(!msg.contains("is not present"), "{msg}");
            }
            assert!(
                !runner.requests().iter().any(|r| matches!(
                    r,
                    CmdRequest::BtrfsDeviceScanForget { .. }
                        | CmdRequest::WipefsBtrfs { .. }
                        | CmdRequest::BtrfsDeviceAdd { .. }
                )),
                "absent or non-LUKS returned target must not scan-forget, wipe, or add"
            );
            assert!(f.paths.pending_op_json().exists());
            assert!(!f.paths.pool_json().exists());
        }
    }

    // Intent
    // Verify recover opens and scans a committed-but-closed returned add
    // target before mounting, then adopts it without replaying the add.
    //
    // Why it exists
    // The offline path owns this reconciliation pass; testing only the
    // post-mount helper would miss the pre-mount discovery behavior.
    //
    // Scenario
    // Recover starts with the pool unmounted, disk2 is already a btrfs member
    // but its mapper is closed, and disk1 remains the pre-membership mount
    // source.
    #[test]
    fn returned_committed_but_closed_not_mounted_replays_via_pre_mount_scan() {
        let f = PoolFixture::empty();
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let inner = with_grow_balance_replay(with_two_disk_pool_probe(
            MockRunner::default()
                .with_output(mountpoint_fail().0, mountpoint_fail().1)
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    cryptsetup_uuid_ok(
                        "/dev/disk/by-id/virtio-disk2",
                        "22222222-2222-2222-2222-222222222222",
                    ),
                )
                .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
                .with_output_stdin(
                    CmdRequest::CryptsetupLuksOpen {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                        mapper: MapperName::from_basename("braid-disk2".into()),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open"),
                )
                .with_output(
                    CmdRequest::BtrfsFilesystemShowTarget {
                        target: "/dev/mapper/braid-disk2".into(),
                    },
                    btrfs_show_target_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceScan {
                        device: "/dev/mapper/braid-disk2".into(),
                    },
                    ok_raw_empty("btrfs device scan"),
                )
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk1".into(),
                    },
                    cryptsetup_uuid_ok(
                        "/dev/disk/by-id/virtio-disk1",
                        "11111111-1111-1111-1111-111111111111",
                    ),
                )
                .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/disk/by-id/virtio-disk1".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupLuksOpen {
                        device: "/dev/disk/by-id/virtio-disk1".into(),
                        mapper: MapperName::from_basename("braid-disk1".into()),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceScanAll,
                    ok_raw_empty("btrfs device scan"),
                )
                .with_output(
                    CmdRequest::Mount {
                        device: "/dev/mapper/braid-disk1".into(),
                        mount_point: MountPoint::new("/mnt/storage".into()),
                    },
                    ok_raw_empty("mount"),
                ),
        ));
        let harness = RemountHarness::new(
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
            inner,
            &["braid-disk1", "braid-disk2"],
        )
        .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
        .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().build();

        cmd_recover(&harness.runner, &harness.fs, &resolver, &params)
            .expect("closed committed target should be discovered and adopted");

        let requests = harness.requests();
        assert!(
            requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::Mount { device, .. }
                        if device == "/dev/mapper/braid-disk1"
                )
            }),
            "initial mount should use the pre-membership disk"
        );
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::WipefsBtrfs { .. } | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "already committed returned target must not be wiped or re-added"
        );
        assert!(!f.paths.pending_op_json().exists());
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(recovered.by_name(&disk_name("disk2")).is_some());
    }

    // Intent
    // Verify fresh add recovery adopts a committed-but-closed target during
    // reconciliation and still runs the owed post-add balance.
    //
    // Why it exists
    // A fresh target that is already a live btrfs member must not be formatted,
    // backed up, wiped, or re-added just because its mapper started closed.
    //
    // Scenario
    // The initial pool probe sees only disk1; recovery opens disk2, scans its
    // existing btrfs signature, re-probes disk1+disk2, and advances to balance.
    #[test]
    fn fresh_committed_but_closed_appears_in_recovered_pool_with_balance() {
        let f = PoolFixture::empty();
        let stored_opts = vec!["--label".into(), "braid-disk2".into()];
        let journal = fresh_pool_mutation_add_journal(stored_opts, None);
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };

        let inner = with_grow_balance_replay(with_two_disk_pool_probe(
            MockRunner::default()
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    cryptsetup_uuid_ok(
                        "/dev/disk/by-id/virtio-disk2",
                        "22222222-2222-2222-2222-222222222222",
                    ),
                )
                .with_output(
                    CmdRequest::CryptsetupLuksDumpText {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    luks_dump_label("braid-disk2"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupLuksOpen {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                        mapper: MapperName::from_basename("braid-disk2".into()),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open"),
                )
                .with_output(
                    CmdRequest::BtrfsFilesystemShowTarget {
                        target: "/dev/mapper/braid-disk2".into(),
                    },
                    btrfs_show_target_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceScan {
                        device: "/dev/mapper/braid-disk2".into(),
                    },
                    ok_raw_empty("btrfs device scan"),
                ),
        ));
        let harness =
            RemountHarness::new(&["/dev/disk/by-id/virtio-disk2"], inner, &["braid-disk2"])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().build();

        execute_add_pool_mutation_recovery(
            &harness.runner,
            &harness.fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("committed fresh target should be adopted without replay");

        let requests = harness.requests();
        assert!(
            requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksOpen { device, mapper }
                    if device == "/dev/disk/by-id/virtio-disk2" && mapper.as_str() == "braid-disk2"
            )),
            "reconciliation should open the closed fresh target"
        );
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksFormat { .. }
                    | CmdRequest::CryptsetupLuksAddKeyFile { .. }
                    | CmdRequest::WipefsBtrfs { .. }
                    | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "already live fresh target must not run destructive replay"
        );
        assert!(
            requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsBalanceRaid1 { .. })),
            "adopted committed target should still run post-add balance replay"
        );
        assert!(!f.paths.pending_op_json().exists());
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(recovered.by_name(&disk_name("disk2")).is_some());
    }

    // Intent
    // Verify existing-pool add recovery plans its mount work from
    // pre-membership, not from the add target.
    //
    // Why it exists
    // A target-only disk may be formatted, missing, or merely committed; the
    // mount phase must use only disks that belonged to the pool before add.
    //
    // Scenario
    // The target-only disk2 add target is absent before mount planning, while
    // disk1 remains available. Recovery still plans the existing-pool mount
    // from disk1 and does not require disk2 to be present.
    #[test]
    fn existing_pool_add_recovery_plans_mount_from_pre_membership_only() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1", "/dev/mapper/braid-disk1"]);
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let runner = MockRunner::default()
            .with_output(mountpoint_fail().0, mountpoint_fail().1)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_mapper_closed("braid-disk1");
        let params = f.recover_params().passphrase_file(None).build();

        let plan = plan_recover(&runner, &fs, &params)
            .expect("planner should mount existing-pool add from pre-membership");
        let open_plan = plan
            .work_plan
            .open_plan
            .as_ref()
            .expect("pool should need initial mount");
        assert_eq!(open_plan.mount_device, "/dev/mapper/braid-disk1");
    }

    #[test]
    fn add_pool_mutation_replays_returned_disk_after_wipefs_crash() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };

        let runner = with_grow_balance_replay(with_two_disk_pool_probe(
            MockRunner::default()
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    cryptsetup_uuid_ok(
                        "/dev/disk/by-id/virtio-disk2",
                        "22222222-2222-2222-2222-222222222222",
                    ),
                )
                .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
                .with_mapper_open(
                    "braid-disk2",
                    "/dev/vdb",
                    "22222222-2222-2222-2222-222222222222",
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/vda".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output(
                    CmdRequest::BtrfsFilesystemShowTarget {
                        target: "/dev/mapper/braid-disk2".into(),
                    },
                    btrfs_show_target_no_btrfs("/dev/mapper/braid-disk2"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceScanForget {
                        devices: vec!["/dev/mapper/braid-disk2".into()],
                    },
                    ok_raw_empty("btrfs device scan --forget"),
                )
                .with_output(
                    CmdRequest::WipefsBtrfs {
                        device: "/dev/mapper/braid-disk2".into(),
                    },
                    ok_raw_empty("wipefs"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceAdd {
                        device: "/dev/mapper/braid-disk2".into(),
                        mount_point: MountPoint::new("/mnt/storage".into()),
                        force: true,
                    },
                    ok_raw_empty("btrfs device add"),
                ),
        ));
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let inhibitor = RequestCountInhibitor::new(runner.clone());
        let params = f.recover_params().sleep_inhibitor(&inhibitor).build();

        execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("returned-disk replay should complete recovery");

        let requests = runner.requests();
        assert!(
            !requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::BtrfsDeviceScan { device }
                        if device == "/dev/mapper/braid-disk2"
                )
            }),
            "returned disk after wipefs has no btrfs signature and must not be scanned"
        );
        let forget = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsDeviceScanForget { .. }))
            .expect("returned replay should forget stale btrfs scan state");
        let wipe = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::WipefsBtrfs { .. }))
            .expect("returned replay should narrowly wipe btrfs signature");
        let add = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsDeviceAdd { force: true, .. }))
            .expect("returned replay should force-add after wipe");
        assert!(forget < wipe && wipe < add);
        assert_eq!(inhibitor.acquire_count(), 1);
        let acquire_at = inhibitor
            .first_acquire_request_count()
            .expect("replay should acquire the sleep inhibitor");
        for (idx, request) in requests.iter().enumerate() {
            if matches!(request, CmdRequest::CryptsetupTestPassphrase { .. }) {
                assert!(
                    idx < acquire_at,
                    "credential verification must finish before inhibitor acquisition; \
                     acquire_at={acquire_at}, request {idx}={request:?}, requests={requests:?}"
                );
            }
        }
        assert!(
            wipe >= acquire_at && add >= acquire_at,
            "destructive replay must stay inside the inhibitor window; \
             acquire_at={acquire_at}, wipe={wipe}, add={add}, requests={requests:?}"
        );
        assert!(!f.paths.pending_op_json().exists());
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk2")).is_some());
    }

    // Intent: ExistingLuks add recovery with `enroll_key_file:
    //   Some(kf)` runs `cryptsetup luksAddKey` + `luksHeaderBackup`
    //   BEFORE `pool_add_device` (which expands to scanForget +
    //   wipefs + btrfs device add).
    // Why it exists: this is the silent-drop bug fix landing in add
    //   recovery. Pre-refactor, mid-`add --enroll DIR` crash recovery
    //   never re-ran the enrollment, so the returning disk shipped
    //   without slot 1 enrolled and could not be auto-unlocked. Pin
    //   the order (addKey before scanForget/wipefs/add) so a future
    //   change cannot silently regress to "no replay" or shift the
    //   mutation past the irreversible btrfs commit.
    // Scenario: braid add --enroll DIR against a returning braid disk
    //   crashed between journal write and pool_add_device. Recovery
    //   resumes and replays the planned enrollment.
    #[test]
    fn add_pool_mutation_replays_keyfile_enrollment_before_pool_add() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");
        let key_dir = tempfile::TempDir::new().unwrap();
        let key_file = write_valid_keyfile(&key_dir, "braid.key");
        let journal = recoverable_pool_mutation_add_journal_with_enroll(key_file.clone());
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };

        let runner = with_grow_balance_replay(with_two_disk_pool_probe(
            MockRunner::default()
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    cryptsetup_uuid_ok(
                        "/dev/disk/by-id/virtio-disk2",
                        "22222222-2222-2222-2222-222222222222",
                    ),
                )
                .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
                .with_mapper_open(
                    "braid-disk2",
                    "/dev/vdb",
                    "22222222-2222-2222-2222-222222222222",
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/vda".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output(
                    CmdRequest::BtrfsFilesystemShowTarget {
                        target: "/dev/mapper/braid-disk2".into(),
                    },
                    btrfs_show_target_no_btrfs("/dev/mapper/braid-disk2"),
                )
                // ensure_keyfile_enrolled probes the keyfile, then enrolls
                // since slot 1 has not yet been populated post-crash.
                .with_output(
                    CmdRequest::CryptsetupTestKeyFile {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                        key_file_path: key_file.display().to_string(),
                    },
                    err_raw("cryptsetup open --test-passphrase --key-file", 2, "No key"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupLuksAddKeyFile {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                        key_file_path: key_file.display().to_string(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup luksAddKey"),
                )
                .with_output(
                    CmdRequest::CryptsetupLuksHeaderBackup {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                        backup_path: f
                            .paths
                            .luks_headers_dir()
                            .join("braid-disk2.luksheader.tmp")
                            .display()
                            .to_string(),
                    },
                    ok_raw_empty("cryptsetup luksHeaderBackup"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceScanForget {
                        devices: vec!["/dev/mapper/braid-disk2".into()],
                    },
                    ok_raw_empty("btrfs device scan --forget"),
                )
                .with_output(
                    CmdRequest::WipefsBtrfs {
                        device: "/dev/mapper/braid-disk2".into(),
                    },
                    ok_raw_empty("wipefs"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceAdd {
                        device: "/dev/mapper/braid-disk2".into(),
                        mount_point: MountPoint::new("/mnt/storage".into()),
                        force: true,
                    },
                    ok_raw_empty("btrfs device add"),
                ),
        ));
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let inhibitor = RequestCountInhibitor::new(runner.clone());
        let params = f.recover_params().sleep_inhibitor(&inhibitor).build();

        execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("replay with enroll_key_file should complete");

        let requests = runner.requests();
        let addkey = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. }))
            .expect("replay must run luksAddKey");
        let backup = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::CryptsetupLuksHeaderBackup { .. }))
            .expect("replay must back up the header");
        let scan_forget = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsDeviceScanForget { .. }))
            .expect("replay must scan-forget");
        let wipe = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::WipefsBtrfs { .. }))
            .expect("replay must wipefs");
        let add = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsDeviceAdd { .. }))
            .expect("replay must btrfs-device-add");
        assert!(
            addkey < backup && backup < scan_forget && scan_forget < wipe && wipe < add,
            "expected addKey({addkey}) < backup({backup}) < scan-forget({scan_forget}) < wipefs({wipe}) < device-add({add}); got: {requests:?}"
        );
        assert!(inhibitor.first_acquire_request_count().is_some());
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent: dry-run preview for add recovery with a journaled
    //   `RecoverableBraidLabeled { enroll_key_file: Some(_) }` target
    //   renders `cryptsetup luksAddKey` + `cryptsetup luksHeaderBackup`
    //   BEFORE `btrfs device scan --forget`, `wipefs`, and
    //   `btrfs device add`.
    // Why it exists: per `docs/design/decisions/022-dry-run-preview-model.md`,
    //   the preview must stay byte-aligned with the executor. The
    //   add-recovery executor inserts addKey + headerBackup before
    //   pool_add_device when the journal carries `enroll_key_file:
    //   Some`; this test pins that the renderer agrees. A regression
    //   that put the keyfile mutation between wipefs and btrfs-add
    //   would falsely show wipefs running before the slot 1 mutation
    //   in dry-run output, contradicting the actual replay order.
    // Scenario: operator runs `braid recover --dry-run` after a crash
    //   mid-`add --enroll DIR` of a returning braid disk. The journal
    //   is `RecoverableBraidLabeled` with the keyfile path set.
    #[test]
    fn render_add_recovery_existing_luks_with_enroll_renders_addkey_before_scanforget() {
        let targets = add_targets(vec![(
            uuid_for_name("disk2"),
            add_target(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                journal::AddJournalMode::RecoverableBraidLabeled {
                    verified_pool_fsid: Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
                        .unwrap(),
                    enroll_key_file: Some(KeyFilePath::new(std::path::PathBuf::from(
                        "/run/keys/braid.key",
                    ))),
                },
            ),
        )]);

        let plan = RecoverWorkPlan {
            open_plan: None,
            pre_resolved_credential: None,
            journal: recoverable_pool_mutation_add_journal(),
            admission_membership: PoolMembership::empty(),
            mount_point: MountPoint::new("/mnt/storage".into()),
            pool_json_path: std::path::PathBuf::from("/var/lib/braid/pool.json"),
            pending_op_path: std::path::PathBuf::from("/var/lib/braid/pending-op.json"),
            luks_headers_dir: std::path::PathBuf::from("/var/lib/braid/luks-headers"),
            actions: Vec::new(),
        };

        let mut steps = Vec::new();
        render_add_pool_mutation_recovery_steps(
            &plan,
            &mut steps,
            &targets,
            false,
            None,
            &std::collections::BTreeSet::new(),
        );
        let output = Step::render_dry_run(&steps);

        let lines: Vec<&str> = output.lines().collect();
        let find = |needle: &str| -> usize {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle:?} in:\n{output}"))
        };
        let addkey = find("$ cryptsetup luksAddKey");
        let backup = find("$ cryptsetup luksHeaderBackup");
        let scan_forget = find("$ btrfs device scan --forget");
        let wipefs = find("$ wipefs");
        let add = find("$ btrfs device add");
        assert!(
            addkey < backup && backup < scan_forget && scan_forget < wipefs && wipefs < add,
            "expected luksAddKey({addkey}) < luksHeaderBackup({backup}) < scan-forget({scan_forget}) < wipefs({wipefs}) < device-add({add}); got:\n{output}"
        );
        // Pin BOTH stringly fields with distinct keyfile and header paths so a
        // keyfile/header transposition in the RecoverableBraidLabeled replay
        // render fails here even though the newtypes guard the boundary.
        assert!(
            lines[addkey].contains("/run/keys/braid.key")
                && !lines[addkey].contains("braid-disk2.luksheader"),
            "luksAddKey must carry the keyfile, not the header path; got: {}",
            lines[addkey]
        );
        assert!(
            lines[backup].contains("braid-disk2.luksheader")
                && !lines[backup].contains("/run/keys/braid.key"),
            "luksHeaderBackup must carry the header path, not the keyfile; got: {}",
            lines[backup]
        );
    }

    // Intent: the FreshLuks recovery replay arm with `enroll_key_file:
    //   Some(kf)` renders luksAddKey -> luksHeaderBackup -> open -> add with
    //   no luksFormat row, and the addKey carries the keyfile while the
    //   backup carries the header path -- never the reverse.
    // Why it exists: the FreshLuks render is the second of the two recover
    //   replay arms that emit an addKey/headerBackup pair into stringly
    //   `CmdRequest` fields by hand. The newtypes guard the function
    //   boundary, but a transposition at the terminal `.display()` still
    //   compiles; this pins both exact fields with distinct paths so such a
    //   swap fails a test.
    //   Recover never formats a disk (finding 261022-1230-002), so the
    //   replay of a formatted fresh target starts at the enrollment.
    // Scenario: a crash mid-`add --enroll DIR` after luksFormat wrote a
    //   fresh disk; recovery replays the enrollment.
    #[test]
    fn render_add_recovery_fresh_luks_with_enroll_pins_keyfile_and_header_fields() {
        let targets = add_targets(vec![(
            uuid_for_name("disk2"),
            add_target(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                fresh_mode(
                    Vec::new(),
                    Some(std::path::PathBuf::from("/run/keys/braid.key")),
                ),
            ),
        )]);

        let plan = RecoverWorkPlan {
            open_plan: None,
            pre_resolved_credential: None,
            journal: recoverable_pool_mutation_add_journal(),
            admission_membership: PoolMembership::empty(),
            mount_point: MountPoint::new("/mnt/storage".into()),
            pool_json_path: std::path::PathBuf::from("/var/lib/braid/pool.json"),
            pending_op_path: std::path::PathBuf::from("/var/lib/braid/pending-op.json"),
            luks_headers_dir: std::path::PathBuf::from("/var/lib/braid/luks-headers"),
            actions: Vec::new(),
        };

        let mut steps = Vec::new();
        render_add_pool_mutation_recovery_steps(
            &plan,
            &mut steps,
            &targets,
            false,
            None,
            &std::collections::BTreeSet::new(),
        );
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();
        let find = |needle: &str| -> usize {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle:?} in:\n{output}"))
        };
        assert!(
            !output.contains("luksFormat"),
            "recover must never render a luksFormat row; got:\n{output}"
        );
        let addkey = find("$ cryptsetup luksAddKey");
        let backup = find("$ cryptsetup luksHeaderBackup");
        let open = find("$ cryptsetup open --type luks");
        assert!(
            addkey < backup && backup < open,
            "expected luksAddKey({addkey}) < luksHeaderBackup({backup}) < open({open}); got:\n{output}"
        );
        // Pin BOTH stringly fields with distinct keyfile and header paths.
        assert!(
            lines[addkey].contains("/run/keys/braid.key")
                && !lines[addkey].contains("braid-disk2.luksheader"),
            "luksAddKey must carry the keyfile, not the header path; got: {}",
            lines[addkey]
        );
        assert!(
            lines[backup].contains("braid-disk2.luksheader")
                && !lines[backup].contains("/run/keys/braid.key"),
            "luksHeaderBackup must carry the header path, not the keyfile; got: {}",
            lines[backup]
        );
    }

    // Intent: dry-run preview for add recovery with `enroll_key_file:
    //   None` is unchanged from the pre-refactor render: only
    //   `scan-forget`, `wipefs`, `btrfs device add` -- no
    //   `luksAddKey` / `luksHeaderBackup`.
    // Why it exists: regression guard against the renderer
    //   unconditionally emitting the keyfile pair when no keyfile is
    //   journaled. Locks the no-enroll preview at byte-equivalence.
    // Scenario: pre-`--enroll`-fix journal, recovery dry-run.
    #[test]
    fn render_add_recovery_existing_luks_without_enroll_emits_no_keyfile_steps() {
        let targets = add_targets(vec![(
            uuid_for_name("disk2"),
            add_target(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                journal::AddJournalMode::RecoverableBraidLabeled {
                    verified_pool_fsid: Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee")
                        .unwrap(),
                    enroll_key_file: None,
                },
            ),
        )]);

        let plan = RecoverWorkPlan {
            open_plan: None,
            pre_resolved_credential: None,
            journal: recoverable_pool_mutation_add_journal(),
            admission_membership: PoolMembership::empty(),
            mount_point: MountPoint::new("/mnt/storage".into()),
            pool_json_path: std::path::PathBuf::from("/var/lib/braid/pool.json"),
            pending_op_path: std::path::PathBuf::from("/var/lib/braid/pending-op.json"),
            luks_headers_dir: std::path::PathBuf::from("/var/lib/braid/luks-headers"),
            actions: Vec::new(),
        };

        let mut steps = Vec::new();
        render_add_pool_mutation_recovery_steps(
            &plan,
            &mut steps,
            &targets,
            false,
            None,
            &std::collections::BTreeSet::new(),
        );
        let output = Step::render_dry_run(&steps);
        assert!(
            !output.contains("$ cryptsetup luksAddKey"),
            "no-enroll renderer must not emit luksAddKey; got:\n{output}"
        );
        assert!(
            !output.contains("$ cryptsetup luksHeaderBackup"),
            "no-enroll renderer must not emit luksHeaderBackup; got:\n{output}"
        );
        assert!(
            output.contains("$ btrfs device scan --forget"),
            "no-enroll renderer must still emit scan-forget; got:\n{output}"
        );
    }

    #[test]
    fn add_pool_mutation_committed_target_scans_without_wipe_or_add() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let runner = with_grow_balance_replay(with_two_disk_pool_probe(
            MockRunner::default()
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    cryptsetup_uuid_ok(
                        "/dev/disk/by-id/virtio-disk2",
                        "22222222-2222-2222-2222-222222222222",
                    ),
                )
                .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
                .with_mapper_open(
                    "braid-disk2",
                    "/dev/vdb",
                    "22222222-2222-2222-2222-222222222222",
                )
                .with_output(
                    CmdRequest::BtrfsFilesystemShowTarget {
                        target: "/dev/mapper/braid-disk2".into(),
                    },
                    btrfs_show_target_fsid("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceScan {
                        device: "/dev/mapper/braid-disk2".into(),
                    },
                    ok_raw_empty("btrfs device scan"),
                ),
        ));
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().build();

        execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("already-committed target should advance to balance recovery");

        let requests = runner.requests();
        assert!(
            requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceScan { device } if device == "/dev/mapper/braid-disk2")),
            "committed closed target should be scanned before re-probe"
        );
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::WipefsBtrfs { .. } | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "already live target must not be wiped or re-added"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent
    // Verify PoolMutation recovery advances an all-live add journal to
    // PostAddBalanceRaid1 before attempting balance replay.
    //
    // Why it exists
    // The all-live fast path must still persist the phase handoff; otherwise
    // an interruption during post-add balance recovery would leave a stale
    // PoolMutation journal that previews or replays the wrong work.
    //
    // Scenario
    // Mixed returned/fresh add journal where every target is already in the
    // live pool; a failing sleep inhibitor stops post-add balance recovery so
    // the preserved journal can be inspected.
    #[test]
    fn add_pool_mutation_initial_all_live_advances_phase_before_balance_inhibitor_failure() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[]);
        let journal = mixed_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let runner = MockRunner::default().with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/vda".into(),
            },
            TEST_PASSPHRASE_BYTES.to_vec(),
            ok_raw_empty("cryptsetup open --test-passphrase"),
        );
        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk2"),
            ("/dev/vdc", "virtio-disk3"),
        ]);
        let inhibitor = FailingInhibitor;
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&inhibitor)
            .build();

        let err = execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_three_disks(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("could not acquire sleep inhibitor"),
            "expected post-add inhibitor failure, got: {err}",
        );
        assert!(
            !runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksFormat { .. }
                    | CmdRequest::CryptsetupLuksAddKeyFile { .. }
                    | CmdRequest::CryptsetupLuksHeaderBackup { .. }
                    | CmdRequest::CryptsetupLuksOpen { .. }
                    | CmdRequest::BtrfsDeviceScan { .. }
                    | CmdRequest::BtrfsDeviceScanForget { .. }
                    | CmdRequest::WipefsBtrfs { .. }
                    | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "all-live path must not prep, scan, wipe, or add targets"
        );
        assert!(
            !runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsBalanceStatus { .. }
                    | CmdRequest::BtrfsBalanceRaid1 { .. }
                    | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "inhibitor failure must stop before balance commands"
        );

        assert!(f.paths.pool_json().exists(), "pool.json should be written");
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(recovered.by_name(&disk_name("disk2")).is_some());
        assert!(recovered.by_name(&disk_name("disk3")).is_some());
        assert!(
            f.paths.pending_op_json().exists(),
            "failing inhibitor should preserve the journal"
        );
        let preserved = journal::load_journal(&f.paths).unwrap().unwrap();
        assert!(
            matches!(
                preserved.op,
                OpKind::Add {
                    phase: journal::AddPhase::PostAddBalanceRaid1,
                    ..
                }
            ),
            "preserved journal should be advanced to PostAddBalanceRaid1"
        );
        // The advance carries the committed membership as the target, so a
        // rerun binds each member at the devid btrfs gave it.
        assert_eq!(
            preserved.target_membership, recovered,
            "the advanced journal target must be the membership saved to pool.json"
        );
        for (name, devid) in [("disk1", 1), ("disk2", 2), ("disk3", 3)] {
            assert_eq!(
                preserved
                    .target_membership
                    .by_uuid(&uuid_for_name(name))
                    .and_then(|m| m.devid),
                Some(Devid::new(devid)),
                "{name}"
            );
        }
    }

    #[test]
    fn returned_replay_wrong_identity_fails_before_wipe_or_add() {
        for wrong_fsid in [false, true] {
            let f = PoolFixture::empty();
            let journal = recoverable_pool_mutation_add_journal();
            journal::write_journal(&f.paths, &journal).unwrap();
            let union = test_recovery_admission_membership(&journal);
            let targets = match &journal.op {
                OpKind::Add { targets, .. } => targets,
                _ => unreachable!("test journal is Add"),
            };

            let uuid = if wrong_fsid {
                "22222222-2222-2222-2222-222222222222"
            } else {
                "99999999-9999-9999-9999-999999999999"
            };
            let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", uuid);
            let mut runner = with_one_disk_pool_probe(MockRunner::default())
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    cryptsetup_uuid_ok("/dev/disk/by-id/virtio-disk2", uuid),
                )
                .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
                .with_mapper_open("braid-disk2", "/dev/vdb", uuid)
                .with_output(
                    CmdRequest::BtrfsDeviceScan {
                        device: "/dev/mapper/braid-disk2".into(),
                    },
                    ok_raw_empty("btrfs device scan"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/vda".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                );
            if wrong_fsid {
                runner = runner
                    .with_output_stdin(
                        CmdRequest::CryptsetupTestPassphrase {
                            device: "/dev/disk/by-id/virtio-disk2".into(),
                        },
                        TEST_PASSPHRASE_BYTES.to_vec(),
                        ok_raw_empty("cryptsetup open --test-passphrase"),
                    )
                    .with_output(
                        CmdRequest::BtrfsFilesystemShowTarget {
                            target: "/dev/mapper/braid-disk2".into(),
                        },
                        btrfs_show_target_fsid("ffffffff-ffff-ffff-ffff-ffffffffffff"),
                    );
            }
            let params = f.recover_params().build();
            let err = execute_add_pool_mutation_recovery(
                &runner,
                &fs,
                &MockByIdResolver::default(),
                &params,
                AddPoolReplayCtx {
                    credential: None,
                    journal: &journal,
                    union: &union,
                    targets,
                    pool: pool_state_one_disk(),
                    owed_balance: OwedRaid1Balance::Grow,
                },
                &std::collections::BTreeSet::new(),
            )
            .unwrap_err();
            let msg = err.to_string();
            if wrong_fsid {
                assert!(msg.contains("btrfs FSID mismatch"), "{msg}");
            } else {
                assert!(msg.contains("LUKS UUID mismatch"), "{msg}");
            }
            assert!(
                !runner.requests().iter().any(|r| matches!(
                    r,
                    CmdRequest::BtrfsDeviceScanForget { .. }
                        | CmdRequest::WipefsBtrfs { .. }
                        | CmdRequest::BtrfsDeviceAdd { .. }
                )),
                "wrong identity must fail before destructive replay"
            );
            assert!(f.paths.pending_op_json().exists());
            assert!(!f.paths.pool_json().exists());
        }
    }

    // Intent: cmd_recover mounts the pre-operation pool before failing a
    // mismatched recoverable add target at the post-mount replay gate.
    // Why it exists: A substring assertion on the UUID mismatch alone would
    // also pass under the old pre-mount hard-fail; the inhibitor count proves
    // recovery reached execute_add_pool_mutation_recovery.
    // Scenario: disk2's by-id path now points at a different LUKS container,
    // while disk1 is still enough to mount the pre-operation pool.
    #[test]
    fn recover_offline_pool_with_uuid_mismatched_target_reaches_post_mount_replay_then_fails() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ])
        .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let runner = MockRunner::default()
            .with_output(mountpoint_fail().0, mountpoint_fail().1)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "99999999-9999-9999-9999-999999999999",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk2")
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                inactive_mapper_status("braid-disk2"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_output_sequence(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                // Status probes for disk1, in order:
                // 1. initial mount planning sees disk1 closed;
                // 2. ensure_luks_open sees disk1 closed and opens it;
                // 3. post-mount probe_pool sees disk1 active.
                vec![
                    inactive_mapper_status("braid-disk1"),
                    inactive_mapper_status("braid-disk1"),
                    cryptsetup_status_active("braid-disk1", "/dev/vda"),
                ],
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("umount"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_one_disk(),
            );
        let request_log = runner.clone();
        let params = f.recover_params().sleep_inhibitor(&f.inhibitor).build();

        let err = cmd_recover(&runner, &fs, &MockByIdResolver::default(), &params).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("LUKS UUID mismatch"), "{msg}");
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "UUID mismatch is a reversible target check and must fail before inhibitor acquisition"
        );
        assert!(
            !request_log.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsDeviceScanForget { .. }
                    | CmdRequest::WipefsBtrfs { .. }
                    | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "wrong identity must fail before destructive add replay"
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    /// Journaled add of fresh disk2 plus fresh disk3, for the drop tests.
    fn fresh_two_target_add_journal() -> journal::Journal {
        let mut journal = fresh_pool_mutation_add_journal(Vec::new(), None);
        let OpKind::Add { targets, .. } = &mut journal.op else {
            unreachable!("test journal is Add");
        };
        targets
            .insert(
                uuid_for_name("disk3"),
                add_target(
                    "disk3",
                    "/dev/disk/by-id/virtio-disk3",
                    fresh_mode(Vec::new(), None),
                ),
            )
            .expect("insert disk3 target");
        journal.target_membership = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
            membership_entry("disk3", "/dev/disk/by-id/virtio-disk3", None, None),
        ]);
        journal
    }

    fn disk2_not_luks(runner: MockRunner) -> MockRunner {
        runner.with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/disk/by-id/virtio-disk2".into(),
            },
            err_raw(
                "cryptsetup luksUUID",
                1,
                "Device is not a valid LUKS device",
            ),
        )
    }

    fn assert_no_format_or_add(requests: &[CmdRequest]) {
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksFormat { .. }
                    | CmdRequest::CryptsetupLuksFormatTestArgs { .. }
                    | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "recover must never format a disk or add a dropped one: {requests:?}"
        );
    }

    // Intent: live-add recovery drops a fresh target luksFormat never wrote,
    //   and when no target remains it saves the pre-add membership and
    //   clears the journal, with no passphrase, open, format, or device add.
    // Why it exists: finding 261022-1230-002. Recover used to replay the
    //   journaled luksFormat argv, so an option cryptsetup rejected failed the
    //   same way on every run and the pool stayed in recovery mode. Recover
    //   now never formats a disk, and a target that still reads as not LUKS
    //   never reached btrfs, so dropping it is safe.
    // Scenario: a journal written before braid checked format options holds
    //   disk2 with the typo `--use-randomm`; luksFormat refused it after the
    //   journal was written, and the operator runs `braid recover`.
    #[test]
    fn add_recovery_drops_never_formatted_target_and_rolls_back() {
        let f = PoolFixture::empty();
        let journal = fresh_pool_mutation_add_journal(vec!["--use-randomm".into()], None);
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let OpKind::Add { targets, .. } = &journal.op else {
            unreachable!("test journal is Add");
        };
        let runner = disk2_not_luks(MockRunner::default());
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2"]);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let never_formatted = std::collections::BTreeSet::from([uuid_for_name("disk2")]);

        execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &never_formatted,
        )
        .expect("a never-formatted target drops and the add rolls back");

        assert!(
            !f.paths.pending_op_json().exists(),
            "the rollback clears the journal"
        );
        assert_eq!(
            membership::load_membership(&f.paths).unwrap(),
            journal.pre_membership,
            "pool.json must hold the pre-add membership"
        );
        let requests = runner.requests();
        assert_no_format_or_add(&requests);
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupTestPassphrase { .. } | CmdRequest::CryptsetupLuksOpen { .. }
            )),
            "a rollback needs no passphrase: {requests:?}"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "a rollback writes no disk, so it holds no sleep inhibitor"
        );
    }

    // Intent: when some targets remain after the drop, recover rewrites the
    //   journal without the dropped target, in both the add targets and
    //   target_membership, before the replay; an absent target still refuses.
    // Why it exists: the replay binds the live pool to the admission
    //   membership, so a dropped target left in target_membership would never
    //   bind and recover would refuse on every run. Writing the reduced
    //   journal first means a failure later in the replay reruns cleanly.
    // Scenario: an add of fresh disk2 and disk3 stopped after luksFormat
    //   refused disk2; disk3 is unplugged when the operator runs recover.
    #[test]
    fn add_recovery_rewrites_journal_without_dropped_target_before_replay() {
        let f = PoolFixture::empty();
        let journal = fresh_two_target_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let OpKind::Add { targets, .. } = &journal.op else {
            unreachable!("test journal is Add");
        };
        let runner = disk2_not_luks(with_two_disk_pool_probe(MockRunner::default()))
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            );
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2"]);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().build();
        let never_formatted = std::collections::BTreeSet::from([uuid_for_name("disk2")]);

        let err = execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &never_formatted,
        )
        .expect_err("an absent disk3 must refuse the replay");
        assert!(
            err.to_string().contains("'disk3'") && err.to_string().contains("not present"),
            "the refusal must name the absent disk3: {err}"
        );

        let rewritten = journal::load_journal(&f.paths)
            .unwrap()
            .expect("the rewritten journal stays for a rerun");
        let OpKind::Add { targets, .. } = &rewritten.op else {
            panic!("rewritten journal must stay an add: {rewritten:?}");
        };
        assert!(
            targets.get(&uuid_for_name("disk2")).is_none()
                && targets.get(&uuid_for_name("disk3")).is_some(),
            "only disk3 stays an add target: {targets:?}"
        );
        assert_eq!(
            membership_name_list(&rewritten.target_membership),
            vec!["disk1".to_owned(), "disk3".to_owned()],
            "target_membership must drop disk2 too"
        );
        assert_eq!(rewritten.pre_membership, journal.pre_membership);
        assert!(
            !f.paths.pool_json().exists(),
            "a partial drop writes no pool.json"
        );
        assert_no_format_or_add(&runner.requests());
    }

    // Intent: recover refuses to drop a never-formatted target, and keeps
    //   the journal, when btrfs reports a MISSING devid that no pre-operation
    //   member records.
    // Why it exists: a disk btrfs already added whose LUKS header now reads
    //   badly also probes as not LUKS. If btrfs counts a member braid cannot
    //   name, the not-LUKS disk could be it, and dropping it would let pool.json
    //   forget a member btrfs holds.
    // Scenario: btrfs added disk2 as devid 2, then disk2's header was damaged
    //   and its mapper closed; btrfs shows devid 2 as MISSING.
    #[test]
    fn add_recovery_refuses_drop_with_unrecorded_missing_devid() {
        let f = PoolFixture::empty();
        let journal = fresh_pool_mutation_add_journal(Vec::new(), None);
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let OpKind::Add { targets, .. } = &journal.op else {
            unreachable!("test journal is Add");
        };
        let runner = disk2_not_luks(MockRunner::default());
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2"]);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().passphrase_file(None).build();
        let never_formatted = std::collections::BTreeSet::from([uuid_for_name("disk2")]);
        let mut pool = pool_state_one_disk();
        pool.missing_count = 1;
        pool.total_devices = 2;
        pool.missing_devids = vec![Devid::new(2)];

        let err = execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool,
                owed_balance: OwedRaid1Balance::Grow,
            },
            &never_formatted,
        )
        .expect_err("an unrecorded MISSING devid must block the drop");
        let msg = err.to_string();
        assert!(
            msg.contains("cannot drop never-formatted add target(s) 'disk2'")
                && msg.contains("missing devid(s) 2"),
            "the refusal must name the target and the devid: {msg}"
        );
        assert_eq!(
            journal::load_journal(&f.paths).unwrap().as_ref(),
            Some(&journal),
            "the journal must stay as it was"
        );
        assert!(!f.paths.pool_json().exists());
        assert_no_format_or_add(&runner.requests());
    }

    // Intent: the replay refuses a fresh target that reads as not LUKS when
    //   the plan did not mark it for a drop, and never formats it.
    // Why it exists: recover never formats a disk (finding 261022-1230-002).
    //   The drop set comes from the plan's probe, so a disk that read
    //   otherwise then and as not LUKS now must stop the replay, not reach a
    //   format; a rerun probes again and drops it.
    // Scenario: disk2 was unplugged while recover planned and is back, still
    //   blank, by the time the replay probes it.
    #[test]
    fn add_replay_refuses_not_luks_target_outside_drop_set() {
        let f = PoolFixture::empty();
        let journal = fresh_pool_mutation_add_journal(vec!["--use-randomm".into()], None);
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let OpKind::Add { targets, .. } = &journal.op else {
            unreachable!("test journal is Add");
        };
        let inner = disk2_not_luks(with_two_disk_pool_probe(MockRunner::default()))
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            );
        let harness = RemountHarness::new(&["/dev/disk/by-id/virtio-disk2"], inner, &[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().build();

        let err = execute_add_pool_mutation_recovery(
            &harness.runner,
            &harness.fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect_err("a not-LUKS target outside the drop set must refuse");
        assert!(
            err.to_string().contains("recover never formats a disk"),
            "the refusal must say recover never formats: {err}"
        );
        assert_no_format_or_add(&harness.requests());
        assert!(f.paths.pending_op_json().exists());
    }

    // Intent: the dry-run of a live add whose only target luksFormat never
    //   wrote shows the drop and the rollback rows, and no format or device
    //   add.
    // Why it exists: the preview must show what execute does (ADR 022).
    //   Recover never formats a disk, so the old replay rows would preview a
    //   command recover no longer runs.
    // Scenario: the operator previews `braid recover --dry-run` after
    //   luksFormat refused disk2's options.
    #[test]
    fn plan_recover_dry_run_renders_never_formatted_drop_and_rollback() {
        let rendered = render_recover_dry_run(
            fresh_pool_mutation_add_journal(vec!["--use-randomm".into()], None),
            MockFs::new(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ]),
            disk2_not_luks(closed_disk1_dry_run_runner()),
            false,
        );

        assert!(
            rendered.contains(
                "drop fresh add target /dev/disk/by-id/virtio-disk2 from pending-op.json (luksFormat never wrote it"
            ),
            "missing drop row: {rendered}"
        );
        assert!(
            rendered.contains("write pre-operation pool.json")
                && rendered.contains("clear pending-op.json"),
            "missing rollback rows: {rendered}"
        );
        assert!(
            !rendered.contains("luksFormat /dev")
                && !rendered.contains("btrfs device add")
                && !rendered.contains("replay fresh add target"),
            "a dropped target must render no replay: {rendered}"
        );
    }

    #[test]
    fn fresh_replay_after_luks_format_does_not_reformat() {
        let f = PoolFixture::empty();
        let journal =
            fresh_pool_mutation_add_journal(vec!["--label".into(), "braid-disk2".into()], None);
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let inner = with_grow_balance_replay(with_two_disk_pool_probe(
            MockRunner::default()
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    cryptsetup_uuid_ok(
                        "/dev/disk/by-id/virtio-disk2",
                        "22222222-2222-2222-2222-222222222222",
                    ),
                )
                .with_output(
                    CmdRequest::CryptsetupLuksDumpText {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    luks_dump_label("braid-disk2"),
                )
                .with_mapper_open(
                    "braid-disk2",
                    "/dev/vdb",
                    "22222222-2222-2222-2222-222222222222",
                )
                .with_output(
                    CmdRequest::BtrfsFilesystemShowTarget {
                        target: "/dev/mapper/braid-disk2".into(),
                    },
                    btrfs_show_target_no_btrfs("/dev/mapper/braid-disk2"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/vda".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output(
                    CmdRequest::CryptsetupLuksHeaderBackup {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                        backup_path: f
                            .paths
                            .luks_headers_dir()
                            .join("braid-disk2.luksheader.tmp")
                            .display()
                            .to_string(),
                    },
                    ok_raw_empty("cryptsetup luksHeaderBackup"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupLuksOpen {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                        mapper: MapperName::from_basename("braid-disk2".into()),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceAdd {
                        device: "/dev/mapper/braid-disk2".into(),
                        mount_point: MountPoint::new("/mnt/storage".into()),
                        force: false,
                    },
                    ok_raw_empty("btrfs device add"),
                ),
        ));
        let harness =
            RemountHarness::new(&["/dev/disk/by-id/virtio-disk2"], inner, &["braid-disk2"])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().build();

        execute_add_pool_mutation_recovery(
            &harness.runner,
            &harness.fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("fresh replay should continue after preexisting LUKS format");

        let requests = harness.requests();
        assert_eq!(
            luks_dump_text_request_count(&requests, "/dev/disk/by-id/virtio-disk2"),
            3,
            "fresh add replay must use cached labels across pre-scan, credential verification, and replay"
        );
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksFormat { .. })),
            "already-formatted fresh target must not be reformatted"
        );
        assert!(
            !requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::BtrfsDeviceScan { device }
                        if device == "/dev/mapper/braid-disk2"
                )
            }),
            "fresh target after LUKS format has no btrfs signature and must not be scanned"
        );
    }

    #[test]
    fn ensure_keyfile_enrolled_is_idempotent_and_fails_on_probe_errors() {
        let key_dir = tempfile::TempDir::new().unwrap();
        let key_file = write_valid_keyfile(&key_dir, "braid.key");
        let accepted = MockRunner::default().with_output(
            CmdRequest::CryptsetupTestKeyFile {
                device: "/dev/disk/by-id/virtio-disk2".into(),
                key_file_path: key_file.display().to_string(),
            },
            ok_raw_empty("cryptsetup open --test-passphrase --key-file"),
        );
        ensure_keyfile_enrolled(
            &accepted,
            "/dev/disk/by-id/virtio-disk2",
            &passphrase("testpass"),
            &KeyFilePath::new(key_file.clone()),
        )
        .expect("accepted keyfile should be treated as already enrolled");
        assert!(
            !accepted
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. })),
            "accepted keyfile must skip luksAddKey"
        );

        let rejected = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupTestKeyFile {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    key_file_path: key_file.display().to_string(),
                },
                err_raw(
                    "cryptsetup open --test-passphrase --key-file",
                    2,
                    "No key available",
                ),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksAddKeyFile {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    key_file_path: key_file.display().to_string(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup luksAddKey"),
            );
        ensure_keyfile_enrolled(
            &rejected,
            "/dev/disk/by-id/virtio-disk2",
            &passphrase("testpass"),
            &KeyFilePath::new(key_file.clone()),
        )
        .expect("rejected keyfile should be enrolled with the passphrase");
        assert!(
            rejected
                .requests()
                .iter()
                .any(|r| { matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. }) })
        );

        let busy = MockRunner::default().with_output(
            CmdRequest::CryptsetupTestKeyFile {
                device: "/dev/disk/by-id/virtio-disk2".into(),
                key_file_path: key_file.display().to_string(),
            },
            err_raw(
                "cryptsetup open --test-passphrase --key-file",
                5,
                "Device busy",
            ),
        );
        let err = ensure_keyfile_enrolled(
            &busy,
            "/dev/disk/by-id/virtio-disk2",
            &passphrase("testpass"),
            &KeyFilePath::new(key_file.clone()),
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("device is already open or busy"),
            "{err}"
        );
    }

    // Intent: recovery replay rejects wrong-size journaled keyfiles before
    //   cryptsetup verification or enrollment.
    // Why it exists: recovery consumes the original `add --enroll` /
    //   `replace --enroll` path later, after the file may have changed.
    // Scenario: a crash journal names a keyfile that has since been truncated.
    #[test]
    fn ensure_keyfile_enrolled_rejects_wrong_size_before_cryptsetup() {
        let dir = tempfile::TempDir::new().unwrap();
        let key_file = dir.path().join("braid.key");
        std::fs::write(&key_file, b"too-short").unwrap();
        let runner = MockRunner::default();

        let err = ensure_keyfile_enrolled(
            &runner,
            "/dev/disk/by-id/virtio-disk2",
            &passphrase("testpass"),
            &KeyFilePath::new(key_file.clone()),
        )
        .expect_err("wrong-size keyfile must fail");

        match err {
            RecoverError::Luks(luks::LuksError::Validation(msg)) => {
                assert!(msg.contains("4096"), "expected 4096 in: {msg}");
            }
            other => panic!("expected Luks Validation, got {other:?}"),
        }
        assert!(
            runner.requests().is_empty(),
            "validation must fail before cryptsetup requests; got {:?}",
            runner.requests()
        );
    }

    #[test]
    fn fresh_missing_or_wrong_target_preserves_journal_and_pool_json() {
        for wrong_label in [false, true] {
            let f = PoolFixture::empty();
            let fs = if wrong_label {
                MockFs::new(&["/dev/disk/by-id/virtio-disk2"])
            } else {
                MockFs::new(&[])
            };
            let journal =
                fresh_pool_mutation_add_journal(vec!["--label".into(), "braid-disk2".into()], None);
            journal::write_journal(&f.paths, &journal).unwrap();
            let union = test_recovery_admission_membership(&journal);
            let targets = match &journal.op {
                OpKind::Add { targets, .. } => targets,
                _ => unreachable!("test journal is Add"),
            };
            let mut runner = MockRunner::default().with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            );
            if wrong_label {
                runner = runner
                    .with_output(
                        CmdRequest::CryptsetupLuksUuid {
                            device: "/dev/disk/by-id/virtio-disk2".into(),
                        },
                        cryptsetup_uuid_ok(
                            "/dev/disk/by-id/virtio-disk2",
                            "22222222-2222-2222-2222-222222222222",
                        ),
                    )
                    .with_output(
                        CmdRequest::CryptsetupLuksDumpText {
                            device: "/dev/disk/by-id/virtio-disk2".into(),
                        },
                        luks_dump_label("wrong-label"),
                    )
                    .with_mapper_closed("braid-disk2");
            }
            let params = f.recover_params().build();
            let err = execute_add_pool_mutation_recovery(
                &runner,
                &fs,
                &MockByIdResolver::default(),
                &params,
                AddPoolReplayCtx {
                    credential: None,
                    journal: &journal,
                    union: &union,
                    targets,
                    pool: pool_state_one_disk(),
                    owed_balance: OwedRaid1Balance::Grow,
                },
                &std::collections::BTreeSet::new(),
            )
            .unwrap_err();
            let msg = err.to_string();
            if wrong_label {
                assert!(msg.contains("unexpected LUKS label"), "{msg}");
            } else {
                assert!(
                    msg.contains(
                        "recover add target 'disk2' (/dev/disk/by-id/virtio-disk2) is not present; reconnect it"
                    ),
                    "{msg}"
                );
            }
            assert!(f.paths.pending_op_json().exists());
            assert!(!f.paths.pool_json().exists());
        }
    }

    #[test]
    fn fresh_present_target_rejects_bad_credential_before_pool_json() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"])
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222")
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");
        let journal =
            fresh_pool_mutation_add_journal(vec!["--label".into(), "braid-disk2".into()], None);
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"wrongpass").unwrap();
        }
        let runner = with_one_disk_pool_probe(
            MockRunner::default()
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    cryptsetup_uuid_ok(
                        "/dev/disk/by-id/virtio-disk2",
                        "22222222-2222-2222-2222-222222222222",
                    ),
                )
                .with_output(
                    CmdRequest::CryptsetupLuksDumpText {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    luks_dump_label("braid-disk2"),
                )
                .with_mapper_open(
                    "braid-disk2",
                    "/dev/vdb",
                    "22222222-2222-2222-2222-222222222222",
                )
                .with_output(
                    CmdRequest::BtrfsFilesystemShowTarget {
                        target: "/dev/mapper/braid-disk2".into(),
                    },
                    btrfs_show_target_no_btrfs("/dev/mapper/braid-disk2"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/vda".into(),
                    },
                    b"wrongpass".to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                    },
                    b"wrongpass".to_vec(),
                    err_raw("cryptsetup open --test-passphrase", 2, "No key available"),
                ),
        );
        let params = f
            .recover_params()
            .passphrase_file(Some(passphrase_file.path()))
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let err = execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("recover add passphrase was rejected by 'disk2'"),
            "{err}"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "bad add recovery credential must fail before acquiring the inhibitor"
        );
        let requests = runner.requests();
        assert_eq!(
            luks_dump_text_request_count(&requests, "/dev/disk/by-id/virtio-disk2"),
            2,
            "fresh add credential verification must use cached labels from its probe calls"
        );
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksFormat { .. }
                    | CmdRequest::BtrfsDeviceAdd { .. }
                    | CmdRequest::BtrfsDeviceScanForget { .. }
                    | CmdRequest::WipefsBtrfs { .. }
            )),
            "bad fresh-target credential must stop before mutation"
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    #[test]
    fn post_add_recovery_never_prepares_or_adds_targets() {
        let f = PoolFixture::empty();
        let journal = committed_two_disk_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = with_two_disk_pool_probe(with_grow_balance_replay(MockRunner::default()));
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();

        execute_add_post_balance_recovery(
            &runner,
            &two_disk_pool_fs(),
            &resolver,
            &params,
            &journal,
            pool_state_two_disks(),
            OwedRaid1Balance::Grow,
            false,
        )
        .expect("post-add recovery should only finish balance work");

        let requests = runner.requests();
        assert_eq!(f.inhibitor.acquire_count(), 1);
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksFormat { .. }
                    | CmdRequest::CryptsetupLuksAddKeyFile { .. }
                    | CmdRequest::CryptsetupLuksHeaderBackup { .. }
                    | CmdRequest::BtrfsDeviceScanForget { .. }
                    | CmdRequest::WipefsBtrfs { .. }
                    | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "PostAddBalanceRaid1 must not replay disk preparation or add"
        );
        assert!(!f.paths.pending_op_json().exists());
        assert!(f.paths.pool_json().exists());
    }

    #[test]
    fn post_add_recovery_refuses_membership_mismatch_and_preserves_journal() {
        let f = PoolFixture::empty();
        let journal = committed_two_disk_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default().with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/vda".into(),
            },
            TEST_PASSPHRASE_BYTES.to_vec(),
            ok_raw_empty("cryptsetup open --test-passphrase"),
        );
        let params = f.recover_params().passphrase_file(None).build();
        let err = execute_add_post_balance_recovery(
            &runner,
            &MockFs::new(&[]),
            &MockByIdResolver::default(),
            &params,
            &journal,
            pool_state_one_disk(),
            OwedRaid1Balance::Grow,
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("post-add recovery expected live pool membership")
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    #[test]
    fn pool_mutation_inhibitor_failure_stops_before_destructive_replay() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[]);
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let runner = MockRunner::default().with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/vda".into(),
            },
            TEST_PASSPHRASE_BYTES.to_vec(),
            ok_raw_empty("cryptsetup open --test-passphrase"),
        );
        let inhibitor = FailingInhibitor;
        let params = f.recover_params().sleep_inhibitor(&inhibitor).build();
        let err = execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: pool_state_one_disk(),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("could not acquire sleep inhibitor")
        );
        assert!(
            !runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::WipefsBtrfs { .. } | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "inhibitor failure must stop before destructive replay"
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    #[test]
    fn post_add_inhibitor_failure_stops_before_balance_and_preserves_journal() {
        let f = PoolFixture::empty();
        let journal = committed_two_disk_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let inhibitor = FailingInhibitor;
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&inhibitor)
            .build();

        let err = execute_add_post_balance_recovery(
            &runner,
            &MockFs::new(&[]),
            &resolver,
            &params,
            &journal,
            pool_state_two_disks(),
            OwedRaid1Balance::Grow,
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("could not acquire sleep inhibitor")
        );
        assert!(
            !runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsBalanceStatus { .. }
                    | CmdRequest::BtrfsBalanceRaid1 { .. }
                    | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "post-add inhibitor failure must stop before balance"
        );
        assert!(f.paths.pending_op_json().exists());
    }

    // Intent: recover finishes an interrupted 3rd-disk add with the hard
    //   RAID1 convert live add runs, not the soft one.
    // Why it exists: on an already-RAID1 pool every chunk is raid1, so a
    //   soft convert skips them all and leaves the new disk empty. Recover
    //   then cleared the journal as if the add had finished, and a nearly
    //   full pool kept failing writes (hunt finding 261022-1230-008).
    // Scenario: a 2-disk pool grows to 3 disks. btrfs device add committed
    //   disk3, then the post-add balance was cut off. The pool is mounted
    //   and its balance is idle when the operator runs recover.
    #[test]
    fn cmd_recover_post_add_replays_hard_convert_onto_third_disk() {
        let f = PoolFixture::empty();
        let journal = two_disk_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let (mp_req, mp_out) = mountpoint_ok();
        // Both converts are mocked, so the run completes whichever one
        // recover picks and the assertions below name the one it picked.
        let runner = with_three_disk_pool_probe(with_idle_balance_status(
            MockRunner::default().with_output(mp_req, mp_out),
        ))
        .with_output(
            CmdRequest::BtrfsBalanceRaid1 {
                mount_point: MountPoint::new("/mnt/storage".into()),
            },
            ok_raw_empty("btrfs balance start"),
        )
        .with_output(
            CmdRequest::BtrfsBalanceRaid1Soft {
                mount_point: MountPoint::new("/mnt/storage".into()),
            },
            ok_raw_empty("btrfs balance start"),
        );
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222")
            .with_luks_dm("braid-disk3", "33333333-3333-3333-3333-333333333333");
        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk2"),
            ("/dev/vdc", "virtio-disk3"),
        ]);

        cmd_recover(
            &runner,
            &fs,
            &resolver,
            &f.recover_params().passphrase_file(None).build(),
        )
        .expect("post-add recovery should finish the owed balance");

        let requests = runner.requests();
        assert!(
            requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsBalanceRaid1 { .. })),
            "post-add recovery must run the hard RAID1 convert live add runs: {requests:?}"
        );
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "a soft convert skips every raid1 chunk and leaves disk3 empty: {requests:?}"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    /// Existing-pool add of disk3 into a disk1+disk2 pool whose pool.json
    /// recorded disk1 at devid 1 and disk2 at devid 2. The target snapshot
    /// gives disk3 no devid, as `AddPlan::execute` writes it.
    fn add_disk3_to_disk1_disk2_journal(phase: journal::AddPhase) -> journal::Journal {
        let entry = |name: &str, devid: Option<u64>| {
            membership_entry(
                name,
                &format!("/dev/disk/by-id/virtio-{name}"),
                None,
                devid.map(Devid::new),
            )
        };
        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Add {
                phase,
                targets: add_targets(vec![(
                    uuid_for_name("disk3"),
                    add_target(
                        "disk3",
                        "/dev/disk/by-id/virtio-disk3",
                        fresh_mode(Vec::new(), None),
                    ),
                )]),
            },
            pre_membership: membership_from(vec![entry("disk1", Some(1)), entry("disk2", Some(2))]),
            target_membership: membership_from(vec![
                entry("disk1", Some(1)),
                entry("disk2", Some(2)),
                entry("disk3", None),
            ]),
        }
    }

    /// Live pool after disk3's `btrfs device add` committed at devid 3:
    /// disk1 and disk3 present, disk2 absent. `null_backed_disk2` picks how
    /// disk2 is absent: btrfs-MISSING with no open mapper, or hot-unplugged
    /// with its mapper still open.
    fn pool_state_disk1_disk3_without_disk2(null_backed_disk2: bool) -> PoolState {
        let present = |name: &str, devid: u64, underlying: &str| PoolDevice {
            mapper: MapperName::from_basename(format!("braid-{name}")),
            luks_uuid: uuid_for_name(name),
            devid: Devid::new(devid),
            underlying: underlying.into(),
        };
        PoolState {
            mounted: true,
            devices: vec![
                present("disk1", 1, "/dev/vda"),
                present("disk3", 3, "/dev/vdc"),
            ],
            missing_count: 1,
            total_devices: 3,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: if null_backed_disk2 {
                vec![]
            } else {
                vec![Devid::new(2)]
            },
            null_underlying: if null_backed_disk2 {
                vec![NullUnderlyingDevice {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                    luks_uuid: uuid_for_name("disk2"),
                    devid: Devid::new(2),
                }]
            } else {
                vec![]
            },
        }
    }

    /// Runner and device-mapper table for a mounted pool with an idle
    /// balance, so an owed replay's fresh probe sees the pool a test hands
    /// recovery. `devids` lists each btrfs devid row: `Some((name, backing))`
    /// for an open `braid-<name>` mapper (`backing` `(null)` when its disk is
    /// gone), `None` for a `path MISSING` row.
    fn mounted_pool_probe(devids: &[(u64, Option<(&str, &str)>)]) -> (MockRunner, MockFs) {
        let mut rows = String::new();
        for (devid, row) in devids {
            rows.push_str(&match row {
                Some((name, _)) => format!(
                    "\tdevid    {devid} size 10.00GiB used 2.00GiB path /dev/mapper/braid-{name}\n"
                ),
                None => format!("\tdevid    {devid} size 0 used 0 path MISSING\n"),
            });
        }
        let show = ok_raw(
            "btrfs filesystem show /mnt/storage",
            &format!(
                "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                 \tTotal devices {} FS bytes used 1.00GiB\n{rows}",
                devids.len()
            ),
        );
        let mut runner = with_idle_balance_status(MockRunner::default()).with_output(
            CmdRequest::BtrfsFilesystemShow {
                mount_point: MountPoint::new("/mnt/storage".into()),
            },
            show,
        );
        let mut fs = MockFs::new(&[]);
        for (name, backing) in devids.iter().filter_map(|(_, row)| *row) {
            let mapper = format!("braid-{name}");
            runner = runner.with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename(mapper.clone()),
                },
                cryptsetup_status_active(&mapper, backing),
            );
            fs = fs.with_luks_dm(&mapper, uuid_for_name(name).as_str());
        }
        (runner, fs)
    }

    /// Runner and device-mapper table for the pool
    /// `pool_state_disk1_disk3_without_disk2` describes.
    fn disk1_disk3_without_disk2_probe(null_backed_disk2: bool) -> (MockRunner, MockFs) {
        let disk2 = null_backed_disk2.then_some(("disk2", "(null)"));
        mounted_pool_probe(&[
            (1, Some(("disk1", "/dev/vda"))),
            (2, disk2),
            (3, Some(("disk3", "/dev/vdc"))),
        ])
    }

    /// Run one add-recovery tail with status lines captured, then assert it
    /// finished, kept disk2 in pool.json at its recorded devid, skipped the
    /// owed balance on the degraded pool, and cleared the journal.
    fn assert_add_recovery_kept_disk2(
        f: &PoolFixture,
        runner: &MockRunner,
        context: &str,
        run: impl FnOnce() -> Result<(), RecoverError>,
    ) {
        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(run());
        });
        result
            .expect("recovery ran")
            .unwrap_or_else(|e| panic!("{context}: add recovery should finish: {e}"));

        assert!(
            !f.paths.pending_op_json().exists(),
            "{context}: recovery must clear the journal"
        );
        let recovered = membership::load_membership(&f.paths).unwrap();
        let disk2 = recovered
            .by_uuid(&uuid_for_name("disk2"))
            .unwrap_or_else(|| panic!("{context}: pool.json must keep disk2: {recovered:?}"));
        assert_eq!(disk2.devid, Some(Devid::new(2)), "{context}");
        assert_eq!(
            disk2.by_id.as_str(),
            "/dev/disk/by-id/virtio-disk2",
            "{context}"
        );
        let disk3 = recovered
            .by_uuid(&uuid_for_name("disk3"))
            .unwrap_or_else(|| panic!("{context}: pool.json must hold disk3: {recovered:?}"));
        assert_eq!(disk3.devid, Some(Devid::new(3)), "{context}");
        assert_eq!(recovered.len(), 3, "{context}: {recovered:?}");
        assert!(
            !runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsBalanceRaid1 { .. } | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "{context}: a degraded pool must not get the owed RAID1 balance"
        );
        let skip_line = format!("[skip] {}\n", OwedRaid1Balance::Grow.skip_line());
        assert!(
            captured.contains(&skip_line),
            "{context}: recovery must print live add's degraded skip line, got: {captured:?}"
        );
    }

    // Intent: existing-pool add recovery on a degraded pool keeps a member
    // btrfs reports only as a MISSING devid in pool.json, bound by the devid
    // pool.json recorded for it, and finishes.
    // Why it exists: add recovery rebuilt pool.json from present devices
    // alone, dropped disk2, advanced the journal, then refused on every rerun
    // because the live devices never matched the target (hunt finding
    // 261022-1230-007). remove-missing then could not name the devid.
    // Scenario: a disk1+disk2 pool, `braid add disk3`, power loss after
    // `btrfs device add`, and disk2 does not come back; the operator runs
    // `braid recover --allow-degraded`.
    #[test]
    fn add_recovery_keeps_absent_member_missing_devid() {
        let f = PoolFixture::empty();
        let journal = add_disk3_to_disk1_disk2_journal(journal::AddPhase::PoolMutation);
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let OpKind::Add { targets, .. } = &journal.op else {
            unreachable!("test journal is Add")
        };
        let (runner, fs) = disk1_disk3_without_disk2_probe(false);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-disk3")]);
        let params = f.recover_params().passphrase_file(None).build();

        assert_add_recovery_kept_disk2(&f, &runner, "MISSING disk2", || {
            execute_add_pool_mutation_recovery(
                &runner,
                &fs,
                &resolver,
                &params,
                AddPoolReplayCtx {
                    credential: None,
                    journal: &journal,
                    union: &union,
                    targets,
                    pool: pool_state_disk1_disk3_without_disk2(false),
                    owed_balance: OwedRaid1Balance::Grow,
                },
                &std::collections::BTreeSet::new(),
            )
        });
    }

    // Intent: add recovery already past the phase advance finishes on the
    // same degraded pool and keeps disk2 at its recorded devid.
    // Why it exists: installs this bug stranded hold a PostAddBalanceRaid1
    // journal whose target still records disk2 at devid 2; a rerun must heal
    // them instead of refusing forever (hunt finding 261022-1230-007).
    // Scenario: the stranded install from the MISSING case reruns
    // `braid recover --allow-degraded` with disk2 still gone.
    #[test]
    fn add_recovery_keeps_absent_member_after_phase_advance() {
        let f = PoolFixture::empty();
        let journal = add_disk3_to_disk1_disk2_journal(journal::AddPhase::PostAddBalanceRaid1);
        journal::write_journal(&f.paths, &journal).unwrap();
        let (runner, fs) = disk1_disk3_without_disk2_probe(false);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-disk3")]);
        let params = f.recover_params().passphrase_file(None).build();

        assert_add_recovery_kept_disk2(&f, &runner, "phase advanced", || {
            execute_add_post_balance_recovery(
                &runner,
                &fs,
                &resolver,
                &params,
                &journal,
                pool_state_disk1_disk3_without_disk2(false),
                OwedRaid1Balance::Grow,
                false,
            )
        });
    }

    // Intent: add recovery keeps a pre-existing member whose disk is gone
    // while its mapper stays open, bound by the LUKS UUID the mapper records.
    // Why it exists: the null-backed member was dropped the same way as a
    // MISSING one, since recovery read present devices only (hunt finding
    // 261022-1230-007).
    // Scenario: as in the MISSING case, but disk2's disk was hot-unplugged
    // while the pool stayed mounted, so btrfs has not marked it MISSING.
    #[test]
    fn add_recovery_keeps_absent_member_null_backed() {
        let f = PoolFixture::empty();
        let journal = add_disk3_to_disk1_disk2_journal(journal::AddPhase::PoolMutation);
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let OpKind::Add { targets, .. } = &journal.op else {
            unreachable!("test journal is Add")
        };
        let (runner, fs) = disk1_disk3_without_disk2_probe(true);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-disk3")]);
        let params = f.recover_params().passphrase_file(None).build();

        assert_add_recovery_kept_disk2(&f, &runner, "null-backed disk2", || {
            execute_add_pool_mutation_recovery(
                &runner,
                &fs,
                &resolver,
                &params,
                AddPoolReplayCtx {
                    credential: None,
                    journal: &journal,
                    union: &union,
                    targets,
                    pool: pool_state_disk1_disk3_without_disk2(true),
                    owed_balance: OwedRaid1Balance::Grow,
                },
                &std::collections::BTreeSet::new(),
            )
        });
    }

    /// The `PoolState` a probe of `mounted_pool_probe(devids)` returns.
    fn mounted_pool_state(devids: &[(u64, Option<(&str, &str)>)]) -> PoolState {
        let mut pool = PoolState {
            mounted: true,
            devices: vec![],
            missing_count: 0,
            total_devices: devids.len() as u64,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        };
        for (devid, row) in devids {
            let devid = Devid::new(*devid);
            match row {
                Some((name, "(null)")) => pool.null_underlying.push(NullUnderlyingDevice {
                    mapper: MapperName::from_basename(format!("braid-{name}")),
                    luks_uuid: uuid_for_name(name),
                    devid,
                }),
                Some((name, backing)) => pool.devices.push(PoolDevice {
                    mapper: MapperName::from_basename(format!("braid-{name}")),
                    luks_uuid: uuid_for_name(name),
                    devid,
                    underlying: (*backing).into(),
                }),
                None => pool.missing_devids.push(devid),
            }
        }
        pool.missing_count = pool.total_devices - pool.devices.len() as u64;
        pool
    }

    /// pool.json's members as (name, devid), sorted by name.
    fn saved_member_devids(paths: &StatePaths) -> Vec<(String, Option<Devid>)> {
        membership::load_membership(paths)
            .unwrap()
            .iter_by_name()
            .into_iter()
            .map(|(_, m)| (m.name.as_str().to_owned(), m.devid))
            .collect()
    }

    fn named_devids(entries: &[(&str, u64)]) -> Vec<(String, Option<Devid>)> {
        entries
            .iter()
            .map(|(name, devid)| ((*name).to_owned(), Some(Devid::new(*devid))))
            .collect()
    }

    // Intent: add recovery finishes when btrfs reports a MISSING devid that no
    // journaled member records, and leaves that devid out of pool.json.
    // Why it exists: the binding carries such a devid instead of refusing, as
    // live add does; only a member with no recorded devid beside it is
    // ambiguous (hunt finding 261022-1230-007).
    // Scenario: an install an earlier recovery stranded still has btrfs devid
    // 7 MISSING with no pool.json member; `braid add disk3` then crashes after
    // `btrfs device add`.
    #[test]
    fn add_recovery_finishes_beside_stray_missing_devid() {
        let f = PoolFixture::empty();
        let journal = add_disk3_to_disk1_disk2_journal(journal::AddPhase::PoolMutation);
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let OpKind::Add { targets, .. } = &journal.op else {
            unreachable!("test journal is Add")
        };
        let devids = [
            (1, Some(("disk1", "/dev/vda"))),
            (2, Some(("disk2", "/dev/vdb"))),
            (3, Some(("disk3", "/dev/vdc"))),
            (7, None),
        ];
        let (runner, fs) = mounted_pool_probe(&devids);
        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk2"),
            ("/dev/vdc", "virtio-disk3"),
        ]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: mounted_pool_state(&devids),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("a MISSING devid no member records must not block add recovery");

        assert!(!f.paths.pending_op_json().exists());
        assert_eq!(
            saved_member_devids(&f.paths),
            named_devids(&[("disk1", 1), ("disk2", 2), ("disk3", 3)])
        );
    }

    // Intent: add recovery counts a new disk btrfs holds null-backed as
    // committed, keeps it at the devid its mapper holds, and sweeps that
    // devid's acked-stats ghost.
    // Why it exists: recovery read present devices only, so a committed target
    // that was hot-unplugged looked uncommitted and was dropped; the sweep then
    // could not find its devid (hunt finding 261022-1230-007).
    // Scenario: `braid add disk3` crashed after `btrfs device add` gave disk3
    // devid 3, and disk3's cable then came loose with its mapper still open.
    #[test]
    fn add_recovery_keeps_null_backed_new_target_and_sweeps_its_devid() {
        let f = PoolFixture::empty();
        let journal = add_disk3_to_disk1_disk2_journal(journal::AddPhase::PoolMutation);
        journal::write_journal(&f.paths, &journal).unwrap();
        let control = acked_disk(false, 11);
        seed_acked_stats(&f.paths, &[(1, control.clone()), (3, acked_disk(true, 33))]);
        let union = test_recovery_admission_membership(&journal);
        let OpKind::Add { targets, .. } = &journal.op else {
            unreachable!("test journal is Add")
        };
        let devids = [
            (1, Some(("disk1", "/dev/vda"))),
            (2, Some(("disk2", "/dev/vdb"))),
            (3, Some(("disk3", "(null)"))),
        ];
        let (runner, fs) = mounted_pool_probe(&devids);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_add_pool_mutation_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: mounted_pool_state(&devids),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .expect("a null-backed new target is committed");

        assert!(!f.paths.pending_op_json().exists());
        assert_eq!(
            saved_member_devids(&f.paths),
            named_devids(&[("disk1", 1), ("disk2", 2), ("disk3", 3)])
        );
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert_eq!(
            recovered
                .by_uuid(&uuid_for_name("disk3"))
                .unwrap()
                .by_id
                .as_str(),
            "/dev/disk/by-id/virtio-disk3"
        );
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceAdd { .. })),
            "a committed target must not be re-added"
        );
        let acked = alert::load_acked_stats(&f.paths);
        assert_eq!(acked.0.get("1"), Some(&control));
        assert!(!acked.0.contains_key("3"), "{acked:?}");
    }

    // Intent: add recovery past the phase advance binds a new disk that is now
    // MISSING by the devid the advanced journal records for it.
    // Why it exists: the advance used to keep the devid-less target snapshot,
    // so a new disk lost before the balance finished could not be bound and
    // was dropped (hunt finding 261022-1230-007).
    // Scenario: `braid add disk3` committed and advanced the journal, the RAID1
    // balance was interrupted, and disk3 did not come back.
    #[test]
    fn add_recovery_after_phase_advance_binds_missing_new_disk_by_devid() {
        let f = PoolFixture::empty();
        let mut journal = add_disk3_to_disk1_disk2_journal(journal::AddPhase::PostAddBalanceRaid1);
        journal
            .target_membership
            .by_uuid_mut(&uuid_for_name("disk3"))
            .unwrap()
            .devid = Some(Devid::new(3));
        journal::write_journal(&f.paths, &journal).unwrap();
        let devids = [
            (1, Some(("disk1", "/dev/vda"))),
            (2, Some(("disk2", "/dev/vdb"))),
            (3, None),
        ];
        let (runner, fs) = mounted_pool_probe(&devids);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_add_post_balance_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            &journal,
            mounted_pool_state(&devids),
            OwedRaid1Balance::Grow,
            false,
        )
        .expect("the journaled devid binds the MISSING new disk");

        assert!(!f.paths.pending_op_json().exists());
        assert_eq!(
            saved_member_devids(&f.paths),
            named_devids(&[("disk1", 1), ("disk2", 2), ("disk3", 3)])
        );
    }

    // Intent: add recovery before the phase advance refuses at the new disk,
    // with the journal kept and no pool.json written, when btrfs added that
    // disk and now shows it only as a MISSING devid.
    // Why it exists: the journal records no devid for a new disk until the
    // advance to PostAddBalanceRaid1, so the binding cannot tell that devid is
    // the target. Recovery must not count the target as committed, drop it,
    // or re-add it; docs/commands/recover.md step 8 states this refusal.
    // Scenario: `braid add disk3` crashed after `btrfs device add` gave disk3
    // devid 3, and disk3 did not come back before `braid recover`.
    #[test]
    fn add_recovery_refuses_missing_new_disk_before_phase_advance() {
        let f = PoolFixture::empty();
        let journal = add_disk3_to_disk1_disk2_journal(journal::AddPhase::PoolMutation);
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let OpKind::Add { targets, .. } = &journal.op else {
            unreachable!("test journal is Add")
        };
        let devids = [
            (1, Some(("disk1", "/dev/vda"))),
            (2, Some(("disk2", "/dev/vdb"))),
            (3, None),
        ];
        let mut runner = MockRunner::default();
        for device in ["/dev/vda", "/dev/vdb"] {
            runner = runner.with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: device.into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            );
        }
        let params = f.recover_params().build();

        let err = execute_add_pool_mutation_recovery(
            &runner,
            &MockFs::new(&[]),
            &MockByIdResolver::default(),
            &params,
            AddPoolReplayCtx {
                credential: None,
                journal: &journal,
                union: &union,
                targets,
                pool: mounted_pool_state(&devids),
                owed_balance: OwedRaid1Balance::Grow,
            },
            &std::collections::BTreeSet::new(),
        )
        .unwrap_err();

        assert!(
            err.to_string().contains(
                "recover add target 'disk3' (/dev/disk/by-id/virtio-disk3) is not present"
            ),
            "{err}"
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
        assert!(
            !runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsDeviceAdd { .. } | CmdRequest::CryptsetupLuksFormat { .. }
            )),
            "an absent target must stop before any disk write: {:?}",
            runner.requests()
        );
    }

    // Intent: bootstrap add recovery keeps a member btrfs holds null-backed,
    // at the devid its mapper holds.
    // Why it exists: the generic path rebuilt pool.json from present devices
    // only, so a hot-unplugged bootstrap disk vanished from pool.json while
    // btrfs still held it (hunt finding 261022-1230-007).
    // Scenario: `braid add disk1 disk2` on an empty pool crashed after mkfs;
    // disk2's cable came loose before `braid recover`.
    #[test]
    fn bootstrap_recovery_keeps_null_backed_member() {
        let f = PoolFixture::empty();
        let journal = bootstrap_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let plan = recover_work_plan_for_journal(journal);
        let devids = [
            (1, Some(("disk1", "/dev/vda"))),
            (2, Some(("disk2", "(null)"))),
        ];
        let (runner, fs) = mounted_pool_probe(&devids);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_generic_live_pool_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            &plan,
            mounted_pool_state(&devids),
            Some(OwedRaid1Balance::RestoreSingle),
        )
        .expect("bootstrap recovery should keep the null-backed member");

        assert!(!f.paths.pending_op_json().exists());
        assert_eq!(
            saved_member_devids(&f.paths),
            named_devids(&[("disk1", 1), ("disk2", 2)])
        );
    }

    // Intent: bootstrap add recovery refuses, keeping the journal, when btrfs
    // reports a member only as a MISSING devid.
    // Why it exists: a bootstrap journal records no devids, so the MISSING
    // devid could be any member not otherwise bound; the generic path used to
    // drop that member from pool.json and clear the journal (hunt finding
    // 261022-1230-007).
    // Scenario: `braid add disk1 disk2` on an empty pool crashed after mkfs;
    // disk2 is gone and the operator runs `braid recover --allow-degraded`.
    #[test]
    fn bootstrap_recovery_refuses_member_seen_only_as_missing_devid() {
        let f = PoolFixture::empty();
        let journal = bootstrap_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let plan = recover_work_plan_for_journal(journal);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();

        let err = execute_generic_live_pool_recovery(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &resolver,
            &params,
            &plan,
            mounted_pool_state(&[(1, Some(("disk1", "/dev/vda"))), (2, None)]),
            Some(OwedRaid1Balance::RestoreSingle),
        )
        .unwrap_err();

        match err {
            RecoverError::AmbiguousMissingDevid { devids, members } => {
                assert_eq!(devids, vec![Devid::new(2)]);
                assert_eq!(members, vec![disk_name("disk2")]);
            }
            other => panic!("expected AmbiguousMissingDevid, got {other:?}"),
        }
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    // Intent: committed remove-missing recovery finishes when btrfs reports a
    // second MISSING devid that no journaled member records.
    // Why it exists: the old topology gate refused any MISSING devid the
    // journal could not name, so an install an earlier recovery stranded
    // could never finish (hunt finding 261022-1230-007).
    // Scenario: btrfs still reports a stray devid 7 MISSING; remove-missing
    // removed devid 2 and crashed before clearing the journal.
    #[test]
    fn remove_missing_recovery_finishes_beside_stray_missing_devid() {
        let f = PoolFixture::empty();
        let mut journal = remove_missing_journal();
        journal.op = OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PoolMutation,
            devid: Devid::new(2),
            restore_raid1_after_commit: false,
        };
        journal::write_journal(&f.paths, &journal).unwrap();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();

        execute_remove_missing_pool_mutation_recovery(
            &MockRunner::default(),
            &MockFs::new(&[]),
            &resolver,
            &params,
            &journal,
            mounted_pool_state(&[(1, Some(("disk1", "/dev/vda"))), (7, None)]),
            Devid::new(2),
            None,
        )
        .expect("a stray MISSING devid must not block remove-missing recovery");

        assert!(!f.paths.pending_op_json().exists());
        assert_eq!(saved_member_devids(&f.paths), named_devids(&[("disk1", 1)]));
    }

    // Intent: RemoveMissing::PoolMutation recovery treats the primary
    // mutation as committed when the journaled missing devid is gone.
    // Why it exists: recovery must advance to post-maintenance and finish the
    // owed RAID1 work without ever rerunning btrfs device remove.
    // Scenario: remove-missing removed devid 2 and crashed before clearing
    // the journal; recover writes committed membership, balances, and clears.
    #[test]
    fn remove_missing_pool_mutation_committed_finishes_post_maintenance() {
        let f = PoolFixture::empty();
        let journal = remove_missing_journal_two_survivors();
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = with_two_disk_pool_probe(with_restore_balance_replay(MockRunner::default()));
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();

        execute_remove_missing_pool_mutation_recovery(
            &runner,
            &two_disk_pool_fs(),
            &resolver,
            &params,
            &journal,
            pool_state_two_disks(),
            Devid::new(3),
            Some(OwedRaid1Balance::RestoreSingle),
        )
        .expect("committed remove-missing should finish post maintenance");

        let requests = runner.requests();
        assert_eq!(f.inhibitor.acquire_count(), 1);
        assert!(
            requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "owed RAID1 maintenance should run"
        );
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceRemove { .. })),
            "recover must never rerun btrfs device remove"
        );
        assert!(!f.paths.pending_op_json().exists());
        let recovered = membership::load_membership(&f.paths).unwrap();
        let mut names: Vec<String> = recovered
            .names()
            .map(|name| name.as_str().to_owned())
            .collect();
        names.sort();
        assert_eq!(names, vec!["disk1".to_owned(), "disk2".to_owned()]);
    }

    // Intent: RemoveMissing::PoolMutation recovery exits recovery mode when
    // the primary remove did not commit.
    // Why it exists: recover may restore bookkeeping, but it must not retry
    // btrfs device remove behind the user's back. The live shape (disk1
    // present, devid 2 missing) is a collapse, whose convert may be paused;
    // the empty request log proves recover neither probes nor resumes it.
    // Scenario: the journal exists but btrfs still reports the same missing
    // devid, so recovery restores pre-operation pool.json and asks for rerun.
    #[test]
    fn remove_missing_pool_mutation_not_committed_restores_pre_membership() {
        let f = PoolFixture::empty();
        let journal = remove_missing_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default();
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();

        execute_remove_missing_pool_mutation_recovery(
            &runner,
            &MockFs::new(&[]),
            &MockByIdResolver::default(),
            &params,
            &journal,
            pool_state_disk1_with_missing_devid2(),
            Devid::new(2),
            Some(OwedRaid1Balance::RestoreSingle),
        )
        .expect("uncommitted remove-missing should clear journal after restoring pre state");

        assert_eq!(f.inhibitor.acquire_count(), 0);
        assert!(runner.requests().is_empty());
        assert!(!f.paths.pending_op_json().exists());
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk2")).is_some());
    }

    // Intent: after an uncommitted remove-missing, recover names the paused
    // convert's way out only when the live pool is a collapse shape.
    // Why it exists: a collapse converts before its device remove, braid
    // mounts with `skip_balance`, and the re-run refuses a paused balance, so
    // a bare "re-run" would send the operator to a refusal. A 3-disk pool's
    // remove-missing issues no pre-remove balance, so it keeps the bare text.
    // Scenario: a crash during the collapse convert of a 2-disk pool leaves
    // disk1 present and devid 2 missing; a 3-disk pool with devid 3 missing
    // crashes during its device remove.
    #[test]
    fn remove_missing_not_committed_guidance_names_paused_convert_only_on_collapse() {
        let mp = MountPoint::new("/mnt/storage".into());
        let collapse = remove_missing_not_committed_guidance(
            &pool_state_disk1_with_missing_devid2(),
            Devid::new(2),
            &mp,
        );
        assert!(
            collapse.starts_with("remove-missing did not complete -- missing devid 2"),
            "{collapse}"
        );
        assert!(
            collapse.contains("`btrfs balance resume /mnt/storage`")
                && collapse.contains("`btrfs balance cancel /mnt/storage`"),
            "{collapse}"
        );

        let three_disk = remove_missing_not_committed_guidance(
            &mounted_pool_state(&[
                (1, Some(("disk1", "/dev/vda"))),
                (2, Some(("disk2", "/dev/vdb"))),
                (3, None),
            ]),
            Devid::new(3),
            &mp,
        );
        assert!(!three_disk.contains("btrfs balance"), "{three_disk}");
        assert!(
            three_disk.ends_with("Re-run braid remove-missing to retry."),
            "{three_disk}"
        );
    }

    // Intent: RemoveMissing::PoolMutation recovery rejects mixed live state.
    // Why it exists: if live topology is neither exact pre nor exact target,
    // clearing the journal would hide an ambiguous storage state.
    // Scenario: the missing devid is gone from btrfs missing_devids, but the
    // old disk name is still live, so recovery preserves pending-op.json.
    #[test]
    fn remove_missing_pool_mutation_mixed_state_preserves_journal() {
        let f = PoolFixture::empty();
        let journal = remove_missing_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default();
        let params = f.recover_params().passphrase_file(None).build();

        let err = execute_remove_missing_pool_mutation_recovery(
            &runner,
            &MockFs::new(&[]),
            &MockByIdResolver::default(),
            &params,
            &journal,
            pool_state_two_disks(),
            Devid::new(2),
            Some(OwedRaid1Balance::RestoreSingle),
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("does not match the target membership"),
            "{err}"
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
        assert!(runner.requests().is_empty());
    }

    // Intent: RemoveMissing::PostRemoveMissingMaintenance honors the stored
    // restore_raid1_after_commit gate.
    // Why it exists: post-phase recovery should not resume or start balances
    // that the original remove-missing did not owe.
    // Scenario: committed remove-missing only needs pool.json repair and
    // journal clear because another missing device remains.
    #[test]
    fn remove_missing_post_maintenance_skips_unowed_balance() {
        let f = PoolFixture::empty();
        let mut journal = remove_missing_journal();
        journal.op = OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
            devid: Devid::new(2),
            restore_raid1_after_commit: false,
        };
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();

        execute_remove_missing_post_maintenance_recovery(
            &runner,
            &MockFs::new(&[]),
            &resolver,
            &params,
            RemoveMissingPostCtx {
                journal: &journal,
                pool: pool_state_one_disk(),
                devid: Devid::new(2),
                owed_balance: None,
                inhibitor_already_held: false,
            },
        )
        .expect("unowed post-remove maintenance should only repair state");

        assert_eq!(f.inhibitor.acquire_count(), 0);
        assert!(runner.requests().is_empty());
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent: post-maintenance inhibitor failure preserves the remove-missing
    // journal and runs no maintenance command.
    // Why it exists: recovering the committed membership is safe, but balance
    // replay must stay behind the inhibitor boundary.
    // Scenario: remove-missing has committed and owes RAID1 restore, but
    // logind refuses the inhibitor.
    #[test]
    fn remove_missing_post_maintenance_inhibitor_failure_preserves_journal() {
        let f = PoolFixture::empty();
        let mut journal = remove_missing_journal();
        journal.op = OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
            devid: Devid::new(2),
            restore_raid1_after_commit: true,
        };
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let inhibitor = FailingInhibitor;
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&inhibitor)
            .build();

        let err = execute_remove_missing_post_maintenance_recovery(
            &runner,
            &MockFs::new(&[]),
            &resolver,
            &params,
            RemoveMissingPostCtx {
                journal: &journal,
                pool: pool_state_one_disk(),
                devid: Devid::new(2),
                owed_balance: Some(OwedRaid1Balance::RestoreSingle),
                inhibitor_already_held: false,
            },
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("could not acquire sleep inhibitor"),
            "{err}"
        );
        assert!(runner.requests().is_empty());
        assert!(f.paths.pending_op_json().exists());
    }

    // Intent: Replace::PoolMutation recovery advances committed replace to
    // post-maintenance instead of restarting replace.
    // Why it exists: a finished kernel replace has already mutated btrfs
    // membership; recover only owes old-mapper close and resize.
    // Scenario: live pool has disk1+new, journal still says PoolMutation.
    #[test]
    fn replace_pool_mutation_committed_finishes_resize_without_replace_start() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[]);
        let journal = replace_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                ok_raw_empty("cryptsetup close braid-old"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: Devid::new(2),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp {
            identity,
            new_target,
            source,
            ..
        }) = &journal.op
        else {
            unreachable!("replace_journal returns Replace");
        };

        execute_replace_pool_mutation_recovery(
            &runner,
            &fs,
            &resolver,
            &params,
            None,
            &journal,
            &union,
            pool_state_disk1_and_new(),
            identity,
            &disk_name("new"),
            new_target,
            source,
            UncommittedReplaceWrites::plan(&disk_name("new"), new_target).as_ref(),
            None,
        )
        .expect("committed replace should finish post maintenance");

        let requests = runner.requests();
        assert_eq!(f.inhibitor.acquire_count(), 1);
        assert!(requests.iter().any(|r| matches!(
            r,
            CmdRequest::BtrfsFilesystemResize { devid, .. } if *devid == Devid::new(2)
        )));
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "recover must not rerun btrfs replace start"
        );
        assert!(!f.paths.pending_op_json().exists());
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("new")).is_some());
        assert!(recovered.by_name(&disk_name("old")).is_none());
    }

    // Intent: Replace::PoolMutation recovery restores pre state when replace
    // did not commit.
    // Why it exists: recovery should exit recovery mode and ask the operator
    // to rerun replace rather than starting btrfs replace itself.
    // Scenario: live pool still contains disk1+old and no new member.
    #[test]
    fn replace_pool_mutation_not_committed_restores_pre_membership() {
        let f = PoolFixture::empty();
        // ExistingLuks recovery now probes the new disk's LUKS UUID before
        // rollback (defensive: refuses to silently roll back if the wrong
        // disk is replugged or the header was zeroed). Seed the probe so
        // the disk reads as PresentLuks with the journaled UUID.
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-new"]);
        let journal = replace_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                luks_dump_label("braid-new"),
            )
            .with_mapper_closed("braid-new");
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp {
            identity,
            new_target,
            source,
            ..
        }) = &journal.op
        else {
            unreachable!("replace_journal returns Replace");
        };

        execute_replace_pool_mutation_recovery(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &params,
            None,
            &journal,
            &union,
            pool_state_disk1_and_old(),
            identity,
            &disk_name("new"),
            new_target,
            source,
            UncommittedReplaceWrites::plan(&disk_name("new"), new_target).as_ref(),
            None,
        )
        .expect("uncommitted replace should restore pre state and clear journal");

        // No-enroll rollback gates inhibitor + credential prompt on the
        // mutation phase, so neither fires when `enroll_key_file: None`.
        assert_eq!(f.inhibitor.acquire_count(), 0);
        let requests = runner.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupTestPassphrase { .. }
                    | CmdRequest::CryptsetupLuksAddKeyFile { .. }
                    | CmdRequest::CryptsetupLuksHeaderBackup { .. }
            )),
            "no-enroll rollback must not run credential or LUKS-mutation commands: {requests:?}"
        );
        assert!(!f.paths.pending_op_json().exists());
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("old")).is_some());
        assert!(recovered.by_name(&disk_name("new")).is_none());
    }

    // Intent: Replace::PoolMutation recovery rejects mixed pre/post topology.
    // Why it exists: a pool containing both old and new cannot be classified
    // safely as either uncommitted or committed.
    // Scenario: live btrfs reports disk1+old+new, so recovery preserves the
    // journal for manual inspection.
    #[test]
    fn replace_pool_mutation_mixed_state_preserves_journal() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[]);
        let journal = replace_journal();
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let runner = MockRunner::default();
        let params = f.recover_params().passphrase_file(None).build();
        let OpKind::Replace(journal::ReplaceOp {
            identity,
            new_target,
            source,
            ..
        }) = &journal.op
        else {
            unreachable!("replace_journal returns Replace");
        };

        let err = execute_replace_pool_mutation_recovery(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &params,
            None,
            &journal,
            &union,
            pool_state_disk1_old_and_new(),
            identity,
            &disk_name("new"),
            new_target,
            source,
            UncommittedReplaceWrites::plan(&disk_name("new"), new_target).as_ref(),
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("does not match either"), "{err}");
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    // Intent: Replace::PoolMutation FreshLuks recovery finishes committed
    // preparation side effects without reformatting or restarting replace.
    // Why it exists: a crash after LUKS format but before btrfs replace must
    // be recoverable by validating the prepared target, enrolling the keyfile,
    // backing up the header, and then asking the user to rerun replace.
    // Scenario: live pool is still disk1+old, while the new target is already
    // LUKS2 with the journaled label and needs keyfile enrollment.
    #[test]
    fn replace_pool_mutation_fresh_luks_expected_label_finishes_prep_only() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-new"]);
        let key_dir = tempfile::TempDir::new().unwrap();
        let key_file = write_valid_keyfile(&key_dir, "braid-new.key");
        let journal = replace_fresh_luks_journal(key_file.clone());
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                luks_dump_label("braid-new"),
            )
            .with_mapper_closed("braid-new")
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vdb".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output(
                CmdRequest::CryptsetupTestKeyFile {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    key_file_path: key_file.display().to_string(),
                },
                err_raw(
                    "cryptsetup open --test-passphrase --key-file",
                    2,
                    "No key available",
                ),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksAddKeyFile {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    key_file_path: key_file.display().to_string(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup luksAddKey"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    backup_path: f
                        .paths
                        .luks_headers_dir()
                        .join("braid-new.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                ok_raw_empty("cryptsetup luksHeaderBackup"),
            );
        let inhibitor = RequestCountInhibitor::new(runner.clone());
        let params = f.recover_params().sleep_inhibitor(&inhibitor).build();
        let OpKind::Replace(journal::ReplaceOp {
            identity,
            new_target,
            source,
            ..
        }) = &journal.op
        else {
            unreachable!("replace_fresh_luks_journal returns Replace");
        };

        execute_replace_pool_mutation_recovery(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &params,
            None,
            &journal,
            &union,
            pool_state_disk1_and_old(),
            identity,
            &disk_name("new"),
            new_target,
            source,
            UncommittedReplaceWrites::plan(&disk_name("new"), new_target).as_ref(),
            None,
        )
        .expect("fresh prepared target should be reconciled without replace start");

        let requests = runner.requests();
        assert_eq!(
            luks_dump_text_request_count(&requests, "/dev/disk/by-id/virtio-new"),
            1,
            "fresh replace recovery must use the label captured by probe_config_disk"
        );
        assert_eq!(inhibitor.acquire_count(), 1);
        let acquire_at = inhibitor
            .first_acquire_request_count()
            .expect("fresh prep reconciliation should acquire inhibitor");
        for (idx, request) in requests.iter().enumerate() {
            if matches!(request, CmdRequest::CryptsetupTestPassphrase { .. }) {
                assert!(
                    idx < acquire_at,
                    "credential verification must finish before inhibitor acquisition; \
                     acquire_at={acquire_at}, request {idx}={request:?}, requests={requests:?}"
                );
            }
        }
        let add_key = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. }))
            .expect("expected keyfile enrollment");
        let backup = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::CryptsetupLuksHeaderBackup { .. }))
            .expect("expected header backup");
        assert!(
            add_key >= acquire_at && backup >= acquire_at,
            "keyfile enrollment and header backup must be inside inhibitor window; \
             acquire_at={acquire_at}, add_key={add_key}, backup={backup}, requests={requests:?}"
        );
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksFormat { .. } | CmdRequest::BtrfsReplaceStart { .. }
            )),
            "prepared target recovery must not reformat or start replace"
        );
        assert!(!f.paths.pending_op_json().exists());
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("old")).is_some());
        assert!(recovered.by_name(&disk_name("new")).is_none());
    }

    // Intent: Replace::PoolMutation FreshLuks recovery refuses a prepared
    // target whose LUKS label does not match the journal.
    // Why it exists: the label check prevents recovery from treating an
    // unrelated LUKS device as braid's interrupted fresh target.
    // Scenario: live pool is still disk1+old, the new by-id path is LUKS2,
    // but its label is not the journaled `braid-new` label.
    #[test]
    fn replace_pool_mutation_fresh_luks_wrong_label_preserves_journal() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-new"]);
        let journal = replace_fresh_luks_journal("/run/keys/braid-new.key".into());
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                luks_dump_label("not-braid-new"),
            )
            .with_mapper_closed("braid-new");
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp {
            identity,
            new_target,
            source,
            ..
        }) = &journal.op
        else {
            unreachable!("replace_fresh_luks_journal returns Replace");
        };

        let err = execute_replace_pool_mutation_recovery(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &params,
            None,
            &journal,
            &union,
            pool_state_disk1_and_old(),
            identity,
            &disk_name("new"),
            new_target,
            source,
            UncommittedReplaceWrites::plan(&disk_name("new"), new_target).as_ref(),
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("unexpected LUKS label"), "{err}");
        assert_eq!(f.inhibitor.acquire_count(), 0);
        let requests = runner.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksFormat { .. }
                    | CmdRequest::CryptsetupLuksAddKeyFile { .. }
                    | CmdRequest::CryptsetupLuksHeaderBackup { .. }
                    | CmdRequest::BtrfsReplaceStart { .. }
                    | CmdRequest::BtrfsFilesystemResize { .. }
            )),
            "wrong-label target must stop before fresh-target side effects: {requests:?}"
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    // Intent: Replace::PoolMutation recovery refuses a missing target device,
    // fresh or existing-LUKS, names it as absent, and preserves the journal.
    // Why it exists: if the replacement disk is absent, recovery cannot prove
    // whether pre-replace preparation completed and must not rewrite pool.json.
    // Scenario: live pool is still disk1+old, but the journaled new by-id path
    // is no longer present after reboot.
    #[test]
    fn replace_pool_mutation_absent_target_preserves_journal() {
        for journal in [
            replace_fresh_luks_journal("/run/keys/braid-new.key".into()),
            replace_journal(),
        ] {
            let f = PoolFixture::empty();
            let fs = MockFs::new(&[]);
            journal::write_journal(&f.paths, &journal).unwrap();
            let union = test_recovery_admission_membership(&journal);
            let runner = MockRunner::default();
            let params = f
                .recover_params()
                .passphrase_file(None)
                .sleep_inhibitor(&f.inhibitor)
                .build();
            let OpKind::Replace(journal::ReplaceOp {
                identity,
                new_target,
                source,
                ..
            }) = &journal.op
            else {
                unreachable!("replace journal fixtures return Replace");
            };

            let err = execute_replace_pool_mutation_recovery(
                &runner,
                &fs,
                &MockByIdResolver::default(),
                &params,
                None,
                &journal,
                &union,
                pool_state_disk1_and_old(),
                identity,
                &disk_name("new"),
                new_target,
                source,
                UncommittedReplaceWrites::plan(&disk_name("new"), new_target).as_ref(),
                None,
            )
            .unwrap_err();

            assert!(
                err.to_string().contains(
                    "recover replace target 'new' (/dev/disk/by-id/virtio-new) is not present; reconnect it"
                ),
                "{err}"
            );
            assert_eq!(f.inhibitor.acquire_count(), 0);
            assert!(
                runner.requests().is_empty(),
                "absent target should fail from the filesystem probe only"
            );
            assert!(f.paths.pending_op_json().exists());
            assert!(!f.paths.pool_json().exists());
        }
    }

    // Intent: Replace::PoolMutation FreshLuks recovery rejects a bad
    // passphrase before acquiring the post-prep inhibitor.
    // Why it exists: credential verification must be complete before recovery
    // enrolls a keyfile, backs up a header, or writes pool.json.
    // Scenario: the prepared target has the expected label, but the supplied
    // passphrase opens the old pool devices and is rejected by the new target.
    #[test]
    fn replace_pool_mutation_fresh_luks_bad_passphrase_preserves_journal() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-new"]);
        let key_file = std::path::PathBuf::from("/run/keys/braid-new.key");
        let journal = replace_fresh_luks_journal(key_file);
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"wrongpass").unwrap();
        }
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                luks_dump_label("braid-new"),
            )
            .with_mapper_closed("braid-new")
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                b"wrongpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vdb".into(),
                },
                b"wrongpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                b"wrongpass".to_vec(),
                err_raw("cryptsetup open --test-passphrase", 2, "No key available"),
            );
        let inhibitor = RequestCountInhibitor::new(runner.clone());
        let params = f
            .recover_params()
            .passphrase_file(Some(passphrase_file.path()))
            .sleep_inhibitor(&inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp {
            identity,
            new_target,
            source,
            ..
        }) = &journal.op
        else {
            unreachable!("replace_fresh_luks_journal returns Replace");
        };

        let err = execute_replace_pool_mutation_recovery(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &params,
            None,
            &journal,
            &union,
            pool_state_disk1_and_old(),
            identity,
            &disk_name("new"),
            new_target,
            source,
            UncommittedReplaceWrites::plan(&disk_name("new"), new_target).as_ref(),
            None,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("recover replace passphrase was rejected by 'new'"),
            "{err}"
        );
        assert_eq!(inhibitor.acquire_count(), 0);
        assert!(inhibitor.first_acquire_request_count().is_none());
        let requests = runner.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksAddKeyFile { .. }
                    | CmdRequest::CryptsetupLuksHeaderBackup { .. }
                    | CmdRequest::BtrfsReplaceStart { .. }
                    | CmdRequest::BtrfsFilesystemResize { .. }
            )),
            "bad credential must stop before fresh-target side effects: {requests:?}"
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    // Intent: Replace::PoolMutation FreshLuks recovery preserves the journal
    // if header backup fails after credential verification.
    // Why it exists: recovery must not clear the journal or write pool.json
    // until all fresh-target preparation side effects are complete.
    // Scenario: the prepared target has the expected label and credential,
    // but `cryptsetup luksHeaderBackup` fails.
    #[test]
    fn replace_pool_mutation_fresh_luks_header_backup_failure_preserves_journal() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-new"]);
        let key_dir = tempfile::TempDir::new().unwrap();
        let key_file = write_valid_keyfile(&key_dir, "braid-new.key");
        let journal = replace_fresh_luks_journal(key_file.clone());
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                luks_dump_label("braid-new"),
            )
            .with_mapper_closed("braid-new")
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vdb".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output(
                CmdRequest::CryptsetupTestKeyFile {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    key_file_path: key_file.display().to_string(),
                },
                ok_raw_empty("cryptsetup open --test-passphrase --key-file"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    backup_path: f
                        .paths
                        .luks_headers_dir()
                        .join("braid-new.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                err_raw("cryptsetup luksHeaderBackup", 1, "backup failed"),
            );
        let params = f.recover_params().sleep_inhibitor(&f.inhibitor).build();
        let OpKind::Replace(journal::ReplaceOp {
            identity,
            new_target,
            source,
            ..
        }) = &journal.op
        else {
            unreachable!("replace_fresh_luks_journal returns Replace");
        };

        let err = execute_replace_pool_mutation_recovery(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &params,
            None,
            &journal,
            &union,
            pool_state_disk1_and_old(),
            identity,
            &disk_name("new"),
            new_target,
            source,
            UncommittedReplaceWrites::plan(&disk_name("new"), new_target).as_ref(),
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("backup failed"), "{err}");
        assert_eq!(f.inhibitor.acquire_count(), 1);
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "fresh-prep recovery must not start btrfs replace"
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    // Intent: ExistingLuks recovery with `enroll_key_file: Some` aborts
    //   when the live LUKS UUID does not match the journaled one, and
    //   preserves the journal (does NOT clear pending-op.json).
    // Why it exists: this is the silent-drop bug fix's defensive guard
    //   in recovery -- before the refactor, ExistingLuks recovery had no
    //   identity probe and would silently roll back even if the user
    //   had swapped the disk between crash and recover. Pinning that a
    //   UUID mismatch preserves the journal blocks a regression that
    //   would let recovery proceed (potentially trashing a different
    //   pool's pre-replace topology). The "preserving pending-op.json"
    //   wording is the operator-facing signal.
    // Scenario: braid replace --enroll DIR crashes between journal
    //   write and pool mutation; before recover runs, the operator
    //   replugs a different pre-formatted LUKS disk into the same
    //   slot.
    #[test]
    fn replace_pool_mutation_existing_luks_with_enroll_uuid_mismatch_preserves_journal() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-new"]);
        let key_dir = tempfile::TempDir::new().unwrap();
        let key_file = write_valid_keyfile(&key_dir, "braid-new.key");
        let journal = replace_existing_luks_with_enroll_journal(key_file.clone());
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        // probe returns a DIFFERENT UUID than the journaled one.
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "55555555-5555-5555-5555-555555555555",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                luks_dump_label("braid-new"),
            )
            .with_mapper_closed("braid-new");
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp {
            identity,
            new_target,
            source,
            ..
        }) = &journal.op
        else {
            unreachable!("returns Replace");
        };

        let err = execute_replace_pool_mutation_recovery(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &params,
            None,
            &journal,
            &union,
            pool_state_disk1_and_old(),
            identity,
            &disk_name("new"),
            new_target,
            source,
            UncommittedReplaceWrites::plan(&disk_name("new"), new_target).as_ref(),
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("LUKS UUID mismatch"), "{err}");
        assert!(
            err.to_string().contains("preserving pending-op.json"),
            "{err}"
        );
        assert_eq!(f.inhibitor.acquire_count(), 0);
        let requests = runner.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksAddKeyFile { .. }
                    | CmdRequest::CryptsetupLuksHeaderBackup { .. }
                    | CmdRequest::CryptsetupTestPassphrase { .. }
            )),
            "uuid mismatch must abort before any LUKS mutation or credential prompt: {requests:?}"
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    // Intent: ExistingLuks recovery with `enroll_key_file: Some` aborts
    //   on bad passphrase before any LUKS mutation, and preserves the
    //   journal.
    // Why it exists: ensures the credential discipline matches the
    //   FreshLuks recovery arm -- wrong passphrase must NOT proceed to
    //   `cryptsetup luksAddKey` or header backup. Preserves the
    //   single-passphrase invariant.
    // Scenario: operator types the wrong passphrase during recovery of
    //   an interrupted `replace --enroll DIR` against a `PresentLuks`
    //   target.
    #[test]
    fn replace_pool_mutation_existing_luks_with_enroll_bad_passphrase_preserves_journal() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-new"]);
        let key_dir = tempfile::TempDir::new().unwrap();
        let key_file = write_valid_keyfile(&key_dir, "braid-new.key");
        let journal = replace_existing_luks_with_enroll_journal(key_file.clone());
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"wrongpass").unwrap();
        }
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                luks_dump_label("braid-new"),
            )
            .with_mapper_closed("braid-new")
            // Existing pool members verify OK; new disk rejects the
            // wrong passphrase. The verifier walks targets in order,
            // so seeding the pool members keeps the failure on the new
            // disk specifically.
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                b"wrongpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vdb".into(),
                },
                b"wrongpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                b"wrongpass".to_vec(),
                err_raw(
                    "cryptsetup open --test-passphrase",
                    2,
                    "No key available with this passphrase.",
                ),
            );
        let params = f
            .recover_params()
            .passphrase_file(Some(passphrase_file.path()))
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp {
            identity,
            new_target,
            source,
            ..
        }) = &journal.op
        else {
            unreachable!("returns Replace");
        };

        let err = execute_replace_pool_mutation_recovery(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &params,
            None,
            &journal,
            &union,
            pool_state_disk1_and_old(),
            identity,
            &disk_name("new"),
            new_target,
            source,
            UncommittedReplaceWrites::plan(&disk_name("new"), new_target).as_ref(),
            None,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("recover replace passphrase was rejected by 'new'"),
            "{err}"
        );
        assert_eq!(f.inhibitor.acquire_count(), 0);
        let requests = runner.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksAddKeyFile { .. }
                    | CmdRequest::CryptsetupLuksHeaderBackup { .. }
            )),
            "wrong passphrase must stop before any LUKS mutation: {requests:?}"
        );
        assert!(f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
    }

    // Intent: ExistingLuks recovery with `enroll_key_file: Some` replays
    //   `cryptsetup luksAddKey` + `luksHeaderBackup` after the identity
    //   probe + credential check, then writes pre-replace pool.json and
    //   clears the journal.
    // Why it exists: the silent-drop bug fix landing in recovery. Pre-
    //   refactor, recovery never ran the addKey replay -- a crash mid-
    //   replace meant the user's `--enroll DIR` was lost forever. This
    //   test pins the happy-path behavior end-to-end so a regression
    //   that drops the addKey/backup step is caught immediately.
    // Scenario: `braid replace --enroll DIR` against a PresentLuks
    //   target crashes between journal write and pool_replace_start;
    //   `braid recover` resumes, identity probe passes, credential
    //   verifies, slot 1 is enrolled, header backup captures it, and
    //   the journal clears.
    #[test]
    fn replace_pool_mutation_existing_luks_with_enroll_replays_keyfile_then_clears_journal() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-new"]);
        let key_dir = tempfile::TempDir::new().unwrap();
        let key_file = write_valid_keyfile(&key_dir, "braid-new.key");
        let journal = replace_existing_luks_with_enroll_journal(key_file.clone());
        journal::write_journal(&f.paths, &journal).unwrap();
        let union = test_recovery_admission_membership(&journal);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                luks_dump_label("braid-new"),
            )
            .with_mapper_closed("braid-new")
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vdb".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            // ensure_keyfile_enrolled probes the keyfile first and
            // skips luksAddKey if it already authenticates. We let it
            // probe (Rejected) and then run the real enroll command.
            .with_output(
                CmdRequest::CryptsetupTestKeyFile {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    key_file_path: key_file.display().to_string(),
                },
                err_raw("cryptsetup open --test-passphrase --key-file", 2, "No key"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksAddKeyFile {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    key_file_path: key_file.display().to_string(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup luksAddKey"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    backup_path: f
                        .paths
                        .luks_headers_dir()
                        .join("braid-new.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                ok_raw_empty("cryptsetup luksHeaderBackup"),
            );
        let params = f.recover_params().sleep_inhibitor(&f.inhibitor).build();
        let OpKind::Replace(journal::ReplaceOp {
            identity,
            new_target,
            source,
            ..
        }) = &journal.op
        else {
            unreachable!("returns Replace");
        };

        execute_replace_pool_mutation_recovery(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &params,
            None,
            &journal,
            &union,
            pool_state_disk1_and_old(),
            identity,
            &disk_name("new"),
            new_target,
            source,
            UncommittedReplaceWrites::plan(&disk_name("new"), new_target).as_ref(),
            None,
        )
        .expect("happy path replays enrollment + clears journal");

        assert_eq!(f.inhibitor.acquire_count(), 1);
        let requests = runner.requests();
        assert!(
            requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksAddKeyFile { device, .. }
                    if device == "/dev/disk/by-id/virtio-new"
            )),
            "happy path must replay luksAddKey: {requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksHeaderBackup { .. })),
            "happy path must back up the header after the addKey: {requests:?}"
        );
        assert!(!f.paths.pending_op_json().exists());
        let recovered = membership::load_membership(&f.paths).unwrap();
        // Replay re-establishes pre-replace topology (the op didn't
        // commit, so we roll back to {disk1, old}).
        assert!(recovered.by_name(&disk_name("old")).is_some());
        assert!(recovered.by_name(&disk_name("new")).is_none());
    }

    // Intent: Replace::PostReplaceMaintenance skips unowed balance work.
    // Why it exists: recovery should not resume an unrelated paused balance
    // when restore_raid1_after_commit is false.
    // Scenario: replace committed and only resize remains.
    #[test]
    fn replace_post_maintenance_skips_unowed_balance() {
        let f = PoolFixture::empty();
        let journal = replace_post_maintenance_journal(
            false,
            journal::ReplaceJournalSource::Missing {
                old_devid: Devid::new(2),
            },
        );
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemResize {
                devid: Devid::new(2),
                mount_point: MountPoint::new("/mnt/storage".into()),
            },
            ok_raw_empty("btrfs filesystem resize"),
        );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp { source, .. }) = &journal.op else {
            unreachable!("replace_journal_in_phase returns Replace");
        };

        execute_replace_post_maintenance_recovery(
            &runner,
            &MockFs::new(&[]),
            &progress::NoopSleeper,
            &resolver,
            &params,
            &journal,
            pool_state_disk1_and_new(),
            &uuid_for_name("new"),
            &disk_name("new"),
            source,
            None,
            false,
        )
        .expect("post-replace maintenance should resize and clear");

        let requests = runner.requests();
        assert_eq!(f.inhibitor.acquire_count(), 1);
        assert!(requests.iter().any(|r| matches!(
            r,
            CmdRequest::BtrfsFilesystemResize { devid, .. } if *devid == Devid::new(2)
        )));
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsBalanceStatus { .. } | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "restore_raid1_after_commit=false must skip balance probes and replay"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    /// Runs `execute_replace_post_maintenance_recovery` for a committed
    /// missing-path replace of devid 2, with only the resize mocked.
    fn run_replace_post_maintenance_for_devid2(f: &PoolFixture) -> Result<(), RecoverError> {
        let journal = replace_post_maintenance_journal(
            false,
            journal::ReplaceJournalSource::Missing {
                old_devid: Devid::new(2),
            },
        );
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemResize {
                devid: Devid::new(2),
                mount_point: MountPoint::new("/mnt/storage".into()),
            },
            ok_raw_empty("btrfs filesystem resize"),
        );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp { source, .. }) = &journal.op else {
            unreachable!("replace_journal_in_phase returns Replace");
        };
        execute_replace_post_maintenance_recovery(
            &runner,
            &MockFs::new(&[]),
            &progress::NoopSleeper,
            &resolver,
            &params,
            &journal,
            pool_state_disk1_and_new(),
            &uuid_for_name("new"),
            &disk_name("new"),
            source,
            None,
            false,
        )
    }

    // Intent: replace post-maintenance recovery forgets the old disk's alert
    //   state on the devid the new disk inherited: its acked baseline is
    //   dropped and its latched error counts are stripped.
    // Why it exists: live replace drops that state after writing the
    //   post-replace journal. A crash before the drop would otherwise leave the
    //   old disk's baseline and latched counts for the new disk to inherit.
    // Scenario: the old devid-2 disk had an acked baseline and latched error
    //   counts; replace committed and crashed before its cleanup; the operator
    //   runs `braid recover`.
    #[test]
    fn replace_post_maintenance_recovery_drops_replaced_devid_alert_state() {
        let f = PoolFixture::empty();
        let counts = alert::DeviceErrorCounters {
            read_io_errs: 4,
            ..Default::default()
        };
        alert::save_acked_stats(
            &alert::AckedStats(std::collections::BTreeMap::from([(
                "2".to_owned(),
                alert::AckedDisk {
                    missing_acked: false,
                    device_stats: counts,
                },
            )])),
            &f.paths,
        )
        .unwrap();
        alert::save_alert_latch(
            &alert::AlertState {
                causes: vec![alert::LatchedCause::new(
                    alert::AlertCause::BtrfsDeviceErrors {
                        devid: Devid::new(2),
                        counters: Some(counts),
                    },
                    "2023-11-14T22:13:20Z".to_owned(),
                )],
            },
            &f.paths,
        )
        .unwrap();

        run_replace_post_maintenance_for_devid2(&f).expect("recovery should finish");

        assert!(
            alert::load_acked_stats_fallible(&f.paths)
                .unwrap()
                .0
                .is_empty(),
            "recovery must drop the replaced devid's baseline"
        );
        let latch = alert::load_alert_latch(&f.paths).unwrap().unwrap();
        assert_eq!(
            latch.causes[0].cause,
            alert::AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(2),
                counters: None,
            },
            "recovery must strip the old disk's latched counts"
        );
    }

    // Intent: replace post-maintenance recovery fails closed when it cannot
    //   drop the old disk's alert state, and keeps pending-op.json.
    // Why it exists: carrying on would leave the new disk compared against,
    //   or acked with, the old disk's counts. Keeping the journal lets a later
    //   `braid recover` retry the drop.
    // Scenario: acked-stats.json holds non-JSON bytes when recovery replays
    //   the post-replace maintenance.
    #[test]
    fn replace_post_maintenance_recovery_alert_cleanup_failure_preserves_journal() {
        let f = PoolFixture::empty();
        std::fs::write(f.paths.acked_stats_json(), b"corrupt").unwrap();

        let err = run_replace_post_maintenance_for_devid2(&f).unwrap_err();

        assert!(
            matches!(
                err,
                RecoverError::AckCleanupFailed {
                    stage: "replace recovery",
                    ..
                }
            ),
            "expected replace-recovery AckCleanupFailed, got {err:?}"
        );
        assert!(f.paths.pending_op_json().exists());
        assert_eq!(
            std::fs::read(f.paths.acked_stats_json()).unwrap(),
            b"corrupt"
        );
    }

    // Intent: recover's replace post-maintenance path routes the old-mapper
    // best-effort close through the retry helper when cryptsetup reports busy.
    // Why it exists: recovery replays the same post-commit close as live
    // replace, so it must not regress to a single-shot close and leak the old
    // mapper on transient EBUSY.
    // Scenario: replace already committed, recovery sees braid-old still
    // present, the first close is busy, and the second close succeeds before
    // resize.
    #[test]
    fn recover_replace_old_close_retries_on_busy_then_succeeds() {
        let f = PoolFixture::empty();
        let journal = replace_post_maintenance_journal(
            false,
            journal::ReplaceJournalSource::Live {
                old_devid: Devid::new(2),
                old_mapper: MapperName::from_basename("braid-old".into()),
            },
        );
        journal::write_journal(&f.paths, &journal).unwrap();
        let close_attempts = Arc::new(AtomicU32::new(0));
        let runner = MockRunner::default()
            .with_handler({
                let close_attempts = close_attempts.clone();
                move |req| match req {
                    CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-old" => {
                        let attempt = close_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                        if attempt == 1 {
                            Some(Ok(err_raw(
                                "cryptsetup close braid-old",
                                5,
                                "device is busy",
                            )))
                        } else {
                            Some(Ok(ok_raw_empty("cryptsetup close braid-old")))
                        }
                    }
                    _ => None,
                }
            })
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                cryptsetup_status_active("braid-old", "/dev/disk/by-id/virtio-old"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-old",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: Devid::new(2),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp { source, .. }) = &journal.op else {
            unreachable!("replace_journal_in_phase returns Replace");
        };

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            execute_replace_post_maintenance_recovery(
                &runner,
                &MockFs::new(&[]).with_luks_dm("braid-old", "22222222-2222-2222-2222-222222222222"),
                &progress::NoopSleeper,
                &resolver,
                &params,
                &journal,
                pool_state_disk1_and_new(),
                &uuid_for_name("new"),
                &disk_name("new"),
                source,
                None,
                false,
            )
            .expect("post-replace maintenance should close after retry and resize");
        });

        let close_count = runner
            .requests()
            .iter()
            .filter(|request| {
                matches!(request, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-old")
            })
            .count();
        assert_eq!(close_count, 2);
        assert!(
            captured.contains("[ok]   disk old: locked"),
            "missing terminal ok row after retry: {captured:?}"
        );
    }

    // Intent: recover labels the replace old-mapper close trailer with the
    //   journaled operator name even when the observed mapper has drifted.
    // Why it exists: recovery mirrors live replace's post-commit close; the
    //   target remains the observed mapper, but display must join through the
    //   journaled DiskName instead of stripping the mapper basename.
    // Scenario: a replace journal records old disk2, recovery finds its old
    //   mapper active as braid-WRONG with the expected UUID, closes it, and
    //   finishes post-maintenance.
    #[test]
    fn recover_replace_old_close_labels_drifted_mapper_with_disk_name() {
        let f = PoolFixture::empty();
        let mut journal = replace_post_maintenance_journal(
            false,
            journal::ReplaceJournalSource::Live {
                old_devid: Devid::new(2),
                old_mapper: MapperName::from_basename("braid-WRONG".into()),
            },
        );
        let old_uuid = uuid_for_name("disk2");
        journal
            .pre_membership
            .remove_by_uuid(&old_uuid)
            .expect("replace fixture has old disk member");
        journal
            .pre_membership
            .insert(
                old_uuid.clone(),
                DiskMember {
                    name: disk_name("disk2"),
                    by_id: by_id_path("/dev/disk/by-id/virtio-disk2"),
                    devid: Some(Devid::new(2)),
                    added_at: None,
                },
            )
            .expect("insert renamed old disk member");
        let OpKind::Replace(journal::ReplaceOp { old_name, .. }) = &mut journal.op else {
            unreachable!("replace_post_maintenance_journal returns Replace");
        };
        *old_name = disk_name("disk2");
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-WRONG".into()),
                },
                cryptsetup_status_active("braid-WRONG", "/dev/disk/by-id/virtio-disk2"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-WRONG".into()),
                },
                ok_raw_empty("cryptsetup close braid-WRONG"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: Devid::new(2),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp { source, .. }) = &journal.op else {
            unreachable!("replace_post_maintenance_journal returns Replace");
        };

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            execute_replace_post_maintenance_recovery(
                &runner,
                &MockFs::new(&[])
                    .with_luks_dm("braid-WRONG", "22222222-2222-2222-2222-222222222222"),
                &progress::NoopSleeper,
                &resolver,
                &params,
                &journal,
                pool_state_disk1_and_new(),
                &uuid_for_name("new"),
                &disk_name("new"),
                source,
                None,
                false,
            )
            .expect("drifted old mapper should close and finish recovery");
        });

        let close_count = runner
            .requests()
            .iter()
            .filter(|request| {
                matches!(request, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-WRONG")
            })
            .count();
        assert_eq!(close_count, 1, "observed drifted mapper must close");
        let wait = "[wait] disk disk2: locking...";
        let ok = "[ok]   disk disk2: locked";
        assert!(captured.contains(wait), "missing wait row: {captured:?}");
        assert!(captured.contains(ok), "missing ok row: {captured:?}");
        assert!(
            captured.find(wait) < captured.find(ok),
            "wait must precede ok, got: {captured:?}"
        );
        assert!(
            !captured.contains("WRONG"),
            "recover close trailer must not echo drifted mapper basename: {captured:?}"
        );
    }

    // Intent: direct post-maintenance replace recovery treats an inactive old
    //   mapper as an already-closed no-op.
    // Why it exists: recovery replays after crashes that may happen after the
    //   live close but before journal clear; that path must stay silent while
    //   still finishing resize and journal cleanup.
    // Scenario: replace committed, braid-old is already closed, and recovery
    //   re-runs PostReplaceMaintenance directly without a remount cycle.
    #[test]
    fn recover_replace_old_close_inactive_silently_skips() {
        let f = PoolFixture::empty();
        let journal = replace_post_maintenance_journal(
            false,
            journal::ReplaceJournalSource::Live {
                old_devid: Devid::new(2),
                old_mapper: MapperName::from_basename("braid-old".into()),
            },
        );
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                inactive_mapper_status("braid-old"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: Devid::new(2),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp { source, .. }) = &journal.op else {
            unreachable!("replace_journal_in_phase returns Replace");
        };

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            execute_replace_post_maintenance_recovery(
                &runner,
                &MockFs::new(&[]),
                &progress::NoopSleeper,
                &resolver,
                &params,
                &journal,
                pool_state_disk1_and_new(),
                &uuid_for_name("new"),
                &disk_name("new"),
                source,
                None,
                false,
            )
            .expect("inactive old mapper should skip close and finish recovery");
        });

        let requests = runner.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-old"
            )),
            "already-closed old mapper must not be closed again: {requests:?}"
        );
        assert!(
            requests.iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsFilesystemResize { devid, .. } if *devid == Devid::new(2)
            )),
            "resize must still replay after inactive close skip: {requests:?}"
        );
        assert!(
            !captured.contains("Warning: post-commit close skipped"),
            "recovery inactive skip must stay silent: {captured:?}"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent: direct post-maintenance replace recovery skips closing a foreign
    //   disk that appears under the old mapper name.
    // Why it exists: recovery must mirror live replace's UUID authority so an
    //   operator-opened foreign dm slot is not torn down during replay.
    // Scenario: replace committed, but before recovery runs a foreign disk is
    //   opened as braid-old; recovery warns, skips close, resizes, and clears.
    #[test]
    fn recover_replace_old_close_foreign_mapper_warns_and_skips() {
        let f = PoolFixture::empty();
        let journal = replace_post_maintenance_journal(
            false,
            journal::ReplaceJournalSource::Live {
                old_devid: Devid::new(2),
                old_mapper: MapperName::from_basename("braid-old".into()),
            },
        );
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                cryptsetup_status_active("braid-old", "/dev/vdf"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdf".into(),
                },
                cryptsetup_uuid_ok("/dev/vdf", "99999999-9999-9999-9999-999999999999"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: Devid::new(2),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp { source, .. }) = &journal.op else {
            unreachable!("replace_journal_in_phase returns Replace");
        };

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            execute_replace_post_maintenance_recovery(
                &runner,
                &MockFs::new(&[]).with_luks_dm("braid-old", "99999999-9999-9999-9999-999999999999"),
                &progress::NoopSleeper,
                &resolver,
                &params,
                &journal,
                pool_state_disk1_and_new(),
                &uuid_for_name("new"),
                &disk_name("new"),
                source,
                None,
                false,
            )
            .expect("foreign old mapper should skip close and finish recovery");
        });

        let requests = runner.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-old"
            )),
            "foreign old mapper must not be closed: {requests:?}"
        );
        assert!(
            requests.iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsFilesystemResize { devid, .. } if *devid == Devid::new(2)
            )),
            "resize must still replay after foreign close skip: {requests:?}"
        );
        assert!(
            captured.contains(
                "Warning: post-commit close skipped for mapper braid-old: \
                 expected LUKS UUID 22222222-2222-2222-2222-222222222222 \
                 but observed 99999999-9999-9999-9999-999999999999\n"
            ),
            "foreign close skip must warn with both UUIDs: {captured:?}"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent: direct post-maintenance replace recovery closes an owned active
    //   mapper even when the `/dev/mapper` path node is absent.
    // Why it exists: dm/cryptsetup status is the close authority; path
    //   existence is only a weaker observable and must not gate recovery.
    // Scenario: replace committed, braid-old is active by dm name with the
    //   expected old UUID, but `/dev/mapper/braid-old` is missing.
    #[test]
    fn recover_replace_old_close_owned_mapper_without_path_node_closes() {
        let f = PoolFixture::empty();
        let journal = replace_post_maintenance_journal(
            false,
            journal::ReplaceJournalSource::Live {
                old_devid: Devid::new(2),
                old_mapper: MapperName::from_basename("braid-old".into()),
            },
        );
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                cryptsetup_status_active("braid-old", "/dev/disk/by-id/virtio-old"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-old",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                ok_raw_empty("cryptsetup close braid-old"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: Devid::new(2),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp { source, .. }) = &journal.op else {
            unreachable!("replace_journal_in_phase returns Replace");
        };

        execute_replace_post_maintenance_recovery(
            &runner,
            &MockFs::new(&[]).with_luks_dm("braid-old", "22222222-2222-2222-2222-222222222222"),
            &progress::NoopSleeper,
            &resolver,
            &params,
            &journal,
            pool_state_disk1_and_new(),
            &uuid_for_name("new"),
            &disk_name("new"),
            source,
            None,
            false,
        )
        .expect("owned old mapper should close even without a path node");

        let close_count = runner
            .requests()
            .iter()
            .filter(|request| {
                matches!(request, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-old")
            })
            .count();
        assert_eq!(close_count, 1, "owned active old mapper must close");
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent: Replace::PostReplaceMaintenance runs owed RAID1 maintenance
    // when the journal says the committed replace cleared the last missing
    // device.
    // Why it exists: the restore_raid1_after_commit gate must be positive as
    // well as negative; otherwise post-replace recovery could strand
    // single-profile chunks after a missing-device replacement.
    // Scenario: replace committed, resize succeeds, no balance is paused, and
    // recovery replays the soft RAID1 balance before clearing the journal.
    #[test]
    fn replace_post_maintenance_runs_owed_balance() {
        let f = PoolFixture::empty();
        let journal = replace_post_maintenance_journal(
            true,
            journal::ReplaceJournalSource::Missing {
                old_devid: Devid::new(2),
            },
        );
        journal::write_journal(&f.paths, &journal).unwrap();
        // The replay re-probes the committed pool: disk1 plus new.
        let runner = with_restore_balance_replay(MockRunner::default())
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: Devid::new(2),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_disk1_and_new(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-new".into()),
                },
                cryptsetup_status_active("braid-new", "/dev/vdc"),
            );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&f.inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp { source, .. }) = &journal.op else {
            unreachable!("replace_journal_in_phase returns Replace");
        };

        execute_replace_post_maintenance_recovery(
            &runner,
            &MockFs::new(&[])
                .with_luks_dm("braid-disk1", uuid_for_name("disk1").as_str())
                .with_luks_dm("braid-new", uuid_for_name("new").as_str()),
            &progress::NoopSleeper,
            &resolver,
            &params,
            &journal,
            pool_state_disk1_and_new(),
            &uuid_for_name("new"),
            &disk_name("new"),
            source,
            Some(OwedRaid1Balance::RestoreSingle),
            false,
        )
        .expect("post-replace maintenance should resize, balance, and clear");

        let requests = runner.requests();
        assert_eq!(f.inhibitor.acquire_count(), 1);
        assert!(
            requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "owed RAID1 maintenance should run"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent: cmd_recover preserves a non-target null-underlying disk during
    // Replace::PostReplaceMaintenance while still replaying the resize for
    // the live replacement disk.
    // Why it exists: replace has path-specific work after
    // recover_membership_matching_expected; this pins both the widened helper
    // fallback and the post-helper new_uuid lookup in pool.devices.
    // Scenario: replace committed old -> disk-new, then unrelated disk3's
    // underlying block device hot-unplugged before recovery rebuilt pool.json.
    #[test]
    fn cmd_recover_replace_post_maintenance_preserves_non_target_null_underlying_disk() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk-new", uuid_for_name("disk-new").as_str())
            .with_luks_dm("braid-disk3", uuid_for_name("disk3").as_str());
        let new_uuid = uuid_for_name("disk-new");
        let new_uuid_text = new_uuid.to_string();
        let pre = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "old",
                "/dev/disk/by-id/virtio-old",
                None,
                Some(Devid::new(2)),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                None,
                Some(Devid::new(3)),
            ),
        ]);
        let target = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            (
                new_uuid.clone(),
                disk_member_named(
                    "disk-new",
                    "/dev/disk/by-id/virtio-disk-new",
                    None,
                    Some(Devid::new(2)),
                ),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                None,
                Some(Devid::new(3)),
            ),
        ]);
        let journal = journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Replace(journal::ReplaceOp {
                phase: journal::ReplacePhase::PostReplaceMaintenance,
                identity: journal::ReplaceIdentity::new(uuid_for_name("old"), new_uuid.clone())
                    .expect("distinct replace UUIDs"),
                old_name: disk_name("old"),
                new_name: disk_name("disk-new"),
                new_target: journal::ReplaceJournalTarget {
                    by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk-new").unwrap(),
                    mode: journal::ReplaceJournalMode::ExistingLuks {
                        enroll_key_file: None,
                    },
                },
                source: journal::ReplaceJournalSource::Missing {
                    old_devid: Devid::new(2),
                },
                restore_raid1_after_commit: false,
            }),
            pre_membership: pre,
            target_membership: target,
        };
        journal::write_journal(&f.paths, &journal).unwrap();

        let show = ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk-new\n\
             \tdevid    3 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk3\n",
        );
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                show,
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk-new".into()),
                },
                cryptsetup_status_active("braid-disk-new", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", &new_uuid_text),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk3".into()),
                },
                cryptsetup_status_active("braid-disk3", "(null)"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: Devid::new(2),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            );
        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk-new"),
        ]);
        let params = f.recover_params().passphrase_file(None).build();

        let result = cmd_recover(&runner, &fs, &resolver, &params);
        result.expect("recover should preserve null-underlying disk3 and resize disk-new");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(recovered.by_name(&disk_name("disk-new")).is_some());
        assert!(recovered.by_name(&disk_name("old")).is_none());
        assert!(
            recovered.by_name(&disk_name("disk3")).is_some(),
            "non-target null-underlying disk3 must be preserved after replace commits"
        );
        let requests = runner.requests();
        assert!(
            requests.iter().any(|request| {
                matches!(
                    request,
                    CmdRequest::BtrfsFilesystemResize { devid, .. } if *devid == Devid::new(2)
                )
            }),
            "post-replace recovery must resize disk-new's live devid: {requests:?}"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent: cmd_recover preserves a non-target MISSING-devid disk during
    // Replace::PostReplaceMaintenance while still replaying the resize for the
    // live replacement disk.
    // Why it exists: the MISSING-devid binding is already unit-pinned by
    // recover_membership_matching_expected_reinserts_missing_devid_member; this
    // test pins the composed command-boundary path -- post-maintenance
    // dispatcher, the recover_membership_matching_expected call that both
    // checks the topology and builds the membership, and the pool.json
    // write -- against a btrfs MISSING devid. It mirrors the
    // null-underlying analog cmd_recover_replace_post_maintenance_preserves_non_target_null_underlying_disk.
    // Scenario: replace committed old -> disk-new, then unrelated disk3 went
    // MISSING (flapping disk) before recovery rebuilt pool.json.
    #[test]
    fn cmd_recover_replace_post_maintenance_preserves_non_target_missing_disk() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk-new", uuid_for_name("disk-new").as_str());
        let new_uuid = uuid_for_name("disk-new");
        let new_uuid_text = new_uuid.to_string();
        let pre = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "old",
                "/dev/disk/by-id/virtio-old",
                None,
                Some(Devid::new(2)),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                None,
                Some(Devid::new(3)),
            ),
        ]);
        let target = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            (
                new_uuid.clone(),
                disk_member_named(
                    "disk-new",
                    "/dev/disk/by-id/virtio-disk-new",
                    None,
                    Some(Devid::new(2)),
                ),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                None,
                Some(Devid::new(3)),
            ),
        ]);
        let journal = journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Replace(journal::ReplaceOp {
                phase: journal::ReplacePhase::PostReplaceMaintenance,
                identity: journal::ReplaceIdentity::new(uuid_for_name("old"), new_uuid.clone())
                    .expect("distinct replace UUIDs"),
                old_name: disk_name("old"),
                new_name: disk_name("disk-new"),
                new_target: journal::ReplaceJournalTarget {
                    by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk-new").unwrap(),
                    mode: journal::ReplaceJournalMode::ExistingLuks {
                        enroll_key_file: None,
                    },
                },
                source: journal::ReplaceJournalSource::Missing {
                    old_devid: Devid::new(2),
                },
                restore_raid1_after_commit: false,
            }),
            pre_membership: pre,
            target_membership: target,
        };
        journal::write_journal(&f.paths, &journal).unwrap();

        let show = ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk-new\n\
             \tdevid    3 size 0 used 0 path MISSING\n",
        );
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                show,
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk-new".into()),
                },
                cryptsetup_status_active("braid-disk-new", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", &new_uuid_text),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: Devid::new(2),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            );
        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk-new"),
        ]);
        let params = f.recover_params().passphrase_file(None).build();

        let result = cmd_recover(&runner, &fs, &resolver, &params);
        result.expect("recover should preserve MISSING disk3 and resize disk-new");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(recovered.by_name(&disk_name("disk-new")).is_some());
        assert!(recovered.by_name(&disk_name("old")).is_none());
        assert!(
            recovered.by_name(&disk_name("disk3")).is_some(),
            "non-target MISSING disk3 must be preserved after replace commits"
        );
        let requests = runner.requests();
        assert!(
            requests.iter().any(|request| {
                matches!(
                    request,
                    CmdRequest::BtrfsFilesystemResize { devid, .. } if *devid == Devid::new(2)
                )
            }),
            "post-replace recovery must resize disk-new's live devid: {requests:?}"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent: post-maintenance inhibitor failure preserves the replace
    // journal and runs no maintenance command.
    // Why it exists: close, resize, and balance are all post-commit
    // maintenance and must stay behind the inhibitor boundary.
    // Scenario: replace committed, but logind refuses the recovery inhibitor.
    #[test]
    fn replace_post_maintenance_inhibitor_failure_preserves_journal() {
        let f = PoolFixture::empty();
        let journal = replace_post_maintenance_journal(
            true,
            journal::ReplaceJournalSource::Live {
                old_devid: Devid::new(2),
                old_mapper: MapperName::from_basename("braid-old".into()),
            },
        );
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = MockRunner::default();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let inhibitor = FailingInhibitor;
        let params = f
            .recover_params()
            .passphrase_file(None)
            .sleep_inhibitor(&inhibitor)
            .build();
        let OpKind::Replace(journal::ReplaceOp { source, .. }) = &journal.op else {
            unreachable!("replace_journal_in_phase returns Replace");
        };

        let err = execute_replace_post_maintenance_recovery(
            &runner,
            &MockFs::new(&[]),
            &progress::NoopSleeper,
            &resolver,
            &params,
            &journal,
            pool_state_disk1_and_new(),
            &uuid_for_name("new"),
            &disk_name("new"),
            source,
            Some(OwedRaid1Balance::RestoreSingle),
            false,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("could not acquire sleep inhibitor"),
            "{err}"
        );
        assert!(runner.requests().is_empty());
        assert!(f.paths.pending_op_json().exists());
    }

    // Intent: the add-recovery binding rejects a live pool device whose LUKS
    // UUID is absent from the admission membership, even if its mapper is not
    // braid-prefixed.
    // Why it exists: recovery must not treat mapper naming as an identity
    // gate; live-pool membership is correlated by UUID.
    // Scenario: a mounted pool contains disk1 plus an externally named LUKS
    // mapper with a foreign UUID. Recovery refuses to rebuild pool.json.
    #[test]
    fn committed_add_targets_rejects_foreign_live_uuid() {
        let pool = pool_state_disk1_and_foreign();
        let union = one_disk_membership();

        let err = committed_add_targets(&pool, &union, &LuksUuidMap::new())
            .expect_err("foreign live UUID must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("device luks-foreign (LUKS UUID 99999999-9999-9999-9999-999999999999)"),
            "error must name the foreign live UUID: {msg}"
        );
        assert!(
            msg.contains("recovery admission membership"),
            "error must describe the admission mismatch: {msg}"
        );
    }

    // Intent: recover_membership_matching_expected reads a live pool device
    // whose LUKS UUID is absent from the expected committed membership as a
    // topology mismatch, even if its mapper is not braid-prefixed.
    // Why it exists: phased recovery builders must preserve UUID identity when
    // comparing live topology against expected membership, and each phase
    // names the mismatch in its own wording.
    // Scenario: committed remove-missing or replace recovery compares a live
    // pool against expected membership while an external mapper is also live.
    #[test]
    fn recover_membership_matching_expected_reads_foreign_live_uuid_as_mismatch() {
        let pool = pool_state_disk1_and_foreign();
        let expected = one_disk_membership();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);

        let recovered = recover_membership_matching_expected(&pool, &expected, None, &resolver)
            .expect("a foreign live UUID is a topology mismatch, not corruption");

        assert_eq!(recovered, None);
    }

    // Intent: recover_membership_matching_expected re-inserts members whose
    // live binding is devid-only, with the same added_at precedence as the
    // live-device path.
    // Why it exists: principles 2/5 authorize btrfs devid as the fallback
    // binding when LUKS UUID is unobservable, and Decision 017 requires
    // added_at preservation. Every recovery path binds this shape through
    // the one `membership::bind_live_pool` pass, so a regression there would
    // drop the MISSING member from pool.json.
    // Scenario: phased remove-missing or replace recovery sees an unrelated
    // disk flap to MISSING; recovery must keep it in pool.json with its
    // original added_at so the operator can still address it.
    #[test]
    fn recover_membership_matching_expected_reinserts_missing_devid_member() {
        let mut pool = pool_state_two_disks();
        pool.missing_count = 1;
        pool.total_devices = 3;
        pool.missing_devids = vec![Devid::new(3)];

        let expected_added_at = "2026-04-01T00:00:00Z";
        let prior_added_at = "2026-01-01T00:00:00Z";
        let expected = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                Some(expected_added_at),
                Some(Devid::new(3)),
            ),
        ]);
        let prior = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                Some(prior_added_at),
                Some(Devid::new(3)),
            ),
        ]);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);

        let recovered =
            recover_membership_matching_expected(&pool, &expected, Some(&prior), &resolver)
                .expect("missing-devid member should be restored from expected membership")
                .expect("the live pool binds every expected member");

        assert!(recovered.by_uuid(&uuid_for_name("disk1")).is_some());
        assert!(recovered.by_uuid(&uuid_for_name("disk2")).is_some());
        let disk3 = recovered
            .by_uuid(&uuid_for_name("disk3"))
            .expect("missing disk3 must remain in recovered membership");
        assert_eq!(disk3.name, disk_name("disk3"));
        assert_eq!(disk3.by_id.as_str(), "/dev/disk/by-id/virtio-disk3");
        assert_eq!(disk3.devid, Some(Devid::new(3)));
        assert_eq!(disk3.added_at.as_deref(), Some(prior_added_at));
    }

    // Intent: live_pool_matches_membership accepts an expected member whose
    // mapper is null-underlying, binding it by the LUKS UUID its dm UUID
    // records.
    // Why it exists: a null-backed mapper keeps its LUKS identity, so recovery
    // must not need a persisted devid to recognize it.
    // Scenario: disk1 is fully observable, and disk2's mapper remains open
    // with `device: (null)` while target membership carries no devid for it.
    #[test]
    fn live_pool_matches_membership_accepts_null_underlying_by_uuid() {
        let pool = pool_state_disk1_with_null_underlying_disk2();
        let expected = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);

        let matches = live_pool_matches_membership(&pool, &expected)
            .expect("a null-backed mapper binds by UUID, not devid");

        assert!(matches);
    }

    // Intent: live_pool_matches_membership reports a topology mismatch when a
    // null-underlying mapper's LUKS UUID is not in the expected membership.
    // Why it exists: a null-backed mapper binds by UUID like a present device,
    // so a foreign one must fail the gate the same way a foreign live device
    // does.
    // Scenario: disk1 is live, but btrfs also reports a null-underlying
    // mapper whose dm UUID records disk4, which the journal never held.
    #[test]
    fn live_pool_matches_membership_rejects_foreign_null_underlying_uuid() {
        let mut pool = pool_state_disk1_with_null_underlying_disk2();
        pool.null_underlying[0].luks_uuid = uuid_for_name("disk4");
        let expected = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
        ]);

        let matches = live_pool_matches_membership(&pool, &expected)
            .expect("a foreign null-backed UUID is a topology mismatch, not corruption");

        assert!(!matches);
    }

    // Intent: recover_membership_matching_expected re-inserts a member whose
    // mapper is null-underlying, bound by its LUKS UUID, recording the live
    // devid and preserving added_at precedence.
    // Why it exists: the topology check and the membership recovery writes
    // come from one binding pass. This pins that the written pool.json keeps
    // the null-backed member the check accepted, rather than dropping a disk
    // btrfs still owns.
    // Scenario: disk1 and disk2 are fully observable, disk3 is
    // null-underlying at devid 3 with no devid in the expected membership,
    // and prior pool.json has disk3's older added_at value.
    #[test]
    fn recover_membership_matching_expected_reinserts_null_underlying_member() {
        let mut pool = pool_state_two_disks();
        pool.total_devices = 3;
        pool.null_underlying.push(NullUnderlyingDevice {
            mapper: MapperName::from_basename("braid-disk3".into()),
            luks_uuid: uuid_for_name("disk3"),
            devid: Devid::new(3),
        });

        let expected_added_at = "2026-04-01T00:00:00Z";
        let prior_added_at = "2026-01-01T00:00:00Z";
        let expected = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                Some(expected_added_at),
                None,
            ),
        ]);
        let prior = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                Some(prior_added_at),
                Some(Devid::new(3)),
            ),
        ]);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);

        let recovered =
            recover_membership_matching_expected(&pool, &expected, Some(&prior), &resolver)
                .expect("null-underlying member should be restored from expected membership")
                .expect("the live pool binds every expected member");

        let disk3 = recovered
            .by_uuid(&uuid_for_name("disk3"))
            .expect("null-underlying disk3 must remain in recovered membership");
        assert_eq!(disk3.name, disk_name("disk3"));
        assert_eq!(disk3.by_id.as_str(), "/dev/disk/by-id/virtio-disk3");
        assert_eq!(disk3.devid, Some(Devid::new(3)));
        assert_eq!(disk3.added_at.as_deref(), Some(prior_added_at));
    }

    // Intent: recover_membership_matching_expected treats the same devid in
    // missing_devids and null_underlying as one recovered membership entry.
    // Why it exists: PoolState already acknowledges this transient shape, and
    // recovery must not fail or double-insert when both probe sources identify
    // the same journaled member.
    // Scenario: disk2's devid appears both as a btrfs MISSING sentinel and as
    // a null-underlying mapper while expected membership carries devid 2.
    #[test]
    fn recover_membership_matching_expected_dedups_missing_and_null_underlying_devid() {
        let mut pool = pool_state_one_disk();
        pool.missing_count = 1;
        pool.total_devices = 2;
        pool.missing_devids = vec![Devid::new(2)];
        pool.null_underlying.push(NullUnderlyingDevice {
            mapper: MapperName::from_basename("braid-disk2".into()),
            luks_uuid: uuid_for_name("disk2"),
            devid: Devid::new(2),
        });
        let expected = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
        ]);
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);

        let recovered = recover_membership_matching_expected(&pool, &expected, None, &resolver)
            .expect("duplicate fallback sources for one devid should be idempotent")
            .expect("the live pool binds every expected member");

        assert!(recovered.by_uuid(&uuid_for_name("disk2")).is_some());
        assert_eq!(recovered.iter().count(), 2);
    }

    // Intent: live_pool_matches_membership refuses a topology where a live
    // device and a null-underlying mapper report the same devid.
    // Why it exists: the gate must fail closed on devid-level collisions even
    // when every UUID binds to a distinct expected member. The
    // live pool contradicts itself, so it refuses rather than reading as a
    // mismatch some other phase might accept.
    // Scenario: live disk2 reports devid 2, while a synthetic
    // null-underlying mapper whose dm UUID records disk4 also reports devid 2.
    #[test]
    fn live_pool_matches_membership_rejects_null_underlying_devid_colliding_with_live_devid() {
        let mut pool = pool_state_two_disks();
        pool.total_devices = 3;
        pool.null_underlying.push(NullUnderlyingDevice {
            mapper: MapperName::from_basename("braid-disk4".into()),
            luks_uuid: uuid_for_name("disk4"),
            devid: Devid::new(2),
        });
        let expected = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(22)),
            ),
            membership_entry(
                "disk4",
                "/dev/disk/by-id/virtio-disk4",
                None,
                Some(Devid::new(2)),
            ),
        ]);

        let err = live_pool_matches_membership(&pool, &expected)
            .expect_err("a devid reported twice must refuse");

        assert!(
            err.to_string()
                .contains("btrfs reports devid 2 for more than one live pool device"),
            "{err}"
        );
    }

    // Intent: live_pool_matches_membership refuses a null-underlying mapper
    // whose LUKS UUID duplicates a present device's UUID.
    // Why it exists: binding null-backed mappers by UUID must not let one
    // UUID count twice (a cloned header behind two mappers), so the gate
    // refuses rather than pick one.
    // Scenario: disk1 is live, and a null-backed braid-disk1-clone mapper's
    // dm UUID also records disk1's UUID.
    #[test]
    fn live_pool_matches_membership_rejects_null_underlying_uuid_duplicating_live_uuid() {
        let mut pool = pool_state_one_disk();
        pool.total_devices = 2;
        pool.null_underlying.push(NullUnderlyingDevice {
            mapper: MapperName::from_basename("braid-disk1-clone".into()),
            luks_uuid: uuid_for_name("disk1"),
            devid: Devid::new(2),
        });
        let expected = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            Some(Devid::new(1)),
        )]);

        let err = live_pool_matches_membership(&pool, &expected)
            .expect_err("a UUID recorded twice must refuse");

        assert!(
            err.to_string()
                .contains("both record LUKS UUID 11111111-1111-1111-1111-111111111111"),
            "{err}"
        );
    }

    // Intent: cmd_recover with dry_run = true previews and returns through
    // the short-circuit without acquiring the sleep inhibitor or performing
    // execute-phase mutation, even for a journal whose real execute path
    // would acquire.
    // Why it exists: the guarantee is enforced only by the cmd_recover dry-run
    // branch. A regression deleting it would fall through to plan.execute()
    // and acquire; the prior version drove plan_recover, which has no acquire
    // site and could only assert a construction-true property.
    // Scenario: interrupted replace committed the pool mutation
    // (PostReplaceMaintenance), so recovery would resize the new devid and
    // clear the journal; the operator runs --dry-run first and must observe
    // zero inhibitor acquisitions and zero state mutation.
    #[test]
    fn recover_dry_run_does_not_acquire_sleep_inhibitor() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk-new", uuid_for_name("disk-new").as_str());
        let new_uuid = uuid_for_name("disk-new");
        let new_uuid_text = new_uuid.to_string();
        let pre = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "old",
                "/dev/disk/by-id/virtio-old",
                None,
                Some(Devid::new(2)),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                None,
                Some(Devid::new(3)),
            ),
        ]);
        let target = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            (
                new_uuid.clone(),
                disk_member_named(
                    "disk-new",
                    "/dev/disk/by-id/virtio-disk-new",
                    None,
                    Some(Devid::new(2)),
                ),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                None,
                Some(Devid::new(3)),
            ),
        ]);
        let journal = journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Replace(journal::ReplaceOp {
                phase: journal::ReplacePhase::PostReplaceMaintenance,
                identity: journal::ReplaceIdentity::new(uuid_for_name("old"), new_uuid.clone())
                    .expect("distinct replace UUIDs"),
                old_name: disk_name("old"),
                new_name: disk_name("disk-new"),
                new_target: journal::ReplaceJournalTarget {
                    by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk-new").unwrap(),
                    mode: journal::ReplaceJournalMode::ExistingLuks {
                        enroll_key_file: None,
                    },
                },
                source: journal::ReplaceJournalSource::Missing {
                    old_devid: Devid::new(2),
                },
                restore_raid1_after_commit: false,
            }),
            pre_membership: pre,
            target_membership: target,
        };
        journal::write_journal(&f.paths, &journal).unwrap();

        let show = ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk-new\n\
             \tdevid    3 size 0 used 0 path MISSING\n",
        );
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                show,
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk-new".into()),
                },
                cryptsetup_status_active("braid-disk-new", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", &new_uuid_text),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: Devid::new(2),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            );
        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk-new"),
        ]);
        let params = f
            .recover_params()
            .passphrase_file(None)
            .dry_run(true)
            .sleep_inhibitor(&f.inhibitor)
            .build();

        cmd_recover(&runner, &fs, &resolver, &params)
            .expect("dry-run recover should preview and return without executing");

        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "dry-run must not acquire the inhibitor"
        );
        assert!(
            f.paths.pending_op_json().exists(),
            "dry-run must not clear the journal"
        );
        assert!(
            membership::load_membership(&f.paths).is_err(),
            "dry-run must not write pool.json"
        );
        let requests = runner.requests();
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, CmdRequest::BtrfsFilesystemResize { .. })),
            "dry-run must not issue the post-replace resize: {requests:?}"
        );
    }

    // Intent: verify that when the live pool contains a braid-prefixed
    // device that is in NEITHER the pre nor target membership snapshot,
    // cmd_recover for a PostAddBalanceRaid1 journal refuses to proceed
    // because the binding pass finds a live UUID the committed membership
    // lacks, surfaces the post-add-recovery mismatch message, and leaves
    // the journal + pool.json untouched.
    // Why it exists: protects the simplification that drops the
    // structurally-redundant validate_live_members_allowed call in
    // execute_add_post_balance_recovery. Without this assertion, a
    // future refactor could re-introduce that call, or check only that
    // every committed member is bound and let a foreign live device
    // through, and silently regress the error-message contract for the
    // live-not-in-target direction. Pinned via the cmd_recover load path so the
    // live-pool probe and PostAddBalanceRaid1 dispatch in
    // RecoverPlan::execute stay under the assertion.
    // Scenario: an interrupted add left a device in the btrfs pool that
    // appears in neither journal snapshot, such as an admin manually
    // running `btrfs device add` outside of braid or a stale mapper
    // surviving a prior recovery. Recovery must refuse to write
    // pool.json and leave the journal intact so the user can intervene.
    #[test]
    fn recover_fails_when_device_missing_from_both_snapshots() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .mounted_from("braid-toshiba")
            .with_luks_dm("braid-toshiba", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-mystery", "22222222-2222-2222-2222-222222222222");

        // pre and target both only know about "toshiba"
        let pre = membership_from(vec![membership_entry(
            "toshiba",
            "/dev/disk/by-id/ata-TOSHIBA",
            None,
            None,
        )]);
        let target = pre.clone();

        // Op is adding "mystery" -- but neither snapshot contains it
        let mut add_targets_by_name = BTreeMap::new();
        add_targets_by_name.insert(
            "mystery".to_owned(),
            ByIdPath::parse("/dev/disk/by-id/ata-MYSTERY").unwrap(),
        );
        let journal = journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: add_op_from_disks(add_targets_by_name),
            pre_membership: pre,
            target_membership: target,
        };
        journal::write_journal(&f.paths, &journal).unwrap();

        // Mock: pool is already mounted with both toshiba and mystery
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_toshiba_and_mystery(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-toshiba".into()),
                },
                cryptsetup_status_active("braid-toshiba", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-mystery".into()),
                },
                cryptsetup_status_active("braid-mystery", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        // Recovery should fail before by-id resolution; this resolver only
        // satisfies the cmd_recover call signature.
        let resolver = resolver_for(&[("/dev/vda", "ata-TOSHIBA")]);
        let params = f.recover_params().passphrase_file(None).build();
        let result = cmd_recover(&runner, &fs, &resolver, &params);

        // Must fail with the post-add mismatch message naming the
        // unknown device, without the deleted by-id-path remediation hint.
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("post-add recovery expected live pool membership"),
            "expected post-add mismatch message, got: {msg}"
        );
        assert!(
            msg.contains("mystery"),
            "expected error to surface the unexpected device name, got: {msg}"
        );
        assert!(
            !msg.contains("no by-id path"),
            "post-balance recovery should use the post-add mismatch message, not \
             the deleted by-id-path message; got: {msg}"
        );

        // pool.json must NOT have been written
        assert!(
            !f.paths.pool_json().exists(),
            "pool.json should not exist after failed recovery"
        );

        // pending-op.json must NOT have been cleared
        assert!(
            f.paths.pending_op_json().exists(),
            "journal should still exist after failed recovery"
        );
    }

    /// Intent: When the pool is not mounted, recover should open LUKS devices,
    /// mount the pool, rebuild pool.json from live state, and clear the journal.
    ///
    /// Why: This is the core fix for the chicken-and-egg problem where unlock
    /// blocks on journal and recover blocks on unmounted pool.
    ///
    /// Scenario: 2-disk RAID1, interrupted add of disk3. Both disk1 and disk2
    /// are present with LUKS closed. Passphrase provided via file.
    #[test]
    fn recover_self_mounts_when_pool_not_mounted() {
        let f = PoolFixture::empty();

        let journal = committed_two_disk_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let inner = MockRunner::default()
            // mount helper: mountpoint check → not mounted
            .with_output(mp_req, mp_out)
            // mount helper: probe disk1 → LUKS
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            // mount helper: probe disk2 → LUKS
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            // mount helper: verify passphrase against every unlockable disk
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            // mount helper: open disk1
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            // mount helper: open disk2
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            // mount helper: btrfs device scan
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            // mount helper: mount (disk3 absent → degraded)
            .with_output(
                CmdRequest::MountWithOptions {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                    options: vec!["degraded".to_owned()],
                },
                ok_raw_empty("mount"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            // remount cycle: umount
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("umount"),
            )
            // remount cycle: scan --forget (drop cached fs_devices) --
            // pool-scoped to the membership mappers the cycle is about
            // to close.
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-disk2".into(),
                    ],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            // remount cycle: close both mappers (the wrapper runner removes
            // mapper paths from the RemountFs after each success and
            // flips status queries to inactive so the re-probe opens them).
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                ok_raw_empty("cryptsetup close braid-disk1"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                ok_raw_empty("cryptsetup close braid-disk2"),
            )
            // remount cycle: re-mount via the same MountWithOptions mock above
            // (MockRunner serves the same response for repeated requests)
            // probe_pool: btrfs filesystem show
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            // probe_pool: cryptsetup status for each device
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            // Idle gate + replay: post-Add recovery probes btrfs balance state
            // before the owed hard RAID1 convert.
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsBalanceRaid1 {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs balance start"),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ]);

        // Initial closed set: both mappers start closed (not yet unlocked).
        // After LuksOpen the wrapper removes them from the closed set so the
        // post-mount probe_pool (which needs active) falls through to the
        // seeded `cryptsetup_status_active` stubs.
        let harness = RemountHarness::new(
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
            inner,
            &["braid-disk1", "braid-disk2"],
        )
        .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
        .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let result = cmd_recover(
            &harness.runner,
            &harness.fs,
            &resolver,
            &f.recover_params().allow_degraded(true).build(), // disk3 is absent
        );

        result.expect("recover should self-mount and succeed");

        // pool.json must have been written with disk1 and disk2
        assert!(f.paths.pool_json().exists(), "pool.json should exist");
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(
            recovered.by_name(&disk_name("disk1")).is_some(),
            "recovered membership should contain disk1"
        );
        assert!(
            recovered.by_name(&disk_name("disk2")).is_some(),
            "recovered membership should contain disk2"
        );

        // pending-op.json must have been cleared
        assert!(
            !f.paths.pending_op_json().exists(),
            "journal should be cleared after recovery"
        );
    }

    /// Intent: Add post-balance recovery can mount an offline pool when all
    /// recorded LUKS mappers are already open.
    ///
    /// Why it exists: replace-specific recovery now owns the relock/remount
    /// cycle. Add post-balance recovery should take the simpler mount-only
    /// path, then repair pool.json, replay the owed balance, and clear the
    /// journal without closing or reopening LUKS mappers.
    ///
    /// Scenario: 2-disk RAID1, interrupted add post-balance journal, both
    /// disk1 and disk2 LUKS mappers manually opened by an operator
    /// (`cryptsetup open` outside braid) before recovery is invoked. The pool
    /// is not mounted. Recover mounts, probes, balances, and completes.
    #[test]
    fn post_add_recovery_mounts_when_all_mappers_already_open() {
        let f = PoolFixture::empty();

        let journal = committed_two_disk_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let inner = MockRunner::default()
            // ── Initial plan_open_pool ──────────────────────────────────
            // mountpoint check → not mounted
            .with_output(mountpoint_fail().0, mountpoint_fail().1)
            // probe disk1 → LUKS, mapper_open=true (mapper path is in fs)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            // probe disk2 → LUKS, mapper_open=true
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            // ── Initial execute_mount_only (no LUKS open) ───────────────
            // (no CryptsetupTestPassphrase / LuksOpen mocks here — should not be called)
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::MountWithOptions {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                    options: vec!["degraded".to_owned()],
                },
                ok_raw_empty("mount"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            // Probe pool after the initial mount.
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            // Idle gate + replay: post-Add recovery probes btrfs balance state
            // before the owed hard RAID1 convert.
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsBalanceRaid1 {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs balance start"),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ]);

        // RemountFs starts with both by-id paths AND both mapper paths.
        // Both mapper paths are already present, modeling an operator who
        // opened LUKS manually before invoking recover.
        let harness = RemountHarness::new(
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/mapper/braid-disk1",
                "/dev/mapper/braid-disk2",
            ],
            inner,
            &[],
        )
        .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
        .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let result = cmd_recover(
            &harness.runner,
            &harness.fs,
            &resolver,
            &f.recover_params().allow_degraded(true).build(), // disk3 is "absent" (not in fs paths)
        );

        result.expect(
            "post-add recovery should mount with already-open mappers and finish balance replay",
        );

        let requests = harness.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupClose { .. } | CmdRequest::CryptsetupLuksOpen { .. }
            )),
            "Add post-balance recovery must not run the replace-only relock cycle"
        );

        // pool.json must have been written from live pool state.
        assert!(f.paths.pool_json().exists(), "pool.json should exist");
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(
            recovered.by_name(&disk_name("disk1")).is_some(),
            "recovered membership should contain disk1"
        );
        assert!(
            recovered.by_name(&disk_name("disk2")).is_some(),
            "recovered membership should contain disk2"
        );

        // pending-op.json must have been cleared.
        assert!(
            !f.paths.pending_op_json().exists(),
            "journal should be cleared after recovery"
        );
    }

    // Intent: the general initial-open cleanup path uses RecoverParams.sleeper
    //   for mapper-close retries after an unlock-and-mount failure.
    // Why it exists: this branch is distinct from bootstrap-add cleanup; a
    //   regression in either branch can silently fall back to RealSleeper.
    // Scenario: recover opens a non-bootstrap add journal, mount fails, and
    //   cleanup sees one mapper remain busy through all retry attempts.
    #[test]
    fn recover_initial_open_general_cleanup_honors_injected_sleeper() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal = committed_two_disk_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                err_raw("mount", 32, "wrong fs type"),
            )
            .with_output_sequence(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                vec![
                    err_raw("cryptsetup close", 5, "busy"),
                    err_raw("cryptsetup close", 5, "busy"),
                    err_raw("cryptsetup close", 5, "busy"),
                ],
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                ok_raw_empty("cryptsetup close"),
            );
        let sleeper = RecordingSleeper::default();
        let params = f.recover_params().sleeper(&sleeper).build();

        let result = cmd_recover(&runner, &fs, &MockByIdResolver::default(), &params);

        let err = result.expect_err("initial mount failure should fail recover");
        assert!(
            err.to_string()
                .contains("mount failed (exit 32): wrong fs type"),
            "primary mount error should be preserved, got: {err}; requests: {:?}",
            runner.requests()
        );
        assert_close_retry_sleeps(sleeper.calls());
    }

    /// Intent: RemoveMissing::PoolMutation recovery with every union mapper
    /// already open must NOT resolve a passphrase. The post-mount completion
    /// path for this op kind has no credential consumer, so the eager resolve
    /// gate in `execute_recover_initial_open` should be skipped (the eager
    /// resolve is now scoped to Replace::PoolMutation only).
    ///
    /// Why it exists: a regression that hoists eager credential resolution
    /// above the per-op gate -- or drops the gate entirely -- would prompt
    /// for a passphrase that recover never uses, breaking the
    /// no-prompt-when-all-mappers-open UX rule that `cmd_unlock` already
    /// honors. RemoveMissing is the cleanest scenario because both branches
    /// of `execute_remove_missing_pool_mutation_recovery` are
    /// credential-free, so any read attempt must come from the eager resolve
    /// in `execute_recover_initial_open`.
    ///
    /// Sentinel for "no credential read": `passphrase_file` points at a path
    /// that does not exist, mirroring
    /// `cmd_unlock_skips_credential_resolution_when_nothing_to_unlock`. If
    /// the gate regresses, `luks::read_passphrase` opens the bogus path and
    /// the test fails with a file-not-found error.
    ///
    /// Scenario: 3-disk pool with a `remove-missing devid 3` journal still
    /// pending; operator manually opened all three LUKS mappers before
    /// invoking `braid recover`. The pool is not mounted. Btrfs reports
    /// devid 3 as MISSING, so recovery takes the "still missing" exit:
    /// save `pre_membership`, clear the journal -- never reads a credential.
    #[test]
    fn remove_missing_pool_mutation_recovery_skips_credential_resolution_when_all_mappers_open() {
        let f = PoolFixture::empty();

        let journal = remove_missing_journal_two_survivors();
        journal::write_journal(&f.paths, &journal).unwrap();

        let inner = MockRunner::default()
            // ── Initial plan_open_pool ──────────────────────────────────
            // mountpoint check -> not mounted
            .with_output(mountpoint_fail().0, mountpoint_fail().1)
            // probe each pre_membership disk -> LUKS, mapper_open=true.
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk3".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk3",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            // mapper_open classification: status + backing UUID for each
            // mapper. Reused by the post-mount probe_pool for the two live
            // (non-MISSING) devices.
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk3".into()),
                },
                cryptsetup_status_active("braid-disk3", "/dev/vdc"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into(),
                },
                cryptsetup_uuid_ok("/dev/vdc", "33333333-3333-3333-3333-333333333333"),
            )
            // ── Initial execute_mount_only ──────────────────────────────
            // No CryptsetupTestPassphrase / LuksOpen mocks: any branch that
            // resolves a credential would hit the bogus passphrase_file
            // before reaching cryptsetup, but if it somehow proceeded, the
            // missing LuksOpen mocks would fail the test as well.
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            // ── Post-mount probe_pool ───────────────────────────────────
            // 3 devices but devid 3 is MISSING -> pool.missing_devids = [3],
            // which routes execute_remove_missing_pool_mutation_recovery
            // through the "still missing" branch (save pre_membership,
            // clear journal, no further commands).
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw(
                    "btrfs filesystem show /mnt/storage",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 3 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
                     \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n\
                     \tdevid    3 size 0 used 0 path MISSING\n",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/disk/by-id/virtio-disk3",
            ]);

        // All three by-id paths AND all three mapper paths present:
        // plan_open_pool sees PresentLuks/mapper_open=true for each member,
        // so to_unlock is empty and the initial mount takes the mount-only
        // branch (which does not consume a credential).
        let harness = RemountHarness::new(
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/disk/by-id/virtio-disk3",
                "/dev/mapper/braid-disk1",
                "/dev/mapper/braid-disk2",
                "/dev/mapper/braid-disk3",
            ],
            inner,
            &[],
        )
        .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
        .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222")
        .with_luks_dm("braid-disk3", "33333333-3333-3333-3333-333333333333");

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);

        // passphrase_file points at a path that does not exist. If a
        // regression resolves a credential eagerly for this op kind,
        // read_passphrase will fail before recovery can complete.
        let bogus = std::path::PathBuf::from("/definitely/not/a/real/path/passphrase");

        let result = cmd_recover(
            &harness.runner,
            &harness.fs,
            &resolver,
            &f.recover_params().passphrase_file(Some(&bogus)).build(),
        );

        result.expect(
            "remove-missing recovery with all mappers open must take the still-missing branch \
             and never attempt to read the (nonexistent) passphrase file",
        );

        let requests = harness.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupClose { .. }
                    | CmdRequest::CryptsetupLuksOpen { .. }
                    | CmdRequest::CryptsetupTestPassphrase { .. }
            )),
            "RemoveMissing recovery must not close/reopen mappers or verify a passphrase"
        );

        // pre_membership preserved on the still-missing branch: all three
        // disks (including the journaled devid 3) remain in pool.json.
        assert!(f.paths.pool_json().exists(), "pool.json should exist");
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(recovered.by_name(&disk_name("disk2")).is_some());
        assert!(
            recovered.by_name(&disk_name("disk3")).is_some(),
            "still-missing branch preserves the journaled missing devid in pool.json"
        );

        // pending-op.json must have been cleared.
        assert!(
            !f.paths.pending_op_json().exists(),
            "journal should be cleared after still-missing recovery"
        );
    }

    /// Intent: Replace::PoolMutation recovery resolves the credential eagerly
    /// even when every union mapper is already open, and uses it to drive the
    /// post-mount remount cycle's close/reopen pair. This pins the
    /// load-bearing case for the now-Replace-only eager resolve gate.
    ///
    /// Why it exists: this is the topology that motivates keeping the eager
    /// resolve at all. The other Replace tests in this file either call
    /// `execute_replace_pool_mutation_recovery` directly with
    /// `credential: None` or use `RemountHarness` with mappers initially
    /// closed (so the initial-unlock branch resolves the credential anyway).
    /// Neither covers `Replace::PoolMutation + open_plan.to_unlock.is_empty()`,
    /// which is the only branch where the gate is what populates
    /// `state.credential` for the RemountCycle action's
    /// `expect("...credential was resolved")` site.
    ///
    /// Scenario: operator started `braid replace old new`; the kernel-side
    /// dev_replace finished but the resize step never landed. Before
    /// invoking recover, the operator manually opened every union LUKS
    /// mapper (disk1, old, new). Recover sees all mappers open, mounts the
    /// pool, runs the cycle (close + reopen disk1, old, new using the
    /// eagerly-resolved passphrase), then resizes the new device and clears
    /// the journal.
    #[test]
    fn replace_pool_mutation_recovery_resolves_credential_and_remount_cycles_when_all_mappers_open()
    {
        let f = PoolFixture::empty();

        let journal = replace_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let inner = MockRunner::default()
            // ── Initial plan_open_pool (all union mappers already open) ─
            .with_output(mountpoint_fail().0, mountpoint_fail().1)
            // probe_config_disk for each union member's by-id.
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-old",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            // classify_mapper_ownership: status + backing-UUID probe per
            // mapper. Reused by the post-cycle probe_pool for the two
            // committed members (disk1, new).
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            // braid-old and braid-new: the underlying paths in
            // cryptsetup_status are deliberately the by-id symlinks
            // themselves. mock_virtio_backing_path_resolver only has
            // overrides for virtio-disk1..disk4, so for virtio-old and
            // virtio-new the resolver canonicalizes identity. Returning
            // the by-id as the underlying lets classify_mapper_ownership's
            // expected vs. found canonicalize-equality check pass without
            // configuring a custom backing-path resolver.
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                cryptsetup_status_active("braid-old", "/dev/disk/by-id/virtio-old"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-new".into()),
                },
                cryptsetup_status_active("braid-new", "/dev/disk/by-id/virtio-new"),
            )
            // ── Initial execute_mount_only (to_unlock is empty) ─────────
            // No CryptsetupTestPassphrase / LuksOpen here: the initial
            // mount takes the mount-only branch. The cycle below will use
            // the eagerly-resolved credential to reopen mappers.
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            // ── wait_for_kernel_replace_to_finish ───────────────────────
            // Post-resume status: Finished. The wait loop returns
            // immediately.
            .with_output(
                CmdRequest::BtrfsReplaceStatus {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw(
                    "btrfs replace status",
                    "Started on 27.Feb 10:30:00, finished on 27.Feb 10:35:00, \
                     0 write errs, 0 uncorr. read errs\n",
                ),
            )
            // ── relock_and_remount cycle ────────────────────────────────
            // 1. Umount.
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("umount"),
            )
            // 2. scan --forget -- pool-scoped to the union mappers.
            //    cycle_close_names iterates union.iter() (uuid order):
            //    disk1 (1111) → old (2222) → new (3333).
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-old".into(),
                        "/dev/mapper/braid-new".into(),
                    ],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            // 3. Close each union mapper. After each successful close,
            //    RemountRunner removes the mapper path from fs and adds
            //    the name to its `closed` set so the cycle's re-probe
            //    sees the mappers as inactive.
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                ok_raw_empty("cryptsetup close braid-disk1"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                ok_raw_empty("cryptsetup close braid-old"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-new".into()),
                },
                ok_raw_empty("cryptsetup close braid-new"),
            )
            // 4. Cycle re-plan: mountpoint check + LuksUuid + LuksDump
            //    mocks reused via MockRunner's HashMap. Status probes for
            //    just-closed mappers short-circuit to inactive via
            //    RemountRunner.
            // 5. Cycle execute_unlock_and_mount: TestPassphrase + LuksOpen
            //    for each member in to_unlock (iter_by_name → disk1, new,
            //    old). LuksOpen success re-adds the mapper path to fs.
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    mapper: MapperName::from_basename("braid-new".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-old".into(),
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            // ── Post-cycle probe_pool ───────────────────────────────────
            // The committed topology: 2 devices, disk1 (devid 1) + new
            // (devid 2). Status + LuksUuid mocks above are reused for the
            // per-device probes.
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_disk1_and_new(),
            )
            // ── execute_replace_post_maintenance_recovery ───────────────
            // Resize-to-max on the new device's devid (2).
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: Devid::new(2),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-old",
                "/dev/disk/by-id/virtio-new",
            ]);
        // Note: BtrfsBalanceStatus / BtrfsBalanceRaid1Soft are not mocked
        // because replace_journal() carries restore_raid1_after_commit=false.

        // All three by-id paths AND all three mapper paths present
        // initially. `already_closed = &[]` means probe_mapper_open hits
        // the inner runner's status mocks (active) on the first plan, then
        // RemountRunner records each successful CryptsetupClose into its
        // closed set during the cycle.
        let harness = RemountHarness::new(
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-old",
                "/dev/disk/by-id/virtio-new",
                "/dev/mapper/braid-disk1",
                "/dev/mapper/braid-old",
                "/dev/mapper/braid-new",
            ],
            inner,
            &[],
        )
        .with_luks_dm("braid-old", "22222222-2222-2222-2222-222222222222")
        .with_luks_dm("braid-new", "33333333-3333-3333-3333-333333333333")
        .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);

        let result = cmd_recover(
            &harness.runner,
            &harness.fs,
            &resolver,
            &f.recover_params().build(),
        );

        result.expect(
            "replace recovery with all union mappers open must mount, cycle, and replay resize",
        );

        // The cycle MUST run: every union mapper closed then reopened.
        let requests = harness.requests();
        let close_count = requests
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupClose { .. }))
            .count();
        assert!(
            close_count >= 3,
            "expected at least 3 CryptsetupClose calls (cycle close set), got {close_count}"
        );
        let old_close_count = requests
            .iter()
            .filter(|r| {
                matches!(r, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-old")
            })
            .count();
        assert_eq!(
            old_close_count, 2,
            "braid-old should close once in the cycle and once in post-maintenance"
        );
        let reopen_count = requests
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupLuksOpen { .. }))
            .count();
        assert_eq!(
            reopen_count, 3,
            "cycle must reopen exactly the three union mappers using the eagerly-resolved credential"
        );

        // pool.json must reflect the committed replace topology.
        assert!(f.paths.pool_json().exists(), "pool.json should exist");
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(
            recovered.by_name(&disk_name("new")).is_some(),
            "post-replace membership must contain new"
        );
        assert!(
            recovered.by_name(&disk_name("old")).is_none(),
            "post-replace membership must not contain old"
        );

        // pending-op.json must have been cleared.
        assert!(
            !f.paths.pending_op_json().exists(),
            "journal should be cleared after committed replace recovery"
        );
    }

    /// Intent: When the post-mount remount cycle's umount fails, recover must
    /// abort with the umount failure before writing pool.json or clearing the
    /// journal — otherwise we would persist a snapshot read from a stale
    /// in-memory mount session.
    ///
    /// Why: The cycle exists to drop cached btrfs_fs_devices that may carry a
    /// post-resume phantom MISSING devid (see recover.rs comment near
    /// remount_for_fresh_kernel_state). If umount fails, we cannot trust
    /// probe_pool's view, so the only safe action is to fail recovery and
    /// leave the journal in place for retry.
    ///
    /// Scenario: interrupted replace. LUKS opens and the first mount succeeds,
    /// but the replace-specific cycle's umount returns EBUSY.
    #[test]
    fn recover_remount_cycle_umount_failure_aborts_before_pool_json() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-new",
            "/dev/disk/by-id/virtio-old",
        ]);

        let journal = replace_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-old",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    mapper: MapperName::from_basename("braid-new".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-old".into(),
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            .with_output(
                CmdRequest::BtrfsReplaceStatus {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw(
                    "btrfs replace status",
                    "Started on 27.Feb 10:30:00, finished on 27.Feb 10:35:00, \
                     0 write errs, 0 uncorr. read errs\n",
                ),
            )
            // Cycle umount fails with EBUSY.
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                err_raw("umount", 32, "umount: target is busy"),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-new",
                "/dev/disk/by-id/virtio-old",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-new", "braid-old"]);
        // No probe_pool / save_membership / clear_journal mocks — those must
        // not be reached.

        let result = cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &f.recover_params().allow_degraded(true).build(),
        );

        let err = result.expect_err("cycle umount failure must abort recover");
        let msg = err.to_string();
        assert!(
            msg.contains("recover remount cycle"),
            "error should name the remount cycle as the failure point, got: {msg}"
        );
        assert!(
            msg.contains("umount"),
            "error should mention umount, got: {msg}"
        );

        // pool.json must NOT have been written.
        assert!(
            !f.paths.pool_json().exists(),
            "pool.json must not be written when the remount cycle aborts"
        );
        // Journal must be intact for retry.
        assert!(
            f.paths.pending_op_json().exists(),
            "journal must remain in place after a failed remount cycle"
        );
    }

    // Intent: relock_and_remount honors the planned close_names and does
    //   not close or forget a membership mapper that appeared in
    //   /dev/mapper between plan and execute.
    // Why it exists: closing a mapper not in the plan reopens the
    //   cryptsetup-close-btrfs-held race because the forget argv is
    //   plan-derived; it also breaks the dry-run -> execute contract.
    // Scenario: plan_recover would have computed close_names =
    //   [disk1, disk2]. Between plan and execute, /dev/mapper/braid-extra
    //   appears (membership union also lists 'extra' for the test, to
    //   prove the new code does not fall back to membership.disks.keys()).
    //   Execute must not issue CryptsetupClose for braid-extra and the
    //   BtrfsDeviceScanForget argv must not contain /dev/mapper/braid-extra.
    #[test]
    fn recover_remount_cycle_honors_close_names_over_membership() {
        let config = Config::new(MountPoint::new("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-extra",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
            "/dev/mapper/braid-extra",
        ])
        .with_luks_dm("braid-extra", "33333333-3333-3333-3333-333333333333");
        let membership = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
            (
                uuid_raw("33333333-3333-3333-3333-333333333333"),
                disk_member_named("extra", "/dev/disk/by-id/virtio-extra", None, None),
            ),
        ]);
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("umount"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-disk2".into(),
                    ],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                ok_raw_empty("cryptsetup close"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                ok_raw_empty("cryptsetup close"),
            )
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-extra".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-extra",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/disk/by-id/virtio-extra",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
            .with_mapper_open(
                "braid-extra",
                "/dev/vdc",
                "33333333-3333-3333-3333-333333333333",
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            );
        let close_names = vec![disk_name("disk1"), disk_name("disk2")];

        let backing_path_resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path("/dev/disk/by-id/virtio-disk1", "/dev/vda")
            .with_path("/dev/disk/by-id/virtio-disk2", "/dev/vdb")
            .with_path("/dev/disk/by-id/virtio-extra", "/dev/vdc");

        relock_and_remount(
            &runner,
            &fs,
            RelockAndRemountCtx {
                sleeper: &progress::NoopSleeper,
                config: &config,
                paths: &crate::test_fixtures::isolated_paths().1,
                membership: &membership,
                backing_path_resolver: &backing_path_resolver,
                allow_degraded: false,
                credential: &OpenCredential::Passphrase(Passphrase::from_zeroizing(
                    zeroize::Zeroizing::new("testpass".to_owned()),
                )),
                close_names: &close_names,
            },
        )
        .expect("remount cycle should succeed without touching unplanned mapper");

        let requests = runner.requests();
        let forget_devices = requests
            .iter()
            .find_map(|r| match r {
                CmdRequest::BtrfsDeviceScanForget { devices } => Some(devices),
                _ => None,
            })
            .expect("scan --forget should run for planned mappers");
        assert_eq!(
            forget_devices,
            &vec![
                "/dev/mapper/braid-disk1".to_owned(),
                "/dev/mapper/braid-disk2".to_owned(),
            ]
        );
        assert!(
            !forget_devices.contains(&"/dev/mapper/braid-extra".to_owned()),
            "forget argv must not include unplanned mapper"
        );

        let close_mappers = requests
            .iter()
            .filter_map(|r| match r {
                CmdRequest::CryptsetupClose { mapper } => Some(mapper.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            close_mappers,
            vec!["braid-disk1", "braid-disk2"],
            "close requests must be limited to planned close_names"
        );
    }

    // Intent: relock_and_remount uses fs.exists only as a disappearance
    //   guard -- if a planned close target's mapper is gone at execute
    //   time, neither cryptsetup close nor the forget argv references it.
    // Why it exists: a previously-open mapper can vanish between plan and
    //   execute. The cycle must degrade gracefully without spurious errors.
    // Scenario: close_names = [disk1, disk2]; only /dev/mapper/braid-disk1
    //   exists at execute time. The forget argv contains exactly
    //   /dev/mapper/braid-disk1 and CryptsetupClose runs only for disk1.
    #[test]
    fn recover_remount_cycle_skips_disappeared_planned_mapper() {
        let config = Config::new(MountPoint::new("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1",
        ]);
        let membership = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("umount"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec!["/dev/mapper/braid-disk1".into()],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                ok_raw_empty("cryptsetup close"),
            )
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            );
        let close_names = vec![disk_name("disk1"), disk_name("disk2")];

        relock_and_remount(
            &runner,
            &fs,
            RelockAndRemountCtx {
                sleeper: &progress::NoopSleeper,
                config: &config,
                paths: &crate::test_fixtures::isolated_paths().1,
                membership: &membership,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
                allow_degraded: false,
                credential: &OpenCredential::Passphrase(Passphrase::from_zeroizing(
                    zeroize::Zeroizing::new("testpass".to_owned()),
                )),
                close_names: &close_names,
            },
        )
        .expect("remount cycle should succeed when a planned mapper disappeared");

        let requests = runner.requests();
        let forget_devices = requests
            .iter()
            .find_map(|r| match r {
                CmdRequest::BtrfsDeviceScanForget { devices } => Some(devices),
                _ => None,
            })
            .expect("scan --forget should run for surviving planned mapper");
        assert_eq!(forget_devices, &vec!["/dev/mapper/braid-disk1".to_owned()]);

        let close_mappers = requests
            .iter()
            .filter_map(|r| match r {
                CmdRequest::CryptsetupClose { mapper } => Some(mapper.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            close_mappers,
            vec!["braid-disk1"],
            "disappeared planned mapper must not be closed"
        );
    }

    // Intent: A post-open failure in the recover remount cycle closes the
    // mappers reopened by that cycle.
    // Why it exists: the cycle intentionally destroys and recreates dm-crypt
    // devices; if re-mount then fails, those newly-opened mappers belong to
    // recover and must not be left open.
    // Scenario: relock/remount closes two membership mappers, reopens both,
    // then the final mount fails.
    #[test]
    fn recover_remount_cycle_mount_failure_closes_reopened_mappers() {
        let config = Config::new(MountPoint::new("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
        ]);
        let membership = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("umount"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-disk2".into(),
                    ],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                ok_raw_empty("cryptsetup close"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                ok_raw_empty("cryptsetup close"),
            )
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                err_raw("mount", 32, "mount failed"),
            );
        let close_names = vec![disk_name("disk1"), disk_name("disk2")];

        let err = relock_and_remount(
            &runner,
            &fs,
            RelockAndRemountCtx {
                sleeper: &progress::NoopSleeper,
                config: &config,
                paths: &crate::test_fixtures::isolated_paths().1,
                membership: &membership,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
                allow_degraded: false,
                credential: &OpenCredential::Passphrase(Passphrase::from_zeroizing(
                    zeroize::Zeroizing::new("testpass".to_owned()),
                )),
                close_names: &close_names,
            },
        )
        .expect_err("final mount should fail");
        assert!(
            err.to_string().contains("recover remount cycle: re-mount"),
            "error should preserve re-mount context: {err}"
        );

        let closes = runner
            .requests()
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupClose { .. }))
            .count();
        assert_eq!(
            closes, 4,
            "two cycle closes plus two cleanup closes should run"
        );
    }

    // Intent: remount-cycle re-mount cleanup uses the injected sleeper for
    //   busy mapper-close retries.
    // Why it exists: relock_and_remount has its own cleanup call site after
    //   reopening mappers, separate from initial-open cleanup.
    // Scenario: relock/remount closes two mappers, reopens both, final mount
    //   fails, and one cleanup close stays busy through all retry attempts.
    #[test]
    fn recover_remount_cycle_mount_failure_cleanup_honors_injected_sleeper() {
        let config = Config::new(MountPoint::new("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
        ]);
        let membership = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("umount"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-disk2".into(),
                    ],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            .with_output_sequence(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                vec![
                    ok_raw_empty("cryptsetup close"),
                    err_raw("cryptsetup close", 5, "busy"),
                    err_raw("cryptsetup close", 5, "busy"),
                    err_raw("cryptsetup close", 5, "busy"),
                ],
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                ok_raw_empty("cryptsetup close"),
            )
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                err_raw("mount", 32, "mount failed"),
            );
        let close_names = vec![disk_name("disk1"), disk_name("disk2")];
        let sleeper = RecordingSleeper::default();

        let err = relock_and_remount(
            &runner,
            &fs,
            RelockAndRemountCtx {
                sleeper: &sleeper,
                config: &config,
                paths: &crate::test_fixtures::isolated_paths().1,
                membership: &membership,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
                allow_degraded: false,
                credential: &OpenCredential::Passphrase(Passphrase::from_zeroizing(
                    zeroize::Zeroizing::new("testpass".to_owned()),
                )),
                close_names: &close_names,
            },
        )
        .expect_err("final mount should fail");
        assert!(
            err.to_string().contains("recover remount cycle: re-mount"),
            "error should preserve re-mount context: {err}"
        );
        assert_close_retry_sleeps(sleeper.calls());
    }

    // Intent: relock_and_remount's step-3 mapper close retries a transient
    //   EBUSY and lets the cycle complete, rather than hard-aborting the
    //   recovery on the first busy close.
    // Why it exists: step 3 used to call CryptsetupClose directly, so unlike
    //   every other close path it failed the whole recovery on the first
    //   transient busy. Folding it through the shared busy-retry core gave it
    //   the same resilience; this pins that the retry actually happens here.
    // Scenario: a short-lived holder keeps braid-disk1 busy for the first
    //   step-3 close attempt, then releases it; the second attempt succeeds and
    //   the cycle re-opens and re-mounts as normal.
    #[test]
    fn recover_remount_cycle_retries_busy_step3_close() {
        let config = Config::new(MountPoint::new("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
        ]);
        let membership = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("umount"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-disk2".into(),
                    ],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            // Step-3 close of braid-disk1 is busy on the first attempt, then
            // releases. Pre-fold this exit-5 hard-aborted the cycle.
            .with_output_sequence(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                vec![
                    err_raw("cryptsetup close", 5, "busy"),
                    ok_raw_empty("cryptsetup close"),
                ],
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                ok_raw_empty("cryptsetup close"),
            )
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            );
        let close_names = vec![disk_name("disk1"), disk_name("disk2")];

        relock_and_remount(
            &runner,
            &fs,
            RelockAndRemountCtx {
                sleeper: &progress::NoopSleeper,
                config: &config,
                paths: &crate::test_fixtures::isolated_paths().1,
                membership: &membership,
                backing_path_resolver: crate::test_fixtures::mock_virtio_backing_path_resolver(),
                allow_degraded: false,
                credential: &OpenCredential::Passphrase(Passphrase::from_zeroizing(
                    zeroize::Zeroizing::new("testpass".to_owned()),
                )),
                close_names: &close_names,
            },
        )
        .expect("remount cycle should complete after retrying the busy step-3 close");

        let disk1_closes = runner
            .requests()
            .iter()
            .filter(|r| {
                matches!(r, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-disk1")
            })
            .count();
        assert_eq!(
            disk1_closes, 2,
            "step-3 close of braid-disk1 must retry the transient busy before succeeding"
        );
    }

    /// Intent: When a disk is absent and --allow-degraded is not passed, recover
    /// must refuse with a structured DegradedRefused error.
    ///
    /// Why: Principle 1 requires explicit opt-in for degraded mounts, even
    /// during recovery.
    ///
    /// Scenario: 2-disk pool with interrupted add of disk3. disk3 is absent.
    /// allow_degraded=false.
    #[test]
    fn recover_refuses_degraded_without_flag() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal = two_disk_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);
        // No mount mock — should not reach mount

        let result = cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &f.recover_params().build(),
        );

        let err = result.expect_err("should refuse degraded mount");
        assert!(
            err.is_degraded_refusal(),
            "expected DegradedRefused, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("braid recover --allow-degraded"),
            "hint should reference 'braid recover --allow-degraded', got: {msg}"
        );

        // Journal must NOT have been cleared
        assert!(
            f.paths.pending_op_json().exists(),
            "journal should still exist after refused recovery"
        );
    }

    /// Intent: When the pool is already mounted, recover should skip the mount
    /// step and proceed directly to rebuilding pool.json.
    ///
    /// Why: The user may have manually opened LUKS and mounted, or the
    /// interrupted operation left the pool mounted. No passphrase needed.
    ///
    /// Scenario: 2-disk pool, already mounted. No passphrase mocks needed.
    #[test]
    fn recover_skips_mount_when_already_mounted() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");

        let journal = committed_two_disk_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            // mount helper: mountpoint check → already mounted
            .with_output(mp_req, mp_out)
            // probe_pool: btrfs filesystem show
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            // probe_pool: cryptsetup status for each device
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            // Idle gate + replay: post-Add recovery probes btrfs balance state
            // before the owed hard RAID1 convert.
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsBalanceRaid1 {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs balance start"),
            );

        // No passphrase — pool is already mounted
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &f.recover_params().passphrase_file(None).build(),
        );

        result.expect("recover should succeed when pool already mounted");

        assert!(f.paths.pool_json().exists(), "pool.json should exist");
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(recovered.by_name(&disk_name("disk2")).is_some());
        assert!(
            !f.paths.pending_op_json().exists(),
            "journal should be cleared"
        );
    }

    /*
     * Intent: verify recover preserves an existing member's `added_at` from
     * current pool.json before consulting the journal.
     * Why it exists: recover used to rebuild every member with a fresh
     * timestamp, erasing the historical first-added value on every run.
     * Scenario: pool.json and the journal both know disk1, but disagree on
     * `added_at`; the live pool contains disk1, so recovered pool.json must
     * keep the current pool.json value.
     */
    #[test]
    fn recover_preserves_added_at_from_current_pool_json() {
        let f = PoolFixture::empty();
        let fs =
            MockFs::new(&[]).with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let mut current = PoolMembership::empty();
        current
            .insert(
                uuid_for_name("disk1"),
                disk_member_named(
                    "disk1",
                    "/dev/disk/by-id/old-disk1",
                    Some(POOL_JSON_ADDED_AT),
                    None,
                ),
            )
            .expect("insert current disk1");
        membership::save_membership(&current, &f.paths).unwrap();

        let journal = interrupted_remove_journal(Some(LEGACY_JOURNAL_ADDED_AT));
        journal::write_journal(&f.paths, &journal).unwrap();

        let runner = already_mounted_one_disk_runner();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &f.recover_params().passphrase_file(None).build(),
        );

        result.expect("recover should succeed");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert_eq!(
            recovered
                .by_name(&disk_name("disk1"))
                .expect("disk1 recovered")
                .1
                .added_at
                .as_deref(),
            Some(POOL_JSON_ADDED_AT)
        );
    }

    /*
     * Intent: verify recover falls back to the journal's `added_at` when
     * current pool.json is absent.
     * Why it exists: recovery mode can be entered after pool.json was not
     * durably written, but pending-op.json still carries the pre-operation
     * membership snapshot that preserves historical timestamps.
     * Scenario: no pool.json exists; the journal's pre-membership has disk1
     * with a known `added_at`; the live pool contains disk1.
     */
    #[test]
    fn recover_preserves_added_at_from_journal_when_pool_json_absent() {
        let f = PoolFixture::empty();
        let fs =
            MockFs::new(&[]).with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let pre = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                Some(JOURNAL_ADDED_AT),
                None,
            ),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);
        let target = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            None,
        )]);
        let journal = journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Remove {
                luks_uuid: uuid_for_name("disk2"),
                name: disk_name("disk2"),
            },
            pre_membership: pre,
            target_membership: target,
        };
        journal::write_journal(&f.paths, &journal).unwrap();

        let runner = already_mounted_one_disk_runner();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &f.recover_params().passphrase_file(None).build(),
        );

        result.expect("recover should succeed");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert_eq!(
            recovered
                .by_name(&disk_name("disk1"))
                .expect("disk1 recovered")
                .1
                .added_at
                .as_deref(),
            Some(JOURNAL_ADDED_AT)
        );
    }

    /*
     * Intent: verify recover stamps a fresh `added_at` only when neither
     * current pool.json nor the journal has a prior timestamp.
     * Why it exists: true bootstrap recovery still needs an added timestamp
     * after the live pool exists; preserving timestamps must not leave the
     * new member permanently unstamped.
     * Scenario: first-ever add reached a mounted one-disk pool, but no
     * pool.json exists and the bootstrap journal has no `added_at`.
     */
    #[test]
    fn recover_stamps_fresh_added_at_when_no_prior_record() {
        let f = PoolFixture::empty();
        let fs =
            MockFs::new(&[]).with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let journal = bootstrap_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let runner = with_idle_balance_status(already_mounted_one_disk_runner());
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &f.recover_params().passphrase_file(None).build(),
        );

        result.expect("recover should succeed");

        let recovered = membership::load_membership(&f.paths).unwrap();
        let added_at = recovered
            .by_name(&disk_name("disk1"))
            .expect("disk1 recovered")
            .1
            .added_at
            .as_deref()
            .expect("recover should stamp added_at");
        time::OffsetDateTime::parse(
            added_at,
            &time::format_description::well_known::Iso8601::DEFAULT,
        )
        .expect("fresh added_at should parse as ISO-8601");
        assert_ne!(added_at, POOL_JSON_ADDED_AT);
        assert_ne!(added_at, JOURNAL_ADDED_AT);
    }

    /*
     * Intent: verify recover applies `added_at` preservation per disk during
     * a partially committed add.
     * Why it exists: a mid-add crash can leave existing members with
     * historical timestamps while the newly added live member has no prior
     * record; recover must preserve one and stamp the other.
     * Scenario: pool.json has only disk1 with a historical `added_at`; the
     * journal target and live pool include disk2 from the in-flight add.
     */
    #[test]
    fn recover_carries_partial_added_at_for_mid_add_crash() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");

        let mut current = PoolMembership::empty();
        current
            .insert(
                uuid_for_name("disk1"),
                disk_member_named(
                    "disk1",
                    "/dev/disk/by-id/virtio-disk1",
                    Some(POOL_JSON_ADDED_AT),
                    None,
                ),
            )
            .expect("insert current disk1");
        membership::save_membership(&current, &f.paths).unwrap();

        let pre = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            Some(POOL_JSON_ADDED_AT),
            None,
        )]);
        let target = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                Some(POOL_JSON_ADDED_AT),
                None,
            ),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);

        let mut add_targets_by_name = BTreeMap::new();
        add_targets_by_name.insert(
            "disk2".to_owned(),
            ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
        );
        let journal = journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: add_op_from_disks(add_targets_by_name),
            pre_membership: pre,
            target_membership: target,
        };
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsBalanceRaid1 {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs balance start"),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &f.recover_params().passphrase_file(None).build(),
        );

        result.expect("recover should succeed");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert_eq!(
            recovered
                .by_name(&disk_name("disk1"))
                .expect("disk1 recovered")
                .1
                .added_at
                .as_deref(),
            Some(POOL_JSON_ADDED_AT)
        );
        let disk2_added_at = recovered
            .by_name(&disk_name("disk2"))
            .expect("disk2 recovered")
            .1
            .added_at
            .as_deref()
            .expect("new disk should be stamped");
        assert_ne!(disk2_added_at, POOL_JSON_ADDED_AT);
    }

    /// Bootstrap journal: pre_membership is empty, target has one disk.
    fn bootstrap_journal() -> journal::Journal {
        let target = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            None,
        )]);

        let mut add_targets_by_name = BTreeMap::new();
        add_targets_by_name.insert(
            "disk1".to_owned(),
            ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: add_op_from_disks(add_targets_by_name),
            pre_membership: PoolMembership::empty(),
            target_membership: target,
        }
    }

    /// Bootstrap journal as `braid add` writes it before mkfs.btrfs: phase
    /// PoolMutation, empty pre_membership, one fresh target.
    fn bootstrap_pool_mutation_journal() -> journal::Journal {
        let mut journal = bootstrap_journal();
        if let OpKind::Add { phase, .. } = &mut journal.op {
            *phase = journal::AddPhase::PoolMutation;
        }
        journal
    }

    /// Runner for a bootstrap whose disk1 luksFormat wrote: closed, with the
    /// braid label and the journaled UUID, and no btrfs on its mapper.
    /// `close` answers the close that ends the btrfs check.
    fn bootstrap_formatted_disk1_runner(close: RawCommandOutput) -> MockRunner {
        let (mp_req, mp_out) = mountpoint_fail();
        MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                luks_dump_label("braid-disk1"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk1".into(),
                },
                err_raw(
                    "btrfs filesystem show",
                    1,
                    "not a valid btrfs filesystem on /dev/mapper/braid-disk1",
                ),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec!["/dev/mapper/braid-disk1".into()],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                close,
            )
            .with_mapper_closed("braid-disk1")
    }

    /// Intent: a bootstrap add that luksFormat wrote but mkfs.btrfs never
    ///   reached is cleared by recover: it opens the formatted disk, finds no
    ///   btrfs, closes the disk, and clears pending-op.json without mounting.
    ///
    /// Why it exists: there is no pool to recover, so a mount can only fail.
    ///   Recover used to refuse with steps for deleting pending-op.json by
    ///   hand (finding 261022-1230-002).
    ///
    /// Scenario: first-ever braid add of one disk. luksFormat succeeded, then
    ///   a crash before mkfs.btrfs. The operator runs braid recover.
    #[test]
    fn recover_bootstrap_formatted_without_btrfs_clears_journal() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1"]);
        journal::write_journal(&f.paths, &bootstrap_pool_mutation_journal()).unwrap();
        let runner = bootstrap_formatted_disk1_runner(ok_raw_empty("cryptsetup close"));

        cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &f.recover_params().sleep_inhibitor(&f.inhibitor).build(),
        )
        .expect("an uncommitted bootstrap clears the journal");
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "clearing an uncommitted bootstrap holds no sleep inhibitor"
        );

        assert!(
            !f.paths.pending_op_json().exists(),
            "the journal must be cleared"
        );
        assert!(!f.paths.pool_json().exists(), "pool.json should not exist");
        let requests = runner.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::Mount { .. }
                    | CmdRequest::MkfsBtrfs { .. }
                    | CmdRequest::MkfsBtrfsRaid1 { .. }
                    | CmdRequest::CryptsetupLuksFormat { .. }
            )),
            "an uncommitted bootstrap must not mount, mkfs, or format: {requests:?}"
        );
        let probe_pos = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsFilesystemShowTarget { .. }))
            .expect("the btrfs check should run");
        let close_pos = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::CryptsetupClose { .. }))
            .expect("the opened mapper should close");
        assert!(
            probe_pos < close_pos,
            "the btrfs check must precede the close"
        );
    }

    /// Intent: when the bootstrap btrfs check cannot close a mapper it
    ///   opened, recover refuses and keeps pending-op.json; the close retries
    ///   use the injected sleeper.
    ///
    /// Why it exists: once recover clears the journal it tells the operator
    ///   to wipe the formatted disks, so a mapper left open would sit on a disk
    ///   being wiped.
    ///
    /// Scenario: the same crash before mkfs.btrfs, but something holds
    ///   braid-disk1 open while recover checks it.
    #[test]
    fn recover_bootstrap_check_close_failure_keeps_journal() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1"]);
        journal::write_journal(&f.paths, &bootstrap_pool_mutation_journal()).unwrap();
        let runner = bootstrap_formatted_disk1_runner(err_raw("cryptsetup close", 5, "busy"));
        let sleeper = RecordingSleeper::default();

        let err = cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &f.recover_params().sleeper(&sleeper).build(),
        )
        .expect_err("a mapper the check cannot close must refuse");

        assert!(
            err.to_string()
                .contains("could not close a mapper it opened"),
            "the refusal must name the close failure: {err}"
        );
        assert!(
            f.paths.pending_op_json().exists(),
            "journal should still exist"
        );
        assert_close_retry_sleeps(sleeper.calls());
    }

    /// Intent: a bootstrap add that stopped before luksFormat wrote any disk
    ///   is cleared with no passphrase, open, or mount.
    ///
    /// Why it exists: nothing changed on disk, so recover has nothing to
    ///   unlock. Before finding 261022-1230-002 recover replayed toward a
    ///   mount of a pool that never existed and refused on every run.
    ///
    /// Scenario: the first braid add's luksFormat refused an option after the
    ///   journal was written; the operator runs braid recover with no
    ///   passphrase source.
    #[test]
    fn recover_bootstrap_with_no_formatted_target_clears_journal_without_passphrase() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1"]);
        journal::write_journal(&f.paths, &bootstrap_pool_mutation_journal()).unwrap();
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                err_raw(
                    "cryptsetup luksUUID",
                    1,
                    "Device is not a valid LUKS device",
                ),
            );

        cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &f.recover_params().passphrase_file(None).build(),
        )
        .expect("a bootstrap that formatted nothing clears the journal");

        assert!(!f.paths.pending_op_json().exists());
        assert!(!f.paths.pool_json().exists());
        let requests = runner.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksOpen { .. }
                    | CmdRequest::CryptsetupTestPassphrase { .. }
                    | CmdRequest::Mount { .. }
                    | CmdRequest::CryptsetupLuksFormat { .. }
            )),
            "nothing to unlock, mount, or format: {requests:?}"
        );
    }

    /// Intent: the dry-run of a bootstrap that formatted nothing shows the
    ///   journal clear as its only step.
    ///
    /// Why it exists: the preview must show what execute does (ADR 022).
    ///
    /// Scenario: the operator previews braid recover --dry-run after the first
    ///   braid add's luksFormat refused an option.
    #[test]
    fn plan_recover_dry_run_bootstrap_with_no_formatted_target_renders_clear() {
        let (mp_req, mp_out) = mountpoint_fail();
        let rendered = render_recover_dry_run(
            bootstrap_pool_mutation_journal(),
            MockFs::new(&["/dev/disk/by-id/virtio-disk1"]),
            MockRunner::default()
                .with_output(mp_req, mp_out)
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk1".into(),
                    },
                    err_raw(
                        "cryptsetup luksUUID",
                        1,
                        "Device is not a valid LUKS device",
                    ),
                ),
            false,
        );
        assert!(
            rendered.contains("clear pending-op.json")
                && rendered
                    .contains("(bootstrap add stopped before mkfs.btrfs; no disk was formatted)"),
            "missing clear row: {rendered}"
        );
        assert!(
            !rendered.contains("$ "),
            "an uncommitted bootstrap runs no command: {rendered}"
        );
    }

    /// Intent: an unmounted bootstrap add whose journaled disk is gone refuses
    ///   at that disk before it opens or mounts anything, with or without
    ///   --allow-degraded, and the refusal is not a degraded refusal, so braid
    ///   exits 1, not 2.
    ///
    /// Why it exists: the bootstrap rule probes every target before mount
    ///   planning, since a gone disk may be one luksFormat wrote. A bootstrap
    ///   journal records no devids, so --allow-degraded cannot finish it; the
    ///   refusal names the disk to reconnect instead of the degraded flag.
    ///
    /// Scenario: first-ever braid add of disk1 and disk2. luksFormat wrote
    ///   disk1, then a crash. disk2 is unplugged when the operator runs braid
    ///   recover.
    #[test]
    fn recover_bootstrap_absent_target_refuses_before_mount() {
        for allow_degraded in [false, true] {
            let f = PoolFixture::empty();
            let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1"]);
            journal::write_journal(&f.paths, &bootstrap_pool_mutation_add_journal()).unwrap();
            let (mp_req, mp_out) = mountpoint_fail();
            let runner = MockRunner::default()
                .with_output(mp_req, mp_out)
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-disk1".into(),
                    },
                    cryptsetup_uuid_ok(
                        "/dev/disk/by-id/virtio-disk1",
                        "11111111-1111-1111-1111-111111111111",
                    ),
                )
                .with_output(
                    CmdRequest::CryptsetupLuksDumpText {
                        device: "/dev/disk/by-id/virtio-disk1".into(),
                    },
                    luks_dump_label("braid-disk1"),
                )
                .with_mapper_closed("braid-disk1");

            let err = cmd_recover(
                &runner,
                &fs,
                &MockByIdResolver::default(),
                &f.recover_params().allow_degraded(allow_degraded).build(),
            )
            .expect_err("an absent bootstrap disk must refuse");

            let msg = err.to_string();
            assert!(
                msg.contains(
                    "recover add target 'disk2' (/dev/disk/by-id/virtio-disk2) is not present; reconnect it"
                ),
                "allow_degraded={allow_degraded}: {msg}"
            );
            assert!(
                !err.is_degraded_refusal(),
                "allow_degraded={allow_degraded}: the refusal must exit 1, not 2: {msg}"
            );
            assert!(
                f.paths.pending_op_json().exists(),
                "allow_degraded={allow_degraded}: the journal must be kept"
            );
            let requests = runner.requests();
            assert!(
                !requests.iter().any(|r| matches!(
                    r,
                    CmdRequest::CryptsetupLuksOpen { .. }
                        | CmdRequest::CryptsetupTestPassphrase { .. }
                        | CmdRequest::Mount { .. }
                )),
                "allow_degraded={allow_degraded}: nothing opens or mounts: {requests:?}"
            );
        }
    }

    // Intent: the uncommitted-bootstrap message names each formatted disk
    //   and the wipe command, or says nothing changed when no disk was
    //   formatted.
    // Why it exists: braid add refuses a braid-labeled disk with no btrfs,
    //   so the operator must wipe braid's LUKS container before a retry.
    // Scenario: recover clears a bootstrap journal after luksFormat wrote
    //   disk1.
    #[test]
    fn uncommitted_bootstrap_message_names_disks_to_wipe() {
        let msg = uncommitted_bootstrap_message(&[(
            disk_name("disk1"),
            by_id_path("/dev/disk/by-id/virtio-disk1"),
        )]);
        assert!(
            msg.contains("disk1 (/dev/disk/by-id/virtio-disk1)")
                && msg.contains("sudo wipefs -a")
                && msg.contains("pending-op.json cleared"),
            "{msg}"
        );
        let none = uncommitted_bootstrap_message(&[]);
        assert!(
            none.contains("nothing changed") && !none.contains("wipefs"),
            "{none}"
        );
    }

    /// Intent: when bootstrap recover fails due to wrong passphrase, the error
    ///   must be the original passphrase error — not the bootstrap escape
    ///   instructions.
    ///
    /// Why it exists: an earlier version caught all MountErrors during bootstrap
    ///   recovery, which would tell the user to wipe disks when the real problem
    ///   was just a typo in the passphrase.
    ///
    /// Scenario: first-ever braid add of one disk. LUKS format succeeded, crash
    ///   before mkfs. User runs braid recover with wrong passphrase. Error should
    ///   say "wrong passphrase", not "bootstrap add was interrupted".
    #[test]
    fn recover_bootstrap_wrong_passphrase_not_masked() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1", "/dev/mapper/braid-disk1"]);

        let journal = bootstrap_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            // probe disk1 → PresentLuks
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            // passphrase FAILS
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                b"wrongpass".to_vec(),
                err_raw("cryptsetup open --test-passphrase", 2, "No key available"),
            )
            // Header probe after verify failure must classify disk1 as Ok so
            // the enrichment path falls through to the existing "wrong
            // passphrase" message rather than emitting the "diagnosis
            // incomplete" text.
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                ok_raw("cryptsetup isLuks", ""),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksDump".into(),
                    stdout: "LUKS header information\nVersion: 2\n".into(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_mapper_closed("braid-disk1");

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"wrongpass").unwrap();
        }

        let result = cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &f.recover_params()
                .passphrase_file(Some(passphrase_file.path()))
                .build(),
        );

        let err = result.expect_err("should fail with passphrase error");
        let msg = err.to_string();
        assert!(
            msg.contains("wrong passphrase"),
            "expected passphrase error, got: {msg}"
        );
        assert!(
            !msg.contains("bootstrap add was interrupted"),
            "must not show bootstrap message for passphrase error, got: {msg}"
        );

        // Journal must NOT have been cleared
        assert!(
            f.paths.pending_op_json().exists(),
            "journal should still exist"
        );
    }

    /// Intent: when a non-bootstrap recover hits a mount failure, the original
    ///   mount error propagates without bootstrap rewriting.
    ///
    /// Why it exists: the bootstrap detection must key off pre_membership being
    ///   empty. A non-empty pre_membership with a mount failure is a different
    ///   situation (e.g. damaged pool) that needs the real error.
    ///
    /// Scenario: 2-disk pool, interrupted add of disk3. All three disks absent.
    ///   Error should be the original "no unlockable disks", not bootstrap advice.
    #[test]
    fn recover_non_bootstrap_mount_failure_propagates() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[]); // all disks absent

        let journal = two_disk_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default().with_output(mp_req, mp_out);

        // Passphrase must be supplied even though no LUKS open will succeed:
        // cmd_recover reads the passphrase eagerly when the pool is not
        // already mounted so it has it on hand for the post-mount remount
        // cycle (see cmd_recover comment on the credential setup). The mount
        // still fails with "no unlockable disks" because fs has no by-id
        // paths, which is what this test pins down.
        let result = cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &f.recover_params().build(),
        );

        let err = result.expect_err("should fail with mount error");
        let msg = err.to_string();
        assert!(
            msg.contains("no unlockable disks"),
            "expected original mount error, got: {msg}"
        );
        assert!(
            !msg.contains("bootstrap"),
            "must not show bootstrap message for non-bootstrap case, got: {msg}"
        );

        // Journal must NOT have been cleared
        assert!(
            f.paths.pending_op_json().exists(),
            "journal should still exist"
        );
    }

    /// Intent: when a formatted bootstrap target carries btrfs, recover takes
    ///   the mount path, and a mount failure propagates as the original mount
    ///   error with the journal kept.
    ///
    /// Why it exists: mkfs may have succeeded but mount failed for another
    ///   reason (missing kernel module, bad options). Clearing the journal and
    ///   telling the operator to wipe the disk would destroy a valid
    ///   filesystem.
    ///
    /// Scenario: first-ever add of one disk. mkfs.btrfs succeeded, mount failed
    ///   for an unrelated reason. The bootstrap check finds btrfs on disk1, so
    ///   recover mounts, and the mount error is what the operator sees.
    #[test]
    fn recover_bootstrap_mount_fails_but_btrfs_exists_propagates_mount_error() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1"]);

        let journal = bootstrap_pool_mutation_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            // probe disk1 → PresentLuks
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            // passphrase ok
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            // LUKS open ok
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            // btrfs scan ok
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            // mount fails for non-btrfs reason
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                err_raw(
                    "mount",
                    32,
                    "mount(2) system call failed: Permission denied",
                ),
            )
            // btrfs probe confirms HasBtrfs — mkfs DID succeed
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk1".into(),
                },
                ok_raw(
                    "btrfs filesystem show",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 1 FS bytes used 256.00KiB\n\
                     \tdevid    1 size 10.00GiB used 536.00MiB path /dev/mapper/braid-disk1\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec!["/dev/mapper/braid-disk1".into()],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                ok_raw_empty("cryptsetup close"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                luks_dump_label("braid-disk1"),
            )
            .with_mapper_closed("braid-disk1");

        let result = cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &f.recover_params().build(),
        );

        let err = result.expect_err("should fail with original mount error");
        let msg = err.to_string();
        assert!(
            msg.contains("mount failed"),
            "expected original mount error, got: {msg}"
        );
        assert!(
            runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::Mount { .. })),
            "a target with btrfs must take the mount path"
        );

        // Journal must NOT have been cleared
        assert!(
            f.paths.pending_op_json().exists(),
            "journal should still exist"
        );
        assert!(
            runner.requests().iter().any(
                |r| matches!(r, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-disk1")
            ),
            "bootstrap mount failure with btrfs superblock should still cleanup opened mapper"
        );
    }

    // --- by_id resolver tests ---

    /// Intent: When the journal's recorded by_id is stale (e.g. cable swap or
    /// USB re-enumeration changed /dev/disk/by-id/ between the mutation start
    /// and recovery), the recovered pool.json must contain the live by-id
    /// path, not the stale journal value.
    ///
    /// Why: The previous code copied by_id from the journal snapshot, which
    /// could persist a path that no longer exists on disk. The next braid
    /// unlock would then fail to find the device.
    ///
    /// Scenario: 2-disk pool already mounted. Journal has the old paths
    /// (virtio-disk1/2). Resolver returns multiple new symlinks per device;
    /// recovery must persist the highest-priority by-id (wwn-) for each.
    #[test]
    fn recover_uses_live_by_id_when_journal_is_stale() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");

        let journal = committed_two_disk_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            // Idle gate + replay: post-Add recovery probes btrfs balance state
            // before the owed hard RAID1 convert.
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsBalanceRaid1 {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs balance start"),
            );

        // Resolver: each /dev/vdN has a wwn (highest priority), an ata
        // (lower priority), and a -part1 partition entry that must be filtered.
        let mut resolver = MockByIdResolver::default()
            .with_entries([
                "wwn-0xAAAA",
                "ata-FOO",
                "ata-FOO-part1",
                "wwn-0xBBBB",
                "ata-BAR",
                "ata-BAR-part1",
            ])
            .with_canonical("/dev/vda", "/dev/vda")
            .with_canonical("/dev/disk/by-id/wwn-0xAAAA", "/dev/vda")
            .with_canonical("/dev/disk/by-id/ata-FOO", "/dev/vda")
            .with_canonical("/dev/disk/by-id/ata-FOO-part1", "/dev/vda1")
            .with_canonical("/dev/vdb", "/dev/vdb")
            .with_canonical("/dev/disk/by-id/wwn-0xBBBB", "/dev/vdb")
            .with_canonical("/dev/disk/by-id/ata-BAR", "/dev/vdb")
            .with_canonical("/dev/disk/by-id/ata-BAR-part1", "/dev/vdb1");
        // Suppress unused-mut by re-borrowing through the binding.
        let _ = &mut resolver;

        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &f.recover_params().passphrase_file(None).build(),
        );

        result.expect("recover should succeed");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert_eq!(
            recovered
                .by_name(&disk_name("disk1"))
                .unwrap()
                .1
                .by_id
                .as_str(),
            "/dev/disk/by-id/wwn-0xAAAA",
            "disk1 should resolve to highest-priority wwn-, not stale journal value"
        );
        assert_eq!(
            recovered
                .by_name(&disk_name("disk2"))
                .unwrap()
                .1
                .by_id
                .as_str(),
            "/dev/disk/by-id/wwn-0xBBBB",
            "disk2 should resolve to highest-priority wwn-, not stale journal value"
        );
    }

    /// Intent: When a live pool device has no /dev/disk/by-id/ symlink
    /// resolving to it, recovery must hard-fail with an actionable error
    /// rather than silently fall back to a stale journal value.
    ///
    /// Why: Falling back to the journal would defeat the purpose of the fix
    /// in exactly the case where the journal value is most likely to be wrong.
    /// The operator needs a concrete remediation, not a guess.
    ///
    /// Scenario: Pool already mounted, journal known. Resolver returns no
    /// matching by-id entry. Recovery must fail loudly and not write pool.json.
    #[test]
    fn recover_hard_fails_when_underlying_has_no_by_id() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");

        let journal = committed_two_disk_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        // Resolver: only canonicalize the underlying paths to themselves; no
        // by-id entries match. resolve_by_id_for_underlying must hard-fail.
        let resolver = MockByIdResolver::default()
            .with_canonical("/dev/vda", "/dev/vda")
            .with_canonical("/dev/vdb", "/dev/vdb");

        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &f.recover_params().passphrase_file(None).build(),
        );

        let err = result.expect_err("recovery should hard-fail when no by-id resolves");
        let msg = err.to_string();
        assert!(
            msg.contains("has no /dev/disk/by-id/ symlink resolving to it"),
            "error should explain the missing-symlink condition, got: {msg}"
        );
        assert!(
            msg.contains("/dev/vda"),
            "error should name the concrete underlying device, got: {msg}"
        );
        assert!(
            msg.contains("udevadm info --query=symlink --name"),
            "error should suggest the udevadm remediation command, got: {msg}"
        );

        // pool.json must NOT have been written
        assert!(
            !f.paths.pool_json().exists(),
            "pool.json should not exist after failed recovery"
        );
        // journal must NOT have been cleared
        assert!(
            f.paths.pending_op_json().exists(),
            "journal should still exist after failed recovery"
        );
    }

    /// Intent: When several /dev/disk/by-id/ symlinks resolve to the same live
    /// device (the normal case for any SATA drive), the resolver must pick the
    /// most stable identifier per `by_id::by_id_priority`.
    ///
    /// Why: SATA drives normally expose wwn-, ata-, and scsi- aliases pointing
    /// at the same kernel device. We want pool.json to record the wwn (most
    /// stable across kernel/firmware changes), exactly like `discover --write`.
    #[test]
    fn resolve_by_id_picks_highest_priority_when_multiple_match() {
        let resolver = MockByIdResolver::default()
            .with_entries(["ata-Y", "scsi-Z", "wwn-X", "ata-OTHER"])
            .with_canonical("/dev/sda", "/dev/sda")
            .with_canonical("/dev/disk/by-id/wwn-X", "/dev/sda")
            .with_canonical("/dev/disk/by-id/scsi-Z", "/dev/sda")
            .with_canonical("/dev/disk/by-id/ata-Y", "/dev/sda")
            .with_canonical("/dev/disk/by-id/ata-OTHER", "/dev/sdb");

        let resolved =
            resolve_by_id_for_underlying(&resolver, "/dev/sda").expect("resolution should succeed");
        assert_eq!(
            resolved.as_str(),
            "/dev/disk/by-id/wwn-X",
            "wwn- has highest priority and must win"
        );
    }

    /// Intent: by-id partition entries (e.g. ata-FOO-part1) must be filtered
    /// out, even when their canonical target matches the live device.
    ///
    /// Why: braid uses whole-disk LUKS, never partition LUKS. Picking a
    /// partition entry would record a misleading path in pool.json.
    #[test]
    fn resolve_by_id_skips_partition_entries() {
        let resolver = MockByIdResolver::default()
            .with_entries(["ata-FOO", "ata-FOO-part1", "ata-FOO-part2"])
            .with_canonical("/dev/sda", "/dev/sda")
            .with_canonical("/dev/disk/by-id/ata-FOO", "/dev/sda")
            .with_canonical("/dev/disk/by-id/ata-FOO-part1", "/dev/sda")
            .with_canonical("/dev/disk/by-id/ata-FOO-part2", "/dev/sda");

        let resolved =
            resolve_by_id_for_underlying(&resolver, "/dev/sda").expect("resolution should succeed");
        assert_eq!(
            resolved.as_str(),
            "/dev/disk/by-id/ata-FOO",
            "partition entries must be filtered, leaving only the whole-disk by-id"
        );
    }

    // --- recovery_guidance tests ---

    fn set_of(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn guidance_add_completed() {
        let pre = set_of(&["disk1", "disk2"]);
        let target = set_of(&["disk1", "disk2", "disk3"]);
        let recovered = set_of(&["disk1", "disk2", "disk3"]);
        let mut add_targets_by_name = BTreeMap::new();
        add_targets_by_name.insert(
            "disk3".to_owned(),
            ByIdPath::parse("/dev/disk/by-id/x").unwrap(),
        );
        let op = add_op_from_disks(add_targets_by_name);

        assert_eq!(
            recovery_guidance(&op, &pre, &target, &recovered),
            "add completed -- 'disk3' now in the pool."
        );
    }

    #[test]
    fn guidance_add_rolled_back() {
        let pre = set_of(&["disk1", "disk2"]);
        let target = set_of(&["disk1", "disk2", "disk3"]);
        let recovered = set_of(&["disk1", "disk2"]);
        let mut add_targets_by_name = BTreeMap::new();
        add_targets_by_name.insert(
            "disk3".to_owned(),
            ByIdPath::parse("/dev/disk/by-id/x").unwrap(),
        );
        let op = add_op_from_disks(add_targets_by_name);

        assert_eq!(
            recovery_guidance(&op, &pre, &target, &recovered),
            "add did not complete -- 'disk3' not in the pool. Re-run braid add to retry."
        );
    }

    #[test]
    fn guidance_remove_completed() {
        let pre = set_of(&["disk1", "toshiba"]);
        let target = set_of(&["disk1"]);
        let recovered = set_of(&["disk1"]);
        let op = OpKind::Remove {
            luks_uuid: uuid_for_name("toshiba"),
            name: disk_name("toshiba"),
        };

        assert_eq!(
            recovery_guidance(&op, &pre, &target, &recovered),
            "remove completed -- 'toshiba' is no longer in the pool."
        );
    }

    #[test]
    fn guidance_remove_rolled_back() {
        let pre = set_of(&["disk1", "toshiba"]);
        let target = set_of(&["disk1"]);
        let recovered = set_of(&["disk1", "toshiba"]);
        let op = OpKind::Remove {
            luks_uuid: uuid_for_name("toshiba"),
            name: disk_name("toshiba"),
        };

        assert_eq!(
            recovery_guidance(&op, &pre, &target, &recovered),
            "remove did not complete -- 'toshiba' is still in the pool. Re-run braid remove to retry."
        );
    }

    #[test]
    fn guidance_remove_missing_completed() {
        let pre = set_of(&["disk1", "disk2"]);
        let target = set_of(&["disk1"]);
        let recovered = set_of(&["disk1"]);
        let op = OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PoolMutation,
            devid: Devid::new(2),
            restore_raid1_after_commit: true,
        };

        assert_eq!(
            recovery_guidance(&op, &pre, &target, &recovered),
            "remove-missing completed -- missing device removed from the pool."
        );
    }

    #[test]
    fn guidance_remove_missing_rolled_back() {
        let pre = set_of(&["disk1", "disk2"]);
        let target = set_of(&["disk1"]);
        let recovered = set_of(&["disk1", "disk2"]);
        let op = OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PoolMutation,
            devid: Devid::new(2),
            restore_raid1_after_commit: true,
        };

        assert_eq!(
            recovery_guidance(&op, &pre, &target, &recovered),
            "remove-missing did not complete -- device still in the pool. Re-run braid remove-missing to retry."
        );
    }

    #[test]
    fn guidance_replace_completed() {
        let pre = set_of(&["disk1", "old"]);
        let target = set_of(&["disk1", "new"]);
        let recovered = set_of(&["disk1", "new"]);
        let op = OpKind::Replace(journal::ReplaceOp {
            phase: journal::ReplacePhase::PoolMutation,
            identity: journal::ReplaceIdentity::new(uuid_for_name("old"), uuid_for_name("new"))
                .expect("distinct replace UUIDs"),
            old_name: disk_name("old"),
            new_name: disk_name("new"),
            new_target: journal::ReplaceJournalTarget {
                by_id: ByIdPath::parse("/dev/disk/by-id/x").unwrap(),
                mode: journal::ReplaceJournalMode::ExistingLuks {
                    enroll_key_file: None,
                },
            },
            source: journal::ReplaceJournalSource::Live {
                old_devid: Devid::new(2),
                old_mapper: MapperName::from_basename("braid-old".into()),
            },
            restore_raid1_after_commit: false,
        });

        assert_eq!(
            recovery_guidance(&op, &pre, &target, &recovered),
            "replace completed -- 'old' replaced by 'new'."
        );
    }

    #[test]
    fn guidance_replace_rolled_back() {
        let pre = set_of(&["disk1", "old"]);
        let target = set_of(&["disk1", "new"]);
        let recovered = set_of(&["disk1", "old"]);
        let op = OpKind::Replace(journal::ReplaceOp {
            phase: journal::ReplacePhase::PoolMutation,
            identity: journal::ReplaceIdentity::new(uuid_for_name("old"), uuid_for_name("new"))
                .expect("distinct replace UUIDs"),
            old_name: disk_name("old"),
            new_name: disk_name("new"),
            new_target: journal::ReplaceJournalTarget {
                by_id: ByIdPath::parse("/dev/disk/by-id/x").unwrap(),
                mode: journal::ReplaceJournalMode::ExistingLuks {
                    enroll_key_file: None,
                },
            },
            source: journal::ReplaceJournalSource::Live {
                old_devid: Devid::new(2),
                old_mapper: MapperName::from_basename("braid-old".into()),
            },
            restore_raid1_after_commit: false,
        });

        assert_eq!(
            recovery_guidance(&op, &pre, &target, &recovered),
            "replace did not complete -- pool still has 'old'. Re-run braid replace to retry."
        );
    }

    #[test]
    fn guidance_partial() {
        let pre = set_of(&["disk1", "disk2"]);
        let target = set_of(&["disk1", "disk2", "disk3"]);
        let recovered = set_of(&["disk1", "disk3"]);
        let mut add_targets_by_name = BTreeMap::new();
        add_targets_by_name.insert(
            "disk3".to_owned(),
            ByIdPath::parse("/dev/disk/by-id/x").unwrap(),
        );
        let op = add_op_from_disks(add_targets_by_name);

        assert_eq!(
            recovery_guidance(&op, &pre, &target, &recovered),
            "pool membership does not match the pre-operation or target state. \
             Run braid status and decide whether to re-run the operation."
        );
    }

    // Intent: pin the `braid recover` entry-banner literal so the
    //   `{:?}` formatting of the lowercase op label cannot drift
    //   silently from what `docs/commands/recover.md` shows.
    // Why it exists: docs/commands/recover.md previously claimed
    //   `Recovering from interrupted Add operation ...` while the
    //   real output was `Recovering from interrupted "add" operation
    //   ...` (quoted lowercase). The VM substring assertion at
    //   tests/cli/braid-recover.py only checks the `"Recovering from
    //   interrupted"` prefix, so the drift went unnoticed until a
    //   doc audit caught it.
    // Scenario: format a journal for each of the four op kinds and
    //   compare against the exact stderr line a real recover run
    //   prints to operators.
    #[test]
    fn format_recover_entry_pins_banner_for_each_op_kind() {
        let started_at = "2026-03-15T14:30:00Z";

        let mut add_targets_by_name = BTreeMap::new();
        add_targets_by_name.insert(
            "disk3".to_owned(),
            ByIdPath::parse("/dev/disk/by-id/x").unwrap(),
        );
        let add_op = add_op_from_disks(add_targets_by_name);

        let remove_op = OpKind::Remove {
            luks_uuid: uuid_for_name("toshiba"),
            name: disk_name("toshiba"),
        };

        let remove_missing_op = OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PoolMutation,
            devid: Devid::new(2),
            restore_raid1_after_commit: true,
        };

        let replace_op = OpKind::Replace(journal::ReplaceOp {
            phase: journal::ReplacePhase::PoolMutation,
            identity: journal::ReplaceIdentity::new(uuid_for_name("old"), uuid_for_name("new"))
                .expect("distinct replace UUIDs"),
            old_name: disk_name("old"),
            new_name: disk_name("new"),
            new_target: journal::ReplaceJournalTarget {
                by_id: ByIdPath::parse("/dev/disk/by-id/x").unwrap(),
                mode: journal::ReplaceJournalMode::ExistingLuks {
                    enroll_key_file: None,
                },
            },
            source: journal::ReplaceJournalSource::Live {
                old_devid: Devid::new(2),
                old_mapper: MapperName::from_basename("braid-old".into()),
            },
            restore_raid1_after_commit: false,
        });

        // Literal, not format!(...{:?}...) -- the quotes ARE the contract;
        // re-deriving them with the impl's own expression would let a
        // {:?}->{} cleanup pass. The `add` literal is byte-identical to
        // docs/commands/recover.md.
        let cases = [
            (
                add_op,
                "Recovering from interrupted \"add\" operation (started 2026-03-15T14:30:00Z)...",
            ),
            (
                remove_op,
                "Recovering from interrupted \"remove\" operation (started 2026-03-15T14:30:00Z)...",
            ),
            (
                remove_missing_op,
                "Recovering from interrupted \"remove-missing\" operation (started 2026-03-15T14:30:00Z)...",
            ),
            (
                replace_op,
                "Recovering from interrupted \"replace\" operation (started 2026-03-15T14:30:00Z)...",
            ),
        ];

        for (op, expected) in cases {
            let journal = journal::Journal {
                started_at: started_at.to_owned(),
                op,
                pre_membership: PoolMembership::empty(),
                target_membership: PoolMembership::empty(),
            };
            assert_eq!(format_recover_entry(&journal), expected);
        }
    }

    // ----- M1 (Pre-M11 remediation) tests -----

    /// Two-device journal modeling an interrupted Replace: pre = {disk1, old},
    /// target = {disk1, new}. The replace went through at the kernel level
    /// (the live pool reports {disk1, new} on the new mapper) but shutdown hit
    /// before braid could re-issue `pool_resize_device`.
    fn replace_journal() -> journal::Journal {
        let pre = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "old",
                "/dev/disk/by-id/virtio-old",
                None,
                Some(Devid::new(2)),
            ),
        ]);
        let target = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry("new", "/dev/disk/by-id/virtio-new", None, None),
        ]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Replace(journal::ReplaceOp {
                phase: journal::ReplacePhase::PoolMutation,
                identity: journal::ReplaceIdentity::new(uuid_for_name("old"), uuid_for_name("new"))
                    .expect("distinct replace UUIDs"),
                old_name: disk_name("old"),
                new_name: disk_name("new"),
                new_target: journal::ReplaceJournalTarget {
                    by_id: ByIdPath::parse("/dev/disk/by-id/virtio-new").unwrap(),
                    mode: journal::ReplaceJournalMode::ExistingLuks {
                        enroll_key_file: None,
                    },
                },
                source: journal::ReplaceJournalSource::Live {
                    old_devid: Devid::new(2),
                    old_mapper: MapperName::from_basename("braid-old".into()),
                },
                restore_raid1_after_commit: false,
            }),
            pre_membership: pre,
            target_membership: target,
        }
    }

    fn replace_journal_in_phase(
        phase: journal::ReplacePhase,
        restore_raid1_after_commit: bool,
        source: journal::ReplaceJournalSource,
    ) -> journal::Journal {
        let mut journal = replace_journal();
        let OpKind::Replace(journal::ReplaceOp {
            phase: stored_phase,
            source: stored_source,
            restore_raid1_after_commit: stored_restore,
            ..
        }) = &mut journal.op
        else {
            unreachable!("replace_journal returns Replace");
        };
        *stored_phase = phase;
        *stored_source = source;
        *stored_restore = restore_raid1_after_commit;
        journal
    }

    fn replace_post_maintenance_journal(
        restore_raid1_after_commit: bool,
        source: journal::ReplaceJournalSource,
    ) -> journal::Journal {
        let mut journal = replace_journal_in_phase(
            journal::ReplacePhase::PostReplaceMaintenance,
            restore_raid1_after_commit,
            source,
        );
        let new_uuid = uuid_for_name("new");
        let new_member = journal
            .target_membership
            .by_uuid(&new_uuid)
            .expect("replace target fixture member")
            .clone();
        journal
            .target_membership
            .remove_by_uuid(&new_uuid)
            .expect("replace target fixture member");
        journal
            .target_membership
            .insert(
                new_uuid,
                DiskMember {
                    devid: Some(Devid::new(2)),
                    ..new_member
                },
            )
            .expect("post-maintenance target enrichment");
        journal
    }

    fn replace_fresh_luks_journal(enroll_key_file: std::path::PathBuf) -> journal::Journal {
        let mut journal = replace_journal();
        let OpKind::Replace(journal::ReplaceOp {
            new_target,
            restore_raid1_after_commit,
            ..
        }) = &mut journal.op
        else {
            unreachable!("replace_journal returns Replace");
        };
        *new_target = journal::ReplaceJournalTarget {
            by_id: ByIdPath::parse("/dev/disk/by-id/virtio-new").unwrap(),
            mode: journal::ReplaceJournalMode::FreshLuks {
                extra_opts: LuksFormatExtraOpts::default(),
                enroll_key_file: Some(KeyFilePath::new(enroll_key_file)),
            },
        };
        *restore_raid1_after_commit = false;
        journal
    }

    /// Existing-LUKS replace journal with `enroll_key_file: Some(kf)`.
    /// Models the silent-drop bug fix: `replace --enroll DIR` against a
    /// pre-formatted target and slot 1 was empty, so the planner picked
    /// `NeedsEnroll` and the journal carries the keyfile for replay.
    fn replace_existing_luks_with_enroll_journal(
        enroll_key_file: std::path::PathBuf,
    ) -> journal::Journal {
        let mut journal = replace_journal();
        let OpKind::Replace(journal::ReplaceOp { new_target, .. }) = &mut journal.op else {
            unreachable!("replace_journal returns Replace");
        };
        if let journal::ReplaceJournalMode::ExistingLuks {
            enroll_key_file: stored,
            ..
        } = &mut new_target.mode
        {
            *stored = Some(KeyFilePath::new(enroll_key_file));
        } else {
            unreachable!("replace_journal returns ExistingLuks");
        }
        journal
    }

    /// btrfs filesystem show for the post-replace pool: disk1 (devid 1) + new
    /// (devid 2). The "new" mapper is what
    /// `execute_replace_post_maintenance_recovery` keys off to resolve the new
    /// device's devid.
    fn btrfs_show_disk1_and_new() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-new\n",
        )
    }

    fn btrfs_show_disk1_and_old() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-old\n",
        )
    }

    // Intent: a suspended kernel dev_replace aborts recover before the
    // remount cycle and keeps the pending journal intact.
    // Why it exists: suspended replace is still kernel-ongoing; clearing
    // pending-op.json would remove braid's only structured recovery context
    // while a fresh replace start is still blocked.
    // Scenario: power returns after a replace target disappeared; recover
    // opens the pool, observes "suspended on" at 50%, and tells the operator
    // to cancel the kernel operation manually before retrying.
    #[test]
    fn recover_aborts_and_preserves_journal_on_suspended_replace() {
        let f = PoolFixture::empty();

        let journal = replace_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let inner = MockRunner::default()
            .with_output(mountpoint_fail().0, mountpoint_fail().1)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-old",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    mapper: MapperName::from_basename("braid-new".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-old".into(),
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            .with_output(
                CmdRequest::BtrfsReplaceStatus {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw(
                    "btrfs replace status",
                    "Started on 27.Feb 10:30:00, suspended on 27.Feb 10:35:00 at 50.0%, \
                     0 write errs, 0 uncorr. read errs\n",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-old",
                "/dev/disk/by-id/virtio-new",
            ]);

        let harness = RemountHarness::new(
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-old",
                "/dev/disk/by-id/virtio-new",
            ],
            inner,
            &["braid-disk1", "braid-new", "braid-old"],
        );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let params = f.recover_params().build();

        let report = plan_recover(&harness.runner, &harness.fs, &params);
        let plan = report.expect("recover planning should succeed");
        let result = plan.execute(&harness.runner, &harness.fs, &resolver, &params);

        let err = result.expect_err("suspended replace should abort recover");
        let msg = err.to_string();
        assert!(
            msg.contains("suspended at 50.0%"),
            "error should include suspended progress, got: {msg}"
        );
        assert!(
            msg.contains("btrfs replace cancel"),
            "error should include manual cancel command, got: {msg}"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "journal should remain after suspended replace abort"
        );

        let requests = harness.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::Umount { .. }
                    | CmdRequest::BtrfsDeviceScanForget { .. }
                    | CmdRequest::CryptsetupClose { .. }
            )),
            "suspended replace must abort before relock_and_remount: {requests:?}"
        );
    }

    /// Intent: After kernel-resumed `btrfs replace` finishes during recover,
    /// `cmd_recover` MUST re-issue `btrfs filesystem resize <new_devid>:max`
    /// before clearing the journal -- otherwise the new disk reports the old
    /// disk's smaller size and capacity is silently lost.
    ///
    /// Why it exists: This closes the GAP B identified in the Pre-M11 audit.
    /// The original replace command runs `pool_resize_device` immediately
    /// after `pool_replace_device`; a
    /// forced shutdown landing between those two calls would leave the new
    /// disk under-sized and `recover` previously had no replay logic for it.
    /// The live VM matrix (M3) needs this fix to reliably assert "final
    /// device layout matches the requested replacement".
    ///
    /// Path: this test exercises the `just_mounted == true` cycle path --
    /// recover opens the mount itself, then runs
    /// `wait_for_kernel_replace_to_finish` + `relock_and_remount` to scrub
    /// any kernel-resumed-dev_replace staleness, then probes and replays
    /// the resize. The previously-existing `mountpoint_ok()` (already-mounted)
    /// variant of this test is intentionally absent: that path is now
    /// refused at the planner level for OpKind::Replace
    /// (`plan_recover_refuses_replace_on_externally_mounted_pool` pins the
    /// refusal).
    ///
    /// Scenario: Operator started `braid replace old new` against a pool that
    /// finished the kernel-side dev_replace under UPS battery, then power
    /// dropped before resize. Pool comes up unmounted (auto-unlock blocked
    /// by the pending-op preflight); recover opens the mount, finishes the
    /// kernel resume + cycle, sees the new device on devid 2, and resizes
    /// it as part of replaying the post-mutation steps before clearing the
    /// journal.
    #[test]
    fn recover_replays_resize_after_replace_via_mount_cycle() {
        let f = PoolFixture::empty();

        let journal = replace_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let inner = MockRunner::default()
            // ── Initial plan_open_pool ──────────────────────────────────
            // mountpoint check → not mounted → plan_open_pool returns
            // Some(open_plan) and recover takes the just_mounted == true
            // path.
            .with_output(mountpoint_fail().0, mountpoint_fail().1)
            // probe each union member's LUKS UUID (mapper closed → mapper_open=false).
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-old",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            // ── Initial execute_unlock_and_mount ────────────────────────
            // Per-disk verify-passphrase + open. Order is BTreeMap-alphabetical:
            // disk1, new, old.
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    mapper: MapperName::from_basename("braid-new".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-old".into(),
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                TEST_PASSPHRASE_BYTES.to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            // No missing members in the union → plain Mount, not
            // MountWithOptions. mount_device is the first to_unlock entry
            // (alphabetical → braid-disk1).
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            // ── wait_for_kernel_replace_to_finish ───────────────────────
            // Realistic post-resume status: Finished. The parser routes
            // "finished on" to ReplaceState::Finished and the wait loop
            // returns immediately.
            .with_output(
                CmdRequest::BtrfsReplaceStatus {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw(
                    "btrfs replace status",
                    "Started on 27.Feb 10:30:00, finished on 27.Feb 10:35:00, \
                     0 write errs, 0 uncorr. read errs\n",
                ),
            )
            // ── relock_and_remount cycle ────────────────────────────────
            // 1. Umount.
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("umount"),
            )
            // 2. scan --forget -- pool-scoped to the union mappers.
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-old".into(),
                        "/dev/mapper/braid-new".into(),
                    ],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            // 3. Close each union mapper. RemountRunner removes the
            //    mapper path from RemountFs and adds the name to its
            //    `closed` set after each successful close.
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                ok_raw_empty("cryptsetup close braid-disk1"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-new".into()),
                },
                ok_raw_empty("cryptsetup close braid-new"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                ok_raw_empty("cryptsetup close braid-old"),
            )
            // 4. Cycle re-plan: mountpoint check + LuksUuid mocks reused
            //    via MockRunner's HashMap lookup. Mappers report inactive
            //    via RemountRunner's `closed` set. TestPassphrase +
            //    LuksOpen mocks above are reused for the cycle reopen.
            // 5. Cycle execute mount: same Mount mock as above.
            // ── Post-cycle probe_pool ───────────────────────────────────
            // The fix-state topology: 2 devices (disk1 + new), no phantom
            // MISSING. This is what btrfs_show_disk1_and_new() returns.
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_disk1_and_new(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-new".into()),
                },
                cryptsetup_status_active("braid-new", "/dev/vdc"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into(),
                },
                cryptsetup_uuid_ok("/dev/vdc", "33333333-3333-3333-3333-333333333333"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                cryptsetup_status_active("braid-old", "/dev/disk/by-id/virtio-old"),
            )
            // ── execute_replace_post_maintenance_recovery ───────────────
            // Resize-to-max on the new device's devid (2). Load-bearing
            // assertion: without this mock the test fails with MissingMock,
            // proving recover actually issued the resize.
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: Devid::new(2),
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            )
            // LUKS dump used by probe_config_disk to classify each by-id
            // path as a real LUKS device.
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-old",
                "/dev/disk/by-id/virtio-new",
            ]);
        // Note: BtrfsBalanceStatus and BtrfsBalanceRaid1Soft are NOT mocked.
        // This live-source replace does not owe post-commit RAID1 maintenance,
        // so any balance replay would fail with MissingMock.

        // RemountFs starts with by-id paths for the union {disk1, old,
        // new}. No mapper paths -- everything starts closed.
        // RemountRunner adds mapper paths after each successful
        // CryptsetupLuksOpen and removes them after each CryptsetupClose.
        // All three union mappers start closed: probe_mapper_open reports
        // inactive via the harness's `closed`-set fast path, so
        // plan_open_pool builds a non-empty to_unlock and the initial
        // mount runs LuksOpen for each. Successful LuksOpen removes the
        // entry from `closed` and adds the mapper path to fs; the cycle's
        // Close re-adds the entry and removes the mapper path; the cycle's
        // LuksOpen reverses again.
        let harness = RemountHarness::new(
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-old",
                "/dev/disk/by-id/virtio-new",
            ],
            inner,
            &["braid-disk1", "braid-new", "braid-old"],
        )
        .with_luks_dm("braid-old", "22222222-2222-2222-2222-222222222222")
        .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
        .with_luks_dm("braid-new", "33333333-3333-3333-3333-333333333333");

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let result = cmd_recover(
            &harness.runner,
            &harness.fs,
            &resolver,
            &f.recover_params().build(),
        );

        result.expect("recover should succeed via the mount cycle and replay the resize");
        let requests = harness.requests();

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(
            recovered.by_name(&disk_name("disk1")).is_some()
                && recovered.by_name(&disk_name("new")).is_some(),
            "recovered membership should match the post-replace target"
        );
        assert!(
            recovered.by_name(&disk_name("old")).is_none(),
            "old disk must not appear in the post-replace membership"
        );

        assert!(
            !f.paths.pending_op_json().exists(),
            "journal must be cleared after a successful resize replay"
        );
        let old_close_count = requests
            .iter()
            .filter(|r| {
                matches!(r, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-old")
            })
            .count();
        assert_eq!(
            old_close_count, 2,
            "braid-old should close once in the cycle and once in post-maintenance"
        );
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsBalanceStatus { .. } | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "live-source replace recovery must not replay unowed balance work: {requests:?}"
        );
    }

    // Intent: RecoverWorkAction::WaitForKernelReplace.execute returns
    // Ok(false) without touching the runner when state.just_mounted is false.
    //
    // Why it exists: The `if state.just_mounted` gate in
    // `RecoverWorkAction::execute` is defense-in-depth on top of
    // `plan_recover`'s already-mounted refusal (pinned by
    // `plan_recover_refuses_replace_on_externally_mounted_pool`) and its
    // open-plan presence gate. Without this test, a
    // regression that flips the gate (`if !state.just_mounted`) or removes it
    // would compile and pass `just test-rust`, leaving production safety
    // dependent solely on the planner refusal.
    //
    // Scenario: TOCTOU window -- plan_recover saw an unmounted pool and
    // produced `open_plan: Some(_)`, but by execute time the mount call
    // observed the pool already mounted and returned `Ok(false)`, so
    // `state.just_mounted` ended up false. WaitForKernelReplace must not
    // probe `btrfs replace status` on a mount session we did not open.
    #[test]
    fn wait_for_kernel_replace_no_ops_when_just_mounted_false() {
        let f = PoolFixture::empty();
        let mut plan = recover_work_plan_for_journal(replace_journal());
        plan.open_plan = Some(OpenPlan {
            to_unlock: Vec::new(),
            any_missing_member: false,
            mount_device: String::new(),
        });

        let mut state = RecoverExecutionState {
            credential: None,
            just_mounted: false,
        };

        let runner = MockRunner::default();
        let fs = MockFs::new(&[]);
        let resolver = resolver_for(&[]);
        let params = f.recover_params().build();

        let result = RecoverWorkAction::WaitForKernelReplace
            .execute(&plan, &mut state, &runner, &fs, &resolver, &params);

        assert!(matches!(result, Ok(false)), "unexpected result: {result:?}");
        assert!(
            runner.requests().is_empty(),
            "expected no runner activity, got: {:?}",
            runner.requests()
        );
    }

    // Intent: RecoverWorkAction::WaitForKernelReplace.execute uses the
    //   sleeper seam supplied by RecoverParams while polling kernel replace.
    // Why it exists: the action previously constructed RealSleeper inline,
    //   making tests burn wall-clock time and leaving the injected seam unused.
    // Scenario: recover just mounted a replace journal; replace status reports
    //   Running once and Finished on the next poll, so exactly one sleep is
    //   recorded by the injected sleeper.
    #[test]
    fn wait_for_kernel_replace_action_honors_injected_sleeper() {
        let f = PoolFixture::empty();
        let mut plan = recover_work_plan_for_journal(replace_journal());
        plan.open_plan = Some(OpenPlan {
            to_unlock: Vec::new(),
            any_missing_member: false,
            mount_device: String::new(),
        });

        let mut state = RecoverExecutionState {
            credential: None,
            just_mounted: true,
        };

        let runner = MockRunner::default().with_output_sequence(
            CmdRequest::BtrfsReplaceStatus {
                mount_point: MountPoint::new("/mnt/storage".into()),
            },
            vec![
                ok_raw(
                    "btrfs replace status -1 /mnt/storage",
                    "5.0% done, 0 write errs, 0 uncorr. read errs\n",
                ),
                ok_raw(
                    "btrfs replace status -1 /mnt/storage",
                    "Started on 27.Feb 10:30:00, finished on 27.Feb 10:35:00, 0 write errs, 0 uncorr. read errs\n",
                ),
            ],
        );
        let fs = MockFs::new(&[]);
        let resolver = resolver_for(&[]);
        let sleeper = RecordingSleeper::default();
        let params = f.recover_params().sleeper(&sleeper).build();

        let result = RecoverWorkAction::WaitForKernelReplace
            .execute(&plan, &mut state, &runner, &fs, &resolver, &params);

        assert!(matches!(result, Ok(false)), "unexpected result: {result:?}");
        assert_eq!(
            sleeper.calls(),
            vec![REPLACE_WAIT_POLL_INTERVAL],
            "replace polling must sleep through RecoverParams.sleeper"
        );
    }

    // Intent: RecoverWorkAction::RemountCycle.execute returns Ok(false)
    // without touching the runner when state.just_mounted is false.
    //
    // Why it exists: Same defense-in-depth pattern as
    // WaitForKernelReplace -- the `if state.just_mounted` gate in
    // `RecoverWorkAction::execute` guards relock_and_remount (umount +
    // scan-forget + LUKS close+reopen + remount), all backstopped by
    // `plan_recover`'s open-plan presence gate. A regression
    // here would attempt to umount a foreign mount session.
    //
    // Scenario: Same TOCTOU window as the WaitForKernelReplace no-op
    // test. The remount cycle must not run when recover did not open the
    // mount itself.
    #[test]
    fn remount_cycle_no_ops_when_just_mounted_false() {
        let f = PoolFixture::empty();
        let mut plan = recover_work_plan_for_journal(replace_journal());
        plan.open_plan = Some(OpenPlan {
            to_unlock: Vec::new(),
            any_missing_member: false,
            mount_device: String::new(),
        });

        let mut state = RecoverExecutionState {
            credential: None,
            just_mounted: false,
        };

        let runner = MockRunner::default();
        let fs = MockFs::new(&[]);
        let resolver = resolver_for(&[]);
        let params = f.recover_params().build();

        let result = RecoverWorkAction::RemountCycle {
            close_names: vec![disk_name("disk1")],
            reopen_names: vec![disk_name("disk1")],
            any_missing_member: false,
        }
        .execute(&plan, &mut state, &runner, &fs, &resolver, &params);

        assert!(matches!(result, Ok(false)), "unexpected result: {result:?}");
        assert!(
            runner.requests().is_empty(),
            "expected no runner activity, got: {:?}",
            runner.requests()
        );
    }

    fn committed_add_recover_runner(balance_status: Option<RawCommandOutput>) -> MockRunner {
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );
        if let Some(output) = balance_status {
            runner.with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                output,
            )
        } else {
            runner
        }
    }

    fn balance_status_paused_output() -> RawCommandOutput {
        RawCommandOutput {
            cmd: "btrfs balance status".into(),
            stdout: "Balance on '/mnt/storage' is paused\n\
                     0 out of about 0 chunks balanced (0 considered), -nan% left\n"
                .into(),
            stderr: String::new(),
            exit_status: 1,
        }
    }

    fn balance_status_running_output() -> RawCommandOutput {
        RawCommandOutput {
            cmd: "btrfs balance status".into(),
            stdout: "Balance on '/mnt/storage' is running\n\
                     3 out of about 10 chunks balanced (7 considered), 70% left\n"
                .into(),
            stderr: String::new(),
            exit_status: 1,
        }
    }

    // Intent: owed RAID1 recovery fails closed for paused, running, and unknown
    // btrfs balance states.
    // Why it exists: replaying owed RAID1 maintenance on top of a
    // crash-paused or otherwise non-idle balance can make the kernel underflow
    // block-group accounting, so recover must preserve the journal for manual
    // inspection instead of resuming or starting balance work.
    // Scenario: an add committed its membership and crashed before post-add
    // RAID1 maintenance finished; next-boot recover sees a non-idle or
    // indeterminate balance state after repairing pool.json.
    #[test]
    fn recover_owed_raid1_non_idle_balance_fails_closed_and_preserves_journal() {
        let cases = [
            ("paused", Some(balance_status_paused_output())),
            ("running", Some(balance_status_running_output())),
            ("unknown", None),
        ];

        for (state, balance_status) in cases {
            let f = PoolFixture::empty();
            let fs = MockFs::new(&[])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");
            let journal = committed_two_disk_add_journal();
            journal::write_journal(&f.paths, &journal).unwrap();

            let runner = committed_add_recover_runner(balance_status);
            let resolver =
                resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
            let err = cmd_recover(
                &runner,
                &fs,
                &resolver,
                &f.recover_params().passphrase_file(None).build(),
            )
            .unwrap_err();

            let msg = err.to_string();
            let expected_state_text = if state == "unknown" {
                "could not determine btrfs balance state"
            } else {
                state
            };
            assert!(
                msg.contains(expected_state_text) && msg.contains("preserving pending-op.json"),
                "unexpected {state} error: {msg}"
            );
            let recovered = membership::load_membership(&f.paths).unwrap();
            assert!(recovered.by_name(&disk_name("disk1")).is_some());
            assert!(recovered.by_name(&disk_name("disk2")).is_some());
            assert!(
                f.paths.pending_op_json().exists(),
                "{state} balance state must preserve pending-op.json"
            );
            let requests = runner.requests();
            assert!(
                !requests.iter().any(|r| matches!(
                    r,
                    CmdRequest::BtrfsBalanceRaid1 { .. } | CmdRequest::BtrfsBalanceRaid1Soft { .. }
                )),
                "{state} balance state must fail before RAID1 replay: {requests:?}"
            );
            assert!(
                requests
                    .iter()
                    .any(|r| matches!(r, CmdRequest::BtrfsBalanceStatus { .. })),
                "{state} balance state should probe btrfs balance status"
            );
        }
    }

    /// Two-disk Remove journal modeling an interrupted 2->1 remove: pre =
    /// {disk1, disk2}, target = {disk1}. Shutdown landed during the
    /// pre-remove `convert_to_one_disk`, so the live pool still has both
    /// disks but the kernel has a paused convert-to-single balance.
    fn remove_2to1_journal() -> journal::Journal {
        let pre = membership_from(vec![
            membership_entry("disk1", "/dev/disk/by-id/virtio-disk1", None, None),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);
        let target = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            None,
        )]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Remove {
                luks_uuid: uuid_for_name("disk2"),
                name: disk_name("disk2"),
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    /// Intent: When `cmd_recover` finds a paused balance after rebuilding
    /// pool.json from an interrupted `OpKind::Remove`, it MUST NOT auto-resume
    /// it. The operator's recovery path is to re-run `braid remove`, which
    /// re-issues the convert itself.
    ///
    /// Why it exists: a 2->1 `braid remove` converts to `ONE_DISK_PROFILES`
    /// (via `convert_to_one_disk`) before its device remove. A shutdown
    /// landing during that pre-balance leaves the kernel with a paused convert
    /// against a still-2-disk pool. If `replay_owed_raid1_maintenance` ran on
    /// this remove path, recover could finish the conversion to single without
    /// ever removing the device, then clear the journal, silently halving
    /// redundancy. The matrix test `ups-lb-during-remove` only
    /// exercises a 3->2 remove, so this unit test is the regression guard
    /// for the 2->1 pre-balance path.
    ///
    /// Scenario: Operator started `braid remove disk2` against a 2-disk
    /// RAID1 pool; UPS LB fired during the pre-remove `convert_to_one_disk`.
    /// Pool comes up with both disks still present and a paused balance.
    /// Recover writes the recovered membership ({disk1, disk2} = pre), skips
    /// balance probing and replay, clears the journal, and prints guidance to
    /// re-run `braid remove`.
    #[test]
    fn recover_skips_balance_replay_for_remove() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");

        let journal = remove_2to1_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            // mountpoint check -> already mounted (skips the mount cycle)
            .with_output(mp_req, mp_out)
            // probe_pool path -- live pool still has both disks because the
            // pre-remove balance was in flight when shutdown hit; the device
            // was never removed.
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );
        // Note: BtrfsBalanceStatus and BtrfsBalanceRaid1Soft are NOT mocked. If
        // the runtime gate for OpKind::Remove regresses and recover calls
        // replay_owed_raid1_maintenance, the test fails with MissingMock,
        // proving recover correctly leaves the paused balance alone for the
        // remove path.

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let inhibitor = RequestCountInhibitor::new(runner.clone());
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &f.recover_params()
                .passphrase_file(None)
                .sleep_inhibitor(&inhibitor)
                .build(),
        );

        result.expect("recover should succeed without resuming the paused remove balance");
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "remove recovery without owed RAID1 replay must not acquire a sleep inhibitor"
        );

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(
            recovered.by_name(&disk_name("disk1")).is_some()
                && recovered.by_name(&disk_name("disk2")).is_some(),
            "recovered membership must reflect the live pool (both disks still present)"
        );

        assert!(
            !f.paths.pending_op_json().exists(),
            "journal must be cleared so the operator can re-run braid remove cleanly"
        );
    }

    /// Intent: `cmd_recover` on an uncommitted remove-missing collapse
    /// restores the pre-operation pool.json and clears the journal without
    /// probing, resuming, or replaying any balance.
    ///
    /// Why it exists: the collapse converts to `ONE_DISK_PROFILES` before
    /// its device remove, so a crash can leave that convert paused. Its
    /// journal owes no RAID1 restore, and recover must leave the paused
    /// convert to the operator, as for a 2->1 `braid remove`. No balance
    /// request is mocked, so any balance probe or resume on this path fails
    /// the test with a missing mock.
    ///
    /// Scenario: a 2-disk pool lost disk2, and the operator ran
    /// `braid remove-missing --missing-id 2`. Power failed during the
    /// convert. The pool comes up mounted with disk1 present and devid 2
    /// still missing.
    #[test]
    fn cmd_recover_remove_missing_collapse_not_committed_leaves_balance_alone() {
        let f = PoolFixture::empty();
        let fs =
            MockFs::new(&[]).with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let mut journal = remove_missing_journal();
        journal.op = OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PoolMutation,
            devid: Devid::new(2),
            restore_raid1_after_commit: false,
        };
        journal::write_journal(&f.paths, &journal).unwrap();

        let show = ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 0 used 0 path MISSING\n",
        );
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                show,
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let inhibitor = RequestCountInhibitor::new(runner.clone());
        cmd_recover(
            &runner,
            &fs,
            &resolver,
            &f.recover_params()
                .passphrase_file(None)
                .sleep_inhibitor(&inhibitor)
                .build(),
        )
        .expect("recover should restore the pre-collapse state and leave the balance alone");

        assert_eq!(inhibitor.acquire_count(), 0);
        assert!(
            !runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsBalanceStatus { .. }
                    | CmdRequest::BtrfsBalanceSingle { .. }
                    | CmdRequest::BtrfsBalanceRaid1Soft { .. }
                    | CmdRequest::BtrfsDeviceRemove { .. }
            )),
            "recover must not touch the balance or rerun the device remove: {:?}",
            runner.requests()
        );
        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(
            recovered.by_name(&disk_name("disk1")).is_some()
                && recovered.by_name(&disk_name("disk2")).is_some(),
            "pool.json must keep the pre-collapse members"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent
    // cmd_recover for a bootstrap-Add journal issues the post-mutation soft
    // RAID1 balance.
    //
    // Why it exists
    // The pivot moved the runtime decision out of a per-op match in
    // replay_post_mutation and into the typed plan's
    // `RecoverCompletion::GenericLivePool.replay_raid1_maintenance`, set at
    // plan-construction time. If that value silently flips to false for Add,
    // or the executor stops consuming it, recovery would clear the journal
    // without replaying the defensive soft RAID1 balance; that pass is normally
    // a near-no-op after a RAID1 bootstrap, but it must keep firing so a future
    // mkfs/profile change cannot silently skip a real convertible chunk. The
    // pre-existing direct-call test
    // `bootstrap_recovery_clears_acked_stats` only asserts acked-stats
    // cleanup, so it would stay green through such a regression. This test
    // fails the moment either end of the construction-time/runtime contract
    // regresses.
    //
    // Scenario
    // A 2-disk bootstrap-Add crashed after btrfs created the filesystem;
    // recovery enters with the live pool already showing both disks, replays
    // the owed maintenance, and clears the journal.
    #[test]
    fn cmd_recover_bootstrap_add_replays_owed_raid1_maintenance() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");

        let journal = bootstrap_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = with_restore_balance_replay(
            MockRunner::default()
                // mountpoint check -> already mounted (skips the mount cycle)
                .with_output(mp_req, mp_out)
                // probe_pool path -- both disks are live because the
                // bootstrap crash landed after btrfs created the filesystem
                // but before pool.json/journal cleanup ran.
                .with_output(
                    CmdRequest::BtrfsFilesystemShow {
                        mount_point: MountPoint::new("/mnt/storage".into()),
                    },
                    btrfs_show_two_disks(),
                )
                .with_output(
                    CmdRequest::CryptsetupStatus {
                        mapper: MapperName::from_basename("braid-disk1".into()),
                    },
                    cryptsetup_status_active("braid-disk1", "/dev/vda"),
                )
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/vda".into(),
                    },
                    cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
                )
                .with_output(
                    CmdRequest::CryptsetupStatus {
                        mapper: MapperName::from_basename("braid-disk2".into()),
                    },
                    cryptsetup_status_active("braid-disk2", "/dev/vdb"),
                )
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/vdb".into(),
                    },
                    cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
                ),
        );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let inhibitor = RequestCountInhibitor::new(runner.clone());
        cmd_recover(
            &runner,
            &fs,
            &resolver,
            &f.recover_params()
                .passphrase_file(None)
                .sleep_inhibitor(&inhibitor)
                .build(),
        )
        .expect("bootstrap-Add recovery should replay owed maintenance");

        let requests = runner.requests();
        let balance_index = requests
            .iter()
            .position(|r| {
                matches!(
                    r,
                    CmdRequest::BtrfsBalanceRaid1Soft { mount_point }
                        if mount_point.as_str() == "/mnt/storage"
                )
            })
            .expect("cmd_recover Add path must issue post-mutation soft RAID1 balance");
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "bootstrap-Add owed RAID1 replay must acquire a sleep inhibitor"
        );
        assert!(
            inhibitor.first_acquire_request_count().unwrap() <= balance_index,
            "sleep inhibitor must be acquired before the soft balance; \
             balance_index={balance_index}, requests={requests:?}"
        );
        assert!(
            inhibitor.drop_request_count().unwrap() > balance_index,
            "sleep inhibitor guard must stay held across the soft balance; \
             balance_index={balance_index}, requests={requests:?}"
        );
        assert!(
            !f.paths.pending_op_json().exists(),
            "journal must clear after successful maintenance replay"
        );
    }

    // Intent: a 1-disk bootstrap add owes no RAID1 balance, so recover
    //   neither previews nor runs one.
    // Why it exists: plain mkfs on one disk cannot make RAID1. Recover used
    //   to preview a balance row, take a sleep inhibitor and check the balance
    //   state for a replay its gate then skipped.
    // Scenario: a first-ever `braid add` of one disk crashed after mkfs and
    //   mount. The pool is mounted on that disk when the operator runs
    //   recover.
    #[test]
    fn cmd_recover_one_disk_bootstrap_add_owes_no_balance() {
        let journal = bootstrap_pool_mutation_add_journal_for(&["disk1"]);
        let fs =
            || MockFs::new(&[]).with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let rendered = render_recover_dry_run(
            journal.clone(),
            fs(),
            already_mounted_one_disk_runner(),
            false,
        );
        assert!(
            !rendered.contains("check btrfs balance status")
                && !rendered.contains("-dconvert=raid1"),
            "a 1-disk bootstrap must preview no balance replay: {rendered:?}"
        );

        let f = PoolFixture::empty();
        journal::write_journal(&f.paths, &journal).unwrap();
        let runner = already_mounted_one_disk_runner();
        let inhibitor = RequestCountInhibitor::new(runner.clone());
        cmd_recover(
            &runner,
            &fs(),
            &resolver_for(&[("/dev/vda", "virtio-disk1")]),
            &f.recover_params()
                .passphrase_file(None)
                .sleep_inhibitor(&inhibitor)
                .build(),
        )
        .expect("1-disk bootstrap recovery should finish");

        let requests = runner.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsBalanceStatus { .. }
                    | CmdRequest::BtrfsBalanceRaid1 { .. }
                    | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "a 1-disk bootstrap must issue no balance work: {requests:?}"
        );
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "a 1-disk bootstrap with no owed balance must not take a sleep inhibitor"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent
    // Bootstrap-add GenericLivePool recovery fails closed when it cannot
    // acquire the sleep inhibitor for the owed RAID1 soft balance.
    //
    // Why it exists
    // The GenericLivePool branch writes pool.json before replaying owed
    // maintenance. If inhibitor acquisition fails, recovery must leave
    // pending-op.json intact and stop before any balance probing or replay so
    // the operator can rerun the idempotent recovery path.
    //
    // Scenario
    // A 2-disk bootstrap-Add crashed after btrfs created the filesystem.
    // Recovery rebuilds membership from the live pool, then logind inhibitor
    // acquisition fails before the owed post-add RAID1 maintenance starts.
    #[test]
    fn bootstrap_add_inhibitor_failure_stops_before_balance_and_preserves_journal() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");

        let journal = bootstrap_pool_mutation_add_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let inhibitor = FailingInhibitor;

        let err = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &f.recover_params()
                .passphrase_file(None)
                .sleep_inhibitor(&inhibitor)
                .build(),
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("could not acquire sleep inhibitor"),
            "expected bootstrap-add inhibitor failure, got: {err}",
        );
        assert!(
            !runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsBalanceStatus { .. } | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "bootstrap-add inhibitor failure must stop before balance"
        );
        assert!(
            f.paths.pending_op_json().exists(),
            "journal must survive for an idempotent retry"
        );
    }

    /// Two-disk Remove journal where disk2 is the eviction target and its
    /// pre-membership entry already carries devid 2. Used by the OpKind::Remove
    /// recover-guard tests so the missing_devids check (which keys off
    /// `pre_membership.disks[name].devid`) has the value it needs.
    fn remove_2to1_journal_with_target_devid() -> journal::Journal {
        let pre = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
        ]);
        let target = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            Some(Devid::new(1)),
        )]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Remove {
                luks_uuid: uuid_for_name("disk2"),
                name: disk_name("disk2"),
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    /// Two-disk Remove journal where disk2 was never enriched with a prior
    /// btrfs devid. Used by the null-underlying recovery carve-out test
    /// because live LUKS UUID is unobservable in that state.
    fn remove_2to1_journal_without_target_devid() -> journal::Journal {
        let pre = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry("disk2", "/dev/disk/by-id/virtio-disk2", None, None),
        ]);
        let target = membership_from(vec![membership_entry(
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            Some(Devid::new(1)),
        )]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Remove {
                luks_uuid: uuid_for_name("disk2"),
                name: disk_name("disk2"),
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    /// btrfs filesystem show output for a 2-disk pool where disk2 is reported
    /// as MISSING (path MISSING sentinel). probe_pool routes the row into
    /// `missing_devids` and only iterates disk1 for cryptsetup probes.
    fn btrfs_show_disk1_and_disk2_missing() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 0 used 0 path MISSING\n",
        )
    }

    // Intent: post-cycle recover aborts when its completion probe finds the
    //   configured pool mount absent.
    // Why it exists: probe_pool reports mounted=false with no devices when
    //   mountinfo has no entry; the GenericLivePool path must not turn that
    //   empty probe into an empty pool.json and clear the journal.
    // Scenario: a remove journal is pending, planning sees the pool as already
    //   mounted, and an external unmount removes it before completion probes.
    #[test]
    fn cmd_recover_aborts_when_post_cycle_probe_reports_unmounted() {
        let f = PoolFixture::empty();
        let fs = MockFs::without_mounted_pool(&[])
            .first_reading(
                "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n",
            )
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let journal = remove_2to1_journal_with_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default().with_output(mp_req, mp_out);

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();
        let err = cmd_recover(&runner, &fs, &resolver, &params)
            .expect_err("recover must fail when the completion probe sees no mount");

        let msg = format!("{err}");
        assert!(
            msg.contains("post-mount probe"),
            "error must name the probe state: {msg}"
        );
        assert!(
            msg.contains("no btrfs mount"),
            "error must name the unmounted state: {msg}"
        );
        assert!(
            !f.paths.pool_json().exists(),
            "pool.json must not be written when the completion probe sees no mount"
        );
        assert!(
            f.paths.pending_op_json().exists(),
            "journal must be preserved when the completion probe sees no mount"
        );
    }

    // Intent: post-cycle recover aborts when its completion probe finds a
    //   mounted btrfs filesystem with zero device rows.
    // Why it exists: the empty-device branch protects pathological btrfs show
    //   output from silently erasing membership through GenericLivePool.
    // Scenario: a remove journal is pending, the pool remains mounted, but
    //   btrfs filesystem show returns an FSID and no device rows.
    #[test]
    fn cmd_recover_aborts_when_post_cycle_probe_reports_zero_devices() {
        let f = PoolFixture::empty();
        let fs =
            MockFs::new(&[]).with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let journal = remove_2to1_journal_with_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_zero_devices(),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();
        let err = cmd_recover(&runner, &fs, &resolver, &params)
            .expect_err("recover must fail when the completion probe sees zero devices");

        let msg = format!("{err}");
        assert!(
            msg.contains("zero btrfs devices"),
            "error must name the zero-device state: {msg}"
        );
        assert!(
            !f.paths.pool_json().exists(),
            "pool.json must not be written when the completion probe sees zero devices"
        );
        assert!(
            f.paths.pending_op_json().exists(),
            "journal must be preserved when the completion probe sees zero devices"
        );
    }

    // Intent: post-cycle recover aborts before writing pool.json when the VFS
    //   mount options report the mounted pool as read-only.
    // Why it exists: operator-issued remount-ro must not let recover rewrite
    //   membership and then fail later with an opaque btrfs balance error.
    // Scenario: a remove journal is pending and the pool remains mounted, but
    //   the mount is read-only at the VFS layer.
    #[test]
    fn cmd_recover_aborts_when_post_mount_probe_reports_vfs_read_only() {
        let f = PoolFixture::empty();
        let fs = MockFs::with_mounted_pool_ro_vfs(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let journal = remove_2to1_journal_with_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_zero_devices(),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();
        let err = cmd_recover(&runner, &fs, &resolver, &params)
            .expect_err("recover must fail when the completion probe sees read-only VFS state");

        let msg = format!("{err}");
        assert!(
            msg.contains("mounted read-only"),
            "error must name the read-only state: {msg}"
        );
        assert!(
            msg.contains("recovery aborted"),
            "error must identify the real-run completion refusal, not the dry-run wording: {msg}"
        );
        assert_recover_read_only_hint_has_both_remedies(&msg);
        assert!(
            !f.paths.pool_json().exists(),
            "pool.json must not be written when the pool is read-only"
        );
        assert!(
            f.paths.pending_op_json().exists(),
            "journal must be preserved when the pool is read-only"
        );
    }

    /// Both halves of recover's read-only hint: the remount for a pool
    /// someone remounted, and the fresh mount through `braid recover` for a
    /// pool btrfs forced read-only. recover always runs with a journal, so
    /// the hint must not send the operator to `braid unlock`, which refuses
    /// then, or to `btrfs check`, which refuses a mounted filesystem.
    fn assert_recover_read_only_hint_has_both_remedies(msg: &str) {
        assert!(
            msg.contains("mount -o remount,rw /mnt/storage"),
            "error must include remount guidance: {msg}"
        );
        assert!(
            msg.contains("\"forced readonly\""),
            "error must name the kernel-log cue: {msg}"
        );
        assert!(
            msg.contains("`braid lock` and `braid recover`"),
            "error must give the fresh-mount path through recover: {msg}"
        );
        assert!(
            !msg.contains("braid unlock"),
            "unlock refuses while the journal exists: {msg}"
        );
        assert!(
            !msg.contains("btrfs check"),
            "btrfs check refuses a mounted filesystem: {msg}"
        );
    }

    // Intent: post-cycle recover aborts before writing pool.json when the
    //   filesystem options report the mounted pool as read-only.
    // Why it exists: btrfs can auto-remount the superblock read-only after
    //   I/O errors without relying on the VFS option field.
    // Scenario: a remove journal is pending and the pool remains mounted, but
    //   mountinfo field 11 carries `ro,space_cache=v2`.
    #[test]
    fn cmd_recover_aborts_when_post_mount_probe_reports_fs_read_only() {
        let f = PoolFixture::empty();
        let fs = MockFs::with_mounted_pool_ro_fs(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let journal = remove_2to1_journal_with_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_zero_devices(),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();
        let err = cmd_recover(&runner, &fs, &resolver, &params)
            .expect_err("recover must fail when the completion probe sees read-only fs state");

        let msg = format!("{err}");
        assert!(
            msg.contains("mounted read-only"),
            "error must name the read-only state: {msg}"
        );
        assert!(
            msg.contains("recovery aborted"),
            "error must identify the real-run completion refusal, not the dry-run wording: {msg}"
        );
        assert_recover_read_only_hint_has_both_remedies(&msg);
        assert!(
            !f.paths.pool_json().exists(),
            "pool.json must not be written when the pool is read-only"
        );
        assert!(
            f.paths.pending_op_json().exists(),
            "journal must be preserved when the pool is read-only"
        );
    }

    /* Intent: cmd_recover for an interrupted OpKind::Remove preserves the
     * target in pool.json when the live pool reports the target's mapper
     * as null-underlying, even though it is absent from `pool.devices`.
     * Why it exists: a rebuild that walked pool.devices only would
     * persist a pool.json that excluded the target and clear the journal -- the
     * recover-layer reproduction of the same phantom-success class that
     * RemovePlan::execute fail-closes at the helper boundary.
     * Scenario: a 2-disk pool started `braid remove disk2`. Disk2's
     * underlying device hot-unplugged before/during the eviction, so
     * btrfs still tracks it (mapper visible in `filesystem show`) but
     * cryptsetup reports `device: (null)`. probe_pool sorts disk2 into
     * `null_underlying`, leaving `pool.devices = [disk1]`. Recover must
     * keep disk2 in pool.json and still clear the journal.
     */
    #[test]
    fn cmd_recover_remove_with_null_underlying_target_preserves_membership() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", uuid_for_name("disk2").as_str());

        let journal = remove_2to1_journal_with_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "(null)"),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().passphrase_file(None).build();
        let result = cmd_recover(&runner, &fs, &resolver, &params);
        result.expect("recover should succeed and preserve null-underlying target");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(
            recovered.by_name(&disk_name("disk1")).is_some(),
            "recovered membership must keep disk1: {:?}",
            membership_name_list(&recovered)
        );
        assert!(
            recovered.by_name(&disk_name("disk2")).is_some(),
            "recovered membership must keep disk2 (null-underlying): {:?}",
            membership_name_list(&recovered)
        );
        assert!(
            !f.paths.pending_op_json().exists(),
            "journal must be cleared after live-pool recovery"
        );
    }

    /* Intent: cmd_recover for an interrupted OpKind::Remove binds a
     * null-underlying target by the LUKS UUID its dm table records, so a
     * journal without the target's btrfs devid still recovers, and the
     * rewritten pool.json records the devid btrfs holds the mapper at.
     *
     * Why it exists: the dm UUID survives the backing disk, so it is the
     * null-underlying member's identity; devid is no longer a fallback for
     * mappers. The live devid is recorded so the member still has a binding
     * once btrfs promotes the device to MISSING and the mapper is gone.
     *
     * Scenario: a hand-built or obsolete pending-op.json lacks disk2's
     * pre-operation devid. Disk2's underlying device hot-unplugged during
     * remove, so btrfs still reports mapper braid-disk2 as devid 2 while
     * cryptsetup reports `device: (null)`. The mapper's dm UUID still
     * records disk2's LUKS UUID.
     */
    #[test]
    fn cmd_recover_remove_without_devid_binds_null_underlying_target_by_dm_uuid() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", uuid_for_name("disk2").as_str());

        let journal = remove_2to1_journal_without_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "(null)"),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().passphrase_file(None).build();
        cmd_recover(&runner, &fs, &resolver, &params)
            .expect("recover should bind the null-underlying target by its dm UUID");

        let recovered = membership::load_membership(&f.paths).unwrap();
        let (_, disk2) = recovered.by_name(&disk_name("disk2")).unwrap_or_else(|| {
            panic!(
                "recovered membership must keep disk2 (null-underlying): {:?}",
                membership_name_list(&recovered)
            )
        });
        assert_eq!(
            disk2.devid,
            Some(Devid::new(2)),
            "disk2 must record the devid btrfs holds its null-backed mapper at"
        );
        assert!(
            !f.paths.pending_op_json().exists(),
            "journal must be cleared after live-pool recovery"
        );
    }

    /* Intent: cmd_recover for an interrupted OpKind::Remove fails closed
     * when a null-underlying mapper named for the target records a foreign
     * LUKS UUID.
     *
     * Why it exists: identity comes from the dm UUID, never the mapper
     * name. A mapper called braid-disk2 that records another container must
     * not stand in for disk2.
     *
     * Scenario: the same unplugged disk2 as the sibling test, but the
     * braid-disk2 mapper records a container no snapshot knows.
     */
    #[test]
    fn cmd_recover_remove_refuses_null_underlying_mapper_recording_foreign_uuid() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "99999999-9999-9999-9999-999999999999");

        let journal = remove_2to1_journal_without_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "(null)"),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().passphrase_file(None).build();
        let err = cmd_recover(&runner, &fs, &resolver, &params)
            .expect_err("recover must not bind a foreign null-underlying mapper by name");

        assert!(
            err.to_string()
                .contains("99999999-9999-9999-9999-999999999999"),
            "error should name the foreign UUID, got: {err}"
        );
        assert!(
            f.paths.pending_op_json().exists(),
            "fail-closed recovery must preserve pending-op.json"
        );
    }

    /* Intent: cmd_recover for an interrupted OpKind::Remove preserves the
     * target in pool.json when btrfs reports the target's devid in
     * `missing_devids`.
     * Why it exists: same Layer-2 phantom-success class as the
     * null-underlying case but via the btrfs-authoritative MISSING path
     * (devid sentinel rather than `device: (null)` mapper). The guard
     * must consult both signals so an in-progress remove against a
     * MISSING device does not silently drop disk2 from pool.json before
     * the operator can decide whether to re-run remove or run
     * remove-missing.
     * Scenario: a 2-disk pool started `braid remove disk2`. btrfs has
     * promoted disk2 to MISSING; `filesystem show` emits `path MISSING`
     * for devid 2. probe_pool sets `missing_devids = [2]` and
     * `pool.devices = [disk1]`. Recover must keep disk2 in pool.json
     * and still clear the journal.
     */
    #[test]
    fn cmd_recover_remove_with_missing_target_preserves_membership() {
        let f = PoolFixture::empty();
        let fs =
            MockFs::new(&[]).with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let journal = remove_2to1_journal_with_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_disk1_and_disk2_missing(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();
        let result = cmd_recover(&runner, &fs, &resolver, &params);
        result.expect("recover should succeed and preserve MISSING target");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(
            recovered.by_name(&disk_name("disk1")).is_some(),
            "recovered membership must keep disk1: {:?}",
            membership_name_list(&recovered)
        );
        assert!(
            recovered.by_name(&disk_name("disk2")).is_some(),
            "recovered membership must keep disk2 (btrfs MISSING): {:?}",
            membership_name_list(&recovered)
        );
        assert!(
            !f.paths.pending_op_json().exists(),
            "journal must be cleared after live-pool recovery"
        );
    }

    /* Intent: cmd_recover for an interrupted OpKind::Remove still drops
     * the target from pool.json when the live pool genuinely no longer
     * tracks it -- not in `pool.devices`, not in `null_underlying`, not
     * in `missing_devids`.
     * Why it exists: the Remove recovery binding must not over-correct.
     * The eviction may have completed before the helper crashed, in
     * which case btrfs has fully removed the device and pool.json must
     * follow.
     * Scenario: a 2-disk pool started `braid remove disk2`; the
     * btrfs device remove succeeded and the LUKS close finished, so the
     * live pool now reports only disk1. Recover must drop disk2 from
     * pool.json and clear the journal.
     */
    /// 3-disk Remove journal with disk2 as the eviction target and devids
    /// pinned on every pre_membership entry. Used by recover-side tests
    /// for the Remove recovery binding, which must preserve any
    /// pre_membership disk that btrfs still owns -- not just the target.
    fn remove_3to2_journal_with_devids() -> journal::Journal {
        let pre = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                None,
                Some(Devid::new(3)),
            ),
        ]);
        let target = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                None,
                Some(Devid::new(3)),
            ),
        ]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Remove {
                luks_uuid: uuid_for_name("disk2"),
                name: disk_name("disk2"),
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    fn remove_missing_3to2_journal_pool_mutation_with_devids() -> journal::Journal {
        let pre = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk2",
                "/dev/disk/by-id/virtio-disk2",
                None,
                Some(Devid::new(2)),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                None,
                Some(Devid::new(3)),
            ),
        ]);
        let target = membership_from(vec![
            membership_entry(
                "disk1",
                "/dev/disk/by-id/virtio-disk1",
                None,
                Some(Devid::new(1)),
            ),
            membership_entry(
                "disk3",
                "/dev/disk/by-id/virtio-disk3",
                None,
                Some(Devid::new(3)),
            ),
        ]);

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::RemoveMissing {
                phase: journal::RemoveMissingPhase::PoolMutation,
                devid: Devid::new(2),
                restore_raid1_after_commit: true,
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    /* Intent: cmd_recover for an interrupted OpKind::Remove preserves a
     * NON-target disk in pool.json when btrfs reports its devid in
     * `missing_devids`.
     * Why it exists: the Remove recovery binding restores any
     * pre_membership disk that btrfs still owns, not just the target.
     * Without this, a post-journal validation failure that preserved the
     * journal would be undermined by recover silently dropping non-target
     * MISSING disks from pool.json.
     * Scenario: a 3-disk pool started `braid remove disk2`; between
     * journal::write_journal and the post-journal validation, disk3 went
     * MISSING (flapping disk). Recover must keep disk2 AND disk3 in
     * pool.json so a follow-up remove or remove-missing can act on them.
     */
    #[test]
    fn cmd_recover_remove_preserves_non_target_missing_disk() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");

        let journal = remove_3to2_journal_with_devids();
        journal::write_journal(&f.paths, &journal).unwrap();

        // Live pool: disk1 and disk2 present; disk3 is MISSING (btrfs-
        // authoritative path MISSING sentinel for devid 3).
        let show = ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n\
             \tdevid    3 size 0 used 0 path MISSING\n",
        );
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                show,
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk2"),
            ("/dev/vdc", "virtio-disk3"),
        ]);
        let params = f.recover_params().passphrase_file(None).build();
        let result = cmd_recover(&runner, &fs, &resolver, &params);
        result.expect("recover should succeed and preserve non-target MISSING");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(
            recovered.by_name(&disk_name("disk2")).is_some(),
            "target disk2 must be preserved (still in pool.devices)"
        );
        assert!(
            recovered.by_name(&disk_name("disk3")).is_some(),
            "non-target MISSING disk3 must be preserved by the recovery binding"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent: cmd_recover preserves non-target MISSING members through the
    // RemoveMissing::PoolMutation committed path.
    // Why it exists: the phased remove-missing dispatcher goes through
    // recover_membership_matching_expected, so this pins that path's devid
    // binding at the command boundary.
    // Scenario: remove-missing committed for devid 2, then unrelated disk3
    // flapped to MISSING before recovery rebuilt pool.json.
    #[test]
    fn cmd_recover_remove_missing_pool_mutation_preserves_non_target_missing_disk() {
        let f = PoolFixture::empty();
        let fs =
            MockFs::new(&[]).with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let journal = remove_missing_3to2_journal_pool_mutation_with_devids();
        journal::write_journal(&f.paths, &journal).unwrap();

        // Live pool: disk1 present, disk2 gone, disk3 still owned by btrfs
        // through the MISSING sentinel for devid 3.
        let show = ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    3 size 0 used 0 path MISSING\n",
        );
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = with_restore_balance_replay(MockRunner::default())
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                show,
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-disk3")]);
        let params = f.recover_params().passphrase_file(None).build();
        let result = cmd_recover(&runner, &fs, &resolver, &params);
        result.expect("recover should succeed and preserve non-target MISSING");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(
            recovered.by_name(&disk_name("disk2")).is_none(),
            "removed devid 2 must stay absent"
        );
        assert!(
            recovered.by_name(&disk_name("disk3")).is_some(),
            "non-target MISSING disk3 must be preserved after remove-missing commits"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    // Intent: cmd_recover preserves non-target null-underlying members through
    // the RemoveMissing::PoolMutation committed path.
    // Why it exists: the phased remove-missing dispatcher goes through
    // recover_membership_matching_expected, so this pins that path's
    // null-underlying binding at the command boundary.
    // Scenario: remove-missing committed for devid 2, then unrelated disk3's
    // underlying block device hot-unplugged before recovery rebuilt pool.json.
    #[test]
    fn cmd_recover_remove_missing_pool_mutation_preserves_non_target_null_underlying_disk() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk3", uuid_for_name("disk3").as_str());

        let journal = remove_missing_3to2_journal_pool_mutation_with_devids();
        journal::write_journal(&f.paths, &journal).unwrap();

        let show = ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    3 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk3\n",
        );
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = with_restore_balance_replay(MockRunner::default())
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                show,
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk3".into()),
                },
                cryptsetup_status_active("braid-disk3", "(null)"),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();
        let result = cmd_recover(&runner, &fs, &resolver, &params);
        result.expect("recover should succeed and preserve non-target null-underlying");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(
            recovered.by_name(&disk_name("disk2")).is_none(),
            "removed devid 2 must stay absent"
        );
        assert!(
            recovered.by_name(&disk_name("disk3")).is_some(),
            "non-target null-underlying disk3 must be preserved after remove-missing commits"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    /// Remove-missing of devid 4 that committed and still owes its RAID1
    /// restore, over a target of disk1, disk2 and disk3.
    fn remove_missing_4to3_post_maintenance_journal() -> journal::Journal {
        let entry = |name: &str, devid: u64| {
            membership_entry(
                name,
                &format!("/dev/disk/by-id/virtio-{name}"),
                None,
                Some(Devid::new(devid)),
            )
        };
        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::RemoveMissing {
                phase: journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
                devid: Devid::new(4),
                restore_raid1_after_commit: true,
            },
            pre_membership: membership_from(vec![
                entry("disk1", 1),
                entry("disk2", 2),
                entry("disk3", 3),
                entry("disk4", 4),
            ]),
            target_membership: membership_from(vec![
                entry("disk1", 1),
                entry("disk2", 2),
                entry("disk3", 3),
            ]),
        }
    }

    /// Already-mounted pool with disk1 and disk2 present, an idle balance,
    /// and a soft balance that would succeed, so only the RAID1 gate decides
    /// whether the owed replay runs. `show` places disk3.
    fn owed_replay_gate_runner(show: RawCommandOutput) -> MockRunner {
        let (mp_req, mp_out) = mountpoint_ok();
        with_restore_balance_replay(MockRunner::default())
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                show,
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", uuid_for_name("disk1").as_str()),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", uuid_for_name("disk2").as_str()),
            )
    }

    /// Run `cmd_recover` with status lines captured, and assert that it
    /// finished, issued no soft balance, printed the restore skip line and
    /// cleared the journal.
    fn assert_owed_replay_skipped(
        f: &PoolFixture,
        runner: &MockRunner,
        fs: &MockFs,
        context: &str,
    ) {
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().passphrase_file(None).build();
        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_recover(runner, fs, &resolver, &params));
        });
        result
            .expect("cmd_recover ran")
            .unwrap_or_else(|e| panic!("{context}: recover should finish after the skip: {e}"));

        let requests = runner.requests();
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "{context}: recover must not replay the soft balance; requests: {requests:?}"
        );
        assert!(
            captured.contains("[skip] pool: rebalance skipped -- redundancy not restored\n"),
            "{context}: recover must print the restore skip line, got: {captured:?}"
        );
        assert!(
            !f.paths.pending_op_json().exists(),
            "{context}: a skipped replay still clears the journal"
        );
    }

    // Intent: recover skips the owed post-remove-missing RAID1 soft balance
    // while a target member is hot-unplugged but not yet btrfs-MISSING, then
    // prints the restore skip line and clears the journal.
    // Why it exists: the replay gated on two present devices only. btrfs
    // still allocates chunks on a null-backed member, so the soft balance
    // could put RAID1 copies on a gone disk and then report the replay done
    // (hunt finding 261022-1230-009).
    // Scenario: remove-missing of devid 4 committed and crashed before its
    // soft balance. disk3's disk was then unplugged while the pool stayed
    // mounted, and the operator ran braid recover.
    #[test]
    fn cmd_recover_owed_replay_skips_while_member_hot_unplugged() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", uuid_for_name("disk1").as_str())
            .with_luks_dm("braid-disk2", uuid_for_name("disk2").as_str())
            .with_luks_dm("braid-disk3", uuid_for_name("disk3").as_str());
        journal::write_journal(&f.paths, &remove_missing_4to3_post_maintenance_journal()).unwrap();
        let runner = owed_replay_gate_runner(ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n\
             \tdevid    3 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk3\n",
        ))
        .with_output(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName::from_basename("braid-disk3".into()),
            },
            cryptsetup_status_active("braid-disk3", "(null)"),
        );

        assert_owed_replay_skipped(&f, &runner, &fs, "hot-unplugged disk3");
    }

    // Intent: recover skips the owed RAID1 soft balance while a target member
    // is btrfs-MISSING, then prints the restore skip line and clears the
    // journal, as the live restore does.
    // Why it exists: recover and the live restore share one RAID1 gate. The
    // replay used to run on any two present devices, so a degraded pool got
    // a balance the live path would skip.
    // Scenario: remove-missing of devid 4 committed and crashed before its
    // soft balance. disk3 then dropped out and btrfs marked it MISSING
    // before the operator ran braid recover.
    #[test]
    fn cmd_recover_owed_replay_skips_while_member_missing() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", uuid_for_name("disk1").as_str())
            .with_luks_dm("braid-disk2", uuid_for_name("disk2").as_str());
        journal::write_journal(&f.paths, &remove_missing_4to3_post_maintenance_journal()).unwrap();
        let runner = owed_replay_gate_runner(ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n\
             \tdevid    3 size 0 used 0 path MISSING\n",
        ));

        assert_owed_replay_skipped(&f, &runner, &fs, "btrfs-MISSING disk3");
    }

    /* Intent: cmd_recover for an interrupted OpKind::Remove preserves a
     * NON-target disk in pool.json when its mapper is in
     * `pool.null_underlying` (cryptsetup reports `device: (null)`).
     * Why it exists: the recovery binding reads both null_underlying and
     * missing_devids; this test pins the null_underlying arm for the
     * non-target case.
     * Scenario: a 3-disk pool started `braid remove disk2`; between
     * journal write and post-journal validation, disk3's underlying
     * block device was hot-unplugged.
     */
    #[test]
    fn cmd_recover_remove_preserves_non_target_null_underlying_disk() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222")
            .with_luks_dm("braid-disk3", uuid_for_name("disk3").as_str());

        let journal = remove_3to2_journal_with_devids();
        journal::write_journal(&f.paths, &journal).unwrap();

        // Live pool reports all three mappers; disk3 has cryptsetup
        // status `device: (null)` so probe_pool sorts it into
        // null_underlying.
        let show = ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk2\n\
             \tdevid    3 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk3\n",
        );
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                show,
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk3".into()),
                },
                cryptsetup_status_active("braid-disk3", "(null)"),
            );

        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk2"),
            ("/dev/vdc", "virtio-disk3"),
        ]);
        let params = f.recover_params().passphrase_file(None).build();
        let result = cmd_recover(&runner, &fs, &resolver, &params);
        result.expect("recover should succeed and preserve non-target null-underlying");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(recovered.by_name(&disk_name("disk2")).is_some());
        assert!(
            recovered.by_name(&disk_name("disk3")).is_some(),
            "non-target null-underlying disk3 must be preserved by the recovery binding"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    /* Intent: cmd_recover for an interrupted OpKind::Remove must NOT
     * resurrect a NON-target disk that is genuinely gone from the live
     * pool (not in devices, not null-underlying, not MISSING).
     * Why: the recovery binding must not over-correct. A non-target disk
     * may have been removed by an earlier, fully-completed operation
     * before the interrupted Remove; recover must drop it from pool.json
     * to match the live pool.
     * Scenario: a 3-disk pool started `braid remove disk2`. The btrfs
     * device-remove on disk2 was interrupted -- BUT disk3 had separately
     * been fully evicted in a prior, completed operation. The live pool
     * now reports only disk1 and disk2. Recover must drop disk3.
     */
    #[test]
    fn cmd_recover_remove_does_not_resurrect_genuinely_gone_non_target() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");

        let journal = remove_3to2_journal_with_devids();
        journal::write_journal(&f.paths, &journal).unwrap();

        // Live pool: only disk1 and disk2 present; disk3 is genuinely
        // gone -- not in pool.devices, not in null_underlying, not in
        // missing_devids.
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = f.recover_params().passphrase_file(None).build();
        let result = cmd_recover(&runner, &fs, &resolver, &params);
        result.expect("recover should succeed when non-target is genuinely gone");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(recovered.by_name(&disk_name("disk1")).is_some());
        assert!(
            recovered.by_name(&disk_name("disk2")).is_some(),
            "target disk2 must be preserved (still in pool.devices)"
        );
        assert!(
            recovered.by_name(&disk_name("disk3")).is_none(),
            "genuinely gone non-target disk3 must NOT be resurrected"
        );
        assert!(!f.paths.pending_op_json().exists());
    }

    #[test]
    fn cmd_recover_remove_with_genuinely_evicted_target_drops_membership() {
        let f = PoolFixture::empty();
        let fs =
            MockFs::new(&[]).with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let journal = remove_2to1_journal_with_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();

        let runner = already_mounted_one_disk_runner();

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let params = f.recover_params().passphrase_file(None).build();
        let result = cmd_recover(&runner, &fs, &resolver, &params);
        result.expect("recover should succeed for genuinely evicted target");

        let recovered = membership::load_membership(&f.paths).unwrap();
        assert!(
            recovered.by_name(&disk_name("disk1")).is_some(),
            "recovered membership must keep disk1: {:?}",
            membership_name_list(&recovered)
        );
        assert!(
            recovered.by_name(&disk_name("disk2")).is_none(),
            "recovered membership must drop genuinely evicted disk2: {:?}",
            membership_name_list(&recovered)
        );
        assert!(
            !f.paths.pending_op_json().exists(),
            "journal must be cleared after live-pool recovery"
        );
    }

    // ----- PR 6 planner-boundary tests -----

    /* Intent: dry-run stepful success on a not-mounted pool renders
     * the entry banner first, then per-disk probe notes, then the
     * open/scan/mount steps, and finally the write-pool.json /
     * clear-pending-op.json steps.
     *
     * Why it exists: PR 6 moves recover's pre-dry-run-gate probe and
     * step compilation into `plan_recover`. A regression that either
     * dropped the entry banner, reordered notes vs. steps, or dropped
     * the state-recovery steps would break the dry-run contract pinned
     * by the CLI test's stdout/stderr split. Covers the planner
     * boundary independently of the subprocess wire contract.
     *
     * Scenario: 2-disk journal (pre={disk1,disk2}, target +disk3);
     * union = {disk1,disk2,disk3}. disk1 and disk2 are present and
     * closed; disk3 is absent (not in fs). allow_degraded=true permits
     * plan_open_pool to succeed; dry_run=true.
     */
    #[test]
    fn plan_recover_dry_run_stepful_not_mounted() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal = two_disk_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);

        let params = f
            .recover_params()
            .passphrase_file(None)
            .allow_degraded(true)
            .dry_run(true)
            .build();

        let rendered = plan_recover(&runner, &fs, &params)
            .expect("plan_recover should succeed with allow_degraded=true")
            .preview()
            .render();

        let entry_banner = format_recover_entry(&journal);
        assert!(
            rendered.starts_with(&format!("{entry_banner}\n")),
            "rendered preview must start with the entry banner, got: {rendered:?}",
        );

        let entry_pos = 0usize;
        let disk1_pos = rendered
            .find("[ok]   disk disk1")
            .unwrap_or_else(|| panic!("disk1 probe note missing: {rendered:?}"));
        let disk2_pos = rendered
            .find("[ok]   disk disk2")
            .unwrap_or_else(|| panic!("disk2 probe note missing: {rendered:?}"));
        let disk3_pos = rendered
            .find("[skip] disk disk3")
            .unwrap_or_else(|| panic!("disk3 skip note missing: {rendered:?}"));
        let scan_pos = rendered
            .find("btrfs device scan")
            .unwrap_or_else(|| panic!("btrfs device scan step missing: {rendered:?}"));
        let write_pos = rendered
            .find("write recovered pool.json")
            .unwrap_or_else(|| panic!("write pool.json step missing: {rendered:?}"));
        let clear_pos = rendered
            .find("clear pending-op.json")
            .unwrap_or_else(|| panic!("clear pending-op.json step missing: {rendered:?}"));

        assert!(
            entry_pos < disk1_pos
                && disk1_pos < disk2_pos
                && disk2_pos < disk3_pos
                && disk3_pos < scan_pos
                && scan_pos < write_pos
                && write_pos < clear_pos,
            "expected entry < probe notes < open/scan < write/clear, got: {rendered:?}",
        );

        assert!(
            rendered.contains("LUKS open /dev/disk/by-id/virtio-disk1"),
            "expected disk1 LUKS open step, got: {rendered:?}",
        );
        assert!(
            rendered.contains("LUKS open /dev/disk/by-id/virtio-disk2"),
            "expected disk2 LUKS open step, got: {rendered:?}",
        );
    }

    /* Intent: dry-run stepful success on an already-mounted pool
     * renders the entry banner, the `pool already mounted at
     * /mnt/storage` Info note, and the write/clear state-recovery
     * steps on a single preview; the literal `nothing to do.\n`
     * fallback must NOT appear because steps are non-empty.
     *
     * Why it exists: recover's already-mounted dry-run is stepful
     * (not note-only) -- state recovery steps always run. A regression
     * that dropped the state-recovery steps, widened the reconciliation
     * into real-run mount work, or appended `nothing to do.` would
     * break the dry-run contract.
     *
     * Scenario: pool already mounted; plan_open_pool short-circuits
     * to Ok(None) with a single AlreadyMounted event. The dry-run
     * reconciliation walks the probed pool and finds both live
     * devices in the recovery admission membership, so it proceeds to emit the
     * write/clear steps.
     */
    #[test]
    fn plan_recover_dry_run_stepful_already_mounted() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222");

        let journal = two_disk_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            // probe_pool (dry-run reconciliation) -- live pool has both disks.
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        let params = f
            .recover_params()
            .passphrase_file(None)
            .dry_run(true)
            .build();

        let rendered = plan_recover(&runner, &fs, &params)
            .expect("plan_recover should succeed on already-mounted pool")
            .preview()
            .render();

        let entry_banner = format_recover_entry(&journal);
        assert!(
            rendered.contains(&entry_banner),
            "rendered preview must contain the entry banner, got: {rendered:?}",
        );
        assert!(
            rendered.contains("pool already mounted at /mnt/storage"),
            "rendered preview must contain the AlreadyMounted note, got: {rendered:?}",
        );
        assert!(
            rendered.contains("write recovered pool.json"),
            "rendered preview must contain the write-pool.json step, got: {rendered:?}",
        );
        assert!(
            rendered.contains("clear pending-op.json"),
            "rendered preview must contain the clear-pending-op.json step, got: {rendered:?}",
        );
        assert!(
            !rendered.contains("nothing to do."),
            "stepful preview must not emit the `nothing to do.` fallback, got: {rendered:?}",
        );
    }

    // Intent: already-mounted recover dry-run rejects a live pool device whose
    // LUKS UUID is absent from the recovery admission membership, even when
    // its mapper is not braid-prefixed.
    // Why it exists: dry-run must not claim recovery is ready when live
    // topology contains a UUID not in the admission membership.
    // Scenario: a mounted btrfs pool contains disk1, disk2, and an externally
    // named LUKS mapper with a foreign UUID.
    #[test]
    fn plan_recover_dry_run_already_mounted_rejects_foreign_live_uuid() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222")
            .with_luks_dm("luks-foreign", "99999999-9999-9999-9999-999999999999");

        let journal = two_disk_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let runner = already_mounted_two_disks_and_foreign_runner();
        let params = f
            .recover_params()
            .passphrase_file(None)
            .dry_run(true)
            .build();

        let failure = match plan_recover(&runner, &fs, &params) {
            Err(failure) => failure,
            other => panic!("expected foreign-live-UUID PlanFailure, got: {other:?}"),
        };
        let err = match &failure.error {
            RecoverError::Failed(msg) => msg,
            other => panic!("expected RecoverError::Failed for foreign live UUID, got: {other:?}"),
        };
        assert!(
            err.contains("device luks-foreign (LUKS UUID 99999999-9999-9999-9999-999999999999)"),
            "error must name the foreign live UUID, got: {err:?}",
        );
        assert!(
            err.contains("recovery admission membership"),
            "error must describe the admission mismatch, got: {err:?}",
        );
    }

    // Intent: Replace::PostReplaceMaintenance planning accepts a realistic
    // committed journal where the new member inherited the old member's devid.
    // Why it exists: post-commit replace recovery must use target-only
    // admission, not a pre-plus-target merge that treats the inherited devid
    // as journal corruption.
    // Scenario: replace old -> new committed, pending-op.json reached
    // PostReplaceMaintenance, and dry-run reuses an already-mounted pool whose
    // live topology is the committed target.
    #[test]
    fn plan_recover_post_replace_maintenance_accepts_inherited_devid() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-new", "33333333-3333-3333-3333-333333333333");

        let journal = replace_post_maintenance_journal(
            false,
            journal::ReplaceJournalSource::Missing {
                old_devid: Devid::new(2),
            },
        );
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_disk1_and_new(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-new".into()),
                },
                cryptsetup_status_active("braid-new", "/dev/vdc"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into(),
                },
                cryptsetup_uuid_ok("/dev/vdc", "33333333-3333-3333-3333-333333333333"),
            );

        let params = f
            .recover_params()
            .passphrase_file(None)
            .dry_run(true)
            .build();

        plan_recover(&runner, &fs, &params)
            .expect("post-replace maintenance planning should accept inherited devid");
    }

    // Intent: non-post-maintenance replace recovery fails cleanly when the
    // admission merge finds a cross-snapshot membership conflict.
    // Why it exists: recovery admission construction must surface a
    // PlanFailure instead of panicking on unexpected journal conflicts.
    // Scenario: individually valid pre and target snapshots conflict only
    // when Replace::PoolMutation admits both snapshots: target new reuses
    // pre old's by-id binding.
    #[test]
    fn plan_recover_pool_mutation_admission_conflict_preserves_entry_note() {
        let f = PoolFixture::empty();
        let fs = MockFs::without_mounted_pool(&[]);

        let mut journal = replace_journal();
        let new_uuid = uuid_for_name("new");
        let mut new_member = journal
            .target_membership
            .remove_by_uuid(&new_uuid)
            .expect("replace target fixture member");
        new_member.by_id = ByIdPath::parse("/dev/disk/by-id/virtio-old").unwrap();
        journal
            .target_membership
            .insert(new_uuid, new_member)
            .expect("target snapshot remains individually valid");
        journal::write_journal(&f.paths, &journal).unwrap();

        let runner = MockRunner::default();
        let params = f.recover_params().passphrase_file(None).build();

        let failure = match plan_recover(&runner, &fs, &params) {
            Err(failure) => failure,
            other => panic!("expected admission-conflict PlanFailure, got: {other:?}"),
        };
        match &failure.error {
            RecoverError::Membership(membership::MembershipError::Conflict(msg)) => {
                assert!(
                    msg.contains("by_id '/dev/disk/by-id/virtio-old' already in use"),
                    "conflict should name the duplicated by-id, got: {msg}"
                );
            }
            other => panic!("expected membership conflict, got: {other:?}"),
        }
        let entry_banner = format_recover_entry(&journal);
        assert!(
            matches!(&failure.notes[0], PreviewNote::Info(msg) if msg == &entry_banner),
            "entry banner note must be preserved, got: {:?}",
            failure.notes,
        );
        assert!(
            runner.requests().is_empty(),
            "admission conflict should fail before mount planning"
        );
    }

    // Intent: already-mounted Replace::PostReplaceMaintenance dry-run rejects
    // a live pre-only old UUID under target-only admission.
    // Why it exists: post-commit replace recovery must not keep admitting the
    // pre snapshot after the journal reaches PostReplaceMaintenance.
    // Scenario: the journal says replace committed old -> new, but the
    // mounted pool still reports old as live; dry-run refuses before
    // advertising recovery steps.
    #[test]
    fn plan_recover_post_replace_maintenance_rejects_live_old_uuid() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-old", "22222222-2222-2222-2222-222222222222");

        let journal = replace_post_maintenance_journal(
            false,
            journal::ReplaceJournalSource::Missing {
                old_devid: Devid::new(2),
            },
        );
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_disk1_and_old(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-old".into()),
                },
                cryptsetup_status_active("braid-old", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        let params = f
            .recover_params()
            .passphrase_file(None)
            .dry_run(true)
            .build();

        let failure = match plan_recover(&runner, &fs, &params) {
            Err(failure) => failure,
            other => panic!("expected target-only admission failure, got: {other:?}"),
        };
        let err = match &failure.error {
            RecoverError::Failed(msg) => msg,
            other => panic!("expected RecoverError::Failed for live old UUID, got: {other:?}"),
        };
        assert!(
            err.contains("device braid-old (LUKS UUID 22222222-2222-2222-2222-222222222222)"),
            "error must name old as the rejected live UUID, got: {err:?}",
        );
        assert!(
            err.contains("recovery admission membership"),
            "error must describe target-only admission, got: {err:?}",
        );
    }

    // Intent: already-mounted recover dry-run keeps mapper-prefix checks out
    // of the read-only failure path.
    // Why it exists: read-only refusal should preserve entry context without
    // making identity decisions from mapper names.
    // Scenario: dry-run probes a mounted pool with a foreign mapper, then sees
    // filesystem-level read-only mount options and aborts before UUID
    // validation can run.
    #[test]
    fn plan_recover_dry_run_read_only_failure_has_no_foreign_mapper_skip() {
        let f = PoolFixture::empty();
        let fs = MockFs::with_mounted_pool_ro_fs(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
            .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222")
            .with_luks_dm("luks-foreign", "99999999-9999-9999-9999-999999999999");

        let journal = two_disk_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let runner = already_mounted_two_disks_and_foreign_runner();
        let params = f
            .recover_params()
            .passphrase_file(None)
            .dry_run(true)
            .build();

        let failure = match plan_recover(&runner, &fs, &params) {
            Err(failure) => failure,
            other => panic!("expected read-only PlanFailure, got: {other:?}"),
        };
        let err = match &failure.error {
            RecoverError::Failed(msg) => msg,
            other => panic!("expected RecoverError::Failed for read-only dry-run, got: {other:?}"),
        };
        assert!(
            err.contains("mounted read-only"),
            "error must name the read-only state, got: {err:?}",
        );
        let rendered =
            preview::render_notes_for_stderr_with(&failure.notes, PerDiskStyle::Bracketed, false);
        assert!(
            !rendered.contains("luks-foreign"),
            "failure notes must not add mapper-name identity skips, got: {rendered:?}",
        );
    }

    /* Intent: when the journal records OpKind::Replace and the pool is already
     * mounted at planner entry, plan_recover MUST return RecoverError::Failed
     * with safe-recovery instructions, preserving the entry banner and
     * AlreadyMounted info note on PlanFailure::notes. The refusal must fire for
     * both dry_run = false (real run) and dry_run = true (preview); the gate
     * sits upstream of that branch, so a regression that affects only one of
     * the two would still be a real regression.
     *
     * Why it exists: kernel-resumed btrfs_resume_dev_replace_async on a session
     * braid did not open leaves stale in-memory fs_devices that probe_pool
     * cannot distinguish from real topology. The mount cycle that scrubs this
     * state is gated on just_mounted == true, and an admin-mounted pool takes
     * the just_mounted == false path. Without this fail-fast, recover would
     * silently corrupt pool.json from stale topology.
     *
     * Scenario: post-crash, an admin ran `cryptsetup open` + `mount(8)`
     * directly (circumventing braid's pending-op preflight on `unlock`), then
     * invoked `braid recover`.
     */
    #[test]
    fn plan_recover_refuses_replace_on_externally_mounted_pool() {
        for dry_run in [false, true] {
            let f = PoolFixture::empty();
            let fs = MockFs::new(&[])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

            let journal = replace_journal();
            journal::write_journal(&f.paths, &journal).unwrap();

            // Only mock the mountpoint check. plan_open_pool short-circuits to
            // Ok(None) before any per-disk probe, so no further mocks are
            // needed -- any subsequent CryptsetupStatus / BtrfsFilesystemShow
            // call would surface as MissingMock, proving the fail-fast fires
            // before probing.
            let (mp_req, mp_out) = mountpoint_ok();
            let runner = MockRunner::default().with_output(mp_req, mp_out);

            let params = f
                .recover_params()
                .passphrase_file(None)
                .dry_run(dry_run)
                .build();

            let failure = match plan_recover(&runner, &fs, &params) {
                Err(failure) => failure,
                other => {
                    panic!("expected RecoverError::Failed for dry_run={dry_run}, got: {other:?}")
                }
            };
            let err = match failure.error {
                RecoverError::Failed(msg) => msg,
                other => {
                    panic!("expected RecoverError::Failed for dry_run={dry_run}, got: {other:?}")
                }
            };
            assert!(
                err.contains("already-mounted"),
                "dry_run={dry_run}: error must mention the already-mounted condition, got: {err:?}",
            );
            assert!(
                err.contains("sudo braid lock"),
                "dry_run={dry_run}: error must direct to `sudo braid lock`, got: {err:?}",
            );
            assert!(
                !err.contains("sudo braid recover"),
                "dry_run={dry_run}: planner error must leave retry guidance to the command boundary, got: {err:?}",
            );

            assert_eq!(
                failure.notes.len(),
                2,
                "dry_run={dry_run}: PlanFailure::notes must hold entry banner + AlreadyMounted, got: {:?}",
                failure.notes,
            );
            let entry_banner = format_recover_entry(&journal);
            match &failure.notes[0] {
                PreviewNote::Info(msg) => assert_eq!(
                    msg, &entry_banner,
                    "dry_run={dry_run}: notes[0] must be the entry banner",
                ),
                other => {
                    panic!("dry_run={dry_run}: notes[0] must be PreviewNote::Info, got: {other:?}")
                }
            }
            match &failure.notes[1] {
                PreviewNote::Info(msg) => assert!(
                    msg.contains("pool already mounted at /mnt/storage"),
                    "dry_run={dry_run}: notes[1] must be the AlreadyMounted info, got: {msg:?}",
                ),
                other => {
                    panic!("dry_run={dry_run}: notes[1] must be PreviewNote::Info, got: {other:?}")
                }
            }
        }
    }

    /* Intent: recover refuses a btrfs at the mount point that is provably not
     * this pool in add-target discovery, before it probes or opens a target.
     * Why it exists: discovery used to take any mount at the mount point for
     * the pool and skip to the pool probe, so recover reconciled its journal
     * against a filesystem braid does not own.
     * Scenario: an add of disk2 was interrupted; the operator mounts a
     * LUKS-encrypted backup btrfs at /mnt/storage by hand and runs
     * `braid recover`.
     */
    #[test]
    fn plan_recover_refuses_btrfs_that_is_not_this_pool_before_discovery() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[])
            .mounted_from("backup")
            .with_luks_dm("backup", "99999999-9999-9999-9999-999999999999");
        journal::write_journal(&f.paths, &recoverable_pool_mutation_add_journal()).unwrap();
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default().with_output(mp_req.clone(), mp_out);
        let params = f.recover_params().passphrase_file(None).build();

        let failure = match plan_recover(&runner, &fs, &params) {
            Err(failure) => failure,
            Ok(_) => panic!("recover must refuse a btrfs that is not this pool"),
        };

        assert!(
            matches!(
                &failure.error,
                RecoverError::Mount(MountError::OtherBtrfs(_))
            ),
            "expected OtherBtrfs, got {:?}",
            failure.error,
        );
        assert_eq!(
            failure.error.to_string(),
            "/mnt/storage holds a btrfs filesystem on /dev/mapper/backup that is not \
             this pool: its LUKS UUID 99999999-9999-9999-9999-999999999999 is not a \
             member in pool.json or the pending-operation journal",
        );
        assert_eq!(
            runner.requests(),
            vec![mp_req],
            "discovery must refuse on its own mount check, before any probe",
        );
    }

    /* Intent: dry-run recover refuses an already-mounted read-only pool with
     * the same PlanFailure note preservation shape used by other preview
     * refusals.
     * Why it exists: preview must agree with execute before recover would
     * write pool.json or clear the pending-operation journal.
     * Scenario: a remove journal is pending, the pool is already mounted, and
     * mountinfo reports filesystem-level read-only state.
     */
    #[test]
    fn plan_recover_dry_run_refuses_already_mounted_read_only_fs_options() {
        let f = PoolFixture::empty();
        let fs = MockFs::with_mounted_pool_ro_fs(&[])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111");

        let journal = remove_2to1_journal_with_target_devid();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_zero_devices(),
            );

        let params = f
            .recover_params()
            .passphrase_file(None)
            .dry_run(true)
            .build();

        let failure = match plan_recover(&runner, &fs, &params) {
            Err(failure) => failure,
            other => panic!("expected RecoverError::Failed for read-only dry-run, got: {other:?}"),
        };
        let err = match &failure.error {
            RecoverError::Failed(msg) => msg,
            other => panic!("expected RecoverError::Failed for read-only dry-run, got: {other:?}"),
        };
        assert!(
            err.contains("mounted read-only"),
            "error must name the read-only state, got: {err:?}",
        );
        assert!(
            err.contains("recover dry-run"),
            "error must identify the dry-run refusal, got: {err:?}",
        );
        assert_recover_read_only_hint_has_both_remedies(err);

        assert_eq!(
            failure.notes.len(),
            2,
            "PlanFailure::notes must hold entry banner + AlreadyMounted, got: {:?}",
            failure.notes,
        );
        let entry_banner = format_recover_entry(&journal);
        match &failure.notes[0] {
            PreviewNote::Info(msg) => assert_eq!(msg, &entry_banner),
            other => panic!("notes[0] must be PreviewNote::Info, got: {other:?}"),
        }
        match &failure.notes[1] {
            PreviewNote::Info(msg) => assert!(
                msg.contains("pool already mounted at /mnt/storage"),
                "notes[1] must be the AlreadyMounted info, got: {msg:?}",
            ),
            other => panic!("notes[1] must be PreviewNote::Info, got: {other:?}"),
        }
    }

    fn render_recover_dry_run(
        journal: journal::Journal,
        fs: MockFs,
        runner: MockRunner,
        allow_degraded: bool,
    ) -> String {
        let f = PoolFixture::empty();
        journal::write_journal(&f.paths, &journal).unwrap();

        let params = f
            .recover_params()
            .passphrase_file(None)
            .allow_degraded(allow_degraded)
            .dry_run(true)
            .build();

        plan_recover(&runner, &fs, &params)
            .expect("plan_recover should render a dry-run preview")
            .preview()
            .render()
    }

    /// Dry-run plan for `journal` under `f`'s state paths, so a test can
    /// name the header backup path the preview derives from them.
    fn recover_dry_run_plan(
        f: &PoolFixture,
        journal: journal::Journal,
        fs: MockFs,
        runner: MockRunner,
    ) -> RecoverPlan {
        journal::write_journal(&f.paths, &journal).unwrap();
        let params = f
            .recover_params()
            .passphrase_file(None)
            .allow_degraded(true)
            .dry_run(true)
            .build();
        plan_recover(&runner, &fs, &params).expect("plan_recover should plan a dry-run preview")
    }

    fn recover_dry_run_steps(
        f: &PoolFixture,
        journal: journal::Journal,
        fs: MockFs,
        runner: MockRunner,
    ) -> Vec<Step> {
        recover_dry_run_plan(f, journal, fs, runner)
            .work_plan
            .render_steps()
    }

    fn rendered_step_block<'a>(rendered: &'a str, needle: &str) -> &'a str {
        let needle_pos = rendered
            .find(needle)
            .unwrap_or_else(|| panic!("missing step containing {needle:?}: {rendered:?}"));
        let block_start = rendered[..needle_pos].rfind('\n').map_or(0, |pos| pos + 1);
        let after_first_line = rendered[block_start..]
            .find('\n')
            .map_or(rendered.len(), |pos| block_start + pos + 1);
        let tail = &rendered[after_first_line..];
        let block_end = if tail.starts_with('[') {
            after_first_line
        } else {
            tail.find("\n[")
                .map_or(rendered.len(), |pos| after_first_line + pos + 1)
        };
        &rendered[block_start..block_end]
    }

    fn closed_two_disk_dry_run_runner() -> MockRunner {
        let (mp_req, mp_out) = mountpoint_fail();
        MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"])
    }

    fn closed_disk1_dry_run_runner() -> MockRunner {
        let (mp_req, mp_out) = mountpoint_fail();
        MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_mapper_closed("braid-disk1")
    }

    fn closed_replace_dry_run_runner() -> MockRunner {
        let (mp_req, mp_out) = mountpoint_fail();
        MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-new",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-new"])
    }

    // Intent
    // Verify an already-mounted PoolMutation dry-run renders no-op rows for
    // already-live returned and fresh add targets.
    //
    // Why it exists
    // The executor skips replay for targets that are already live. The
    // preview must not advertise destructive replay commands for the common
    // crash-after-pool.json-save window.
    //
    // Scenario
    // Mixed returned/fresh add journal where disk1, disk2, and disk3 are all
    // already in the mounted live pool.
    #[test]
    fn plan_recover_dry_run_pool_mutation_already_mounted_all_live_mixed_modes_renders_safe_placeholders()
     {
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = with_three_disk_pool_probe(MockRunner::default().with_output(mp_req, mp_out));

        let rendered = render_recover_dry_run(
            mixed_pool_mutation_add_journal(),
            MockFs::new(&[])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222")
                .with_luks_dm("braid-disk3", "33333333-3333-3333-3333-333333333333"),
            runner,
            false,
        );

        assert!(
            rendered.contains(
                "reconcile journaled add targets against live pool (no replay needed: all targets already live)"
            ),
            "all-live reconcile header should be annotated: {rendered:?}",
        );
        assert!(
            rendered.contains(
                "[safe] replay verified returned-disk add /dev/mapper/braid-disk2 (skipped: target already live in pool)"
            ),
            "returned target should render a safe skip placeholder: {rendered:?}",
        );
        assert!(
            rendered.contains(
                "[safe] replay fresh add target /dev/disk/by-id/virtio-disk3 (skipped: target already live in pool)"
            ),
            "fresh target should render a safe skip placeholder: {rendered:?}",
        );
        assert!(
            !rendered.contains("$ cryptsetup luksFormat")
                && !rendered.contains("$ cryptsetup luksAddKey")
                && !rendered.contains("$ cryptsetup luksHeaderBackup")
                && !rendered.contains("$ cryptsetup open")
                && !rendered.contains("$ wipefs")
                && !rendered.contains("$ btrfs device add")
                && !rendered.contains("$ btrfs device scan --forget"),
            "all-live PoolMutation dry-run must not render replay argv rows: {rendered:?}",
        );
    }

    // Intent
    // Verify an already-mounted PoolMutation dry-run skips targets proven
    // live while keeping conditional replay rows for targets absent from the
    // planner's live snapshot.
    //
    // Why it exists
    // The executor can open/scan a committed but closed returned target before
    // replay, so the preview must describe that concrete rows are conditional.
    //
    // Scenario
    // Two returned targets are journaled; disk2 is already live, while disk3
    // is journaled but absent from the probed mounted pool.
    #[test]
    fn plan_recover_dry_run_pool_mutation_already_mounted_partial_live_returned_conditional_replay()
    {
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = with_two_disk_pool_probe(MockRunner::default().with_output(mp_req, mp_out));

        let rendered = render_recover_dry_run(
            two_target_recoverable_pool_mutation_add_journal(),
            MockFs::new(&[])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222"),
            runner,
            false,
        );

        assert!(
            rendered.contains("reconcile journaled add targets against live pool"),
            "missing reconcile header: {rendered:?}",
        );
        assert!(
            !rendered.contains("no replay needed: all targets already live"),
            "mixed live-set header must stay unannotated: {rendered:?}",
        );

        let disk2_block = rendered_step_block(
            &rendered,
            "replay verified returned-disk add /dev/mapper/braid-disk2",
        );
        assert!(
            disk2_block.contains("(skipped: target already live in pool)"),
            "disk2 should be a skip placeholder: {disk2_block:?}",
        );
        assert!(
            !disk2_block.contains("$ "),
            "disk2 placeholder must not render argv rows: {disk2_block:?}",
        );

        let disk3_block = rendered_step_block(
            &rendered,
            "replay verified returned-disk add /dev/mapper/braid-disk3",
        );
        assert!(
            disk3_block.contains(
                "(skipped at runtime if open/scan reconciliation makes target live before replay)"
            ),
            "disk3 replay row should advertise runtime skip: {disk3_block:?}",
        );
        assert!(
            disk3_block.contains("$ btrfs device scan --forget /dev/mapper/braid-disk3")
                && disk3_block.contains("$ wipefs --all --types btrfs /dev/mapper/braid-disk3")
                && disk3_block.contains(
                    "$ btrfs device add --enqueue -f /dev/mapper/braid-disk3 /mnt/storage"
                ),
            "disk3 replay row should render returned-target argv rows: {disk3_block:?}",
        );
    }

    // Intent
    // Verify a not-mounted PoolMutation dry-run renders fresh-target setup as
    // conditional replay, with no format row.
    //
    // Why it exists
    // The planner has no mounted live-set snapshot in this shape, but the
    // executor can still open/scan a committed target before replay. Recover
    // never formats a disk (finding 261022-1230-002), so the row names the
    // LUKS header the replay demands instead of a luksFormat command.
    //
    // Scenario
    // Fresh disk2 add journal with the pool offline and disk2 not visible to
    // the planner; recover will mount disk1 first, then conditionally replay
    // disk2 setup/add.
    #[test]
    fn plan_recover_dry_run_pool_mutation_not_mounted_fresh_conditional_replay_without_format_row()
    {
        let rendered = render_recover_dry_run(
            fresh_pool_mutation_add_journal(vec!["--label".into(), "braid-disk2".into()], None),
            MockFs::new(&["/dev/disk/by-id/virtio-disk1"]),
            closed_disk1_dry_run_runner(),
            false,
        );

        assert!(
            rendered.contains("reconcile journaled add targets against live pool"),
            "missing reconcile header: {rendered:?}",
        );
        assert!(
            !rendered.contains("no replay needed: all targets already live"),
            "offline dry-run must not use the all-live annotation: {rendered:?}",
        );

        let disk2_block = rendered_step_block(
            &rendered,
            "replay fresh add target /dev/disk/by-id/virtio-disk2",
        );
        assert!(
            disk2_block.contains(
                "(skipped at runtime if open/scan reconciliation makes target live before replay)"
            ),
            "fresh replay row should advertise runtime skip: {disk2_block:?}",
        );
        assert!(
            disk2_block.contains(
                "(refused at runtime unless the disk shows a LUKS header with the journaled UUID and the 'braid-disk2' label)"
            ),
            "fresh replay row should name the header the replay demands: {disk2_block:?}",
        );
        assert!(
            !disk2_block.contains("luksFormat"),
            "recover must never render a luksFormat row: {disk2_block:?}",
        );
        assert!(
            disk2_block.contains("$ cryptsetup luksHeaderBackup --header-backup-file")
                && disk2_block.contains("braid-disk2.luksheader /dev/disk/by-id/virtio-disk2")
                && disk2_block.contains(
                    "$ cryptsetup open --type luks '--key-file=-' --perf-no_read_workqueue --perf-no_write_workqueue /dev/disk/by-id/virtio-disk2 braid-disk2"
                )
                && disk2_block.contains(
                    "$ btrfs device add --enqueue /dev/mapper/braid-disk2 /mnt/storage"
                )
                && !disk2_block.contains(
                    "$ btrfs device add --enqueue -f /dev/mapper/braid-disk2 /mnt/storage"
                ),
            "fresh replay row should render non-force setup/add argv rows: {disk2_block:?}",
        );
    }

    /* Intent
     * Verify a not-mounted recover dry-run previews the full remount cycle.
     *
     * Why it exists
     * Execution unmounts, forgets btrfs scan state, closes mappers, reopens
     * LUKS, scans, and remounts before probing the recovered pool. The
     * dry-run preview must not hide that offline window.
     *
     * Scenario
     * Interrupted replace with disk1 and new present, old absent, and
     * --allow-degraded supplied so the initial mount plan succeeds.
     */
    #[test]
    fn plan_recover_dry_run_includes_remount_cycle_when_not_mounted() {
        let rendered = render_recover_dry_run(
            replace_journal(),
            MockFs::new(&["/dev/disk/by-id/virtio-disk1", "/dev/disk/by-id/virtio-new"]),
            closed_replace_dry_run_runner(),
            true,
        );

        assert!(
            rendered.contains("unmount /mnt/storage (recover remount cycle)"),
            "missing cycle unmount step: {rendered:?}",
        );
        assert!(
            rendered.contains(
                "$ btrfs device scan --forget /dev/mapper/braid-disk1 /dev/mapper/braid-new"
            ),
            "missing pool-scoped scan --forget command: {rendered:?}",
        );
        assert!(
            rendered.contains("close LUKS mapper braid-disk1 (recover remount cycle)")
                && rendered.contains("close LUKS mapper braid-new (recover remount cycle)"),
            "missing cycle close steps: {rendered:?}",
        );
        assert!(
            rendered.contains(
                "LUKS open /dev/disk/by-id/virtio-disk1 -> braid-disk1 (recover remount cycle)"
            ) && rendered.contains(
                "LUKS open /dev/disk/by-id/virtio-new -> braid-new (recover remount cycle)"
            ),
            "missing cycle reopen steps: {rendered:?}",
        );
        assert!(
            rendered.contains("mount -> /mnt/storage (recover remount cycle, degraded)"),
            "missing degraded cycle mount step: {rendered:?}",
        );
    }

    /* Intent
     * Verify an already-mounted recover dry-run does not preview the remount
     * cycle.
     *
     * Why it exists
     * Execution only runs relock_and_remount when recover just mounted the
     * pool. Showing the cycle for an already-mounted pool would overstate
     * the work and incorrectly warn about an offline window.
     *
     * Scenario
     * Interrupted add with the pool already mounted; dry-run reconciliation
     * probes the live pool and then renders only state recovery and replay
     * placeholders.
     */
    #[test]
    fn plan_recover_dry_run_omits_remount_cycle_when_already_mounted() {
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        let rendered = render_recover_dry_run(
            two_disk_journal(),
            MockFs::new(&[])
                .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111")
                .with_luks_dm("braid-disk2", "22222222-2222-2222-2222-222222222222"),
            runner,
            false,
        );

        assert!(
            !rendered.contains("recover remount cycle"),
            "already-mounted preview must not show the remount cycle: {rendered:?}",
        );
    }

    /* Intent
     * Verify replace dry-run previews the kernel dev_replace wait only when
     * recover needs to mount the pool.
     *
     * Why it exists
     * Interrupted replace can resume kernel dev_replace work on remount and
     * block recover before the relock cycle. Operators need that long-running
     * possibility in the dry-run output.
     *
     * Scenario
     * Replace journal where disk1 and new are present, old is absent, and
     * recover will mount degraded before rebuilding pool.json.
     */
    #[test]
    fn plan_recover_dry_run_replace_not_mounted_includes_dev_replace_wait() {
        let rendered = render_recover_dry_run(
            replace_journal(),
            MockFs::new(&["/dev/disk/by-id/virtio-disk1", "/dev/disk/by-id/virtio-new"]),
            closed_replace_dry_run_runner(),
            true,
        );

        assert!(
            rendered
                .contains("wait for kernel dev_replace to finish (skipped if no running replace)"),
            "replace preview should include the dev_replace wait: {rendered:?}",
        );
        assert!(
            !rendered.contains("$ btrfs replace status"),
            "conditional wait placeholder must not render a command: {rendered:?}",
        );
    }

    /* Intent
     * Verify non-replace dry-run does not preview the kernel dev_replace wait.
     *
     * Why it exists
     * Add, remove, and remove-missing do not have interrupted kernel
     * dev_replace work that auto-resumes on mount. Showing the wait outside
     * replace would make dry-run noisier and less accurate.
     *
     * Scenario
     * Interrupted add with a not-mounted degraded open plan.
     */
    #[test]
    fn plan_recover_dry_run_add_not_mounted_omits_dev_replace_wait() {
        let rendered = render_recover_dry_run(
            two_disk_journal(),
            MockFs::new(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ]),
            closed_two_disk_dry_run_runner(),
            true,
        );

        assert!(
            !rendered.contains("dev_replace"),
            "add preview must not include replace-only wait rows: {rendered:?}",
        );
    }

    /* Intent
     * Verify the remount cycle close set includes an existing mapper even
     * when that disk will not be reopened by the cycle.
     *
     * Why it exists
     * Execution closes every union mapper path that exists, not only disks
     * that probe as healthy. A dry-run based only on reopen names would miss
     * damaged or absent disks whose mapper is still live.
     *
     * Scenario
     * Interrupted replace where old's by-id path is absent but
     * /dev/mapper/braid-old exists at recover planning time.
     */
    #[test]
    fn plan_recover_dry_run_cycle_close_set_includes_absent_open_mapper() {
        let rendered = render_recover_dry_run(
            replace_journal(),
            MockFs::new(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-new",
                "/dev/mapper/braid-old",
            ]),
            closed_replace_dry_run_runner(),
            true,
        );

        assert!(
            rendered.contains(
                "$ btrfs device scan --forget /dev/mapper/braid-disk1 /dev/mapper/braid-old /dev/mapper/braid-new"
            ),
            "scan --forget should include absent-but-open old mapper: {rendered:?}",
        );
        assert!(
            rendered.contains("close LUKS mapper braid-old (recover remount cycle)"),
            "close set should include old mapper: {rendered:?}",
        );
        assert!(
            !rendered.contains("LUKS open /dev/disk/by-id/virtio-old"),
            "reopen set should not include absent old disk: {rendered:?}",
        );
    }

    /* Intent
     * Verify the remount cycle reopen set excludes a by-id-present disk
     * classified LuksHeaderUnreadable, even when its mapper path exists.
     *
     * Why it exists
     * The cycle reopen set must come from healthy probe events, not from
     * by-id path existence. A regression back to `fs.exists(by_id)` would
     * incorrectly preview reopening a disk whose LUKS header cannot be used.
     *
     * Scenario
     * Interrupted replace where old's by-id and mapper paths both exist, but
     * `luksUUID` fails so old is classified PresentNotLuks and reported as an
     * unreadable header.
     */
    #[test]
    fn plan_recover_dry_run_cycle_reopen_set_excludes_unreadable_header_disk() {
        let runner = closed_two_disk_dry_run_runner()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                err_raw("cryptsetup luksUUID", 1, "LUKS metadata corrupted"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-new")
            .with_mapper_closed("braid-new");

        let rendered = render_recover_dry_run(
            replace_journal(),
            MockFs::new(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-new",
                "/dev/disk/by-id/virtio-old",
                "/dev/mapper/braid-old",
            ]),
            runner,
            true,
        );

        assert!(
            rendered.contains("[skip] disk old: LUKS header unreadable"),
            "test setup should classify old as unreadable, got: {rendered:?}",
        );
        assert!(
            rendered.contains("close LUKS mapper braid-old (recover remount cycle)"),
            "close set should include unreadable old mapper: {rendered:?}",
        );
        assert!(
            !rendered.contains(
                "LUKS open /dev/disk/by-id/virtio-old -> braid-old (recover remount cycle)"
            ),
            "reopen set should not include unreadable old disk: {rendered:?}",
        );
    }

    /* Intent
     * Verify the cycle mount command uses the cycle's first reopen disk, not
     * the initial open plan's mount device.
     *
     * Why it exists
     * Mixed open/closed states can make the initial plan mount from the first
     * closed disk while the post-close cycle reopens from the first healthy
     * union disk. Dry-run must mirror the re-planned cycle device.
     *
     * Scenario
     * disk1 mapper is already open, new is closed, old is absent. The
     * initial mount uses braid-new, while the cycle mount uses braid-disk1.
     */
    #[test]
    fn plan_recover_dry_run_cycle_mount_uses_first_reopen_not_initial_mount_device() {
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-new",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-new",
            ])
            .with_mapper_open(
                "braid-disk1",
                "/dev/vda",
                "11111111-1111-1111-1111-111111111111",
            )
            .with_mapper_closed("braid-new");

        let rendered = render_recover_dry_run(
            replace_journal(),
            MockFs::new(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-new",
                "/dev/mapper/braid-disk1",
            ])
            .with_luks_dm("braid-disk1", "11111111-1111-1111-1111-111111111111"),
            runner,
            true,
        );

        let initial = "$ mount -o 'noatime,skip_balance,subvolid=5,degraded,nosuid,nodev' /dev/mapper/braid-new /mnt/storage";
        let cycle = "$ mount -o 'noatime,skip_balance,subvolid=5,degraded,nosuid,nodev' /dev/mapper/braid-disk1 /mnt/storage";
        let initial_pos = rendered
            .find(initial)
            .unwrap_or_else(|| panic!("initial mount should use braid-new: {rendered:?}"));
        let cycle_pos = rendered
            .find(cycle)
            .unwrap_or_else(|| panic!("cycle mount should use braid-disk1: {rendered:?}"));
        assert!(
            initial_pos < cycle_pos,
            "initial mount should precede cycle mount: {rendered:?}",
        );
    }

    /* Intent
     * Verify add dry-run previews post-mutation balance replay as conditional
     * placeholders without command lines, naming the hard convert live add
     * runs.
     *
     * Why it exists
     * Execution replays the balance only after an idle balance-status check
     * and only when the live pool can host RAID1. Dry-run must describe the
     * conditional work without promising command argv rows, and must name
     * the same convert the replay runs.
     *
     * Scenario
     * Interrupted add with a not-mounted degraded open plan.
     */
    #[test]
    fn plan_recover_dry_run_add_post_mutation_placeholders_have_no_commands() {
        let rendered = render_recover_dry_run(
            two_disk_journal(),
            MockFs::new(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ]),
            closed_two_disk_dry_run_runner(),
            true,
        );

        let write_pos = rendered
            .find("write recovered pool.json")
            .unwrap_or_else(|| panic!("missing write step: {rendered:?}"));
        let status_pos = rendered
            .find("check btrfs balance status /mnt/storage is idle before RAID1 replay")
            .unwrap_or_else(|| panic!("missing balance status placeholder: {rendered:?}"));
        let balance_pos = rendered
            .find("btrfs balance -dconvert=raid1 -mconvert=raid1 /mnt/storage (skipped if the pool is degraded or has <2 devices)")
            .unwrap_or_else(|| panic!("missing hard balance placeholder: {rendered:?}"));
        let clear_pos = rendered
            .find("clear pending-op.json")
            .unwrap_or_else(|| panic!("missing clear step: {rendered:?}"));

        assert!(
            write_pos < status_pos && status_pos < balance_pos && balance_pos < clear_pos,
            "post-mutation placeholders must sit between write and clear: {rendered:?}",
        );
        assert!(
            !rendered.contains("$ btrfs balance status")
                && !rendered.contains("$ btrfs balance start"),
            "conditional placeholders must not render btrfs balance commands: {rendered:?}",
        );
        assert!(
            !rendered.contains("-dconvert=raid1,soft"),
            "add recovery must not preview the soft convert: {rendered:?}",
        );
    }

    #[test]
    fn plan_recover_dry_run_post_add_balance_only_has_no_target_replay() {
        let rendered = render_recover_dry_run(
            committed_two_disk_add_journal(),
            MockFs::new(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ]),
            closed_two_disk_dry_run_runner(),
            false,
        );

        assert!(
            rendered.contains("check btrfs balance status /mnt/storage is idle before RAID1 replay")
                && rendered.contains(
                    "btrfs balance -dconvert=raid1 -mconvert=raid1 /mnt/storage (skipped if the pool is degraded or has <2 devices)"
                ),
            "post-add preview should include only balance completion work: {rendered:?}",
        );
        assert!(
            !rendered.contains("replay fresh add target")
                && !rendered.contains("replay verified returned-disk add")
                && !rendered.contains("wipefs")
                && !rendered.contains("btrfs device add"),
            "PostAddBalanceRaid1 dry-run must not show target replay commands: {rendered:?}",
        );
    }

    /* Intent
     * Verify live-source replace dry-run previews post-mutation resize but
     * omits RAID1 balance replay when no degraded repair is owed.
     *
     * Why it exists
     * Execution resolves the replacement disk devid from the post-mount live
     * pool, skips resize if replacement did not commit, and gates balance
     * replay on the journaled restore_raid1_after_commit flag.
     *
     * Scenario
     * Interrupted replace where disk1 and new are present and old is absent.
     */
    #[test]
    fn plan_recover_dry_run_replace_replay_placeholders_have_no_commands() {
        let rendered = render_recover_dry_run(
            replace_journal(),
            MockFs::new(&["/dev/disk/by-id/virtio-disk1", "/dev/disk/by-id/virtio-new"]),
            closed_replace_dry_run_runner(),
            true,
        );

        assert!(
            rendered.contains(
                "btrfs filesystem resize <devid>:max /mnt/storage (skipped if replacement did not commit; devid for 'new' resolved post-mount)"
            ),
            "replace preview should include the resize replay placeholder: {rendered:?}",
        );
        assert!(
            !rendered.contains("$ btrfs filesystem resize"),
            "conditional resize placeholder must not render a command: {rendered:?}",
        );
        assert!(
            !rendered.contains("$ btrfs balance status") && !rendered.contains("-dconvert=raid1"),
            "live-source replace preview must not include RAID1 balance replay: {rendered:?}",
        );
    }

    // Intent: an uncommitted-replace dry-run shows the keyfile enrollment and
    //   LUKS header backup recover runs on the new disk, ahead of the pool.json
    //   write, each marked as skipped if the replacement committed.
    // Why it exists: finish_uncommitted_replace_recovery runs luksAddKey and
    //   luksHeaderBackup on the new disk before it clears the journal, but the
    //   ReplacePoolMutation preview rendered only the committed branch's tail,
    //   so `recover --dry-run` hid a keyslot write.
    // Scenario: `braid replace --enroll DIR` onto a fresh disk crashed after
    //   the journal write; the operator runs `braid recover --dry-run`.
    #[test]
    fn plan_recover_dry_run_replace_pool_mutation_previews_uncommitted_luks_writes() {
        let f = PoolFixture::empty();
        let key_file = std::path::PathBuf::from("/run/keys/braid-new.key");
        let steps = recover_dry_run_steps(
            &f,
            replace_fresh_luks_journal(key_file.clone()),
            MockFs::new(&["/dev/disk/by-id/virtio-disk1", "/dev/disk/by-id/virtio-new"]),
            closed_replace_dry_run_runner(),
        );

        let add_key = CmdRequest::CryptsetupLuksAddKeyFile {
            device: "/dev/disk/by-id/virtio-new".into(),
            key_file_path: key_file.display().to_string(),
        };
        let backup = CmdRequest::CryptsetupLuksHeaderBackup {
            device: "/dev/disk/by-id/virtio-new".into(),
            backup_path: f
                .paths
                .luks_headers_dir()
                .join("braid-new.luksheader")
                .display()
                .to_string(),
        };
        let step_running = |command: &CmdRequest| {
            steps
                .iter()
                .position(|step| step.commands.contains(command))
                .unwrap_or_else(|| panic!("dry-run must preview {command:?}: {steps:#?}"))
        };
        let add_key_pos = step_running(&add_key);
        let backup_pos = step_running(&backup);
        let write_pos = steps
            .iter()
            .position(|step| step.description.starts_with("write recovered pool.json"))
            .unwrap_or_else(|| panic!("missing pool.json write: {steps:#?}"));
        assert!(
            add_key_pos < backup_pos && backup_pos < write_pos,
            "enroll then backup must precede the pool.json write: {steps:#?}"
        );
        for pos in [add_key_pos, backup_pos] {
            assert!(
                steps[pos]
                    .description
                    .contains("skipped if replacement committed"),
                "uncommitted-only LUKS row must name its commit condition: {:?}",
                steps[pos]
            );
        }
    }

    // Intent: an uncommitted-replace dry-run marks the owed RAID1 balance as
    //   skipped if the replacement did not commit, next to the shared gate's
    //   condition.
    // Why it exists: recover runs the owed balance only on the committed
    //   branch, but the preview named only the degraded / <2 devices skip, so
    //   it over-reported the balance for an uncommitted replace.
    // Scenario: a replace that owes the RAID1 restore crashed before btrfs
    //   replace committed; the operator runs `braid recover --dry-run`.
    #[test]
    fn plan_recover_dry_run_replace_pool_mutation_balance_names_commit_condition() {
        let journal = replace_journal_in_phase(
            journal::ReplacePhase::PoolMutation,
            true,
            journal::ReplaceJournalSource::Live {
                old_devid: Devid::new(2),
                old_mapper: MapperName::from_basename("braid-old".into()),
            },
        );
        let rendered = render_recover_dry_run(
            journal,
            MockFs::new(&["/dev/disk/by-id/virtio-disk1", "/dev/disk/by-id/virtio-new"]),
            closed_replace_dry_run_runner(),
            true,
        );

        assert!(
            rendered.contains(
                "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft /mnt/storage \
                 (skipped if replacement did not commit, or if the pool is degraded or has <2 devices)"
            ),
            "replace PoolMutation balance row must name the commit condition: {rendered:?}",
        );
    }

    // Intent: for every replace target mode, the uncommitted branch of
    //   replace recovery issues exactly the luksAddKey and luksHeaderBackup
    //   requests its dry-run previews, in the same order, and an
    //   existing-LUKS disk with no keyfile gets no LUKS write, no passphrase
    //   read and no credential check.
    // Why it exists: the preview and the replay both read the planned
    //   UncommittedReplaceWrites, so a write added outside that value would
    //   reach the disk unpreviewed again.
    // Scenario: replace crashed before btrfs replace committed, for each of
    //   ExistingLuks and FreshLuks, with and without --enroll; the new disk
    //   reads as LUKS with the journaled UUID and braid label, and the
    //   keyfile does not open it yet.
    #[test]
    fn uncommitted_replace_recovery_runs_exactly_the_previewed_luks_writes() {
        let key_dir = tempfile::TempDir::new().unwrap();
        let key_file = write_valid_keyfile(&key_dir, "braid-new.key");
        let fresh_without_key_file = {
            let mut journal = replace_fresh_luks_journal(key_file.clone());
            let OpKind::Replace(journal::ReplaceOp { new_target, .. }) = &mut journal.op else {
                unreachable!("replace_fresh_luks_journal returns Replace");
            };
            let journal::ReplaceJournalMode::FreshLuks {
                enroll_key_file, ..
            } = &mut new_target.mode
            else {
                unreachable!("replace_fresh_luks_journal returns FreshLuks");
            };
            *enroll_key_file = None;
            journal
        };
        let cases = [
            ("existing LUKS without keyfile", replace_journal(), 0),
            (
                "existing LUKS with keyfile",
                replace_existing_luks_with_enroll_journal(key_file.clone()),
                2,
            ),
            ("fresh LUKS without keyfile", fresh_without_key_file, 1),
            (
                "fresh LUKS with keyfile",
                replace_fresh_luks_journal(key_file.clone()),
                2,
            ),
        ];
        let is_luks_write = |request: &CmdRequest| {
            matches!(
                request,
                CmdRequest::CryptsetupLuksAddKeyFile { .. }
                    | CmdRequest::CryptsetupLuksHeaderBackup { .. }
            )
        };

        for (case, journal, expected_writes) in cases {
            let f = PoolFixture::empty();
            let plan = recover_dry_run_plan(
                &f,
                journal,
                MockFs::new(&["/dev/disk/by-id/virtio-disk1", "/dev/disk/by-id/virtio-new"]),
                closed_replace_dry_run_runner(),
            );
            let previewed: Vec<CmdRequest> = plan
                .work_plan
                .render_steps()
                .into_iter()
                .flat_map(|step| step.commands)
                .filter(is_luks_write)
                .collect();
            assert_eq!(
                previewed.len(),
                expected_writes,
                "{case}: previewed LUKS writes: {previewed:?}"
            );
            let Some(RecoverWorkAction::Complete(completion)) = plan.work_plan.actions.last()
            else {
                panic!("{case}: recover plan must end in a completion");
            };
            let RecoverCompletion::ReplacePoolMutation {
                identity,
                new_name,
                new_target,
                source,
                uncommitted_writes,
                owed_balance,
            } = completion.as_ref()
            else {
                panic!("{case}: replace PoolMutation journal must plan a ReplacePoolMutation");
            };

            let runner = MockRunner::default()
                .with_output(
                    CmdRequest::CryptsetupLuksUuid {
                        device: "/dev/disk/by-id/virtio-new".into(),
                    },
                    cryptsetup_uuid_ok(
                        "/dev/disk/by-id/virtio-new",
                        "33333333-3333-3333-3333-333333333333",
                    ),
                )
                .with_output(
                    CmdRequest::CryptsetupLuksDumpText {
                        device: "/dev/disk/by-id/virtio-new".into(),
                    },
                    luks_dump_label("braid-new"),
                )
                .with_mapper_closed("braid-new")
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/vda".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/vdb".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupTestPassphrase {
                        device: "/dev/disk/by-id/virtio-new".into(),
                    },
                    TEST_PASSPHRASE_BYTES.to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output(
                    CmdRequest::CryptsetupTestKeyFile {
                        device: "/dev/disk/by-id/virtio-new".into(),
                        key_file_path: key_file.display().to_string(),
                    },
                    err_raw("cryptsetup open --test-passphrase --key-file", 2, "No key"),
                )
                // Accept any LUKS write, so a request that differs from the
                // preview fails the comparison below, not a mock lookup.
                .with_handler(move |request| {
                    is_luks_write(request).then(|| Ok(ok_raw_empty("cryptsetup luks write")))
                });
            // A disk owed no LUKS write gets no passphrase source, so a read
            // fails the run at once instead of prompting.
            let no_passphrase = ScriptedPassphraseReader::new(Vec::<String>::new());
            let params = if expected_writes == 0 {
                f.recover_params()
                    .passphrase_file(None)
                    .tty(&no_passphrase)
                    .build()
            } else {
                f.recover_params().build()
            };

            execute_replace_pool_mutation_recovery(
                &runner,
                &MockFs::new(&["/dev/disk/by-id/virtio-new"]),
                &MockByIdResolver::default(),
                &params,
                None,
                &plan.work_plan.journal,
                &plan.work_plan.admission_membership,
                pool_state_disk1_and_old(),
                identity,
                new_name,
                new_target,
                source,
                uncommitted_writes.as_ref(),
                *owed_balance,
            )
            .unwrap_or_else(|e| panic!("{case}: uncommitted replace recovery failed: {e}"));

            let requests = runner.requests();
            // The header backup writer stages to `<path>.tmp` and renames it
            // over the previewed path (`cli/src/luks.rs#backup_luks_header_to`).
            let issued: Vec<CmdRequest> = requests
                .iter()
                .filter(|request| is_luks_write(request))
                .cloned()
                .map(|request| match request {
                    CmdRequest::CryptsetupLuksHeaderBackup {
                        device,
                        backup_path,
                    } => CmdRequest::CryptsetupLuksHeaderBackup {
                        device,
                        backup_path: backup_path
                            .strip_suffix(".tmp")
                            .unwrap_or_else(|| panic!("{case}: backup must stage to .tmp"))
                            .to_owned(),
                    },
                    other => other,
                })
                .collect();
            assert_eq!(issued, previewed, "{case}: replay and preview disagree");
            if expected_writes == 0 {
                assert!(
                    !requests
                        .iter()
                        .any(|r| matches!(r, CmdRequest::CryptsetupTestPassphrase { .. })),
                    "{case}: a disk owed no LUKS write needs no credential check: {requests:?}"
                );
            }
            assert!(!f.paths.pending_op_json().exists(), "{case}: journal kept");
        }
    }

    /* Intent
     * Verify remove-missing dry-run previews the balance replay placeholders.
     *
     * Why it exists
     * Remove-missing can leave incomplete post-mutation balancing just like add
     * and replace. The preview needs to name the idle gate and the follow-up
     * work.
     *
     * Scenario
     * Interrupted remove-missing with disk1 present, disk2 missing, and
     * --allow-degraded supplied.
     */
    #[test]
    fn plan_recover_dry_run_remove_missing_post_mutation_placeholders_are_shown() {
        let rendered = render_recover_dry_run(
            remove_missing_journal(),
            MockFs::new(&["/dev/disk/by-id/virtio-disk1"]),
            closed_disk1_dry_run_runner(),
            true,
        );

        assert!(
            rendered.contains("check btrfs balance status /mnt/storage is idle before RAID1 replay")
                && rendered.contains(
                    "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft /mnt/storage (skipped if the pool is degraded or has <2 devices)"
                ),
            "remove-missing preview should include balance replay placeholders: {rendered:?}",
        );
        assert!(
            !rendered.contains("btrfs filesystem resize <devid>:max"),
            "remove-missing preview must not include replace resize: {rendered:?}",
        );
    }

    /* Intent
     * Verify remove dry-run omits all post-mutation replay placeholders.
     *
     * Why it exists
     * Remove is intentionally recovered by rerunning braid remove rather than
     * resuming an ambiguous paused balance. The dry-run preview must preserve
     * that distinction.
     *
     * Scenario
     * Interrupted remove with disk1 present, disk2 missing, and
     * --allow-degraded supplied.
     */
    #[test]
    fn plan_recover_dry_run_remove_post_mutation_replay_rows_omitted() {
        let rendered = render_recover_dry_run(
            interrupted_remove_journal(None),
            MockFs::new(&["/dev/disk/by-id/virtio-disk1"]),
            closed_disk1_dry_run_runner(),
            true,
        );

        assert!(
            !rendered.contains("check btrfs balance status")
                && !rendered.contains("-dconvert=raid1")
                && !rendered.contains("btrfs filesystem resize <devid>:max"),
            "remove preview should omit replay placeholders: {rendered:?}",
        );
    }

    /* Intent: a degraded-refusal at the planner boundary preserves the
     * entry banner + per-disk probe notes on `PlanFailure::notes`
     * in order, and routes the error as
     * `RecoverError::Mount(MountError::DegradedRefused(_))`.
     *
     * Why it exists: recover's preserved-context contract says the
     * entry banner and accumulated probe context survive on
     * `PlanFailure::notes` so `cmd_recover` can render them to stderr
     * before the refusal message -- mirroring today's
     * `eprintln!(entry) + print_probe_events + ?` sequence. Without
     * this boundary test, a regression that dropped the banner from the
     * failure path or reordered the notes could still pass the
     * end-to-end CLI test (which only greps for substring markers)
     * while breaking the planner's documented contract.
     *
     * Scenario: 2-disk journal with union = {disk1, disk2, disk3};
     * disk1 + disk2 are present and closed, disk3 is absent;
     * allow_degraded=false. plan_open_pool accumulates
     * DiskAvailable(disk1), DiskAvailable(disk2), DiskAbsent(disk3),
     * then returns DegradedRefused.
     */
    #[test]
    fn plan_recover_preserves_notes_on_degraded_refused() {
        let f = PoolFixture::empty();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal = two_disk_journal();
        journal::write_journal(&f.paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/virtio-disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);

        let params = f
            .recover_params()
            .passphrase_file(None)
            .dry_run(true)
            .build();

        let failure = match plan_recover(&runner, &fs, &params) {
            Ok(_) => panic!("degraded refusal must surface as Err"),
            Err(failure) => failure,
        };

        let entry_banner = format_recover_entry(&journal);
        assert!(
            matches!(&failure.notes[0], PreviewNote::Info(msg) if msg == &entry_banner),
            "first note must be the entry banner, got: {:?}",
            failure.notes,
        );

        let per_disk: Vec<&PreviewNote> = failure
            .notes
            .iter()
            .filter(|n| matches!(n, PreviewNote::PerDisk { .. }))
            .collect();
        assert_eq!(
            per_disk.len(),
            3,
            "PlanFailure::notes must carry one per-disk note per union disk, got: {:?}",
            failure.notes,
        );
        assert!(
            matches!(
                per_disk[0],
                PreviewNote::PerDisk { name, level: NoteLevel::Ok, .. } if name == "disk1",
            ),
            "first per-disk note must be disk1 Ok, got: {:?}",
            per_disk[0],
        );
        assert!(
            matches!(
                per_disk[1],
                PreviewNote::PerDisk { name, level: NoteLevel::Ok, .. } if name == "disk2",
            ),
            "second per-disk note must be disk2 Ok, got: {:?}",
            per_disk[1],
        );
        assert!(
            matches!(
                per_disk[2],
                PreviewNote::PerDisk { name, level: NoteLevel::Skip, .. } if name == "disk3",
            ),
            "third per-disk note must be disk3 Skip, got: {:?}",
            per_disk[2],
        );

        let err = failure.error;
        assert!(
            matches!(&err, RecoverError::Mount(MountError::DegradedRefused(_))),
            "expected DegradedRefused, got: {err:?}",
        );
    }
}
