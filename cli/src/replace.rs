use crate::btrfs_ioctl::BtrfsDevInfo;
use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::{Config, luks_label_for, mapper_name};
use crate::confirm;
use crate::credential_verify::{
    Credential, CredentialVerifyError, CredentialVerifyTarget, verify_credential_for_targets,
};
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal;
use crate::luks::{
    BackingPathResolver, KeySlotState, LUKS_SLOT_KEYFILE, MapperOwnershipFailure, OwnershipError,
    backup_luks_header_post_mutation, check_key_slot, classify_mapper_ownership, ensure_luks_open,
    format_keyfile_asymmetry_warning, format_keyfile_enrollment_probe_failure,
    format_target_keyfile_probe_failure, luks_format, luks_header_backup_path,
    probe_pool_keyfile_enrollment, read_passphrase,
};
use crate::mapper_close::{CloseContext, close_mapper_best_effort};
use crate::membership::{self, PoolMembership};
use crate::parse::{parse_btrfs_device_stats, parse_cryptsetup_luks_uuid};
use crate::pool::{pool_replace_device, pool_resize_device};
use crate::preflight;
use crate::preview::{self, PerDiskStyle, PlanFailure, Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{Filesystem, ProbeError, probe_config_disk, probe_pool};
use crate::probe_mapper_uuid::{
    MapperOwnership, probe_observed_mapper_uuid, warn_close_skipped_inactive,
};
use crate::progress::{self, ProgressOutput};
use crate::repair_hint;
use crate::state_paths::StatePaths;
use crate::status_tag::{StatusTag, color_enabled_for_stderr, status_line};
use crate::types::*;
use std::fmt;
use std::path::{Path, PathBuf};

/// Which uniqueness axis `assert_new_uuid_unique` collided on. Rendered as
/// `"membership"` and `"live_pool"` so the operator-facing message names
/// the same surface the pre-migration text contract used.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuplicateUuidScope {
    Membership,
    LivePool,
}

impl fmt::Display for DuplicateUuidScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DuplicateUuidScope::Membership => f.write_str("membership"),
            DuplicateUuidScope::LivePool => f.write_str("live_pool"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ReplaceError {
    #[error("{0}")]
    Validation(String),
    #[error("{source}\n{advice}")]
    JournalLifecycle {
        #[source]
        source: Box<ReplaceError>,
        advice: String,
    },
    /// Pre-journal-write refusal raised when the new target's UUID
    /// (generated for fresh-LUKS or probed for existing-LUKS) collides
    /// with a UUID already present in membership (excluding `old_uuid`,
    /// which is being replaced) or with a UUID observed in the live
    /// `pool.devices` set at planning time. Refused BEFORE any journal
    /// write and BEFORE any `CryptsetupLuksFormat` so the residual blast
    /// radius is bounded.
    #[error(
        "duplicate LUKS UUID {uuid} for new replace target: already present in {scope} -- detach the conflicting disk before retrying"
    )]
    DuplicateUuid {
        uuid: LuksUuid,
        scope: DuplicateUuidScope,
    },
    /// Operator passed `--luks-format-arg` containing a braid-managed
    /// cryptsetup option (`--uuid`, `--label`). Surfaced through
    /// `ReplaceError` (rather than `LuksFormatExtraOptsError` directly) so
    /// the CLI matches one error type at the boundary; mirrors
    /// `AddError::ManagedFormatFlag`.
    #[error("{0}")]
    ManagedFormatFlag(#[from] LuksFormatExtraOptsError),
    /// New-target identity drift between planning-time probe (or the
    /// generated `new_uuid` for FreshLuks via journal replay) and the
    /// live disk at `new_target.by_id` immediately before
    /// `ensure_luks_open`. Symmetric with the post-commit close
    /// double-drift probe but targets the open boundary so a foreign disk
    /// swap-in-place cannot pass through `cryptsetup open` to the
    /// destructive `btrfs replace start`.
    #[error(
        "replace target '{by_id}' LUKS UUID mismatch: expected {expected}, found {observed} -- detach the foreign disk and retry"
    )]
    NewTargetUuidMismatchAtOpen {
        by_id: ByIdPath,
        expected: LuksUuid,
        observed: String,
    },
    /// Carries the conflicting mapper handle so remediation can close the exact
    /// open device that blocked the replace.
    #[error(
        "replace target '{by_id}' open mapper backing mismatch: mapper is backed by \
         '{found_path}', expected '{expected_path}' -- close the conflicting mapper \
         with 'sudo cryptsetup close {mapper}' and re-run."
    )]
    NewTargetMapperBackingMismatch {
        by_id: ByIdPath,
        mapper: MapperName,
        expected_path: String,
        found_path: String,
    },
    #[error(
        "replace target '{by_id}' open mapper backing-path check failed: could not \
         canonicalize '{resolved}' ({source_message}) -- check that the disk is plugged in \
         and that udev has populated /dev/disk/by-id/."
    )]
    NewTargetMapperBackingResolveError {
        by_id: ByIdPath,
        resolved: String,
        source_message: String,
    },
    /// Operator-supplied `--old <name>` did not resolve to a member of
    /// the persisted pool membership. The display name they typed has
    /// no `(uuid, member)` entry; planning aborts before any inhibitor,
    /// probe, or journal write.
    #[error(
        "'{name}' not found in pool.json membership -- no disk entry has this name. Pool membership may need manual repair."
    )]
    OldMemberNotFound { name: String },
    /// Operator's `--missing-id` disagrees with the old member's persisted
    /// non-null `devid`: `--old` resolves to a member recording one devid
    /// while `--missing-id` names another. A typo guard caught before any
    /// btrfs missing-set cross-check.
    #[error(
        "--old '{old_name}' records devid {pool_devid} in pool.json, but --missing-id was {supplied_devid}. --old and --missing-id disagree about which member is being replaced."
    )]
    OldDevidMismatch {
        old_name: String,
        pool_devid: Devid,
        supplied_devid: Devid,
    },
    /// Old member is being replaced via the missing path but the
    /// persisted membership row has no `devid`. Without a persisted
    /// devid there is no way to confirm the operator and btrfs are
    /// referring to the same physical slot.
    #[error(
        "'{name}' has no persisted devid in pool.json. Cannot confirm which missing btrfs devid corresponds to '{name}'; pool membership may need manual repair."
    )]
    OldMemberMissingDevid { name: String },
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

pub struct ReplaceParams<'a> {
    pub config: &'a Config,
    pub old_name: &'a str,
    pub new_name: &'a str,
    pub missing_id: Option<Devid>,
    pub dry_run: bool,
    pub yes: bool,
    pub passphrase_stdin: bool,
    pub passphrase_file: Option<&'a Path>,
    pub enroll_key_file: Option<&'a Path>,
    pub luks_format_extra_opts: &'a [String],
    pub progress: ProgressOutput,
    pub paths: &'a StatePaths,
    /// Seam for acquiring a logind sleep inhibitor before the irreversible
    /// portion of the replace. Production passes `&RealSleepInhibitor`;
    /// unit tests pass `&NoopSleepInhibitor` to avoid spawning subprocesses.
    pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
    /// Seam for the operator go/no-go prompt. Production prints the
    /// assembled prompt and reads from the tty; tests record the prompt
    /// and provide a deterministic verdict.
    pub confirm: &'a dyn confirm::Confirm,
    /// Sleeper seam for retrying transiently-busy mapper closes without
    /// slowing unit tests.
    pub sleeper: &'a dyn progress::Sleeper,
    /// Seam for resolving by-id paths and mapper backings to the same
    /// kernel block-device namespace at the already-open mapper boundary.
    pub backing_path_resolver: &'a dyn BackingPathResolver,
}

/// Dry-run preview source of truth for `braid replace` plus the preflight
/// state `execute()` needs to finish the operation. `notes` carries
/// plan-derived preflight diagnostics (busy-op Info, readonly-probe-fail
/// Warn) and keyfile enrollment diagnostics so both dry-run stdout and
/// real-run stderr see the same wording. The 1-disk leftover `WARNING:`
/// remains confirmation-only behind the `!params.yes` gate.
pub struct ReplacePlan {
    pub notes: Vec<PreviewNote>,
    work_plan: ReplaceWorkPlan,
}

#[derive(Debug, Clone)]
enum ReplaceTargetPrep {
    FreshLuks {
        /// Validated `--luks-format-arg` extras. Managed flags
        /// (`--uuid`, `--label`) are rejected by
        /// `LuksFormatExtraOpts::parse` at the CLI boundary inside
        /// `plan_replace`; the structured `uuid` and `label` fields on
        /// `CmdRequest::CryptsetupLuksFormat` carry the managed
        /// identity. No raw `--label braid-<name>` injection.
        extra_opts: LuksFormatExtraOpts,
        enroll_key_file: Option<KeyFilePath>,
    },
    ExistingLuks {
        mapper_open: bool,
        /// Resolved keyfile to enroll into LUKS slot 1 on the new disk.
        /// `Some(kf)` only when `--enroll DIR` was passed AND the new
        /// disk classified as `NeedsEnroll` (slot 1 empty). `None` for
        /// no-`--enroll`, or for the idempotent `AlreadyEnrolled` skip
        /// where slot 1 already authenticates with `kf`. Slot-1
        /// conflicts are rejected at planning time before any journal
        /// write.
        enroll_key_file: Option<KeyFilePath>,
    },
}

impl ReplaceTargetPrep {
    fn needs_luks_format(&self) -> bool {
        matches!(self, ReplaceTargetPrep::FreshLuks { .. })
    }
}

#[derive(Debug, Clone)]
struct ReplaceWorkPlan {
    config: Config,
    old_uuid: LuksUuid,
    old_name: DiskName,
    new_uuid: LuksUuid,
    new_name: DiskName,
    new_by_id: ByIdPath,
    pool: PoolState,
    replace_source: ReplaceSource,
    /// Lets `render_steps` derive header-backup paths without carrying
    /// `StatePaths` across the preview boundary.
    luks_headers_dir: PathBuf,
    target_prep: ReplaceTargetPrep,
    journal_target: journal::ReplaceJournalTarget,
    journal_source: journal::ReplaceJournalSource,
    restore_raid1_after_commit: bool,
    new_mapper: MapperName,
    new_mapper_path: String,
    /// Existing-pool-member credential-verify targets, resolved at plan time
    /// so each member's display `name` is UUID-joined through membership
    /// (decision 024) and survives mapper drift. Identity is carried by each
    /// target's `device`; the name is cosmetic. The new-disk target is built
    /// separately in `execute` from `new_name`/`new_by_id`.
    member_verify_targets: Vec<CredentialVerifyTarget>,
}

impl ReplaceWorkPlan {
    fn render_steps(&self) -> Vec<Step> {
        let mut steps = Vec::new();

        match &self.target_prep {
            ReplaceTargetPrep::FreshLuks {
                extra_opts,
                enroll_key_file,
            } => {
                let label = luks_label_for(&self.new_name);
                let header_backup_path =
                    luks_header_backup_path(&self.luks_headers_dir, &self.new_mapper);
                steps.push(Step {
                    risk: "destructive",
                    description: format!("LUKS format {}", self.new_by_id),
                    // preview variant: real uuid minted at execute; ADR-022
                    commands: vec![CmdRequest::CryptsetupLuksFormatPreview {
                        device: self.new_by_id.as_str().to_owned(),
                        label,
                        extra_opts: extra_opts.clone(),
                    }],
                });
                if let Some(kf) = enroll_key_file {
                    steps.push(Step {
                        risk: "safe",
                        description: format!("enroll keyfile -> LUKS slot 1 on {}", self.new_by_id),
                        commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
                            device: self.new_by_id.as_str().to_owned(),
                            key_file_path: kf.as_path().display().to_string(),
                        }],
                    });
                }
                steps.push(Step {
                    risk: "safe",
                    description: format!(
                        "LUKS header backup -> {}",
                        header_backup_path.as_path().display()
                    ),
                    commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
                        device: self.new_by_id.as_str().to_owned(),
                        backup_path: header_backup_path.as_path().display().to_string(),
                    }],
                });
                steps.push(Step {
                    risk: "safe",
                    description: format!("LUKS open -> {}", self.new_mapper),
                    commands: vec![CmdRequest::CryptsetupLuksOpen {
                        device: self.new_by_id.as_str().to_owned(),
                        mapper: self.new_mapper.clone(),
                    }],
                });
            }
            ReplaceTargetPrep::ExistingLuks {
                mapper_open,
                enroll_key_file,
            } => {
                if let Some(kf) = enroll_key_file {
                    let header_backup_path =
                        luks_header_backup_path(&self.luks_headers_dir, &self.new_mapper);
                    steps.push(Step {
                        risk: "safe",
                        description: format!("enroll keyfile -> LUKS slot 1 on {}", self.new_by_id),
                        commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
                            device: self.new_by_id.as_str().to_owned(),
                            key_file_path: kf.as_path().display().to_string(),
                        }],
                    });
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "LUKS header backup -> {}",
                            header_backup_path.as_path().display()
                        ),
                        commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
                            device: self.new_by_id.as_str().to_owned(),
                            backup_path: header_backup_path.as_path().display().to_string(),
                        }],
                    });
                }
                if !mapper_open {
                    steps.push(Step {
                        risk: "safe",
                        description: format!("LUKS open -> {}", self.new_mapper),
                        commands: vec![CmdRequest::CryptsetupLuksOpen {
                            device: self.new_by_id.as_str().to_owned(),
                            mapper: self.new_mapper.clone(),
                        }],
                    });
                }
            }
        }

        let devid = match &self.replace_source {
            ReplaceSource::Live { devid, .. } | ReplaceSource::Missing { devid } => *devid,
        };

        steps.push(Step {
            risk: "long",
            description: format!(
                "btrfs replace start {} {} {}",
                devid,
                self.new_mapper.dev_path(),
                self.config.mount_point()
            ),
            commands: vec![CmdRequest::BtrfsReplaceStart {
                devid,
                target_device: self.new_mapper_path.clone(),
                mount_point: self.config.mount_point().clone(),
            }],
        });

        if let ReplaceSource::Live { mapper, .. } = &self.replace_source {
            steps.push(Step {
                risk: "safe",
                description: format!("cryptsetup close {}", mapper),
                commands: vec![CmdRequest::CryptsetupClose {
                    mapper: mapper.clone(),
                }],
            });
        }

        steps.push(Step {
            risk: "safe",
            description: format!(
                "btrfs filesystem resize {}:max {}",
                devid,
                self.config.mount_point()
            ),
            commands: vec![CmdRequest::BtrfsFilesystemResize {
                devid,
                mount_point: self.config.mount_point().clone(),
            }],
        });

        if self.restore_raid1_after_commit {
            steps.push(Step {
                risk: "long",
                description:
                    "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft (restore redundancy)"
                        .into(),
                commands: vec![CmdRequest::BtrfsBalanceRaid1Soft {
                    mount_point: self.config.mount_point().clone(),
                }],
            });
        }

        steps
    }
}

impl ReplacePlan {
    /// Build a `Preview` carrying any plan-derived notes. The 1-disk
    /// leftover `WARNING:` line stays in `execute()` behind the
    /// `!params.yes` gate and does not appear here.
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
        params: &ReplaceParams<'_>,
    ) -> Result<(), ReplaceError> {
        let mut journal_state = journal::JournalWriteState::NotAttempted;
        self.execute_inner(runner, fs, params, &mut journal_state)
            .map_err(|error| {
                if journal_state == journal::JournalWriteState::NotAttempted {
                    error
                } else {
                    ReplaceError::JournalLifecycle {
                        source: Box::new(error),
                        advice: journal::mutation_error_advice(params.paths, journal_state),
                    }
                }
            })
    }

    fn execute_inner<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
        self,
        runner: &R,
        fs: &F,
        params: &ReplaceParams<'_>,
        journal_state: &mut journal::JournalWriteState,
    ) -> Result<(), ReplaceError> {
        let color_enabled = color_enabled_for_stderr();
        let ReplacePlan { notes, work_plan } = self;
        let ReplaceWorkPlan {
            config,
            old_uuid,
            old_name,
            new_uuid,
            new_name,
            new_by_id,
            pool,
            replace_source,
            luks_headers_dir: _,
            target_prep,
            journal_target: new_target,
            journal_source,
            restore_raid1_after_commit,
            new_mapper: new_mn,
            new_mapper_path,
            member_verify_targets,
        } = work_plan;

        // Render accumulated notes to stderr via the shared helper
        // BEFORE any mutation. Matches the other note-carrying commands so
        // preflight diagnostics surface identically across success,
        // failure, and dry-run stdout.
        emit_replace_notes_to_stderr(&notes);

        // Confirm
        if !params.yes {
            let old_underlying = match &replace_source {
                ReplaceSource::Live { .. } => pool.underlying_for_uuid(&old_uuid),
                ReplaceSource::Missing { .. } => None,
            };
            let old_hw = old_underlying.map(|u| confirm::query_disk_hw_info(runner, u));
            let new_hw = confirm::query_disk_hw_info(runner, new_by_id.as_str());
            let is_missing = matches!(&replace_source, ReplaceSource::Missing { .. });

            let mut prompt = format!(
                "{}\n",
                format_replace_confirm(
                    &ReplaceConfirmOld {
                        name: old_name.as_str(),
                        hw: old_hw.as_ref(),
                        source: &replace_source,
                    },
                    &ReplaceConfirmNew {
                        name: new_name.as_str(),
                        by_id: new_by_id.as_str(),
                        hw: &new_hw,
                        needs_luks_format: target_prep.needs_luks_format(),
                        is_rebuild: is_missing,
                    },
                    pool.total_devices,
                )
            );
            if pool.total_devices == 1 {
                prompt.push_str("WARNING: This replace leaves only 1 disk -- no redundancy.\n\n");
            }
            params
                .confirm
                .confirm(&prompt)
                .map_err(ReplaceError::Validation)?;
        }

        // Read passphrase
        let passphrase = read_passphrase(params.passphrase_file, params.passphrase_stdin)?;

        // Existing-pool-member targets were resolved at plan time (names
        // UUID-joined through membership; see `build_member_verify_targets`).
        // The new-disk target is appended here because its identity
        // (`new_name`/`new_by_id`) is op-level, not a live pool member.
        let mut credential_targets = member_verify_targets;
        let new_disk_target = match &target_prep {
            ReplaceTargetPrep::ExistingLuks { .. } => Some(
                CredentialVerifyTarget::named_candidate(&new_name, &new_by_id),
            ),
            ReplaceTargetPrep::FreshLuks { .. } => None,
        };
        if let Some(target) = &new_disk_target {
            credential_targets.push(target.clone());
        }

        if !credential_targets.is_empty() {
            match verify_credential_for_targets(
                runner,
                &credential_targets,
                Credential::Passphrase(&passphrase),
                color_enabled,
                emit_replace_stderr,
            ) {
                Ok(()) => {}
                Err(CredentialVerifyError::Rejected { target }) => {
                    let is_new_disk = new_disk_target.as_ref() == Some(&target);
                    return Err(ReplaceError::Validation(if is_new_disk {
                        format!(
                            "passphrase rejected by new disk '{}' ({})",
                            target.name(),
                            target.device()
                        )
                    } else {
                        format!(
                            "passphrase does not match existing pool member '{}'",
                            target.name()
                        )
                    }));
                }
                Err(CredentialVerifyError::Luks { source, .. }) => {
                    return Err(ReplaceError::Luks(source));
                }
            }
        }

        verify_replace_execute_live_pool_uuid(runner, fs, config.mount_point(), &pool, &new_uuid)?;
        // Pre-journal new-target identity gate (two-tier defense; see the
        // dispatcher doc). The arm-appropriate ExistingLuks identity check runs
        // here, BEFORE the inhibitor + journal, so a post-confirmation
        // disk-swap/backing-drift aborts on the reversible side instead of
        // stranding pending-op.json. The Step-1 post-journal re-probe stays as
        // the tight pre-open TOCTOU guard.
        verify_existing_luks_new_target_preflight(
            runner,
            &target_prep,
            &new_name,
            &new_mn,
            &new_by_id,
            &new_uuid,
            params.backing_path_resolver,
        )?;

        // Hold a logind sleep inhibitor for the rest of the replace operation --
        // covers Step 1 LUKS init, the long-running btrfs replace start, and
        // the post-replace soft balance for missing-path replaces. Suspending
        // mid-replace produces kernel-level topology corruption on every kernel
        // -- see issues #45 and #48 and the upstream warning at
        // reference/btrfs-progs/Documentation/btrfs-replace.rst:49-50.
        //
        // Acquired here, AFTER all interactive/reversible work (confirmation,
        // passphrase read+verify) and BEFORE
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

        // (Confirm/passphrase/inhibitor-window guard) Re-load pool.json and
        // re-derive target_membership here, mirroring RemovePlan::execute:
        // journaling/saving the plan-time snapshot would persist a stale
        // membership if pool.json was rewritten during the
        // confirmation/passphrase/inhibitor window. Reject if old drifted out;
        // derive_replace_target_membership's insert re-runs the four-axis
        // uniqueness invariant against the fresh read. Pinned by
        // replace_execute_rejects_when_pool_json_drifts_after_planning.
        let pre_membership = membership::load_membership(params.paths).map_err(|e| {
            ReplaceError::Validation(format!("failed to load pool membership: {e}"))
        })?;
        if pre_membership.by_uuid(&old_uuid).is_none() {
            return Err(absent_from_membership_error(old_name.as_str()));
        }
        let target_membership = derive_replace_target_membership(
            &pre_membership,
            &old_uuid,
            &new_uuid,
            &new_name,
            &new_by_id,
        )?;

        // Write journal before irreversible disk ops. pre_membership and
        // target_membership are derived above from a fresh pool.json read.
        let journal = journal::build_journal(
            pre_membership,
            target_membership.clone(),
            journal::OpKind::Replace {
                phase: journal::ReplacePhase::PoolMutation,
                old_uuid: old_uuid.clone(),
                old_name: old_name.clone(),
                new_uuid: new_uuid.clone(),
                new_name: new_name.clone(),
                new_target: new_target.clone(),
                source: journal_source.clone(),
                restore_raid1_after_commit,
            },
        );
        journal::write_journal_tracked(params.paths, &journal, journal_state)
            .map_err(|e| ReplaceError::Validation(e.to_string()))?;

        // Step 1: Init new disk (LUKS format/open) -- irreversible from here.
        match &target_prep {
            ReplaceTargetPrep::FreshLuks {
                extra_opts,
                enroll_key_file,
                ..
            } => {
                // Passphrase already verified above.
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Wait,
                        color_enabled,
                        &format!("disk {new_name}: formatting LUKS..."),
                    )
                );
                let label = luks_label_for(&new_name);
                luks_format(
                    runner,
                    new_by_id.as_str(),
                    &passphrase,
                    &new_uuid,
                    &label,
                    extra_opts,
                )?;
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Ok,
                        color_enabled,
                        &format!("disk {new_name}: LUKS formatted"),
                    )
                );

                if let Some(kf) = enroll_key_file {
                    eprint!(
                        "{}",
                        status_line(
                            StatusTag::Wait,
                            color_enabled,
                            &format!("disk {new_name}: enrolling keyfile in slot 1..."),
                        )
                    );
                    crate::luks::enroll_key_file(runner, new_by_id.as_str(), &passphrase, kf)?;
                    eprint!(
                        "{}",
                        status_line(
                            StatusTag::Ok,
                            color_enabled,
                            &format!("disk {new_name}: keyfile enrolled in slot 1"),
                        )
                    );
                }

                let backup_path = backup_luks_header_post_mutation(
                    runner,
                    new_by_id.as_str(),
                    &new_mn,
                    params.paths,
                )?;
                eprintln!("LUKS header backed up: {backup_path}");

                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Wait,
                        color_enabled,
                        &format!("disk {new_name}: unlocking..."),
                    )
                );
                // FreshLuks paths do NOT need the by-id-form re-probe at the
                // open boundary: the structured `uuid` field on
                // `CmdRequest::CryptsetupLuksFormat` above writes the
                // journaled UUID into the disk's header before the open,
                // and any swap-in-place reformat is caught by FreshLuks
                // adoption gates at finish-time and recovery replay.
                ensure_luks_open(
                    runner,
                    &new_name,
                    &new_by_id,
                    params.backing_path_resolver,
                    &passphrase,
                )?;
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Ok,
                        color_enabled,
                        &format!("disk {new_name}: unlocked"),
                    )
                );
            }
            ReplaceTargetPrep::ExistingLuks {
                mapper_open,
                enroll_key_file,
                ..
            } => {
                // Enroll the keyfile + back up the header BEFORE unlock so
                // the slot 1 mutation is captured in the post-mutation
                // backup. Mirrors the FreshLuks ordering (luksFormat ->
                // addKey -> headerBackup -> open) the dry-run snapshot
                // pins. cryptsetup luksAddKey works whether the mapper is
                // currently open or closed -- it operates on the
                // underlying block device, not the dm slot.
                if let Some(kf) = enroll_key_file {
                    eprint!(
                        "{}",
                        status_line(
                            StatusTag::Wait,
                            color_enabled,
                            &format!("disk {new_name}: enrolling keyfile in slot 1..."),
                        )
                    );
                    crate::luks::enroll_key_file(runner, new_by_id.as_str(), &passphrase, kf)?;
                    eprint!(
                        "{}",
                        status_line(
                            StatusTag::Ok,
                            color_enabled,
                            &format!("disk {new_name}: keyfile enrolled in slot 1"),
                        )
                    );
                    let backup_path = backup_luks_header_post_mutation(
                        runner,
                        new_by_id.as_str(),
                        &new_mn,
                        params.paths,
                    )?;
                    eprintln!("LUKS header backed up: {backup_path}");
                }

                if !*mapper_open {
                    eprint!(
                        "{}",
                        status_line(
                            StatusTag::Wait,
                            color_enabled,
                            &format!("disk {new_name}: unlocking..."),
                        )
                    );
                    // Open-boundary defense-in-depth: re-probe the LUKS UUID
                    // at the by-id form right before `ensure_luks_open` so a
                    // disk-swap between planning and execution cannot route
                    // pool data into a foreign LUKS volume. Fresh-LUKS new
                    // targets skip this gate (the `cryptsetup luksFormat`
                    // step writes the journaled UUID directly).
                    probe_existing_luks_new_target_uuid(runner, &new_by_id, &new_uuid)?;
                    ensure_luks_open(
                        runner,
                        &new_name,
                        &new_by_id,
                        params.backing_path_resolver,
                        &passphrase,
                    )?;
                    eprint!(
                        "{}",
                        status_line(
                            StatusTag::Ok,
                            color_enabled,
                            &format!("disk {new_name}: unlocked"),
                        )
                    );
                } else {
                    // Open-boundary defense-in-depth for the already-open
                    // path: re-classify ownership right before
                    // `pool_replace_device` so a close+reopen between
                    // planning and execution cannot route pool data into a
                    // foreign disk. The classifier checks both backing path
                    // and UUID, which catches cloned LUKS headers.
                    verify_existing_luks_open_mapper_target(
                        runner,
                        &new_name,
                        &new_mn,
                        &new_by_id,
                        &new_uuid,
                        params.backing_path_resolver,
                    )?;
                }
            }
        }

        // Step 2+: Execute replacement -- both paths use btrfs replace start.
        // Kickoff wording differs (replace-in-place vs rebuild-missing), but the
        // underlying `btrfs replace start` + resize sequence is identical. Bind
        // devid here so the shared spine below runs once.
        let devid = match &replace_source {
            ReplaceSource::Live { devid, .. } => {
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Wait,
                        color_enabled,
                        &format!("pool: replacing devid {devid} with {new_mn}..."),
                    )
                );
                *devid
            }
            ReplaceSource::Missing { devid } => {
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Wait,
                        color_enabled,
                        &format!("pool: rebuilding missing devid {devid} onto {new_mn}..."),
                    )
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
        eprint!(
            "{}",
            status_line(StatusTag::Ok, color_enabled, "pool: replace complete")
        );

        // Membership committed by btrfs replace. Enrich with kernel-assigned
        // devid + observed luks_uuid from a fresh probe, best-effort: if the
        // probe itself fails, warn and persist the target membership
        // unenriched. The new member's added_at fallback is still stamped
        // below. The journal still covers maintenance, so recovery can replay
        // it if we crash before clear_journal. Pinned by
        // cmd_replace_warns_when_post_mount_probe_errors.
        let mut target_membership = target_membership;
        match probe_pool(runner, fs, config.mount_point()) {
            Ok(pool_after) => {
                membership::enrich_from_pool_state(&mut target_membership, &pool_after);
            }
            Err(e) => crate::status_tag::emit_status(&format!(
                "Warning: failed to probe pool for metadata refresh: {e}\n"
            )),
        }
        if let Some(new_member) = target_membership.by_uuid_mut(&new_uuid)
            && new_member.added_at.is_none()
        {
            new_member.added_at = Some(crate::util::now_iso());
        }
        membership::save_membership(&target_membership, params.paths).map_err(|e| {
            ReplaceError::Validation(format!("failed to persist pool membership: {e}"))
        })?;
        let journal = journal::rewrite_journal(
            params.paths,
            &journal,
            journal::OpKind::Replace {
                phase: journal::ReplacePhase::PostReplaceMaintenance,
                old_uuid: old_uuid.clone(),
                old_name: old_name.clone(),
                new_uuid: new_uuid.clone(),
                new_name: new_name.clone(),
                new_target,
                source: journal_source,
                restore_raid1_after_commit,
            },
            Some(target_membership.clone()),
        )
        .map_err(|e| ReplaceError::Validation(e.to_string()))?;

        // Live-only: best-effort close of old mapper. Runs BEFORE the resize
        // so a resize failure does not `?` out and strand the old dm slot
        // bound to the backing disk until `braid lock` or reboot. Missing has
        // no old mapper to close.
        //
        // Defense-in-depth double-drift probe: before `CryptsetupClose`,
        // probe the live LUKS UUID at the observed mapper and require it
        // to equal the journaled `old_uuid`. On mismatch or unverifiable
        // state, demote the close to a logged-warning skip; inactive is
        // caller-classified below because it is surprising during live
        // execution but normal during recovery replay.
        if let journal::OpKind::Replace {
            source:
                journal::ReplaceJournalSource::Live {
                    old_mapper: mapper, ..
                },
            old_uuid: journaled_old_uuid,
            ..
        } = &journal.op
        {
            match probe_observed_mapper_uuid(runner, mapper, journaled_old_uuid) {
                MapperOwnership::Owned => {
                    if close_mapper_best_effort(
                        runner,
                        params.sleeper,
                        mapper,
                        &old_name,
                        CloseContext::Normal,
                        color_enabled,
                    ) {
                        eprintln!(
                            "Old device closed. If repurposing the physical disk, wipe it separately."
                        );
                    }
                }
                MapperOwnership::Inactive => {
                    warn_close_skipped_inactive(mapper, journaled_old_uuid);
                }
                MapperOwnership::Unverified => {}
            }
        }

        pool_resize_device(runner, devid, config.mount_point())?;

        // Restore RAID1 redundancy for missing-path replacements that clear the last missing device
        if restore_raid1_after_commit {
            crate::pool::maybe_restore_raid1(
                runner,
                fs,
                config.mount_point(),
                pool.missing_count,
                params.progress,
            )
            .map_err(ReplaceError::Pool)?;
        }

        // Maintenance complete -- safe to clear the journal.
        journal::clear_journal(params.paths)
            .map_err(|e| ReplaceError::Validation(e.to_string()))?;

        eprintln!("Done. Replaced {} with {}.", old_name, new_name);
        Ok(())
    }
}

/// Execute-time live-pool UUID gate for `replace`'s pre-journal seam.
/// This catches confirmation/passphrase-window races where the planned
/// replacement UUID enters the mounted pool after planning but before the
/// irreversible replace journal is written.
fn verify_replace_execute_live_pool_uuid<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    planned_pool: &PoolState,
    new_uuid: &LuksUuid,
) -> Result<(), ReplaceError> {
    let fresh_pool = probe_pool(runner, fs, mount_point)?;
    if !fresh_pool.mounted {
        return Err(ReplaceError::Validation(format!(
            "pool unmounted between planning and execution -- aborting before journal write. Re-mount {mount_point} and re-run `braid replace`."
        )));
    }

    if fresh_pool.fsid != planned_pool.fsid {
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
        return Err(ReplaceError::Validation(format!(
            "pool fsid changed between planning and execution (was {planned}, now {fresh}) -- aborting before journal write. The pool you planned against is no longer the same filesystem."
        )));
    }

    if fresh_pool.device_by_uuid(new_uuid).is_some() {
        return Err(ReplaceError::DuplicateUuid {
            uuid: new_uuid.clone(),
            scope: DuplicateUuidScope::LivePool,
        });
    }

    Ok(())
}

/// Open-boundary re-probe for `ReplaceJournalMode::ExistingLuks` new
/// targets. The planning-time `cryptsetup luksUUID` probe identifies the
/// physical disk that should be opened; between planning and execution
/// the operator could swap the disk at `new_target.by_id` (USB shuffle,
/// hot-plug into the wrong slot). Re-probing the by-id form right before
/// `ensure_luks_open` rejects the swap so pool data cannot be routed
/// into a foreign LUKS volume by the subsequent `btrfs replace start`.
/// Mismatch and probe failure both abort with
/// `ReplaceError::NewTargetUuidMismatchAtOpen`; the wording mirrors the
/// `finish_uncommitted_replace_recovery` recovery arm so
/// operator remediation reads identically across planning, execution,
/// and recovery boundaries.
fn probe_existing_luks_new_target_uuid<R: CommandRunner>(
    runner: &R,
    new_by_id: &ByIdPath,
    expected: &LuksUuid,
) -> Result<(), ReplaceError> {
    let probe = runner
        .run(&CmdRequest::CryptsetupLuksUuid {
            device: new_by_id.as_str().to_owned(),
        })
        .map_err(|e| ReplaceError::NewTargetUuidMismatchAtOpen {
            by_id: new_by_id.clone(),
            expected: expected.clone(),
            observed: format!("probe failed: {e}"),
        })?;
    match parse_cryptsetup_luks_uuid(&probe) {
        Ok(parsed) if parsed.uuid == *expected => Ok(()),
        Ok(parsed) => Err(ReplaceError::NewTargetUuidMismatchAtOpen {
            by_id: new_by_id.clone(),
            expected: expected.clone(),
            observed: parsed.uuid.as_str().to_owned(),
        }),
        Err(e) => Err(ReplaceError::NewTargetUuidMismatchAtOpen {
            by_id: new_by_id.clone(),
            expected: expected.clone(),
            observed: format!("probe parse failed: {e}"),
        }),
    }
}

/// Re-classify an already-open ExistingLuks replace target at execute time.
/// This verifies the configured by-id still backs the mapper and still has
/// the planned UUID; live-pool duplicate UUIDs are handled by the separate
/// pre-journal `verify_replace_execute_live_pool_uuid` gate.
fn verify_existing_luks_open_mapper_target<R: CommandRunner>(
    runner: &R,
    new_name: &DiskName,
    new_mapper: &MapperName,
    new_by_id: &ByIdPath,
    new_uuid: &LuksUuid,
    backing_path_resolver: &dyn BackingPathResolver,
) -> Result<(), ReplaceError> {
    classify_mapper_ownership(
        runner,
        new_name,
        new_mapper,
        new_by_id,
        backing_path_resolver,
        || Ok(new_uuid.clone()),
    )
    .map(|_| ())
    .map_err(|e| match e {
        OwnershipError::MapperOwnership(failure) => match failure {
            MapperOwnershipFailure::BackingPathMismatch {
                expected_path,
                found_path,
                ..
            } => ReplaceError::NewTargetMapperBackingMismatch {
                by_id: new_by_id.clone(),
                mapper: new_mapper.clone(),
                expected_path,
                found_path,
            },
            MapperOwnershipFailure::BackingPathResolveError { by_id, source, .. } => {
                ReplaceError::NewTargetMapperBackingResolveError {
                    by_id: new_by_id.clone(),
                    resolved: by_id,
                    source_message: source.to_string(),
                }
            }
            MapperOwnershipFailure::Conflict { found, .. } => {
                ReplaceError::NewTargetUuidMismatchAtOpen {
                    by_id: new_by_id.clone(),
                    expected: new_uuid.clone(),
                    observed: found
                        .map(|u| u.as_str().to_owned())
                        .unwrap_or_else(|| "(no backing)".into()),
                }
            }
        },
        OwnershipError::Parse(e) => ReplaceError::Validation(e.to_string()),
        OwnershipError::Cmd(e) => ReplaceError::Validation(e.to_string()),
    })
}

/// Pre-journal new-target identity gate for `replace`. Hoists the
/// arm-appropriate ExistingLuks identity check above `journal::write_journal`
/// so an operator disk-swap/backing-drift in the post-confirmation window
/// aborts on the reversible side (principles.md "line of no return") instead
/// of stranding pending-op.json. FreshLuks has no pre-existing identity to
/// probe. The post-journal probe/verify in Step 1 remain as the tight
/// pre-open guard (two-tier).
fn verify_existing_luks_new_target_preflight<R: CommandRunner>(
    runner: &R,
    target_prep: &ReplaceTargetPrep,
    new_name: &DiskName,
    new_mapper: &MapperName,
    new_by_id: &ByIdPath,
    new_uuid: &LuksUuid,
    backing_path_resolver: &dyn BackingPathResolver,
) -> Result<(), ReplaceError> {
    match target_prep {
        ReplaceTargetPrep::ExistingLuks {
            mapper_open: false, ..
        } => probe_existing_luks_new_target_uuid(runner, new_by_id, new_uuid),
        ReplaceTargetPrep::ExistingLuks {
            mapper_open: true, ..
        } => verify_existing_luks_open_mapper_target(
            runner,
            new_name,
            new_mapper,
            new_by_id,
            new_uuid,
            backing_path_resolver,
        ),
        ReplaceTargetPrep::FreshLuks { .. } => Ok(()),
    }
}

/// True iff the stats row identified by `devid` has any non-zero error
/// counter. Pairing is by the btrfs-native devid row key, not by mapper
/// path -- the row's path string can differ from the expected mapper path
/// without changing which live btrfs device it describes.
fn source_has_io_errors(stats: &crate::parse::types::BtrfsDeviceStatsOutput, devid: Devid) -> bool {
    stats.devices.iter().any(|d| {
        d.devid == devid
            && (d.read_io_errs > 0
                || d.write_io_errs > 0
                || d.flush_io_errs > 0
                || d.corruption_errs > 0
                || d.generation_errs > 0)
    })
}

/// Shared source-health note body so dry-run stdout and real-run stderr render
/// the same warning through `PreviewNote::Warn`'s owned `[warn]` prefix.
fn format_source_io_error_warning(devid: Devid) -> String {
    format!(
        "source device (devid {devid}) has I/O errors. \
         btrfs replace will read from mirrors where possible, \
         but may fail if any data lacks a healthy mirror copy."
    )
}

/// Shared source-health probe failure body for the non-blocking diagnostic
/// path; replace must keep planning even when the stats probe is unavailable.
fn format_source_io_probe_failure(devid: Devid, err: &str) -> String {
    format!("could not probe source device (devid {devid}) for I/O errors: {err}")
}

fn emit_replace_notes_to_stderr(notes: &[PreviewNote]) {
    let rendered =
        render_replace_notes_for_stderr(notes, crate::status_tag::color_enabled_for_stderr());
    emit_replace_stderr(&rendered);
}

fn render_replace_notes_for_stderr(notes: &[PreviewNote], color_enabled: bool) -> String {
    preview::render_notes_for_stderr_with(notes, PerDiskStyle::Bracketed, color_enabled)
}

fn emit_replace_stderr(rendered: &str) {
    #[cfg(test)]
    if replace_stderr_capture::write(rendered) {
        return;
    }
    eprint!("{rendered}");
}

#[cfg(test)]
mod replace_stderr_capture {
    use std::cell::RefCell;

    thread_local! {
        static CAPTURED_STDERR: RefCell<Option<String>> = const { RefCell::new(None) };
    }

    pub(super) fn capture<F, T>(f: F) -> (T, String)
    where
        F: FnOnce() -> T,
    {
        CAPTURED_STDERR.with(|slot| {
            let mut slot = slot.borrow_mut();
            assert!(slot.is_none(), "nested replace stderr capture");
            *slot = Some(String::new());
        });

        let result = f();
        let stderr = CAPTURED_STDERR.with(|slot| {
            slot.borrow_mut()
                .take()
                .expect("replace stderr capture must be active")
        });
        (result, stderr)
    }

    pub(super) fn write(text: &str) -> bool {
        CAPTURED_STDERR.with(|slot| {
            let mut slot = slot.borrow_mut();
            match slot.as_mut() {
                Some(stderr) => {
                    stderr.push_str(text);
                    true
                }
                None => false,
            }
        })
    }
}

/// Plan a `braid replace` run after dispatch has already checked for a
/// pending operation and loaded config under the pool lock. Owns `--new` spec
/// parsing, `--old == --new` guard, keyfile path validation, `probe_pool` +
/// mounted validation, mutation/UPS preflight, replace-source resolution,
/// membership load + `build_replacement_membership`, new-disk probe, and step
/// compilation. On success, accumulated notes move into `plan.notes`; on
/// post-preflight failure, accumulated notes stay on `PlanFailure::notes` so
/// `cmd_replace` can render them before returning the error.
///
/// Does not read or verify the passphrase or acquire the sleep
/// inhibitor -- those happen inside `ReplacePlan::execute` so
/// `--dry-run` keeps short-circuiting before them.
pub fn plan_replace<R, F, D>(
    runner: &R,
    fs: &F,
    dev_info: &D,
    params: &ReplaceParams<'_>,
) -> Result<ReplacePlan, PlanFailure<ReplaceError>>
where
    R: CommandRunner + Sync,
    F: Filesystem + ?Sized,
    D: BtrfsDevInfo + ?Sized,
{
    // Notes accumulator. Pre-preflight exits have no notes; later exits
    // preserve preflight diagnostics on `PlanFailure::notes`.
    let mut notes: Vec<PreviewNote> = Vec::new();

    let config = params.config;

    // Validate `--luks-format-arg` at the CLI boundary BEFORE any
    // probing, journal write, or `cryptsetup luksFormat`. A managed
    // token (`--uuid`/`--label`) surfaces as
    // `ReplaceError::ManagedFormatFlag`; mirrors `add.rs` so the CLI
    // matches one error type at the boundary.
    let luks_format_extra_opts = match LuksFormatExtraOpts::parse(params.luks_format_extra_opts) {
        Ok(o) => o,
        Err(e) => return Err(PlanFailure::empty(ReplaceError::ManagedFormatFlag(e))),
    };

    // Parse new_name as name=by_id spec
    let (new_name_parsed, new_by_id) = match membership::parse_disk_spec(params.new_name) {
        Ok(v) => v,
        Err(e) => return Err(PlanFailure::empty(ReplaceError::Validation(e.to_string()))),
    };
    let new_name_str = new_name_parsed.as_str();

    // --old == --new: reject before any pool or disk probes.
    if params.old_name == new_name_str {
        return Err(PlanFailure::empty(ReplaceError::Validation(
            "--old and --new must be different disks".into(),
        )));
    }

    if let Some(kf) = params.enroll_key_file
        && let Err(e) = crate::enroll_key_file::validate_key_file_path(kf, false)
    {
        return Err(PlanFailure::empty(ReplaceError::Validation(e.to_string())));
    }

    let pool = match probe_pool(runner, fs, config.mount_point()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return Err(PlanFailure::empty(ReplaceError::Validation(
                "pool is not mounted. Cannot replace.".into(),
            )));
        }
        Err(e) => return Err(PlanFailure::empty(ReplaceError::Probe(e))),
    };

    if !pool.mounted {
        return Err(PlanFailure::empty(ReplaceError::Validation(
            "pool is not mounted. Cannot replace.".into(),
        )));
    }

    // Preflight
    let fsid = pool.fsid.as_ref().expect("mounted pool must have FSID");
    match preflight::require_mutation_preflight(fs, fsid, config.mount_point()) {
        Ok(preflight_notes) => notes.extend(preflight_notes),
        Err(msg) => return Err(PlanFailure::empty(ReplaceError::Validation(msg))),
    }
    if let Err(msg) = preflight::check_ups_not_on_battery(
        runner,
        config.ups().map(|u| u.name.as_str()),
        "replace",
    ) {
        return Err(PlanFailure::with_notes(
            notes,
            ReplaceError::Validation(msg),
        ));
    }

    // Load membership BEFORE replace-source resolution so we can resolve
    // `--old <name>` to a `(uuid, member)` pair and route every downstream
    // identity decision through `old_uuid`.
    let pre_membership = match membership::load_membership(params.paths) {
        Ok(m) => m,
        Err(e) => {
            return Err(PlanFailure::with_notes(
                notes,
                ReplaceError::Validation(format!("failed to load pool membership: {e}")),
            ));
        }
    };

    // Resolve `--old <name>` to its UUID-keyed member. The name typed
    // by the operator is presentation; identity decisions read the
    // resolved UUID from here on. Mirrors `remove.rs`'s
    // `resolve_target_in_membership`.
    let old_name_parsed = match DiskName::parse(params.old_name) {
        Ok(n) => n,
        Err(e) => {
            return Err(PlanFailure::with_notes(
                notes,
                ReplaceError::Validation(format!(
                    "'{}' is not a valid disk name: {e}",
                    params.old_name
                )),
            ));
        }
    };
    let (old_uuid, old_member) = match pre_membership.by_name(&old_name_parsed) {
        Some((u, m)) => (u.clone(), m.clone()),
        None => {
            return Err(PlanFailure::with_notes(
                notes,
                ReplaceError::OldMemberNotFound {
                    name: params.old_name.to_owned(),
                },
            ));
        }
    };

    // Resolve replace source via UUID (live) or persisted devid + btrfs
    // missing_devids cross-check (missing). Pattern 4: live find is by
    // `PoolDevice.luks_uuid == old_uuid`; the observed `mapper` is cloned
    // from the matched device, not reconstructed from the resolved name.
    let replace_source = match resolve_replace_source(
        &old_name_parsed,
        &old_uuid,
        &old_member,
        params.missing_id,
        &pool,
    ) {
        Ok(v) => v,
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, e));
        }
    };

    // Source-read-health note. Pushed as a plan note so it renders in
    // --dry-run stdout and the pre-confirmation prelude, before journal
    // write and luksFormat. Live-only: a Missing source has no live device
    // to stat.
    if let ReplaceSource::Live { devid, .. } = &replace_source {
        let probe = runner
            .run(&CmdRequest::BtrfsDeviceStatsJson {
                mount_point: config.mount_point().clone(),
            })
            .map_err(|e| e.to_string())
            .and_then(|raw| parse_btrfs_device_stats(&raw).map_err(|e| e.to_string()));
        match probe {
            Ok(stats) if source_has_io_errors(&stats, *devid) => {
                notes.push(PreviewNote::Warn(format_source_io_error_warning(*devid)));
            }
            Ok(_) => {}
            Err(e) => notes.push(PreviewNote::Warn(format_source_io_probe_failure(
                *devid, &e,
            ))),
        }
    }

    // Probe --new disk state
    let new_probed = match probe_config_disk(
        runner,
        fs,
        &new_name_parsed,
        &new_by_id,
        params.backing_path_resolver,
    ) {
        Ok(p) => p,
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, e.into()));
        }
    };
    let new_probed: PresentConfigDisk = match PresentConfigDisk::try_from(new_probed) {
        Ok(p) => p,
        Err(orig) => {
            return Err(PlanFailure::with_notes(
                notes,
                ReplaceError::Validation(format!(
                    "new disk '{}' ({}) is not present. Is it plugged in?",
                    orig.name, orig.by_id_path
                )),
            ));
        }
    };

    let source_probe = preflight::ReplaceSourceProbe {
        devid: match &replace_source {
            ReplaceSource::Live { devid, .. } | ReplaceSource::Missing { devid } => *devid,
        },
    };
    let target_probe = match &new_probed.state {
        PresentConfigDiskState::PresentLuks { .. } => preflight::ReplaceTargetProbe::PresentLuks {
            by_id: new_by_id.as_str(),
        },
        PresentConfigDiskState::PresentNotLuks => preflight::ReplaceTargetProbe::PresentNotLuks {
            by_id: new_by_id.as_str(),
        },
    };
    let mount = Path::new(config.mount_point().as_str());
    if let Err(msg) = preflight::check_replace_target_capacity(
        runner,
        dev_info,
        mount,
        source_probe,
        target_probe,
    ) {
        return Err(PlanFailure::with_notes(
            notes,
            ReplaceError::Validation(msg),
        ));
    }

    // Keyfile diagnostics are plan notes, not confirmation-only stderr.
    // This keeps dry-run stdout, real-run stderr, and preserved-error
    // stderr on the same PreviewNote contract used by `add`.
    if params.enroll_key_file.is_none() {
        let target_lacks_keyfile = match &new_probed.state {
            PresentConfigDiskState::PresentNotLuks => true,
            PresentConfigDiskState::PresentLuks { .. } => {
                match check_key_slot(runner, new_by_id.as_str(), LUKS_SLOT_KEYFILE) {
                    Ok(KeySlotState::Empty) => true,
                    Ok(KeySlotState::Occupied) => false,
                    Err(err) => {
                        notes.push(PreviewNote::Warn(format_target_keyfile_probe_failure(
                            &new_by_id, &err,
                        )));
                        false
                    }
                }
            }
        };

        if target_lacks_keyfile {
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

    // Resolve the keyfile decision for `--enroll DIR` against an
    // already-LUKS new disk via the shared per-disk classifier. This is
    // the silent-drop fix: `Some(kf) + PresentLuks` was the case the
    // old code dropped on the floor; routing it through
    // `plan_single_disk_enrollment` makes idempotent skip / refuse-on-
    // slot-conflict / will-enroll structurally explicit and shared
    // with `add` and `enroll`. Refusal happens before journal write.
    // For PresentNotLuks targets (fresh format) the user input flows
    // through directly -- no keyfile probe, no slot-1 check.
    let resolved_enroll_key_file: Option<KeyFilePath> =
        match (&new_probed.state, params.enroll_key_file) {
            (PresentConfigDiskState::PresentLuks { .. }, Some(kf)) => {
                match crate::enroll_key_file::plan_single_disk_enrollment(
                    runner,
                    &new_name_parsed,
                    &new_by_id,
                    kf,
                    crate::enroll_key_file::EnrollmentPlanMode::ExistingKeyfile,
                ) {
                    Ok(crate::enroll_key_file::DiskEnrollAction::AlreadyEnrolled { .. }) => None,
                    Ok(crate::enroll_key_file::DiskEnrollAction::NeedsEnroll { .. }) => {
                        Some(KeyFilePath::new(kf.to_path_buf()))
                    }
                    Err(e) => {
                        return Err(PlanFailure::with_notes(
                            notes,
                            ReplaceError::Validation(e.to_string()),
                        ));
                    }
                }
            }
            (_, kf) => kf.map(|p| KeyFilePath::new(p.to_path_buf())),
        };

    // Derive `new_uuid`:
    //   - FreshLuks: generate via `LuksUuid::new_v4()`. Recorded into
    //     the journal at op level BEFORE `cryptsetup luksFormat` runs,
    //     and into the structured `uuid` field on
    //     `CmdRequest::CryptsetupLuksFormat` so the kernel writes the
    //     journaled identity.
    //   - ExistingLuks: read from the planning-time probe via
    //     `probe_config_disk`. The open-boundary re-probe at execute
    //     time defends against operator disk swap between plan and
    //     execute.
    //   - Absent: rejected at the probe boundary above.
    let new_uuid = match &new_probed.state {
        PresentConfigDiskState::PresentNotLuks => LuksUuid::new_v4(),
        PresentConfigDiskState::PresentLuks { uuid, .. } => uuid.clone(),
    };

    // Pre-journal-write `new_uuid` uniqueness assert. Refused BEFORE
    // any journal write and BEFORE `CryptsetupLuksFormat`. Membership
    // check excludes `old_uuid` (which is being replaced); live-pool
    // check inspects the planning-time `pool.devices` UUID set.
    if let Err(e) = assert_new_uuid_unique(&new_uuid, &old_uuid, &pre_membership, &pool) {
        return Err(PlanFailure::with_notes(notes, e));
    }

    // Validate the four-axis membership invariant at plan time so a
    // colliding target is rejected during --dry-run and before the
    // confirmation prompt. The derived membership is discarded: the
    // authoritative derivation runs again in `ReplacePlan::execute` against a
    // fresh pool.json read, so the plan stores no membership snapshot.
    if let Err(e) = derive_replace_target_membership(
        &pre_membership,
        &old_uuid,
        &new_uuid,
        &new_name_parsed,
        &new_by_id,
    ) {
        return Err(PlanFailure::with_notes(notes, e.into()));
    }

    // Resolve existing-member credential-verify display names now, while the
    // plan-time `pre_membership` is in scope, so the join reuses the
    // already-loaded membership (no extra pool.json read) and only the
    // resolved strings -- not a membership snapshot -- ride on the plan.
    let member_verify_targets =
        build_member_verify_targets(&pre_membership, &pool, &replace_source, &old_uuid);

    let work_plan = build_replace_work_plan(ReplaceWorkPlanInput {
        config: config.clone(),
        old_uuid,
        old_name: old_member.name.clone(),
        new_uuid,
        new_name: new_name_parsed,
        new_by_id,
        new_probed,
        replace_source,
        pool,
        paths: params.paths,
        enroll_key_file: resolved_enroll_key_file,
        luks_format_extra_opts,
        member_verify_targets,
    });

    Ok(ReplacePlan { notes, work_plan })
}

/// Pre-journal-write refusal for a colliding `new_uuid`. Inspects
/// membership keys (excluding `old_uuid`) and the live `pool.devices`
/// UUID set; the first collision returns the structured
/// `ReplaceError::DuplicateUuid`. Membership scope wins ordering ties
/// because the membership-side collision is the more common operator
/// failure mode (a foreign disk attached and discovered between
/// command invocations). The mirror with `add` is intentionally
/// asymmetric: `add` splits the identity- and live-pool concerns across
/// `assert_target_uuid_unique` and per-caller live-pool guards (its
/// live-pool check is caller-dependent -- backing-aware classify for a
/// returned PresentLuks disk vs. a plain scan for a freshly-minted
/// FreshLuks UUID). `replace` keeps one uniform scan here because its new
/// disk is always distinct hardware -- there is no same-backing returned-disk
/// no-op to disambiguate -- so bundling both axes in one function is
/// right-sized for replace's semantics.
fn assert_new_uuid_unique(
    new_uuid: &LuksUuid,
    old_uuid: &LuksUuid,
    membership: &PoolMembership,
    pool: &PoolState,
) -> Result<(), ReplaceError> {
    if new_uuid != old_uuid && membership.by_uuid(new_uuid).is_some() {
        return Err(ReplaceError::DuplicateUuid {
            uuid: new_uuid.clone(),
            scope: DuplicateUuidScope::Membership,
        });
    }
    if pool.device_by_uuid(new_uuid).is_some() {
        return Err(ReplaceError::DuplicateUuid {
            uuid: new_uuid.clone(),
            scope: DuplicateUuidScope::LivePool,
        });
    }
    Ok(())
}

/// Derive post-replace membership: drop `old_uuid`, insert the new member,
/// running `PoolMembership::insert`'s four-axis uniqueness invariant. Shared by
/// `plan_replace` (early/dry-run rejection) and `ReplacePlan::execute`
/// (authoritative, against a fresh pool.json read) so both derive identically.
fn derive_replace_target_membership(
    pre_membership: &PoolMembership,
    old_uuid: &LuksUuid,
    new_uuid: &LuksUuid,
    new_name: &DiskName,
    new_by_id: &ByIdPath,
) -> Result<PoolMembership, membership::MembershipError> {
    let mut target = pre_membership.clone();
    target.remove_by_uuid(old_uuid);
    target.insert(
        new_uuid.clone(),
        membership::DiskMember {
            name: new_name.clone(),
            by_id: new_by_id.clone(),
            devid: None,
            added_at: None,
        },
    )?;
    Ok(target)
}

/// `replace`'s execute-time `by_uuid` drift error -- same operator wording as
/// remove's `absent_from_membership_error`, so the two commands reject an
/// absent member identically before journaling.
fn absent_from_membership_error(name: &str) -> ReplaceError {
    ReplaceError::Validation(format!(
        "'{name}' not found in pool.json membership -- \
         no disk entry has this name. Pool membership may need manual repair."
    ))
}

/// Plan-then-execute device replacement; dry-run renders the same typed plan
/// built from membership, mount/preflight, duplicate-UUID, and target probes.
/// Real execution verifies the passphrase and holds a sleep inhibitor.
pub fn cmd_replace<R, F, D>(
    runner: &R,
    fs: &F,
    dev_info: &D,
    params: &ReplaceParams<'_>,
) -> Result<(), ReplaceError>
where
    R: CommandRunner + Sync,
    F: Filesystem + ?Sized,
    D: BtrfsDevInfo + ?Sized,
{
    let plan = match plan_replace(runner, fs, dev_info, params) {
        Ok(p) => p,
        Err(PlanFailure { notes, error }) => {
            // Preserved-context failure: accumulated notes render to
            // stderr before the error via the SAME helper as the Ok
            // path (`ReplacePlan::execute`), so preflight diagnostics
            // surface identically across success, failure, and dry-run
            // stdout.
            emit_replace_notes_to_stderr(&notes);
            return Err(error);
        }
    };
    if params.dry_run {
        plan.preview().print_colored();
        return Ok(());
    }
    plan.execute(runner, fs, params)
}

#[derive(Debug, Clone)]
pub enum ReplaceSource {
    /// Old disk is alive in the pool -- replace via `btrfs replace start`.
    Live { mapper: MapperName, devid: Devid },
    /// Old disk is missing -- replace via `btrfs replace start` by devid.
    Missing { devid: Devid },
}

struct ReplaceWorkPlanInput<'a> {
    config: Config,
    old_uuid: LuksUuid,
    old_name: DiskName,
    new_uuid: LuksUuid,
    new_name: DiskName,
    new_by_id: ByIdPath,
    new_probed: PresentConfigDisk,
    replace_source: ReplaceSource,
    pool: PoolState,
    paths: &'a StatePaths,
    /// Resolved keyfile decision after `plan_single_disk_enrollment`
    /// for `PresentLuks` targets. `Some(kf)` means an enrollment will
    /// run. `None` covers the no-`--enroll` case AND the idempotent
    /// `AlreadyEnrolled` skip. For `PresentNotLuks` (fresh format),
    /// resolution is a no-op so this carries the raw user input.
    enroll_key_file: Option<KeyFilePath>,
    luks_format_extra_opts: LuksFormatExtraOpts,
    /// Existing-pool-member credential-verify targets, resolved by the caller
    /// at plan time so the join reuses the already-loaded `pre_membership`
    /// (no extra `pool.json` read) and the plan stores only display strings.
    member_verify_targets: Vec<CredentialVerifyTarget>,
}

/// Resolve the existing pool members whose passphrase must match before a
/// replace touches anything, with each member's display `name` UUID-joined
/// through membership (decision 024) so the credential-verify message survives
/// mapper drift. The anchor selection mirrors what `execute` consumes: a live
/// replace verifies every retained member, but when it would leave no other
/// member it falls back to the disk being replaced (the only passphrase we can
/// check); a missing-source replace verifies all live members. Identity is
/// carried by each target's `device`, never the cosmetic `name`.
fn build_member_verify_targets(
    pre_membership: &PoolMembership,
    pool: &PoolState,
    replace_source: &ReplaceSource,
    old_uuid: &LuksUuid,
) -> Vec<CredentialVerifyTarget> {
    let retained_members: Vec<_> = match replace_source {
        ReplaceSource::Live { .. } => pool
            .devices
            .iter()
            .filter(|device| device.luks_uuid != *old_uuid)
            .collect(),
        ReplaceSource::Missing { .. } => pool.devices.iter().collect(),
    };
    let anchor_members: Vec<_> =
        if matches!(replace_source, ReplaceSource::Live { .. }) && retained_members.is_empty() {
            pool.devices
                .iter()
                .filter(|device| device.luks_uuid == *old_uuid)
                .collect()
        } else {
            retained_members
        };
    anchor_members
        .into_iter()
        .map(|device| CredentialVerifyTarget::existing_pool_member(pre_membership, device))
        .collect()
}

fn build_replace_work_plan(input: ReplaceWorkPlanInput<'_>) -> ReplaceWorkPlan {
    let new_mapper = mapper_name(&input.new_name);
    let new_mapper_path = new_mapper.dev_path();
    let journal_target = build_replace_journal_target(
        &input.new_by_id,
        &input.new_probed,
        input.enroll_key_file.as_ref(),
        &input.luks_format_extra_opts,
    );
    let journal_source = build_replace_journal_source(&input.replace_source);
    let will_clear_last_missing = matches!(&input.replace_source, ReplaceSource::Missing { .. })
        && input.pool.missing_count == 1;
    // +1: the new device added by this replace fills the cleared missing slot.
    let remaining_present = input.pool.devices.len() + 1;
    let restore_raid1_after_commit =
        crate::pool::should_restore_raid1(will_clear_last_missing, remaining_present);
    let target_prep = match input.new_probed.state {
        PresentConfigDiskState::PresentNotLuks => ReplaceTargetPrep::FreshLuks {
            extra_opts: input.luks_format_extra_opts.clone(),
            enroll_key_file: input.enroll_key_file.clone(),
        },
        PresentConfigDiskState::PresentLuks { mapper_open, .. } => {
            ReplaceTargetPrep::ExistingLuks {
                mapper_open,
                enroll_key_file: input.enroll_key_file.clone(),
            }
        }
    };

    ReplaceWorkPlan {
        config: input.config,
        old_uuid: input.old_uuid,
        old_name: input.old_name,
        new_uuid: input.new_uuid,
        new_name: input.new_name,
        new_by_id: input.new_by_id,
        pool: input.pool,
        replace_source: input.replace_source,
        luks_headers_dir: input.paths.luks_headers_dir(),
        target_prep,
        journal_target,
        journal_source,
        restore_raid1_after_commit,
        new_mapper,
        new_mapper_path,
        member_verify_targets: input.member_verify_targets,
    }
}

fn build_replace_journal_source(source: &ReplaceSource) -> journal::ReplaceJournalSource {
    match source {
        ReplaceSource::Live { mapper, devid } => journal::ReplaceJournalSource::Live {
            old_devid: *devid,
            old_mapper: mapper.clone(),
        },
        ReplaceSource::Missing { devid } => {
            journal::ReplaceJournalSource::Missing { old_devid: *devid }
        }
    }
}

fn build_replace_journal_target(
    new_by_id: &ByIdPath,
    new_probed: &PresentConfigDisk,
    enroll_key_file: Option<&KeyFilePath>,
    luks_format_extra_opts: &LuksFormatExtraOpts,
) -> journal::ReplaceJournalTarget {
    let mode = match &new_probed.state {
        PresentConfigDiskState::PresentNotLuks => journal::ReplaceJournalMode::FreshLuks {
            extra_opts: luks_format_extra_opts.clone(),
            enroll_key_file: enroll_key_file.cloned(),
        },
        PresentConfigDiskState::PresentLuks { .. } => journal::ReplaceJournalMode::ExistingLuks {
            enroll_key_file: enroll_key_file.cloned(),
        },
    };
    journal::ReplaceJournalTarget {
        by_id: new_by_id.clone(),
        mode,
    }
}

/// Resolve the replace source from the resolved `(old_uuid, old_member)`
/// pair, the pool's live state, and the operator's optional
/// `--missing-id` override. Pattern 4 site:
///   - Live find predicate: `d.luks_uuid == old_uuid`. The observed
///     `mapper` is cloned from the matched `PoolDevice.mapper`, NOT
///     reconstructed via `mapper_name(&old_name)`. The cloned value
///     propagates into `ReplaceJournalSource::Live.old_mapper` and
///     downstream to the post-commit `close_mapper_best_effort` call so
///     mapper drift between plan and execute still targets the right
///     dm slot.
///   - Missing arm: cross-check `old_member.devid` against the resolved
///     btrfs missing devid; reject any disagreement and reject any
///     `--old` whose persisted member has no devid.
fn resolve_replace_source(
    old_name: &DiskName,
    old_uuid: &LuksUuid,
    old_member: &membership::DiskMember,
    missing_id: Option<Devid>,
    pool: &PoolState,
) -> Result<ReplaceSource, ReplaceError> {
    // Pattern 4: find by UUID, not by reconstructed mapper.
    if let Some(matched) = pool.device_by_uuid(old_uuid) {
        // Live old disk in pool.
        if missing_id.is_some() {
            return Err(ReplaceError::Validation(
                "--missing-id cannot be used when the old disk is still alive in the pool".into(),
            ));
        }
        if pool.missing_count > 0 {
            let repair_command = repair_hint::missing_replace_command(None);
            let status_hint = repair_hint::see_missing_names_in_status(pool.missing_count);
            return Err(ReplaceError::Validation(format!(
                "pool has {} missing device{}. \
                 Repair the missing device{} first with `{repair_command}`, \
                 then retry this live replace. {status_hint}",
                pool.missing_count,
                if pool.missing_count == 1 { "" } else { "s" },
                if pool.missing_count == 1 { "" } else { "s" },
            )));
        }
        // Observed mapper, NOT mapper_name(&old_name). Journaling the
        // reconstructed name would leak the post-commit close past
        // operator-applied mapper drift.
        return Ok(ReplaceSource::Live {
            mapper: matched.mapper.clone(),
            devid: matched.devid,
        });
    }

    // Old disk not in pool -- dead/missing path.
    // Cross-check persisted devid against the resolved btrfs missing devid.
    let persisted_devid = match old_member.devid {
        Some(d) => d,
        None => {
            return Err(ReplaceError::OldMemberMissingDevid {
                name: old_name.as_str().to_owned(),
            });
        }
    };

    let null_underlying_refusal = |devid: Devid| {
        ReplaceError::Validation(repair_hint::hot_unplug_not_yet_missing(
            devid,
            "braid replace",
        ))
    };

    let resolved = if let Some(supplied) = missing_id {
        // --missing-id supplied: must equal persisted devid AND appear
        // in the btrfs missing set.
        if supplied != persisted_devid {
            return Err(ReplaceError::OldDevidMismatch {
                old_name: old_name.as_str().to_owned(),
                pool_devid: persisted_devid,
                supplied_devid: supplied,
            });
        }
        if pool.devices.iter().any(|d| d.devid == supplied) {
            return Err(ReplaceError::Validation(format!(
                "devid {supplied} is a live device, not a missing one."
            )));
        }
        if !pool.missing_devids.contains(&supplied) {
            if pool.null_underlying.iter().any(|d| d.devid == supplied) {
                return Err(null_underlying_refusal(supplied));
            }
            return Err(ReplaceError::Validation(format!(
                "devid {supplied} is not a missing device in this pool. \
                 {}",
                repair_hint::see_devids_in_status()
            )));
        }
        supplied
    } else {
        // Auto-resolve: the persisted devid must be missing in btrfs.
        if !pool.missing_devids.contains(&persisted_devid) {
            if pool
                .null_underlying
                .iter()
                .any(|d| d.devid == persisted_devid)
            {
                return Err(null_underlying_refusal(persisted_devid));
            }
            if pool.missing_devids.is_empty() {
                return Err(ReplaceError::Validation(format!(
                    "disk '{}' not found in pool and no missing devices detected.",
                    old_name
                )));
            }
            return Err(ReplaceError::Validation(format!(
                "disk '{}' records devid {} in pool.json, but btrfs reports it is not missing. \
                 Pool membership may be out of date; run `braid status` to inspect.",
                old_name, persisted_devid
            )));
        }
        // Sanity: if multiple devids are missing, the operator must
        // supply `--missing-id` to disambiguate UNLESS the persisted
        // devid pinpoints exactly one. We confirmed
        // `missing_devids.contains(&persisted_devid)`, so the persisted
        // value already picks the right one.
        persisted_devid
    };

    Ok(ReplaceSource::Missing { devid: resolved })
}

#[cfg(test)]
struct ReplaceWorkPlanTestInput<'a> {
    new_name: &'a str,
    new_by_id: &'a ByIdPath,
    new_probed: &'a PresentConfigDisk,
    replace_source: &'a ReplaceSource,
    mount_point: &'a MountPoint,
    will_clear_last_missing: bool,
    total_devices: u64,
    paths: &'a StatePaths,
    enroll_key_file: Option<&'a Path>,
    luks_format_extra_opts: &'a [String],
}

#[cfg(test)]
fn replace_work_plan_for_test(input: &ReplaceWorkPlanTestInput<'_>) -> ReplaceWorkPlan {
    let config = Config::new(input.mount_point.clone()).expect("valid test mount point");
    let old_uuid = LuksUuid::parse("99999999-9999-9999-9999-999999999999").unwrap();
    let pool = replace_work_plan_test_pool(
        input.replace_source,
        &old_uuid,
        input.will_clear_last_missing,
        input.total_devices,
    );
    // Synthesize an op-level identity for the test plan. The synthesized
    // `new_uuid` lines up with what the planner would have produced from
    // the probed `PresentConfigDiskState`: FreshLuks gets a fresh v4,
    // ExistingLuks reuses the probed value.
    let new_uuid = match &input.new_probed.state {
        PresentConfigDiskState::PresentNotLuks => LuksUuid::new_v4(),
        PresentConfigDiskState::PresentLuks { uuid, .. } => uuid.clone(),
    };
    let old_name = DiskName::parse("disk2").expect("valid disk name");
    let new_name = DiskName::parse(input.new_name).expect("valid new disk name in test");
    let extra_opts = LuksFormatExtraOpts::parse(input.luks_format_extra_opts)
        .expect("test extras must be valid (no managed flags)");
    // Same selection `execute` consumes; an empty membership is fine here
    // because render tests do not inspect the cosmetic `name`, and the
    // `device` set (which drives verification) is membership-independent.
    let member_verify_targets = build_member_verify_targets(
        &PoolMembership::empty(),
        &pool,
        input.replace_source,
        &old_uuid,
    );
    build_replace_work_plan(ReplaceWorkPlanInput {
        config,
        old_uuid,
        old_name,
        new_uuid,
        new_name,
        new_by_id: (*input.new_by_id).clone(),
        new_probed: input.new_probed.clone(),
        replace_source: input.replace_source.clone(),
        pool,
        paths: input.paths,
        enroll_key_file: input
            .enroll_key_file
            .map(|p| KeyFilePath::new(p.to_path_buf())),
        luks_format_extra_opts: extra_opts,
        member_verify_targets,
    })
}

#[cfg(test)]
fn replace_work_plan_test_pool(
    replace_source: &ReplaceSource,
    old_uuid: &LuksUuid,
    will_clear_last_missing: bool,
    total_devices: u64,
) -> PoolState {
    let missing_count = match replace_source {
        ReplaceSource::Live { .. } => 0,
        ReplaceSource::Missing { .. } if will_clear_last_missing => 1,
        ReplaceSource::Missing { .. } => 2,
    };
    let live_device_count = total_devices.saturating_sub(missing_count) as usize;
    let mut devices = Vec::new();

    if let ReplaceSource::Live { mapper, devid } = replace_source
        && live_device_count > 0
    {
        devices.push(PoolDevice {
            mapper: mapper.clone(),
            luks_uuid: old_uuid.clone(),
            devid: *devid,
            underlying: format!("/dev/test-{devid}"),
        });
    }

    let mut next_devid = 100;
    while devices.len() < live_device_count {
        let test_name = DiskName::parse(&format!("test{}", devices.len() + 1))
            .expect("valid synthetic disk name");
        devices.push(PoolDevice {
            mapper: mapper_name(&test_name),
            luks_uuid: synth_test_uuid(next_devid),
            devid: Devid::new(next_devid),
            underlying: format!("/dev/test-{next_devid}"),
        });
        next_devid += 1;
    }

    PoolState {
        mounted: true,
        devices,
        missing_count,
        total_devices,
        fsid: None,
        missing_devids: Vec::new(),
        null_underlying: Vec::new(),
    }
}

/// Build a deterministic canonical `LuksUuid` keyed on a small integer
/// seed (typically `devid`). Used inside `replace_work_plan_test_pool`
/// so synthetic `PoolDevice` rows carry well-formed UUIDs that pass
/// `LuksUuid::parse`.
#[cfg(test)]
fn synth_test_uuid(seed: u64) -> LuksUuid {
    LuksUuid::parse(&format!("00000000-0000-0000-0000-{seed:012x}"))
        .expect("seed produces a canonical UUID")
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btrfs_ioctl::tests_support::PanicBtrfsDevInfo;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::state_paths::StatePaths;
    use crate::test_fixtures::{
        MockBackingPathResolver, assert_exact_lines_in_order, assert_lines_in_order, line_index,
    };

    fn test_paths() -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        (tmp, paths)
    }

    fn mp() -> MountPoint {
        MountPoint::new("/mnt/storage".into())
    }

    fn test_config() -> Config {
        Config::new(mp()).unwrap()
    }

    struct PanicRunner;

    impl CommandRunner for PanicRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, crate::cmd::CmdError> {
            panic!("planner-boundary test: runner must not be invoked; got: {request:?}");
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, crate::cmd::CmdError> {
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

    /*
     * Intent: source_has_io_errors pairs a stats row to the live source
     * by devid, even when the row's path differs from the canonical
     * /dev/mapper/braid-<name>. With non-zero counters on the matched
     * row, returns true; with zero counters or no devid match, false.
     *
     * Why it exists: the live-replace warning previously matched on
     * `target.as_path() == /dev/mapper/<mapper>`, which silently dropped
     * the warning whenever btrfs reported the row by an alternate path
     * (e.g. /dev/dm-N). Pairing by devid removes that path-match blind spot.
     * This test pins the new behavior so a future revert to path
     * matching cannot land silently.
     *
     * Scenario: stats row for devid 1 carries path "/dev/dm-1" (not
     * "/dev/mapper/disk1") and read_io_errs = 5; replace must still
     * recognize the I/O errors on the live source.
     */
    #[test]
    fn live_replace_detects_io_errors_when_stats_path_differs() {
        use crate::cmd::RawCommandOutput;
        let stats_raw = RawCommandOutput {
            cmd: "btrfs device stats".into(),
            stdout: r#"{"device-stats": [
                {"device": "/dev/dm-1", "devid": 1, "write_io_errs": 0, "read_io_errs": 5, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
                {"device": "/dev/dm-2", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
            ]}"#
            .into(),
            stderr: String::new(),
            exit_status: 0,
        };
        let stats = parse_btrfs_device_stats(&stats_raw).expect("parses");

        assert!(
            source_has_io_errors(&stats, Devid::new(1)),
            "devid 1 has read_io_errs=5; mismatched path must not hide it"
        );
        assert!(
            !source_has_io_errors(&stats, Devid::new(2)),
            "devid 2 has zero counters; must not report errors"
        );
        assert!(
            !source_has_io_errors(&stats, Devid::new(99)),
            "no row for devid 99; must not report errors"
        );
    }

    /* Intent: replace's stderr-note wrapper colors only bracketed
     * warning tags.
     * Why it exists: replace routes notes through a capture-aware
     * wrapper, so it needs direct coverage in addition to Preview's
     * shared renderer tests.
     * Scenario: an Info note followed by a Warn note is rendered in
     * plain and colored modes.
     */
    #[test]
    fn render_replace_notes_for_stderr_colors_only_warn_tag() {
        let notes = vec![
            PreviewNote::Info("waiting for in-flight device add".into()),
            PreviewNote::Warn("pool is mounted read-only".into()),
        ];

        let plain = render_replace_notes_for_stderr(&notes, false);
        assert_eq!(
            plain,
            "waiting for in-flight device add\n[warn] pool is mounted read-only\n"
        );

        let colored = render_replace_notes_for_stderr(&notes, true);
        assert_eq!(
            colored,
            "waiting for in-flight device add\n\x1b[33m[warn]\x1b[0m pool is mounted read-only\n"
        );
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
                source: &ReplaceSource::Missing {
                    devid: Devid::new(2),
                },
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
                source: &ReplaceSource::Missing {
                    devid: Devid::new(2),
                },
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
            missing_devids: vec![],
            total_devices: 2,
            fsid: None,
            null_underlying: vec![],
        }
    }

    fn null_underlying_device(devid: Devid) -> NullUnderlyingDevice {
        NullUnderlyingDevice {
            mapper: mapper_name(&disk_name(&format!("disk{}", devid.get()))),
            devid,
        }
    }

    /// Build a `DiskName` from a literal, panicking on invalid input.
    /// Test-only helper to keep call sites short.
    fn disk_name(s: &str) -> DiskName {
        DiskName::parse(s).expect("valid disk name in fixture")
    }

    /// Build the disk2 `(uuid, member)` pair used by the live-resolution
    /// tests against `two_device_pool`. UUID matches the synthetic
    /// pool entry so Pattern 4's UUID-keyed find succeeds.
    fn disk2_member_for_two_device_pool() -> (LuksUuid, membership::DiskMember) {
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let member = membership::DiskMember {
            name: disk_name("disk2"),
            by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
            devid: Some(Devid::new(2)),
            added_at: None,
        };
        (uuid, member)
    }

    #[test]
    // Intent: live old disk in healthy pool resolves to ReplaceSource::Live.
    // Why: core behavior -- replace must accept live disks when pool has no missing.
    // Scenario: operator swaps a slow-but-alive drive for a faster one.
    fn live_old_resolution_succeeds_no_missing() {
        let pool = two_device_pool();
        let (uuid, member) = disk2_member_for_two_device_pool();
        let result = resolve_replace_source(&disk_name("disk2"), &uuid, &member, None, &pool);
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
        let (uuid, member) = disk2_member_for_two_device_pool();
        let err = resolve_replace_source(
            &disk_name("disk2"),
            &uuid,
            &member,
            Some(Devid::new(99)),
            &pool,
        )
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
        let (uuid, member) = disk2_member_for_two_device_pool();
        let err =
            resolve_replace_source(&disk_name("disk2"), &uuid, &member, None, &pool).unwrap_err();
        assert!(
            err.to_string().contains("missing device"),
            "unexpected error: {err}"
        );
        assert!(
            err.to_string().contains(
                "braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>"
            ),
            "should suggest the shared replace repair command: {err}"
        );
        assert!(
            !err.to_string().contains("replace --missing-id"),
            "should not suggest replace --missing-id: {err}"
        );
        assert!(
            !err.to_string().contains("remove-missing"),
            "should not suggest remove-missing: {err}"
        );
    }

    // Pool.json plumbing tests for the previous `build_replacement_membership`
    // helper are now covered by the inline membership construction in
    // `plan_replace`. Coverage:
    //   - by-id rename conflict: blocked by `PoolMembership::insert` axis 3
    //     (by-id collision). The `assert_target_uuid_unique` invariants in
    //     add.rs + `PoolMembership::insert` four-axis check exercise the
    //     same surface; replace inherits via the shared insert call.
    //   - absent old-name: covered by `OldMemberNotFound` (see
    //     `cmd_replace_missing_path_rejects_old_name_absent_from_membership`).
    //   - missing-path devid mismatch: covered by `OldDevidMismatch`
    //     emitted from `resolve_replace_source` (the missing-path
    //     decoy-regression test exercises the persisted-devid cross-check
    //     at the same boundary).
    //   - happy paths: exercised end-to-end by the live and missing-path
    //     execute tests, which now also assert membership transitions.

    #[test]
    // Intent: dry-run for live path shows btrfs replace and resize steps.
    // Why: operator should see what the live replace will do before committing.
    // Scenario: operator runs --dry-run to preview live replace.
    fn dry_run_live_path_shows_btrfs_replace() {
        let config_json = serde_json::json!({
            "mount_point": "/mnt/storage"
        });
        let _config: crate::config::Config =
            serde_json::from_value(config_json).expect("valid config");
        let new_probed = PresentConfigDisk {
            name: DiskName::parse("disk3").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            state: PresentConfigDiskState::PresentNotLuks,
        };
        let source = ReplaceSource::Live {
            mapper: MapperName::from_basename("braid-disk2".into()),
            devid: Devid::new(2),
        };
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint::new("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .render_steps();
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
            "mount_point": "/mnt/storage"
        });
        let _config: crate::config::Config =
            serde_json::from_value(config_json).expect("valid config");
        let new_probed = PresentConfigDisk {
            name: DiskName::parse("disk3").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            state: PresentConfigDiskState::PresentNotLuks,
        };
        let source = ReplaceSource::Missing {
            devid: Devid::new(2),
        };
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint::new("/mnt/storage".into()),
            will_clear_last_missing: true,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .render_steps();
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
                    mapper: MapperName::from_basename("braid-disk2".into()),
                    devid: Devid::new(2),
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
    // Intent: dead path resolution auto-detects the missing devid from
    // the persisted member.
    // Why: when the operator does not supply `--missing-id`, planning uses
    // `old_member.devid` (cross-checked against btrfs missing_devids) to
    // pick the target. Pattern 4 / persisted-devid cross-check.
    // Scenario: operator replaces a dead disk (1 missing device, no --missing-id).
    fn dead_old_resolution_single_missing() {
        let mut pool = two_device_pool();
        // Simulate disk2 missing
        pool.devices.retain(|d| d.mapper.as_str() != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        pool.missing_devids = vec![Devid::new(2)];
        let (uuid, member) = disk2_member_for_two_device_pool();
        let result = resolve_replace_source(&disk_name("disk2"), &uuid, &member, None, &pool);
        assert!(
            matches!(result, Ok(ReplaceSource::Missing { devid }) if devid == Devid::new(2)),
            "expected Missing {{ devid: 2 }}, got: {result:?}"
        );
    }

    #[test]
    // Intent: dead path with explicit --missing-id resolves to that devid
    // when it matches the persisted devid.
    // Why: regression guard for --missing-id path.
    // Scenario: operator passes --missing-id 2 for disk2 whose pool.json
    // entry records devid 2.
    fn dead_old_resolution_with_devid() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.as_str() != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        pool.missing_devids = vec![Devid::new(2)];
        let (uuid, member) = disk2_member_for_two_device_pool();
        let result = resolve_replace_source(
            &disk_name("disk2"),
            &uuid,
            &member,
            Some(Devid::new(2)),
            &pool,
        );
        assert!(
            matches!(result, Ok(ReplaceSource::Missing { devid }) if devid == Devid::new(2)),
            "expected Missing {{ devid: 2 }}, got: {result:?}"
        );
    }

    // Intent: with two devids missing and no `--missing-id`, auto-resolve selects
    //   the devid recorded in pool.json, independent of the missing count.
    // Why it exists: the auto-resolve-independent-of-count contract is
    //   unverified; every other dead-disk test uses a single-element missing
    //   set, so indexing `missing_devids[0]` or requiring `--missing-id` for
    //   multiple missing devices would otherwise pass.
    // Scenario: two devices are missing (devids 2 and 3); the operator runs
    //   `braid replace --old disk3` with no `--missing-id`, and pool.json
    //   records the old member as devid 3.
    #[test]
    fn dead_old_resolution_multiple_missing_picks_persisted_devid() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.as_str() != "braid-disk2");
        pool.missing_count = 2;
        pool.total_devices = 3;
        pool.missing_devids = vec![Devid::new(2), Devid::new(3)];
        let uuid = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let member = membership::DiskMember {
            name: disk_name("disk3"),
            by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            devid: Some(Devid::new(3)),
            added_at: None,
        };
        let result = resolve_replace_source(&disk_name("disk3"), &uuid, &member, None, &pool);
        assert!(
            matches!(result, Ok(ReplaceSource::Missing { devid }) if devid == Devid::new(3)),
            "expected Missing {{ devid: 3 }} (persisted devid disambiguates the \
             two-element missing set), got: {result:?}"
        );
    }

    // Intent: explicit `--missing-id` refuses a null-underlying-only devid
    // with the hot-unplug diagnostic.
    // Why it exists: status reports null-underlying devids as alert-missing,
    // but replace must wait until btrfs promotes the devid to MISSING.
    // Scenario: the old disk's mapper remains open with `device: (null)`,
    // and the operator passes its devid to
    // `braid replace --old disk2 --new disk3=... --missing-id`.
    #[test]
    fn missing_id_null_underlying_refused() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.as_str() != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        pool.null_underlying
            .push(null_underlying_device(Devid::new(2)));
        let (uuid, member) = disk2_member_for_two_device_pool();

        let err = resolve_replace_source(
            &disk_name("disk2"),
            &uuid,
            &member,
            Some(Devid::new(2)),
            &pool,
        )
        .unwrap_err();

        assert_eq!(
            err.to_string(),
            "devid 2 is hot-unplugged but btrfs has not yet promoted it to MISSING \
             (LUKS mapper open, backing device gone). `braid replace` only operates on \
             btrfs-authoritative MISSING devids. Confirm the disk is truly gone, then \
             relock and re-unlock the pool degraded (`braid lock` then `braid unlock \
             --allow-degraded`) so btrfs promotes devid 2, and retry."
        );
    }

    // Intent: auto-resolve refuses a persisted null-underlying-only devid
    // with the hot-unplug diagnostic.
    // Why it exists: no-flag dead-disk replacement must not fall back to the
    // generic no-missing wording when status has already surfaced the devid.
    // Scenario: pool.json records disk2 as devid 2, that mapper is
    // null-underlying, and the operator runs `braid replace` without
    // `--missing-id`.
    #[test]
    fn auto_resolve_null_underlying_refused() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.as_str() != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        pool.null_underlying
            .push(null_underlying_device(Devid::new(2)));
        let (uuid, member) = disk2_member_for_two_device_pool();

        let err =
            resolve_replace_source(&disk_name("disk2"), &uuid, &member, None, &pool).unwrap_err();

        assert_eq!(
            err.to_string(),
            "devid 2 is hot-unplugged but btrfs has not yet promoted it to MISSING \
             (LUKS mapper open, backing device gone). `braid replace` only operates on \
             btrfs-authoritative MISSING devids. Confirm the disk is truly gone, then \
             relock and re-unlock the pool degraded (`braid lock` then `braid unlock \
             --allow-degraded`) so btrfs promotes devid 2, and retry."
        );
    }

    // Intent: a supplied devid present in both missing and null-underlying
    // resolves through the btrfs-authoritative missing path.
    // Why it exists: btrfs can promote a hot-unplugged devid while the mapper
    // still reports `(null)`, and that overlap must not be refused.
    // Scenario: operator supplies `--missing-id 2` after btrfs has promoted
    // the hot-unplugged old disk to MISSING.
    #[test]
    fn missing_id_in_both_missing_and_null_underlying_proceeds() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.as_str() != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        pool.missing_devids = vec![Devid::new(2)];
        pool.null_underlying
            .push(null_underlying_device(Devid::new(2)));
        let (uuid, member) = disk2_member_for_two_device_pool();

        let result = resolve_replace_source(
            &disk_name("disk2"),
            &uuid,
            &member,
            Some(Devid::new(2)),
            &pool,
        );

        assert!(
            matches!(result, Ok(ReplaceSource::Missing { devid }) if devid == Devid::new(2)),
            "expected Missing {{ devid: 2 }}, got: {result:?}"
        );
    }

    #[test]
    // Intent: --missing-id pointing to a live device is rejected.
    // Why: the operator may have confused devids; replacing a live device
    //   via the missing path would corrupt data.
    // Scenario: operator passes --missing-id with the devid of a healthy
    //   disk while pool.json records devid 1 for `--old`.
    fn missing_id_pointing_to_live_device_rejected() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.as_str() != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        // disk2's persisted member pins devid 1 (the live one) so the
        // operator's --missing-id 1 lines up with the persisted entry --
        // but devid 1 is live in `pool.devices`, so the live-check fires.
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let member = membership::DiskMember {
            name: disk_name("disk2"),
            by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
            devid: Some(Devid::new(1)),
            added_at: None,
        };
        let err = resolve_replace_source(
            &disk_name("disk2"),
            &uuid,
            &member,
            Some(Devid::new(1)),
            &pool,
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("live device"),
            "expected 'live device' error, got: {err}"
        );
    }

    #[test]
    // Intent: --missing-id that disagrees with the persisted member's
    // devid is rejected before any btrfs cross-check.
    // Why: --old and --missing-id must agree about which member is being
    //   replaced; the persisted devid is the source of truth and a
    //   disagreement is a typo guard.
    // Scenario: operator passes --missing-id 99 but disk2's persisted
    //   member records devid 2.
    fn missing_id_disagrees_with_persisted_devid() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.as_str() != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let (uuid, member) = disk2_member_for_two_device_pool();
        let err = resolve_replace_source(
            &disk_name("disk2"),
            &uuid,
            &member,
            Some(Devid::new(99)),
            &pool,
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains("Some(2)"),
            "must not leak Debug Option wrapper: {msg}"
        );
        let stale_btrfs_report = ["btrfs reports", "missing devid"].join(" ");
        assert!(
            !msg.contains(&stale_btrfs_report),
            "must not attribute --missing-id to btrfs: {msg}"
        );
        assert!(
            msg.contains("records devid 2") && msg.contains("--missing-id was 99"),
            "should show persisted devid 2 and supplied 99: {msg}"
        );
        match err {
            ReplaceError::OldDevidMismatch {
                old_name,
                pool_devid,
                supplied_devid,
            } => {
                assert_eq!(old_name, "disk2");
                assert_eq!(pool_devid, Devid::new(2));
                assert_eq!(supplied_devid, Devid::new(99));
            }
            other => panic!("expected OldDevidMismatch, got: {other:?}"),
        }
    }

    #[test]
    // Intent: persisted devid not in btrfs missing_devids fails closed.
    // Why: pool.json's view of which devid is missing must agree with
    //   btrfs's view; a stale pool.json (e.g. devid was reclaimed by a
    //   subsequent add) must not silently fall through to the live
    //   path through the missing arm.
    // Scenario: pool.json records disk2 with devid 2, but btrfs reports
    //   devid 3 missing instead.
    fn persisted_devid_not_in_missing_set_rejected() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.as_str() != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        pool.missing_devids = vec![Devid::new(3)];
        let (uuid, member) = disk2_member_for_two_device_pool();
        let err =
            resolve_replace_source(&disk_name("disk2"), &uuid, &member, None, &pool).unwrap_err();
        assert!(
            err.to_string()
                .contains("Pool membership may be out of date"),
            "expected stale-pool message, got: {err}"
        );
    }

    #[test]
    // Intent: missing-path with no persisted devid fails closed.
    // Why: without a persisted devid the operator and btrfs cannot agree
    //   on which physical member is being replaced. Reject with the
    //   structured `OldMemberMissingDevid` variant.
    // Scenario: pool.json's disk2 entry has `devid: None` (e.g. discover
    //   bootstrap had not enriched it yet).
    fn missing_path_without_persisted_devid_rejected() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.as_str() != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let member = membership::DiskMember {
            name: disk_name("disk2"),
            by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
            devid: None,
            added_at: None,
        };
        let err =
            resolve_replace_source(&disk_name("disk2"), &uuid, &member, None, &pool).unwrap_err();
        assert!(
            matches!(err, ReplaceError::OldMemberMissingDevid { .. }),
            "expected OldMemberMissingDevid, got: {err:?}"
        );
    }

    #[test]
    // Intent: supplied --missing-id cannot rescue a missing persisted
    //   devid.
    // Why: `OldMemberMissingDevid` is the sole remediation path when
    //   pool.json lacks the old member's devid; operator input is only a
    //   cross-check against persisted state.
    // Scenario: pool.json's disk2 entry has `devid: None`, and the
    //   operator passes `--missing-id 2` for the missing-path replace.
    fn missing_path_without_persisted_devid_rejected_with_missing_id() {
        let mut pool = two_device_pool();
        pool.devices.retain(|d| d.mapper.as_str() != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let member = membership::DiskMember {
            name: disk_name("disk2"),
            by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
            devid: None,
            added_at: None,
        };
        let err = resolve_replace_source(
            &disk_name("disk2"),
            &uuid,
            &member,
            Some(Devid::new(2)),
            &pool,
        )
        .unwrap_err();
        assert!(
            matches!(err, ReplaceError::OldMemberMissingDevid { .. }),
            "expected OldMemberMissingDevid, got: {err:?}"
        );
    }

    fn make_replace_config() -> crate::config::Config {
        let config_json = serde_json::json!({
            "mount_point": "/mnt/storage"
        });
        serde_json::from_value(config_json).expect("valid config")
    }

    fn new_probed_not_luks() -> PresentConfigDisk {
        PresentConfigDisk {
            name: DiskName::parse("disk3").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            state: PresentConfigDiskState::PresentNotLuks,
        }
    }

    // Intent: build_replace_journal_target records a fresh replacement disk
    // as FreshLuks with the generated braid label.
    // Why it exists: recovery relies on the journaled FreshLuks label and
    // effective luksFormat args to recognize an interrupted prepared target.
    // Scenario: replace plans against a present non-LUKS disk with an enroll
    // key file and extra luksFormat options.
    #[test]
    fn build_replace_journal_target_records_fresh_luks_target() {
        let new_by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap();
        let key_file = KeyFilePath::new(std::path::PathBuf::from("/run/keys/braid-disk3.key"));
        let extra_opts = LuksFormatExtraOpts::parse(&["--pbkdf".to_owned(), "pbkdf2".to_owned()])
            .expect("valid extras");
        let new_probed = PresentConfigDisk {
            name: DiskName::parse("disk3").expect("valid disk name in test fixture"),
            by_id_path: new_by_id.clone(),
            state: PresentConfigDiskState::PresentNotLuks,
        };

        let target =
            build_replace_journal_target(&new_by_id, &new_probed, Some(&key_file), &extra_opts);

        assert_eq!(target.by_id, new_by_id);
        match target.mode {
            journal::ReplaceJournalMode::FreshLuks {
                extra_opts: got_extras,
                enroll_key_file,
            } => {
                // Structured extras: only the user-supplied tokens. No
                // raw `--label braid-<name>` injection -- label flows
                // through the structured `CryptsetupLuksFormat.label` field.
                assert_eq!(got_extras.as_slice(), &["--pbkdf", "pbkdf2"]);
                assert_eq!(enroll_key_file, Some(key_file));
            }
            other => panic!("expected FreshLuks journal target, got {other:?}"),
        }
    }

    // Intent: build_replace_journal_target records an already-LUKS
    // replacement disk as ExistingLuks; identity flows through the
    // op-level `new_uuid` rather than a value-side field in the variant.
    // Why it exists: recovery must not run FreshLuks label matching or
    // keyfile/header-prep replay for a disk that was already LUKS.
    // Scenario: replace plans against a present LUKS disk whose mapper is not
    // yet open.
    #[test]
    fn build_replace_journal_target_records_existing_luks_target() {
        let new_by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap();
        let luks_uuid = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let new_probed = PresentConfigDisk {
            name: DiskName::parse("disk3").expect("valid disk name in test fixture"),
            by_id_path: new_by_id.clone(),
            state: PresentConfigDiskState::PresentLuks {
                uuid: luks_uuid.clone(),
                label: Some("braid-disk3".to_owned()),
                mapper_open: false,
            },
        };

        let target = build_replace_journal_target(
            &new_by_id,
            &new_probed,
            None,
            &LuksFormatExtraOpts::default(),
        );

        assert_eq!(target.by_id, new_by_id);
        match target.mode {
            journal::ReplaceJournalMode::ExistingLuks { enroll_key_file } => {
                assert_eq!(enroll_key_file, None);
            }
            other => panic!("expected ExistingLuks journal target, got {other:?}"),
        }
    }

    // Intent: build_replace_journal_source preserves the live-source mapper
    // and devid in the journal.
    // Why it exists: recovery uses this identity to close only the replaced
    // old mapper and to distinguish live-source from missing-source cleanup.
    // Scenario: replace plans from an old disk that is still present in the
    // live btrfs pool.
    #[test]
    fn build_replace_journal_source_records_live_mapper() {
        let source = ReplaceSource::Live {
            mapper: MapperName::from_basename("braid-disk2".into()),
            devid: Devid::new(2),
        };

        let journal_source = build_replace_journal_source(&source);

        assert_eq!(
            journal_source,
            journal::ReplaceJournalSource::Live {
                old_devid: Devid::new(2),
                old_mapper: MapperName::from_basename("braid-disk2".into()),
            }
        );
    }

    // Intent: build_replace_journal_source preserves the missing-source
    // devid without inventing an old mapper.
    // Why it exists: missing-source recovery must not try to close a mapper
    // for a device that was already gone.
    // Scenario: replace plans by `--missing-id 2`.
    #[test]
    fn build_replace_journal_source_records_missing_devid() {
        let source = ReplaceSource::Missing {
            devid: Devid::new(2),
        };

        let journal_source = build_replace_journal_source(&source);

        assert_eq!(
            journal_source,
            journal::ReplaceJournalSource::Missing {
                old_devid: Devid::new(2),
            }
        );
    }

    #[test]
    // Intent: missing-path dry-run (not last missing) omits rebalance step.
    // Why: if other missing devices remain, a rebalance would be premature.
    // Scenario: 3-disk pool, 2 missing, replacing 1 -- still degraded after.
    fn dry_run_missing_not_last_omits_rebalance() {
        let _config = make_replace_config();
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Missing {
            devid: Devid::new(2),
        };
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint::new("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 3,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .render_steps();
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
        let source = ReplaceSource::Missing {
            devid: Devid::new(2),
        };
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint::new("/mnt/storage".into()),
            will_clear_last_missing: true,
            total_devices: 1,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .render_steps();
        let descriptions: Vec<&str> = steps.iter().map(|s| s.description.as_str()).collect();
        assert!(
            !descriptions
                .iter()
                .any(|d| d.contains("-dconvert=raid1,soft")),
            "should NOT show soft balance with total_devices == 1, got: {descriptions:?}"
        );
    }

    use crate::cmd::CmdError;
    use crate::membership::{self, PoolMembership};
    use crate::test_fixtures::{
        MockFs, PoolFixture, ReplacementPool, mock_ok, replace_dev_info_sufficient,
        with_lsblk_hw_info,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

    /// Override handler that fails `BtrfsReplaceStart` so live-path
    /// failure tests can drive cmd_replace through preflight + journal
    /// write and watch the failure propagate. Layered on top of the
    /// canonical pool topology via `with_handler`.
    fn replace_start_fails_handler()
    -> impl Fn(&CmdRequest) -> Option<Result<RawCommandOutput, CmdError>> + Send + Sync + 'static
    {
        |req| match req {
            CmdRequest::BtrfsReplaceStart { .. } => Some(Ok(RawCommandOutput {
                cmd: "btrfs replace start".into(),
                stdout: String::new(),
                stderr: "ERROR: target device is too small".into(),
                exit_status: 1,
            })),
            _ => None,
        }
    }

    fn plan_replace<R, F>(
        runner: &R,
        fs: &F,
        params: &ReplaceParams<'_>,
    ) -> Result<ReplacePlan, PlanFailure<ReplaceError>>
    where
        R: CommandRunner + Sync,
        F: Filesystem + ?Sized,
    {
        let dev_info = replace_dev_info_sufficient();
        super::plan_replace(runner, fs, &dev_info, params)
    }

    fn cmd_replace<R, F>(runner: &R, fs: &F, params: &ReplaceParams<'_>) -> Result<(), ReplaceError>
    where
        R: CommandRunner + Sync,
        F: Filesystem + ?Sized,
    {
        let dev_info = replace_dev_info_sufficient();
        super::cmd_replace(runner, fs, &dev_info, params)
    }

    const REPLACE_TEST_FSID: &str = "cc86845b-aec3-408e-bef5-553affc1f2b1";

    // Intent: a declined replace confirmation aborts before irreversible
    //   side effects.
    // Why it exists: the interactive gate must remain before the sleep
    //   inhibitor and journal write so a decline cannot strand recovery state.
    // Scenario: an operator starts replacing live disk2 with disk3 and
    //   declines at the prompt.
    #[test]
    fn cmd_replace_declined_confirm_aborts_before_side_effects() {
        let f = PoolFixture::two_disk_healthy();
        f.confirm.decline();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(replace_start_fails_handler());

        let err = cmd_replace(&runner, &fs, &f.replace_params().yes(false).build())
            .expect_err("declined confirm should abort");

        assert_eq!(err.to_string(), "aborted by user");
        assert_eq!(f.inhibitor.acquire_count(), 0);
        assert!(journal::load_journal(&f.paths).unwrap().is_none());
        let calls = runner.requests();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsReplaceStart { .. })),
            "declined confirm must not issue BtrfsReplaceStart: {calls:?}"
        );
    }

    // Intent: accepted replace confirmation records the exact assembled
    //   prompt, including the single-device warning bytes.
    // Why it exists: the confirm seam must receive the formatter output plus
    //   the warning exactly once when the planned topology has one disk.
    // Scenario: replacing the only live source leaves a single-disk pool, so
    //   the operator sees the normal replace prompt and the no-redundancy warning.
    #[test]
    fn cmd_replace_accepted_confirm_records_prompt_with_warning() {
        let f = PoolFixture::empty();
        f.confirm.accept();
        let new_by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap();
        let new_probed = PresentConfigDisk {
            name: disk_name("disk3"),
            by_id_path: new_by_id.clone(),
            state: PresentConfigDiskState::PresentNotLuks,
        };
        let source = ReplaceSource::Live {
            mapper: MapperName::from_basename("braid-disk2".into()),
            devid: Devid::new(2),
        };
        let plan = ReplacePlan {
            notes: vec![],
            work_plan: replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
                new_name: "disk3",
                new_by_id: &new_by_id,
                new_probed: &new_probed,
                replace_source: &source,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                will_clear_last_missing: false,
                total_devices: 1,
                paths: &f.paths,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
            }),
        };
        let fs = MockFs::unmounted(vec![]);
        let runner = MockRunner::default();

        let _ = plan.execute(&runner, &fs, &f.replace_params().yes(false).build());

        let mut expected = format!(
            "{}\n",
            format_replace_confirm(
                &ReplaceConfirmOld {
                    name: "disk2",
                    hw: Some(&confirm::DiskHwInfo::default()),
                    source: &source,
                },
                &ReplaceConfirmNew {
                    name: "disk3",
                    by_id: "/dev/disk/by-id/virtio-disk3",
                    hw: &confirm::DiskHwInfo::default(),
                    needs_luks_format: true,
                    is_rebuild: false,
                },
                1,
            )
        );
        expected.push_str("WARNING: This replace leaves only 1 disk -- no redundancy.\n\n");
        assert_eq!(f.confirm.prompts(), vec![expected]);
    }

    // Intent: a live (present old) -> fresh (not-present new) replace confirm
    //   resolves the OLD hw line from the live backing path (/dev/test-2) and
    //   the NEW hw line from the by-id handle (/dev/disk/by-id/virtio-disk3),
    //   per decision 024 -- and never transposes the two device args.
    // Why it exists: replace hands `query_disk_hw_info` two different device
    //   sources (`pool.underlying_for_uuid` for the present old disk, `new_by_id`
    //   for the not-yet-present new disk), but nothing pinned that routing
    //   through execute(): the sibling confirm test builds both prompts from
    //   `DiskHwInfo::default()` against a runner with no LsblkField handler, so
    //   `get_lsblk_field`'s `.ok()?` swallow of `MissingMock` blanks both lines
    //   no matter which path is queried. Distinct old/new values plus a
    //   byte-exact assertion make this both path-sensitive (wrong path -> blank
    //   line -> mismatch) and swap-sensitive (old/new args transposed -> old
    //   line shows NEW_MODEL -> mismatch), the most plausible replace regression.
    // Scenario: replacing the only live disk2 with fresh disk3 leaves a
    //   single-disk pool; the operator's prompt shows disk2's hardware probed
    //   from /dev/test-2 and disk3's from its by-id handle.
    #[test]
    fn cmd_replace_confirm_hw_lines_route_old_to_live_new_to_by_id() {
        const OLD_MODEL: &str = "WD Red Plus WD60EFPX";
        const OLD_SERIAL: &str = "OLDDISK2SERIAL";
        const OLD_SIZE: u64 = 6_000_000_000_000;
        const NEW_MODEL: &str = "Seagate IronWolf ST8000VN004";
        const NEW_SERIAL: &str = "NEWDISK3SERIAL";
        const NEW_SIZE: u64 = 8_000_000_000_000;

        let f = PoolFixture::empty();
        f.confirm.accept();
        let new_by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap();
        let new_probed = PresentConfigDisk {
            name: disk_name("disk3"),
            by_id_path: new_by_id.clone(),
            state: PresentConfigDiskState::PresentNotLuks,
        };
        let source = ReplaceSource::Live {
            mapper: MapperName::from_basename("braid-disk2".into()),
            devid: Devid::new(2),
        };
        let plan = ReplacePlan {
            notes: vec![],
            work_plan: replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
                new_name: "disk3",
                new_by_id: &new_by_id,
                new_probed: &new_probed,
                replace_source: &source,
                mount_point: &MountPoint::new("/mnt/storage".into()),
                will_clear_last_missing: false,
                total_devices: 1,
                paths: &f.paths,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
            }),
        };
        let fs = MockFs::unmounted(vec![]);
        // Old disk2's live backing is /dev/test-2 (replace_work_plan_test_pool
        // sets the live source's `underlying` to /dev/test-{devid}); the fresh
        // new disk is probed via its by-id handle.
        let runner = with_lsblk_hw_info(
            with_lsblk_hw_info(
                MockRunner::default(),
                "/dev/test-2",
                OLD_MODEL,
                OLD_SERIAL,
                OLD_SIZE,
            ),
            "/dev/disk/by-id/virtio-disk3",
            NEW_MODEL,
            NEW_SERIAL,
            NEW_SIZE,
        );

        // execute() fails downstream on this minimal runner, but the prompt is
        // already recorded at the confirm gate -- the assertion target.
        let _ = plan.execute(&runner, &fs, &f.replace_params().yes(false).build());

        let old_hw = confirm::DiskHwInfo {
            model: Some(OLD_MODEL.into()),
            serial: Some(OLD_SERIAL.into()),
            size: Some(OLD_SIZE),
        };
        let new_hw = confirm::DiskHwInfo {
            model: Some(NEW_MODEL.into()),
            serial: Some(NEW_SERIAL.into()),
            size: Some(NEW_SIZE),
        };
        let mut expected = format!(
            "{}\n",
            format_replace_confirm(
                &ReplaceConfirmOld {
                    name: "disk2",
                    hw: Some(&old_hw),
                    source: &source,
                },
                &ReplaceConfirmNew {
                    name: "disk3",
                    by_id: "/dev/disk/by-id/virtio-disk3",
                    hw: &new_hw,
                    needs_luks_format: true,
                    is_rebuild: false,
                },
                1,
            )
        );
        expected.push_str("WARNING: This replace leaves only 1 disk -- no redundancy.\n\n");
        assert_eq!(f.confirm.prompts(), vec![expected]);
    }

    // Intent: accepted replace confirmation does not block the mutation.
    // Why it exists: the seam must preserve the happy path, not just the
    //   declined abort path.
    // Scenario: the operator accepts a live replace and braid reaches
    //   `btrfs replace start`.
    #[test]
    fn cmd_replace_accepted_confirm_proceeds_to_replace_start() {
        let f = PoolFixture::two_disk_healthy();
        f.confirm.accept();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(replace_start_fails_handler());

        let result = cmd_replace(&runner, &fs, &f.replace_params().yes(false).build());

        assert!(
            result.is_err(),
            "test runner forces replace start to fail after the gate"
        );
        let calls = runner.requests();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsReplaceStart { .. })),
            "accepted confirm must reach BtrfsReplaceStart: {calls:?}"
        );
    }

    fn fresh_luks_execute_plan_for_test(f: &PoolFixture, new_uuid: LuksUuid) -> ReplacePlan {
        let old_uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let new_name = disk_name("disk3");
        let new_by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap();

        let pool = PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName::from_basename("braid-disk1".into()),
                    luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                    devid: Devid::new(1),
                    underlying: "/dev/vdb".into(),
                },
                PoolDevice {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                    luks_uuid: old_uuid.clone(),
                    devid: Devid::new(2),
                    underlying: "/dev/vdc".into(),
                },
            ],
            missing_count: 0,
            total_devices: 2,
            fsid: Some(Fsid::parse(REPLACE_TEST_FSID).unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        };

        let replace_source = ReplaceSource::Live {
            mapper: MapperName::from_basename("braid-disk2".into()),
            devid: Devid::new(2),
        };
        let member_verify_targets = build_member_verify_targets(
            &PoolMembership::empty(),
            &pool,
            &replace_source,
            &old_uuid,
        );

        ReplacePlan {
            notes: vec![],
            work_plan: build_replace_work_plan(ReplaceWorkPlanInput {
                config: f.config.clone(),
                old_uuid,
                old_name: disk_name("disk2"),
                new_uuid,
                new_name: new_name.clone(),
                new_by_id: new_by_id.clone(),
                new_probed: PresentConfigDisk {
                    name: new_name,
                    by_id_path: new_by_id,
                    state: PresentConfigDiskState::PresentNotLuks,
                },
                replace_source,
                pool,
                paths: &f.paths,
                enroll_key_file: None,
                luks_format_extra_opts: LuksFormatExtraOpts::parse(&[]).unwrap(),
                member_verify_targets,
            }),
        }
    }

    fn btrfs_show_pool_text(fsid: &str, devices: &[(&str, u64)]) -> String {
        let mut out = format!(
            "Label: none  uuid: {fsid}\n\tTotal devices {} FS bytes used 16.17MiB\n",
            devices.len()
        );
        for (mapper, devid) in devices {
            out.push_str(&format!(
                "\tdevid    {devid} size 496.00MiB used 121.56MiB path /dev/mapper/{mapper}\n"
            ));
        }
        out
    }

    fn mock_status_active(mapper: &str, underlying: &str) -> RawCommandOutput {
        mock_ok(
            &format!("cryptsetup status {mapper}"),
            &format!(
                "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {underlying}\n  mode:    read/write\n"
            ),
        )
    }

    fn execute_gate_runner(
        fresh_pool_show: Option<String>,
        clone_uuid: LuksUuid,
        mock_downstream_success_until_replace: bool,
    ) -> MockRunner {
        let replace_done = Arc::new(AtomicBool::new(false));
        ReplacementPool::two_disk_healthy()
            .with_mapper_closed("braid-disk3")
            .install(MockRunner::default(), replace_done)
            .with_handler(move |req| match req {
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    fresh_pool_show.as_ref().map(|show| {
                        Ok(mock_ok(
                            &format!("btrfs filesystem show {mount_point}"),
                            show,
                        ))
                    })
                }
                CmdRequest::CryptsetupStatus { mapper } if mapper.as_str() == "clone-foreign" => {
                    Some(Ok(mock_status_active("clone-foreign", "/dev/vde")))
                }
                CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vde" => Some(Ok(
                    mock_ok("cryptsetup luksUUID /dev/vde", &format!("{clone_uuid}\n")),
                )),
                CmdRequest::CryptsetupLuksFormat { device, .. }
                    if mock_downstream_success_until_replace =>
                {
                    Some(Ok(mock_ok(&format!("cryptsetup luksFormat {device}"), "")))
                }
                CmdRequest::CryptsetupLuksHeaderBackup { device, .. }
                    if mock_downstream_success_until_replace =>
                {
                    Some(Ok(mock_ok(
                        &format!("cryptsetup luksHeaderBackup {device}"),
                        "",
                    )))
                }
                CmdRequest::CryptsetupLuksOpen { device, .. }
                    if mock_downstream_success_until_replace =>
                {
                    Some(Ok(mock_ok(&format!("cryptsetup open {device}"), "")))
                }
                CmdRequest::BtrfsReplaceStart { .. } if mock_downstream_success_until_replace => {
                    Some(Ok(RawCommandOutput {
                        cmd: "btrfs replace start".into(),
                        stdout: String::new(),
                        stderr: "ERROR: target device is too small".into(),
                        exit_status: 1,
                    }))
                }
                _ => None,
            })
    }

    // Intent: `ReplacePlan::execute` re-probes the live pool before the
    //   inhibitor and journal, rejecting a FreshLuks target UUID that appears
    //   in the fresh live pool.
    // Why it exists: confirmation and passphrase prompts leave a TOCTOU window
    //   after planning. A cloned replacement UUID added to btrfs during that
    //   pause must hit the canonical LivePool duplicate refusal before any
    //   pending-op.json or btrfs replace start.
    // Scenario: disk3 was planned as a fresh replacement; before execution
    //   reaches the journal, `/dev/mapper/clone-foreign` with disk3's UUID
    //   appears in the mounted pool.
    #[test]
    fn execute_rechecks_live_pool_rejects_fresh_luks_uuid_collision() {
        let f = PoolFixture::two_disk_healthy();
        let new_uuid = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let plan = fresh_luks_execute_plan_for_test(&f, new_uuid.clone());
        let show = btrfs_show_pool_text(
            REPLACE_TEST_FSID,
            &[("braid-disk1", 1), ("braid-disk2", 2), ("clone-foreign", 3)],
        );
        let runner = execute_gate_runner(Some(show), new_uuid.clone(), false);
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);

        let result = plan.execute(&runner, &fs, &f.replace_params().build());

        match result {
            Err(ReplaceError::DuplicateUuid { uuid, scope }) => {
                assert_eq!(uuid, new_uuid);
                assert_eq!(scope, DuplicateUuidScope::LivePool);
            }
            other => panic!("expected DuplicateUuid {{ LivePool }}, got: {other:?}"),
        }
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "execute-time live-pool duplicate must reject before inhibitor acquisition"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "execute-time live-pool duplicate must not write pending-op.json"
        );
        let log = runner.requests();
        assert!(
            !log.iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "no BtrfsReplaceStart may issue after execute-time live-pool duplicate"
        );
    }

    // Intent: `ReplacePlan::execute` fails closed if the planned mounted pool
    //   disappears before the pre-journal live-pool re-check.
    // Why it exists: replacing against stale mounted-pool state would write a
    //   journal for a filesystem that is no longer the one the user approved.
    // Scenario: the pool was mounted during planning, but is unmounted after
    //   confirmation/passphrase verification and before journal write.
    #[test]
    fn execute_rechecks_live_pool_rejects_unmounted_pool() {
        let f = PoolFixture::two_disk_healthy();
        let new_uuid = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let plan = fresh_luks_execute_plan_for_test(&f, new_uuid.clone());
        let runner = execute_gate_runner(None, new_uuid, false);
        let fs = MockFs::unmounted(vec!["/dev/disk/by-id/virtio-disk3".into()]);

        let result = plan.execute(&runner, &fs, &f.replace_params().build());

        match result {
            Err(ReplaceError::Validation(msg)) => {
                assert!(
                    msg.contains("pool unmounted between planning and execution"),
                    "expected unmounted-pool validation, got: {msg}"
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert_eq!(f.inhibitor.acquire_count(), 0);
        assert!(journal::load_journal(&f.paths).unwrap().is_none());
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "no BtrfsReplaceStart may issue after unmounted-pool validation"
        );
    }

    // Intent: `ReplacePlan::execute` fails closed if the mounted pool FSID
    //   changes before the pre-journal live-pool re-check.
    // Why it exists: the fresh live-pool probe is only meaningful if it still
    //   describes the same btrfs filesystem that produced the plan.
    // Scenario: `/mnt/storage` is remounted to a different btrfs filesystem
    //   after planning but before replace writes pending-op.json.
    #[test]
    fn execute_rechecks_live_pool_rejects_fsid_drift() {
        let f = PoolFixture::two_disk_healthy();
        let new_uuid = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let plan = fresh_luks_execute_plan_for_test(&f, new_uuid.clone());
        let show = btrfs_show_pool_text(
            "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee",
            &[("braid-disk1", 1), ("braid-disk2", 2)],
        );
        let runner = execute_gate_runner(Some(show), new_uuid, false);
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);

        let result = plan.execute(&runner, &fs, &f.replace_params().build());

        match result {
            Err(ReplaceError::Validation(msg)) => {
                assert!(
                    msg.contains("pool fsid changed between planning and execution"),
                    "expected FSID-drift validation, got: {msg}"
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert_eq!(f.inhibitor.acquire_count(), 0);
        assert!(journal::load_journal(&f.paths).unwrap().is_none());
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "no BtrfsReplaceStart may issue after FSID-drift validation"
        );
    }

    // Intent: `ReplacePlan::execute` allows a fresh live-pool re-check whose
    //   FSID still matches and whose devices do not carry the replacement UUID.
    // Why it exists: the execute-time gate must be a collision guard, not a
    //   blanket refusal of normal replace execution.
    // Scenario: disk3 was planned as a fresh replacement; the fresh pool probe
    //   still shows only disk1 and disk2, so execution proceeds to the journal
    //   and then fails at an intentional mocked `btrfs replace start` error.
    #[test]
    fn execute_rechecks_live_pool_allows_clean_pool_before_journal() {
        let f = PoolFixture::two_disk_healthy();
        let new_uuid = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let plan = fresh_luks_execute_plan_for_test(&f, new_uuid.clone());
        let runner = execute_gate_runner(None, new_uuid, true);
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);

        let result = plan.execute(&runner, &fs, &f.replace_params().build());

        assert!(
            matches!(
                result,
                Err(ReplaceError::JournalLifecycle { ref source, .. })
                    if matches!(source.as_ref(), ReplaceError::Pool(_))
            ),
            "expected downstream pool failure after clean re-check, got: {result:?}"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "clean re-check should proceed to inhibitor acquisition"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "clean re-check should proceed through pending-op.json write"
        );
        assert!(
            runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "clean re-check should proceed to BtrfsReplaceStart"
        );
    }

    // Intent: `ReplacePlan::execute` re-loads pool.json after the inhibitor
    //   and rejects a drift that dropped the old member out of membership,
    //   before journaling -- mirroring RemovePlan::execute's drift guard.
    // Why it exists: execute journals/saves from the re-derived membership;
    //   without a fresh by_uuid re-check, a pool.json rewrite during the
    //   confirmation/passphrase/inhibitor window would let the derive run
    //   against a stale snapshot and persist a misleading membership.
    // Scenario: disk2 is planned for replacement, then pool.json is rewritten
    //   to drop disk2 before execute reaches the journal. execute must fail
    //   with no journal and the drifted pool.json left untouched.
    #[test]
    fn replace_execute_rejects_when_pool_json_drifts_after_planning() {
        let f = PoolFixture::two_disk_healthy();
        let new_uuid = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let plan = fresh_luks_execute_plan_for_test(&f, new_uuid.clone());
        let runner = execute_gate_runner(None, new_uuid, true);
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);

        // Drift: rewrite pool.json to drop disk2 (the old member) after the
        // plan was built but before execute re-loads.
        let mut drifted = PoolMembership::empty();
        drifted
            .insert(
                LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                membership::DiskMember {
                    name: disk_name("disk1"),
                    by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
                    devid: None,
                    added_at: None,
                },
            )
            .expect("drifted membership is unique");
        membership::save_membership(&drifted, &f.paths).expect("save drifted pool.json");

        let result = plan.execute(&runner, &fs, &f.replace_params().build());

        match result {
            Err(ReplaceError::Validation(msg)) => {
                assert!(
                    msg.contains("not found in pool.json membership"),
                    "expected pool.json membership error: {msg}"
                );
                assert!(msg.contains("disk2"), "expected disk2 in error: {msg}");
            }
            other => panic!("expected Err(ReplaceError::Validation), got: {other:?}"),
        }
        assert!(
            !f.paths.pending_op_json().exists(),
            "execute-time drift rejection must happen before journal write",
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "execute guard must run after acquiring the sleep inhibitor",
        );
        assert_eq!(
            membership::load_membership(&f.paths).unwrap(),
            drifted,
            "rejection must leave the drifted pool.json unchanged",
        );
    }

    // Intent: `ReplacePlan::execute`'s fresh-read re-derive runs the four-axis
    //   uniqueness invariant -- a drift that keeps the old member but adds a
    //   member colliding with the disk being added fails closed pre-journal.
    // Why it exists: the re-derive feeds `derive_replace_target_membership`'s
    //   `insert ?`; dropping or swallowing that `?` would silently let a
    //   colliding fresh read journal a conflicting membership. This pins the
    //   conflict path the absent-name guard does NOT cover.
    // Scenario: disk2 is planned for replacement with disk3
    //   (/dev/disk/by-id/virtio-disk3), then a concurrent write adds a member
    //   binding that same by-id under a foreign UUID before execute re-loads.
    //   execute must fail with a membership conflict and no journal.
    #[test]
    fn replace_execute_rejects_when_pool_json_drift_conflicts_with_new_disk() {
        let f = PoolFixture::two_disk_healthy();
        let new_uuid = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let plan = fresh_luks_execute_plan_for_test(&f, new_uuid.clone());
        let runner = execute_gate_runner(None, new_uuid, true);
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);

        // Drift: keep disk1 + disk2 (so the absent-name guard does not
        // short-circuit) and add a foreign member binding the new target's
        // by-id, so the re-derive's insert collides on the by_id axis.
        let mut drifted = PoolMembership::empty();
        drifted
            .insert(
                LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                membership::DiskMember {
                    name: disk_name("disk1"),
                    by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
                    devid: None,
                    added_at: None,
                },
            )
            .expect("disk1 insert");
        drifted
            .insert(
                LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap(),
                membership::DiskMember {
                    name: disk_name("disk2"),
                    by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
                    devid: None,
                    added_at: None,
                },
            )
            .expect("disk2 insert");
        drifted
            .insert(
                LuksUuid::parse("44444444-4444-4444-4444-444444444444").unwrap(),
                membership::DiskMember {
                    name: disk_name("decoy"),
                    by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
                    devid: None,
                    added_at: None,
                },
            )
            .expect("foreign by-id insert");
        membership::save_membership(&drifted, &f.paths).expect("save drifted pool.json");

        let result = plan.execute(&runner, &fs, &f.replace_params().build());

        match result {
            Err(ReplaceError::Membership(membership::MembershipError::Conflict(msg))) => {
                assert!(
                    msg.contains("by_id") && msg.contains("virtio-disk3"),
                    "expected by_id conflict against the new target: {msg}"
                );
            }
            other => panic!("expected Err(ReplaceError::Membership(Conflict)), got: {other:?}"),
        }
        assert!(
            !f.paths.pending_op_json().exists(),
            "membership conflict on the fresh read must reject before journal write",
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "execute guard must run after acquiring the sleep inhibitor",
        );
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
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(replace_start_fails_handler());
        let result = cmd_replace(&runner, &fs, &f.replace_params().build());

        assert!(
            result.is_err(),
            "replace should fail when btrfs replace fails"
        );
        assert!(
            result
                .as_ref()
                .unwrap_err()
                .to_string()
                .contains("run `braid recover` before retrying"),
            "post-journal replace failure must use the shared recovery advice"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        // The journal exists, which proves we got past journal::write_journal,
        // which proves the inhibitor was acquired exactly once on the way in.
        // Locks in the seam placement: if a refactor moves the acquire to a
        // post-journal point or skips it entirely, this assert flips.
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
        );
    }

    #[test]
    // Intent: cmd_replace rejects --old == --new (post-parse) with a
    //   Validation error, on the reversible side of the inhibitor/journal
    //   seam.
    //
    // Why it exists: the `--old == --new` guard in `plan_replace` is a
    //   user-visible CLI contract (operator typo protection). It fires
    //   before probe_config_disk's mapper-conflict detection would
    //   otherwise surface the same bug as a confusing mapper conflict
    //   probe error. Without direct cmd-level coverage, a refactor that
    //   drops the guard would change the rejection variant from
    //   Validation("must be different") to Probe(MapperOwnership), and a
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
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(replace_start_fails_handler());
        let result = cmd_replace(
            &runner,
            &fs,
            &f.replace_params()
                .old("disk1")
                .new_disk("disk1=/dev/disk/by-id/virtio-disk3")
                .build(),
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
            f.inhibitor.acquire_count(),
            0,
            "old==new typo must be caught before the inhibitor seam -- a caught typo must not hold logind"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
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
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(replace_start_fails_handler());
        let result = cmd_replace(&runner, &fs, &f.replace_params().dry_run(true).build());

        assert!(result.is_ok(), "dry-run should succeed: {result:?}");
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "dry-run must NOT acquire the sleep inhibitor -- it has no irreversible work to protect"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "dry-run must not write the journal"
        );
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
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                move |req| match req {
                    CmdRequest::BtrfsReplaceStart { .. } => {
                        replace_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        Some(Ok(mock_ok("btrfs replace start", "")))
                    }
                    CmdRequest::CryptsetupClose { .. } => Some(Ok(mock_ok("cryptsetup close", ""))),
                    CmdRequest::BtrfsFilesystemResize { .. } => Some(Ok(RawCommandOutput {
                        cmd: "btrfs filesystem resize".into(),
                        stdout: String::new(),
                        stderr: "ERROR: unable to resize".into(),
                        exit_status: 1,
                    })),
                    _ => None,
                }
            });
        let result = cmd_replace(&runner, &fs, &f.replace_params().build());

        match &result {
            Err(ReplaceError::JournalLifecycle { source, advice }) => match source.as_ref() {
                ReplaceError::Pool(crate::pool::PoolError::Failed(msg)) => {
                    assert!(
                        msg.contains("btrfs filesystem resize failed"),
                        "expected typed PoolError::Failed carrying resize message, got: {msg}"
                    );
                    assert!(advice.contains("run `braid recover`"));
                }
                other => panic!("expected PoolError::Failed source, got: {other:?}"),
            },
            other => {
                panic!("expected Err(ReplaceError::Pool(PoolError::Failed(..))), got: {other:?}")
            }
        }

        let log = runner.requests();
        let close_idx = log
            .iter()
            .position(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-disk2"
                )
            })
            .expect("cryptsetup close on braid-disk2 must be issued even when resize fails");
        let resize_idx = log
            .iter()
            .position(|r| {
                matches!(r, CmdRequest::BtrfsFilesystemResize { devid, .. } if *devid == Devid::new(2))
            })
            .expect("btrfs filesystem resize on devid 2 must be issued");
        assert!(
            close_idx < resize_idx,
            "close (index {close_idx}) must run BEFORE resize (index {resize_idx}) \
             so a resize failure does not strand the old dm slot"
        );

        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        let journal = journal::load_journal(&f.paths)
            .unwrap()
            .expect("journal should remain after post-replace resize failure");
        assert!(
            matches!(
                journal.op,
                journal::OpKind::Replace {
                    phase: journal::ReplacePhase::PostReplaceMaintenance,
                    ..
                }
            ),
            "journal should advance after btrfs replace commits: {:?}",
            journal.op
        );

        // Membership commits at btrfs replace start; pool.json must reflect
        // the new topology even when the post-replace resize fails.
        let saved = membership::load_membership(&f.paths)
            .expect("pool.json must exist after the membership commit");
        let saved_names: Vec<&str> = saved.names().map(|n| n.as_str()).collect();
        assert!(
            !saved_names.contains(&"disk2"),
            "old disk must be gone from pool.json once btrfs replace succeeds, \
             even when the post-replace resize fails (saved: {saved_names:?})",
        );
        let disk3_name = DiskName::parse("disk3").unwrap();
        let (_disk3_uuid, disk3) = saved
            .by_name(&disk3_name)
            .unwrap_or_else(|| panic!(
                "new disk must be in pool.json once btrfs replace succeeds (saved: {saved_names:?})",
            ));
        // luks_uuid is the key under which `disk3` is stored, so its
        // presence is implicit. `devid` and `added_at` must be present
        // from the post-replace enrichment.
        assert!(
            disk3.devid.is_some() && disk3.added_at.is_some(),
            "new disk must carry enriched metadata (devid, added_at) \
             from the post-replace probe: {disk3:?}"
        );
    }

    // Intent: live-replace's best-effort close of the old mapper closes its
    // [wait] row with [warn] when cryptsetup returns non-zero exit.
    // Why it exists: Principle 13 forbids dangling [wait] rows; a best-effort
    // close that exits the command 0 must still announce the failure on the
    // same subject so the wait window is closed for the operator.
    // Scenario: live replace of disk2 -> disk3 succeeds end-to-end except the
    // trailing cryptsetup close of the old mapper, which returns ENODEV.
    #[test]
    fn live_replace_old_close_failure_emits_warn_row() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                move |req| match req {
                    CmdRequest::BtrfsReplaceStart { .. } => {
                        replace_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        Some(Ok(mock_ok("btrfs replace start", "")))
                    }
                    CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-disk2" => {
                        Some(Ok(RawCommandOutput {
                            cmd: "cryptsetup close".into(),
                            stdout: String::new(),
                            stderr: "device does not exist".into(),
                            exit_status: 4,
                        }))
                    }
                    CmdRequest::CryptsetupClose { .. } => Some(Ok(mock_ok("cryptsetup close", ""))),
                    CmdRequest::BtrfsFilesystemResize { .. } => {
                        Some(Ok(mock_ok("btrfs filesystem resize", "")))
                    }
                    _ => None,
                }
            });

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            let result = cmd_replace(&runner, &fs, &f.replace_params().build());
            assert!(
                result.is_ok(),
                "best-effort close failure must not fail the replace command, got: {result:?}"
            );
        });

        let wait = "[wait] disk disk2: locking...";
        let warn = "[warn] disk disk2: lock failed (cryptsetup close braid-disk2 failed (exit 4): device does not exist)";
        assert!(captured.contains(wait), "missing wait row: {captured:?}");
        assert!(captured.contains(warn), "missing warn row: {captured:?}");
        assert!(
            captured.find(wait) < captured.find(warn),
            "wait must precede warn, got: {captured:?}"
        );
    }

    // Intent: live-replace routes its old-mapper best-effort close through
    // the retry helper when cryptsetup reports the mapper busy.
    // Why it exists: replacing a live disk must not leak the old mapper when a
    // transient holder makes the first close return EBUSY.
    // Scenario: live replace of disk2 -> disk3 commits, the first close of
    // braid-disk2 is busy, and the second close succeeds before resize.
    #[test]
    fn live_replace_old_retries_on_busy_then_succeeds() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let close_attempts = Arc::new(AtomicU32::new(0));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                let close_attempts = close_attempts.clone();
                move |req| match req {
                    CmdRequest::BtrfsReplaceStart { .. } => {
                        replace_done.store(true, Ordering::Relaxed);
                        Some(Ok(mock_ok("btrfs replace start", "")))
                    }
                    CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-disk2" => {
                        let attempt = close_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                        if attempt == 1 {
                            Some(Ok(RawCommandOutput {
                                cmd: "cryptsetup close".into(),
                                stdout: String::new(),
                                stderr: "device is busy".into(),
                                exit_status: 5,
                            }))
                        } else {
                            Some(Ok(mock_ok("cryptsetup close", "")))
                        }
                    }
                    CmdRequest::CryptsetupClose { .. } => Some(Ok(mock_ok("cryptsetup close", ""))),
                    CmdRequest::BtrfsFilesystemResize { .. } => {
                        Some(Ok(mock_ok("btrfs filesystem resize", "")))
                    }
                    _ => None,
                }
            });

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            let result = cmd_replace(&runner, &fs, &f.replace_params().build());
            assert!(
                result.is_ok(),
                "replace should succeed after retry: {result:?}"
            );
        });

        let close_count = runner
            .requests()
            .iter()
            .filter(|request| {
                matches!(request, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-disk2")
            })
            .count();
        assert_eq!(close_count, 2);
        assert!(
            captured.contains("[ok]   disk disk2: locked"),
            "missing terminal ok row after retry: {captured:?}"
        );
    }

    // Intent: live replace labels the post-commit close trailer with the
    //   journaled operator name even when the observed old mapper has drifted.
    // Why it exists: decision 024 forbids deriving user-facing disk labels
    //   from a mapper basename; the close target stays the observed mapper,
    //   but the status row must say 'disk2', not 'WRONG'.
    // Scenario: disk2 is live under braid-WRONG, replace commits, the
    //   close-time UUID probe confirms ownership, and the old mapper closes
    //   successfully.
    #[test]
    fn live_replace_old_close_labels_drifted_mapper_with_disk_name() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                move |req| match req {
                    CmdRequest::BtrfsFilesystemShow { mount_point }
                        if !replace_done.load(Ordering::Relaxed) =>
                    {
                        Some(Ok(mock_ok(
                            &format!("btrfs filesystem show {mount_point}"),
                            &btrfs_show_pool_text(
                                REPLACE_TEST_FSID,
                                &[("braid-disk1", 1), ("braid-WRONG", 2)],
                            ),
                        )))
                    }
                    CmdRequest::BtrfsReplaceStart { .. } => {
                        replace_done.store(true, Ordering::Relaxed);
                        Some(Ok(mock_ok("btrfs replace start", "")))
                    }
                    CmdRequest::CryptsetupStatus { mapper } if mapper.as_str() == "braid-WRONG" => {
                        Some(Ok(mock_status_active("braid-WRONG", "/dev/vdc")))
                    }
                    CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-WRONG" => {
                        Some(Ok(mock_ok("cryptsetup close braid-WRONG", "")))
                    }
                    CmdRequest::BtrfsFilesystemResize { .. } => {
                        Some(Ok(mock_ok("btrfs filesystem resize", "")))
                    }
                    _ => None,
                }
            });

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            cmd_replace(&runner, &fs, &f.replace_params().build())
                .expect("drifted old mapper should close after replace");
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
            "close trailer must not echo drifted mapper basename: {captured:?}"
        );
    }

    // Intent: live replace warns and skips the old-mapper close when the
    //   close-time UUID probe reports an inactive mapper.
    // Why it exists: inactive is now caller-classified; the helper returns it
    //   silently, so this execute path must keep the operator-facing warning.
    // Scenario: live replace of disk2 -> disk3 commits, but braid-disk2 is
    //   already closed before the post-commit best-effort close runs.
    #[test]
    fn live_replace_old_close_inactive_warns_and_skips_close() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                move |req| match req {
                    CmdRequest::BtrfsReplaceStart { .. } => {
                        replace_done.store(true, Ordering::Relaxed);
                        Some(Ok(mock_ok("btrfs replace start", "")))
                    }
                    CmdRequest::CryptsetupStatus { mapper }
                        if mapper.as_str() == "braid-disk2"
                            && replace_done.load(Ordering::Relaxed) =>
                    {
                        Some(Ok(RawCommandOutput {
                            cmd: "cryptsetup status braid-disk2".into(),
                            stdout: String::new(),
                            stderr: "/dev/mapper/braid-disk2 is inactive.\n".into(),
                            exit_status: 4,
                        }))
                    }
                    CmdRequest::BtrfsFilesystemResize { .. } => {
                        Some(Ok(mock_ok("btrfs filesystem resize", "")))
                    }
                    _ => None,
                }
            });

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            cmd_replace(&runner, &fs, &f.replace_params().build())
                .expect("inactive close skip must not fail replace");
        });

        assert!(
            captured.contains(
                "Warning: post-commit close skipped for mapper braid-disk2: \
                 probe failed (mapper is inactive); expected LUKS UUID \
                 22222222-2222-2222-2222-222222222222\n"
            ),
            "inactive old-mapper close must warn: {captured:?}"
        );
        assert!(
            !runner.requests().iter().any(|request| {
                matches!(request, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-disk2")
            }),
            "inactive old-mapper probe must skip close"
        );
    }

    // Intent: live replace warns and SKIPS the old-mapper close when the post-commit
    //   UUID probe finds braid-disk2's mapper now backs a FOREIGN LUKS volume
    //   (operator double-drift: a different disk opened under the same mapper name
    //   between plan and the post-commit close).
    // Why it exists: the MapperOwnership::Unverified arm in ReplacePlan::execute is
    //   the guard against tearing down a foreign disk's dm slot after a swap-in-place.
    //   Until now only the probe helper was unit-tested, so the arm could regress to
    //   `Unverified => { close it }` with every test still green. remove and recover
    //   already pin this at execute level
    //   (post_commit_close_uuid_probe_demotes_to_skip_on_mismatch,
    //   recover_replace_old_close_foreign_mapper_warns_and_skips); this brings live
    //   replace to parity.
    // Scenario: live replace of disk2 -> disk3 commits; afterwards
    //   `cryptsetup status braid-disk2` resolves to a foreign backing /dev/vdf whose
    //   LUKS UUID is U_FOREIGN != the journaled 2222...2222.
    #[test]
    fn live_replace_old_close_foreign_mapper_warns_and_skips_close() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                move |req| match req {
                    CmdRequest::BtrfsReplaceStart { .. } => {
                        replace_done.store(true, Ordering::Relaxed);
                        Some(Ok(mock_ok("btrfs replace start", "")))
                    }
                    // Post-commit: braid-disk2's mapper now backs a foreign disk.
                    CmdRequest::CryptsetupStatus { mapper }
                        if mapper.as_str() == "braid-disk2"
                            && replace_done.load(Ordering::Relaxed) =>
                    {
                        Some(Ok(mock_ok(
                            "cryptsetup status braid-disk2",
                            "braid-disk2 is active and is in use.\n  type:    LUKS2\n  \
                             device:  /dev/vdf\n  mode:    read/write\n",
                        )))
                    }
                    CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vdf" => {
                        Some(Ok(mock_ok(
                            "cryptsetup luksUUID /dev/vdf",
                            "99999999-9999-9999-9999-999999999999\n",
                        )))
                    }
                    // A regressed arm would issue this; answer it so the regression
                    // fails on the assertion below, not on a dispatch error.
                    CmdRequest::CryptsetupClose { .. } => Some(Ok(mock_ok("cryptsetup close", ""))),
                    CmdRequest::BtrfsFilesystemResize { .. } => {
                        Some(Ok(mock_ok("btrfs filesystem resize", "")))
                    }
                    _ => None,
                }
            });

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            cmd_replace(&runner, &fs, &f.replace_params().build())
                .expect("foreign old-mapper close skip must not fail replace");
        });

        let requests = runner.requests();
        // Core invariant: the foreign mapper is never closed.
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-disk2"
            )),
            "foreign old-mapper probe must skip close: {requests:?}"
        );
        // The post-commit probe actually ran against the foreign backing.
        let foreign_probes = requests
            .iter()
            .filter(
                |r| matches!(r, CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vdf"),
            )
            .count();
        assert_eq!(
            foreign_probes, 1,
            "exactly one post-commit UUID probe against braid-disk2's foreign backing"
        );
        // Execution continues past the skip -- maintenance still replays.
        assert!(
            requests.iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsFilesystemResize { devid, .. } if *devid == Devid::new(2)
            )),
            "resize must still replay after the foreign close skip: {requests:?}"
        );
        // Operator-facing warning names both UUIDs (emitted inside the probe helper).
        assert!(
            captured.contains(
                "Warning: post-commit close skipped for mapper braid-disk2: \
                 expected LUKS UUID 22222222-2222-2222-2222-222222222222 \
                 but observed 99999999-9999-9999-9999-999999999999\n"
            ),
            "foreign close skip must warn with both UUIDs: {captured:?}"
        );
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
    //   must fire at the "reversible preflight before inhibitor" boundary.
    fn cmd_replace_missing_path_rejects_old_name_absent_from_membership() {
        // pool.json records only disk1 -- the typo scenario where btrfs
        // reports devid 2 missing but the operator's --old name does not
        // match any pool.json row. The btrfs side still presents disk1 +
        // missing-devid-2 via the canonical one-live-one-missing fixture.
        let f = PoolFixture::one_live_only();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner =
            ReplacementPool::one_live_one_missing().install(MockRunner::default(), replace_done);
        let result = cmd_replace(
            &runner,
            &fs,
            &f.replace_params()
                .old("disk2")
                .new_disk("disk3=/dev/disk/by-id/virtio-disk3")
                .missing_id(Some(Devid::new(2)))
                .build(),
        );

        assert!(
            matches!(result, Err(ReplaceError::OldMemberNotFound { .. })),
            "expected Err(ReplaceError::OldMemberNotFound) for --old absent from pool.json, got: {result:?}"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "validation must fire before the inhibitor seam -- a caught typo must not hold logind"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
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
            mapper: MapperName::from_basename("braid-disk2".into()),
            devid: Devid::new(2),
        };
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint::new("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .render_steps();
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
            mapper: MapperName::from_basename("braid-disk2".into()),
            devid: Devid::new(2),
        };
        let kf = Path::new("/mnt/usb/braid.key");
        let luks_format_extra_opts = vec![
            "--pbkdf".to_owned(),
            "pbkdf2".to_owned(),
            "--iter-time".to_owned(),
            "1".to_owned(),
        ];
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint::new("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: Some(kf),
            luks_format_extra_opts: &luks_format_extra_opts,
        })
        .render_steps();
        let output = Step::render_dry_run(&steps);

        // Header backup runs after the final keyslot mutation so the backup
        // captures slot 1; ordering invariant is format < addKey < backup < open.
        assert_eq!(
            steps.len(),
            7,
            "fresh live replace must emit format + enroll + backup + open + replace + close + resize; got {:?}",
            steps
        );
        assert_lines_in_order(
            &output,
            &[
                "[destructive]",
                "$ cryptsetup luksFormat",
                "enroll keyfile",
                "$ cryptsetup luksAddKey",
                "LUKS header backup",
                "$ cryptsetup luksHeaderBackup",
                "LUKS open",
                "$ cryptsetup open --type luks",
                "[long]",
                "$ btrfs replace start",
                "cryptsetup close",
                "$ cryptsetup close braid-disk2",
                "btrfs filesystem resize",
            ],
        );

        let format_line = output
            .lines()
            .nth(line_index(&output, "$ cryptsetup luksFormat"))
            .expect("format line present");
        assert!(format_line.contains("--pbkdf pbkdf2 --iter-time 1"));
        assert!(format_line.contains("--label braid-disk3"));

        // Keyfile enrollment (runs before backup so slot 1 lands in the backup).
        // Pin BOTH stringly fields with distinct keyfile and header paths so a
        // transposition at the render boundary fails here.
        let lines: Vec<&str> = output.lines().collect();
        let addkey = line_index(&output, "$ cryptsetup luksAddKey");
        let backup = line_index(&output, "$ cryptsetup luksHeaderBackup");
        assert!(
            lines[addkey].contains("/mnt/usb/braid.key")
                && !lines[addkey].contains("braid-disk3.luksheader"),
            "luksAddKey must carry the keyfile, not the header path; got: {}",
            lines[addkey]
        );

        // Header backup
        assert!(
            lines[backup].contains("braid-disk3.luksheader")
                && !lines[backup].contains("/mnt/usb/braid.key"),
            "luksHeaderBackup must carry the header path, not the keyfile; got: {}",
            lines[backup]
        );

        // Close old mapper (before resize: a resize failure must not strand
        // the old dm slot)
        assert_exact_lines_in_order(&output, &["$ cryptsetup close braid-disk2"]);
    }

    #[test]
    // Intent: two consecutive dry-run renders of the same fresh-disk `replace`
    //   are byte-identical, and the format line shows the fixed
    //   `<generated-at-format-time>` placeholder, not a per-invocation random
    //   UUID.
    // Why it exists: a fresh (PresentNotLuks) replace target derives a random
    //   `new_uuid` via `LuksUuid::new_v4()` at plan time (ADR-024). Before the
    //   preview-variant fix that real UUID flowed into the rendered `--uuid`, so
    //   two dry-runs of the identical command printed different output. A single
    //   StatePaths fixes the header-backup path so the minted UUID is the only
    //   variable. Fails pre-fix, passes post-fix.
    // Scenario: an operator runs `braid replace --dry-run` twice against the
    //   same fresh disk and expects identical, honest preview output.
    fn dry_run_render_fresh_replace_uuid_is_reproducible_across_invocations() {
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Live {
            mapper: MapperName::from_basename("braid-disk2".into()),
            devid: Devid::new(2),
        };
        let new_by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap();
        let mount_point = MountPoint::new("/mnt/storage".into());
        let extra_opts: Vec<String> = Vec::new();
        // Bind ONE StatePaths so the header-backup path is fixed across both
        // builder calls; the minted `new_uuid` is then the only variable.
        let (_tmp, paths) = test_paths();

        let input = ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &new_by_id,
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &mount_point,
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &paths,
            enroll_key_file: None,
            luks_format_extra_opts: &extra_opts,
        };

        // Each call mints a fresh `LuksUuid::new_v4()` internally.
        let first = Step::render_dry_run(&replace_work_plan_for_test(&input).render_steps());
        let second = Step::render_dry_run(&replace_work_plan_for_test(&input).render_steps());

        assert_eq!(
            first, second,
            "two dry-run renders of the same fresh replace must be byte-identical"
        );
        assert!(
            first.contains("<generated-at-format-time>"),
            "fresh-format preview must show the placeholder token, got:\n{first}"
        );
    }

    // Intent: dry-run for an already-LUKS replace target with `--enroll
    //   DIR` shows `cryptsetup luksAddKey` + `cryptsetup
    //   luksHeaderBackup` BEFORE `cryptsetup open`. This is the silent-
    //   drop fix landing in the dry-run preview surface.
    // Why it exists: pre-refactor, `--enroll DIR` against a
    //   `PresentLuks` new disk silently dropped the keyfile -- dry-run
    //   showed only `cryptsetup open` and the new disk shipped without
    //   slot 1 enrolled. After the refactor, the planner routes
    //   `Some(kf) + PresentLuks` through `plan_single_disk_enrollment`,
    //   resolves to `NeedsEnroll` when slot 1 is empty, and renders the
    //   addKey + headerBackup pair. This test pins both the presence
    //   and the order (addKey < headerBackup < open) so the slot-1
    //   mutation lands in the post-mutation backup the way the FreshLuks
    //   ordering pin requires.
    // Scenario: returning braid disk that was originally added without a
    //   keyfile gets re-installed via `replace --enroll DIR` against a
    //   pre-formatted `PresentLuks` new disk with empty slot 1.
    #[test]
    fn dry_run_render_existing_luks_replace_with_enroll_renders_addkey_and_backup() {
        let luks_uuid = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let new_probed = PresentConfigDisk {
            name: DiskName::parse("disk3").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            state: PresentConfigDiskState::PresentLuks {
                uuid: luks_uuid.clone(),
                label: Some("braid-disk3".to_owned()),
                mapper_open: false,
            },
        };
        let source = ReplaceSource::Live {
            mapper: MapperName::from_basename("braid-disk2".into()),
            devid: Devid::new(2),
        };
        let kf = Path::new("/mnt/usb/braid.key");
        let (_tmp, paths) = test_paths();

        let old_uuid = LuksUuid::parse("99999999-9999-9999-9999-999999999999").unwrap();
        // ExistingLuks: new_uuid carries the probed value from new_probed.
        let new_uuid_for_test = luks_uuid.clone();
        let work_plan = build_replace_work_plan(ReplaceWorkPlanInput {
            config: Config::new(MountPoint::new("/mnt/storage".into())).unwrap(),
            old_uuid: old_uuid.clone(),
            old_name: DiskName::parse("disk2").unwrap(),
            new_uuid: new_uuid_for_test,
            new_name: DiskName::parse("disk3").unwrap(),
            new_by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed,
            replace_source: source,
            pool: replace_work_plan_test_pool(
                &ReplaceSource::Live {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                    devid: Devid::new(2),
                },
                &old_uuid,
                false,
                2,
            ),
            paths: &paths,
            // Resolved value as `plan_replace` would compute it for a
            // `NeedsEnroll` outcome: the planner would have run
            // `plan_single_disk_enrollment` and the slot-1 check
            // returned `Empty`, so the keyfile path flows in as `Some`.
            enroll_key_file: Some(KeyFilePath::new(kf.to_path_buf())),
            luks_format_extra_opts: LuksFormatExtraOpts::default(),
            // Render-only test: render_steps() never reads the credential
            // targets, so they need not be resolved here.
            member_verify_targets: Vec::new(),
        });

        let steps = work_plan.render_steps();
        let output = Step::render_dry_run(&steps);
        assert_lines_in_order(
            &output,
            &[
                "$ cryptsetup luksAddKey",
                "$ cryptsetup luksHeaderBackup",
                "$ cryptsetup open --type luks",
            ],
        );
        let p_addkey = line_index(&output, "$ cryptsetup luksAddKey");
        let p_backup = line_index(&output, "$ cryptsetup luksHeaderBackup");
        let lines: Vec<&str> = output.lines().collect();
        // Pin BOTH stringly fields with distinct keyfile and header paths so a
        // transposition at the ExistingLuks render boundary fails here.
        assert!(
            lines[p_addkey].contains("/mnt/usb/braid.key")
                && !lines[p_addkey].contains("braid-disk3.luksheader"),
            "addKey command must carry the keyfile, not the header path: {}",
            lines[p_addkey]
        );
        assert!(
            lines[p_backup].contains("braid-disk3.luksheader")
                && !lines[p_backup].contains("/mnt/usb/braid.key"),
            "headerBackup command must carry the header path, not the keyfile: {}",
            lines[p_backup]
        );
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
        let source = ReplaceSource::Missing {
            devid: Devid::new(2),
        };
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint::new("/mnt/storage".into()),
            will_clear_last_missing: true,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .render_steps();
        let output = Step::render_dry_run(&steps);

        // Substring order: LUKS format -> header backup -> LUKS open ->
        // btrfs replace start -> btrfs filesystem resize -> soft balance.
        assert_lines_in_order(
            &output,
            &[
                "$ cryptsetup luksFormat",
                "$ cryptsetup luksHeaderBackup",
                "$ cryptsetup open --type luks",
                "$ btrfs replace start",
                "btrfs filesystem resize",
                "-dconvert=raid1,soft",
            ],
        );

        // Missing path has no old mapper, so no cryptsetup close anywhere.
        assert!(
            !output.contains("cryptsetup close"),
            "missing path must not render a cryptsetup close step:\n{output}"
        );
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
        let f = PoolFixture::two_disk_healthy();
        // Only the new disk's by_id exists. /dev/mapper/braid-disk3 is
        // absent because the mapper is closed (with_mapper_closed below).
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .with_mapper_closed("braid-disk3")
            .install(MockRunner::default(), replace_done)
            .with_handler(|req| match req {
                CmdRequest::CryptsetupTestPassphrase { device }
                    if device == "/dev/disk/by-id/virtio-disk3" =>
                {
                    Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksOpen --test-passphrase {device}"),
                        stdout: String::new(),
                        stderr: "No key available with this passphrase.\n".into(),
                        exit_status: 2,
                    }))
                }
                _ => None,
            });
        let result = cmd_replace(&runner, &fs, &f.replace_params().build());
        let rendered = result
            .as_ref()
            .expect_err("wrong passphrase must fail")
            .to_string();

        assert!(
            matches!(result, Err(ReplaceError::Validation(_))),
            "expected Err(ReplaceError::Validation(_)) for wrong passphrase on a closed-LUKS new disk, got: {result:?}"
        );
        assert!(
            !rendered.contains("braid recover"),
            "pre-journal refusal must not direct the operator to recovery: {rendered}"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "pending-op.json must not be written -- wrong passphrase is a reversible preflight failure"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "sleep inhibitor must not be acquired before passphrase verification"
        );
    }

    // Intent: under mapper drift (a retained member open as braid-WRONG), the
    //   execute-time passphrase rejection names the member resolved through
    //   membership ('disk1'), not the drifted mapper basename.
    // Why it exists: the retained-member credential targets are resolved at
    //   plan time precisely so this message survives drift (decision 024). An
    //   execute-level assertion is required: a plan-builder check on
    //   member_verify_targets[0].name would pass even if execute re-derived a
    //   mapper-based name.
    // Scenario: replacing old disk2 while the retained member disk1 is open
    //   under a stale 'braid-WRONG' mapper and the typed passphrase is wrong.
    #[test]
    fn replace_execute_passphrase_rejection_names_drifted_member_via_membership() {
        let f = PoolFixture::two_disk_healthy();
        let old_uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let drifted_uuid = LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap();
        let pool = PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName::from_basename("braid-WRONG".into()),
                    luks_uuid: drifted_uuid.clone(),
                    devid: Devid::new(1),
                    underlying: "/dev/vdb".into(),
                },
                PoolDevice {
                    mapper: MapperName::from_basename("braid-disk2".into()),
                    luks_uuid: old_uuid.clone(),
                    devid: Devid::new(2),
                    underlying: "/dev/vdc".into(),
                },
            ],
            missing_count: 0,
            total_devices: 2,
            fsid: Some(Fsid::parse(REPLACE_TEST_FSID).unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        };
        let mut membership = PoolMembership::empty();
        membership
            .insert(
                drifted_uuid,
                membership::DiskMember {
                    name: disk_name("disk1"),
                    by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
                    devid: Some(Devid::new(1)),
                    added_at: None,
                },
            )
            .unwrap();
        let replace_source = ReplaceSource::Live {
            mapper: MapperName::from_basename("braid-disk2".into()),
            devid: Devid::new(2),
        };
        let member_verify_targets =
            build_member_verify_targets(&membership, &pool, &replace_source, &old_uuid);
        let new_name = disk_name("disk3");
        let new_by_id = ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap();
        let work_plan = build_replace_work_plan(ReplaceWorkPlanInput {
            config: f.config.clone(),
            old_uuid,
            old_name: disk_name("disk2"),
            new_uuid: LuksUuid::new_v4(),
            new_name: new_name.clone(),
            new_by_id: new_by_id.clone(),
            new_probed: PresentConfigDisk {
                name: new_name,
                by_id_path: new_by_id,
                state: PresentConfigDiskState::PresentNotLuks,
            },
            replace_source,
            pool,
            paths: &f.paths,
            enroll_key_file: None,
            luks_format_extra_opts: LuksFormatExtraOpts::parse(&[]).unwrap(),
            member_verify_targets,
        });
        let plan = ReplacePlan {
            notes: vec![],
            work_plan,
        };
        // Reject the retained (drifted) member's passphrase; it is the first
        // credential target, so execute aborts here before any mutation.
        let runner = MockRunner::default().with_handler(|req| match req {
            CmdRequest::CryptsetupTestPassphrase { device } if device == "/dev/vdb" => {
                Some(Ok(RawCommandOutput {
                    cmd: format!("cryptsetup open --test-passphrase {device}"),
                    stdout: String::new(),
                    stderr: "No key available with this passphrase.\n".into(),
                    exit_status: 2,
                }))
            }
            _ => None,
        });
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);

        let err = plan
            .execute(&runner, &fs, &f.replace_params().yes(true).build())
            .expect_err("wrong passphrase on the retained member must abort");

        match err {
            ReplaceError::Validation(msg) => {
                assert!(
                    msg.contains("passphrase does not match existing pool member 'disk1'"),
                    "drifted member must resolve to 'disk1' via membership, got: {msg}"
                );
                assert!(
                    !msg.contains("WRONG"),
                    "must not surface the drifted mapper basename, got: {msg}"
                );
            }
            other => panic!("expected Err(ReplaceError::Validation), got: {other:?}"),
        }
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "credential rejection must precede the journal write"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "credential rejection must precede the sleep inhibitor"
        );
    }

    #[test]
    // Intent: when the new disk is PresentLuks { mapper_open: true },
    //   cmd_replace verifies its slot 0 but must not issue a second
    //   CryptsetupLuksOpen against that disk's by_id.
    //
    // Why it exists: already-open LUKS candidates enter post-operation
    //   membership, so their slot 0 must be verified before the journal.
    //   The already-open path still must not run ensure_luks_open and bind
    //   the mapper a second time.
    //
    // Scenario: a previous replace/add opened /dev/mapper/braid-disk3 but
    //   never added it to the pool (e.g. crash). Operator retries
    //   `braid replace --old disk2 --new disk3=...`; the command picks up
    //   the already-open mapper and proceeds to btrfs replace start without
    //   a second LUKS interaction on the new disk.
    fn mapper_open_true_verifies_but_does_not_open_new_disk_luks() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(replace_start_fails_handler());
        let result = cmd_replace(&runner, &fs, &f.replace_params().build());

        // The handler forces BtrfsReplaceStart to fail (exit 1), so
        // cmd_replace must return a Pool error -- this confirms the flow
        // reached the btrfs phase rather than stopping short, which is a
        // prerequisite for the zero-counts below to mean "not called"
        // instead of "test aborted early".
        assert!(
            matches!(
                result,
                Err(ReplaceError::JournalLifecycle { ref source, .. })
                    if matches!(source.as_ref(), ReplaceError::Pool(_))
            ),
            "expected Err(ReplaceError::Pool(_)) from btrfs replace start failure, got: {result:?}"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "journal must be written -- the failure is post-journal"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the way in"
        );

        let log = runner.requests();
        let new_by_id = "/dev/disk/by-id/virtio-disk3";

        let test_passphrase_calls = log
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupTestPassphrase { device } if device == new_by_id))
            .count();
        assert_eq!(
            test_passphrase_calls, 1,
            "mapper_open: true must verify CryptsetupTestPassphrase on the new disk exactly once"
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

    #[test]
    // Intent: a drifted live-pool row with the replacement mapper name
    //   must not skip the already-open replacement mapper verifier, and the
    //   verifier's PRE-journal tier must catch the drift before write_journal.
    // Why it exists: mapper names are runtime handles, not membership
    //   identity; a mapper-keyed skip would let a foreign open mapper reach
    //   `btrfs replace start` when the live row's UUID differs from the
    //   planned replacement UUID. The open-arm identity check now runs as the
    //   pre-journal tier (verify_existing_luks_new_target_preflight), so a
    //   single post-confirmation backing drift aborts on the reversible side
    //   (no stranded pending-op.json, no inhibitor) per ADR 019's pre-journal
    //   excluded scope -- this is the open-arm analog of
    //   cmd_replace_existing_luks_closed_mapper_open_boundary_swap_aborts. The
    //   residual post-journal verify still guards the keyfile-enroll window and
    //   is covered by the --enroll post-journal-tier test below.
    // Scenario: planning observes a pool row named `braid-disk3` with a
    //   foreign LUKS UUID, then the replacement disk plans as the correct
    //   already-open `braid-disk3`. Before execute reaches the journal, the
    //   open mapper's backing UUID drifts again; the pre-journal verifier
    //   catches it.
    fn mapper_name_drift_does_not_skip_open_mapper_verifier() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let disk3_backing_uuid_calls = Arc::new(AtomicU32::new(0));
        let new_uuid = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let drifted_pool_uuid = LuksUuid::parse("44444444-4444-4444-4444-444444440001").unwrap();
        let execute_uuid = LuksUuid::parse("55555555-5555-5555-5555-555555550001").unwrap();
        let new_uuid_for_handler = new_uuid.clone();
        let drifted_pool_uuid_for_handler = drifted_pool_uuid.clone();
        let execute_uuid_for_handler = execute_uuid.clone();
        let drifted_pre_show = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
             \tTotal devices 3 FS bytes used 16.17MiB\n\
             \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\
             \tdevid    3 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n";
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                let disk3_backing_uuid_calls = disk3_backing_uuid_calls.clone();
                move |req| match req {
                    CmdRequest::BtrfsFilesystemShow { mount_point }
                        if !replace_done.load(Ordering::Relaxed) =>
                    {
                        Some(Ok(mock_ok(
                            &format!("btrfs filesystem show {mount_point}"),
                            drifted_pre_show,
                        )))
                    }
                    CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vdd" => {
                        let call = disk3_backing_uuid_calls.fetch_add(1, Ordering::SeqCst);
                        let observed = match call {
                            0 => &drifted_pool_uuid_for_handler,
                            1 => &new_uuid_for_handler,
                            _ => &execute_uuid_for_handler,
                        };
                        Some(Ok(mock_ok(
                            &format!("cryptsetup luksUUID {device}"),
                            &format!("{observed}\n"),
                        )))
                    }
                    _ => None,
                }
            });

        let result = cmd_replace(&runner, &fs, &f.replace_params().build());

        match result {
            Err(ReplaceError::NewTargetUuidMismatchAtOpen {
                by_id,
                expected,
                observed,
            }) => {
                assert_eq!(by_id.as_str(), "/dev/disk/by-id/virtio-disk3");
                assert_eq!(expected, new_uuid);
                assert_eq!(observed, execute_uuid.as_str());
            }
            other => panic!("expected NewTargetUuidMismatchAtOpen, got: {other:?}"),
        }
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "no BtrfsReplaceStart may issue when the open mapper verifier fails"
        );
        // Pre-journal-tier proof: the backing drift is caught before the journal
        // and before the inhibitor, so a single swap is a reversible-side abort.
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "pending-op.json must not be written -- the open-arm drift is caught pre-journal"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "sleep inhibitor must not be acquired -- the pre-journal identity gate aborts before the inhibitor"
        );
    }

    #[test]
    // Intent: with `--enroll`, the already-open ExistingLuks path passes the
    //   PRE-journal identity gate, enrolls the slot-1 keyfile, then catches a
    //   backing drift at the residual POST-journal verify -- aborting before
    //   BtrfsReplaceStart, with the journal already written.
    //
    // Why it exists: the open arm's counterpart to
    //   cmd_replace_existing_luks_closed_mapper_enroll_post_journal_swap_aborts.
    //   The pre-journal tier (verify_existing_luks_new_target_preflight) now
    //   intercepts the single-drift scenario before the journal, so deleting the
    //   post-journal verify (verify_existing_luks_open_mapper_target) in Step 1
    //   would no longer fail any single-drift full-execute test. This makes the
    //   pre-journal verify PASS and drifts the mapper backing UUID only AFTER the
    //   slot-1 `cryptsetup luksAddKey`, keeping that post-journal verify
    //   load-bearing against the journal->open keyfile-enroll window.
    //
    // Scenario: operator runs `braid replace --old disk2 --new disk3=<by-id>
    //   --enroll DIR` against an already-LUKS, already-OPEN disk3 with slot 1
    //   empty (NeedsEnroll). The pre-journal verify reads the mapper backing as
    //   U_NEW (pass); the journal is written; the keyfile is enrolled; THEN the
    //   open mapper's backing UUID drifts to U_FOREIGN, so the post-journal
    //   verify aborts before the pool mutation.
    fn cmd_replace_existing_luks_open_mapper_enroll_post_journal_drift_aborts() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));

        let kf_dir = tempfile::tempdir().unwrap();
        let kf_path = kf_dir.path().join("braid.key");
        std::fs::write(&kf_path, [0u8; crate::luks::KEYFILE_SIZE]).unwrap();

        let u_new = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let u_foreign = LuksUuid::parse("44444444-4444-4444-4444-444444440769").unwrap();
        let u_new_h = u_new.clone();
        let u_foreign_h = u_foreign.clone();
        // Phase flag (see the closed-arm sibling): the open-arm verify reads the
        // mapper backing (/dev/vdd). Planning identifies the disk by its by-id
        // (virtio-disk3, canonical 33 via the install handler); only the backing
        // reads route through this gate. The post-journal verify is the only
        // backing read after the AddKey, so gate /dev/vdd on "enroll seen".
        let enrolled = Arc::new(AtomicBool::new(false));

        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler({
                let enrolled = enrolled.clone();
                move |req| match req {
                    // Keyfile not yet enrolled -> NeedsEnroll at planning.
                    CmdRequest::CryptsetupTestKeyFile { device, .. }
                        if device == "/dev/disk/by-id/virtio-disk3" =>
                    {
                        Some(Ok(RawCommandOutput {
                            cmd: "cryptsetup open --test-passphrase --key-file".into(),
                            stdout: String::new(),
                            stderr: "No key available with this passphrase.\n".into(),
                            exit_status: 2,
                        }))
                    }
                    CmdRequest::CryptsetupLuksAddKeyFile { device, .. }
                        if device == "/dev/disk/by-id/virtio-disk3" =>
                    {
                        enrolled.store(true, Ordering::SeqCst);
                        Some(Ok(mock_ok(&format!("cryptsetup luksAddKey {device}"), "")))
                    }
                    CmdRequest::CryptsetupLuksHeaderBackup { device, .. } => Some(Ok(mock_ok(
                        &format!("cryptsetup luksHeaderBackup {device}"),
                        "",
                    ))),
                    CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vdd" => {
                        let uuid = if enrolled.load(Ordering::SeqCst) {
                            &u_foreign_h
                        } else {
                            &u_new_h
                        };
                        Some(Ok(mock_ok(
                            &format!("cryptsetup luksUUID {device}"),
                            &format!("{uuid}\n"),
                        )))
                    }
                    _ => None,
                }
            });

        let result = cmd_replace(
            &runner,
            &fs,
            &f.replace_params()
                .enroll_key_file(Some(kf_path.as_path()))
                .build(),
        );

        match result {
            Err(ReplaceError::JournalLifecycle { source, advice }) => match *source {
                ReplaceError::NewTargetUuidMismatchAtOpen {
                    by_id,
                    expected,
                    observed,
                } => {
                    assert_eq!(by_id.as_str(), "/dev/disk/by-id/virtio-disk3");
                    assert_eq!(expected, u_new);
                    assert_eq!(observed, u_foreign.as_str());
                    assert!(advice.contains("run `braid recover`"));
                }
                other => panic!("expected UUID mismatch source, got: {other:?}"),
            },
            other => panic!("expected NewTargetUuidMismatchAtOpen, got: {other:?}"),
        }
        let log = runner.requests();
        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "journal must be written -- the post-journal verify fires after write_journal"
        );
        assert!(
            log.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksAddKeyFile { device, .. }
                    if device == "/dev/disk/by-id/virtio-disk3"
            )),
            "slot-1 keyfile enroll must have run -- it is the window the post-journal verify guards"
        );
        assert!(
            !log.iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "no BtrfsReplaceStart may issue -- no pool data routed onto the drifted disk"
        );
    }

    #[test]
    // Intent: cmd_replace, on the ExistingLuks closed-mapper path, re-probes the
    //   new target's by-id LUKS UUID at the PRE-journal new-target identity gate
    //   and aborts with NewTargetUuidMismatchAtOpen when it no longer matches the
    //   UUID captured at planning -- before write_journal, before the sleep
    //   inhibitor, and before any CryptsetupLuksOpen or BtrfsReplaceStart.
    //
    // Why it exists: the closed-mapper identity check is the ONLY guard on this
    //   path -- ensure_luks_open blindly opens whatever sits at the by-id
    //   (classify_mapper_ownership returns Inactive without checking UUID for a
    //   closed mapper), and verify_replace_execute_live_pool_uuid only rejects the
    //   planned UUID as a live-pool duplicate. A single post-confirmation disk
    //   swap is a reversible preflight-class failure, so it must abort on the
    //   reversible side per ADR 019's pre-journal excluded scope and
    //   principles.md's "line of no return" -- not strand pending-op.json and
    //   force the operator into `braid recover`. The pre-journal tier hoists this
    //   probe (verify_existing_luks_new_target_preflight) above the journal; the
    //   tight post-journal re-probe still runs as the pre-open TOCTOU guard for
    //   the keyfile-enroll window and is covered by the --enroll post-journal-tier
    //   test below. The mapper_open=true arm has the matching pre-journal wiring
    //   test (mapper_name_drift_does_not_skip_open_mapper_verifier).
    //
    // Scenario: operator runs `braid replace --old disk2 --new disk3=<by-id>` where
    //   disk3 is already LUKS-formatted with its mapper closed. Between planning
    //   (UUID = U_NEW) and the execute-time pre-journal probe, the by-id slot is
    //   swapped to a foreign LUKS volume (UUID = U_FOREIGN, no pool-member
    //   collision). The command must abort on the reversible side: no journal, no
    //   inhibitor, no LUKS open, no btrfs replace start.
    fn cmd_replace_existing_luks_closed_mapper_open_boundary_swap_aborts() {
        let f = PoolFixture::two_disk_healthy();
        // Mapper closed -> only the by-id exists, not /dev/mapper/braid-disk3.
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let replace_done = Arc::new(AtomicBool::new(false));

        let u_new = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let u_foreign = LuksUuid::parse("44444444-4444-4444-4444-444444440746").unwrap();
        let u_new_h = u_new.clone();
        let u_foreign_h = u_foreign.clone();

        let runner = ReplacementPool::two_disk_healthy()
            .with_mapper_closed("braid-disk3")
            .install(MockRunner::default(), replace_done)
            .with_handler({
                let calls = Arc::new(AtomicU32::new(0));
                move |req| match req {
                    CmdRequest::CryptsetupLuksUuid { device }
                        if device == "/dev/disk/by-id/virtio-disk3" =>
                    {
                        let n = calls.fetch_add(1, Ordering::SeqCst);
                        let uuid = if n == 0 { &u_new_h } else { &u_foreign_h };
                        Some(Ok(mock_ok(
                            &format!("cryptsetup luksUUID {device}"),
                            &format!("{uuid}\n"),
                        )))
                    }
                    _ => None,
                }
            });

        let result = cmd_replace(&runner, &fs, &f.replace_params().build());

        // Core safety assertions (each independently fails if the probe is removed):
        match result {
            Err(ReplaceError::NewTargetUuidMismatchAtOpen {
                by_id,
                expected,
                observed,
            }) => {
                assert_eq!(by_id.as_str(), "/dev/disk/by-id/virtio-disk3");
                assert_eq!(expected, u_new);
                assert_eq!(observed, u_foreign.as_str());
            }
            other => panic!("expected NewTargetUuidMismatchAtOpen, got: {other:?}"),
        }
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksOpen { .. })),
            "no CryptsetupLuksOpen may issue on the swap-abort path"
        );
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "no BtrfsReplaceStart may issue on the swap-abort path"
        );

        // Boundary-pinning assertions: prove the abort is the PRE-journal
        // new-target identity gate, not the residual post-journal re-probe. The
        // pre-journal tier fires before write_journal and before the inhibitor,
        // so a single swap leaves no stranded pending-op.json -- matching the
        // pre-journal wrong-passphrase abort (journal None + acquire_count 0) and
        // distinct from the --enroll post-journal-tier catch (journal Some).
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "pending-op.json must not be written -- a single swap is caught on the reversible side, before the journal"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "sleep inhibitor must not be acquired -- the pre-journal identity gate aborts before the inhibitor"
        );
    }

    #[test]
    // Intent: with `--enroll`, the closed-mapper ExistingLuks path passes the
    //   PRE-journal identity gate, enrolls the slot-1 keyfile, then catches a
    //   swap at the residual POST-journal re-probe -- aborting before
    //   ensure_luks_open and BtrfsReplaceStart, with the journal already written.
    //
    // Why it exists: the pre-journal tier (verify_existing_luks_new_target_preflight)
    //   now intercepts the single-swap scenario before the journal, so deleting
    //   the post-journal probe in Step 1 (probe_existing_luks_new_target_uuid)
    //   would no longer fail any single-swap test. This is the only full-execute
    //   test that keeps that post-journal probe load-bearing: it makes the
    //   pre-journal probe PASS and drifts the by-id UUID only AFTER the slot-1
    //   `cryptsetup luksAddKey`, which sits between the two tiers. Guarding that
    //   journal->open keyfile-enroll window is the documented purpose of the
    //   two-tier design (ADR 019 / principles.md).
    //
    // Scenario: operator runs `braid replace --old disk2 --new disk3=<by-id>
    //   --enroll DIR` against an already-LUKS disk3 with mapper closed and slot 1
    //   empty (NeedsEnroll). The pre-journal probe sees U_NEW (pass); the journal
    //   is written; the keyfile is enrolled into slot 1; THEN the by-id slot is
    //   swapped to a foreign LUKS volume, so the post-journal re-probe sees
    //   U_FOREIGN and aborts before the disk is opened.
    fn cmd_replace_existing_luks_closed_mapper_enroll_post_journal_swap_aborts() {
        let f = PoolFixture::two_disk_healthy();
        // Mapper closed -> only the by-id exists, not /dev/mapper/braid-disk3.
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let replace_done = Arc::new(AtomicBool::new(false));

        let kf_dir = tempfile::tempdir().unwrap();
        let kf_path = kf_dir.path().join("braid.key");
        std::fs::write(&kf_path, [0u8; crate::luks::KEYFILE_SIZE]).unwrap();

        let u_new = LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap();
        let u_foreign = LuksUuid::parse("44444444-4444-4444-4444-444444440746").unwrap();
        let u_new_h = u_new.clone();
        let u_foreign_h = u_foreign.clone();
        // Phase flag, not a raw call counter: the post-journal probe is the only
        // by-id luksUUID read that happens AFTER the slot-1 enroll. Gating the
        // foreign UUID on "enroll seen" makes planning + the pre-journal probe
        // (both before the AddKey) observe U_NEW and only the post-journal
        // re-probe observe U_FOREIGN -- robust to the exact count of pre-enroll
        // probes (the index brittleness the plan warns about).
        let enrolled = Arc::new(AtomicBool::new(false));

        let runner = ReplacementPool::two_disk_healthy()
            .with_mapper_closed("braid-disk3")
            .install(MockRunner::default(), replace_done)
            .with_handler({
                let enrolled = enrolled.clone();
                move |req| match req {
                    // Keyfile not yet enrolled -> NeedsEnroll at planning.
                    CmdRequest::CryptsetupTestKeyFile { device, .. }
                        if device == "/dev/disk/by-id/virtio-disk3" =>
                    {
                        Some(Ok(RawCommandOutput {
                            cmd: "cryptsetup open --test-passphrase --key-file".into(),
                            stdout: String::new(),
                            stderr: "No key available with this passphrase.\n".into(),
                            exit_status: 2,
                        }))
                    }
                    CmdRequest::CryptsetupLuksAddKeyFile { device, .. }
                        if device == "/dev/disk/by-id/virtio-disk3" =>
                    {
                        enrolled.store(true, Ordering::SeqCst);
                        Some(Ok(mock_ok(&format!("cryptsetup luksAddKey {device}"), "")))
                    }
                    CmdRequest::CryptsetupLuksHeaderBackup { device, .. } => Some(Ok(mock_ok(
                        &format!("cryptsetup luksHeaderBackup {device}"),
                        "",
                    ))),
                    CmdRequest::CryptsetupLuksUuid { device }
                        if device == "/dev/disk/by-id/virtio-disk3" =>
                    {
                        let uuid = if enrolled.load(Ordering::SeqCst) {
                            &u_foreign_h
                        } else {
                            &u_new_h
                        };
                        Some(Ok(mock_ok(
                            &format!("cryptsetup luksUUID {device}"),
                            &format!("{uuid}\n"),
                        )))
                    }
                    _ => None,
                }
            });

        let result = cmd_replace(
            &runner,
            &fs,
            &f.replace_params()
                .enroll_key_file(Some(kf_path.as_path()))
                .build(),
        );

        match result {
            Err(ReplaceError::JournalLifecycle { source, advice }) => match *source {
                ReplaceError::NewTargetUuidMismatchAtOpen {
                    by_id,
                    expected,
                    observed,
                } => {
                    assert_eq!(by_id.as_str(), "/dev/disk/by-id/virtio-disk3");
                    assert_eq!(expected, u_new);
                    assert_eq!(observed, u_foreign.as_str());
                    assert!(advice.contains("run `braid recover`"));
                }
                other => panic!("expected UUID mismatch source, got: {other:?}"),
            },
            other => panic!("expected NewTargetUuidMismatchAtOpen, got: {other:?}"),
        }
        let log = runner.requests();
        // The journal IS written -- the accepted post-journal residual. This is
        // what distinguishes the post-journal tier from the pre-journal abort.
        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "journal must be written -- the post-journal tier fires after write_journal"
        );
        // The enroll window was exercised: the AddKey sits between the two tiers,
        // so the post-journal probe is guarding a real mutation window.
        assert!(
            log.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksAddKeyFile { device, .. }
                    if device == "/dev/disk/by-id/virtio-disk3"
            )),
            "slot-1 keyfile enroll must have run -- it is the window the post-journal probe guards"
        );
        assert!(
            !log.iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksOpen { .. })),
            "no CryptsetupLuksOpen may issue -- abort precedes ensure_luks_open"
        );
        assert!(
            !log.iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "no BtrfsReplaceStart may issue -- no pool data routed onto the foreign disk"
        );
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
        let f = PoolFixture::one_live_one_missing();
        // disk3 is already LUKS-open (PresentLuks { mapper_open: true }),
        // so cmd_replace skips LUKS format/open/enroll. That keeps the test
        // focused on the replace+resize+balance sequence.
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::one_live_one_missing()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                move |req| match req {
                    CmdRequest::BtrfsReplaceStart { .. } => {
                        replace_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        Some(Ok(mock_ok("btrfs replace start", "")))
                    }
                    CmdRequest::BtrfsFilesystemResize { .. } => {
                        Some(Ok(mock_ok("btrfs filesystem resize", "")))
                    }
                    CmdRequest::BtrfsBalanceRaid1Soft { .. } => {
                        Some(Ok(mock_ok("btrfs balance raid1 soft", "")))
                    }
                    _ => None,
                }
            });
        let result = cmd_replace(
            &runner,
            &fs,
            &f.replace_params()
                .old("disk2")
                .new_disk("disk3=/dev/disk/by-id/virtio-disk3")
                .missing_id(Some(Devid::new(2)))
                .build(),
        );

        assert!(
            matches!(result, Ok(())),
            "expected Ok(()) from successful missing-path replace, got: {result:?}"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "pending-op.json must be cleared on successful completion"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the way in"
        );

        let log = runner.requests();
        let replace_idx = log
            .iter()
            .position(|r| {
                matches!(r, CmdRequest::BtrfsReplaceStart { devid, .. } if *devid == Devid::new(2))
            })
            .expect("btrfs replace start on devid 2 must be issued");
        let resize_idx = log
            .iter()
            .position(|r| {
                matches!(r, CmdRequest::BtrfsFilesystemResize { devid, .. } if *devid == Devid::new(2))
            })
            .expect("btrfs filesystem resize on devid 2 must be issued");
        let balance_idx = log
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsBalanceRaid1Soft { .. }))
            .expect(
                "btrfs soft balance must be issued after replace+resize on missing path \
                 -- maybe_restore_raid1 is part of the `replace` contract per \
                 docs/design/principles.md",
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

    #[test]
    // Intent: missing-path replace skips the soft-balance runtime step
    //   when the replacement does not clear the last missing device.
    //
    // Why it exists: the shared restore-RAID1 predicate must gate the
    //   runtime maybe_restore_raid1 call, not merely rely on the runtime
    //   re-probe to veto a still-degraded pool.
    //
    // Scenario: pool has disk1 live, devid 2 missing, and devid 4
    //   missing. Replacing devid 2 with already-open disk3 still leaves
    //   devid 4 missing, so resize completes without a post-resize
    //   BtrfsFilesystemShow or soft balance.
    fn cmd_replace_missing_path_skips_soft_balance_when_not_last_missing() {
        let f = PoolFixture::one_live_two_missing();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::one_live_two_missing()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                move |req| match req {
                    CmdRequest::BtrfsReplaceStart { .. } => {
                        replace_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        Some(Ok(mock_ok("btrfs replace start", "")))
                    }
                    CmdRequest::BtrfsFilesystemResize { .. } => {
                        Some(Ok(mock_ok("btrfs filesystem resize", "")))
                    }
                    _ => None,
                }
            });
        let result = cmd_replace(
            &runner,
            &fs,
            &f.replace_params()
                .old("disk2")
                .new_disk("disk3=/dev/disk/by-id/virtio-disk3")
                .missing_id(Some(Devid::new(2)))
                .build(),
        );

        assert!(
            matches!(result, Ok(())),
            "expected Ok(()) from successful multi-missing replace, got: {result:?}"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "pending-op.json must be cleared on successful completion"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the way in"
        );

        let log = runner.requests();
        assert!(
            !log.iter()
                .any(|r| matches!(r, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "soft balance must not run when another missing device remains: {log:?}"
        );
        let resize_idx = log
            .iter()
            .position(|r| {
                matches!(r, CmdRequest::BtrfsFilesystemResize { devid, .. } if *devid == Devid::new(2))
            })
            .expect("btrfs filesystem resize on devid 2 must be issued");
        // The metadata-enrichment probe after BtrfsReplaceStart is
        // unconditional. After resize, the only BtrfsFilesystemShow
        // would be the gated maybe_restore_raid1 re-probe.
        assert!(
            !log.iter().enumerate().any(|(idx, r)| {
                idx > resize_idx && matches!(r, CmdRequest::BtrfsFilesystemShow { .. })
            }),
            "plan gate must suppress post-resize BtrfsFilesystemShow re-probe; log: {log:?}"
        );
    }

    #[test]
    // Intent: missing-path replace persists restore_raid1_after_commit=false
    //   when another missing device remains after the replacement.
    //
    // Why it exists: recover replays the post-replace journal flag, so
    //   the recovery contract must preserve the plan gate decision.
    //
    // Scenario: the same two-missing replacement reaches
    //   PostReplaceMaintenance, then btrfs filesystem resize fails. The
    //   surviving journal must record a Missing source and a closed
    //   restore flag.
    fn cmd_replace_missing_path_not_last_missing_persists_restore_raid1_false() {
        let f = PoolFixture::one_live_two_missing();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::one_live_two_missing()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                move |req| match req {
                    CmdRequest::BtrfsReplaceStart { .. } => {
                        replace_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        Some(Ok(mock_ok("btrfs replace start", "")))
                    }
                    CmdRequest::BtrfsFilesystemResize { .. } => Some(Ok(RawCommandOutput {
                        cmd: "btrfs filesystem resize".into(),
                        stdout: String::new(),
                        stderr: "ERROR: unable to resize".into(),
                        exit_status: 1,
                    })),
                    _ => None,
                }
            });
        let result = cmd_replace(
            &runner,
            &fs,
            &f.replace_params()
                .old("disk2")
                .new_disk("disk3=/dev/disk/by-id/virtio-disk3")
                .missing_id(Some(Devid::new(2)))
                .build(),
        );

        match &result {
            Err(ReplaceError::JournalLifecycle { source, advice }) => match source.as_ref() {
                ReplaceError::Pool(crate::pool::PoolError::Failed(msg)) => {
                    assert!(
                        msg.contains("btrfs filesystem resize failed"),
                        "expected typed PoolError::Failed carrying resize message, got: {msg}"
                    );
                    assert!(advice.contains("run `braid recover`"));
                }
                other => panic!("expected PoolError::Failed source, got: {other:?}"),
            },
            other => {
                panic!("expected Err(ReplaceError::Pool(PoolError::Failed(..))), got: {other:?}")
            }
        }

        let journal = journal::load_journal(&f.paths)
            .unwrap()
            .expect("journal should remain after post-replace resize failure");
        match journal.op {
            journal::OpKind::Replace {
                phase,
                source,
                restore_raid1_after_commit,
                ..
            } => {
                assert_eq!(
                    phase,
                    journal::ReplacePhase::PostReplaceMaintenance,
                    "journal should advance after btrfs replace commits"
                );
                assert_eq!(
                    source,
                    journal::ReplaceJournalSource::Missing {
                        old_devid: Devid::new(2)
                    },
                    "journal source must preserve the missing devid"
                );
                assert!(
                    !restore_raid1_after_commit,
                    "multi-missing replace journal must keep restore flag false"
                );
            }
            other => panic!("expected Replace journal, got: {other:?}"),
        }
    }

    /// Override handler for the keyfile-ordering tests: marks disk3
    /// (the raw replacement) as not-LUKS and answers the LUKS init
    /// chain (Format/AddKeyFile/HeaderBackup) successfully. LuksOpen is
    /// intentionally unmocked so cmd_replace falls through to the
    /// canonical fixture's `None`, then to MissingMock, leaving the
    /// full LUKS request log intact for ordering assertions.
    /// Header-backup file-write happens via MockRunner's
    /// apply_side_effects, not in this handler.
    fn keyfile_ordering_success_handler()
    -> impl Fn(&CmdRequest) -> Option<Result<RawCommandOutput, CmdError>> + Send + Sync + 'static
    {
        |req| match req {
            CmdRequest::CryptsetupLuksUuid { device }
                if device == "/dev/disk/by-id/virtio-disk3" =>
            {
                Some(Ok(RawCommandOutput {
                    cmd: format!("cryptsetup luksUUID {device}"),
                    stdout: String::new(),
                    stderr: "Device is not a valid LUKS device.\n".into(),
                    exit_status: 1,
                }))
            }
            CmdRequest::CryptsetupLuksFormat { device, .. } => {
                Some(Ok(mock_ok(&format!("cryptsetup luksFormat {device}"), "")))
            }
            CmdRequest::CryptsetupLuksAddKeyFile { device, .. } => {
                Some(Ok(mock_ok(&format!("cryptsetup luksAddKey {device}"), "")))
            }
            CmdRequest::CryptsetupLuksHeaderBackup { device, .. } => Some(Ok(mock_ok(
                &format!("cryptsetup luksHeaderBackup {device}"),
                "",
            ))),
            _ => None,
        }
    }

    /*
     * Intent: cmd_replace with `--enroll-key-file` against a fresh
     *   (PresentNotLuks) new disk emits LUKS commands in the order
     *   LuksFormat -> LuksAddKeyFile -> LuksHeaderBackup -> LuksOpen
     *   in the real execute path.
     *
     * Why it exists: `ReplacePlan::execute` consumes `ReplaceWorkPlan`
     *   directly rather than executing rendered `Step`s, so preview
     *   ordering coverage alone does not protect runtime ordering.
     *   Pinning the chain at the real-execute layer also covers the
     *   "no backup before open" guarantee that keeps the no-backup
     *   window narrow if `LuksAddKeyFile` ever fails between
     *   `LuksFormat` and `LuksHeaderBackup`. The dry-run preview path is
     *   pinned by `dry_run_render_fresh_disk_live_replace_with_keyfile`.
     *
     * Scenario: 2-disk pool with disk2 missing. Operator runs
     *   `braid replace --old disk2 --missing-id 2 --new disk3=...
     *   --enroll-key-file=/tmp/braid.key`. The recording runner makes
     *   header backup succeed (so we proceed past it) and falls through
     *   to MissingMock at LuksOpen, leaving the full LUKS request log
     *   for ordering assertions.
     */
    #[test]
    fn cmd_replace_with_keyfile_orders_format_addkey_backup_open() {
        let f = PoolFixture::one_live_one_missing();

        let kf_dir = tempfile::tempdir().unwrap();
        let kf_path = kf_dir.path().join("braid.key");
        std::fs::write(&kf_path, [0u8; crate::luks::KEYFILE_SIZE]).unwrap();

        // Only disk3's by_id exists; canonical fixture's CryptsetupStatus
        // on braid-disk3 reports inactive (via with_mapper_closed) so
        // `ensure_luks_open` proceeds to issue `LuksOpen`.
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::one_live_one_missing()
            .with_mapper_closed("braid-disk3")
            .install(MockRunner::default(), replace_done)
            .with_handler(keyfile_ordering_success_handler());
        let luks_format_extra_opts = vec![
            "--pbkdf".to_owned(),
            "pbkdf2".to_owned(),
            "--iter-time".to_owned(),
            "1".to_owned(),
        ];

        let result = cmd_replace(
            &runner,
            &fs,
            &f.replace_params()
                .old("disk2")
                .new_disk("disk3=/dev/disk/by-id/virtio-disk3")
                .missing_id(Some(Devid::new(2)))
                .enroll_key_file(Some(kf_path.as_path()))
                .luks_format_extra_opts(&luks_format_extra_opts)
                .build(),
        );

        assert!(
            result.is_err(),
            "cmd_replace must abort at the unmocked LuksOpen request, got: {result:?}"
        );

        let log = runner.requests();
        let position = |label: &str, pred: fn(&CmdRequest) -> bool| -> usize {
            log.iter()
                .position(pred)
                .unwrap_or_else(|| panic!("{label} not found in log: {log:?}"))
        };
        let format = position("LuksFormat", |r| {
            matches!(r, CmdRequest::CryptsetupLuksFormat { .. })
        });
        let CmdRequest::CryptsetupLuksFormat {
            extra_opts,
            uuid: format_uuid,
            label,
            ..
        } = &log[format]
        else {
            unreachable!("format index matched CryptsetupLuksFormat")
        };
        // Structured extras now exclude the managed `--label` token; the
        // label is carried in the structured `label` field. The UUID
        // field carries the journaled op-level identity.
        assert_eq!(
            extra_opts.as_slice(),
            &[
                "--pbkdf".to_owned(),
                "pbkdf2".to_owned(),
                "--iter-time".to_owned(),
                "1".to_owned(),
            ]
        );
        assert_eq!(label.as_str(), "braid-disk3");
        // The journaled UUID is generated at planning time; assert
        // canonical UUID form rather than a specific value.
        assert!(
            uuid::Uuid::parse_str(format_uuid.as_str()).is_ok(),
            "structured uuid field must be a canonical UUID, got: {format_uuid}"
        );
        let addkey = position("LuksAddKeyFile", |r| {
            matches!(r, CmdRequest::CryptsetupLuksAddKeyFile { .. })
        });
        let backup = position("LuksHeaderBackup", |r| {
            matches!(r, CmdRequest::CryptsetupLuksHeaderBackup { .. })
        });
        let open = position("LuksOpen", |r| {
            matches!(r, CmdRequest::CryptsetupLuksOpen { .. })
        });

        assert!(
            format < addkey && addkey < backup && backup < open,
            "expected order LuksFormat({format}) < LuksAddKeyFile({addkey}) < \
             LuksHeaderBackup({backup}) < LuksOpen({open}); log = {log:?}"
        );
    }

    // Intent: fresh replace enriches a LUKS header-backup failure after
    // luksFormat has already succeeded.
    // Why it exists: the replace callsite must keep using the post-mutation
    // wrapper, not the raw local-backup helper.
    // Scenario: replacing a missing disk formats the new disk, then the state
    // directory cannot accept the local header backup.
    #[test]
    fn replace_returns_enriched_error_when_post_format_backup_fails() {
        let f = PoolFixture::one_live_one_missing();
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::one_live_one_missing()
            .with_mapper_closed("braid-disk3")
            .install(MockRunner::default(), replace_done)
            .with_handler(|req| match req {
                CmdRequest::CryptsetupLuksUuid { device }
                    if device == "/dev/disk/by-id/virtio-disk3" =>
                {
                    Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksUUID {device}"),
                        stdout: String::new(),
                        stderr: "Device is not a valid LUKS device.\n".into(),
                        exit_status: 1,
                    }))
                }
                CmdRequest::CryptsetupLuksFormat { device, .. } => {
                    Some(Ok(mock_ok(&format!("cryptsetup luksFormat {device}"), "")))
                }
                CmdRequest::CryptsetupLuksAddKeyFile { device, .. } => {
                    Some(Ok(mock_ok(&format!("cryptsetup luksAddKey {device}"), "")))
                }
                CmdRequest::CryptsetupLuksHeaderBackup { device, .. } => {
                    Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksHeaderBackup {device}"),
                        stdout: String::new(),
                        stderr: "No space left on device".into(),
                        exit_status: 1,
                    }))
                }
                _ => None,
            });

        let err = cmd_replace(
            &runner,
            &fs,
            &f.replace_params()
                .old("disk2")
                .new_disk("disk3=/dev/disk/by-id/virtio-disk3")
                .missing_id(Some(Devid::new(2)))
                .build(),
        )
        .expect_err("post-format header-backup failure should abort replace")
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
     * Intent: on the missing path, `pool.json` already reflects the new
     * membership (disk2 gone, disk3 enriched) when the post-replace soft
     * balance fails, and `pending-op.json` survives so `braid recover`
     * can drive replay.
     *
     * Why it exists: replace previously persisted `pool.json` only after
     * the entire post-mutation maintenance chain (resize + soft balance)
     * succeeded. A soft-balance failure therefore left `pool.json`
     * naming the now-replaced missing disk while the live btrfs pool
     * already had disk3 in its place -- forcing the operator into
     * recovery just to reconcile bookkeeping. The fix moves the
     * `save_membership` call to immediately after `btrfs replace start`,
     * which is the membership commit point. This test pins that
     * ordering: revert the early save and the assertions on
     * `pool.json`'s contents fail because the FailingSoftBalance branch
     * returns before the late-save would have run.
     *
     * Scenario: pool has disk1 live + devid 2 missing (disk2 row).
     * Operator runs `braid replace --old disk2 --missing-id 2 --new
     * disk3=...`. `btrfs replace start` and `btrfs filesystem resize`
     * succeed; the post-replace `btrfs balance start -dconvert=raid1,soft`
     * fails (e.g. ENOSPC, kernel I/O error). The command exits non-zero
     * but `pool.json` already has disk3 enriched and `pending-op.json`
     * is still on disk for recovery.
     */
    #[test]
    fn pool_json_persisted_when_missing_path_soft_balance_fails() {
        let f = PoolFixture::one_live_one_missing();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::one_live_one_missing()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                move |req| match req {
                    CmdRequest::BtrfsReplaceStart { .. } => {
                        replace_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        Some(Ok(mock_ok("btrfs replace start", "")))
                    }
                    CmdRequest::BtrfsFilesystemResize { .. } => {
                        Some(Ok(mock_ok("btrfs filesystem resize", "")))
                    }
                    CmdRequest::BtrfsBalanceRaid1Soft { .. } => Some(Ok(RawCommandOutput {
                        cmd: "btrfs balance raid1 soft".into(),
                        stdout: String::new(),
                        stderr: "ERROR: error during balancing".into(),
                        exit_status: 1,
                    })),
                    _ => None,
                }
            });
        let result = cmd_replace(
            &runner,
            &fs,
            &f.replace_params()
                .old("disk2")
                .new_disk("disk3=/dev/disk/by-id/virtio-disk3")
                .missing_id(Some(Devid::new(2)))
                .build(),
        );

        match &result {
            Err(ReplaceError::JournalLifecycle { source, advice })
                if matches!(source.as_ref(), ReplaceError::Pool(_)) =>
            {
                assert!(advice.contains("run `braid recover`"));
            }
            other => panic!("expected Err(ReplaceError::Pool(..)), got: {other:?}"),
        }

        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        let journal = journal::load_journal(&f.paths)
            .unwrap()
            .expect("journal should remain after post-replace balance failure");
        assert!(
            matches!(
                journal.op,
                journal::OpKind::Replace {
                    phase: journal::ReplacePhase::PostReplaceMaintenance,
                    ..
                }
            ),
            "journal should advance after btrfs replace commits: {:?}",
            journal.op
        );

        let saved = membership::load_membership(&f.paths)
            .expect("pool.json must exist after the membership commit");
        let saved_names: Vec<&str> = saved.names().map(|n| n.as_str()).collect();
        assert!(
            !saved_names.contains(&"disk2"),
            "old missing disk must be gone from pool.json once btrfs replace succeeds, \
             even when the post-replace soft balance fails (saved: {saved_names:?})",
        );
        let disk3_name = DiskName::parse("disk3").unwrap();
        let (_disk3_uuid, disk3) = saved.by_name(&disk3_name).unwrap_or_else(|| {
            panic!(
                "new disk must be in pool.json once btrfs replace succeeds (saved: {saved_names:?})",
            )
        });
        assert!(
            disk3.devid.is_some() && disk3.added_at.is_some(),
            "new disk must carry enriched metadata (devid, added_at) \
            from the post-replace probe: {disk3:?}"
        );
    }

    // Intent: `cmd_replace` warns when its best-effort post-replace
    //   `probe_pool` returns an error, while still succeeding.
    // Why it exists: a completed `btrfs replace` must not silently skip
    //   optional pool.json metadata enrichment; operators need one visible
    //   warning while the durable membership commit still happens.
    // Scenario: live replace of disk2 -> disk3 succeeds. The first
    //   post-replace pool probe returns a command error, then resize and
    //   cleanup complete normally.
    #[test]
    fn cmd_replace_warns_when_post_mount_probe_errors() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .with_post_replace_probe_failure()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                move |req| match req {
                    CmdRequest::BtrfsReplaceStart { .. } => {
                        replace_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        Some(Ok(mock_ok("btrfs replace start", "")))
                    }
                    CmdRequest::CryptsetupClose { .. } => Some(Ok(mock_ok("cryptsetup close", ""))),
                    CmdRequest::BtrfsFilesystemResize { .. } => {
                        Some(Ok(mock_ok("btrfs filesystem resize", "")))
                    }
                    _ => None,
                }
            });

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_replace(&runner, &fs, &f.replace_params().build()));
        });

        result
            .expect("cmd_replace should run")
            .expect("replace should tolerate post-replace probe errors");
        assert_eq!(
            captured
                .matches("Warning: failed to probe pool for metadata refresh: ")
                .count(),
            1,
            "expected one metadata-refresh warning, got: {captured:?}"
        );
        assert!(
            captured.contains("post-replace probe failed"),
            "warning should include the probe error detail, got: {captured:?}"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "pending-op.json should be cleared after successful replace"
        );
        let saved = membership::load_membership(&f.paths)
            .expect("pool.json must exist after the membership commit");
        let disk3_name = DiskName::parse("disk3").unwrap();
        assert!(
            saved.by_name(&disk3_name).is_some(),
            "new disk must be in pool.json once btrfs replace succeeds"
        );
    }

    // Intent: `cmd_replace` must tolerate a post-replace enrichment
    //   `probe_pool` that returns `Err(ProbeError::Parse(_))`, persist the
    //   target membership, and clear the journal.
    // Why it exists: the post-replace membership commit is the durable
    //   bookkeeping boundary after `btrfs replace start`; enrichment is
    //   best-effort. Parser drift in `btrfs filesystem show` must not turn a
    //   completed replace into a hard failure.
    // Scenario: live replace of disk2 -> disk3 succeeds. The first
    //   post-replace `BtrfsFilesystemShow` returns malformed stdout lacking
    //   `Total devices`, so the enrichment probe fails, but resize and cleanup
    //   still complete.
    #[test]
    fn cmd_replace_tolerates_post_replace_probe_err() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let malformed_post_replace_probe_emitted = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done.clone())
            .with_handler({
                let replace_done = replace_done.clone();
                let malformed_post_replace_probe_emitted =
                    malformed_post_replace_probe_emitted.clone();
                move |req| match req {
                    CmdRequest::BtrfsReplaceStart { .. } => {
                        replace_done.store(true, std::sync::atomic::Ordering::Relaxed);
                        Some(Ok(mock_ok("btrfs replace start", "")))
                    }
                    CmdRequest::BtrfsFilesystemShow { mount_point }
                        if replace_done.load(std::sync::atomic::Ordering::Relaxed)
                            && !malformed_post_replace_probe_emitted
                                .swap(true, std::sync::atomic::Ordering::Relaxed) =>
                    {
                        Some(Ok(RawCommandOutput {
                            cmd: format!("btrfs filesystem show {mount_point}"),
                            stdout: "This is not btrfs output at all\nrandom garbage data".into(),
                            stderr: String::new(),
                            exit_status: 0,
                        }))
                    }
                    CmdRequest::CryptsetupClose { .. } => Some(Ok(mock_ok("cryptsetup close", ""))),
                    CmdRequest::BtrfsFilesystemResize { .. } => {
                        Some(Ok(mock_ok("btrfs filesystem resize", "")))
                    }
                    _ => None,
                }
            });

        cmd_replace(&runner, &fs, &f.replace_params().build())
            .expect("replace should tolerate post-replace probe parse errors");

        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "pending-op.json should be cleared after successful replace"
        );
        let saved = membership::load_membership(&f.paths)
            .expect("pool.json must exist after the membership commit");
        let saved_names: Vec<&str> = saved.names().map(|n| n.as_str()).collect();
        assert!(
            !saved_names.contains(&"disk2"),
            "old disk must be gone from pool.json once btrfs replace succeeds \
             (saved: {saved_names:?})",
        );
        let disk3_name = DiskName::parse("disk3").unwrap();
        let (_disk3_uuid, disk3) = saved.by_name(&disk3_name).unwrap_or_else(|| {
            panic!(
                "new disk must be in pool.json once btrfs replace succeeds (saved: {saved_names:?})",
            )
        });
        assert!(
            disk3.devid.is_none(),
            "new disk devid must remain None when post-replace probe returns Err, got: {:?}",
            disk3.devid
        );
        assert!(
            disk3.added_at.is_some(),
            "new disk added_at fallback must be stamped even when post-replace probe returns Err"
        );
    }

    /* Intent: the live-path dry-run preview flows through
     * `plan_replace(...).preview().render()`, and the confirmation-only
     * 1-disk `WARNING:` line must never leak into `--dry-run` stdout.
     *
     * Why it exists: `braid replace --dry-run` routes through
     * `ReplacePlan::preview()` instead of `Step::print_dry_run(&steps)`.
     * A regression that surfaced the confirmation-only `WARNING:` line
     * on dry-run would change the bytes an operator sees. A regression
     * that reordered steps or dropped the cryptsetup-close step would
     * also change the rendered output. Both failure modes are caught
     * here.
     *
     * Scenario: 2-disk pool, operator previews
     * `braid replace --old disk2 --new disk3=...` with a fresh (but
     * already-LUKS-open) replacement. Mirrors the fixture used by
     * `dry_run_does_not_acquire_inhibitor`.
     */
    #[test]
    fn plan_replace_live_preview_has_no_notes_and_matches_legacy_step_render() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner =
            ReplacementPool::two_disk_healthy().install(MockRunner::default(), replace_done);
        let plan = plan_replace(&runner, &fs, &f.replace_params().dry_run(true).build())
            .expect("plan_replace should succeed on live-path fixture");

        let preview = plan.preview();
        let rendered = preview.render();
        let legacy = Step::render_dry_run(&preview.steps);
        // Byte-equivalence holds because this fixture produces zero
        // notes (clean preflight on a rw pool with no busy op). A
        // future fixture with real preflight notes would render them
        // above the step block and byte-equivalence would no longer
        // hold.
        assert_eq!(
            rendered, legacy,
            "plan.preview().render() must be byte-equivalent to Step::render_dry_run(&plan.preview().steps) for the live path",
        );

        assert!(
            rendered.contains("btrfs replace start"),
            "live-path preview must contain `btrfs replace start` step, got:\n{rendered}",
        );
        assert!(
            rendered.contains("cryptsetup close braid-disk2"),
            "live-path preview must close the old mapper, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("WARNING:"),
            "live-path dry-run preview must not leak confirmation-only WARNING lines, got:\n{rendered}",
        );
        assert!(
            f.inhibitor.acquire_count() == 0,
            "plan_replace must not acquire the sleep inhibitor",
        );
    }

    /* Intent: the missing-path dry-run preview flows through
     * `plan_replace(...).preview().render()`, and the confirmation-only
     * 1-disk `WARNING:` line must never leak into `--dry-run` stdout.
     *
     * Why it exists: same regression surface as the live-path
     * preview test, but for the `ReplaceSource::Missing` branch
     * (different step sequence: no cryptsetup close, soft balance at
     * the tail when clearing the last missing device).
     *
     * Scenario: 2-disk pool, devid 2 missing, operator previews
     * `braid replace --old disk2 --missing-id 2 --new disk3=...`.
     * Mirrors the fixture used by
     * `cmd_replace_missing_path_runs_soft_balance_after_replace_and_resize`.
     */
    #[test]
    fn plan_replace_missing_preview_has_no_notes_and_matches_legacy_step_render() {
        let f = PoolFixture::one_live_one_missing();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::one_live_one_missing()
            .install(MockRunner::default(), replace_done.clone());
        let plan = plan_replace(
            &runner,
            &fs,
            &f.replace_params()
                .old("disk2")
                .new_disk("disk3=/dev/disk/by-id/virtio-disk3")
                .missing_id(Some(Devid::new(2)))
                .dry_run(true)
                .build(),
        )
        .expect("plan_replace should succeed on missing-path fixture");

        let preview = plan.preview();
        let rendered = preview.render();
        let legacy = Step::render_dry_run(&preview.steps);
        // Byte-equivalence holds because this fixture produces zero
        // notes (clean preflight on a rw pool with no busy op). A
        // future fixture with real preflight notes would render them
        // above the step block and byte-equivalence would no longer
        // hold.
        assert_eq!(
            rendered, legacy,
            "plan.preview().render() must be byte-equivalent to Step::render_dry_run(&plan.preview().steps) for the missing path",
        );

        assert!(
            rendered.contains("btrfs replace start"),
            "missing-path preview must contain `btrfs replace start`, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("cryptsetup close"),
            "missing-path preview must NOT carry a cryptsetup close step (no old mapper), got:\n{rendered}",
        );
        assert!(
            rendered.contains("-dconvert=raid1,soft"),
            "missing-path preview must carry the soft balance step when clearing the last missing device, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("WARNING:"),
            "missing-path dry-run preview must not leak confirmation-only WARNING lines, got:\n{rendered}",
        );

        // plan_replace must not mutate btrfs state -- replace_done
        // flips only inside execute()'s BtrfsReplaceStart dispatch.
        assert!(
            !replace_done.load(std::sync::atomic::Ordering::Relaxed),
            "plan_replace must not issue btrfs replace start",
        );
        assert!(
            f.inhibitor.acquire_count() == 0,
            "plan_replace must not acquire the sleep inhibitor",
        );
    }

    // Intent: missing-path planning resolves `--old` by persisted member
    //   name/UUID/devid without probing by-id paths that only look like the
    //   old name.
    // Why it exists: the helper-level no-probe assertion became vacuous once
    //   `resolve_replace_source` stopped receiving a runner. This pins the
    //   behavior at the command-request boundary that still owns probes.
    // Scenario: pool.json has `misleading-label` at `/dev/disk/by-id/right`
    //   and `decoy` at `/dev/disk/by-id/misleading-label`; btrfs reports
    //   devid 2 missing, and planning must select devid 2 without targeting
    //   either decoy by-id.
    #[test]
    fn plan_replace_missing_path_decoy_does_not_probe_old_by_ids() {
        let f = PoolFixture::empty();
        let u_r = LuksUuid::parse("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaa0600").unwrap();
        let u_d = LuksUuid::parse("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbb0601").unwrap();
        let member_r = membership::DiskMember {
            name: disk_name("misleading-label"),
            by_id: ByIdPath::parse("/dev/disk/by-id/right").unwrap(),
            devid: Some(Devid::new(2)),
            added_at: None,
        };
        let member_d = membership::DiskMember {
            name: disk_name("decoy"),
            by_id: ByIdPath::parse("/dev/disk/by-id/misleading-label").unwrap(),
            devid: Some(Devid::new(99)),
            added_at: None,
        };
        let pre = membership_from(vec![(u_r, member_r), (u_d, member_d)]);
        membership::save_membership(&pre, &f.paths).expect("save decoy membership");

        let fs = MockFs::storage(vec!["/dev/disk/by-id/new".into()]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::one_live_one_missing()
            .install(MockRunner::default(), replace_done)
            .with_handler(|req| match req {
                CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/disk/by-id/new" => {
                    Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksUUID {device}"),
                        stdout: String::new(),
                        stderr: "Device is not a valid LUKS device.\n".into(),
                        exit_status: 1,
                    }))
                }
                CmdRequest::LsblkField {
                    device,
                    field: crate::cmd::LsblkFieldKind::Size,
                } if device == "/dev/disk/by-id/new" => {
                    Some(Ok(mock_ok(&format!("lsblk -b {device}"), "536870912\n")))
                }
                _ => None,
            });

        let plan = plan_replace(
            &runner,
            &fs,
            &f.replace_params()
                .old("misleading-label")
                .new_disk("replacement=/dev/disk/by-id/new")
                .dry_run(true)
                .build(),
        )
        .expect("missing-path decoy fixture should plan");
        match &plan.work_plan.replace_source {
            ReplaceSource::Missing { devid } => assert_eq!(*devid, Devid::new(2)),
            other => panic!("expected Missing {{ devid: 2 }}, got: {other:?}"),
        }

        let forbidden_targets = ["/dev/disk/by-id/right", "/dev/disk/by-id/misleading-label"];
        let requests = runner.requests();
        for request in &requests {
            let argv = request.to_argv();
            assert!(
                argv.args
                    .iter()
                    .all(|arg| !forbidden_targets.contains(&arg.as_str())),
                "missing-path planning must not target decoy by-id paths; request={request:?}, argv={argv:?}, requests={requests:?}"
            );
        }
    }

    /* Intent: plan_replace surfaces an in-flight exclusive op as a
     * PreviewNote::Info on `plan.notes`, and the rendered preview
     * contains the "waiting for in-flight <op>" line. Confirmation-only
     * 1-disk `WARNING:` output still does not leak into the preview.
     * Why it exists: PlanFailure migration moves the busy-op diagnostic
     * from stderr into plan.notes; a regression leaking it back to
     * stderr breaks the dry-run stdout-only contract.
     * Scenario: 2-disk pool, sysfs reports "device add", operator
     * previews `braid replace --old disk2 --new disk3=...`. Mirrors
     * the live-path preview fixture.
     */
    #[test]
    fn plan_replace_preflight_busy_op_becomes_info_note() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ])
        .with_excl_op("device add\n");
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner =
            ReplacementPool::two_disk_healthy().install(MockRunner::default(), replace_done);
        let report = plan_replace(&runner, &fs, &f.replace_params().dry_run(true).build());
        let plan = report.expect("plan_replace should succeed on live-path fixture + busy op");
        assert_eq!(
            plan.notes.len(),
            1,
            "expected one preflight Info note, got {:?}",
            plan.notes,
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
        assert!(
            !rendered.contains("WARNING:"),
            "confirmation-only WARNING lines must not leak into preview, got:\n{rendered}",
        );
    }

    // Intent: live replacement with a fresh-LUKS undersized target is refused
    //   during planning.
    // Why it exists: this is the destructive regression path -- without the
    //   check, replace would write the journal and run luksFormat before
    //   btrfs rejects the target as too small.
    // Scenario: disk2 is live, disk3 is raw, and disk3's modeled mapper
    //   capacity is one byte smaller than disk2's btrfs `total_bytes`.
    #[test]
    fn plan_replace_refuses_when_target_smaller_live_fresh() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(|req| match req {
                CmdRequest::CryptsetupLuksUuid { device }
                    if device == "/dev/disk/by-id/virtio-disk3" =>
                {
                    Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksUUID {device}"),
                        stdout: String::new(),
                        stderr: "Device is not a valid LUKS device.\n".into(),
                        exit_status: 1,
                    }))
                }
                _ => None,
            });
        let dev_info = crate::btrfs_ioctl::tests_support::MockBtrfsDevInfo::default()
            .with_total_bytes("/mnt/storage", Devid::new(2), 520_093_697);

        let failure = match super::plan_replace(
            &runner,
            &fs,
            &dev_info,
            &f.replace_params().dry_run(true).build(),
        ) {
            Ok(_) => panic!("undersized fresh target should fail planning"),
            Err(failure) => failure,
        };

        match &failure.error {
            ReplaceError::Validation(msg) => {
                assert!(
                    msg.contains("smaller than the disk being replaced"),
                    "unexpected validation: {msg}"
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "planning refusal must not write pending-op.json"
        );
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksFormat { .. })),
            "planning refusal must not format the new disk"
        );
    }

    // Intent: live replacement with an existing-LUKS undersized target is
    //   refused using the parsed LUKS2 segment capacity.
    // Why it exists: existing LUKS targets skip luksFormat, but still must
    //   fail before journal write and mapper open.
    // Scenario: disk3 already has a dynamic LUKS2 segment whose mapper
    //   capacity is one byte smaller than disk2's btrfs `total_bytes`.
    #[test]
    fn plan_replace_refuses_when_target_smaller_live_existing_luks() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner =
            ReplacementPool::two_disk_healthy().install(MockRunner::default(), replace_done);
        let dev_info = crate::btrfs_ioctl::tests_support::MockBtrfsDevInfo::default()
            .with_total_bytes("/mnt/storage", Devid::new(2), 520_093_697);

        let failure = match super::plan_replace(
            &runner,
            &fs,
            &dev_info,
            &f.replace_params().dry_run(true).build(),
        ) {
            Ok(_) => panic!("undersized existing-LUKS target should fail planning"),
            Err(failure) => failure,
        };

        match &failure.error {
            ReplaceError::Validation(msg) => {
                assert!(
                    msg.contains("smaller than the disk being replaced"),
                    "unexpected validation: {msg}"
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "planning refusal must not write pending-op.json"
        );
    }

    // Intent: missing-source replacement still reads the missing devid's
    //   btrfs `total_bytes` and refuses an undersized target.
    // Why it exists: btrfs reports size 0 in some text surfaces for missing
    //   devices; the ioctl authority must be used for this branch.
    // Scenario: disk2 is missing, `--missing-id 2` is supplied, and disk3 is
    //   one byte too small.
    #[test]
    fn plan_replace_refuses_when_target_smaller_missing() {
        let f = PoolFixture::one_live_one_missing();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner =
            ReplacementPool::one_live_one_missing().install(MockRunner::default(), replace_done);
        let dev_info = crate::btrfs_ioctl::tests_support::MockBtrfsDevInfo::default()
            .with_total_bytes("/mnt/storage", Devid::new(2), 520_093_697);

        let failure = match super::plan_replace(
            &runner,
            &fs,
            &dev_info,
            &f.replace_params()
                .missing_id(Some(Devid::new(2)))
                .dry_run(true)
                .build(),
        ) {
            Ok(_) => panic!("undersized target for missing source should fail planning"),
            Err(failure) => failure,
        };

        match &failure.error {
            ReplaceError::Validation(msg) => {
                assert!(
                    msg.contains("smaller than the disk being replaced"),
                    "unexpected validation: {msg}"
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "planning refusal must not write pending-op.json"
        );
    }

    // Intent: source-size lookup failures are preserved as planning-time
    //   validation errors.
    // Why it exists: unknown source size must fail closed because the
    //   downstream failure would happen after journal and format boundaries.
    // Scenario: btrfs device-info cannot find the resolved source devid.
    #[test]
    fn plan_replace_refuses_when_dev_info_devid_not_found() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner =
            ReplacementPool::two_disk_healthy().install(MockRunner::default(), replace_done);
        let dev_info = crate::btrfs_ioctl::tests_support::MockBtrfsDevInfo::default();

        let failure = match super::plan_replace(
            &runner,
            &fs,
            &dev_info,
            &f.replace_params().dry_run(true).build(),
        ) {
            Ok(_) => panic!("missing dev-info row should fail planning"),
            Err(failure) => failure,
        };

        match &failure.error {
            ReplaceError::Validation(msg) => {
                assert!(
                    msg.contains("failed to read btrfs total_bytes for devid 2"),
                    "unexpected validation: {msg}"
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "planning refusal must not write pending-op.json"
        );
    }

    // Intent: plan_replace rejects an absent replacement disk with the
    //   exact validation text and the requested disk identity.
    // Why it exists: the absent-disk rejection moved from the builder to
    //   the probe boundary, and must keep preserving preflight notes.
    // Scenario: the pool is healthy but sysfs reports an in-flight
    //   device add while the requested new disk is unplugged.
    #[test]
    fn plan_replace_rejects_absent_new_disk_with_exact_message() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![]).with_excl_op("device add\n");
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner =
            ReplacementPool::two_disk_healthy().install(MockRunner::default(), replace_done);

        let failure = match plan_replace(&runner, &fs, &f.replace_params().dry_run(true).build()) {
            Ok(_) => panic!("expected absent new disk to fail planning"),
            Err(failure) => failure,
        };

        match &failure.error {
            ReplaceError::Validation(body) => {
                assert_eq!(
                    body,
                    "new disk 'disk3' (/dev/disk/by-id/virtio-disk3) is not present. Is it plugged in?"
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert!(
            failure.notes.iter().any(|n| matches!(
                n,
                PreviewNote::Info(body)
                    if body.contains("waiting for in-flight")
                        && body.contains("device add")
            )),
            "preflight Info note must survive absent-disk rejection: {:?}",
            failure.notes,
        );
    }

    // Intent: a same-name `braid replace --old/--new` typo aborts in the
    // planner before any injected probe boundary.
    // Why it exists: the pre-hoist preserved-note behavior conflated a pure
    // input-shape error with I/O-precondition context; this path now fails
    // before preflight can accumulate state-context notes.
    // Scenario: user runs `braid replace --old disk2 --new disk2=/...`;
    // the planner returns the same-name validation error with no probes.
    #[test]
    fn plan_replace_old_equals_new_aborts_before_any_probe() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();
        let failure = match super::plan_replace(
            &PanicRunner,
            &PanicFilesystem,
            &PanicBtrfsDevInfo,
            &ReplaceParams {
                config: &test_config(),
                old_name: "disk2",
                new_name: "disk2=/dev/disk/by-id/virtio-disk2",
                missing_id: None,
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: None,
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                confirm: &confirm,
                sleeper: &crate::progress::NoopSleeper,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        ) {
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
            Err(failure) => failure,
        };
        match &failure.error {
            ReplaceError::Validation(msg) => {
                assert!(
                    msg.contains("--old and --new must be different"),
                    "expected same-name refusal wording, got: {msg}"
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert_eq!(
            failure.notes.len(),
            0,
            "same-name input validation must not preserve preflight notes, got: {:?}",
            failure.notes,
        );
        assert_eq!(inhibitor.acquire_count(), 0);
    }

    // Intent: `braid replace --enroll` rejects a missing braid.key during
    // planning, before any injected probe boundary.
    // Why it exists: a typoed keyfile path must not let replace format the
    // new disk and then fail only at keyfile enrollment.
    // Scenario: user passes a nonexistent enroll directory while replacing a
    // disk; the command refuses with a keyfile error and no probes run.
    #[test]
    fn plan_replace_aborts_when_keyfile_missing_before_any_probe() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let config_tmp = tempfile::tempdir().unwrap();
        let kf_path = config_tmp.path().join("does-not-exist").join("braid.key");
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();

        let report = super::plan_replace(
            &PanicRunner,
            &PanicFilesystem,
            &PanicBtrfsDevInfo,
            &ReplaceParams {
                config: &test_config(),
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: Some(kf_path.as_path()),
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                confirm: &confirm,
                sleeper: &crate::progress::NoopSleeper,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        );

        let failure = match report {
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
            Err(failure) => failure,
        };
        match failure.error {
            ReplaceError::Validation(msg) => assert!(
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

    // Intent: `braid replace --enroll` rejects a directory at braid.key during
    // planning, before any injected probe boundary.
    // Why it exists: checking only existence would still allow an invalid
    // keyfile path to reach destructive LUKS work before enrollment fails.
    // Scenario: user points --enroll at a directory containing a subdirectory
    // named braid.key; the command refuses before any disk inspection.
    #[test]
    fn plan_replace_aborts_when_keyfile_is_directory_before_any_probe() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let config_tmp = tempfile::tempdir().unwrap();
        let kf_path = config_tmp.path().join("braid.key");
        std::fs::create_dir(&kf_path).unwrap();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let confirm = crate::confirm::RecordingConfirm::new();

        let report = super::plan_replace(
            &PanicRunner,
            &PanicFilesystem,
            &PanicBtrfsDevInfo,
            &ReplaceParams {
                config: &test_config(),
                old_name: "disk2",
                new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                missing_id: None,
                dry_run: true,
                yes: true,
                passphrase_stdin: false,
                passphrase_file: None,
                enroll_key_file: Some(kf_path.as_path()),
                luks_format_extra_opts: &[],
                progress: crate::progress::ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
                confirm: &confirm,
                sleeper: &crate::progress::NoopSleeper,
                backing_path_resolver:
                    crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
            },
        );

        let failure = match report {
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
            Err(failure) => failure,
        };
        match failure.error {
            ReplaceError::Validation(msg) => assert!(
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

    /// Override handler for the keyfile-probe tests: marks disk3 (the
    /// raw replacement) as not-LUKS and forces every existing-member
    /// `CryptsetupLuksDump` to fail with stderr containing the device
    /// path. Layered on top of the canonical pool topology via
    /// `with_handler`; reverse-order dispatch means this shadows the
    /// fixture's disk3 LUKS UUID for the not-LUKS classification.
    fn keyfile_probe_all_failures_handler()
    -> impl Fn(&CmdRequest) -> Option<Result<RawCommandOutput, CmdError>> + Send + Sync + 'static
    {
        |req| match req {
            CmdRequest::CryptsetupLuksUuid { device }
                if device == "/dev/disk/by-id/virtio-disk3" =>
            {
                Some(Ok(RawCommandOutput {
                    cmd: format!("cryptsetup luksUUID {device}"),
                    stdout: String::new(),
                    stderr: "Device is not a valid LUKS device.\n".into(),
                    exit_status: 1,
                }))
            }
            CmdRequest::CryptsetupLuksDump { device } => Some(Ok(RawCommandOutput {
                cmd: format!("cryptsetup luksDump --dump-json-metadata {device}"),
                stdout: String::new(),
                stderr: format!("forced luksDump failure on {device}"),
                exit_status: 5,
            })),
            _ => None,
        }
    }

    /* Intent: plan_replace turns keyfile probe failures into warning notes
     * on the replacement plan.
     * Why it exists: dry-run, real-run, and preserved-error output should use
     * the same PreviewNote contract instead of a confirmation-only stderr
     * side path.
     * Scenario: operator previews replacing disk2 with raw disk3 while
     * existing pool members would fail the keyfile probe.
     */
    #[test]
    fn plan_replace_keyfile_probe_failure_becomes_warn_notes() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(keyfile_probe_all_failures_handler());
        let report = plan_replace(
            &runner,
            &fs,
            &f.replace_params().dry_run(true).yes(false).build(),
        );
        let plan = report.expect("plan_replace should succeed");
        let rendered = plan.preview().render();

        assert!(
            rendered.contains(
                "[warn] could not check keyfile enrollment on /dev/vdb: cryptsetup luksDump failed (exit 5): forced luksDump failure on /dev/vdb; proceeding as if no keyfile is enrolled"
            ),
            "dry-run preview must include the first probe-failure warning, got:\n{rendered}",
        );
        assert!(
            rendered.contains(
                "[warn] could not check keyfile enrollment on /dev/vdc: cryptsetup luksDump failed (exit 5): forced luksDump failure on /dev/vdc; proceeding as if no keyfile is enrolled"
            ),
            "dry-run preview must include the second probe-failure warning, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("Existing pool drives have a keyfile"),
            "probe-failure-only case must not emit the keyfile-asymmetry warning, got:\n{rendered}",
        );
        assert!(
            runner
                .requests()
                .iter()
                .any(|request| matches!(request, CmdRequest::CryptsetupLuksDump { .. })),
            "plan_replace must run the keyfile enrollment probe"
        );
    }

    /* Intent: plan_replace emits keyfile-asymmetry as a PreviewNote::Warn
     * once any pool member proves slot 1 is occupied.
     * Why it exists: replace should use the same keyfile warning policy and
     * wording as add, with no legacy `WARNING:` block.
     * Scenario: disk1's luksDump fails, disk2 reports slot 1 occupied, and
     * the operator replaces with a raw disk without --enroll.
     */
    #[test]
    fn plan_replace_keyfile_asymmetry_suppresses_probe_failure_warning() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(|req| match req {
                CmdRequest::CryptsetupLuksUuid { device }
                    if device == "/dev/disk/by-id/virtio-disk3" =>
                {
                    Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksUUID {device}"),
                        stdout: String::new(),
                        stderr: "Device is not a valid LUKS device.\n".into(),
                        exit_status: 1,
                    }))
                }
                CmdRequest::CryptsetupLuksDump { device } => match device.as_str() {
                    "/dev/vdb" => Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksDump --dump-json-metadata {device}"),
                        stdout: String::new(),
                        stderr: format!("forced luksDump failure on {device}"),
                        exit_status: 5,
                    })),
                    "/dev/vdc" => Some(Ok(mock_ok(
                        "cryptsetup luksDump --dump-json-metadata",
                        r#"{"keyslots":{"0":{"type":"luks2"},"1":{"type":"luks2"}}}"#,
                    ))),
                    _ => None,
                },
                _ => None,
            });
        let report = plan_replace(
            &runner,
            &fs,
            &f.replace_params().dry_run(true).yes(false).build(),
        );
        let plan = report.expect("plan_replace should succeed");
        let rendered = plan.preview().render();

        assert_eq!(
            plan.notes.len(),
            1,
            "occupied slot 1 must emit exactly one warning, got {:?}",
            plan.notes
        );
        assert!(
            matches!(
                &plan.notes[0],
                PreviewNote::Warn(body) if body == &format_keyfile_asymmetry_warning()
            ),
            "occupied slot 1 must emit only the keyfile-asymmetry warning, got {:?}",
            plan.notes
        );
        assert!(
            rendered.contains("[warn] Existing pool drives have a keyfile (keyslot-1)"),
            "mixed probe result must render canonical keyfile-asymmetry warning, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("WARNING:"),
            "keyfile-asymmetry warning must not use legacy WARNING prefix, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("could not check keyfile enrollment"),
            "occupied slot 1 must suppress probe-failure uncertainty notes, got:\n{rendered}",
        );
    }

    fn keyfile_pool_occupied_handler()
    -> impl Fn(&CmdRequest) -> Option<Result<RawCommandOutput, CmdError>> + Send + Sync + 'static
    {
        |req| match req {
            CmdRequest::CryptsetupLuksDump { device }
                if device == "/dev/vdb" || device == "/dev/vdc" =>
            {
                Some(Ok(mock_ok(
                    &format!("cryptsetup luksDump --dump-json-metadata {device}"),
                    r#"{"keyslots":{"0":{"type":"luks2"},"1":{"type":"luks2"}}}"#,
                )))
            }
            _ => None,
        }
    }

    fn luks_dump_dynamic_json(keyslots: &str) -> String {
        format!(
            r#"{{
  "keyslots": {{{keyslots}}},
  "tokens": {{}},
  "segments": {{
    "0": {{
      "type": "crypt",
      "offset": "16777216",
      "size": "dynamic",
      "iv_tweak": "0",
      "encryption": "aes-xts-plain64",
      "sector_size": 512
    }}
  }},
  "digests": {{}},
  "config": {{}}
}}"#
        )
    }

    fn luks_dump_keyslot_json(slot: u8) -> String {
        format!(
            r#""{slot}": {{"type": "luks2", "key_size": 64, "af": {{}}, "area": {{}}, "kdf": {{}}}}"#
        )
    }

    // Intent: replacing with a returning LUKS disk whose slot 1 is empty emits
    // the keyfile-asymmetry warning when the pool has keyfile enrollment.
    // Why it exists: replace used to warn only for fresh-format targets, so
    // returning LUKS disks could silently remain outside auto-unlock.
    // Scenario: disk3 is a closed LUKS replacement target with slot 0 only,
    // while disk1 and disk2 prove keyfile enrollment.
    #[test]
    fn plan_replace_keyfile_asymmetry_emits_warn_for_returning_disk_with_empty_slot_1() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .with_mapper_closed("braid-disk3")
            .install(MockRunner::default(), replace_done)
            .with_handler(keyfile_pool_occupied_handler());

        let plan = plan_replace(
            &runner,
            &fs,
            &f.replace_params().dry_run(true).yes(false).build(),
        )
        .expect("plan_replace should succeed");

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

    // Intent: replacing with a returning LUKS disk whose slot 1 is occupied
    // does not emit keyfile-asymmetry warnings.
    // Why it exists: returning targets should warn only when they would lack
    // slot 1 after replacement.
    // Scenario: disk3 is a closed LUKS replacement target that already has
    // keyfile slot 1 populated.
    #[test]
    fn plan_replace_keyfile_no_warn_for_returning_disk_with_occupied_slot_1() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .with_mapper_closed("braid-disk3")
            .install(MockRunner::default(), replace_done)
            .with_handler(|req| match req {
                CmdRequest::CryptsetupLuksDump { device }
                    if device == "/dev/disk/by-id/virtio-disk3" =>
                {
                    let keyslots = format!(
                        "{},{}",
                        luks_dump_keyslot_json(0),
                        luks_dump_keyslot_json(1)
                    );
                    Some(Ok(mock_ok(
                        &format!("cryptsetup luksDump --dump-json-metadata {device}"),
                        &luks_dump_dynamic_json(&keyslots),
                    )))
                }
                _ => None,
            })
            .with_handler(keyfile_pool_occupied_handler());

        let plan = plan_replace(
            &runner,
            &fs,
            &f.replace_params().dry_run(true).yes(false).build(),
        )
        .expect("plan_replace should succeed");

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

    // Intent: returning replacement target slot-probe failures surface as
    // target-specific PreviewNote::Warn diagnostics.
    // Why it exists: a target-side luksDump error should not be rendered as
    // existing-pool enrollment uncertainty.
    // Scenario: disk3 is a closed LUKS replacement target, but probing its
    // JSON luksDump fails during preview.
    #[test]
    fn plan_replace_keyfile_emits_target_probe_failure_for_returning_disk_dump_error() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec!["/dev/disk/by-id/virtio-disk3".into()]);
        let target = ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap();
        let replace_done = Arc::new(AtomicBool::new(false));
        let target_dump_count = Arc::new(AtomicU32::new(0));
        let runner = ReplacementPool::two_disk_healthy()
            .with_mapper_closed("braid-disk3")
            .install(MockRunner::default(), replace_done)
            .with_handler({
                let target_dump_count = target_dump_count.clone();
                move |req| match req {
                    CmdRequest::CryptsetupLuksDump { device }
                        if device == "/dev/disk/by-id/virtio-disk3" =>
                    {
                        if target_dump_count.fetch_add(1, Ordering::SeqCst) == 0 {
                            return Some(Ok(mock_ok(
                                &format!("cryptsetup luksDump --dump-json-metadata {device}"),
                                &luks_dump_dynamic_json(""),
                            )));
                        }
                        Some(Ok(RawCommandOutput {
                            cmd: format!("cryptsetup luksDump --dump-json-metadata {device}"),
                            stdout: String::new(),
                            stderr: format!("forced luksDump failure on target {device}"),
                            exit_status: 5,
                        }))
                    }
                    _ => None,
                }
            })
            .with_handler(keyfile_pool_occupied_handler());

        let plan = plan_replace(
            &runner,
            &fs,
            &f.replace_params().dry_run(true).yes(false).build(),
        )
        .expect("plan_replace should succeed");

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

    fn source_io_stats_json(devid: u64, read_io_errs: u64) -> String {
        format!(
            r#"{{
  "device-stats": [
    {{
      "devid": {devid},
      "read_io_errs": {read_io_errs},
      "write_io_errs": 0,
      "flush_io_errs": 0,
      "corruption_errs": 0,
      "generation_errs": 0
    }}
  ]
}}"#
        )
    }

    fn dirty_source_stats_handler(
        devid: u64,
    ) -> impl Fn(&CmdRequest) -> Option<Result<RawCommandOutput, CmdError>> + Send + Sync + 'static
    {
        let stats = source_io_stats_json(devid, 3);
        move |req| match req {
            CmdRequest::BtrfsDeviceStatsJson { .. } => {
                Some(Ok(mock_ok("btrfs device stats", &stats)))
            }
            _ => None,
        }
    }

    // Intent: live-source planning emits a structured warning note when the
    // selected source devid has non-zero btrfs I/O error counters.
    // Why it exists: source-health diagnostics must appear in dry-run stdout
    // before an operator commits to wiping the replacement disk.
    // Scenario: disk2 resolves to live devid 2 and `btrfs device stats`
    // reports read_io_errs = 3 for that same devid.
    #[test]
    fn plan_replace_live_emits_warn_when_source_has_io_errors() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(dirty_source_stats_handler(2));

        let plan = plan_replace(&runner, &fs, &f.replace_params().dry_run(true).build())
            .expect("plan_replace should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(warns.len(), 1, "expected one Warn note, got {warns:?}");
        assert_eq!(warns[0], &format_source_io_error_warning(Devid::new(2)));

        let rendered = plan.preview().render();
        assert!(
            rendered.contains("[warn] source device (devid 2) has I/O errors"),
            "dry-run preview must include the source I/O warning, got:\n{rendered}"
        );
    }

    // Intent: live-source planning turns a failed btrfs stats command into a
    // structured warning note rather than aborting replacement planning.
    // Why it exists: the source-health probe is informational and must stay
    // non-blocking even when the external command is unavailable.
    // Scenario: disk2 resolves to live devid 2, but `btrfs device stats`
    // returns a runner error.
    #[test]
    fn plan_replace_live_warns_when_source_stats_probe_fails() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(|req| match req {
                CmdRequest::BtrfsDeviceStatsJson { .. } => {
                    Some(Err(CmdError::Failed("stats unavailable".into())))
                }
                _ => None,
            });

        let plan = plan_replace(&runner, &fs, &f.replace_params().dry_run(true).build())
            .expect("plan_replace should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(warns.len(), 1, "expected one Warn note, got {warns:?}");
        assert!(
            warns[0].starts_with("could not probe source device (devid 2) for I/O errors"),
            "runner failures must surface as source probe warnings, got: {:?}",
            warns[0]
        );
    }

    // Intent: live-source planning turns unparseable btrfs stats JSON into a
    // structured warning note rather than aborting replacement planning.
    // Why it exists: parser drift in an informational source-health probe
    // should be visible but must not become a hard replace refusal.
    // Scenario: disk2 resolves to live devid 2, but `btrfs device stats --format
    // json` returns invalid JSON.
    #[test]
    fn plan_replace_live_warns_when_source_stats_unparseable() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(|req| match req {
                CmdRequest::BtrfsDeviceStatsJson { .. } => Some(Ok(RawCommandOutput {
                    cmd: "btrfs device stats".into(),
                    stdout: "not json{".into(),
                    stderr: String::new(),
                    exit_status: 0,
                })),
                _ => None,
            });

        let plan = plan_replace(&runner, &fs, &f.replace_params().dry_run(true).build())
            .expect("plan_replace should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(warns.len(), 1, "expected one Warn note, got {warns:?}");
        assert!(
            warns[0].starts_with("could not probe source device (devid 2) for I/O errors"),
            "parse failures must surface as source probe warnings, got: {:?}",
            warns[0]
        );
    }

    // Intent: missing-source planning does not run the live-source stats probe.
    // Why it exists: a missing source has no live device to read from, so the
    // live-source I/O warning would be misleading and wasteful.
    // Scenario: disk2 is a btrfs MISSING devid and the stats handler would
    // report dirty counters if it were called.
    #[test]
    fn plan_replace_missing_source_skips_io_probe_even_with_dirty_stats() {
        let f = PoolFixture::one_live_one_missing();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::one_live_one_missing()
            .install(MockRunner::default(), replace_done)
            .with_handler(dirty_source_stats_handler(2));

        let plan = plan_replace(
            &runner,
            &fs,
            &f.replace_params()
                .old("disk2")
                .new_disk("disk3=/dev/disk/by-id/virtio-disk3")
                .missing_id(Some(Devid::new(2)))
                .dry_run(true)
                .build(),
        )
        .expect("plan_replace should succeed");

        let warns: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Warn(b) => Some(b),
                _ => None,
            })
            .collect();
        assert!(
            warns.iter().all(|body| !body.contains("I/O errors")),
            "missing-source planning must not emit source I/O warnings, got {warns:?}"
        );
        let calls = runner.requests();
        let stats_requests = calls
            .iter()
            .filter(|r| matches!(r, CmdRequest::BtrfsDeviceStatsJson { .. }))
            .count();
        assert_eq!(
            stats_requests, 0,
            "missing-source planning must not run btrfs device stats, got {calls:?}"
        );
    }

    // Intent: real replace runs render the source I/O warning exactly once.
    // Why it exists: moving the warning into planning must delete the legacy
    // execute-time stats probe, or real runs would probe and warn twice.
    // Scenario: disk2 has dirty stats, confirmation is accepted, and a forced
    // `btrfs replace start` failure stops the run after note rendering.
    #[test]
    fn cmd_replace_live_source_io_warning_renders_once_on_real_run() {
        let f = PoolFixture::two_disk_healthy();
        f.confirm.accept();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        let runner = ReplacementPool::two_disk_healthy()
            .install(MockRunner::default(), replace_done)
            .with_handler(dirty_source_stats_handler(2))
            .with_handler(replace_start_fails_handler());

        let (result, stderr) = super::replace_stderr_capture::capture(|| {
            cmd_replace(&runner, &fs, &f.replace_params().yes(false).build())
        });

        assert!(result.is_err(), "forced replace-start failure must surface");
        assert_eq!(
            stderr
                .matches(&format_source_io_error_warning(Devid::new(2)))
                .count(),
            1,
            "real-run stderr must render source I/O warning exactly once, got:\n{stderr}"
        );
        let calls = runner.requests();
        let stats_requests = calls
            .iter()
            .filter(|r| matches!(r, CmdRequest::BtrfsDeviceStatsJson { .. }))
            .count();
        assert_eq!(
            stats_requests, 1,
            "source stats must be probed only by plan_replace, got {calls:?}"
        );
    }

    // Intent: cmd_replace returns the same-name validation error without
    // rendering preserved preflight notes.
    // Why it exists: the pre-hoist preserved-note behavior was intentionally
    // retired; pure input-shape errors now abort before I/O-precondition
    // context can be gathered or rendered.
    // Scenario: user runs `braid replace --old disk2 --new disk2=/...`; the
    // command returns the validation error and stderr has no busy-op note.
    #[test]
    fn cmd_replace_old_equals_new_aborts_before_any_probe() {
        // KEEP the panic runner, filesystem, and dev-info reader -- the
        // assertion is precisely that no injected probe boundary runs before
        // validation. PoolFixture::empty supplies the temp dirs + config and
        // ReplaceParamsBuilder constructs identical params, with
        // passphrase_file=None preserved.
        let f = PoolFixture::empty();
        let (result, stderr) = super::replace_stderr_capture::capture(|| {
            super::cmd_replace(
                &PanicRunner,
                &PanicFilesystem,
                &PanicBtrfsDevInfo,
                &f.replace_params()
                    .new_disk("disk2=/dev/disk/by-id/virtio-disk2")
                    .passphrase_file(None)
                    .build(),
            )
        });

        match &result {
            Err(ReplaceError::Validation(msg)) => {
                assert!(
                    msg.contains("--old and --new must be different"),
                    "expected same-name refusal wording, got: {msg}"
                );
            }
            Err(other) => panic!("expected Validation, got: {other:?}"),
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
        }
        assert!(
            !stderr.contains("waiting for in-flight"),
            "same-name validation must not render busy-op notes, got:\n{stderr}",
        );
        assert!(
            stderr.is_empty(),
            "expected no preserved notes, got:\n{stderr}"
        );
        assert_eq!(f.inhibitor.acquire_count(), 0);
    }

    // -----------------------------------------------------------------
    // Phase 3c -- UUID identity migration test surface (seeds 600-699)
    // -----------------------------------------------------------------

    /// Build a `PoolMembership` from `(uuid, member)` pairs by routing
    /// through `PoolMembership::insert` so the four-axis uniqueness
    /// invariant is exercised at fixture-construction time. Mirrors the
    /// remove.rs / add.rs test fixture pattern.
    fn membership_from(pairs: Vec<(LuksUuid, membership::DiskMember)>) -> PoolMembership {
        let mut m = PoolMembership::empty();
        for (uuid, member) in pairs {
            m.insert(uuid, member).expect("test seed inserts uniquely");
        }
        m
    }

    /// Seed 600: missing-path decoy regression for `--old <name>` ->
    /// UUID resolution. Pool membership has two entries with intentionally
    /// confusing presentation:
    ///   - U_R -> { name: "misleading-label", by_id: "/dev/disk/by-id/right", devid: Some(2) }
    ///   - U_D -> { name: "decoy", by_id: "/dev/disk/by-id/misleading-label", devid: Some(99) }
    ///
    /// btrfs reports `missing_devids = [2]`. A buggy by-id-keyed lookup
    /// would chase the basename "misleading-label" inside U_D's by-id;
    /// the UUID-keyed model must select U_R via `by_name(&"misleading-label")`
    /// and confirm the persisted devid (2) appears in `missing_devids`.
    /// Pinned by Test Plan section "Missing-path `replace` decoy regression".
    //
    // Intent: name-to-UUID resolution at the boundary picks the member by
    //   `DiskName`, not by by-id basename. Persisted-devid cross-check
    //   confirms the resolved UUID names a member whose devid appears in
    //   btrfs missing_devids; the journal records `old_uuid = U_R`.
    // Why it exists: a regression that reverted to by-id-keyed lookup on
    //   the missing path would silently select U_D (whose by-id basename
    //   matches the typed name) and corrupt pool.json.
    // Scenario: operator runs
    //   `braid replace --old misleading-label --new replacement=/dev/disk/by-id/new`
    //   with the decoy membership and one missing devid.
    #[test]
    fn replace_missing_path_decoy_regression_resolves_by_name_to_uuid() {
        let u_r = LuksUuid::parse("aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaa0600").unwrap();
        let u_d = LuksUuid::parse("bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbb0601").unwrap();
        let member_r = membership::DiskMember {
            name: disk_name("misleading-label"),
            by_id: ByIdPath::parse("/dev/disk/by-id/right").unwrap(),
            devid: Some(Devid::new(2)),
            added_at: None,
        };
        let member_d = membership::DiskMember {
            name: disk_name("decoy"),
            by_id: ByIdPath::parse("/dev/disk/by-id/misleading-label").unwrap(),
            devid: Some(Devid::new(99)),
            added_at: None,
        };
        let pre = membership_from(vec![(u_r.clone(), member_r), (u_d.clone(), member_d)]);
        // Pool reports devid 2 as the one missing device. No live device
        // has the U_R or U_D UUID -- the live arm of resolve_replace_source
        // must NOT match (Pattern 4 find by UUID), and resolution must
        // flow to the missing-path branch.
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-keeper".into()),
                luks_uuid: LuksUuid::parse("cccccccc-cccc-cccc-cccc-cccccccc0602").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/vda".into(),
            }],
            missing_count: 1,
            missing_devids: vec![Devid::new(2)],
            total_devices: 2,
            fsid: None,
            null_underlying: vec![],
        };
        let target_name = disk_name("misleading-label");
        let (resolved_uuid, resolved_member) = pre
            .by_name(&target_name)
            .expect("name resolution must find U_R");
        assert_eq!(
            resolved_uuid, &u_r,
            "name 'misleading-label' must resolve to U_R, not U_D"
        );
        assert_eq!(
            resolved_member.devid,
            Some(Devid::new(2)),
            "U_R's persisted devid must be 2 (the missing devid)"
        );
        let source =
            resolve_replace_source(&target_name, resolved_uuid, resolved_member, None, &pool)
                .expect("missing-path resolution must succeed for U_R");
        match source {
            ReplaceSource::Missing { devid } => assert_eq!(devid, Devid::new(2)),
            other => panic!("expected ReplaceSource::Missing {{ devid: 2 }}, got {other:?}"),
        }
        // Build target_membership the way plan_replace does and assert
        // U_R is gone, U_D is unchanged, a fresh UUID is inserted under
        // the new name.
        let new_uuid = LuksUuid::new_v4();
        assert_ne!(
            new_uuid, u_r,
            "freshly generated UUID must not collide with U_R"
        );
        assert_ne!(
            new_uuid, u_d,
            "freshly generated UUID must not collide with U_D"
        );
        let mut target_membership = pre.clone();
        target_membership.remove_by_uuid(&u_r);
        target_membership
            .insert(
                new_uuid.clone(),
                membership::DiskMember {
                    name: disk_name("replacement"),
                    by_id: ByIdPath::parse("/dev/disk/by-id/new").unwrap(),
                    devid: None,
                    added_at: None,
                },
            )
            .expect("insert new replacement member");
        assert!(target_membership.by_uuid(&u_r).is_none(), "U_R is removed");
        assert!(
            target_membership.by_uuid(&u_d).is_some(),
            "U_D entry unchanged"
        );
        assert!(
            target_membership.by_uuid(&new_uuid).is_some(),
            "new UUID inserted"
        );
    }

    /// Seed 610: Pattern 4 observed-mapper journaling regression. When
    /// the matching `PoolDevice.mapper` has drifted ("braid-WRONG")
    /// between plan and the in-process post-commit close, the journaled
    /// `ReplaceJournalSource::Live.old_mapper` MUST clone the observed
    /// value, NOT reconstruct via `mapper_name(&old_name)`. This pins
    /// Pattern 4's "close observed, not reconstructed" doctrine at the
    /// planning site.
    //
    // Intent: resolve_replace_source returns the observed mapper.
    // Why: a regression that journaled `mapper_name(&right)` would
    //   make the post-commit close target the wrong dm slot when the
    //   operator drifted the mapper between plan and execute.
    // Scenario: pool has one device whose mapper is "braid-WRONG" but
    //   whose luks_uuid matches `old_uuid`; membership records the
    //   same UUID under name "right".
    #[test]
    fn replace_live_observed_mapper_journaling_regression() {
        let u_old = LuksUuid::parse("dddddddd-dddd-dddd-dddd-dddddddd0610").unwrap();
        let member = membership::DiskMember {
            name: disk_name("right"),
            by_id: ByIdPath::parse("/dev/disk/by-id/virtio-right").unwrap(),
            devid: Some(Devid::new(7)),
            added_at: None,
        };
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-WRONG".into()),
                luks_uuid: u_old.clone(),
                devid: Devid::new(7),
                underlying: "/dev/vdz".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };
        let source = resolve_replace_source(&disk_name("right"), &u_old, &member, None, &pool)
            .expect("Pattern 4 find by UUID must succeed");
        match source.clone() {
            ReplaceSource::Live { mapper, devid } => {
                assert_eq!(
                    mapper.as_str(),
                    "braid-WRONG",
                    "observed mapper must be cloned, not reconstructed"
                );
                assert_eq!(devid, Devid::new(7));
            }
            other => panic!("expected ReplaceSource::Live, got {other:?}"),
        }
        // build_replace_journal_source propagates the observed mapper
        // into ReplaceJournalSource::Live.old_mapper.
        let journal_source = build_replace_journal_source(&source);
        assert_eq!(
            journal_source,
            journal::ReplaceJournalSource::Live {
                old_devid: Devid::new(7),
                old_mapper: MapperName::from_basename("braid-WRONG".into()),
            }
        );
    }

    /// Build a recording `MockRunner` that injects a `CryptsetupLuksUuid`
    /// probe response for `device`, returning the supplied canned
    /// `RawCommandOutput`. Used by the open-boundary re-probe tests
    /// (`replace_existing_luks_open_boundary_probe_*`).
    fn runner_with_luks_uuid_probe(device: &'static str, canned: RawCommandOutput) -> MockRunner {
        MockRunner::default().with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: device.to_owned(),
            },
            canned,
        )
    }

    fn runner_with_active_mapper_uuid(
        mapper: &'static str,
        backing_device: &'static str,
        canned: RawCommandOutput,
    ) -> MockRunner {
        MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename(mapper.to_owned()),
                },
                mock_ok(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {backing_device}\n  mode:    read/write\n"
                    ),
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: backing_device.to_owned(),
                },
                canned,
            )
    }

    #[test]
    // Intent: the pre-journal new-target preflight dispatcher is a no-op for
    //   FreshLuks targets -- it issues no probe and returns Ok(()).
    // Why it exists: a FreshLuks new target has no pre-existing LUKS identity
    //   to probe -- its journaled UUID is minted by `cryptsetup luksFormat`
    //   AFTER the journal and is gated at finish-time/recovery instead. Pinning
    //   the skip structure-insensitively (zero requests) keeps a future edit
    //   from routing FreshLuks through an ExistingLuks probe and reading a disk
    //   that has no header yet. The ExistingLuks closed/open routing is already
    //   covered by the helper unit tests below
    //   (replace_existing_luks_open_boundary_probe_* and
    //   replace_existing_luks_open_mapper_backing_*); this only pins the
    //   FreshLuks-skip branch and the dispatch.
    // Scenario: the planner classified the new disk as FreshLuks (blank or
    //   non-LUKS disk to be formatted); execute calls the pre-journal gate.
    fn preflight_dispatcher_fresh_luks_is_noop() {
        let runner = MockRunner::default();
        let target_prep = ReplaceTargetPrep::FreshLuks {
            extra_opts: LuksFormatExtraOpts::default(),
            enroll_key_file: None,
        };
        let resolver = MockBackingPathResolver::default();
        let result = verify_existing_luks_new_target_preflight(
            &runner,
            &target_prep,
            &disk_name("disk3"),
            &MapperName::from_basename("braid-disk3".into()),
            &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            &LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap(),
            &resolver,
        );
        assert!(
            result.is_ok(),
            "FreshLuks preflight must be a no-op Ok(()), got: {result:?}"
        );
        assert!(
            runner.requests().is_empty(),
            "FreshLuks preflight must issue zero probes, got: {:?}",
            runner.requests()
        );
    }

    /// Seed 630: ExistingLuks new-target open-boundary re-probe
    /// mismatch arm. A disk-swap between planning and execution
    /// (operator moves the by-id slot to a foreign LUKS volume)
    /// returns `U_FOREIGN`. `probe_existing_luks_new_target_uuid`
    /// aborts with a structured error naming the by-id, expected
    /// UUID, and observed UUID. Pinned by Test Plan section
    /// "Replace ExistingLuks new-target open-boundary UUID re-probe".
    //
    // Intent: probe_existing_luks_new_target_uuid returns
    //   NewTargetUuidMismatchAtOpen on mismatch.
    // Why: closes the open-boundary swap hazard before any
    //   CryptsetupLuksOpen or BtrfsReplaceStart hits the foreign disk.
    // Scenario: op-level new_uuid = U_NEW; probe at
    //   /dev/disk/by-id/Y returns U_FOREIGN.
    #[test]
    fn replace_existing_luks_open_boundary_probe_mismatch_aborts() {
        let u_new = LuksUuid::parse("22222222-2222-2222-2222-222222220630").unwrap();
        let u_foreign = LuksUuid::parse("33333333-3333-3333-3333-333333330631").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/Y").unwrap();
        let runner = runner_with_luks_uuid_probe(
            "/dev/disk/by-id/Y",
            RawCommandOutput {
                cmd: "cryptsetup luksUUID /dev/disk/by-id/Y".into(),
                stdout: format!("{u_foreign}\n"),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let err = probe_existing_luks_new_target_uuid(&runner, &by_id, &u_new).unwrap_err();
        match err {
            ReplaceError::NewTargetUuidMismatchAtOpen {
                by_id: err_by_id,
                expected,
                observed,
            } => {
                assert_eq!(err_by_id, by_id);
                assert_eq!(expected, u_new);
                assert_eq!(observed, u_foreign.as_str().to_owned());
            }
            other => panic!("expected NewTargetUuidMismatchAtOpen, got: {other:?}"),
        }
        let requests = runner.requests();
        assert_eq!(
            requests.len(),
            1,
            "exactly one probe must run before the abort"
        );
        assert!(
            matches!(&requests[0], CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/disk/by-id/Y"),
            "probe must target the new target's by-id form: {:?}",
            requests[0],
        );
        // No open / replace start can have issued on the abort path.
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksOpen { .. })),
            "no CryptsetupLuksOpen may issue on the mismatch path"
        );
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "no BtrfsReplaceStart may issue on the mismatch path"
        );
    }

    /// Seed 631: Control arm for the ExistingLuks open-boundary
    /// re-probe. When the live UUID at by-id matches `new_uuid`,
    /// `probe_existing_luks_new_target_uuid` returns Ok and execution
    /// proceeds to `ensure_luks_open`.
    //
    // Intent: probe match returns Ok(()); fail-safe-skip-only semantic.
    // Why: a future change to gate any condition (e.g. fail on any
    //   probe call) would flip this assertion.
    // Scenario: op-level new_uuid = U_NEW; probe at by-id returns U_NEW.
    #[test]
    fn replace_existing_luks_open_boundary_probe_match_continues() {
        let u_new = LuksUuid::parse("44444444-4444-4444-4444-444444440631").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/Y").unwrap();
        let runner = runner_with_luks_uuid_probe(
            "/dev/disk/by-id/Y",
            RawCommandOutput {
                cmd: "cryptsetup luksUUID /dev/disk/by-id/Y".into(),
                stdout: format!("{u_new}\n"),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        probe_existing_luks_new_target_uuid(&runner, &by_id, &u_new)
            .expect("matched probe must return Ok(())");
    }

    /// Seed 632: already-open ExistingLuks replace target backing-path
    /// mismatch arm. A cloned LUKS header can make UUIDs match, so the
    /// open mapper must be rejected when its canonical backing path differs
    /// from the configured by-id target.
    //
    // Intent: verify_existing_luks_open_mapper_target returns
    //   NewTargetMapperBackingMismatch before replace can start.
    // Why: closes the cloned-header hole in the mapper_open=true path.
    // Scenario: /dev/disk/by-id/Y resolves to /dev/vdb, but braid-disk3 is
    //   already open against /dev/vdz.
    #[test]
    fn replace_existing_luks_open_mapper_backing_mismatch_aborts() {
        let u_new = LuksUuid::parse("55555555-5555-5555-5555-555555550632").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/Y").unwrap();
        let runner = runner_with_active_mapper_uuid(
            "braid-disk3",
            "/dev/vdz",
            RawCommandOutput {
                cmd: "cryptsetup luksUUID /dev/vdz".into(),
                stdout: format!("{u_new}\n"),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let resolver =
            MockBackingPathResolver::default().with_path("/dev/disk/by-id/Y", "/dev/vdb");

        let err = verify_existing_luks_open_mapper_target(
            &runner,
            &disk_name("disk3"),
            &MapperName::from_basename("braid-disk3".into()),
            &by_id,
            &u_new,
            &resolver,
        )
        .unwrap_err();

        let rendered = err.to_string();
        assert!(
            rendered.contains("sudo cryptsetup close braid-disk3"),
            "remediation must name the resolved mapper, got: {rendered}"
        );
        match err {
            ReplaceError::NewTargetMapperBackingMismatch {
                by_id: err_by_id,
                mapper,
                expected_path,
                found_path,
            } => {
                assert_eq!(err_by_id, by_id);
                assert_eq!(mapper, MapperName::from_basename("braid-disk3".into()));
                assert_eq!(expected_path, "/dev/vdb");
                assert_eq!(found_path, "/dev/vdz");
            }
            other => panic!("expected NewTargetMapperBackingMismatch, got: {other:?}"),
        }
        let requests = runner.requests();
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksOpen { .. })),
            "no CryptsetupLuksOpen may issue on the backing-mismatch path"
        );
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "no BtrfsReplaceStart may issue on the backing-mismatch path"
        );
    }

    /// Seed 633: control arm for the already-open ExistingLuks replace
    /// target check. Matching canonical backing path and UUID allow
    /// execution to proceed.
    //
    // Intent: verify_existing_luks_open_mapper_target returns Ok.
    // Why: the new path check must not reject the healthy already-open case.
    // Scenario: /dev/disk/by-id/Y resolves to the same /dev/vdb backing that
    //   cryptsetup status reports for braid-disk3.
    #[test]
    fn replace_existing_luks_open_mapper_backing_match_continues() {
        let u_new = LuksUuid::parse("66666666-6666-6666-6666-666666660633").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/Y").unwrap();
        let runner = runner_with_active_mapper_uuid(
            "braid-disk3",
            "/dev/vdb",
            RawCommandOutput {
                cmd: "cryptsetup luksUUID /dev/vdb".into(),
                stdout: format!("{u_new}\n"),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let resolver =
            MockBackingPathResolver::default().with_path("/dev/disk/by-id/Y", "/dev/vdb");

        verify_existing_luks_open_mapper_target(
            &runner,
            &disk_name("disk3"),
            &MapperName::from_basename("braid-disk3".into()),
            &by_id,
            &u_new,
            &resolver,
        )
        .expect("matching backing path and UUID must continue");
    }

    /// Seed 634: already-open ExistingLuks replace target backing-path
    /// resolve-error arm. Resolver failures must stay distinct from both
    /// UUID mismatch and backing mismatch.
    //
    // Intent: maps MapperOwnershipFailure::BackingPathResolveError to the
    //   replace-specific NewTargetMapperBackingResolveError variant.
    // Why: stale by-id/udev failures have different operator remediation.
    // Scenario: cryptsetup status sees braid-disk3, but canonicalizing the
    //   configured replace target by-id path fails.
    #[test]
    fn replace_existing_luks_open_mapper_backing_resolve_error_aborts() {
        let u_new = LuksUuid::parse("77777777-7777-7777-7777-777777770634").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/Y").unwrap();
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName::from_basename("braid-disk3".into()),
            },
            mock_ok(
                "cryptsetup status braid-disk3",
                "braid-disk3 is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n",
            ),
        );
        let resolver = MockBackingPathResolver::default()
            .with_error("/dev/disk/by-id/Y", std::io::ErrorKind::NotFound);

        let err = verify_existing_luks_open_mapper_target(
            &runner,
            &disk_name("disk3"),
            &MapperName::from_basename("braid-disk3".into()),
            &by_id,
            &u_new,
            &resolver,
        )
        .unwrap_err();
        let msg = err.to_string();

        match err {
            ReplaceError::NewTargetMapperBackingResolveError {
                by_id: err_by_id,
                resolved,
                source_message,
            } => {
                assert_eq!(err_by_id, by_id);
                assert_eq!(resolved, "/dev/disk/by-id/Y");
                assert!(source_message.contains("mock canonicalize error"));
            }
            other => panic!("expected NewTargetMapperBackingResolveError, got: {other:?}"),
        }
        assert!(
            msg.contains("check that the disk is plugged in"),
            "resolver remediation missing from Display: {msg}"
        );
        let requests = runner.requests();
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "no BtrfsReplaceStart may issue on the resolve-error path"
        );
    }

    /// Seed 635: already-open ExistingLuks replace target UUID-mismatch
    /// arm. When the open mapper's backing kernel path canonicalizes to
    /// the configured by-id target but the backing device's LUKS UUID
    /// disagrees with the journaled `new_uuid`, the classifier returns
    /// `MapperOwnershipFailure::Conflict` and `verify_existing_luks_open_mapper_target`
    /// maps it to `NewTargetUuidMismatchAtOpen` before any
    /// `BtrfsReplaceStart`.
    //
    // Intent: verify_existing_luks_open_mapper_target maps
    //   MapperOwnershipFailure::Conflict to NewTargetUuidMismatchAtOpen on the
    //   mapper_open=true path, with no replace mutation issued.
    // Why: pins the only untested arm of the 4-arm OwnershipError ->
    //   ReplaceError map in `verify_existing_luks_open_mapper_target`;
    //   collapsing Conflict into Validation or BackingPathMismatch would
    //   otherwise pass.
    // Scenario: /dev/disk/by-id/Y and the live backing /dev/vdf both
    //   canonicalize to /dev/vdf (path check passes), but
    //   cryptsetup luksUUID /dev/vdf returns U_FOREIGN != U_NEW.
    #[test]
    fn replace_existing_luks_open_mapper_backing_uuid_mismatch_aborts() {
        let u_new = LuksUuid::parse("88888888-8888-8888-8888-888888880635").unwrap();
        let u_foreign = LuksUuid::parse("99999999-9999-9999-9999-999999990636").unwrap();
        let by_id = ByIdPath::parse("/dev/disk/by-id/Y").unwrap();
        let runner = runner_with_active_mapper_uuid(
            "braid-disk3",
            "/dev/vdf",
            RawCommandOutput {
                cmd: "cryptsetup luksUUID /dev/vdf".into(),
                stdout: format!("{u_foreign}\n"),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let resolver =
            MockBackingPathResolver::default().with_path("/dev/disk/by-id/Y", "/dev/vdf");

        let err = verify_existing_luks_open_mapper_target(
            &runner,
            &disk_name("disk3"),
            &MapperName::from_basename("braid-disk3".into()),
            &by_id,
            &u_new,
            &resolver,
        )
        .unwrap_err();

        match err {
            ReplaceError::NewTargetUuidMismatchAtOpen {
                by_id: err_by_id,
                expected,
                observed,
            } => {
                assert_eq!(err_by_id, by_id);
                assert_eq!(expected, u_new);
                assert_eq!(observed, u_foreign.as_str().to_owned());
            }
            other => panic!("expected NewTargetUuidMismatchAtOpen, got: {other:?}"),
        }
        let requests = runner.requests();
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksOpen { .. })),
            "no CryptsetupLuksOpen may issue on the UUID-mismatch path"
        );
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "no BtrfsReplaceStart may issue on the UUID-mismatch path"
        );
    }

    /// Seed 640: Pre-journal-write `new_uuid` uniqueness assert,
    /// Membership scope. A `new_uuid` that already exists in
    /// `PoolMembership` (under a different `old_uuid`) is refused
    /// before any journal write or `CryptsetupLuksFormat`.
    //
    // Intent: assert_new_uuid_unique returns
    //   DuplicateUuid { scope: Membership } when the candidate UUID
    //   exists in membership under a different key.
    // Why: protects the UUID-uniqueness invariant against operator
    //   actions that surface a colliding UUID between commands.
    // Scenario: new_uuid happens to match an existing membership UUID
    //   (other than old_uuid); planning aborts pre-write.
    #[test]
    fn replace_pre_write_uniqueness_membership_scope_collision() {
        let colliding = LuksUuid::parse("55555555-5555-5555-5555-555555550640").unwrap();
        let old_uuid = LuksUuid::parse("66666666-6666-6666-6666-666666660641").unwrap();
        let member = membership::DiskMember {
            name: disk_name("clash"),
            by_id: ByIdPath::parse("/dev/disk/by-id/clash").unwrap(),
            devid: Some(Devid::new(9)),
            added_at: None,
        };
        let pre = membership_from(vec![
            (
                old_uuid.clone(),
                membership::DiskMember {
                    name: disk_name("oldname"),
                    by_id: ByIdPath::parse("/dev/disk/by-id/old").unwrap(),
                    devid: Some(Devid::new(7)),
                    added_at: None,
                },
            ),
            (colliding.clone(), member),
        ]);
        let pool = PoolState {
            mounted: true,
            devices: vec![],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 0,
            fsid: None,
            null_underlying: vec![],
        };
        let err = assert_new_uuid_unique(&colliding, &old_uuid, &pre, &pool).unwrap_err();
        match err {
            ReplaceError::DuplicateUuid { uuid, scope } => {
                assert_eq!(uuid, colliding);
                assert_eq!(scope, DuplicateUuidScope::Membership);
                assert_eq!(scope.to_string(), "membership");
            }
            other => panic!("expected DuplicateUuid {{ Membership }}, got: {other:?}"),
        }
    }

    /// Seed 641: Pre-journal-write `new_uuid` uniqueness assert,
    /// LivePool scope. A `new_uuid` observed in `pool.devices` (but
    /// not in membership) is refused before any journal write or
    /// `CryptsetupLuksFormat`.
    //
    // Intent: assert_new_uuid_unique returns
    //   DuplicateUuid { scope: LivePool } when the candidate UUID
    //   appears in the live pool devices.
    // Why: planning-time observation of a colliding live UUID must
    //   refuse before kernel state is mutated.
    // Scenario: live pool has a device with U_FOREIGN; replacing
    //   `--old <name>` with a fresh disk happens to generate (or
    //   probe) the same U_FOREIGN. Refused.
    #[test]
    fn replace_pre_write_uniqueness_live_pool_scope_collision() {
        let colliding = LuksUuid::parse("77777777-7777-7777-7777-777777770641").unwrap();
        let old_uuid = LuksUuid::parse("88888888-8888-8888-8888-888888880642").unwrap();
        let pre = membership_from(vec![(
            old_uuid.clone(),
            membership::DiskMember {
                name: disk_name("oldname"),
                by_id: ByIdPath::parse("/dev/disk/by-id/old").unwrap(),
                devid: Some(Devid::new(11)),
                added_at: None,
            },
        )]);
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-foreign".into()),
                luks_uuid: colliding.clone(),
                devid: Devid::new(22),
                underlying: "/dev/foreign".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };
        let err = assert_new_uuid_unique(&colliding, &old_uuid, &pre, &pool).unwrap_err();
        match err {
            ReplaceError::DuplicateUuid { uuid, scope } => {
                assert_eq!(uuid, colliding);
                assert_eq!(scope, DuplicateUuidScope::LivePool);
                assert_eq!(scope.to_string(), "live_pool");
            }
            other => panic!("expected DuplicateUuid {{ LivePool }}, got: {other:?}"),
        }
    }

    /// Seed 642: Old-UUID exclusion. The membership scope check
    /// EXCLUDES `old_uuid` (which is being replaced), so a fresh
    /// `new_uuid == old_uuid` (degenerate but defendable case) does
    /// not trip the Membership-scope refusal. Sanity check on the
    /// gate ordering.
    //
    // Intent: assert_new_uuid_unique tolerates `new_uuid == old_uuid`
    //   for the Membership-scope check (excluding the row being
    //   replaced).
    // Why: a regression that removed the `new_uuid != old_uuid` guard
    //   would deadlock the gate.
    #[test]
    fn replace_pre_write_uniqueness_excludes_old_uuid() {
        let old_uuid = LuksUuid::parse("99999999-9999-9999-9999-999999990642").unwrap();
        let pre = membership_from(vec![(
            old_uuid.clone(),
            membership::DiskMember {
                name: disk_name("oldname"),
                by_id: ByIdPath::parse("/dev/disk/by-id/old").unwrap(),
                devid: Some(Devid::new(11)),
                added_at: None,
            },
        )]);
        let pool = PoolState {
            mounted: true,
            devices: vec![],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 0,
            fsid: None,
            null_underlying: vec![],
        };
        assert_new_uuid_unique(&old_uuid, &old_uuid, &pre, &pool)
            .expect("old_uuid exclusion must let new_uuid == old_uuid pass the membership scope");
    }

    /// Seed 650: `--luks-format-arg` rejection covers `replace` for the
    /// shared validation surface. A category sample proves wiring through
    /// `ReplaceError::ManagedFormatFlag`.
    //
    // Intent: plan_replace fails closed on a braid-managed identity or
    //   storage-model-breaking token in `--luks-format-arg` before any
    //   probe, journal write, inhibitor acquisition, or format request.
    // Why: pinning the rejection at the planner boundary mirrors the
    //   `add.rs` contract and protects the LUKS identity, passphrase path,
    //   keyslot layout, header placement, LUKS type, and modeled integrity
    //   mode from user override.
    // Scenario: operator passes `--luks-format-arg=--header=/tmp/header`.
    #[test]
    fn plan_replace_rejects_managed_format_flag() {
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
            let state_tmp = tempfile::tempdir().unwrap();
            let paths = StatePaths::custom(state_tmp.path().into());
            let inhibitor = crate::inhibit::RecordingInhibitor::new();
            let confirm = crate::confirm::RecordingConfirm::new();
            let bad = vec![token.to_owned()];
            let result = super::plan_replace(
                &PanicRunner,
                &PanicFilesystem,
                &PanicBtrfsDevInfo,
                &ReplaceParams {
                    config: &test_config(),
                    old_name: "disk2",
                    new_name: "disk3=/dev/disk/by-id/virtio-disk3",
                    missing_id: None,
                    dry_run: true,
                    yes: true,
                    passphrase_stdin: false,
                    passphrase_file: None,
                    enroll_key_file: None,
                    luks_format_extra_opts: &bad,
                    progress: crate::progress::ProgressOutput::Off,
                    paths: &paths,
                    sleep_inhibitor: &inhibitor,
                    confirm: &confirm,
                    sleeper: &crate::progress::NoopSleeper,
                    backing_path_resolver:
                        crate::test_fixtures::mock_virtio_offset_backing_path_resolver(),
                },
            );
            match result {
                Err(PlanFailure {
                    error:
                        ReplaceError::ManagedFormatFlag(
                            crate::types::LuksFormatExtraOptsError::ManagedFormatFlag { token: t },
                        ),
                    ..
                }) => {
                    assert_eq!(t, token, "token must be the offending input verbatim");
                }
                Err(PlanFailure { error, .. }) => {
                    panic!("expected ManagedFormatFlag refusal for {token:?}, got: {error:?}")
                }
                Ok(_) => panic!("expected ManagedFormatFlag refusal for {token:?}, got Ok(_)"),
            }
            assert_eq!(
                inhibitor.acquire_count(),
                0,
                "managed-flag rejection must fire before the inhibitor seam for {token:?}"
            );
            assert!(
                journal::load_journal(&paths).unwrap().is_none(),
                "managed-flag rejection must not write a journal for {token:?}"
            );
        }
    }

    /// Seed 660: Positive-extras forwarding. A valid
    /// `--luks-format-arg=--use-random` flows through
    /// `CryptsetupLuksFormat.extra_opts` unchanged, in argv order, and
    /// does NOT leak into the structured `uuid` or `label` fields.
    /// Pinned by Test Plan section "Positive-extras forwarding".
    //
    // Intent: structured `extra_opts` carries user-supplied non-managed
    //   tokens verbatim; managed identity flows through the structured
    //   `uuid` and `label` fields.
    // Why: a regression that silently dropped accepted extras at
    //   execute time would pass parse and pass the empty-extras suite
    //   but fail this test.
    // Scenario: live replace of disk2 -> disk3 with one extra token.
    #[test]
    fn cmd_replace_forwards_positive_luks_format_extra_to_request() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![
            "/dev/disk/by-id/virtio-disk3".into(),
            "/dev/mapper/braid-disk3".into(),
        ]);
        let replace_done = Arc::new(AtomicBool::new(false));
        // Force replace to fail at BtrfsReplaceStart so we don't have to
        // mock the full post-commit chain; the assertion targets the
        // structured CryptsetupLuksFormat that already shipped.
        let runner = ReplacementPool::two_disk_healthy()
            .with_mapper_closed("braid-disk3")
            .install(MockRunner::default(), replace_done)
            .with_handler(|req| match req {
                CmdRequest::CryptsetupLuksUuid { device }
                    if device == "/dev/disk/by-id/virtio-disk3" =>
                {
                    Some(Ok(RawCommandOutput {
                        cmd: format!("cryptsetup luksUUID {device}"),
                        stdout: String::new(),
                        stderr: "Device is not a valid LUKS device.\n".into(),
                        exit_status: 1,
                    }))
                }
                CmdRequest::CryptsetupLuksFormat { device, .. } => {
                    Some(Ok(mock_ok(&format!("cryptsetup luksFormat {device}"), "")))
                }
                _ => None,
            });
        let extras = vec!["--use-random".to_owned()];
        let _ = cmd_replace(
            &runner,
            &fs,
            &f.replace_params().luks_format_extra_opts(&extras).build(),
        );

        let log = runner.requests();
        let fmt_idx = log
            .iter()
            .position(|r| matches!(r, CmdRequest::CryptsetupLuksFormat { .. }))
            .expect("CryptsetupLuksFormat must be issued for fresh replace");
        let CmdRequest::CryptsetupLuksFormat {
            uuid,
            label,
            extra_opts,
            ..
        } = &log[fmt_idx]
        else {
            unreachable!("position predicate matched");
        };
        assert_eq!(
            extra_opts.as_slice(),
            &["--use-random".to_owned()],
            "user-supplied extras must round-trip through extra_opts"
        );
        assert_eq!(
            label.as_str(),
            "braid-disk3",
            "structured label is derived at boundary"
        );
        assert!(
            uuid::Uuid::parse_str(uuid.as_str()).is_ok(),
            "structured uuid carries the journaled identity"
        );
    }
}
