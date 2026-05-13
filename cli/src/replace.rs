use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::{Config, config_read, mapper_name, name_from_mapper};
use crate::confirm;
use crate::credential_verify::{
    Credential, CredentialVerifyError, CredentialVerifyTarget, verify_credential_for_targets,
};
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal;
use crate::luks::{
    backup_luks_header_post_mutation, ensure_luks_open, format_keyfile_asymmetry_warning,
    format_keyfile_enrollment_probe_failure, luks_format, probe_pool_keyfile_enrollment,
    read_passphrase,
};
use crate::mapper_close::close_mapper_best_effort;
use crate::membership::{self, PoolMembership};
use crate::parse::{parse_btrfs_device_stats, parse_cryptsetup_luks_uuid};
use crate::pool::{pool_replace_device, pool_resize_device};
use crate::preflight;
use crate::preview::{self, PerDiskStyle, PlanFailure, Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{Filesystem, ProbeError, probe_config_disk, probe_pool};
use crate::probe_mapper_uuid::probe_observed_mapper_uuid;
use crate::progress::{self, ProgressOutput};
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
    /// Operator-supplied `--old <name>` did not resolve to a member of
    /// the persisted pool membership. The display name they typed has
    /// no `(uuid, member)` entry; planning aborts before any inhibitor,
    /// probe, or journal write.
    #[error(
        "'{name}' not found in pool.json membership -- no disk entry has this name. Pool membership may need manual repair."
    )]
    OldMemberNotFound { name: String },
    /// `--missing-id` cross-check against the old member's persisted
    /// `devid` failed: either the persisted devid is `None`, or the
    /// resolved missing devid does not match the persisted value.
    #[error(
        "--old '{old_name}' records devid {pool_devid:?} in pool.json, but btrfs reports missing devid {observed}. --old and --missing-id disagree about which member is being replaced."
    )]
    OldDevidMismatch {
        old_name: String,
        pool_devid: Option<u64>,
        observed: u64,
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
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] crate::parse::ParseError),
    #[error("membership error: {0}")]
    Membership(#[from] membership::MembershipError),
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
    pub luks_format_extra_opts: &'a [String],
    pub progress: ProgressOutput,
    pub paths: &'a StatePaths,
    /// Seam for acquiring a logind sleep inhibitor before the irreversible
    /// portion of the replace. Production passes `&RealSleepInhibitor`;
    /// unit tests pass `&NoopSleepInhibitor` to avoid spawning subprocesses.
    pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
    /// Sleeper seam for retrying transiently-busy mapper closes without
    /// slowing unit tests.
    pub sleeper: &'a dyn progress::Sleeper,
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
        enroll_key_file: Option<PathBuf>,
        header_backup_path: PathBuf,
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
        enroll_key_file: Option<PathBuf>,
        /// Where the post-enrollment LUKS header backup should land.
        /// Always populated so render_steps can reference it without
        /// re-deriving from `paths` (mirrors the FreshLuks variant).
        /// Unused when `enroll_key_file` is `None`.
        header_backup_path: PathBuf,
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
    target_prep: ReplaceTargetPrep,
    pre_membership: PoolMembership,
    target_membership: PoolMembership,
    journal_target: journal::ReplaceJournalTarget,
    journal_source: journal::ReplaceJournalSource,
    restore_raid1_after_commit: bool,
    new_mapper: MapperName,
    new_mapper_path: String,
}

impl ReplaceWorkPlan {
    fn render_steps(&self) -> Vec<Step> {
        let mut steps = Vec::new();

        match &self.target_prep {
            ReplaceTargetPrep::FreshLuks {
                extra_opts,
                enroll_key_file,
                header_backup_path,
            } => {
                let label = format!("braid-{}", self.new_name);
                steps.push(Step {
                    risk: "destructive",
                    description: format!("LUKS format {}", self.new_by_id),
                    commands: vec![CmdRequest::CryptsetupLuksFormat {
                        device: self.new_by_id.as_str().to_owned(),
                        uuid: self.new_uuid.clone(),
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
                            key_file_path: kf.display().to_string(),
                        }],
                    });
                }
                steps.push(Step {
                    risk: "safe",
                    description: format!("LUKS header backup -> {}", header_backup_path.display()),
                    commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
                        device: self.new_by_id.as_str().to_owned(),
                        backup_path: header_backup_path.display().to_string(),
                    }],
                });
                steps.push(Step {
                    risk: "safe",
                    description: format!("LUKS open -> {}", self.new_mapper),
                    commands: vec![CmdRequest::CryptsetupLuksOpen {
                        device: self.new_by_id.as_str().to_owned(),
                        mapper: self.new_mapper.0.clone(),
                    }],
                });
            }
            ReplaceTargetPrep::ExistingLuks {
                mapper_open,
                enroll_key_file,
                header_backup_path,
            } => {
                if let Some(kf) = enroll_key_file {
                    steps.push(Step {
                        risk: "safe",
                        description: format!("enroll keyfile -> LUKS slot 1 on {}", self.new_by_id),
                        commands: vec![CmdRequest::CryptsetupLuksAddKeyFile {
                            device: self.new_by_id.as_str().to_owned(),
                            key_file_path: kf.display().to_string(),
                        }],
                    });
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "LUKS header backup -> {}",
                            header_backup_path.display()
                        ),
                        commands: vec![CmdRequest::CryptsetupLuksHeaderBackup {
                            device: self.new_by_id.as_str().to_owned(),
                            backup_path: header_backup_path.display().to_string(),
                        }],
                    });
                }
                if !mapper_open {
                    steps.push(Step {
                        risk: "safe",
                        description: format!("LUKS open -> {}", self.new_mapper),
                        commands: vec![CmdRequest::CryptsetupLuksOpen {
                            device: self.new_by_id.as_str().to_owned(),
                            mapper: self.new_mapper.0.clone(),
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
                "btrfs replace start {} /dev/mapper/{} {}",
                devid,
                self.new_mapper,
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
                    mapper: mapper.0.clone(),
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
    /// Real-run and failure-path stderr for `replace` use `Bracketed`
    /// per-disk style to match the canonical dry-run render. `replace`
    /// does not emit `PerDisk` notes today, but the constant keeps the
    /// stderr-note contract uniform with the other migrated commands.
    pub const STDERR_STYLE: PerDiskStyle = PerDiskStyle::Bracketed;

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
            target_prep,
            pre_membership,
            target_membership,
            journal_target: new_target,
            journal_source,
            restore_raid1_after_commit,
            new_mapper: new_mn,
            new_mapper_path,
        } = work_plan;

        // Render accumulated notes to stderr via the shared helper
        // BEFORE any mutation. Matches the other note-carrying commands so
        // preflight diagnostics surface identically across success,
        // failure, and dry-run stdout.
        emit_replace_notes_to_stderr(&notes);

        // Confirm
        if !params.yes {
            let old_underlying = match &replace_source {
                ReplaceSource::Live { mapper, .. } => pool
                    .devices
                    .iter()
                    .find(|d| d.luks_uuid == old_uuid)
                    .map(|d| d.underlying.as_str())
                    .or_else(|| {
                        // Fallback for tests that synthesize a pool whose
                        // observed mapper matches but luks_uuid differs;
                        // identity decisions already flowed via old_uuid.
                        pool.devices
                            .iter()
                            .find(|d| d.mapper == *mapper)
                            .map(|d| d.underlying.as_str())
                    }),
                ReplaceSource::Missing { .. } => None,
            };
            let old_hw = old_underlying.map(|u| confirm::query_disk_hw_info(runner, u));
            let new_hw = confirm::query_disk_hw_info(runner, new_by_id.as_str());
            let is_missing = matches!(&replace_source, ReplaceSource::Missing { .. });

            emit_replace_stderr(&format!(
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
            ));
            if pool.total_devices == 1 {
                emit_replace_stderr(
                    "WARNING: This replace leaves only 1 disk -- no redundancy.\n\n",
                );
            }
            confirm::confirm_yes().map_err(ReplaceError::Validation)?;
        }

        // Read passphrase
        let passphrase = read_passphrase(params.passphrase_file, params.passphrase_stdin)?;

        let retained_members: Vec<_> = match &replace_source {
            ReplaceSource::Live { .. } => pool
                .devices
                .iter()
                .filter(|device| device.luks_uuid != old_uuid)
                .collect(),
            ReplaceSource::Missing { .. } => pool.devices.iter().collect(),
        };
        let anchor_members: Vec<_> = if matches!(&replace_source, ReplaceSource::Live { .. })
            && retained_members.is_empty()
        {
            pool.devices
                .iter()
                .filter(|device| device.luks_uuid == old_uuid)
                .collect()
        } else {
            retained_members
        };
        let mut credential_targets: Vec<CredentialVerifyTarget> = anchor_members
            .into_iter()
            .map(|device| CredentialVerifyTarget {
                // Display-only fallback for credential error messages; replace
                // source identity is resolved by LUKS UUID before this point.
                name: name_from_mapper(&device.mapper.0)
                    .unwrap_or(device.mapper.0.as_str())
                    .to_owned(),
                device: device.underlying.clone(),
            })
            .collect();
        let new_disk_target = match &target_prep {
            ReplaceTargetPrep::ExistingLuks { .. } => Some(CredentialVerifyTarget {
                name: new_name.as_str().to_owned(),
                device: new_by_id.as_str().to_owned(),
            }),
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
                            target.name, target.device
                        )
                    } else {
                        format!(
                            "passphrase does not match existing pool member '{}'",
                            target.name
                        )
                    }));
                }
                Err(CredentialVerifyError::Luks { source, .. }) => {
                    return Err(ReplaceError::Luks(source));
                }
            }
        }

        // Guard: new disk must not already be in the pool.
        check_new_not_in_pool(new_name.as_str(), &new_mn, &pool)?;

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
        journal::write_journal(params.paths, &journal)
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
                let label = format!("braid-{}", new_name);
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
                    &new_mn.0,
                    params.paths,
                )?;
                eprintln!("LUKS header backed up: {}", backup_path.display());

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
                ensure_luks_open(runner, new_name.as_str(), &new_by_id, &passphrase)?;
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
                        &new_mn.0,
                        params.paths,
                    )?;
                    eprintln!("LUKS header backed up: {}", backup_path.display());
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
                    ensure_luks_open(runner, new_name.as_str(), &new_by_id, &passphrase)?;
                    eprint!(
                        "{}",
                        status_line(
                            StatusTag::Ok,
                            color_enabled,
                            &format!("disk {new_name}: unlocked"),
                        )
                    );
                } else if !pool.devices.iter().any(|d| d.mapper == new_mn) {
                    // mapper_open: identity has already been verified by
                    // `verify_credential_for_targets` against `new_by_id`
                    // above, so the by-id re-probe is not required here.
                    eprintln!(
                        "note: LUKS mapper is already open but device is not yet in pool. Completing replace."
                    );
                }
            }
        }

        // Step 2+: Execute replacement -- both paths use btrfs replace start.
        // Live-only: warn if the source device has accumulated I/O errors.
        if let ReplaceSource::Live { mapper: _, devid } = &replace_source {
            let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStatsJson {
                mount_point: config.mount_point().clone(),
            });
            if let Ok(ref raw) = stats_raw
                && let Ok(stats) = parse_btrfs_device_stats(raw)
                && source_has_io_errors(&stats, *devid)
            {
                eprintln!(
                    "Warning: source device (devid {devid}) has I/O errors. \
                     btrfs replace will read from mirrors where possible, \
                     but may fail if any data lacks a healthy mirror copy."
                );
            }
        }

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
        // devid + observed luks_uuid from a fresh probe, then persist before
        // the post-replace cleanup, resize, and (missing-path) soft balance.
        // The journal still covers maintenance, so recovery can replay it if
        // we crash before clear_journal.
        let mut target_membership = target_membership;
        if let Ok(pool_after) = probe_pool(runner, fs, config.mount_point()) {
            // Discard `EnrichmentReport.foreign` for now; doctor/status
            // wiring is Phase 5. The function is fail-closed on
            // foreign-UUID admission (no insert), so the discard is safe.
            let _ = membership::enrich_from_pool_state(&mut target_membership, &pool_after)?;
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
        // to equal the journaled `old_uuid`. On mismatch or probe failure,
        // demote the close to a logged-warning skip so we don't tear down
        // a foreign dm slot that the operator opened under the same
        // mapper between plan and this point. Mirrors the
        // `remove.rs::probe_observed_mapper_uuid` site (Phase 3a) at the
        // same defense-in-depth seam.
        if let journal::OpKind::Replace {
            source:
                journal::ReplaceJournalSource::Live {
                    old_mapper: mapper, ..
                },
            old_uuid: journaled_old_uuid,
            ..
        } = &journal.op
        {
            let old_label = mapper.0.strip_prefix("braid-").unwrap_or(&mapper.0);
            if probe_observed_mapper_uuid(runner, mapper, journaled_old_uuid)
                && close_mapper_best_effort(
                    runner,
                    params.sleeper,
                    &mapper.0,
                    old_label,
                    color_enabled,
                )
            {
                eprintln!(
                    "Old device closed. If repurposing the physical disk, wipe it separately."
                );
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

/// Open-boundary re-probe for `ReplaceJournalMode::ExistingLuks` new
/// targets. The planning-time `cryptsetup luksUUID` probe identifies the
/// physical disk that should be opened; between planning and execution
/// the operator could swap the disk at `new_target.by_id` (USB shuffle,
/// hot-plug into the wrong slot). Re-probing the by-id form right before
/// `ensure_luks_open` rejects the swap so pool data cannot be routed
/// into a foreign LUKS volume by the subsequent `btrfs replace start`.
/// Mismatch and probe failure both abort with
/// `ReplaceError::NewTargetUuidMismatchAtOpen`; the wording mirrors the
/// `finish_uncommitted_replace_recovery` `:2697` recovery arm so
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

/// True iff the stats row identified by `devid` has any non-zero error
/// counter. Pairing is by devid (canonical identity from btrfs), not by
/// mapper path -- the row's path string can differ from the canonical
/// /dev/mapper/braid-X without changing which physical device it describes.
fn source_has_io_errors(stats: &crate::parse::types::BtrfsDeviceStatsOutput, devid: u64) -> bool {
    stats.devices.iter().any(|d| {
        d.devid == devid
            && (d.read_io_errs > 0
                || d.write_io_errs > 0
                || d.flush_io_errs > 0
                || d.corruption_errs > 0
                || d.generation_errs > 0)
    })
}

fn emit_replace_notes_to_stderr(notes: &[PreviewNote]) {
    let rendered =
        render_replace_notes_for_stderr(notes, crate::status_tag::color_enabled_for_stderr());
    emit_replace_stderr(&rendered);
}

fn render_replace_notes_for_stderr(notes: &[PreviewNote], color_enabled: bool) -> String {
    preview::render_notes_for_stderr_with(notes, ReplacePlan::STDERR_STYLE, color_enabled)
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

/// Plan a `braid replace` run. Owns everything above today's `--dry-run`
/// gate: pending-op preflight, config read, `--new` spec parsing,
/// `--old == --new` guard, keyfile path validation, `probe_pool` + mounted
/// validation, mutation/UPS preflight, replace-source resolution,
/// membership load + `build_replacement_membership`, new-disk probe, and
/// step compilation. On success, accumulated notes move into `plan.notes`;
/// on post-preflight failure, accumulated notes stay on `PlanFailure::notes`
/// so `cmd_replace` can render them before returning the error.
///
/// Does not read or verify the passphrase, acquire the sleep inhibitor,
/// or run `check_new_not_in_pool` -- those happen inside
/// `ReplacePlan::execute` so `--dry-run` keeps short-circuiting before
/// them.
pub fn plan_replace<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &ReplaceParams<'_>,
) -> Result<ReplacePlan, PlanFailure<ReplaceError>> {
    // Notes accumulator. Pre-preflight exits have no notes; later exits
    // preserve preflight diagnostics on `PlanFailure::notes`.
    let mut notes: Vec<PreviewNote> = Vec::new();

    if let Err(msg) = preflight::check_no_pending_operation(params.paths) {
        return Err(PlanFailure::empty(ReplaceError::Validation(msg)));
    }

    let config = match config_read(params.config_path) {
        Ok(c) => c,
        Err(e) => return Err(PlanFailure::empty(e.into())),
    };

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
    let fsid = pool.fsid.as_deref().expect("mounted pool must have FSID");
    match preflight::require_mutation_preflight(runner, fs, fsid, config.mount_point()) {
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
        runner,
        &old_name_parsed,
        &old_uuid,
        &old_member,
        params.missing_id,
        &pool,
        config.mount_point(),
    ) {
        Ok(v) => v,
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, e));
        }
    };

    // Probe --new disk state
    let new_probed = match probe_config_disk(runner, fs, &new_name_parsed, &new_by_id) {
        Ok(p) => p,
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, e.into()));
        }
    };

    // Keyfile diagnostics are plan notes, not confirmation-only stderr.
    // This keeps dry-run stdout, real-run stderr, and preserved-error
    // stderr on the same PreviewNote contract used by `add`.
    if matches!(new_probed.state, ConfigDiskState::PresentNotLuks)
        && params.enroll_key_file.is_none()
    {
        let keyfile_probe = probe_pool_keyfile_enrollment(runner, &pool.devices);
        if keyfile_probe.has_enrollment {
            notes.push(PreviewNote::Warn(format_keyfile_asymmetry_warning()));
        } else {
            notes.extend(keyfile_probe.failures.iter().map(|failure| {
                PreviewNote::Warn(format_keyfile_enrollment_probe_failure(failure))
            }));
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
    let resolved_enroll_key_file: Option<PathBuf> =
        match (&new_probed.state, params.enroll_key_file) {
            (ConfigDiskState::PresentLuks { .. }, Some(kf)) => {
                match crate::enroll_key_file::plan_single_disk_enrollment(
                    runner,
                    new_name_str,
                    &new_by_id,
                    kf,
                    crate::enroll_key_file::EnrollmentPlanMode::ExistingKeyfile,
                ) {
                    Ok(crate::enroll_key_file::DiskEnrollAction::AlreadyEnrolled { .. }) => None,
                    Ok(crate::enroll_key_file::DiskEnrollAction::NeedsEnroll { .. }) => {
                        Some(kf.to_path_buf())
                    }
                    Err(e) => {
                        return Err(PlanFailure::with_notes(
                            notes,
                            ReplaceError::Validation(e.to_string()),
                        ));
                    }
                }
            }
            (_, kf) => kf.map(Path::to_path_buf),
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
    //   - Absent: rejected by `build_replace_work_plan`.
    let new_uuid = match &new_probed.state {
        ConfigDiskState::PresentNotLuks => LuksUuid::new_v4(),
        ConfigDiskState::PresentLuks { uuid, .. } => uuid.clone(),
        ConfigDiskState::Absent => {
            return Err(PlanFailure::with_notes(
                notes,
                ReplaceError::Validation(format!(
                    "new disk '{}' ({}) is not present. Is it plugged in?",
                    new_name_str, new_by_id
                )),
            ));
        }
    };

    // Pre-journal-write `new_uuid` uniqueness assert. Refused BEFORE
    // any journal write and BEFORE `CryptsetupLuksFormat`. Membership
    // check excludes `old_uuid` (which is being replaced); live-pool
    // check inspects the planning-time `pool.devices` UUID set.
    if let Err(e) = assert_new_uuid_unique(&new_uuid, &old_uuid, &pre_membership, &pool) {
        return Err(PlanFailure::with_notes(notes, e));
    }

    // Build target_membership through `PoolMembership::insert` so the
    // four-axis uniqueness invariant runs once. Mirrors the
    // `validate_no_conflicts` deletion called out in the plan.
    let mut target_membership = pre_membership.clone();
    target_membership.remove_by_uuid(&old_uuid);
    let new_member = membership::DiskMember {
        name: new_name_parsed.clone(),
        by_id: new_by_id.clone(),
        devid: None,
        added_at: None,
    };
    if let Err(e) = target_membership.insert(new_uuid.clone(), new_member) {
        return Err(PlanFailure::with_notes(notes, e.into()));
    }

    let work_plan = match build_replace_work_plan(ReplaceWorkPlanInput {
        config,
        old_uuid,
        old_name: old_member.name.clone(),
        new_uuid,
        new_name: new_name_parsed,
        new_by_id,
        new_probed,
        replace_source,
        pool,
        pre_membership,
        target_membership,
        paths: params.paths,
        enroll_key_file: resolved_enroll_key_file,
        luks_format_extra_opts,
    }) {
        Ok(plan) => plan,
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, e));
        }
    };

    Ok(ReplacePlan { notes, work_plan })
}

/// Pre-journal-write refusal for a colliding `new_uuid`. Inspects
/// membership keys (excluding `old_uuid`) and the live `pool.devices`
/// UUID set; the first collision returns the structured
/// `ReplaceError::DuplicateUuid`. Membership scope wins ordering ties
/// because the membership-side collision is the more common operator
/// failure mode (a foreign disk attached and discovered between
/// command invocations). Mirrors `add.rs::assert_target_uuid_unique`
/// at the same defense-in-depth seam.
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
    if pool.devices.iter().any(|d| d.luks_uuid == *new_uuid) {
        return Err(ReplaceError::DuplicateUuid {
            uuid: new_uuid.clone(),
            scope: DuplicateUuidScope::LivePool,
        });
    }
    Ok(())
}

pub fn cmd_replace<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &ReplaceParams<'_>,
) -> Result<(), ReplaceError> {
    let plan = match plan_replace(runner, fs, params) {
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
    Live { mapper: MapperName, devid: u64 },
    /// Old disk is missing -- replace via `btrfs replace start` by devid.
    Missing { devid: u64 },
}

struct ReplaceWorkPlanInput<'a> {
    config: Config,
    old_uuid: LuksUuid,
    old_name: DiskName,
    new_uuid: LuksUuid,
    new_name: DiskName,
    new_by_id: ByIdPath,
    new_probed: ConfigDisk,
    replace_source: ReplaceSource,
    pool: PoolState,
    pre_membership: PoolMembership,
    target_membership: PoolMembership,
    paths: &'a StatePaths,
    /// Resolved keyfile decision after `plan_single_disk_enrollment`
    /// for `PresentLuks` targets. `Some(kf)` means an enrollment will
    /// run. `None` covers the no-`--enroll` case AND the idempotent
    /// `AlreadyEnrolled` skip. For `PresentNotLuks` (fresh format) and
    /// `Absent` paths, resolution is a no-op so this carries the raw
    /// user input.
    enroll_key_file: Option<PathBuf>,
    luks_format_extra_opts: LuksFormatExtraOpts,
}

fn build_replace_work_plan(
    input: ReplaceWorkPlanInput<'_>,
) -> Result<ReplaceWorkPlan, ReplaceError> {
    let new_mapper = mapper_name(input.new_name.as_str());
    let new_mapper_path = format!("/dev/mapper/{}", new_mapper.0);
    let journal_target = build_replace_journal_target(
        &input.new_by_id,
        &input.new_probed,
        input.enroll_key_file.as_deref(),
        &input.luks_format_extra_opts,
    )?;
    let journal_source = build_replace_journal_source(&input.replace_source);
    let restore_raid1_after_commit = matches!(&input.replace_source, ReplaceSource::Missing { .. })
        && input.pool.missing_count == 1
        && input.pool.devices.len() + 1 >= 2;
    let target_prep = match input.new_probed.state {
        ConfigDiskState::Absent => {
            return Err(ReplaceError::Validation(format!(
                "new disk '{}' ({}) is not present. Is it plugged in?",
                input.new_name, input.new_by_id
            )));
        }
        ConfigDiskState::PresentNotLuks => ReplaceTargetPrep::FreshLuks {
            extra_opts: input.luks_format_extra_opts.clone(),
            enroll_key_file: input.enroll_key_file.clone(),
            header_backup_path: input
                .paths
                .luks_headers_dir()
                .join(format!("{}.luksheader", new_mapper.0)),
        },
        ConfigDiskState::PresentLuks { mapper_open, .. } => ReplaceTargetPrep::ExistingLuks {
            mapper_open,
            enroll_key_file: input.enroll_key_file.clone(),
            header_backup_path: input
                .paths
                .luks_headers_dir()
                .join(format!("{}.luksheader", new_mapper.0)),
        },
    };

    Ok(ReplaceWorkPlan {
        config: input.config,
        old_uuid: input.old_uuid,
        old_name: input.old_name,
        new_uuid: input.new_uuid,
        new_name: input.new_name,
        new_by_id: input.new_by_id,
        pool: input.pool,
        replace_source: input.replace_source,
        target_prep,
        pre_membership: input.pre_membership,
        target_membership: input.target_membership,
        journal_target,
        journal_source,
        restore_raid1_after_commit,
        new_mapper,
        new_mapper_path,
    })
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
    new_probed: &ConfigDisk,
    enroll_key_file: Option<&Path>,
    luks_format_extra_opts: &LuksFormatExtraOpts,
) -> Result<journal::ReplaceJournalTarget, ReplaceError> {
    let mode = match &new_probed.state {
        ConfigDiskState::PresentNotLuks => journal::ReplaceJournalMode::FreshLuks {
            extra_opts: luks_format_extra_opts.clone(),
            enroll_key_file: enroll_key_file.map(|p| p.to_path_buf()),
        },
        ConfigDiskState::PresentLuks { .. } => journal::ReplaceJournalMode::ExistingLuks {
            enroll_key_file: enroll_key_file.map(|p| p.to_path_buf()),
        },
        ConfigDiskState::Absent => {
            return Err(ReplaceError::Validation(format!(
                "new disk '{}' is not present. Is it plugged in?",
                new_by_id
            )));
        }
    };
    Ok(journal::ReplaceJournalTarget {
        by_id: new_by_id.clone(),
        mode,
    })
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
fn resolve_replace_source<R: CommandRunner>(
    runner: &R,
    old_name: &DiskName,
    old_uuid: &LuksUuid,
    old_member: &membership::DiskMember,
    missing_id: Option<u64>,
    pool: &PoolState,
    mount_point: &MountPoint,
) -> Result<ReplaceSource, ReplaceError> {
    // Pattern 4: find by UUID, not by reconstructed mapper.
    if let Some(matched) = pool.devices.iter().find(|d| d.luks_uuid == *old_uuid) {
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

    // Probe actual missing devids for validation and auto-resolution.
    let missing_devids =
        preflight::probe_missing_devids(runner, mount_point).map_err(ReplaceError::Validation)?;

    let resolved = if let Some(supplied) = missing_id {
        // --missing-id supplied: must equal persisted devid AND appear
        // in the btrfs missing set.
        if supplied != persisted_devid {
            return Err(ReplaceError::OldDevidMismatch {
                old_name: old_name.as_str().to_owned(),
                pool_devid: Some(persisted_devid),
                observed: supplied,
            });
        }
        if pool.devices.iter().any(|d| d.devid == supplied) {
            return Err(ReplaceError::Validation(format!(
                "devid {supplied} is a live device, not a missing one."
            )));
        }
        if !missing_devids.contains(&supplied) {
            return Err(ReplaceError::Validation(format!(
                "devid {supplied} is not a missing device in this pool. \
                 Use 'braid status' to see device IDs."
            )));
        }
        supplied
    } else {
        // Auto-resolve: the persisted devid must be missing in btrfs.
        if missing_devids.is_empty() {
            return Err(ReplaceError::Validation(format!(
                "disk '{}' not found in pool and no missing devices detected.",
                old_name
            )));
        }
        if !missing_devids.contains(&persisted_devid) {
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
    new_probed: &'a ConfigDisk,
    replace_source: &'a ReplaceSource,
    mount_point: &'a MountPoint,
    will_clear_last_missing: bool,
    total_devices: u64,
    paths: &'a StatePaths,
    enroll_key_file: Option<&'a Path>,
    luks_format_extra_opts: &'a [String],
}

#[cfg(test)]
fn replace_work_plan_for_test(
    input: &ReplaceWorkPlanTestInput<'_>,
) -> Result<ReplaceWorkPlan, ReplaceError> {
    let config = Config::new(input.mount_point.clone()).expect("valid test mount point");
    let pool = replace_work_plan_test_pool(
        input.replace_source,
        input.will_clear_last_missing,
        input.total_devices,
    );
    // Synthesize an op-level identity for the test plan. The synthesized
    // `new_uuid` lines up with what the planner would have produced from
    // the probed `ConfigDiskState`: FreshLuks gets a fresh v4, ExistingLuks
    // reuses the probed value.
    let new_uuid = match &input.new_probed.state {
        ConfigDiskState::PresentLuks { uuid, .. } => uuid.clone(),
        _ => LuksUuid::new_v4(),
    };
    let old_uuid = LuksUuid::parse("99999999-9999-9999-9999-999999999999").unwrap();
    let old_name = DiskName::parse("disk2").expect("valid disk name");
    let new_name = DiskName::parse(input.new_name).expect("valid new disk name in test");
    let extra_opts = LuksFormatExtraOpts::parse(input.luks_format_extra_opts)
        .expect("test extras must be valid (no managed flags)");
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
        pre_membership: PoolMembership::empty(),
        target_membership: PoolMembership::empty(),
        paths: input.paths,
        enroll_key_file: input.enroll_key_file.map(Path::to_path_buf),
        luks_format_extra_opts: extra_opts,
    })
}

#[cfg(test)]
fn replace_work_plan_test_pool(
    replace_source: &ReplaceSource,
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
            luks_uuid: synth_test_uuid(*devid),
            devid: *devid,
            underlying: format!("/dev/test-{devid}"),
        });
    }

    let mut next_devid = 100;
    while devices.len() < live_device_count {
        devices.push(PoolDevice {
            mapper: MapperName(format!("braid-test{}", devices.len() + 1)),
            luks_uuid: synth_test_uuid(next_devid),
            devid: next_devid,
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
     * (e.g. /dev/dm-N). Pairing by devid removes that identity weakness.
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
            source_has_io_errors(&stats, 1),
            "devid 1 has read_io_errs=5; mismatched path must not hide it"
        );
        assert!(
            !source_has_io_errors(&stats, 2),
            "devid 2 has zero counters; must not report errors"
        );
        assert!(
            !source_has_io_errors(&stats, 99),
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
                    luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                    devid: 1,
                    underlying: "/dev/vda".into(),
                },
                PoolDevice {
                    mapper: MapperName("braid-disk2".into()),
                    luks_uuid: LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap(),
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

    /// Build a `DiskName` from a literal, panicking on invalid input.
    /// Test-only helper to keep call sites short.
    fn disk_name(s: &str) -> DiskName {
        DiskName::parse(s).expect("valid disk name in fixture")
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

    /// Build the disk2 `(uuid, member)` pair used by the live-resolution
    /// tests against `two_device_pool`. UUID matches the synthetic
    /// pool entry so Pattern 4's UUID-keyed find succeeds.
    fn disk2_member_for_two_device_pool() -> (LuksUuid, membership::DiskMember) {
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let member = membership::DiskMember {
            name: disk_name("disk2"),
            by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
            devid: Some(2),
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
        let runner = MockRunner::default();
        let (uuid, member) = disk2_member_for_two_device_pool();
        let result = resolve_replace_source(
            &runner,
            &disk_name("disk2"),
            &uuid,
            &member,
            None,
            &pool,
            &mp(),
        );
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
        let (uuid, member) = disk2_member_for_two_device_pool();
        let err = resolve_replace_source(
            &runner,
            &disk_name("disk2"),
            &uuid,
            &member,
            Some(99),
            &pool,
            &mp(),
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
        let runner = MockRunner::default();
        let (uuid, member) = disk2_member_for_two_device_pool();
        let err = resolve_replace_source(
            &runner,
            &disk_name("disk2"),
            &uuid,
            &member,
            None,
            &pool,
            &mp(),
        )
        .unwrap_err();
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
            name: DiskName::parse("disk3").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            state: ConfigDiskState::PresentNotLuks,
        };
        let source = ReplaceSource::Live {
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .unwrap()
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
            name: DiskName::parse("disk3").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            state: ConfigDiskState::PresentNotLuks,
        };
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: true,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .unwrap()
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
        check_new_not_in_pool("disk3", &new_mn, &pool)
            .expect("disk3 is not in pool -- should pass");
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
    // Intent: dead path resolution auto-detects the missing devid from
    // the persisted member.
    // Why: when the operator does not supply `--missing-id`, planning uses
    // `old_member.devid` (cross-checked against btrfs missing_devids) to
    // pick the target. Pattern 4 / persisted-devid cross-check.
    // Scenario: operator replaces a dead disk (1 missing device, no --missing-id).
    fn dead_old_resolution_single_missing() {
        let mut pool = two_device_pool();
        // Simulate disk2 missing
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        let (uuid, member) = disk2_member_for_two_device_pool();
        let result = resolve_replace_source(
            &runner,
            &disk_name("disk2"),
            &uuid,
            &member,
            None,
            &pool,
            &mp(),
        );
        assert!(
            matches!(result, Ok(ReplaceSource::Missing { devid: 2 })),
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
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        let (uuid, member) = disk2_member_for_two_device_pool();
        let result = resolve_replace_source(
            &runner,
            &disk_name("disk2"),
            &uuid,
            &member,
            Some(2),
            &pool,
            &mp(),
        );
        assert!(
            matches!(result, Ok(ReplaceSource::Missing { devid: 2 })),
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
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        // disk2's persisted member pins devid 1 (the live one) so the
        // operator's --missing-id 1 lines up with the persisted entry --
        // but devid 1 is live in `pool.devices`, so the live-check fires.
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let member = membership::DiskMember {
            name: disk_name("disk2"),
            by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
            devid: Some(1),
            added_at: None,
        };
        let err = resolve_replace_source(
            &runner,
            &disk_name("disk2"),
            &uuid,
            &member,
            Some(1),
            &pool,
            &mp(),
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
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        let (uuid, member) = disk2_member_for_two_device_pool();
        let err = resolve_replace_source(
            &runner,
            &disk_name("disk2"),
            &uuid,
            &member,
            Some(99),
            &pool,
            &mp(),
        )
        .unwrap_err();
        match err {
            ReplaceError::OldDevidMismatch {
                old_name,
                pool_devid,
                observed,
            } => {
                assert_eq!(old_name, "disk2");
                assert_eq!(pool_devid, Some(2));
                assert_eq!(observed, 99);
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
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[3]);
        let (uuid, member) = disk2_member_for_two_device_pool();
        let err = resolve_replace_source(
            &runner,
            &disk_name("disk2"),
            &uuid,
            &member,
            None,
            &pool,
            &mp(),
        )
        .unwrap_err();
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
        pool.devices.retain(|d| d.mapper.0 != "braid-disk2");
        pool.missing_count = 1;
        pool.total_devices = 2;
        let runner = mock_with_missing_devids(&[2]);
        let uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let member = membership::DiskMember {
            name: disk_name("disk2"),
            by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
            devid: None,
            added_at: None,
        };
        let err = resolve_replace_source(
            &runner,
            &disk_name("disk2"),
            &uuid,
            &member,
            None,
            &pool,
            &mp(),
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

    fn new_probed_not_luks() -> ConfigDisk {
        ConfigDisk {
            name: DiskName::parse("disk3").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            state: ConfigDiskState::PresentNotLuks,
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
        let key_file = std::path::Path::new("/run/keys/braid-disk3.key");
        let extra_opts = LuksFormatExtraOpts::parse(&["--pbkdf".to_owned(), "pbkdf2".to_owned()])
            .expect("valid extras");
        let new_probed = ConfigDisk {
            name: DiskName::parse("disk3").expect("valid disk name in test fixture"),
            by_id_path: new_by_id.clone(),
            state: ConfigDiskState::PresentNotLuks,
        };

        let target =
            build_replace_journal_target(&new_by_id, &new_probed, Some(key_file), &extra_opts)
                .expect("fresh disk should build a replace journal target");

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
                assert_eq!(enroll_key_file, Some(key_file.to_path_buf()));
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
        let new_probed = ConfigDisk {
            name: DiskName::parse("disk3").expect("valid disk name in test fixture"),
            by_id_path: new_by_id.clone(),
            state: ConfigDiskState::PresentLuks {
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
        )
        .expect("existing LUKS disk should build a replace journal target");

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
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };

        let journal_source = build_replace_journal_source(&source);

        assert_eq!(
            journal_source,
            journal::ReplaceJournalSource::Live {
                old_devid: 2,
                old_mapper: MapperName("braid-disk2".into()),
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
        let source = ReplaceSource::Missing { devid: 2 };

        let journal_source = build_replace_journal_source(&source);

        assert_eq!(
            journal_source,
            journal::ReplaceJournalSource::Missing { old_devid: 2 }
        );
    }

    #[test]
    // Intent: missing-path dry-run (not last missing) omits rebalance step.
    // Why: if other missing devices remain, a rebalance would be premature.
    // Scenario: 3-disk pool, 2 missing, replacing 1 -- still degraded after.
    fn dry_run_missing_not_last_omits_rebalance() {
        let _config = make_replace_config();
        let new_probed = new_probed_not_luks();
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 3,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .unwrap()
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
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: true,
            total_devices: 1,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .unwrap()
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
    use crate::test_fixtures::{MockFs, PoolFixture, ReplacementPool, mock_ok};
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
            Err(ReplaceError::Pool(crate::pool::PoolError::Failed(msg))) => {
                assert!(
                    msg.contains("btrfs filesystem resize failed"),
                    "expected typed PoolError::Failed carrying resize message, got: {msg}"
                );
            }
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
                    CmdRequest::CryptsetupClose { mapper } if mapper == "braid-disk2"
                )
            })
            .expect("cryptsetup close on braid-disk2 must be issued even when resize fails");
        let resize_idx = log
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsFilesystemResize { devid: 2, .. }))
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
                    CmdRequest::CryptsetupClose { mapper } if mapper == "braid-disk2" => {
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
                    CmdRequest::CryptsetupClose { mapper } if mapper == "braid-disk2" => {
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
                matches!(request, CmdRequest::CryptsetupClose { mapper } if mapper == "braid-disk2")
            })
            .count();
        assert_eq!(close_count, 2);
        assert!(
            captured.contains("[ok]   disk disk2: locked"),
            "missing terminal ok row after retry: {captured:?}"
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
    //   must fire before the inhibitor seam at the "reversible preflight
    //   before inhibitor" boundary (cli/src/replace.rs:224-229).
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
                .missing_id(Some(2))
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
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .unwrap()
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
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
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
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: false,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: Some(kf),
            luks_format_extra_opts: &luks_format_extra_opts,
        })
        .unwrap()
        .render_steps();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // Steps: LUKS format, keyfile enroll, header backup, LUKS open,
        //        replace start, close old, resize = 7 steps x 2 lines each = 14
        // Header backup runs after the final keyslot mutation so the backup
        // captures slot 1; ordering invariant is format < addKey < backup < open.
        assert_eq!(lines.len(), 14, "expected 14 lines, got:\n{output}");

        // LUKS format
        assert!(lines[0].contains("[destructive]"));
        assert!(lines[1].contains("$ cryptsetup luksFormat"));
        assert!(lines[1].contains("--pbkdf pbkdf2 --iter-time 1"));
        assert!(lines[1].contains("--label braid-disk3"));

        // Keyfile enrollment (runs before backup so slot 1 lands in the backup)
        assert!(lines[2].contains("enroll keyfile"));
        assert!(lines[3].contains("$ cryptsetup luksAddKey"));
        assert!(lines[3].contains("/mnt/usb/braid.key"));

        // Header backup
        assert!(lines[4].contains("LUKS header backup"));
        assert!(lines[5].contains("$ cryptsetup luksHeaderBackup"));

        // LUKS open
        assert!(lines[6].contains("LUKS open"));
        assert!(lines[7].contains("$ cryptsetup open --type luks"));

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
        let new_probed = ConfigDisk {
            name: DiskName::parse("disk3").expect("valid disk name in test fixture"),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            state: ConfigDiskState::PresentLuks {
                uuid: luks_uuid.clone(),
                label: Some("braid-disk3".to_owned()),
                mapper_open: false,
            },
        };
        let source = ReplaceSource::Live {
            mapper: MapperName("braid-disk2".into()),
            devid: 2,
        };
        let kf = Path::new("/mnt/usb/braid.key");
        let (_tmp, paths) = test_paths();

        let old_uuid = LuksUuid::parse("99999999-9999-9999-9999-999999999999").unwrap();
        // ExistingLuks: new_uuid carries the probed value from new_probed.
        let new_uuid_for_test = luks_uuid.clone();
        let work_plan = build_replace_work_plan(ReplaceWorkPlanInput {
            config: Config::new(MountPoint("/mnt/storage".into())).unwrap(),
            old_uuid,
            old_name: DiskName::parse("disk2").unwrap(),
            new_uuid: new_uuid_for_test,
            new_name: DiskName::parse("disk3").unwrap(),
            new_by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed,
            replace_source: source,
            pool: replace_work_plan_test_pool(
                &ReplaceSource::Live {
                    mapper: MapperName("braid-disk2".into()),
                    devid: 2,
                },
                false,
                2,
            ),
            pre_membership: PoolMembership::empty(),
            target_membership: PoolMembership::empty(),
            paths: &paths,
            // Resolved value as `plan_replace` would compute it for a
            // `NeedsEnroll` outcome: the planner would have run
            // `plan_single_disk_enrollment` and the slot-1 check
            // returned `Empty`, so the keyfile path flows in as `Some`.
            enroll_key_file: Some(kf.to_path_buf()),
            luks_format_extra_opts: LuksFormatExtraOpts::default(),
        })
        .expect("ExistingLuks + enroll should plan");

        let steps = work_plan.render_steps();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        let pos_of = |needle: &str| {
            lines
                .iter()
                .position(|line| line.contains(needle))
                .unwrap_or_else(|| panic!("missing {needle:?} in render:\n{output}"))
        };

        let p_addkey = pos_of("$ cryptsetup luksAddKey");
        let p_backup = pos_of("$ cryptsetup luksHeaderBackup");
        let p_open = pos_of("$ cryptsetup open --type luks");

        assert!(
            p_addkey < p_backup,
            "luksAddKey must precede luksHeaderBackup so slot 1 is captured in the backup; got addKey@{p_addkey} backup@{p_backup}"
        );
        assert!(
            p_backup < p_open,
            "luksHeaderBackup must precede luksOpen; got backup@{p_backup} open@{p_open}"
        );
        assert!(
            lines[p_addkey].contains("/mnt/usb/braid.key"),
            "addKey command must reference the keyfile path: {}",
            lines[p_addkey]
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
        let source = ReplaceSource::Missing { devid: 2 };
        let steps = replace_work_plan_for_test(&ReplaceWorkPlanTestInput {
            new_name: "disk3",
            new_by_id: &ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
            new_probed: &new_probed,
            replace_source: &source,
            mount_point: &MountPoint("/mnt/storage".into()),
            will_clear_last_missing: true,
            total_devices: 2,
            paths: &test_paths().1,
            enroll_key_file: None,
            luks_format_extra_opts: &[],
        })
        .unwrap()
        .render_steps();
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

        assert!(
            matches!(result, Err(ReplaceError::Validation(_))),
            "expected Err(ReplaceError::Validation(_)) for wrong passphrase on a closed-LUKS new disk, got: {result:?}"
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
            matches!(result, Err(ReplaceError::Pool(_))),
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
                .missing_id(Some(2))
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
                .missing_id(Some(2))
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
        assert_eq!(label, "braid-disk3");
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
                .missing_id(Some(2))
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
                .missing_id(Some(2))
                .build(),
        );

        match &result {
            Err(ReplaceError::Pool(_)) => {}
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
                .missing_id(Some(2))
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

    // Intent: a same-name `braid replace --old/--new` typo aborts in the
    // planner before any shell or Filesystem-backed probe.
    // Why it exists: the pre-hoist preserved-note behavior conflated a pure
    // input-shape error with I/O-precondition context; this path now fails
    // before preflight can accumulate state-context notes.
    // Scenario: user runs `braid replace --old disk2 --new disk2=/...`;
    // the planner returns the same-name validation error with no probes.
    #[test]
    fn plan_replace_old_equals_new_aborts_before_any_probe() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let failure = match plan_replace(
            &PanicRunner,
            &PanicFilesystem,
            &ReplaceParams {
                config_path: &config_path,
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
                sleeper: &crate::progress::NoopSleeper,
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
    // planning, before shell probes or Filesystem-backed pool/disk probes.
    // Why it exists: a typoed keyfile path must not let replace format the
    // new disk and then fail only at keyfile enrollment.
    // Scenario: user passes a nonexistent enroll directory while replacing a
    // disk; the command refuses with a keyfile error and no probes run.
    #[test]
    fn plan_replace_aborts_when_keyfile_missing_before_any_probe() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let kf_path = config_tmp.path().join("does-not-exist").join("braid.key");
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let report = plan_replace(
            &PanicRunner,
            &PanicFilesystem,
            &ReplaceParams {
                config_path: &config_path,
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
                sleeper: &crate::progress::NoopSleeper,
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
    // planning, before shell probes or Filesystem-backed pool/disk probes.
    // Why it exists: checking only existence would still allow an invalid
    // keyfile path to reach destructive LUKS work before enrollment fails.
    // Scenario: user points --enroll at a directory containing a subdirectory
    // named braid.key; the command refuses before any disk inspection.
    #[test]
    fn plan_replace_aborts_when_keyfile_is_directory_before_any_probe() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let kf_path = config_tmp.path().join("braid.key");
        std::fs::create_dir(&kf_path).unwrap();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();

        let report = plan_replace(
            &PanicRunner,
            &PanicFilesystem,
            &ReplaceParams {
                config_path: &config_path,
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
                sleeper: &crate::progress::NoopSleeper,
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

    // Intent: cmd_replace returns the same-name validation error without
    // rendering preserved preflight notes.
    // Why it exists: the pre-hoist preserved-note behavior was intentionally
    // retired; pure input-shape errors now abort before I/O-precondition
    // context can be gathered or rendered.
    // Scenario: user runs `braid replace --old disk2 --new disk2=/...`; the
    // command returns the validation error and stderr has no busy-op note.
    #[test]
    fn cmd_replace_old_equals_new_aborts_before_any_probe() {
        // KEEP &PanicRunner / &PanicFilesystem -- the assertion is precisely
        // that no probe runs before validation; substituting a regular
        // runner/fs would let an accidental pre-validation probe pass
        // silently. PoolFixture::empty supplies the temp dirs + config and
        // ReplaceParamsBuilder constructs identical params, with
        // passphrase_file=None preserved.
        let f = PoolFixture::empty();
        let (result, stderr) = super::replace_stderr_capture::capture(|| {
            cmd_replace(
                &PanicRunner,
                &PanicFilesystem,
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
            devid: Some(2),
            added_at: None,
        };
        let member_d = membership::DiskMember {
            name: disk_name("decoy"),
            by_id: ByIdPath::parse("/dev/disk/by-id/misleading-label").unwrap(),
            devid: Some(99),
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
                mapper: MapperName("braid-keeper".into()),
                luks_uuid: LuksUuid::parse("cccccccc-cccc-cccc-cccc-cccccccc0602").unwrap(),
                devid: 1,
                underlying: "/dev/vda".into(),
            }],
            missing_count: 1,
            missing_devids: vec![2],
            total_devices: 2,
            fsid: None,
            null_underlying: vec![],
        };
        let runner = mock_with_missing_devids(&[2]);
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
            Some(2),
            "U_R's persisted devid must be 2 (the missing devid)"
        );
        let source = resolve_replace_source(
            &runner,
            &target_name,
            resolved_uuid,
            resolved_member,
            None,
            &pool,
            &mp(),
        )
        .expect("missing-path resolution must succeed for U_R");
        match source {
            ReplaceSource::Missing { devid } => assert_eq!(devid, 2),
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
        // No CryptsetupLuksUuid was issued for any decoy by-id: the
        // missing-path resolution path uses persisted state, not probes.
        let requests = runner.requests();
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::CryptsetupLuksUuid { .. })),
            "missing-path planning must not probe LUKS UUID against any decoy by-id: {requests:?}"
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
            devid: Some(7),
            added_at: None,
        };
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-WRONG".into()),
                luks_uuid: u_old.clone(),
                devid: 7,
                underlying: "/dev/vdz".into(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };
        let runner = MockRunner::default();
        let source = resolve_replace_source(
            &runner,
            &disk_name("right"),
            &u_old,
            &member,
            None,
            &pool,
            &mp(),
        )
        .expect("Pattern 4 find by UUID must succeed");
        match source.clone() {
            ReplaceSource::Live { mapper, devid } => {
                assert_eq!(
                    mapper.0, "braid-WRONG",
                    "observed mapper must be cloned, not reconstructed"
                );
                assert_eq!(devid, 7);
            }
            other => panic!("expected ReplaceSource::Live, got {other:?}"),
        }
        // build_replace_journal_source propagates the observed mapper
        // into ReplaceJournalSource::Live.old_mapper.
        let journal_source = build_replace_journal_source(&source);
        assert_eq!(
            journal_source,
            journal::ReplaceJournalSource::Live {
                old_devid: 7,
                old_mapper: MapperName("braid-WRONG".into()),
            }
        );
    }

    /// Build a recording `MockRunner` that injects a `CryptsetupLuksUuid`
    /// probe response for `device`, returning the supplied canned
    /// `RawCommandOutput`. Used by the open-boundary re-probe and
    /// post-commit close double-drift regression tests.
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
                    mapper: mapper.to_owned(),
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

    /// Seed 620: Post-commit close double-drift probe mismatch arm.
    /// When the active mapper's backing-device LUKS UUID disagrees
    /// with the journaled `old_uuid`, the close is demoted to a
    /// logged warning skip; zero `CryptsetupClose` requests reach the
    /// runner.
    //
    // Intent: probe_observed_mapper_uuid returns false on mismatch and
    //   the caller skips the close.
    // Why: defense-in-depth against operator double-drift -- closing
    //   the wrong dm slot would tear down a foreign disk's mapper.
    // Scenario: journaled old_uuid = U_OLD, observed mapper
    //   "braid-WRONG" now reports backing device /dev/vdf, whose LUKS
    //   UUID is U_FOREIGN.
    #[test]
    fn replace_post_commit_close_probe_mismatch_skips_close() {
        let u_old = LuksUuid::parse("eeeeeeee-eeee-eeee-eeee-eeeeeeee0620").unwrap();
        let u_foreign = LuksUuid::parse("ffffffff-ffff-ffff-ffff-ffffffff0621").unwrap();
        let runner = runner_with_active_mapper_uuid(
            "braid-WRONG",
            "/dev/vdf",
            RawCommandOutput {
                cmd: "cryptsetup luksUUID /dev/vdf".into(),
                stdout: format!("{u_foreign}\n"),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let mapper = MapperName("braid-WRONG".into());
        let matched = probe_observed_mapper_uuid(&runner, &mapper, &u_old);
        assert!(
            !matched,
            "probe mismatch must signal skip-close (returned false)"
        );
        // Caller must NOT issue CryptsetupClose on the false arm.
        let requests = runner.requests();
        let probe_count = requests
            .iter()
            .filter(
                |r| matches!(r, CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vdf"),
            )
            .count();
        assert_eq!(
            probe_count, 1,
            "exactly one backing-device probe must run before the skip decision"
        );
        let close_count = requests
            .iter()
            .filter(|r| matches!(r, CmdRequest::CryptsetupClose { .. }))
            .count();
        assert_eq!(
            close_count, 0,
            "no close may issue when the probe disagrees with the journaled UUID"
        );
    }

    /// Seed 621: Control arm for the post-commit close double-drift
    /// probe. When the active mapper's backing-device LUKS UUID matches
    /// the journaled value, `probe_observed_mapper_uuid` returns true so
    /// the caller proceeds to close.
    //
    // Intent: probe match returns true; caller continues to close.
    // Why: pins the fail-safe-skip-only semantic so a future change
    //   that turned the probe into fail-skip-on-any-condition would
    //   flip this assertion.
    // Scenario: journaled old_uuid = U_OLD, observed mapper
    //   "braid-disk2" reports backing device /dev/vdc, whose probe
    //   returns U_OLD.
    #[test]
    fn replace_post_commit_close_probe_match_allows_close() {
        let u_old = LuksUuid::parse("11111111-1111-1111-1111-111111110621").unwrap();
        let runner = runner_with_active_mapper_uuid(
            "braid-disk2",
            "/dev/vdc",
            RawCommandOutput {
                cmd: "cryptsetup luksUUID /dev/vdc".into(),
                stdout: format!("{u_old}\n"),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let mapper = MapperName("braid-disk2".into());
        let matched = probe_observed_mapper_uuid(&runner, &mapper, &u_old);
        assert!(matched, "probe match must allow close (returned true)");
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
            devid: Some(9),
            added_at: None,
        };
        let pre = membership_from(vec![
            (
                old_uuid.clone(),
                membership::DiskMember {
                    name: disk_name("oldname"),
                    by_id: ByIdPath::parse("/dev/disk/by-id/old").unwrap(),
                    devid: Some(7),
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
                devid: Some(11),
                added_at: None,
            },
        )]);
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-foreign".into()),
                luks_uuid: colliding.clone(),
                devid: 22,
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
                devid: Some(11),
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
    /// shared validation surface. One symmetric case (`--label=foo`)
    /// proves the wiring through `ReplaceError::ManagedFormatFlag`.
    //
    // Intent: plan_replace fails closed on a managed token in
    //   `--luks-format-arg` before any probe, journal write, or
    //   format request.
    // Why: pinning the rejection at the planner boundary mirrors the
    //   `add.rs` contract and protects the journaled identity from
    //   user override.
    // Scenario: operator passes `--luks-format-arg=--label=foo`.
    #[test]
    fn plan_replace_rejects_managed_format_flag() {
        let state_tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(state_tmp.path().into());
        let config_tmp = tempfile::tempdir().unwrap();
        let config_path = config_tmp.path().join("config.json");
        std::fs::write(
            &config_path,
            serde_json::to_vec(&serde_json::json!({ "mount_point": "/mnt/storage" })).unwrap(),
        )
        .unwrap();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let bad = vec!["--label=foo".to_owned()];
        let result = plan_replace(
            &PanicRunner,
            &PanicFilesystem,
            &ReplaceParams {
                config_path: &config_path,
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
                sleeper: &crate::progress::NoopSleeper,
            },
        );
        match result {
            Err(PlanFailure {
                error:
                    ReplaceError::ManagedFormatFlag(
                        crate::types::LuksFormatExtraOptsError::ManagedFormatFlag { token },
                    ),
                ..
            }) => {
                assert_eq!(token, "--label=foo");
            }
            Err(PlanFailure { error, .. }) => {
                panic!("expected ManagedFormatFlag refusal, got: {error:?}")
            }
            Ok(_) => panic!("expected ManagedFormatFlag refusal, got Ok(_)"),
        }
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "managed-flag rejection must fire before the inhibitor seam"
        );
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
            label, "braid-disk3",
            "structured label is derived at boundary"
        );
        assert!(
            uuid::Uuid::parse_str(uuid.as_str()).is_ok(),
            "structured uuid carries the journaled identity"
        );
    }
}
