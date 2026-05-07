use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::{self, Config};
use crate::credential::{self, OpenCredential};
use crate::credential_verify::{Credential, CredentialVerifyTarget, verify_credential_for_targets};
use crate::discover;
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal::{self, Journal};
use crate::luks::{self, VerifyOutcome};
use crate::membership::{self, DiskMember, PoolMembership};
use crate::mount::{self, MountError, OpenPlan};
use crate::parse::btrfs_filesystem_show::{DeviceBtrfsProbe, classify_btrfs_probe};
use crate::parse::{ReplaceState, parse_btrfs_replace_status};
use crate::preview::{self, PerDiskStyle, Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{self, Filesystem, ProbeError};
use crate::progress::{self, ProgressOutput, RealSleeper, Sleeper};
use crate::secret::Passphrase;
use crate::state_paths::StatePaths;
use crate::status::{BalanceReport, get_balance_report};
use crate::status_tag::{StatusTag, color_enabled_for_stderr, emit_status, status_line};
use crate::types::{ByIdPath, ConfigDiskState, MountPoint, PoolState};
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RecoverError {
    #[error("{0}")]
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

/// Resolve `/dev/disk/by-id/` symlinks against live device identity during recovery.
///
/// Narrow recovery-local abstraction so the production code path can read symlinks
/// without widening the shared `probe::Filesystem` trait (which has 14 mock impls).
/// `RealByIdResolver` is the production implementation; tests inject their own.
pub trait ByIdResolver {
    /// List filenames under `/dev/disk/by-id/`. Returns an empty vec if the
    /// directory does not exist (mirrors `Filesystem::list_dir` semantics).
    fn list_by_id_entries(&self) -> Result<Vec<String>, std::io::Error>;

    /// Canonicalize `path` (resolve all symlinks to an absolute path).
    fn canonicalize(&self, path: &str) -> Result<String, std::io::Error>;
}

pub struct RealByIdResolver;

impl ByIdResolver for RealByIdResolver {
    fn list_by_id_entries(&self) -> Result<Vec<String>, std::io::Error> {
        match std::fs::read_dir("/dev/disk/by-id") {
            Ok(entries) => entries
                .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(e) => Err(e),
        }
    }

    fn canonicalize(&self, path: &str) -> Result<String, std::io::Error> {
        std::fs::canonicalize(path).map(|p| p.to_string_lossy().into_owned())
    }
}

/// Find the `/dev/disk/by-id/` symlink whose canonical target matches `underlying`.
///
/// `underlying` is a live pool device's backing kernel path (from `cryptsetup status`).
/// We pick the highest-priority match by `discover::by_id_priority` so the recorded
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
        if discover::is_partition_entry(&name) {
            continue;
        }
        let full = format!("{by_id_dir}/{name}");
        // Skip dangling/broken symlinks silently — they cannot match anything.
        let Ok(resolved) = resolver.canonicalize(&full) else {
            continue;
        };
        if resolved == target {
            matches.push((discover::by_id_priority(&name), name, full));
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
             the kernel, then re-run `braid recover`. If by-id entries exist \
             but none match this device's canonical path, file a braid bug \
             with the udevadm output."
        )));
    }

    // Stable highest-priority pick: lowest by_id_priority wins, ties broken by filename.
    matches.sort_by(|a, b| (a.0, &a.1).cmp(&(b.0, &b.1)));
    Ok(ByIdPath(matches.into_iter().next().unwrap().2))
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
    /// Progress output for the post-mount remediation phase (replace resize
    /// replay and paused-balance resume). Off in tests; Human/Json in real
    /// use because the resume can be long-running.
    pub progress: ProgressOutput,
    pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
}

/// Dry-run preview source of truth for `braid recover` plus the
/// execute inputs pre-computed during planning. `preview()` renders
/// accumulated notes plus steps from the semantic work plan; `execute()`
/// renders `notes` to stderr with `STDERR_STYLE` before any mutation,
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

/// Report returned by `plan_recover`. On the `Ok` branch, all
/// accumulated notes (entry banner + probe events) have been moved
/// into `plan.notes` and `notes` here is empty. On the `Err` branch,
/// `notes` carries the banner + per-disk context accumulated before
/// planning bailed (e.g. `DegradedRefused`) so the caller can render
/// it to stderr before the error.
pub struct RecoverPlanReport {
    pub notes: Vec<PreviewNote>,
    pub result: Result<RecoverPlan, RecoverError>,
}

#[derive(Debug)]
struct RecoverWorkPlan {
    open_plan: Option<OpenPlan>,
    pre_resolved_credential: Option<OpenCredential>,
    journal: Journal,
    union: PoolMembership,
    mount_point: MountPoint,
    pool_json_path: PathBuf,
    pending_op_path: PathBuf,
    luks_headers_dir: PathBuf,
    actions: Vec<RecoverWorkAction>,
}

#[derive(Debug)]
enum RecoverWorkAction {
    InitialOpenPool,
    WaitForKernelReplace,
    RemountCycle {
        close_names: Vec<String>,
        reopen_names: Vec<String>,
        any_missing_member: bool,
    },
    Complete(RecoverCompletion),
}

#[derive(Debug)]
enum RecoverCompletion {
    AddPoolMutation {
        targets: std::collections::BTreeMap<String, journal::AddJournalTarget>,
        all_targets_already_live: bool,
        live_names: Option<std::collections::BTreeSet<String>>,
    },
    AddPostBalance,
    RemoveMissingPoolMutation {
        devid: u64,
        restore_raid1_after_commit: bool,
    },
    RemoveMissingPostMaintenance {
        devid: u64,
        restore_raid1_after_commit: bool,
    },
    ReplacePoolMutation {
        old_name: String,
        new_name: String,
        new_target: journal::ReplaceJournalTarget,
        source: journal::ReplaceJournalSource,
        restore_raid1_after_commit: bool,
    },
    ReplacePostMaintenance {
        new_name: String,
        source: journal::ReplaceJournalSource,
        restore_raid1_after_commit: bool,
    },
    GenericLivePool {
        replay_raid1_maintenance: bool,
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
                    .map(|name| format!("/dev/mapper/{}", config::mapper_name(name).0))
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
                        commands: vec![CmdRequest::CryptsetupClose {
                            mapper: mn.0.clone(),
                        }],
                    });
                }

                for name in reopen_names {
                    let member = plan
                        .union
                        .disks
                        .get(name)
                        .expect("remount-cycle reopen target validated during planning");
                    let mn = config::mapper_name(name);
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "LUKS open {} → {} (recover remount cycle)",
                            member.by_id, mn,
                        ),
                        commands: vec![CmdRequest::CryptsetupLuksOpen {
                            device: member.by_id.0.clone(),
                            mapper: mn.0.clone(),
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
                let mount_device =
                    format!("/dev/mapper/{}", config::mapper_name(first_reopen_name).0);
                if *any_missing_member {
                    steps.push(Step {
                        risk: "safe",
                        description: format!(
                            "mount → {} (recover remount cycle, degraded)",
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
                            "mount → {} (recover remount cycle)",
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
            RecoverWorkAction::InitialOpenPool => {
                execute_recover_initial_open(plan, state, runner, fs, params)?;
                Ok(false)
            }
            RecoverWorkAction::WaitForKernelReplace => {
                if state.just_mounted {
                    wait_for_kernel_replace_to_finish(
                        runner,
                        &plan.mount_point,
                        &progress::RealSleeper,
                        color_enabled_for_stderr(),
                    )?;
                }
                Ok(false)
            }
            RecoverWorkAction::RemountCycle { close_names, .. } => {
                if state.just_mounted {
                    let recovery_mount_membership =
                        mount_membership_for_recover(&plan.journal, &plan.union).clone();
                    let cred = state.credential.as_ref().expect(
                        "just_mounted implies open_plan was Some and credential was resolved",
                    );
                    relock_and_remount(
                        runner,
                        fs,
                        params.config,
                        &recovery_mount_membership,
                        params.allow_degraded,
                        cred,
                        close_names,
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
                live_names,
            } => {
                render_add_pool_mutation_recovery_steps(
                    plan,
                    steps,
                    targets,
                    *all_targets_already_live,
                    live_names.as_ref(),
                );
                render_recovery_tail(plan, steps, None, true);
            }
            RecoverCompletion::AddPostBalance => {
                render_recovery_tail(plan, steps, None, true);
            }
            RecoverCompletion::RemoveMissingPoolMutation {
                restore_raid1_after_commit,
                ..
            }
            | RecoverCompletion::RemoveMissingPostMaintenance {
                restore_raid1_after_commit,
                ..
            } => {
                render_recovery_tail(plan, steps, None, *restore_raid1_after_commit);
            }
            RecoverCompletion::ReplacePoolMutation {
                new_name,
                restore_raid1_after_commit,
                ..
            } => {
                render_recovery_tail(
                    plan,
                    steps,
                    Some(ReplaceResizePreview {
                        new_name,
                        skipped_if_replacement_not_committed: true,
                    }),
                    *restore_raid1_after_commit,
                );
            }
            RecoverCompletion::ReplacePostMaintenance {
                new_name,
                restore_raid1_after_commit,
                ..
            } => {
                render_recovery_tail(
                    plan,
                    steps,
                    Some(ReplaceResizePreview {
                        new_name,
                        skipped_if_replacement_not_committed: false,
                    }),
                    *restore_raid1_after_commit,
                );
            }
            RecoverCompletion::GenericLivePool {
                replay_raid1_maintenance,
            } => {
                render_recovery_tail(plan, steps, None, *replay_raid1_maintenance);
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
        match self {
            RecoverCompletion::AddPoolMutation { targets, .. } => {
                execute_add_pool_mutation_recovery(
                    runner,
                    fs,
                    by_id_resolver,
                    params,
                    AddPoolReplayCtx {
                        credential: state.credential.as_ref(),
                        journal: &plan.journal,
                        union: &plan.union,
                        targets,
                        pool,
                    },
                )
            }
            RecoverCompletion::AddPostBalance => execute_add_post_balance_recovery(
                runner,
                by_id_resolver,
                params,
                &plan.journal,
                &plan.union,
                pool,
                false,
            ),
            RecoverCompletion::RemoveMissingPoolMutation {
                devid,
                restore_raid1_after_commit,
            } => execute_remove_missing_pool_mutation_recovery(
                runner,
                by_id_resolver,
                params,
                &plan.journal,
                pool,
                *devid,
                *restore_raid1_after_commit,
            ),
            RecoverCompletion::RemoveMissingPostMaintenance {
                devid,
                restore_raid1_after_commit,
            } => execute_remove_missing_post_maintenance_recovery(
                runner,
                by_id_resolver,
                params,
                RemoveMissingPostCtx {
                    journal: &plan.journal,
                    pool,
                    devid: *devid,
                    restore_raid1_after_commit: *restore_raid1_after_commit,
                    inhibitor_already_held: false,
                },
            ),
            RecoverCompletion::ReplacePoolMutation {
                old_name,
                new_name,
                new_target,
                source,
                restore_raid1_after_commit,
            } => execute_replace_pool_mutation_recovery(
                runner,
                fs,
                by_id_resolver,
                params,
                state.credential.as_ref(),
                &plan.journal,
                &plan.union,
                pool,
                old_name,
                new_name,
                new_target,
                source,
                *restore_raid1_after_commit,
            ),
            RecoverCompletion::ReplacePostMaintenance {
                new_name,
                source,
                restore_raid1_after_commit,
            } => execute_replace_post_maintenance_recovery(
                runner,
                by_id_resolver,
                params,
                &plan.journal,
                pool,
                new_name,
                source,
                fs,
                *restore_raid1_after_commit,
                false,
            ),
            RecoverCompletion::GenericLivePool { .. } => {
                execute_generic_live_pool_recovery(runner, by_id_resolver, params, plan, pool)
            }
        }
    }
}

struct ReplaceResizePreview<'a> {
    new_name: &'a str,
    skipped_if_replacement_not_committed: bool,
}

fn render_add_pool_mutation_recovery_steps(
    plan: &RecoverWorkPlan,
    steps: &mut Vec<Step>,
    targets: &std::collections::BTreeMap<String, journal::AddJournalTarget>,
    all_targets_already_live: bool,
    live_names: Option<&std::collections::BTreeSet<String>>,
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
    for (name, target) in targets {
        let mapper_path = format!("/dev/mapper/{}", target.mapper_name);
        if live_names.is_some_and(|live| live.contains(name)) {
            let (kind, label) = match &target.mode {
                journal::AddJournalMode::RecoverableBraidLabeled { .. } => {
                    ("verified returned-disk add", mapper_path.clone())
                }
                journal::AddJournalMode::FreshLuks { .. } => {
                    ("fresh add target", target.by_id.0.clone())
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
        match &target.mode {
            journal::AddJournalMode::RecoverableBraidLabeled { .. } => {
                steps.push(Step {
                    risk: "safe",
                    description: format!(
                        "replay verified returned-disk add {mapper_path}{conditional_suffix}"
                    ),
                    commands: vec![
                        CmdRequest::BtrfsDeviceScanForget {
                            devices: vec![mapper_path.clone()],
                        },
                        CmdRequest::WipefsBtrfs {
                            device: mapper_path.clone(),
                        },
                        CmdRequest::BtrfsDeviceAdd {
                            device: mapper_path,
                            mount_point: plan.mount_point.clone(),
                            force: true,
                        },
                    ],
                });
            }
            journal::AddJournalMode::FreshLuks {
                luks_format_extra_opts,
                enroll_key_file,
                ..
            } => {
                let mut commands = vec![CmdRequest::CryptsetupLuksFormat {
                    device: target.by_id.0.clone(),
                    extra_opts: luks_format_extra_opts.clone(),
                }];
                if let Some(key_file) = enroll_key_file {
                    commands.push(CmdRequest::CryptsetupLuksAddKeyFile {
                        device: target.by_id.0.clone(),
                        key_file_path: key_file.display().to_string(),
                    });
                }
                commands.push(CmdRequest::CryptsetupLuksHeaderBackup {
                    device: target.by_id.0.clone(),
                    backup_path: plan
                        .luks_headers_dir
                        .join(format!("{}.luksheader", target.mapper_name))
                        .display()
                        .to_string(),
                });
                commands.push(CmdRequest::CryptsetupLuksOpen {
                    device: target.by_id.0.clone(),
                    mapper: target.mapper_name.clone(),
                });
                commands.push(CmdRequest::BtrfsDeviceAdd {
                    device: mapper_path,
                    mount_point: plan.mount_point.clone(),
                    force: false,
                });
                steps.push(Step {
                    risk: "destructive",
                    description: format!(
                        "replay fresh add target {}{conditional_suffix}",
                        target.by_id
                    ),
                    commands,
                });
            }
        }
    }
}

fn render_recovery_tail(
    plan: &RecoverWorkPlan,
    steps: &mut Vec<Step>,
    resize: Option<ReplaceResizePreview<'_>>,
    show_raid1_maintenance: bool,
) {
    steps.push(Step {
        risk: "safe",
        description: format!(
            "write recovered pool.json → {}",
            plan.pool_json_path.display()
        ),
        commands: vec![],
    });

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

    if show_raid1_maintenance {
        steps.push(Step {
            risk: "long",
            description: format!(
                "btrfs balance resume {} (skipped if no paused balance)",
                plan.mount_point
            ),
            commands: vec![],
        });
        steps.push(Step {
            risk: "long",
            description: format!(
                "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft {} \
                     (skipped if pool has <2 devices)",
                plan.mount_point
            ),
            commands: vec![],
        });
    }

    steps.push(Step {
        risk: "safe",
        description: format!("clear pending-op.json → {}", plan.pending_op_path.display()),
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

    // Recover-specific gate: resolve a credential whenever we have an
    // initial mount plan. This is eager on purpose -- even if every mapper
    // is already open, a replace remount cycle closes every mapper and must
    // reopen them with the same credential.
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

    enum InitialOpenFailure {
        MountOnly(MountError),
        Unlock(mount::UnlockAndMountFailure),
    }

    let res: Result<bool, InitialOpenFailure> = if open_plan.to_unlock.is_empty() {
        mount::execute_mount_only(runner, fs, params.config, open_plan)
            .map_err(InitialOpenFailure::MountOnly)
    } else {
        let cred = state
            .credential
            .as_ref()
            .expect("credential resolved above when open_plan is Some");
        mount::execute_unlock_and_mount(runner, fs, params.config, open_plan, cred)
            .map_err(InitialOpenFailure::Unlock)
    };

    state.just_mounted = match res {
        Ok(just_mounted) => just_mounted,
        Err(InitialOpenFailure::MountOnly(e)) => {
            return Err(e.into());
        }
        Err(InitialOpenFailure::Unlock(failure)) => {
            // Bootstrap mount failure: probe the target devices to confirm
            // no btrfs superblock exists -- only then is it safe to advise
            // wiping.
            if plan.journal.pre_membership.disks.is_empty()
                && let mount::MountError::MountFailed(_) = &failure.error
                && let journal::OpKind::Add { targets, .. } = &plan.journal.op
            {
                let all_no_btrfs = targets.values().all(|target| {
                    let mapper = format!("/dev/mapper/{}", target.mapper_name);
                    match runner.run(&CmdRequest::BtrfsFilesystemShowTarget { target: mapper }) {
                        Ok(raw) => matches!(classify_btrfs_probe(&raw), DeviceBtrfsProbe::NoBtrfs),
                        Err(_) => false,
                    }
                });
                let _ = mount::close_opened_mappers(
                    runner,
                    &RealSleeper,
                    fs,
                    &failure.opened_mappers,
                    color_enabled_for_stderr(),
                );
                if all_no_btrfs {
                    let disk_list: Vec<_> = plan
                        .union
                        .disks
                        .iter()
                        .map(|(name, m)| format!("  {} ({})", name, m.by_id))
                        .collect();
                    return Err(RecoverError::Failed(format!(
                        "bootstrap add was interrupted before the filesystem was \
                         created.\n\
                         The pool does not exist yet, so there is nothing to \
                         recover.\n\n\
                         To return to a clean state:\n\
                         1. rm {}\n\
                         2. Wipe the LUKS container from each disk that was being \
                            added:\n{}\n\
                            e.g.: wipefs -a /dev/disk/by-id/<device>\n\
                         3. Re-run braid add",
                        params.paths.pending_op_json().display(),
                        disk_list.join("\n"),
                    )));
                }
                return Err(failure.error.into());
            }
            let _ = mount::close_opened_mappers(
                runner,
                &RealSleeper,
                fs,
                &failure.opened_mappers,
                color_enabled_for_stderr(),
            );
            return Err(failure.error.into());
        }
    };
    Ok(())
}

fn execute_generic_live_pool_recovery<R: CommandRunner + Sync>(
    runner: &R,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    plan: &RecoverWorkPlan,
    pool: PoolState,
) -> Result<(), RecoverError> {
    let prior = membership::load_membership(params.paths).ok();
    let recovered =
        build_membership_from_live_pool(&pool, &plan.union, prior.as_ref(), by_id_resolver)?;

    let pre_names: std::collections::BTreeSet<_> =
        plan.journal.pre_membership.disks.keys().collect();
    let target_names: std::collections::BTreeSet<_> =
        plan.journal.target_membership.disks.keys().collect();
    let recovered_names: std::collections::BTreeSet<_> = recovered.disks.keys().collect();

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

    replay_post_mutation(
        runner,
        &plan.mount_point,
        &plan.journal.op,
        &pool,
        params.progress,
    )?;

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
    /// Real-run and failure-path stderr both use `Bracketed`, matching
    /// today's `mount::render_probe_events` output. `Preview::render`
    /// is already `Bracketed`, so success/failure/real-run all share
    /// the same per-disk wording.
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
        by_id_resolver: &dyn ByIdResolver,
        params: &RecoverParams<'_>,
    ) -> Result<(), RecoverError> {
        // Render accumulated notes (entry banner + probe events) to
        // stderr before any mutation. This replaces today's pair of
        // `eprintln!(entry)` + `mount::print_probe_events(&events)`
        // calls with byte-identical output in the same order.
        eprint!(
            "{}",
            preview::render_notes_for_stderr_with(
                &self.notes,
                Self::STDERR_STYLE,
                crate::status_tag::color_enabled_for_stderr(),
            ),
        );

        let RecoverPlan {
            notes: _,
            work_plan,
        } = self;
        work_plan.execute(runner, fs, by_id_resolver, params)
    }
}

/// Plan a `braid recover` run. Owns everything above today's real-run
/// mutation body: journal load, `union_memberships`,
/// `mount::plan_open_pool`, ProbeEvent-to-PreviewNote conversion,
/// dry-run already-mounted reconciliation, and dry-run step
/// construction (write pool.json / clear pending-op.json, plus
/// compile_open_steps when an initial mount is required).
///
/// On success, accumulated notes (entry banner + probe events) live
/// on `plan.notes` (the single render source for both preview and
/// execute). On failure after accumulating notes, those notes move
/// to `report.notes` so the caller can render them to stderr before
/// the error.
pub fn plan_recover<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RecoverParams<'_>,
) -> RecoverPlanReport {
    // 1. Load journal (required -- nothing to recover if absent). The
    // no-journal failure is a no-context failure by design: nothing has
    // been probed or accumulated yet, so `report.notes` stays empty.
    let journal = match journal::load_journal(params.paths) {
        Ok(Some(j)) => j,
        Ok(None) => {
            return RecoverPlanReport {
                notes: Vec::new(),
                result: Err(RecoverError::Failed(
                    "no pending operation journal found -- nothing to recover".into(),
                )),
            };
        }
        Err(e) => {
            return RecoverPlanReport {
                notes: Vec::new(),
                result: Err(RecoverError::Journal(e.to_string())),
            };
        }
    };

    // Entry banner always comes first -- whether the run succeeds, fails at
    // plan_open_pool, or fails at the already-mounted reconciliation.
    let mut notes = vec![PreviewNote::Info(format_recover_entry(&journal))];

    let union = union_memberships(&journal);
    let mount_membership = mount_membership_for_recover(&journal, &union);
    let mut pre_resolved_credential = None;

    if let journal::OpKind::Add {
        phase: journal::AddPhase::PoolMutation,
        targets,
    } = &journal.op
        && !journal.pre_membership.disks.is_empty()
        && !params.dry_run
    {
        match discover_add_targets_before_mount(runner, fs, params, &journal, targets) {
            Ok(credential) => pre_resolved_credential = credential,
            Err(e) => {
                return RecoverPlanReport {
                    notes,
                    result: Err(e),
                };
            }
        }
    }

    let report = mount::plan_open_pool(
        runner,
        fs,
        params.config,
        mount_membership,
        params.allow_degraded,
        "recover",
    );
    for event in &report.events {
        notes.push(event.to_preview_note());
    }

    let cycle_reopen_names: Vec<String> = report
        .events
        .iter()
        .filter_map(|e| match e {
            mount::ProbeEvent::DiskAvailable { name }
            | mount::ProbeEvent::DiskAlreadyOpen { name } => Some(name.clone()),
            _ => None,
        })
        .collect();
    let cycle_close_names: Vec<String> = union
        .disks
        .keys()
        .filter(|name| {
            if cycle_reopen_names.contains(name) {
                return true;
            }
            let mapper_path = format!("/dev/mapper/{}", config::mapper_name(name).0);
            fs.exists(&mapper_path)
        })
        .cloned()
        .collect();

    let open_plan = match report.result {
        Ok(op) => op,
        Err(e) => {
            return RecoverPlanReport {
                notes,
                result: Err(RecoverError::Mount(e)),
            };
        }
    };

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
    if open_plan.is_none() && is_replace_pool_mutation(&journal.op) {
        return RecoverPlanReport {
            notes,
            result: Err(RecoverError::Failed(
                "recover refuses to probe an already-mounted pool when the journal \
                 records a replace -- the kernel may have resumed an interrupted \
                 dev_replace on this mount session, leaving stale in-memory device \
                 state that probe_pool cannot distinguish from real topology.\n\n\
                 To recover safely, fully cycle the mount yourself first:\n  \
                 sudo braid lock\n  sudo braid recover\n\n\
                 braid lock works with a pending-operation journal and unmounts + \
                 closes LUKS, after which braid recover opens a fresh mount session \
                 and clears the staleness via the relock cycle."
                    .to_owned(),
            )),
        };
    }

    let probed_live_pool = if open_plan.is_none() && params.dry_run {
        // Pool is already mounted -- run the same read-only reconciliation
        // validation that execution's later probe_pool loop does. This
        // catches errors like "device X has no by-id path in either
        // snapshot" before claiming recovery is ready. Kept dry-run only
        // to preserve today's asymmetry: real-run already-mounted skips
        // this check because it happens implicitly downstream in
        // `execute()` when it walks the probed pool devices.
        let mount_point = params.config.mount_point();
        let pool = match probe::probe_pool(runner, fs, mount_point) {
            Ok(p) => p,
            Err(e) => {
                return RecoverPlanReport {
                    notes,
                    result: Err(e.into()),
                };
            }
        };
        if let Err(e) = validate_live_members_allowed(&pool, &union) {
            return RecoverPlanReport {
                notes,
                result: Err(e),
            };
        }
        Some(pool)
    } else {
        None
    };

    let mut actions = Vec::new();
    if open_plan.is_some() {
        actions.push(RecoverWorkAction::InitialOpenPool);
    }

    if is_replace_pool_mutation(&journal.op) && open_plan.is_some() {
        actions.push(RecoverWorkAction::WaitForKernelReplace);
    }

    if let Some(initial_open_plan) = &open_plan
        && is_replace_pool_mutation(&journal.op)
    {
        for name in &cycle_reopen_names {
            if !union.disks.contains_key(name) {
                return RecoverPlanReport {
                    notes,
                    result: Err(RecoverError::Failed(format!(
                        "recover remount cycle preview: disk '{name}' missing from membership union"
                    ))),
                };
            }
        }
        if cycle_reopen_names.is_empty() {
            return RecoverPlanReport {
                notes,
                result: Err(RecoverError::Failed(
                    "recover remount cycle preview: no disks available to reopen".into(),
                )),
            };
        }
        actions.push(RecoverWorkAction::RemountCycle {
            close_names: cycle_close_names,
            reopen_names: cycle_reopen_names,
            any_missing_member: initial_open_plan.any_missing_member,
        });
    }

    let completion = match &journal.op {
        journal::OpKind::Add {
            phase: journal::AddPhase::PoolMutation,
            targets,
        } if !journal.pre_membership.disks.is_empty() => {
            let all_targets_already_live = probed_live_pool
                .as_ref()
                .is_some_and(|pool| add_targets_all_live(pool, targets));
            let live_names = probed_live_pool.as_ref().map(live_member_names);
            RecoverCompletion::AddPoolMutation {
                targets: targets.clone(),
                all_targets_already_live,
                live_names,
            }
        }
        journal::OpKind::Add {
            phase: journal::AddPhase::PostAddBalanceRaid1,
            ..
        } => RecoverCompletion::AddPostBalance,
        journal::OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PoolMutation,
            devid,
            restore_raid1_after_commit,
        } => RecoverCompletion::RemoveMissingPoolMutation {
            devid: *devid,
            restore_raid1_after_commit: *restore_raid1_after_commit,
        },
        journal::OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
            devid,
            restore_raid1_after_commit,
        } => RecoverCompletion::RemoveMissingPostMaintenance {
            devid: *devid,
            restore_raid1_after_commit: *restore_raid1_after_commit,
        },
        journal::OpKind::Replace {
            phase: journal::ReplacePhase::PoolMutation,
            new_name,
            old_name,
            new_target,
            source,
            restore_raid1_after_commit,
            ..
        } => RecoverCompletion::ReplacePoolMutation {
            old_name: old_name.clone(),
            new_name: new_name.clone(),
            new_target: new_target.clone(),
            source: source.clone(),
            restore_raid1_after_commit: *restore_raid1_after_commit,
        },
        journal::OpKind::Replace {
            phase: journal::ReplacePhase::PostReplaceMaintenance,
            new_name,
            source,
            restore_raid1_after_commit,
            ..
        } => RecoverCompletion::ReplacePostMaintenance {
            new_name: new_name.clone(),
            source: source.clone(),
            restore_raid1_after_commit: *restore_raid1_after_commit,
        },
        journal::OpKind::Add { .. } => RecoverCompletion::GenericLivePool {
            replay_raid1_maintenance: true,
        },
        journal::OpKind::Remove { .. } => RecoverCompletion::GenericLivePool {
            replay_raid1_maintenance: false,
        },
    };
    actions.push(RecoverWorkAction::Complete(completion));

    let work_plan = RecoverWorkPlan {
        open_plan,
        pre_resolved_credential,
        journal,
        union,
        mount_point: params.config.mount_point().clone(),
        pool_json_path: params.paths.pool_json(),
        pending_op_path: params.paths.pending_op_json(),
        luks_headers_dir: params.paths.luks_headers_dir(),
        actions,
    };
    RecoverPlanReport {
        notes: Vec::new(),
        result: Ok(RecoverPlan { notes, work_plan }),
    }
}

pub fn cmd_recover<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
) -> Result<(), RecoverError> {
    let report = plan_recover(runner, fs, params);
    let plan = match report.result {
        Ok(p) => p,
        Err(e) => {
            // Preserved-context failure: any accumulated notes (entry
            // banner + per-disk probe events) render to stderr before
            // the error, mirroring today's `eprintln!(entry)` +
            // `mount::print_probe_events` + `?` sequence.
            eprint!(
                "{}",
                preview::render_notes_for_stderr_with(
                    &report.notes,
                    RecoverPlan::STDERR_STYLE,
                    crate::status_tag::color_enabled_for_stderr(),
                ),
            );
            return Err(e);
        }
    };

    if params.dry_run {
        plan.preview().print_colored();
        return Ok(());
    }

    plan.execute(runner, fs, by_id_resolver, params)
}

fn is_replace_pool_mutation(op: &journal::OpKind) -> bool {
    matches!(
        op,
        journal::OpKind::Replace {
            phase: journal::ReplacePhase::PoolMutation,
            ..
        }
    )
}

fn live_pool_matches_membership(pool: &PoolState, membership: &PoolMembership) -> bool {
    let live = live_member_names(pool);
    let missing_devids: std::collections::BTreeSet<u64> =
        pool.missing_devids.iter().copied().collect();
    let membership_missing_devids: std::collections::BTreeSet<u64> = membership
        .disks
        .values()
        .filter_map(|member| member.devid)
        .filter(|devid| missing_devids.contains(devid))
        .collect();
    if membership_missing_devids != missing_devids {
        return false;
    }

    let expected_live: std::collections::BTreeSet<String> = membership
        .disks
        .iter()
        .filter(|(_, member)| {
            member
                .devid
                .is_none_or(|devid| !missing_devids.contains(&devid))
        })
        .map(|(name, _)| name.clone())
        .collect();
    live == expected_live
}

fn recover_membership_matching_expected(
    pool: &PoolState,
    expected: &PoolMembership,
    prior: Option<&PoolMembership>,
    by_id_resolver: &dyn ByIdResolver,
) -> Result<PoolMembership, RecoverError> {
    let mut recovered = expected.clone();
    for dev in &pool.devices {
        let Some(name) = config::name_from_mapper(&dev.mapper.0) else {
            eprintln!("  skip: device {} has no braid- prefix", dev.mapper.0);
            continue;
        };
        if !expected.disks.contains_key(name) {
            return Err(RecoverError::Failed(format!(
                "device {} is in the live pool but is not part of the expected \
                 committed membership.",
                dev.mapper.0
            )));
        }
        let by_id = resolve_by_id_for_underlying(by_id_resolver, &dev.underlying)?;
        let added_at = prior
            .and_then(|p| p.disks.get(name))
            .and_then(|m| m.added_at.clone())
            .or_else(|| expected.disks.get(name).and_then(|m| m.added_at.clone()));
        recovered.disks.insert(
            name.to_owned(),
            DiskMember {
                by_id,
                luks_uuid: None,
                devid: None,
                added_at,
            },
        );
    }
    membership::enrich_from_pool_state(pool, &mut recovered);
    Ok(recovered)
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
    let journal::OpKind::Replace {
        old_name,
        new_name,
        new_by_id,
        new_target,
        source,
        restore_raid1_after_commit,
        ..
    } = &journal.op
    else {
        return Err(RecoverError::Journal(
            "cannot advance non-replace journal".into(),
        ));
    };
    journal::rewrite_journal(
        paths,
        journal,
        journal::OpKind::Replace {
            phase,
            old_name: old_name.clone(),
            new_name: new_name.clone(),
            new_by_id: new_by_id.clone(),
            new_target: new_target.clone(),
            source: source.clone(),
            restore_raid1_after_commit: *restore_raid1_after_commit,
        },
        target_membership,
    )
    .map_err(|e| RecoverError::Journal(e.to_string()))
}

fn replay_owed_raid1_maintenance<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &MountPoint,
    label: &str,
    pool: &PoolState,
    progress: ProgressOutput,
) -> Result<(), RecoverError> {
    let color_enabled = color_enabled_for_stderr();
    if let BalanceReport::Paused { .. } = get_balance_report(runner, mount_point) {
        eprint!(
            "{}",
            status_line(
                StatusTag::Wait,
                color_enabled,
                &format!("pool: resuming paused balance left by interrupted {label}..."),
            )
        );
        crate::pool::pool_balance_resume(runner, mount_point, progress)
            .map_err(|e| RecoverError::Failed(format!("recover balance resume: {e}")))?;
        eprint!(
            "{}",
            status_line(
                StatusTag::Ok,
                color_enabled,
                "pool: balance resume complete",
            )
        );
    }

    if pool.devices.len() >= 2 {
        eprint!(
            "{}",
            status_line(
                StatusTag::Wait,
                color_enabled,
                &format!(
                    "pool: replaying post-{label} RAID1 soft balance (skip already-RAID1 chunks)..."
                ),
            )
        );
        crate::pool::pool_balance_raid1_soft(runner, mount_point, progress)
            .map_err(|e| RecoverError::Failed(format!("recover balance replay: {e}")))?;
        eprint!(
            "{}",
            status_line(
                StatusTag::Ok,
                color_enabled,
                "pool: RAID1 soft balance replay complete",
            )
        );
    }
    Ok(())
}

/// Re-issue legacy generic post-mutation work after pool.json has been
/// rewritten and before the journal is cleared.
///
/// This helper is intentionally limited to generic Add balance replay plus
/// Remove's explicit no-op. Phased Add, Replace, and RemoveMissing recovery
/// use phase-specific handlers so post phases cannot accidentally rerun their
/// primary btrfs membership mutation.
///
///    `OpKind::Remove` is intentionally skipped for BOTH the resume and
///    the soft replay: the operator's recovery path is to re-run
///    `braid remove`, which itself runs the appropriate
///    `pool_balance_single`. Resuming a paused balance here would be
///    actively wrong for the 2->1 case -- `braid remove` runs
///    `pool_balance_single` BEFORE the device is dropped, so a shutdown
///    that lands during that pre-balance leaves the kernel with a paused
///    convert-to-single balance against a still-2-disk pool. Resuming it
///    would convert RAID1 -> single without ever removing the device,
///    then this function returns Ok and the journal gets cleared,
///    silently halving redundancy. Letting `braid remove` rerun handles
///    every shape (2->1 pre-balance, 3->2 / 4->3 with no pre-balance)
///    correctly.
///
///    For `OpKind::Add` the new disk is already in the pool (so
///    `braid add` would refuse on rerun), so recover-side replay avoids
///    stranding the operator with single-profile chunks they have to fix
///    manually with `btrfs balance start`.
fn replay_post_mutation<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &MountPoint,
    op: &journal::OpKind,
    pool: &PoolState,
    progress: ProgressOutput,
) -> Result<(), RecoverError> {
    let color_enabled = color_enabled_for_stderr();
    match op {
        journal::OpKind::Add { .. } => {
            if let BalanceReport::Paused { .. } = get_balance_report(runner, mount_point) {
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Wait,
                        color_enabled,
                        &format!(
                            "pool: resuming paused balance left by interrupted {}...",
                            journal_op_label(op)
                        ),
                    )
                );
                crate::pool::pool_balance_resume(runner, mount_point, progress)
                    .map_err(|e| RecoverError::Failed(format!("recover balance resume: {e}")))?;
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Ok,
                        color_enabled,
                        "pool: balance resume complete",
                    )
                );
            }

            if pool.devices.len() >= 2 {
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Wait,
                        color_enabled,
                        &format!(
                            "pool: replaying post-{} RAID1 soft balance (skip already-RAID1 chunks)...",
                            journal_op_label(op)
                        ),
                    )
                );
                crate::pool::pool_balance_raid1_soft(runner, mount_point, progress)
                    .map_err(|e| RecoverError::Failed(format!("recover balance replay: {e}")))?;
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Ok,
                        color_enabled,
                        "pool: RAID1 soft balance replay complete",
                    )
                );
            }
        }
        journal::OpKind::Remove { .. } => {
            // No resume, no replay. `braid remove` is the only mutation
            // whose pre-mutation phase issues a balance (the RAID1 ->
            // single conversion in the 2->1 case), so a paused balance
            // observed here may belong to an unfinished pre-remove rather
            // than to a post-mutation rebalance. Resuming it would
            // complete the conversion-to-single without removing the
            // device, then we'd clear the journal and silently lose
            // redundancy. The recovery_guidance message directs the
            // operator to re-run `braid remove` instead, which handles
            // every shape (2->1 pre-balance, 3->2 / 4->3 with no
            // pre-balance) correctly.
        }
        journal::OpKind::RemoveMissing { .. } | journal::OpKind::Replace { .. } => {
            return Err(RecoverError::Failed(
                "internal error: phased replace/remove-missing recovery reached generic replay"
                    .into(),
            ));
        }
    }

    Ok(())
}

fn journal_op_label(op: &journal::OpKind) -> &'static str {
    match op {
        journal::OpKind::Add { .. } => "add",
        journal::OpKind::Remove { .. } => "remove",
        journal::OpKind::RemoveMissing { .. } => "remove-missing",
        journal::OpKind::Replace { .. } => "replace",
    }
}

fn live_member_names(pool: &PoolState) -> std::collections::BTreeSet<String> {
    pool.devices
        .iter()
        .filter_map(|dev| config::name_from_mapper(&dev.mapper.0).map(str::to_owned))
        .collect()
}

fn validate_live_members_allowed(
    pool: &PoolState,
    allowed: &PoolMembership,
) -> Result<(), RecoverError> {
    for dev in &pool.devices {
        let Some(name) = config::name_from_mapper(&dev.mapper.0) else {
            continue;
        };
        if !allowed.disks.contains_key(name) {
            return Err(RecoverError::Failed(format!(
                "device {} is in the live pool but has no by-id path in either \
                 the pre-operation or target membership snapshot.\n\
                 This must be resolved manually -- provide the correct \
                 /dev/disk/by-id/ path and re-run recovery.",
                dev.mapper.0
            )));
        }
    }
    Ok(())
}

fn add_targets_all_live(
    pool: &PoolState,
    targets: &std::collections::BTreeMap<String, journal::AddJournalTarget>,
) -> bool {
    let live = live_member_names(pool);
    targets.keys().all(|name| live.contains(name))
}

fn build_membership_from_live_pool(
    pool: &PoolState,
    union: &PoolMembership,
    prior: Option<&PoolMembership>,
    by_id_resolver: &dyn ByIdResolver,
) -> Result<PoolMembership, RecoverError> {
    let mut recovered = PoolMembership::empty();
    for dev in &pool.devices {
        let Some(name) = config::name_from_mapper(&dev.mapper.0) else {
            eprintln!("  skip: device {} has no braid- prefix", dev.mapper.0);
            continue;
        };
        if !union.disks.contains_key(name) {
            return Err(RecoverError::Failed(format!(
                "device {} is in the live pool but has no by-id path in either \
                 the pre-operation or target membership snapshot.\n\
                 This must be resolved manually -- provide the correct \
                 /dev/disk/by-id/ path and re-run recovery.",
                dev.mapper.0
            )));
        }
        let by_id = resolve_by_id_for_underlying(by_id_resolver, &dev.underlying)?;
        let added_at = prior
            .and_then(|p| p.disks.get(name))
            .and_then(|m| m.added_at.clone())
            .or_else(|| union.disks.get(name).and_then(|m| m.added_at.clone()));
        recovered.disks.insert(
            name.to_owned(),
            DiskMember {
                by_id,
                luks_uuid: None,
                devid: None,
                added_at,
            },
        );
    }
    membership::enrich_from_pool_state(pool, &mut recovered);
    Ok(recovered)
}

fn write_add_phase(
    paths: &StatePaths,
    journal: &Journal,
    phase: journal::AddPhase,
) -> Result<Journal, RecoverError> {
    let mut next = journal.clone();
    if let journal::OpKind::Add { phase: p, .. } = &mut next.op {
        *p = phase;
    }
    journal::write_journal(paths, &next).map_err(|e| RecoverError::Journal(e.to_string()))?;
    Ok(next)
}

fn recover_passphrase<'a>(
    existing: Option<&'a OpenCredential>,
    params: &RecoverParams<'_>,
) -> Result<RecoverPassphrase<'a>, RecoverError> {
    match existing {
        Some(OpenCredential::Passphrase(passphrase)) => Ok(RecoverPassphrase::Borrowed(passphrase)),
        Some(OpenCredential::KeyFile(_)) => Err(RecoverError::Failed(
            "add recovery requires a passphrase for delayed LUKS format".into(),
        )),
        None => Ok(RecoverPassphrase::Owned(luks::read_passphrase(
            params.passphrase_file,
            params.passphrase_stdin,
        )?)),
    }
}

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

fn discover_add_targets_before_mount<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RecoverParams<'_>,
    journal: &Journal,
    targets: &std::collections::BTreeMap<String, journal::AddJournalTarget>,
) -> Result<Option<OpenCredential>, RecoverError> {
    let mount_result = runner.run(&CmdRequest::MountpointCheck {
        path: params.config.mount_point().clone(),
    })?;
    if mount_result.exit_status == 0 {
        return Ok(None);
    }

    let mut credential: Option<OpenCredential> = None;
    for (name, target) in targets {
        if journal.pre_membership.disks.contains_key(name) {
            continue;
        }

        let probed = probe::probe_config_disk(runner, fs, name, &target.by_id)?;
        let ConfigDiskState::PresentLuks {
            uuid,
            label,
            mapper_open,
        } = probed.state
        else {
            continue;
        };

        match &target.mode {
            journal::AddJournalMode::RecoverableBraidLabeled { luks_uuid, .. } => {
                if &uuid != luks_uuid {
                    continue;
                }
            }
            journal::AddJournalMode::FreshLuks { luks_label, .. } => {
                if label.as_deref() != Some(luks_label.as_str()) {
                    continue;
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
            luks::ensure_luks_open(runner, name, &target.by_id, passphrase)?;
        }

        scan_mapper_if_btrfs_visible(runner, &format!("/dev/mapper/{}", target.mapper_name))?;
    }

    Ok(credential)
}

fn verify_recover_passphrase_for_add_replay<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    pool: &PoolState,
    targets: &std::collections::BTreeMap<String, journal::AddJournalTarget>,
    passphrase: &Passphrase,
) -> Result<(), RecoverError> {
    let mut verify_targets: Vec<_> = pool
        .devices
        .iter()
        .map(|device| CredentialVerifyTarget {
            name: config::name_from_mapper(&device.mapper.0)
                .unwrap_or(device.mapper.0.as_str())
                .to_owned(),
            device: device.underlying.clone(),
        })
        .collect();
    if verify_targets.is_empty() {
        return Err(RecoverError::Failed(
            "cannot verify add recovery passphrase because no live pool members were found".into(),
        ));
    }

    let live = live_member_names(pool);
    for (name, target) in targets {
        if live.contains(name) {
            continue;
        }
        let probed = probe::probe_config_disk(runner, fs, name, &target.by_id)?;
        let ConfigDiskState::PresentLuks { uuid, label, .. } = probed.state else {
            continue;
        };
        match &target.mode {
            journal::AddJournalMode::RecoverableBraidLabeled { luks_uuid, .. } => {
                if &uuid != luks_uuid {
                    return Err(RecoverError::Failed(format!(
                        "recover add target '{}' LUKS UUID mismatch: expected {}, found {}",
                        name, luks_uuid, uuid
                    )));
                }
            }
            journal::AddJournalMode::FreshLuks { luks_label, .. } => {
                if label.as_deref() != Some(luks_label.as_str()) {
                    return Err(RecoverError::Failed(format!(
                        "recover add target '{}' has unexpected LUKS label",
                        name
                    )));
                }
            }
        }
        verify_targets.push(CredentialVerifyTarget {
            name: name.clone(),
            device: target.by_id.0.clone(),
        });
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
                target.name
            ))
        }
        crate::credential_verify::CredentialVerifyError::Luks { target, source } => {
            RecoverError::Failed(format!(
                "recover add credential verification failed on '{}': {source}",
                target.name
            ))
        }
    })
}

fn visible_btrfs_fsid<R: CommandRunner>(
    runner: &R,
    mapper_path: &str,
) -> Result<Option<String>, RecoverError> {
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
    key_file: &std::path::Path,
) -> Result<(), RecoverError> {
    luks::validate_user_keyfile_path(key_file)?;
    match luks::verify_key_file(runner, device, key_file)? {
        VerifyOutcome::Authenticated => Ok(()),
        VerifyOutcome::Rejected => {
            luks::enroll_key_file(runner, device, passphrase, key_file)?;
            Ok(())
        }
    }
}

fn execute_add_post_balance_recovery<R: CommandRunner + Sync>(
    runner: &R,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    journal: &Journal,
    union: &PoolMembership,
    pool: PoolState,
    inhibitor_already_held: bool,
) -> Result<(), RecoverError> {
    let live = live_member_names(&pool);
    let target: std::collections::BTreeSet<_> =
        journal.target_membership.disks.keys().cloned().collect();
    if live != target {
        return Err(RecoverError::Failed(format!(
            "post-add recovery expected live pool membership {:?}, found {:?}",
            target, live
        )));
    }

    let prior = membership::load_membership(params.paths).ok();
    let recovered = build_membership_from_live_pool(&pool, union, prior.as_ref(), by_id_resolver)?;
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
    replay_post_mutation(
        runner,
        params.config.mount_point(),
        &journal.op,
        &pool,
        params.progress,
    )?;
    journal::clear_journal(params.paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
    eprintln!("pending-op.json cleared. Recovery complete.");
    Ok(())
}

/// Per-replay state for the `add` PoolMutation recovery path: keeps the
/// replay-time inputs (credential, journal slice, union membership,
/// per-disk targets) and the live `PoolState` that the helper rebuilds
/// after opening any returned disks.
struct AddPoolReplayCtx<'a> {
    credential: Option<&'a OpenCredential>,
    journal: &'a Journal,
    union: &'a PoolMembership,
    targets: &'a std::collections::BTreeMap<String, journal::AddJournalTarget>,
    pool: PoolState,
}

fn execute_add_pool_mutation_recovery<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
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
    } = ctx;
    validate_live_members_allowed(&pool, union)?;
    let mount_point = params.config.mount_point();

    if !add_targets_all_live(&pool, targets) {
        let mut opened_or_scanned = false;
        let mut passphrase: Option<RecoverPassphrase<'_>> = None;
        for (name, target) in targets {
            if live_member_names(&pool).contains(name) {
                continue;
            }
            let probed = probe::probe_config_disk(runner, fs, name, &target.by_id)?;
            let ConfigDiskState::PresentLuks {
                uuid,
                label,
                mapper_open,
            } = probed.state
            else {
                continue;
            };

            match &target.mode {
                journal::AddJournalMode::RecoverableBraidLabeled { luks_uuid, .. } => {
                    if &uuid != luks_uuid {
                        continue;
                    }
                }
                journal::AddJournalMode::FreshLuks { luks_label, .. } => {
                    if label.as_deref() != Some(luks_label.as_str()) {
                        continue;
                    }
                }
            }

            if !mapper_open {
                if passphrase.is_none() {
                    passphrase = Some(recover_passphrase(credential, params)?);
                }
                let passphrase = passphrase
                    .as_ref()
                    .map(|p| p.expose_secret())
                    .expect("passphrase was resolved above");
                luks::ensure_luks_open(runner, name, &target.by_id, passphrase)?;
            }
            if scan_mapper_if_btrfs_visible(runner, &format!("/dev/mapper/{}", target.mapper_name))?
            {
                opened_or_scanned = true;
            }
        }

        if opened_or_scanned {
            pool = probe::probe_pool(runner, fs, mount_point)?;
            validate_live_members_allowed(&pool, union)?;
        }
    }

    if !add_targets_all_live(&pool, targets) {
        let passphrase = recover_passphrase(credential, params)?;
        verify_recover_passphrase_for_add_replay(
            runner,
            fs,
            &pool,
            targets,
            passphrase.expose_secret(),
        )?;
        let _guard = params
            .sleep_inhibitor
            .acquire("replaying interrupted add")
            .map_err(|e| RecoverError::Failed(format!("could not acquire sleep inhibitor: {e}")))?;

        for (name, target) in targets {
            if live_member_names(&pool).contains(name) {
                continue;
            }
            let mapper_path = format!("/dev/mapper/{}", target.mapper_name);
            match &target.mode {
                journal::AddJournalMode::RecoverableBraidLabeled {
                    verified_pool_fsid,
                    luks_uuid,
                } => {
                    let probed = probe::probe_config_disk(runner, fs, name, &target.by_id)?;
                    let ConfigDiskState::PresentLuks {
                        uuid, mapper_open, ..
                    } = probed.state
                    else {
                        return Err(RecoverError::Failed(format!(
                            "recover add target '{}' is not a LUKS device",
                            name
                        )));
                    };
                    if &uuid != luks_uuid {
                        return Err(RecoverError::Failed(format!(
                            "recover add target '{}' LUKS UUID mismatch: expected {}, found {}",
                            name, luks_uuid, uuid
                        )));
                    }
                    if !mapper_open {
                        luks::ensure_luks_open(
                            runner,
                            name,
                            &target.by_id,
                            passphrase.expose_secret(),
                        )?;
                    }
                    if let Some(fsid) = visible_btrfs_fsid(runner, &mapper_path)?
                        && &fsid != verified_pool_fsid
                    {
                        return Err(RecoverError::Failed(format!(
                            "recover add target '{}' btrfs FSID mismatch: expected {}, found {}",
                            name, verified_pool_fsid, fsid
                        )));
                    }
                    crate::pool::pool_add_device(runner, &mapper_path, mount_point, true)
                        .map_err(|e| RecoverError::Failed(format!("recover add replay: {e}")))?;
                }
                journal::AddJournalMode::FreshLuks {
                    luks_label,
                    luks_format_extra_opts,
                    enroll_key_file,
                } => {
                    let probed = probe::probe_config_disk(runner, fs, name, &target.by_id)?;
                    match probed.state {
                        ConfigDiskState::PresentNotLuks => {
                            luks::luks_format(
                                runner,
                                &target.by_id.0,
                                passphrase.expose_secret(),
                                luks_format_extra_opts,
                            )?;
                        }
                        ConfigDiskState::PresentLuks { label, .. } => {
                            if label.as_deref() != Some(luks_label.as_str()) {
                                return Err(RecoverError::Failed(format!(
                                    "recover add target '{}' has unexpected LUKS label",
                                    name
                                )));
                            }
                        }
                        ConfigDiskState::Absent => {
                            return Err(RecoverError::Failed(format!(
                                "recover add target '{}' ({}) is not present",
                                name, target.by_id
                            )));
                        }
                    }

                    if let Some(key_file) = enroll_key_file {
                        ensure_keyfile_enrolled(
                            runner,
                            &target.by_id.0,
                            passphrase.expose_secret(),
                            key_file,
                        )?;
                    }
                    luks::backup_luks_header(
                        runner,
                        &target.by_id.0,
                        &target.mapper_name,
                        params.paths,
                    )?;
                    luks::ensure_luks_open(
                        runner,
                        name,
                        &target.by_id,
                        passphrase.expose_secret(),
                    )?;
                    crate::pool::pool_add_device(runner, &mapper_path, mount_point, false)
                        .map_err(|e| RecoverError::Failed(format!("recover add replay: {e}")))?;
                }
            }
            pool = probe::probe_pool(runner, fs, mount_point)?;
            validate_live_members_allowed(&pool, union)?;
        }

        if !add_targets_all_live(&pool, targets) {
            return Err(RecoverError::Failed(
                "recover add replay finished but not every journaled target is live".into(),
            ));
        }

        let prior = membership::load_membership(params.paths).ok();
        let recovered =
            build_membership_from_live_pool(&pool, union, prior.as_ref(), by_id_resolver)?;
        membership::save_membership(&recovered, params.paths)?;
        eprintln!("pool.json written from completed add membership.");
        let journal = write_add_phase(
            params.paths,
            journal,
            journal::AddPhase::PostAddBalanceRaid1,
        )?;
        return execute_add_post_balance_recovery(
            runner,
            by_id_resolver,
            params,
            &journal,
            union,
            pool,
            true,
        );
    }

    let prior = membership::load_membership(params.paths).ok();
    let recovered = build_membership_from_live_pool(&pool, union, prior.as_ref(), by_id_resolver)?;
    membership::save_membership(&recovered, params.paths)?;
    eprintln!("pool.json written from completed add membership.");
    let journal = write_add_phase(
        params.paths,
        journal,
        journal::AddPhase::PostAddBalanceRaid1,
    )?;
    execute_add_post_balance_recovery(runner, by_id_resolver, params, &journal, union, pool, false)
}

fn execute_remove_missing_pool_mutation_recovery<R: CommandRunner + Sync>(
    runner: &R,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    journal: &Journal,
    pool: PoolState,
    devid: u64,
    restore_raid1_after_commit: bool,
) -> Result<(), RecoverError> {
    if pool.missing_devids.contains(&devid) {
        if !live_pool_matches_membership(&pool, &journal.pre_membership) {
            return Err(RecoverError::Failed(format!(
                "remove-missing recovery found devid {devid} still missing, but live pool \
                 topology does not match the pre-operation membership"
            )));
        }
        membership::save_membership(&journal.pre_membership, params.paths)?;
        journal::clear_journal(params.paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
        eprintln!(
            "remove-missing did not complete -- missing devid {devid} is still recorded. \
             Re-run braid remove-missing to retry."
        );
        return Ok(());
    }

    if !live_pool_matches_membership(&pool, &journal.target_membership) {
        return Err(RecoverError::Failed(format!(
            "remove-missing recovery found devid {devid} gone, but live pool topology \
             does not match the target membership"
        )));
    }

    let recovered = recover_membership_matching_expected(
        &pool,
        &journal.target_membership,
        membership::load_membership(params.paths).ok().as_ref(),
        by_id_resolver,
    )?;
    let journal = write_remove_missing_phase(
        params.paths,
        journal,
        journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
        Some(recovered),
    )?;
    execute_remove_missing_post_maintenance_recovery(
        runner,
        by_id_resolver,
        params,
        RemoveMissingPostCtx {
            journal: &journal,
            pool,
            devid,
            restore_raid1_after_commit,
            inhibitor_already_held: false,
        },
    )
}

/// Per-replay state for the post-remove-missing recovery path: bundles the
/// journal slice, the resumed `PoolState`, the devid invariants the helper
/// asserts, and the inhibitor coordination flag set by the caller chain.
struct RemoveMissingPostCtx<'a> {
    journal: &'a Journal,
    pool: PoolState,
    devid: u64,
    restore_raid1_after_commit: bool,
    inhibitor_already_held: bool,
}

fn execute_remove_missing_post_maintenance_recovery<R: CommandRunner + Sync>(
    runner: &R,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    ctx: RemoveMissingPostCtx<'_>,
) -> Result<(), RecoverError> {
    let RemoveMissingPostCtx {
        journal,
        pool,
        devid,
        restore_raid1_after_commit,
        inhibitor_already_held,
    } = ctx;
    if pool.missing_devids.contains(&devid) {
        return Err(RecoverError::Failed(format!(
            "post-remove-missing recovery expected devid {devid} to be gone, \
             but btrfs still reports it missing"
        )));
    }
    if !live_pool_matches_membership(&pool, &journal.target_membership) {
        return Err(RecoverError::Failed(
            "post-remove-missing recovery live pool does not match target membership".into(),
        ));
    }

    let prior = membership::load_membership(params.paths).ok();
    let recovered = recover_membership_matching_expected(
        &pool,
        &journal.target_membership,
        prior.as_ref(),
        by_id_resolver,
    )?;
    membership::save_membership(&recovered, params.paths)?;
    eprintln!("pool.json written from committed remove-missing membership.");

    let _guard = if inhibitor_already_held || !restore_raid1_after_commit {
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
    if restore_raid1_after_commit {
        replay_owed_raid1_maintenance(
            runner,
            params.config.mount_point(),
            "remove-missing",
            &pool,
            params.progress,
        )?;
    }
    journal::clear_journal(params.paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
    eprintln!("pending-op.json cleared. Recovery complete.");
    Ok(())
}

fn recover_passphrase_for_context<'a>(
    existing: Option<&'a OpenCredential>,
    params: &RecoverParams<'_>,
    context: &str,
) -> Result<RecoverPassphrase<'a>, RecoverError> {
    match existing {
        Some(OpenCredential::Passphrase(passphrase)) => Ok(RecoverPassphrase::Borrowed(passphrase)),
        Some(OpenCredential::KeyFile(_)) => Err(RecoverError::Failed(format!(
            "{context} requires a passphrase"
        ))),
        None => Ok(RecoverPassphrase::Owned(luks::read_passphrase(
            params.passphrase_file,
            params.passphrase_stdin,
        )?)),
    }
}

fn verify_replace_fresh_prep_passphrase<R: CommandRunner>(
    runner: &R,
    pool: &PoolState,
    new_name: &str,
    new_by_id: &ByIdPath,
    passphrase: &Passphrase,
) -> Result<(), RecoverError> {
    let mut targets: Vec<_> = pool
        .devices
        .iter()
        .map(|device| CredentialVerifyTarget {
            name: config::name_from_mapper(&device.mapper.0)
                .unwrap_or(device.mapper.0.as_str())
                .to_owned(),
            device: device.underlying.clone(),
        })
        .collect();
    targets.push(CredentialVerifyTarget {
        name: new_name.to_owned(),
        device: new_by_id.0.clone(),
    });
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
                target.name
            ))
        }
        crate::credential_verify::CredentialVerifyError::Luks { target, source } => {
            RecoverError::Failed(format!(
                "recover replace credential verification failed on '{}': {source}",
                target.name
            ))
        }
    })
}

/// Finish-time inputs for the uncommitted replace recovery branch: bundles
/// the credential, journal slice, pool snapshot, and incoming-disk
/// identifiers needed to retry the prep-only steps without touching the
/// live pool.
struct ReplaceFinishCtx<'a> {
    credential: Option<&'a OpenCredential>,
    journal: &'a Journal,
    pool: &'a PoolState,
    new_name: &'a str,
    new_target: &'a journal::ReplaceJournalTarget,
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
        new_name,
        new_target,
    } = ctx;
    match &new_target.mode {
        journal::ReplaceJournalMode::ExistingLuks { .. } => {
            membership::save_membership(&journal.pre_membership, params.paths)?;
            journal::clear_journal(params.paths)
                .map_err(|e| RecoverError::Journal(e.to_string()))?;
        }
        journal::ReplaceJournalMode::FreshLuks {
            luks_label,
            enroll_key_file,
            ..
        } => {
            let probed = probe::probe_config_disk(runner, fs, new_name, &new_target.by_id)?;
            match probed.state {
                ConfigDiskState::PresentNotLuks => {
                    membership::save_membership(&journal.pre_membership, params.paths)?;
                    journal::clear_journal(params.paths)
                        .map_err(|e| RecoverError::Journal(e.to_string()))?;
                }
                ConfigDiskState::PresentLuks { label, .. } => {
                    if label.as_deref() != Some(luks_label.as_str()) {
                        return Err(RecoverError::Failed(format!(
                            "recover replace target '{}' has unexpected LUKS label",
                            new_name
                        )));
                    }

                    let passphrase =
                        recover_passphrase_for_context(credential, params, "replace recovery")?;
                    verify_replace_fresh_prep_passphrase(
                        runner,
                        pool,
                        new_name,
                        &new_target.by_id,
                        passphrase.expose_secret(),
                    )?;
                    let _guard = params
                        .sleep_inhibitor
                        .acquire("finishing interrupted replace preparation")
                        .map_err(|e| {
                            RecoverError::Failed(format!("could not acquire sleep inhibitor: {e}"))
                        })?;
                    if let Some(key_file) = enroll_key_file {
                        ensure_keyfile_enrolled(
                            runner,
                            &new_target.by_id.0,
                            passphrase.expose_secret(),
                            key_file,
                        )?;
                    }
                    luks::backup_luks_header(
                        runner,
                        &new_target.by_id.0,
                        &new_target.mapper_name,
                        params.paths,
                    )?;
                    membership::save_membership(&journal.pre_membership, params.paths)?;
                    journal::clear_journal(params.paths)
                        .map_err(|e| RecoverError::Journal(e.to_string()))?;
                }
                ConfigDiskState::Absent => {
                    return Err(RecoverError::Failed(format!(
                        "recover replace target '{}' ({}) is not present",
                        new_name, new_target.by_id
                    )));
                }
            }
        }
    }

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
    old_name: &str,
    new_name: &str,
    new_target: &journal::ReplaceJournalTarget,
    source: &journal::ReplaceJournalSource,
    restore_raid1_after_commit: bool,
) -> Result<(), RecoverError> {
    validate_live_members_allowed(&pool, union)?;
    let live = live_member_names(&pool);
    let committed = live.contains(new_name) && !live.contains(old_name);
    let pre_topology =
        live_pool_matches_membership(&pool, &journal.pre_membership) && !live.contains(new_name);

    if committed {
        if !live_pool_matches_membership(&pool, &journal.target_membership) {
            return Err(RecoverError::Failed(
                "replace recovery found the new disk live, but live pool topology \
                 does not match the target membership"
                    .into(),
            ));
        }
        let recovered = recover_membership_matching_expected(
            &pool,
            &journal.target_membership,
            membership::load_membership(params.paths).ok().as_ref(),
            by_id_resolver,
        )?;
        let journal = write_replace_phase(
            params.paths,
            journal,
            journal::ReplacePhase::PostReplaceMaintenance,
            Some(recovered),
        )?;
        return execute_replace_post_maintenance_recovery(
            runner,
            by_id_resolver,
            params,
            &journal,
            pool,
            new_name,
            source,
            fs,
            restore_raid1_after_commit,
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
                new_name,
                new_target,
            },
        );
    }

    Err(RecoverError::Failed(
        "replace recovery live pool does not match either the pre-replace or \
         committed target topology; preserving pending-op.json"
            .into(),
    ))
}

fn close_old_mapper_best_effort<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mapper: &crate::types::MapperName,
) {
    if !fs.exists(&format!("/dev/mapper/{}", mapper.0)) {
        return;
    }
    let color_enabled = color_enabled_for_stderr();
    let old_label = mapper.0.strip_prefix("braid-").unwrap_or(&mapper.0);
    emit_status(&status_line(
        StatusTag::Wait,
        color_enabled,
        &format!("disk {old_label}: locking..."),
    ));
    let close_result = runner.run(&CmdRequest::CryptsetupClose {
        mapper: mapper.0.clone(),
    });
    match close_result {
        Ok(r) if r.exit_status == 0 => {
            emit_status(&status_line(
                StatusTag::Ok,
                color_enabled,
                &format!("disk {old_label}: locked"),
            ));
            eprintln!("Old device closed. If repurposing the physical disk, wipe it separately.");
        }
        Ok(r) => {
            emit_status(&status_line(
                StatusTag::Warn,
                color_enabled,
                &format!("disk {old_label}: lock failed (exit {})", r.exit_status),
            ));
        }
        Err(e) => {
            emit_status(&status_line(
                StatusTag::Warn,
                color_enabled,
                &format!("disk {old_label}: lock failed ({e})"),
            ));
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn execute_replace_post_maintenance_recovery<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
    journal: &Journal,
    pool: PoolState,
    new_name: &str,
    source: &journal::ReplaceJournalSource,
    fs: &F,
    restore_raid1_after_commit: bool,
    inhibitor_already_held: bool,
) -> Result<(), RecoverError> {
    if !live_pool_matches_membership(&pool, &journal.target_membership) {
        return Err(RecoverError::Failed(
            "post-replace recovery live pool does not match target membership".into(),
        ));
    }
    let prior = membership::load_membership(params.paths).ok();
    let recovered = recover_membership_matching_expected(
        &pool,
        &journal.target_membership,
        prior.as_ref(),
        by_id_resolver,
    )?;
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
        close_old_mapper_best_effort(runner, fs, old_mapper);
    }

    let new_mn = config::mapper_name(new_name);
    let Some(dev) = pool.devices.iter().find(|d| d.mapper == new_mn) else {
        return Err(RecoverError::Failed(format!(
            "post-replace recovery could not find new disk '{}' in the live pool",
            new_name
        )));
    };
    eprintln!(
        "Replaying post-replace resize on devid {} (new disk '{}')...",
        dev.devid, new_name
    );
    crate::pool::pool_resize_device(runner, dev.devid, params.config.mount_point())
        .map_err(|e| RecoverError::Failed(format!("recover replace resize: {e}")))?;

    if restore_raid1_after_commit {
        replay_owed_raid1_maintenance(
            runner,
            params.config.mount_point(),
            "replace",
            &pool,
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
/// Two failure modes are handled differently. A subprocess error from
/// `runner.run` (transient races, ENOMEM, signals) is best-effort: emit
/// `[warn]` and proceed, since a transient runner Err on a never-replaced
/// pool would otherwise force-fail every recover. `Suspended` and a parser
/// `Err` (unrecognised zero-exit stdout, e.g. an upstream wording change)
/// both mean we cannot reason about kernel replace state, so they emit
/// `[fail]` and return `RecoverError::Failed` -- preserving the journal so
/// the next `braid recover` can retry instead of racing the resume worker
/// and clearing `pending-op.json`.
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
                    StatusTag::Fail,
                    color_enabled,
                    "pool: kernel dev_replace canceled",
                ));
                return Ok(());
            }
            ReplaceState::Suspended { pct } => {
                let message = format!(
                    "pool: kernel dev_replace is suspended at {pct:.1}% (target device unavailable). Run `btrfs replace cancel {mount_point}` to clear it, then re-run `braid recover`."
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
/// the staleness — see plans/wip/sharded-drifting-beaver-findings.md.
fn relock_and_remount<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    allow_degraded: bool,
    credential: &OpenCredential,
    close_names: &[String],
) -> Result<(), RecoverError> {
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
        .map(|name| format!("/dev/mapper/{}", config::mapper_name(name).0))
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
        let mapper_path = format!("/dev/mapper/{}", mn.0);
        if !fs.exists(&mapper_path) {
            continue;
        }
        eprint!(
            "{}",
            status_line(
                StatusTag::Wait,
                color_enabled,
                &format!("disk {name}: locking..."),
            )
        );
        let close = runner
            .run(&CmdRequest::CryptsetupClose {
                mapper: mn.0.clone(),
            })
            .map_err(|e| {
                RecoverError::Failed(format!(
                    "recover remount cycle: cryptsetup close {}: {e}",
                    mn.0
                ))
            })?;
        if close.exit_status != 0 {
            return Err(RecoverError::Failed(format!(
                "recover remount cycle: cryptsetup close {} failed (exit {}): {}",
                mn.0,
                close.exit_status,
                close.stderr.trim()
            )));
        }
        eprint!(
            "{}",
            status_line(
                StatusTag::Ok,
                color_enabled,
                &format!("disk {name}: locked"),
            )
        );
    }

    // 4. Re-open LUKS and mount via the standard helper. With the dm
    //    devices freshly recreated and the cached fs_devices dropped, the
    //    kernel reads the chunk tree from disk and rebuilds a fresh
    //    fs_devices reflecting the post-resume on-disk state.
    //
    // The cycle just closed planned mappers, so the cycle's plan ALWAYS has
    // `to_unlock` non-empty — we always pass the credential. (If somehow
    // plan_open_pool returns None here it means another mounter raced us.)
    let cycle_report =
        mount::plan_open_pool(runner, fs, config, membership, allow_degraded, "recover");
    mount::print_probe_events(&cycle_report.events);
    let cycle_plan = cycle_report
        .result
        .map_err(|e| RecoverError::Failed(format!("recover remount cycle: plan: {e}")))?
        .ok_or_else(|| {
            RecoverError::Failed("recover remount cycle: pool already mounted after umount?".into())
        })?;
    match mount::execute_unlock_and_mount(runner, fs, config, &cycle_plan, credential) {
        Ok(_) => {}
        Err(failure) => {
            let _ = mount::close_opened_mappers(
                runner,
                &RealSleeper,
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
    pre_names: &std::collections::BTreeSet<&String>,
    target_names: &std::collections::BTreeSet<&String>,
    recovered_names: &std::collections::BTreeSet<&String>,
) -> String {
    if recovered_names == target_names {
        match op {
            journal::OpKind::Add { targets, .. } => {
                let names: Vec<_> = targets.keys().map(|n| format!("'{n}'")).collect();
                format!("add completed -- {} now in the pool.", names.join(", "))
            }
            journal::OpKind::Remove { name } => {
                format!("remove completed -- '{name}' is no longer in the pool.")
            }
            journal::OpKind::RemoveMissing { .. } => {
                "remove-missing completed -- missing device removed from the pool.".to_owned()
            }
            journal::OpKind::Replace {
                old_name, new_name, ..
            } => {
                format!("replace completed -- '{old_name}' replaced by '{new_name}'.")
            }
        }
    } else if recovered_names == pre_names {
        match op {
            journal::OpKind::Add { targets, .. } => {
                let names: Vec<_> = targets.keys().map(|n| format!("'{n}'")).collect();
                format!(
                    "add did not complete -- {} not in the pool. Re-run braid add to retry.",
                    names.join(", ")
                )
            }
            journal::OpKind::Remove { name } => {
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
            journal::OpKind::Replace { old_name, .. } => {
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

/// Merge pre_membership and target_membership into a single set of all known devices.
fn union_memberships(journal: &Journal) -> PoolMembership {
    let mut union = journal.pre_membership.clone();
    for (name, member) in &journal.target_membership.disks {
        union
            .disks
            .entry(name.clone())
            .or_insert_with(|| member.clone());
    }
    union
}

fn mount_membership_for_recover<'a>(
    journal: &'a Journal,
    union: &'a PoolMembership,
) -> &'a PoolMembership {
    match &journal.op {
        journal::OpKind::Add {
            phase: journal::AddPhase::PoolMutation,
            ..
        } if !journal.pre_membership.disks.is_empty() => &journal.pre_membership,
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
        journal::OpKind::Replace {
            phase: journal::ReplacePhase::PoolMutation,
            ..
        } => union,
        journal::OpKind::Replace {
            phase: journal::ReplacePhase::PostReplaceMaintenance,
            ..
        } => &journal.target_membership,
        journal::OpKind::Add {
            phase: journal::AddPhase::PoolMutation,
            ..
        } => union,
        journal::OpKind::Remove { .. } => union,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, MockRunner, RawCommandOutput};
    use crate::journal::{self, OpKind};
    use crate::mount::MountError;
    use crate::preview::NoteLevel;
    use crate::probe::Filesystem;
    use crate::types::{ByIdPath, LuksUuid, MapperName, MountPoint, PoolDevice};
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;

    fn passphrase(s: &str) -> Passphrase {
        Passphrase::from_zeroizing(zeroize::Zeroizing::new(s.to_owned()))
    }

    fn write_valid_keyfile(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
        let key_file = dir.path().join(name);
        std::fs::write(&key_file, vec![0u8; luks::KEYFILE_SIZE]).unwrap();
        key_file
    }

    struct MockFs {
        paths: Vec<String>,
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
            }
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
                return Ok(
                    "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                        .to_owned(),
                );
            }
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    /// Shared `(Arc<Mutex<HashSet<String>>>)` so the StatefulMockFs and
    /// MapperClosingRunner can both observe path mutations.
    /// `CommandRunner: Sync`, so `Rc<RefCell<...>>` is not usable here.
    type SharedPaths = std::sync::Arc<std::sync::Mutex<std::collections::HashSet<String>>>;

    struct NoopInhibitor;

    impl AcquireSleepInhibitor for NoopInhibitor {
        fn acquire(&self, _why: &str) -> std::io::Result<Box<dyn crate::inhibit::SleepGuard>> {
            Ok(Box::new(()))
        }
    }

    static NOOP_INHIBITOR: NoopInhibitor = NoopInhibitor;

    struct FailingInhibitor;

    impl AcquireSleepInhibitor for FailingInhibitor {
        fn acquire(&self, _why: &str) -> std::io::Result<Box<dyn crate::inhibit::SleepGuard>> {
            Err(std::io::Error::other("forced inhibitor failure"))
        }
    }

    struct RequestCountInhibitor {
        runner: MockRunner,
        first_acquire_request_count: std::cell::Cell<Option<usize>>,
        acquire_count: std::cell::Cell<usize>,
    }

    impl RequestCountInhibitor {
        fn new(runner: MockRunner) -> Self {
            Self {
                runner,
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
    }

    impl AcquireSleepInhibitor for RequestCountInhibitor {
        fn acquire(&self, _why: &str) -> std::io::Result<Box<dyn crate::inhibit::SleepGuard>> {
            self.acquire_count.set(self.acquire_count.get() + 1);
            if self.first_acquire_request_count.get().is_none() {
                self.first_acquire_request_count
                    .set(Some(self.runner.requests().len()));
            }
            Ok(Box::new(()))
        }
    }

    /// Mock filesystem with interior mutability so test code can model
    /// device-mapper paths disappearing when `cryptsetup close` runs.
    /// Used together with `MapperClosingRunner` for tests that exercise
    /// the recover relock cycle on initially-open mappers.
    struct StatefulMockFs {
        paths: SharedPaths,
    }

    impl StatefulMockFs {
        fn new(initial: &[&str]) -> Self {
            Self {
                paths: std::sync::Arc::new(std::sync::Mutex::new(
                    initial.iter().map(|s| s.to_string()).collect(),
                )),
            }
        }

        fn handle(&self) -> SharedPaths {
            std::sync::Arc::clone(&self.paths)
        }
    }

    impl Filesystem for StatefulMockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.lock().unwrap().contains(path)
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }

        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path == "/proc/self/mountinfo" {
                return Ok(
                    "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                        .to_owned(),
                );
            }
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    /// Wraps a `MockRunner` and removes `/dev/mapper/<mapper>` from a shared
    /// `StatefulMockFs` whenever a `CryptsetupClose` request succeeds. Also
    /// tracks which mappers have been closed so subsequent `CryptsetupStatus`
    /// queries on those mappers report inactive, mirroring real-kernel
    /// behavior across the recover relock cycle.
    struct MapperClosingRunner {
        inner: MockRunner,
        fs_paths: SharedPaths,
        closed: std::sync::Mutex<std::collections::HashSet<String>>,
    }

    impl MapperClosingRunner {
        fn inactive_status(mapper: &str) -> RawCommandOutput {
            RawCommandOutput {
                cmd: format!("cryptsetup status {mapper}"),
                stdout: String::new(),
                stderr: format!("/dev/mapper/{mapper} is inactive.\n"),
                exit_status: 4,
            }
        }
    }

    impl CommandRunner for MapperClosingRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, crate::cmd::CmdError> {
            if let CmdRequest::CryptsetupStatus { mapper } = request
                && self.closed.lock().unwrap().contains(mapper)
            {
                return Ok(Self::inactive_status(mapper));
            }
            let result = self.inner.run(request)?;
            match request {
                CmdRequest::CryptsetupClose { mapper } if result.exit_status == 0 => {
                    self.fs_paths
                        .lock()
                        .unwrap()
                        .remove(&format!("/dev/mapper/{}", mapper));
                    self.closed.lock().unwrap().insert(mapper.clone());
                }
                CmdRequest::CryptsetupLuksOpen { mapper, .. }
                | CmdRequest::CryptsetupLuksOpenKeyFile { mapper, .. }
                    if result.exit_status == 0 =>
                {
                    self.fs_paths
                        .lock()
                        .unwrap()
                        .insert(format!("/dev/mapper/{}", mapper));
                    self.closed.lock().unwrap().remove(mapper);
                }
                _ => {}
            }
            Ok(result)
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            stdin: &[u8],
        ) -> Result<RawCommandOutput, crate::cmd::CmdError> {
            let result = self.inner.run_with_stdin(request, stdin)?;
            match request {
                CmdRequest::CryptsetupLuksOpen { mapper, .. }
                | CmdRequest::CryptsetupLuksOpenKeyFile { mapper, .. }
                    if result.exit_status == 0 =>
                {
                    self.fs_paths
                        .lock()
                        .unwrap()
                        .insert(format!("/dev/mapper/{}", mapper));
                    self.closed.lock().unwrap().remove(mapper);
                }
                _ => {}
            }
            Ok(result)
        }
    }

    /// Test resolver for `ByIdResolver`. `entries` is what `list_by_id_entries`
    /// returns; `canonicalize_results` is the symlink → canonical-path map used
    /// by `canonicalize`. Unmocked paths return `NotFound`.
    #[derive(Default)]
    struct MockByIdResolver {
        entries: Vec<String>,
        canonicalize_results: BTreeMap<String, String>,
    }

    impl MockByIdResolver {
        fn with_entries<I, S>(mut self, entries: I) -> Self
        where
            I: IntoIterator<Item = S>,
            S: Into<String>,
        {
            self.entries = entries.into_iter().map(Into::into).collect();
            self
        }

        fn with_canonical(mut self, path: &str, target: &str) -> Self {
            self.canonicalize_results
                .insert(path.to_string(), target.to_string());
            self
        }
    }

    impl ByIdResolver for MockByIdResolver {
        fn list_by_id_entries(&self) -> Result<Vec<String>, std::io::Error> {
            Ok(self.entries.clone())
        }

        fn canonicalize(&self, path: &str) -> Result<String, std::io::Error> {
            self.canonicalize_results.get(path).cloned().ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::NotFound, format!("mock: {path}"))
            })
        }
    }

    /// Build a `MockByIdResolver` from `(underlying, by_id_filename)` pairs.
    /// For each pair, the by-id entry is registered and both the entry and the
    /// underlying canonicalize to the underlying path. Use this for success-path
    /// tests where the resolver should find a matching entry per pool device.
    fn resolver_for(mappings: &[(&str, &str)]) -> MockByIdResolver {
        let mut resolver = MockByIdResolver::default();
        for (underlying, filename) in mappings {
            resolver.entries.push((*filename).to_string());
            resolver.canonicalize_results.insert(
                format!("/dev/disk/by-id/{filename}"),
                (*underlying).to_string(),
            );
            resolver
                .canonicalize_results
                .insert((*underlying).to_string(), (*underlying).to_string());
        }
        resolver
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
        let mount_point = MountPoint("/mnt/storage".into());
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
    // Intent: canceled kernel dev_replace reports a fail row but does not abort
    // recovery after an observed in-flight wait.
    // Why it exists: canceled means the kernel rolled topology back; downstream
    // replace recovery can still classify and clean up the journal safely.
    // Scenario: recover observes one running poll, then the kernel reports
    // "canceled on" for the same replace.
    fn wait_for_kernel_replace_emits_fail_on_canceled_returns_ok() {
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
        let mount_point = MountPoint("/mnt/storage".into());
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
                "[fail] pool: kernel dev_replace canceled",
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
        let mount_point = MountPoint("/mnt/storage".into());
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
                "[fail] pool: kernel dev_replace is suspended at 12.5% (target device unavailable). Run `btrfs replace cancel /mnt/storage` to clear it, then re-run `braid recover`.",
            ],
        );
    }

    #[test]
    // Intent: canceled kernel dev_replace reports a fail row even when it is
    // the first status observed.
    // Why it exists: canceled is terminal and diagnostic, not a normal
    // "nothing to wait for" condition gated by a prior wait row.
    // Scenario: recover mounts after the kernel already transitioned the
    // resumed replace to CANCELED.
    fn wait_for_kernel_replace_emits_fail_on_canceled_first_poll() {
        let runner = ReplaceStatusSequenceRunner::new(vec![ReplaceStatusItem::Output(ok_raw(
            "btrfs replace status -1 /mnt/storage",
            "Started on 27.Feb 10:30:00, canceled on 27.Feb 10:35:00 at 0.0%, 0 write errs, 0 uncorr. read errs\n",
        ))]);
        let mount_point = MountPoint("/mnt/storage".into());
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
            vec!["[fail] pool: kernel dev_replace canceled"],
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
        let mount_point = MountPoint("/mnt/storage".into());
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
        let mount_point = MountPoint("/mnt/storage".into());
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
        let mount_point = MountPoint("/mnt/storage".into());
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
        let mount_point = MountPoint("/mnt/storage".into());
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
        let mount_point = MountPoint("/mnt/storage".into());
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
        let mount_point = MountPoint("/mnt/storage".into());
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
        let mount_point = MountPoint("/mnt/storage".into());
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
                path: MountPoint("/mnt/storage".into()),
            },
            ok_raw_empty("mountpoint"),
        )
    }

    fn mountpoint_fail() -> (CmdRequest, RawCommandOutput) {
        (
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".into()),
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

    fn btrfs_show_one_disk() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 1 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n",
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

    const POOL_JSON_ADDED_AT: &str = "2024-06-15T12:34:56Z";
    const JOURNAL_ADDED_AT: &str = "2023-08-30T10:00:00Z";
    const LEGACY_JOURNAL_ADDED_AT: &str = "2023-01-01T00:00:00Z";

    fn disk_member(by_id: &str, added_at: Option<&str>) -> DiskMember {
        DiskMember {
            by_id: ByIdPath(by_id.to_owned()),
            luks_uuid: None,
            devid: None,
            added_at: added_at.map(str::to_owned),
        }
    }

    fn disk_member_with_devid(by_id: &str, devid: u64) -> DiskMember {
        DiskMember {
            by_id: ByIdPath(by_id.to_owned()),
            luks_uuid: None,
            devid: Some(devid),
            added_at: None,
        }
    }

    fn add_op_from_disks(disks: BTreeMap<String, ByIdPath>) -> OpKind {
        OpKind::Add {
            phase: journal::AddPhase::PostAddBalanceRaid1,
            targets: disks
                .into_iter()
                .map(|(name, by_id)| {
                    (
                        name.clone(),
                        journal::AddJournalTarget {
                            by_id,
                            mapper_name: config::mapper_name(&name).0,
                            mode: journal::AddJournalMode::FreshLuks {
                                luks_label: format!("braid-{name}"),
                                luks_format_extra_opts: vec![
                                    "--label".into(),
                                    format!("braid-{name}"),
                                ],
                                enroll_key_file: None,
                            },
                        },
                    )
                })
                .collect(),
        }
    }

    fn already_mounted_one_disk_runner() -> MockRunner {
        let (mp_req, mp_out) = mountpoint_ok();
        MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_one_disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            disk_member("/dev/disk/by-id/virtio-disk1", disk1_added_at),
        );
        pre_disks.insert(
            "disk2".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        let pre = PoolMembership { disks: pre_disks };

        let mut target_disks = BTreeMap::new();
        target_disks.insert(
            "disk1".to_owned(),
            disk_member("/dev/disk/by-id/virtio-disk1", disk1_added_at),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Remove {
                name: "disk2".to_owned(),
            },
            pre_membership: pre,
            target_membership: PoolMembership {
                disks: target_disks,
            },
        }
    }

    fn remove_missing_journal() -> journal::Journal {
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk1", 1),
        );
        pre_disks.insert(
            "disk2".to_owned(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk2", 2),
        );

        let mut target_disks = BTreeMap::new();
        target_disks.insert(
            "disk1".to_owned(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk1", 1),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::RemoveMissing {
                phase: journal::RemoveMissingPhase::PoolMutation,
                devid: 2,
                restore_raid1_after_commit: true,
            },
            pre_membership: PoolMembership { disks: pre_disks },
            target_membership: PoolMembership {
                disks: target_disks,
            },
        }
    }

    fn remove_missing_journal_two_survivors() -> journal::Journal {
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk1", 1),
        );
        pre_disks.insert(
            "disk2".to_owned(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk2", 2),
        );
        pre_disks.insert(
            "disk3".to_owned(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk3", 3),
        );

        let mut target_disks = BTreeMap::new();
        target_disks.insert(
            "disk1".to_owned(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk1", 1),
        );
        target_disks.insert(
            "disk2".to_owned(),
            disk_member_with_devid("/dev/disk/by-id/virtio-disk2", 2),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::RemoveMissing {
                phase: journal::RemoveMissingPhase::PoolMutation,
                devid: 3,
                restore_raid1_after_commit: true,
            },
            pre_membership: PoolMembership { disks: pre_disks },
            target_membership: PoolMembership {
                disks: target_disks,
            },
        }
    }

    /// Two-disk journal for interrupted add: pre has disk1+disk2, target has disk1+disk2+disk3.
    fn two_disk_journal() -> journal::Journal {
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        pre_disks.insert(
            "disk2".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        let pre = PoolMembership { disks: pre_disks };

        let mut target_disks = pre.disks.clone();
        target_disks.insert(
            "disk3".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk3".into())),
        );
        let target = PoolMembership {
            disks: target_disks,
        };

        let mut add_disks = BTreeMap::new();
        add_disks.insert(
            "disk3".to_owned(),
            ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: add_op_from_disks(add_disks),
            pre_membership: pre,
            target_membership: target,
        }
    }

    /// Add journal for the committed post-add phase: pre has disk1,
    /// target/live pool has disk1+disk2, and recovery only owes balance.
    fn committed_two_disk_add_journal() -> journal::Journal {
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let pre = PoolMembership { disks: pre_disks };

        let mut target_disks = pre.disks.clone();
        target_disks.insert(
            "disk2".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        let target = PoolMembership {
            disks: target_disks,
        };

        let mut add_disks = BTreeMap::new();
        add_disks.insert(
            "disk2".to_owned(),
            ByIdPath("/dev/disk/by-id/virtio-disk2".into()),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: add_op_from_disks(add_disks),
            pre_membership: pre,
            target_membership: target,
        }
    }

    fn recoverable_pool_mutation_add_journal() -> journal::Journal {
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let pre = PoolMembership { disks: pre_disks };

        let mut target_disks = pre.disks.clone();
        target_disks.insert(
            "disk2".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        let target = PoolMembership {
            disks: target_disks,
        };

        let mut targets = BTreeMap::new();
        targets.insert(
            "disk2".to_owned(),
            journal::AddJournalTarget {
                by_id: ByIdPath("/dev/disk/by-id/virtio-disk2".into()),
                mapper_name: "braid-disk2".into(),
                mode: journal::AddJournalMode::RecoverableBraidLabeled {
                    verified_pool_fsid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    luks_uuid: LuksUuid("22222222-2222-2222-2222-222222222222".into()),
                },
            },
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

    fn two_pre_recoverable_add_disk3_journal() -> journal::Journal {
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        pre_disks.insert(
            "disk2".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        let pre = PoolMembership { disks: pre_disks };

        let mut target_disks = pre.disks.clone();
        target_disks.insert(
            "disk3".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk3".into())),
        );
        let target = PoolMembership {
            disks: target_disks,
        };

        let mut targets = BTreeMap::new();
        targets.insert(
            "disk3".to_owned(),
            journal::AddJournalTarget {
                by_id: ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
                mapper_name: "braid-disk3".into(),
                mode: journal::AddJournalMode::RecoverableBraidLabeled {
                    verified_pool_fsid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    luks_uuid: LuksUuid("33333333-3333-3333-3333-333333333333".into()),
                },
            },
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

    fn two_target_recoverable_pool_mutation_add_journal() -> journal::Journal {
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let pre = PoolMembership { disks: pre_disks };

        let mut target_disks = pre.disks.clone();
        for name in ["disk2", "disk3"] {
            target_disks.insert(
                name.to_owned(),
                DiskMember::from_by_id(ByIdPath(format!("/dev/disk/by-id/virtio-{name}"))),
            );
        }
        let target = PoolMembership {
            disks: target_disks,
        };

        let mut targets = BTreeMap::new();
        for (name, uuid) in [
            ("disk2", "22222222-2222-2222-2222-222222222222"),
            ("disk3", "33333333-3333-3333-3333-333333333333"),
        ] {
            targets.insert(
                name.to_owned(),
                journal::AddJournalTarget {
                    by_id: ByIdPath(format!("/dev/disk/by-id/virtio-{name}")),
                    mapper_name: format!("braid-{name}"),
                    mode: journal::AddJournalMode::RecoverableBraidLabeled {
                        verified_pool_fsid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                        luks_uuid: LuksUuid(uuid.into()),
                    },
                },
            );
        }

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
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let pre = PoolMembership { disks: pre_disks };

        let mut target_disks = pre.disks.clone();
        for name in ["disk2", "disk3"] {
            target_disks.insert(
                name.to_owned(),
                DiskMember::from_by_id(ByIdPath(format!("/dev/disk/by-id/virtio-{name}"))),
            );
        }
        let target = PoolMembership {
            disks: target_disks,
        };

        let mut targets = BTreeMap::new();
        targets.insert(
            "disk2".to_owned(),
            journal::AddJournalTarget {
                by_id: ByIdPath("/dev/disk/by-id/virtio-disk2".into()),
                mapper_name: "braid-disk2".into(),
                mode: journal::AddJournalMode::RecoverableBraidLabeled {
                    verified_pool_fsid: "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into(),
                    luks_uuid: LuksUuid("22222222-2222-2222-2222-222222222222".into()),
                },
            },
        );
        targets.insert(
            "disk3".to_owned(),
            journal::AddJournalTarget {
                by_id: ByIdPath("/dev/disk/by-id/virtio-disk3".into()),
                mapper_name: "braid-disk3".into(),
                mode: journal::AddJournalMode::FreshLuks {
                    luks_label: "braid-disk3".into(),
                    luks_format_extra_opts: vec!["--label".into(), "braid-disk3".into()],
                    enroll_key_file: Some(std::path::PathBuf::from(
                        "/var/lib/braid/keyfiles/braid-disk3.key",
                    )),
                },
            },
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

    fn fresh_pool_mutation_add_journal(
        luks_format_extra_opts: Vec<String>,
        enroll_key_file: Option<std::path::PathBuf>,
    ) -> journal::Journal {
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let pre = PoolMembership { disks: pre_disks };

        let mut target_disks = pre.disks.clone();
        target_disks.insert(
            "disk2".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        let target = PoolMembership {
            disks: target_disks,
        };

        let mut targets = BTreeMap::new();
        targets.insert(
            "disk2".to_owned(),
            journal::AddJournalTarget {
                by_id: ByIdPath("/dev/disk/by-id/virtio-disk2".into()),
                mapper_name: "braid-disk2".into(),
                mode: journal::AddJournalMode::FreshLuks {
                    luks_label: "braid-disk2".into(),
                    luks_format_extra_opts,
                    enroll_key_file,
                },
            },
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

    fn pool_state_one_disk() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk1".into()),
                luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
                devid: 1,
                underlying: "/dev/vda".into(),
            }],
            missing_count: 0,
            total_devices: 1,
            fsid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    fn pool_state_two_disks() -> PoolState {
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
            total_devices: 2,
            fsid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    fn pool_state_disk1_and_old() -> PoolState {
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
                    mapper: MapperName("braid-old".into()),
                    luks_uuid: LuksUuid("22222222-2222-2222-2222-222222222222".into()),
                    devid: 2,
                    underlying: "/dev/vdb".into(),
                },
            ],
            missing_count: 0,
            total_devices: 2,
            fsid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    fn pool_state_disk1_and_new() -> PoolState {
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
                    mapper: MapperName("braid-new".into()),
                    luks_uuid: LuksUuid("33333333-3333-3333-3333-333333333333".into()),
                    devid: 2,
                    underlying: "/dev/vdc".into(),
                },
            ],
            missing_count: 0,
            total_devices: 2,
            fsid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    fn pool_state_disk1_old_and_new() -> PoolState {
        let mut pool = pool_state_disk1_and_old();
        pool.devices.push(PoolDevice {
            mapper: MapperName("braid-new".into()),
            luks_uuid: LuksUuid("33333333-3333-3333-3333-333333333333".into()),
            devid: 3,
            underlying: "/dev/vdc".into(),
        });
        pool.total_devices = 3;
        pool
    }

    fn pool_state_disk1_with_missing_devid2() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk1".into()),
                luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
                devid: 1,
                underlying: "/dev/vda".into(),
            }],
            missing_count: 1,
            total_devices: 2,
            fsid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
            missing_devids: vec![2],
            null_underlying: vec![],
        }
    }

    fn pool_state_three_disks() -> PoolState {
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
                PoolDevice {
                    mapper: MapperName("braid-disk3".into()),
                    luks_uuid: LuksUuid("33333333-3333-3333-3333-333333333333".into()),
                    devid: 3,
                    underlying: "/dev/vdc".into(),
                },
            ],
            missing_count: 0,
            total_devices: 3,
            fsid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
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
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_one_disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk2".into(),
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

    fn with_three_disk_pool_probe(runner: MockRunner) -> MockRunner {
        runner
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_three_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk2".into(),
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
                    mapper: "braid-disk3".into(),
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

    fn with_balance_replay(runner: MockRunner) -> MockRunner {
        runner
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsBalanceRaid1Soft {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs balance start"),
            )
    }

    fn recover_params<'a>(
        config: &'a Config,
        paths: &'a StatePaths,
        passphrase_file: Option<&'a std::path::Path>,
        dry_run: bool,
    ) -> RecoverParams<'a> {
        recover_params_with_inhibitor(config, paths, passphrase_file, dry_run, &NOOP_INHIBITOR)
    }

    fn recover_params_with_inhibitor<'a>(
        config: &'a Config,
        paths: &'a StatePaths,
        passphrase_file: Option<&'a std::path::Path>,
        dry_run: bool,
        sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
    ) -> RecoverParams<'a> {
        RecoverParams {
            config,
            paths,
            passphrase_stdin: false,
            passphrase_file,
            allow_degraded: false,
            dry_run,
            progress: ProgressOutput::Off,
            sleep_inhibitor,
        }
    }

    #[test]
    fn plan_recover_discovers_add_targets_before_mount_planning() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = StatefulMockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);
        let fs_handle = fs.handle();

        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

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
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
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
        let request_log = inner.clone();
        let runner = MapperClosingRunner {
            inner,
            fs_paths: fs_handle,
            closed: Mutex::new(["braid-disk1".to_owned(), "braid-disk2".to_owned()].into()),
        };

        let params = RecoverParams {
            config: &config,
            paths: &paths,
            passphrase_stdin: false,
            passphrase_file: Some(passphrase_file.path()),
            allow_degraded: false,
            dry_run: false,
            progress: ProgressOutput::Off,
            sleep_inhibitor: &NOOP_INHIBITOR,
        };

        let plan = plan_recover(&runner, &fs, &params)
            .result
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
                "disk1".to_owned(),
                ByIdPath("/dev/disk/by-id/virtio-disk1".into())
            )]
        );

        let requests = request_log.requests();
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
                        if device == "/dev/disk/by-id/virtio-disk2" && mapper == "braid-disk2"
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = StatefulMockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);
        let fs_handle = fs.handle();

        let journal =
            fresh_pool_mutation_add_journal(vec!["--label".into(), "braid-disk2".into()], None);
        journal::write_journal(&paths, &journal).unwrap();

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

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
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShowTarget {
                    target: "/dev/mapper/braid-disk2".into(),
                },
                btrfs_show_target_no_btrfs("/dev/mapper/braid-disk2"),
            );
        let request_log = inner.clone();
        let runner = MapperClosingRunner {
            inner,
            fs_paths: fs_handle,
            closed: Mutex::new(["braid-disk1".to_owned(), "braid-disk2".to_owned()].into()),
        };

        let params = recover_params(&config, &paths, Some(passphrase_file.path()), false);
        let plan = plan_recover(&runner, &fs, &params)
            .result
            .expect("planner should discover fresh add target, then plan from pre-membership");
        let open_plan = plan
            .work_plan
            .open_plan
            .expect("pool should still need initial mount");
        assert_eq!(
            open_plan.to_unlock,
            vec![(
                "disk1".to_owned(),
                ByIdPath("/dev/disk/by-id/virtio-disk1".into())
            )]
        );

        let requests = request_log.requests();
        let disk2_open = requests
            .iter()
            .position(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksOpen { device, mapper }
                        if device == "/dev/disk/by-id/virtio-disk2" && mapper == "braid-disk2"
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&paths, &journal).unwrap();

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
                    mapper: "braid-disk2".into(),
                },
                MapperClosingRunner::inactive_status("braid-disk2"),
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
                    mapper: "braid-disk1".into(),
                },
                MapperClosingRunner::inactive_status("braid-disk1"),
            );
        let request_log = runner.clone();

        let params = recover_params(&config, &paths, None, false);
        let plan = plan_recover(&runner, &fs, &params)
            .result
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
                "disk1".to_owned(),
                ByIdPath("/dev/disk/by-id/virtio-disk1".into())
            )]
        );

        let requests = request_log.requests();
        assert!(
            !requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksOpen { mapper, .. }
                        if mapper == "braid-disk2"
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal =
            fresh_pool_mutation_add_journal(vec!["--label".into(), "braid-disk2".into()], None);
        journal::write_journal(&paths, &journal).unwrap();

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
                    mapper: "braid-disk2".into(),
                },
                MapperClosingRunner::inactive_status("braid-disk2"),
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
                    mapper: "braid-disk1".into(),
                },
                MapperClosingRunner::inactive_status("braid-disk1"),
            );
        let request_log = runner.clone();

        let params = recover_params(&config, &paths, None, false);
        let plan = plan_recover(&runner, &fs, &params)
            .result
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
                "disk1".to_owned(),
                ByIdPath("/dev/disk/by-id/virtio-disk1".into())
            )]
        );

        let requests = request_log.requests();
        assert!(
            !requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksOpen { mapper, .. }
                        if mapper == "braid-disk2"
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-disk3",
        ]);

        let journal = two_target_recoverable_pool_mutation_add_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

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
                    mapper: "braid-disk2".into(),
                },
                MapperClosingRunner::inactive_status("braid-disk2"),
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
                    mapper: "braid-disk3".into(),
                },
                MapperClosingRunner::inactive_status("braid-disk3"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk3".into(),
                    mapper: "braid-disk3".into(),
                },
                b"testpass".to_vec(),
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
                    mapper: "braid-disk1".into(),
                },
                MapperClosingRunner::inactive_status("braid-disk1"),
            );
        let request_log = runner.clone();

        let params = recover_params(&config, &paths, Some(passphrase_file.path()), false);
        let plan = plan_recover(&runner, &fs, &params)
            .result
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
                        if mapper == "braid-disk2"
                )
            }),
            "mismatched first target must not be opened"
        );
        assert!(
            requests.iter().any(|r| {
                matches!(
                    r,
                    CmdRequest::CryptsetupLuksOpen { device, mapper }
                        if device == "/dev/disk/by-id/virtio-disk3" && mapper == "braid-disk3"
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

    #[test]
    fn add_pool_mutation_replay_verifies_open_journaled_target_passphrase() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"]);

        let journal = recoverable_pool_mutation_add_journal();
        let union = union_memberships(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

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
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_one_disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                },
                b"testpass".to_vec(),
                err_raw("cryptsetup open --test-passphrase", 2, "No key available"),
            );

        let resolver = MockByIdResolver::default();
        let params = RecoverParams {
            config: &config,
            paths: &paths,
            passphrase_stdin: false,
            passphrase_file: Some(passphrase_file.path()),
            allow_degraded: false,
            dry_run: false,
            progress: ProgressOutput::Off,
            sleep_inhibitor: &NOOP_INHIBITOR,
        };
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
            },
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);
        let journal = two_pre_recoverable_add_disk3_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };

        let mut pool = pool_state_two_disks();
        pool.devices.push(PoolDevice {
            mapper: MapperName("braid-mystery".into()),
            luks_uuid: LuksUuid("99999999-9999-9999-9999-999999999999".into()),
            devid: 3,
            underlying: "/dev/vdz".into(),
        });
        pool.total_devices = 3;
        let runner = MockRunner::default();
        let params = recover_params(&config, &paths, None, false);

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
            },
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
                CmdRequest::CryptsetupLuksOpen { mapper, .. } if mapper == "braid-disk3"
            )),
            "unknown live member must abort before opening the journaled target"
        );
        assert!(paths.pending_op_json().exists());
        assert!(!paths.pool_json().exists());
    }

    // Intent
    // Verify a missing returned add target preserves the recovery journal and
    // does not write pool.json.
    //
    // Why it exists
    // Operators need to reattach the returned disk and rerun recover; clearing
    // or rewriting state on the absent-target path would strand the operation.
    //
    // Scenario
    // The pool has disk1 mounted after an interrupted returned-disk add, but
    // the journaled disk2 by-id path is physically absent.
    #[test]
    fn returned_target_absent_preserves_journal_and_pool_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/mapper/braid-disk1"]);
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }
        let runner = MockRunner::default().with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/vda".into(),
            },
            b"testpass".to_vec(),
            ok_raw_empty("cryptsetup open --test-passphrase"),
        );
        let params = recover_params(&config, &paths, Some(passphrase_file.path()), false);

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
            },
        )
        .unwrap_err();

        let msg = err.to_string();
        assert!(msg.contains("is not a LUKS device"), "{msg}");
        assert!(
            !runner.requests().iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsDeviceScanForget { .. }
                    | CmdRequest::WipefsBtrfs { .. }
                    | CmdRequest::BtrfsDeviceAdd { .. }
            )),
            "absent returned target must not scan-forget, wipe, or add"
        );
        assert!(paths.pending_op_json().exists());
        assert!(!paths.pool_json().exists());
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = StatefulMockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);
        let fs_handle = fs.handle();
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let inner = with_balance_replay(with_two_disk_pool_probe(
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
                        mapper: "braid-disk2".into(),
                    },
                    b"testpass".to_vec(),
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
                    b"testpass".to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                )
                .with_output_stdin(
                    CmdRequest::CryptsetupLuksOpen {
                        device: "/dev/disk/by-id/virtio-disk1".into(),
                        mapper: "braid-disk1".into(),
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
                        mount_point: MountPoint("/mnt/storage".into()),
                    },
                    ok_raw_empty("mount"),
                ),
        ));
        let request_log = inner.clone();
        let runner = MapperClosingRunner {
            inner,
            fs_paths: fs_handle,
            closed: Mutex::new(["braid-disk1".to_owned(), "braid-disk2".to_owned()].into()),
        };
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = recover_params(&config, &paths, Some(passphrase_file.path()), false);

        cmd_recover(&runner, &fs, &resolver, &params)
            .expect("closed committed target should be discovered and adopted");

        let requests = request_log.requests();
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
        assert!(!paths.pending_op_json().exists());
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(recovered.disks.contains_key("disk1"));
        assert!(recovered.disks.contains_key("disk2"));
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = StatefulMockFs::new(&["/dev/disk/by-id/virtio-disk2"]);
        let fs_handle = fs.handle();
        let stored_opts = vec!["--label".into(), "braid-disk2".into()];
        let journal = fresh_pool_mutation_add_journal(stored_opts, None);
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let inner = with_balance_replay(with_two_disk_pool_probe(
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
                        mapper: "braid-disk2".into(),
                    },
                    b"testpass".to_vec(),
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
        let request_log = inner.clone();
        let runner = MapperClosingRunner {
            inner,
            fs_paths: fs_handle,
            closed: Mutex::new(["braid-disk2".to_owned()].into()),
        };
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = recover_params(&config, &paths, Some(passphrase_file.path()), false);

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
            },
        )
        .expect("committed fresh target should be adopted without replay");

        let requests = request_log.requests();
        assert!(
            requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupLuksOpen { device, mapper }
                    if device == "/dev/disk/by-id/virtio-disk2" && mapper == "braid-disk2"
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
                .any(|r| matches!(r, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "adopted committed target should still run post-add balance replay"
        );
        assert!(!paths.pending_op_json().exists());
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(recovered.disks.contains_key("disk1"));
        assert!(recovered.disks.contains_key("disk2"));
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1", "/dev/mapper/braid-disk1"]);
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&paths, &journal).unwrap();

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
        let params = recover_params(&config, &paths, None, false);

        let plan = plan_recover(&runner, &fs, &params)
            .result
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"]);
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let runner = with_balance_replay(with_two_disk_pool_probe(
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
                        mount_point: MountPoint("/mnt/storage".into()),
                        force: true,
                    },
                    ok_raw_empty("btrfs device add"),
                ),
        ));
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let inhibitor = RequestCountInhibitor::new(runner.clone());
        let params = recover_params_with_inhibitor(
            &config,
            &paths,
            Some(passphrase_file.path()),
            false,
            &inhibitor,
        );

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
            },
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
        assert!(!paths.pending_op_json().exists());
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(recovered.disks.contains_key("disk2"));
    }

    #[test]
    fn add_pool_mutation_committed_target_scans_without_wipe_or_add() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"]);
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }
        let runner = with_balance_replay(with_two_disk_pool_probe(
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
        let params = recover_params(&config, &paths, Some(passphrase_file.path()), false);

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
            },
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
        assert!(!paths.pending_op_json().exists());
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);
        let journal = mixed_pool_mutation_add_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let runner = MockRunner::default().with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/vda".into(),
            },
            b"testpass".to_vec(),
            ok_raw_empty("cryptsetup open --test-passphrase"),
        );
        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk2"),
            ("/dev/vdc", "virtio-disk3"),
        ]);
        let inhibitor = FailingInhibitor;
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);

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
            },
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
                    | CmdRequest::BtrfsBalanceResume { .. }
                    | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "inhibitor failure must stop before balance commands"
        );

        assert!(paths.pool_json().exists(), "pool.json should be written");
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(recovered.disks.contains_key("disk1"));
        assert!(recovered.disks.contains_key("disk2"));
        assert!(recovered.disks.contains_key("disk3"));
        assert!(
            paths.pending_op_json().exists(),
            "failing inhibitor should preserve the journal"
        );
        let preserved = journal::load_journal(&paths).unwrap().unwrap();
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
    }

    #[test]
    fn returned_replay_wrong_identity_fails_before_wipe_or_add() {
        for wrong_fsid in [false, true] {
            let tmp = tempfile::TempDir::new().unwrap();
            let paths = StatePaths::custom(tmp.path().into());
            let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
            let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"]);
            let journal = recoverable_pool_mutation_add_journal();
            journal::write_journal(&paths, &journal).unwrap();
            let union = union_memberships(&journal);
            let targets = match &journal.op {
                OpKind::Add { targets, .. } => targets,
                _ => unreachable!("test journal is Add"),
            };
            let passphrase_file = tempfile::NamedTempFile::new().unwrap();
            {
                use std::io::Write;
                passphrase_file.as_file().write_all(b"testpass").unwrap();
            }

            let uuid = if wrong_fsid {
                "22222222-2222-2222-2222-222222222222"
            } else {
                "99999999-9999-9999-9999-999999999999"
            };
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
                    b"testpass".to_vec(),
                    ok_raw_empty("cryptsetup open --test-passphrase"),
                );
            if wrong_fsid {
                runner = runner
                    .with_output_stdin(
                        CmdRequest::CryptsetupTestPassphrase {
                            device: "/dev/disk/by-id/virtio-disk2".into(),
                        },
                        b"testpass".to_vec(),
                        ok_raw_empty("cryptsetup open --test-passphrase"),
                    )
                    .with_output(
                        CmdRequest::BtrfsFilesystemShowTarget {
                            target: "/dev/mapper/braid-disk2".into(),
                        },
                        btrfs_show_target_fsid("ffffffff-ffff-ffff-ffff-ffffffffffff"),
                    );
            }
            let params = recover_params(&config, &paths, Some(passphrase_file.path()), false);
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
                },
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
            assert!(paths.pending_op_json().exists());
            assert!(!paths.pool_json().exists());
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

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
                    mapper: "braid-disk2".into(),
                },
                MapperClosingRunner::inactive_status("braid-disk2"),
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
                    mapper: "braid-disk1".into(),
                },
                // Status probes for disk1, in order:
                // 1. initial mount planning sees disk1 closed;
                // 2. ensure_luks_open sees disk1 closed and opens it;
                // 3. post-mount probe_pool sees disk1 active.
                vec![
                    MapperClosingRunner::inactive_status("braid-disk1"),
                    MapperClosingRunner::inactive_status("braid-disk1"),
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
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
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
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("umount"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_one_disk(),
            );
        let request_log = runner.clone();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(
            &config,
            &paths,
            Some(passphrase_file.path()),
            false,
            &inhibitor,
        );

        let err = cmd_recover(&runner, &fs, &MockByIdResolver::default(), &params).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("LUKS UUID mismatch"), "{msg}");
        assert_eq!(
            inhibitor.acquire_count(),
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
        assert!(paths.pending_op_json().exists());
        assert!(!paths.pool_json().exists());
    }

    #[test]
    fn fresh_replay_formats_with_stored_opts_and_ignores_current_env() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = StatefulMockFs::new(&["/dev/disk/by-id/virtio-disk2"]);
        let fs_handle = fs.handle();
        let stored_opts = vec![
            "--pbkdf".into(),
            "pbkdf2".into(),
            "--label".into(),
            "braid-disk2".into(),
        ];
        let journal = fresh_pool_mutation_add_journal(stored_opts.clone(), None);
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let runner = MapperClosingRunner {
            inner: with_balance_replay(with_two_disk_pool_probe(
                MockRunner::default()
                    .with_output(
                        CmdRequest::CryptsetupLuksUuid {
                            device: "/dev/disk/by-id/virtio-disk2".into(),
                        },
                        err_raw(
                            "cryptsetup luksUUID",
                            1,
                            "Device is not a valid LUKS device",
                        ),
                    )
                    .with_output_stdin(
                        CmdRequest::CryptsetupTestPassphrase {
                            device: "/dev/vda".into(),
                        },
                        b"testpass".to_vec(),
                        ok_raw_empty("cryptsetup open --test-passphrase"),
                    )
                    .with_output_stdin(
                        CmdRequest::CryptsetupLuksFormat {
                            device: "/dev/disk/by-id/virtio-disk2".into(),
                            extra_opts: stored_opts.clone(),
                        },
                        b"testpass".to_vec(),
                        ok_raw_empty("cryptsetup luksFormat"),
                    )
                    .with_output(
                        CmdRequest::CryptsetupLuksHeaderBackup {
                            device: "/dev/disk/by-id/virtio-disk2".into(),
                            backup_path: paths
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
                            mapper: "braid-disk2".into(),
                        },
                        b"testpass".to_vec(),
                        ok_raw_empty("cryptsetup open"),
                    )
                    .with_output(
                        CmdRequest::BtrfsDeviceAdd {
                            device: "/dev/mapper/braid-disk2".into(),
                            mount_point: MountPoint("/mnt/storage".into()),
                            force: false,
                        },
                        ok_raw_empty("btrfs device add"),
                    ),
            )),
            fs_paths: fs_handle,
            closed: Mutex::new(["braid-disk2".to_owned()].into()),
        };
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = recover_params(&config, &paths, Some(passphrase_file.path()), false);

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
            },
        )
        .expect("fresh replay should use stored format options");
    }

    #[test]
    fn fresh_replay_after_luks_format_does_not_reformat() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = StatefulMockFs::new(&["/dev/disk/by-id/virtio-disk2"]);
        let fs_handle = fs.handle();
        let journal =
            fresh_pool_mutation_add_journal(vec!["--label".into(), "braid-disk2".into()], None);
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }
        let inner = with_balance_replay(with_two_disk_pool_probe(
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
                .with_output(
                    CmdRequest::CryptsetupLuksHeaderBackup {
                        device: "/dev/disk/by-id/virtio-disk2".into(),
                        backup_path: paths
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
                        mapper: "braid-disk2".into(),
                    },
                    b"testpass".to_vec(),
                    ok_raw_empty("cryptsetup open"),
                )
                .with_output(
                    CmdRequest::BtrfsDeviceAdd {
                        device: "/dev/mapper/braid-disk2".into(),
                        mount_point: MountPoint("/mnt/storage".into()),
                        force: false,
                    },
                    ok_raw_empty("btrfs device add"),
                ),
        ));
        let request_log = inner.clone();
        let runner = MapperClosingRunner {
            inner,
            fs_paths: fs_handle,
            closed: Mutex::new(["braid-disk2".to_owned()].into()),
        };
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let params = recover_params(&config, &paths, Some(passphrase_file.path()), false);

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
            },
        )
        .expect("fresh replay should continue after preexisting LUKS format");

        let requests = request_log.requests();
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
            &key_file,
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
            &key_file,
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
            &key_file,
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
            &key_file,
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
            let tmp = tempfile::TempDir::new().unwrap();
            let paths = StatePaths::custom(tmp.path().into());
            let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
            let fs = if wrong_label {
                MockFs::new(&["/dev/disk/by-id/virtio-disk2"])
            } else {
                MockFs::new(&[])
            };
            let journal =
                fresh_pool_mutation_add_journal(vec!["--label".into(), "braid-disk2".into()], None);
            journal::write_journal(&paths, &journal).unwrap();
            let union = union_memberships(&journal);
            let targets = match &journal.op {
                OpKind::Add { targets, .. } => targets,
                _ => unreachable!("test journal is Add"),
            };
            let passphrase_file = tempfile::NamedTempFile::new().unwrap();
            {
                use std::io::Write;
                passphrase_file.as_file().write_all(b"testpass").unwrap();
            }
            let mut runner = MockRunner::default().with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vda".into(),
                },
                b"testpass".to_vec(),
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
            let params = recover_params(&config, &paths, Some(passphrase_file.path()), false);
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
                },
            )
            .unwrap_err();
            let msg = err.to_string();
            if wrong_label {
                assert!(msg.contains("unexpected LUKS label"), "{msg}");
            } else {
                assert!(msg.contains("is not present"), "{msg}");
            }
            assert!(paths.pending_op_json().exists());
            assert!(!paths.pool_json().exists());
        }
    }

    #[test]
    fn fresh_present_target_rejects_bad_credential_before_pool_json() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk2", "/dev/mapper/braid-disk2"]);
        let journal =
            fresh_pool_mutation_add_journal(vec!["--label".into(), "braid-disk2".into()], None);
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
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
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(
            &config,
            &paths,
            Some(passphrase_file.path()),
            false,
            &inhibitor,
        );
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
            },
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("recover add passphrase was rejected by 'disk2'"),
            "{err}"
        );
        assert_eq!(
            inhibitor.acquire_count(),
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
        assert!(paths.pending_op_json().exists());
        assert!(!paths.pool_json().exists());
    }

    #[test]
    fn post_add_recovery_never_prepares_or_adds_targets() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let journal = committed_two_disk_add_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let runner = with_balance_replay(MockRunner::default());
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);

        execute_add_post_balance_recovery(
            &runner,
            &resolver,
            &params,
            &journal,
            &union,
            pool_state_two_disks(),
            false,
        )
        .expect("post-add recovery should only finish balance work");

        let requests = runner.requests();
        assert_eq!(inhibitor.acquire_count(), 1);
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
        assert!(!paths.pending_op_json().exists());
        assert!(paths.pool_json().exists());
    }

    #[test]
    fn post_add_recovery_refuses_membership_mismatch_and_preserves_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let journal = committed_two_disk_add_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let runner = MockRunner::default().with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/vda".into(),
            },
            b"testpass".to_vec(),
            ok_raw_empty("cryptsetup open --test-passphrase"),
        );
        let params = recover_params(&config, &paths, None, false);
        let err = execute_add_post_balance_recovery(
            &runner,
            &MockByIdResolver::default(),
            &params,
            &journal,
            &union,
            pool_state_one_disk(),
            false,
        )
        .unwrap_err();
        assert!(
            err.to_string()
                .contains("post-add recovery expected live pool membership")
        );
        assert!(paths.pending_op_json().exists());
        assert!(!paths.pool_json().exists());
    }

    #[test]
    fn pool_mutation_inhibitor_failure_stops_before_destructive_replay() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);
        let journal = recoverable_pool_mutation_add_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let targets = match &journal.op {
            OpKind::Add { targets, .. } => targets,
            _ => unreachable!("test journal is Add"),
        };
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }
        let runner = MockRunner::default().with_output_stdin(
            CmdRequest::CryptsetupTestPassphrase {
                device: "/dev/vda".into(),
            },
            b"testpass".to_vec(),
            ok_raw_empty("cryptsetup open --test-passphrase"),
        );
        let inhibitor = FailingInhibitor;
        let params = recover_params_with_inhibitor(
            &config,
            &paths,
            Some(passphrase_file.path()),
            false,
            &inhibitor,
        );
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
            },
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
        assert!(paths.pending_op_json().exists());
        assert!(!paths.pool_json().exists());
    }

    #[test]
    fn post_add_inhibitor_failure_stops_before_balance_and_preserves_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let journal = committed_two_disk_add_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let runner = MockRunner::default();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let inhibitor = FailingInhibitor;
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);

        let err = execute_add_post_balance_recovery(
            &runner,
            &resolver,
            &params,
            &journal,
            &union,
            pool_state_two_disks(),
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
                    | CmdRequest::BtrfsBalanceResume { .. }
                    | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "post-add inhibitor failure must stop before balance"
        );
        assert!(paths.pending_op_json().exists());
    }

    // Intent: RemoveMissing::PoolMutation recovery treats the primary
    // mutation as committed when the journaled missing devid is gone.
    // Why it exists: recovery must advance to post-maintenance and finish the
    // owed RAID1 work without ever rerunning btrfs device remove.
    // Scenario: remove-missing removed devid 2 and crashed before clearing
    // the journal; recover writes committed membership, balances, and clears.
    #[test]
    fn remove_missing_pool_mutation_committed_finishes_post_maintenance() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let journal = remove_missing_journal_two_survivors();
        journal::write_journal(&paths, &journal).unwrap();
        let runner = with_balance_replay(MockRunner::default());
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);

        execute_remove_missing_pool_mutation_recovery(
            &runner,
            &resolver,
            &params,
            &journal,
            pool_state_two_disks(),
            3,
            true,
        )
        .expect("committed remove-missing should finish post maintenance");

        let requests = runner.requests();
        assert_eq!(inhibitor.acquire_count(), 1);
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
        assert!(!paths.pending_op_json().exists());
        let recovered = membership::load_membership(&paths).unwrap();
        assert_eq!(
            recovered.disks.keys().cloned().collect::<Vec<_>>(),
            vec!["disk1".to_owned(), "disk2".to_owned()]
        );
    }

    // Intent: RemoveMissing::PoolMutation recovery exits recovery mode when
    // the primary remove did not commit.
    // Why it exists: recover may restore bookkeeping, but it must not retry
    // btrfs device remove behind the user's back.
    // Scenario: the journal exists but btrfs still reports the same missing
    // devid, so recovery restores pre-operation pool.json and asks for rerun.
    #[test]
    fn remove_missing_pool_mutation_not_committed_restores_pre_membership() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let journal = remove_missing_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let runner = MockRunner::default();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);

        execute_remove_missing_pool_mutation_recovery(
            &runner,
            &MockByIdResolver::default(),
            &params,
            &journal,
            pool_state_disk1_with_missing_devid2(),
            2,
            true,
        )
        .expect("uncommitted remove-missing should clear journal after restoring pre state");

        assert_eq!(inhibitor.acquire_count(), 0);
        assert!(runner.requests().is_empty());
        assert!(!paths.pending_op_json().exists());
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(recovered.disks.contains_key("disk2"));
    }

    // Intent: RemoveMissing::PoolMutation recovery rejects mixed live state.
    // Why it exists: if live topology is neither exact pre nor exact target,
    // clearing the journal would hide an ambiguous storage state.
    // Scenario: the missing devid is gone from btrfs missing_devids, but the
    // old disk name is still live, so recovery preserves pending-op.json.
    #[test]
    fn remove_missing_pool_mutation_mixed_state_preserves_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let journal = remove_missing_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let runner = MockRunner::default();
        let params = recover_params(&config, &paths, None, false);

        let err = execute_remove_missing_pool_mutation_recovery(
            &runner,
            &MockByIdResolver::default(),
            &params,
            &journal,
            pool_state_two_disks(),
            2,
            true,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("does not match the target membership"),
            "{err}"
        );
        assert!(paths.pending_op_json().exists());
        assert!(!paths.pool_json().exists());
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let mut journal = remove_missing_journal();
        journal.op = OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
            devid: 2,
            restore_raid1_after_commit: false,
        };
        journal::write_journal(&paths, &journal).unwrap();
        let runner = MockRunner::default();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);

        execute_remove_missing_post_maintenance_recovery(
            &runner,
            &resolver,
            &params,
            RemoveMissingPostCtx {
                journal: &journal,
                pool: pool_state_one_disk(),
                devid: 2,
                restore_raid1_after_commit: false,
                inhibitor_already_held: false,
            },
        )
        .expect("unowed post-remove maintenance should only repair state");

        assert_eq!(inhibitor.acquire_count(), 0);
        assert!(runner.requests().is_empty());
        assert!(!paths.pending_op_json().exists());
    }

    // Intent: post-maintenance inhibitor failure preserves the remove-missing
    // journal and runs no maintenance command.
    // Why it exists: recovering the committed membership is safe, but balance
    // replay must stay behind the inhibitor boundary.
    // Scenario: remove-missing has committed and owes RAID1 restore, but
    // logind refuses the inhibitor.
    #[test]
    fn remove_missing_post_maintenance_inhibitor_failure_preserves_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let mut journal = remove_missing_journal();
        journal.op = OpKind::RemoveMissing {
            phase: journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
            devid: 2,
            restore_raid1_after_commit: true,
        };
        journal::write_journal(&paths, &journal).unwrap();
        let runner = MockRunner::default();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let inhibitor = FailingInhibitor;
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);

        let err = execute_remove_missing_post_maintenance_recovery(
            &runner,
            &resolver,
            &params,
            RemoveMissingPostCtx {
                journal: &journal,
                pool: pool_state_one_disk(),
                devid: 2,
                restore_raid1_after_commit: true,
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
        assert!(paths.pending_op_json().exists());
    }

    // Intent: Replace::PoolMutation recovery advances committed replace to
    // post-maintenance instead of restarting replace.
    // Why it exists: a finished kernel replace has already mutated btrfs
    // membership; recover only owes old-mapper close and resize.
    // Scenario: live pool has disk1+new, journal still says PoolMutation.
    #[test]
    fn replace_pool_mutation_committed_finishes_resize_without_replace_start() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);
        let journal = replace_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-old".into(),
                },
                ok_raw_empty("cryptsetup close braid-old"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: 2,
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs filesystem resize"),
            );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);
        let OpKind::Replace {
            new_target, source, ..
        } = &journal.op
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
            "old",
            "new",
            new_target,
            source,
            false,
        )
        .expect("committed replace should finish post maintenance");

        let requests = runner.requests();
        assert_eq!(inhibitor.acquire_count(), 1);
        assert!(
            requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsFilesystemResize { devid: 2, .. }))
        );
        assert!(
            !requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "recover must not rerun btrfs replace start"
        );
        assert!(!paths.pending_op_json().exists());
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(recovered.disks.contains_key("new"));
        assert!(!recovered.disks.contains_key("old"));
    }

    // Intent: Replace::PoolMutation recovery restores pre state when replace
    // did not commit.
    // Why it exists: recovery should exit recovery mode and ask the operator
    // to rerun replace rather than starting btrfs replace itself.
    // Scenario: live pool still contains disk1+old and no new member.
    #[test]
    fn replace_pool_mutation_not_committed_restores_pre_membership() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);
        let journal = replace_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let runner = MockRunner::default();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);
        let OpKind::Replace {
            new_target, source, ..
        } = &journal.op
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
            "old",
            "new",
            new_target,
            source,
            false,
        )
        .expect("uncommitted replace should restore pre state and clear journal");

        assert_eq!(inhibitor.acquire_count(), 0);
        assert!(runner.requests().is_empty());
        assert!(!paths.pending_op_json().exists());
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(recovered.disks.contains_key("old"));
        assert!(!recovered.disks.contains_key("new"));
    }

    // Intent: Replace::PoolMutation recovery rejects mixed pre/post topology.
    // Why it exists: a pool containing both old and new cannot be classified
    // safely as either uncommitted or committed.
    // Scenario: live btrfs reports disk1+old+new, so recovery preserves the
    // journal for manual inspection.
    #[test]
    fn replace_pool_mutation_mixed_state_preserves_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);
        let journal = replace_journal();
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let runner = MockRunner::default();
        let params = recover_params(&config, &paths, None, false);
        let OpKind::Replace {
            new_target, source, ..
        } = &journal.op
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
            "old",
            "new",
            new_target,
            source,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("does not match either"), "{err}");
        assert!(paths.pending_op_json().exists());
        assert!(!paths.pool_json().exists());
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-new"]);
        let key_file = write_valid_keyfile(&tmp, "braid-new.key");
        let journal = replace_fresh_luks_journal(key_file.clone());
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
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
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vdb".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                b"testpass".to_vec(),
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
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup luksAddKey"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksHeaderBackup {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    backup_path: paths
                        .luks_headers_dir()
                        .join("braid-new.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                ok_raw_empty("cryptsetup luksHeaderBackup"),
            );
        let inhibitor = RequestCountInhibitor::new(runner.clone());
        let params = recover_params_with_inhibitor(
            &config,
            &paths,
            Some(passphrase_file.path()),
            false,
            &inhibitor,
        );
        let OpKind::Replace {
            new_target, source, ..
        } = &journal.op
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
            "old",
            "new",
            new_target,
            source,
            false,
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
        assert!(!paths.pending_op_json().exists());
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(recovered.disks.contains_key("old"));
        assert!(!recovered.disks.contains_key("new"));
    }

    // Intent: Replace::PoolMutation FreshLuks recovery refuses a prepared
    // target whose LUKS label does not match the journal.
    // Why it exists: the label check prevents recovery from treating an
    // unrelated LUKS device as braid's interrupted fresh target.
    // Scenario: live pool is still disk1+old, the new by-id path is LUKS2,
    // but its label is not the journaled `braid-new` label.
    #[test]
    fn replace_pool_mutation_fresh_luks_wrong_label_preserves_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-new"]);
        let journal = replace_fresh_luks_journal("/run/keys/braid-new.key".into());
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
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
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);
        let OpKind::Replace {
            new_target, source, ..
        } = &journal.op
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
            "old",
            "new",
            new_target,
            source,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("unexpected LUKS label"), "{err}");
        assert_eq!(inhibitor.acquire_count(), 0);
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
        assert!(paths.pending_op_json().exists());
        assert!(!paths.pool_json().exists());
    }

    // Intent: Replace::PoolMutation FreshLuks recovery refuses a missing
    // target device and preserves the journal.
    // Why it exists: if the replacement disk is absent, recovery cannot prove
    // whether pre-replace preparation completed and must not rewrite pool.json.
    // Scenario: live pool is still disk1+old, but the journaled new by-id path
    // is no longer present after reboot.
    #[test]
    fn replace_pool_mutation_fresh_luks_absent_target_preserves_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);
        let journal = replace_fresh_luks_journal("/run/keys/braid-new.key".into());
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let runner = MockRunner::default();
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);
        let OpKind::Replace {
            new_target, source, ..
        } = &journal.op
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
            "old",
            "new",
            new_target,
            source,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("is not present"), "{err}");
        assert_eq!(inhibitor.acquire_count(), 0);
        assert!(
            runner.requests().is_empty(),
            "absent target should fail from the filesystem probe only"
        );
        assert!(paths.pending_op_json().exists());
        assert!(!paths.pool_json().exists());
    }

    // Intent: Replace::PoolMutation FreshLuks recovery rejects a bad
    // passphrase before acquiring the post-prep inhibitor.
    // Why it exists: credential verification must be complete before recovery
    // enrolls a keyfile, backs up a header, or writes pool.json.
    // Scenario: the prepared target has the expected label, but the supplied
    // passphrase opens the old pool devices and is rejected by the new target.
    #[test]
    fn replace_pool_mutation_fresh_luks_bad_passphrase_preserves_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-new"]);
        let key_file = std::path::PathBuf::from("/run/keys/braid-new.key");
        let journal = replace_fresh_luks_journal(key_file);
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
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
        let params = recover_params_with_inhibitor(
            &config,
            &paths,
            Some(passphrase_file.path()),
            false,
            &inhibitor,
        );
        let OpKind::Replace {
            new_target, source, ..
        } = &journal.op
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
            "old",
            "new",
            new_target,
            source,
            false,
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
        assert!(paths.pending_op_json().exists());
        assert!(!paths.pool_json().exists());
    }

    // Intent: Replace::PoolMutation FreshLuks recovery preserves the journal
    // if header backup fails after credential verification.
    // Why it exists: recovery must not clear the journal or write pool.json
    // until all fresh-target preparation side effects are complete.
    // Scenario: the prepared target has the expected label and credential,
    // but `cryptsetup luksHeaderBackup` fails.
    #[test]
    fn replace_pool_mutation_fresh_luks_header_backup_failure_preserves_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-new"]);
        let key_file = write_valid_keyfile(&tmp, "braid-new.key");
        let journal = replace_fresh_luks_journal(key_file.clone());
        journal::write_journal(&paths, &journal).unwrap();
        let union = union_memberships(&journal);
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
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
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/vdb".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                b"testpass".to_vec(),
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
                    backup_path: paths
                        .luks_headers_dir()
                        .join("braid-new.luksheader.tmp")
                        .display()
                        .to_string(),
                },
                err_raw("cryptsetup luksHeaderBackup", 1, "backup failed"),
            );
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(
            &config,
            &paths,
            Some(passphrase_file.path()),
            false,
            &inhibitor,
        );
        let OpKind::Replace {
            new_target, source, ..
        } = &journal.op
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
            "old",
            "new",
            new_target,
            source,
            false,
        )
        .unwrap_err();

        assert!(err.to_string().contains("backup failed"), "{err}");
        assert_eq!(inhibitor.acquire_count(), 1);
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsReplaceStart { .. })),
            "fresh-prep recovery must not start btrfs replace"
        );
        assert!(paths.pending_op_json().exists());
        assert!(!paths.pool_json().exists());
    }

    // Intent: Replace::PostReplaceMaintenance skips unowed balance work.
    // Why it exists: recovery should not resume an unrelated paused balance
    // when restore_raid1_after_commit is false.
    // Scenario: replace committed and only resize remains.
    #[test]
    fn replace_post_maintenance_skips_unowed_balance() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);
        let journal = replace_journal_in_phase(
            journal::ReplacePhase::PostReplaceMaintenance,
            false,
            journal::ReplaceJournalSource::Missing { old_devid: 2 },
        );
        journal::write_journal(&paths, &journal).unwrap();
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemResize {
                devid: 2,
                mount_point: MountPoint("/mnt/storage".into()),
            },
            ok_raw_empty("btrfs filesystem resize"),
        );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);
        let OpKind::Replace { source, .. } = &journal.op else {
            unreachable!("replace_journal_in_phase returns Replace");
        };

        execute_replace_post_maintenance_recovery(
            &runner,
            &resolver,
            &params,
            &journal,
            pool_state_disk1_and_new(),
            "new",
            source,
            &fs,
            false,
            false,
        )
        .expect("post-replace maintenance should resize and clear");

        let requests = runner.requests();
        assert_eq!(inhibitor.acquire_count(), 1);
        assert!(
            requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsFilesystemResize { devid: 2, .. }))
        );
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsBalanceStatus { .. }
                    | CmdRequest::BtrfsBalanceResume { .. }
                    | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "restore_raid1_after_commit=false must skip balance probes and replay"
        );
        assert!(!paths.pending_op_json().exists());
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);
        let journal = replace_journal_in_phase(
            journal::ReplacePhase::PostReplaceMaintenance,
            true,
            journal::ReplaceJournalSource::Missing { old_devid: 2 },
        );
        journal::write_journal(&paths, &journal).unwrap();
        let runner = with_balance_replay(MockRunner::default()).with_output(
            CmdRequest::BtrfsFilesystemResize {
                devid: 2,
                mount_point: MountPoint("/mnt/storage".into()),
            },
            ok_raw_empty("btrfs filesystem resize"),
        );
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);
        let OpKind::Replace { source, .. } = &journal.op else {
            unreachable!("replace_journal_in_phase returns Replace");
        };

        execute_replace_post_maintenance_recovery(
            &runner,
            &resolver,
            &params,
            &journal,
            pool_state_disk1_and_new(),
            "new",
            source,
            &fs,
            true,
            false,
        )
        .expect("post-replace maintenance should resize, balance, and clear");

        let requests = runner.requests();
        assert_eq!(inhibitor.acquire_count(), 1);
        assert!(
            requests
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "owed RAID1 maintenance should run"
        );
        assert!(!paths.pending_op_json().exists());
    }

    // Intent: post-maintenance inhibitor failure preserves the replace
    // journal and runs no maintenance command.
    // Why it exists: close, resize, and balance are all post-commit
    // maintenance and must stay behind the inhibitor boundary.
    // Scenario: replace committed, but logind refuses the recovery inhibitor.
    #[test]
    fn replace_post_maintenance_inhibitor_failure_preserves_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/mapper/braid-old"]);
        let journal = replace_journal_in_phase(
            journal::ReplacePhase::PostReplaceMaintenance,
            true,
            journal::ReplaceJournalSource::Live {
                old_devid: 2,
                old_mapper: MapperName("braid-old".into()),
            },
        );
        journal::write_journal(&paths, &journal).unwrap();
        let runner = MockRunner::default();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let inhibitor = FailingInhibitor;
        let params = recover_params_with_inhibitor(&config, &paths, None, false, &inhibitor);
        let OpKind::Replace { source, .. } = &journal.op else {
            unreachable!("replace_journal_in_phase returns Replace");
        };

        let err = execute_replace_post_maintenance_recovery(
            &runner,
            &resolver,
            &params,
            &journal,
            pool_state_disk1_and_new(),
            "new",
            source,
            &fs,
            true,
            false,
        )
        .unwrap_err();

        assert!(
            err.to_string()
                .contains("could not acquire sleep inhibitor"),
            "{err}"
        );
        assert!(runner.requests().is_empty());
        assert!(paths.pending_op_json().exists());
    }

    #[test]
    fn recover_dry_run_does_not_acquire_sleep_inhibitor() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        journal::write_journal(&paths, &recoverable_pool_mutation_add_journal()).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1"]);
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
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let params = recover_params_with_inhibitor(&config, &paths, None, true, &inhibitor);
        plan_recover(&runner, &fs, &params)
            .result
            .expect("dry-run planning should not acquire inhibitor");
        assert_eq!(inhibitor.acquire_count(), 0);
    }

    // Intent: verify that when the live pool contains a braid-prefixed
    // device that is in NEITHER the pre nor target membership snapshot,
    // cmd_recover for a PostAddBalanceRaid1 journal refuses to proceed
    // via the set-equality check, surfaces the post-add-recovery
    // mismatch message, and leaves the journal + pool.json untouched.
    // Why it exists: protects the simplification that drops the
    // structurally-redundant validate_live_members_allowed call in
    // execute_add_post_balance_recovery. Without this assertion, a
    // future refactor could re-introduce that call or replace the
    // set-equality check with a one-direction check and silently
    // regress the error-message contract for the live-not-in-target
    // direction. Pinned via the cmd_recover load path so the
    // live-pool probe and PostAddBalanceRaid1 dispatch in
    // RecoverPlan::execute stay under the assertion.
    // Scenario: an interrupted add left a device in the btrfs pool that
    // appears in neither journal snapshot, such as an admin manually
    // running `btrfs device add` outside of braid or a stale mapper
    // surviving a prior recovery. Recovery must refuse to write
    // pool.json and leave the journal intact so the user can intervene.
    #[test]
    fn recover_fails_when_device_missing_from_both_snapshots() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        // pre and target both only know about "toshiba"
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "toshiba".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/ata-TOSHIBA".into())),
        );
        let pre = PoolMembership { disks: pre_disks };
        let target = pre.clone();

        // Op is adding "mystery" -- but neither snapshot contains it
        let mut add_disks = BTreeMap::new();
        add_disks.insert(
            "mystery".to_owned(),
            ByIdPath("/dev/disk/by-id/ata-MYSTERY".into()),
        );
        let journal = journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: add_op_from_disks(add_disks),
            pre_membership: pre,
            target_membership: target,
        };
        journal::write_journal(&paths, &journal).unwrap();

        // Mock: pool is already mounted with both toshiba and mystery
        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_toshiba_and_mystery(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-toshiba".into(),
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
                    mapper: "braid-mystery".into(),
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
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: None,
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        // Must fail with the set-equality mismatch message naming the
        // unknown device, without the deleted by-id-path remediation hint.
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("post-add recovery expected live pool membership"),
            "expected set-mismatch message, got: {msg}"
        );
        assert!(
            msg.contains("mystery"),
            "expected error to surface the unexpected device name, got: {msg}"
        );
        assert!(
            !msg.contains("no by-id path"),
            "post-balance recovery should use the set-mismatch message, not \
             the deleted by-id-path message; got: {msg}"
        );

        // pool.json must NOT have been written
        assert!(
            !paths.pool_json().exists(),
            "pool.json should not exist after failed recovery"
        );

        // pending-op.json must NOT have been cleared
        assert!(
            paths.pending_op_json().exists(),
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = StatefulMockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);
        let fs_handle = fs.handle();

        let journal = committed_two_disk_add_journal();
        journal::write_journal(&paths, &journal).unwrap();

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
            // mount helper: open disk1
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            // mount helper: open disk2
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
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
                    mount_point: MountPoint("/mnt/storage".into()),
                    options: vec!["degraded".to_owned()],
                },
                ok_raw_empty("mount"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            // remount cycle: umount
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint("/mnt/storage".into()),
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
            // mapper paths from the StatefulMockFs after each success and
            // flips status queries to inactive so the re-probe opens them).
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-disk1".into(),
                },
                ok_raw_empty("cryptsetup close braid-disk1"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-disk2".into(),
                },
                ok_raw_empty("cryptsetup close braid-disk2"),
            )
            // remount cycle: re-mount via the same MountWithOptions mock above
            // (MockRunner serves the same response for repeated requests)
            // probe_pool: btrfs filesystem show
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            // probe_pool: cryptsetup status for each device
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk2".into(),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            // M1: replay_post_mutation runs the post-Add soft RAID1
            // balance because pool has 2 devices and OpKind is Add.
            .with_output(
                CmdRequest::BtrfsBalanceRaid1Soft {
                    mount_point: MountPoint("/mnt/storage".into()),
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
        let mut closed0 = std::collections::HashSet::new();
        closed0.insert("braid-disk1".to_owned());
        closed0.insert("braid-disk2".to_owned());
        let runner = MapperClosingRunner {
            inner,
            fs_paths: fs_handle,
            closed: std::sync::Mutex::new(closed0),
        };

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: Some(passphrase_file.path()),
                allow_degraded: true, // disk3 is absent
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        result.expect("recover should self-mount and succeed");

        // pool.json must have been written with disk1 and disk2
        assert!(paths.pool_json().exists(), "pool.json should exist");
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(
            recovered.disks.contains_key("disk1"),
            "recovered membership should contain disk1"
        );
        assert!(
            recovered.disks.contains_key("disk2"),
            "recovered membership should contain disk2"
        );

        // pending-op.json must have been cleared
        assert!(
            !paths.pending_op_json().exists(),
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();

        // StatefulMockFs starts with both by-id paths AND both mapper paths.
        // Both mapper paths are already present, modeling an operator who
        // opened LUKS manually before invoking recover.
        let fs = StatefulMockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
        ]);
        let fs_handle = fs.handle();

        let journal = committed_two_disk_add_journal();
        journal::write_journal(&paths, &journal).unwrap();

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
                    mount_point: MountPoint("/mnt/storage".into()),
                    options: vec!["degraded".to_owned()],
                },
                ok_raw_empty("mount"),
            )
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            // Probe pool after the initial mount.
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk2".into(),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            // M1: replay_post_mutation runs the post-Add soft RAID1
            // balance because pool has 2 devices and OpKind is Add.
            .with_output(
                CmdRequest::BtrfsBalanceRaid1Soft {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs balance start"),
            )
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ]);

        let runner = MapperClosingRunner {
            inner,
            fs_paths: fs_handle,
            closed: std::sync::Mutex::new(std::collections::HashSet::new()),
        };

        // Passphrase file with the canonical "testpass" — the cycle's
        // CryptsetupTestPassphrase mock asserts on this exact value.
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: Some(passphrase_file.path()),
                allow_degraded: true, // disk3 is "absent" (not in fs paths)
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        result.expect(
            "post-add recovery should mount with already-open mappers and finish balance replay",
        );

        let requests = runner.inner.requests();
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::CryptsetupClose { .. } | CmdRequest::CryptsetupLuksOpen { .. }
            )),
            "Add post-balance recovery must not run the replace-only relock cycle"
        );

        // pool.json must have been written from live pool state.
        assert!(paths.pool_json().exists(), "pool.json should exist");
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(
            recovered.disks.contains_key("disk1"),
            "recovered membership should contain disk1"
        );
        assert!(
            recovered.disks.contains_key("disk2"),
            "recovered membership should contain disk2"
        );

        // pending-op.json must have been cleared.
        assert!(
            !paths.pending_op_json().exists(),
            "journal should be cleared after recovery"
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-new",
            "/dev/disk/by-id/virtio-old",
        ]);

        let journal = replace_journal();
        journal::write_journal(&paths, &journal).unwrap();

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
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    mapper: "braid-new".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-old".into(),
                    mapper: "braid-old".into(),
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
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            .with_output(
                CmdRequest::BtrfsReplaceStatus {
                    mount_point: MountPoint("/mnt/storage".into()),
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
                    mount_point: MountPoint("/mnt/storage".into()),
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

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: Some(passphrase_file.path()),
                allow_degraded: true,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
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
            !paths.pool_json().exists(),
            "pool.json must not be written when the remount cycle aborts"
        );
        // Journal must be intact for retry.
        assert!(
            paths.pending_op_json().exists(),
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
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/disk/by-id/virtio-extra",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
            "/dev/mapper/braid-extra",
        ]);
        let membership = PoolMembership {
            disks: BTreeMap::from([
                (
                    "disk1".to_owned(),
                    DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
                ),
                (
                    "disk2".to_owned(),
                    DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
                ),
                (
                    "extra".to_owned(),
                    DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-extra".into())),
                ),
            ]),
        };
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint("/mnt/storage".into()),
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
                    mapper: "braid-disk1".into(),
                },
                ok_raw_empty("cryptsetup close"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-disk2".into(),
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
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
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
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            );
        let close_names = vec!["disk1".to_owned(), "disk2".to_owned()];

        relock_and_remount(
            &runner,
            &fs,
            &config,
            &membership,
            false,
            &OpenCredential::Passphrase(Passphrase::from_zeroizing(zeroize::Zeroizing::new(
                "testpass".to_owned(),
            ))),
            &close_names,
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
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1",
        ]);
        let membership = PoolMembership {
            disks: BTreeMap::from([
                (
                    "disk1".to_owned(),
                    DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
                ),
                (
                    "disk2".to_owned(),
                    DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
                ),
            ]),
        };
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint("/mnt/storage".into()),
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
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
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
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            );
        let close_names = vec!["disk1".to_owned(), "disk2".to_owned()];

        relock_and_remount(
            &runner,
            &fs,
            &config,
            &membership,
            false,
            &OpenCredential::Passphrase(Passphrase::from_zeroizing(zeroize::Zeroizing::new(
                "testpass".to_owned(),
            ))),
            &close_names,
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
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
        ]);
        let membership = PoolMembership {
            disks: BTreeMap::from([
                (
                    "disk1".to_owned(),
                    DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
                ),
                (
                    "disk2".to_owned(),
                    DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
                ),
            ]),
        };
        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint("/mnt/storage".into()),
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
                    mapper: "braid-disk1".into(),
                },
                ok_raw_empty("cryptsetup close"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-disk2".into(),
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
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
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
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                err_raw("mount", 32, "mount failed"),
            );
        let close_names = vec!["disk1".to_owned(), "disk2".to_owned()];

        let err = relock_and_remount(
            &runner,
            &fs,
            &config,
            &membership,
            false,
            &OpenCredential::Passphrase(Passphrase::from_zeroizing(zeroize::Zeroizing::new(
                "testpass".to_owned(),
            ))),
            &close_names,
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal = two_disk_journal();
        journal::write_journal(&paths, &journal).unwrap();

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
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk2".into(),
                    mapper: "braid-disk2".into(),
                },
                b"testpass".to_vec(),
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

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: Some(passphrase_file.path()),
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        let err = result.expect_err("should refuse degraded mount");
        assert!(
            matches!(&err, RecoverError::Mount(MountError::DegradedRefused(_))),
            "expected DegradedRefused, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("braid recover --allow-degraded"),
            "hint should reference 'braid recover --allow-degraded', got: {msg}"
        );

        // Journal must NOT have been cleared
        assert!(
            paths.pending_op_json().exists(),
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        let journal = committed_two_disk_add_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            // mount helper: mountpoint check → already mounted
            .with_output(mp_req, mp_out)
            // probe_pool: btrfs filesystem show
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            // probe_pool: cryptsetup status for each device
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk2".into(),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            // M1: replay_post_mutation runs the post-Add soft RAID1
            // balance because pool has 2 devices and OpKind is Add.
            .with_output(
                CmdRequest::BtrfsBalanceRaid1Soft {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs balance start"),
            );

        // No passphrase — pool is already mounted
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: None,
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        result.expect("recover should succeed when pool already mounted");

        assert!(paths.pool_json().exists(), "pool.json should exist");
        let recovered = membership::load_membership(&paths).unwrap();
        assert!(recovered.disks.contains_key("disk1"));
        assert!(recovered.disks.contains_key("disk2"));
        assert!(
            !paths.pending_op_json().exists(),
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        let mut current = PoolMembership::empty();
        current.disks.insert(
            "disk1".to_owned(),
            disk_member("/dev/disk/by-id/old-disk1", Some(POOL_JSON_ADDED_AT)),
        );
        membership::save_membership(&current, &paths).unwrap();

        let journal = interrupted_remove_journal(Some(LEGACY_JOURNAL_ADDED_AT));
        journal::write_journal(&paths, &journal).unwrap();

        let runner = already_mounted_one_disk_runner();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: None,
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        result.expect("recover should succeed");

        let recovered = membership::load_membership(&paths).unwrap();
        assert_eq!(
            recovered.disks["disk1"].added_at.as_deref(),
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            DiskMember {
                by_id: ByIdPath("/dev/disk/by-id/virtio-disk1".into()),
                luks_uuid: None,
                devid: None,
                added_at: Some(JOURNAL_ADDED_AT.into()),
            },
        );
        pre_disks.insert(
            "disk2".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        let pre = PoolMembership { disks: pre_disks };
        let mut target_disks = BTreeMap::new();
        target_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let journal = journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Remove {
                name: "disk2".to_owned(),
            },
            pre_membership: pre,
            target_membership: PoolMembership {
                disks: target_disks,
            },
        };
        journal::write_journal(&paths, &journal).unwrap();

        let runner = already_mounted_one_disk_runner();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: None,
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        result.expect("recover should succeed");

        let recovered = membership::load_membership(&paths).unwrap();
        assert_eq!(
            recovered.disks["disk1"].added_at.as_deref(),
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        let journal = bootstrap_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let runner = already_mounted_one_disk_runner();
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: None,
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        result.expect("recover should succeed");

        let recovered = membership::load_membership(&paths).unwrap();
        let added_at = recovered.disks["disk1"]
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        let mut current = PoolMembership::empty();
        current.disks.insert(
            "disk1".to_owned(),
            disk_member("/dev/disk/by-id/virtio-disk1", Some(POOL_JSON_ADDED_AT)),
        );
        membership::save_membership(&current, &paths).unwrap();

        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            disk_member("/dev/disk/by-id/virtio-disk1", Some(POOL_JSON_ADDED_AT)),
        );
        let pre = PoolMembership { disks: pre_disks };

        let mut target_disks = pre.disks.clone();
        target_disks.insert(
            "disk2".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        let target = PoolMembership {
            disks: target_disks,
        };

        let mut add_disks = BTreeMap::new();
        add_disks.insert(
            "disk2".to_owned(),
            ByIdPath("/dev/disk/by-id/virtio-disk2".into()),
        );
        let journal = journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: add_op_from_disks(add_disks),
            pre_membership: pre,
            target_membership: target,
        };
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk2".into(),
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
                CmdRequest::BtrfsBalanceRaid1Soft {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs balance start"),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: None,
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        result.expect("recover should succeed");

        let recovered = membership::load_membership(&paths).unwrap();
        assert_eq!(
            recovered.disks["disk1"].added_at.as_deref(),
            Some(POOL_JSON_ADDED_AT)
        );
        let disk2_added_at = recovered.disks["disk2"]
            .added_at
            .as_deref()
            .expect("new disk should be stamped");
        assert_ne!(disk2_added_at, POOL_JSON_ADDED_AT);
    }

    /// Bootstrap journal: pre_membership is empty, target has one disk.
    fn bootstrap_journal() -> journal::Journal {
        let mut target_disks = BTreeMap::new();
        target_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let target = PoolMembership {
            disks: target_disks,
        };

        let mut add_disks = BTreeMap::new();
        add_disks.insert(
            "disk1".to_owned(),
            ByIdPath("/dev/disk/by-id/virtio-disk1".into()),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: add_op_from_disks(add_disks),
            pre_membership: PoolMembership::empty(),
            target_membership: target,
        }
    }

    /// Intent: when bootstrap add crashes after LUKS format but before mkfs,
    ///   recover detects the unmountable state and prints step-by-step escape
    ///   instructions.
    ///
    /// Why it exists: without this, the user is stuck in recovery mode with no
    ///   documented way out — recover fails, add is blocked by the journal, and
    ///   the error message gives no guidance.
    ///
    /// Scenario: first-ever braid add of one disk. LUKS format succeeded, crash
    ///   before mkfs.btrfs. User runs braid recover. Mount fails because no btrfs
    ///   superblock exists. Error should name the pending-op.json path, the disk's
    ///   by-id path, and wipefs.
    #[test]
    fn recover_bootstrap_crash_gives_actionable_instructions() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1"]);

        let journal = bootstrap_journal();
        journal::write_journal(&paths, &journal).unwrap();

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
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            // LUKS open ok
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            // btrfs scan ok
            .with_output(
                CmdRequest::BtrfsDeviceScanAll,
                ok_raw_empty("btrfs device scan"),
            )
            // mount fails — no btrfs superblock
            .with_output(
                CmdRequest::Mount {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                err_raw(
                    "mount",
                    32,
                    "wrong fs type, bad option, bad superblock on /dev/mapper/braid-disk1",
                ),
            )
            // btrfs probe confirms NoBtrfs
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
                    mapper: "braid-disk1".into(),
                },
                ok_raw_empty("cryptsetup close"),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_mapper_closed("braid-disk1");

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: Some(passphrase_file.path()),
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        let err = result.expect_err("should fail with bootstrap instructions");
        let msg = err.to_string();
        assert!(
            msg.contains("bootstrap add was interrupted"),
            "expected bootstrap message, got: {msg}"
        );
        assert!(
            msg.contains("pending-op.json"),
            "should mention pending-op.json, got: {msg}"
        );
        assert!(msg.contains("wipefs"), "should mention wipefs, got: {msg}");
        assert!(
            msg.contains("virtio-disk1"),
            "should list disk by-id path, got: {msg}"
        );

        // Journal must NOT have been cleared
        assert!(
            paths.pending_op_json().exists(),
            "journal should still exist"
        );
        // pool.json must NOT have been written
        assert!(!paths.pool_json().exists(), "pool.json should not exist");
        let requests = runner.requests();
        let probe_pos = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::BtrfsFilesystemShowTarget { .. }))
            .expect("bootstrap probe should run");
        let close_pos = requests
            .iter()
            .position(|r| matches!(r, CmdRequest::CryptsetupClose { .. }))
            .expect("cleanup close should run");
        assert!(
            probe_pos < close_pos,
            "bootstrap probe must precede cleanup"
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1", "/dev/mapper/braid-disk1"]);

        let journal = bootstrap_journal();
        journal::write_journal(&paths, &journal).unwrap();

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
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: Some(passphrase_file.path()),
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
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
            paths.pending_op_json().exists(),
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]); // all disks absent

        let journal = two_disk_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default().with_output(mp_req, mp_out);

        // Passphrase must be supplied even though no LUKS open will succeed:
        // cmd_recover reads the passphrase eagerly when the pool is not
        // already mounted so it has it on hand for the post-mount remount
        // cycle (see cmd_recover comment on the credential setup). The mount
        // still fails with "no unlockable disks" because fs has no by-id
        // paths, which is what this test pins down.
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: Some(passphrase_file.path()),
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
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
            paths.pending_op_json().exists(),
            "journal should still exist"
        );
    }

    /// Intent: when bootstrap recover's mount fails but the disk actually has a
    ///   btrfs superblock, the original mount error must propagate — the guidance
    ///   to wipe disks would be wrong.
    ///
    /// Why it exists: mkfs may have succeeded but mount failed for another reason
    ///   (missing kernel module, bad options). Telling the user to wipefs would
    ///   destroy a valid filesystem.
    ///
    /// Scenario: first-ever add of one disk. mkfs.btrfs succeeded, mount failed
    ///   for an unrelated reason. btrfs filesystem show confirms HasBtrfs. Error
    ///   should be the original mount error, not bootstrap guidance.
    #[test]
    fn recover_bootstrap_mount_fails_but_btrfs_exists_propagates_mount_error() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&["/dev/disk/by-id/virtio-disk1"]);

        let journal = bootstrap_journal();
        journal::write_journal(&paths, &journal).unwrap();

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
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            // LUKS open ok
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
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
                    mount_point: MountPoint("/mnt/storage".into()),
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
                    mapper: "braid-disk1".into(),
                },
                ok_raw_empty("cryptsetup close"),
            )
            .with_luks_dump_text_luks2("/dev/disk/by-id/virtio-disk1")
            .with_mapper_closed("braid-disk1");

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let result = cmd_recover(
            &runner,
            &fs,
            &MockByIdResolver::default(),
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: Some(passphrase_file.path()),
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        let err = result.expect_err("should fail with original mount error");
        let msg = err.to_string();
        assert!(
            msg.contains("mount failed"),
            "expected original mount error, got: {msg}"
        );
        assert!(
            !msg.contains("bootstrap add was interrupted"),
            "must not show bootstrap message when btrfs exists, got: {msg}"
        );

        // Journal must NOT have been cleared
        assert!(
            paths.pending_op_json().exists(),
            "journal should still exist"
        );
        assert!(
            runner.requests().iter().any(
                |r| matches!(r, CmdRequest::CryptsetupClose { mapper } if mapper == "braid-disk1")
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        let journal = committed_two_disk_add_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk2".into(),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            // M1: replay_post_mutation runs the post-Add soft RAID1
            // balance because pool has 2 devices and OpKind is Add.
            .with_output(
                CmdRequest::BtrfsBalanceRaid1Soft {
                    mount_point: MountPoint("/mnt/storage".into()),
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
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: None,
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        result.expect("recover should succeed");

        let recovered = membership::load_membership(&paths).unwrap();
        assert_eq!(
            recovered.disks["disk1"].by_id.0, "/dev/disk/by-id/wwn-0xAAAA",
            "disk1 should resolve to highest-priority wwn-, not stale journal value"
        );
        assert_eq!(
            recovered.disks["disk2"].by_id.0, "/dev/disk/by-id/wwn-0xBBBB",
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        let journal = committed_two_disk_add_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk2".into(),
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
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: None,
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
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
            !paths.pool_json().exists(),
            "pool.json should not exist after failed recovery"
        );
        // journal must NOT have been cleared
        assert!(
            paths.pending_op_json().exists(),
            "journal should still exist after failed recovery"
        );
    }

    /// Intent: When several /dev/disk/by-id/ symlinks resolve to the same live
    /// device (the normal case for any SATA drive), the resolver must pick the
    /// most stable identifier per `discover::by_id_priority`.
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
            resolved.0, "/dev/disk/by-id/wwn-X",
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
            resolved.0, "/dev/disk/by-id/ata-FOO",
            "partition entries must be filtered, leaving only the whole-disk by-id"
        );
    }

    // --- recovery_guidance tests ---

    fn set_of(names: &[&str]) -> std::collections::BTreeSet<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    fn ref_set(s: &std::collections::BTreeSet<String>) -> std::collections::BTreeSet<&String> {
        s.iter().collect()
    }

    #[test]
    fn guidance_add_completed() {
        let pre = set_of(&["disk1", "disk2"]);
        let target = set_of(&["disk1", "disk2", "disk3"]);
        let recovered = set_of(&["disk1", "disk2", "disk3"]);
        let mut add_disks = BTreeMap::new();
        add_disks.insert("disk3".to_owned(), ByIdPath("/dev/disk/by-id/x".into()));
        let op = add_op_from_disks(add_disks);

        assert_eq!(
            recovery_guidance(&op, &ref_set(&pre), &ref_set(&target), &ref_set(&recovered)),
            "add completed -- 'disk3' now in the pool."
        );
    }

    #[test]
    fn guidance_add_rolled_back() {
        let pre = set_of(&["disk1", "disk2"]);
        let target = set_of(&["disk1", "disk2", "disk3"]);
        let recovered = set_of(&["disk1", "disk2"]);
        let mut add_disks = BTreeMap::new();
        add_disks.insert("disk3".to_owned(), ByIdPath("/dev/disk/by-id/x".into()));
        let op = add_op_from_disks(add_disks);

        assert_eq!(
            recovery_guidance(&op, &ref_set(&pre), &ref_set(&target), &ref_set(&recovered)),
            "add did not complete -- 'disk3' not in the pool. Re-run braid add to retry."
        );
    }

    #[test]
    fn guidance_remove_completed() {
        let pre = set_of(&["disk1", "toshiba"]);
        let target = set_of(&["disk1"]);
        let recovered = set_of(&["disk1"]);
        let op = OpKind::Remove {
            name: "toshiba".to_owned(),
        };

        assert_eq!(
            recovery_guidance(&op, &ref_set(&pre), &ref_set(&target), &ref_set(&recovered)),
            "remove completed -- 'toshiba' is no longer in the pool."
        );
    }

    #[test]
    fn guidance_remove_rolled_back() {
        let pre = set_of(&["disk1", "toshiba"]);
        let target = set_of(&["disk1"]);
        let recovered = set_of(&["disk1", "toshiba"]);
        let op = OpKind::Remove {
            name: "toshiba".to_owned(),
        };

        assert_eq!(
            recovery_guidance(&op, &ref_set(&pre), &ref_set(&target), &ref_set(&recovered)),
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
            devid: 2,
            restore_raid1_after_commit: true,
        };

        assert_eq!(
            recovery_guidance(&op, &ref_set(&pre), &ref_set(&target), &ref_set(&recovered)),
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
            devid: 2,
            restore_raid1_after_commit: true,
        };

        assert_eq!(
            recovery_guidance(&op, &ref_set(&pre), &ref_set(&target), &ref_set(&recovered)),
            "remove-missing did not complete -- device still in the pool. Re-run braid remove-missing to retry."
        );
    }

    #[test]
    fn guidance_replace_completed() {
        let pre = set_of(&["disk1", "old"]);
        let target = set_of(&["disk1", "new"]);
        let recovered = set_of(&["disk1", "new"]);
        let op = OpKind::Replace {
            phase: journal::ReplacePhase::PoolMutation,
            old_name: "old".to_owned(),
            new_name: "new".to_owned(),
            new_by_id: ByIdPath("/dev/disk/by-id/x".into()),
            new_target: journal::ReplaceJournalTarget {
                by_id: ByIdPath("/dev/disk/by-id/x".into()),
                mapper_name: "braid-new".into(),
                mode: journal::ReplaceJournalMode::ExistingLuks {
                    luks_uuid: LuksUuid("luks-new".into()),
                },
            },
            source: journal::ReplaceJournalSource::Live {
                old_devid: 2,
                old_mapper: MapperName("braid-old".into()),
            },
            restore_raid1_after_commit: false,
        };

        assert_eq!(
            recovery_guidance(&op, &ref_set(&pre), &ref_set(&target), &ref_set(&recovered)),
            "replace completed -- 'old' replaced by 'new'."
        );
    }

    #[test]
    fn guidance_replace_rolled_back() {
        let pre = set_of(&["disk1", "old"]);
        let target = set_of(&["disk1", "new"]);
        let recovered = set_of(&["disk1", "old"]);
        let op = OpKind::Replace {
            phase: journal::ReplacePhase::PoolMutation,
            old_name: "old".to_owned(),
            new_name: "new".to_owned(),
            new_by_id: ByIdPath("/dev/disk/by-id/x".into()),
            new_target: journal::ReplaceJournalTarget {
                by_id: ByIdPath("/dev/disk/by-id/x".into()),
                mapper_name: "braid-new".into(),
                mode: journal::ReplaceJournalMode::ExistingLuks {
                    luks_uuid: LuksUuid("luks-new".into()),
                },
            },
            source: journal::ReplaceJournalSource::Live {
                old_devid: 2,
                old_mapper: MapperName("braid-old".into()),
            },
            restore_raid1_after_commit: false,
        };

        assert_eq!(
            recovery_guidance(&op, &ref_set(&pre), &ref_set(&target), &ref_set(&recovered)),
            "replace did not complete -- pool still has 'old'. Re-run braid replace to retry."
        );
    }

    #[test]
    fn guidance_partial() {
        let pre = set_of(&["disk1", "disk2"]);
        let target = set_of(&["disk1", "disk2", "disk3"]);
        let recovered = set_of(&["disk1", "disk3"]);
        let mut add_disks = BTreeMap::new();
        add_disks.insert("disk3".to_owned(), ByIdPath("/dev/disk/by-id/x".into()));
        let op = add_op_from_disks(add_disks);

        assert_eq!(
            recovery_guidance(&op, &ref_set(&pre), &ref_set(&target), &ref_set(&recovered)),
            "pool membership does not match the pre-operation or target state. \
             Run braid status and decide whether to re-run the operation."
        );
    }

    // ----- M1 (Pre-M11 remediation) tests -----

    /// Two-device journal modeling an interrupted Replace: pre = {disk1, old},
    /// target = {disk1, new}. The replace went through at the kernel level
    /// (the live pool reports {disk1, new} on the new mapper) but shutdown hit
    /// before braid could re-issue `pool_resize_device`.
    fn replace_journal() -> journal::Journal {
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        pre_disks.insert(
            "old".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-old".into())),
        );
        let pre = PoolMembership { disks: pre_disks };

        let mut target_disks = BTreeMap::new();
        target_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        target_disks.insert(
            "new".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-new".into())),
        );
        let target = PoolMembership {
            disks: target_disks,
        };

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Replace {
                phase: journal::ReplacePhase::PoolMutation,
                old_name: "old".to_owned(),
                new_name: "new".to_owned(),
                new_by_id: ByIdPath("/dev/disk/by-id/virtio-new".into()),
                new_target: journal::ReplaceJournalTarget {
                    by_id: ByIdPath("/dev/disk/by-id/virtio-new".into()),
                    mapper_name: "braid-new".into(),
                    mode: journal::ReplaceJournalMode::ExistingLuks {
                        luks_uuid: LuksUuid("luks-new".into()),
                    },
                },
                source: journal::ReplaceJournalSource::Live {
                    old_devid: 2,
                    old_mapper: MapperName("braid-old".into()),
                },
                restore_raid1_after_commit: false,
            },
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
        let OpKind::Replace {
            phase: stored_phase,
            source: stored_source,
            restore_raid1_after_commit: stored_restore,
            ..
        } = &mut journal.op
        else {
            unreachable!("replace_journal returns Replace");
        };
        *stored_phase = phase;
        *stored_source = source;
        *stored_restore = restore_raid1_after_commit;
        journal
    }

    fn replace_fresh_luks_journal(enroll_key_file: std::path::PathBuf) -> journal::Journal {
        let mut journal = replace_journal();
        let OpKind::Replace {
            new_target,
            restore_raid1_after_commit,
            ..
        } = &mut journal.op
        else {
            unreachable!("replace_journal returns Replace");
        };
        *new_target = journal::ReplaceJournalTarget {
            by_id: ByIdPath("/dev/disk/by-id/virtio-new".into()),
            mapper_name: "braid-new".into(),
            mode: journal::ReplaceJournalMode::FreshLuks {
                luks_label: "braid-new".into(),
                luks_format_extra_opts: vec!["--label".into(), "braid-new".into()],
                enroll_key_file: Some(enroll_key_file),
            },
        };
        *restore_raid1_after_commit = false;
        journal
    }

    /// btrfs filesystem show for the post-replace pool: disk1 (devid 1) + new
    /// (devid 2). The "new" mapper is what `replay_post_mutation` keys off to
    /// resolve the new device's devid.
    fn btrfs_show_disk1_and_new() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-new\n",
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();

        let fs = StatefulMockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-old",
            "/dev/disk/by-id/virtio-new",
        ]);
        let fs_handle = fs.handle();

        let journal = replace_journal();
        journal::write_journal(&paths, &journal).unwrap();

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
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    mapper: "braid-new".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-old".into(),
                    mapper: "braid-old".into(),
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
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            .with_output(
                CmdRequest::BtrfsReplaceStatus {
                    mount_point: MountPoint("/mnt/storage".into()),
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

        let mut closed0 = std::collections::HashSet::new();
        closed0.insert("braid-disk1".to_owned());
        closed0.insert("braid-new".to_owned());
        closed0.insert("braid-old".to_owned());
        let runner = MapperClosingRunner {
            inner,
            fs_paths: fs_handle,
            closed: std::sync::Mutex::new(closed0),
        };

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }
        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let params = RecoverParams {
            config: &config,
            paths: &paths,
            passphrase_stdin: false,
            passphrase_file: Some(passphrase_file.path()),
            allow_degraded: false,
            dry_run: false,
            progress: ProgressOutput::Off,
            sleep_inhibitor: &NOOP_INHIBITOR,
        };

        let report = plan_recover(&runner, &fs, &params);
        let plan = report.result.expect("recover planning should succeed");
        let result = plan.execute(&runner, &fs, &resolver, &params);

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
            journal::load_journal(&paths).unwrap().is_some(),
            "journal should remain after suspended replace abort"
        );

        let requests = runner.inner.requests();
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
    /// after `pool_replace_device` (`cli/src/replace.rs:327` / `:359`); a
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();

        // StatefulMockFs starts with by-id paths for the union {disk1, old,
        // new}. No mapper paths -- everything starts closed.
        // MapperClosingRunner adds mapper paths after each successful
        // CryptsetupLuksOpen and removes them after each CryptsetupClose.
        let fs = StatefulMockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-old",
            "/dev/disk/by-id/virtio-new",
        ]);
        let fs_handle = fs.handle();

        let journal = replace_journal();
        journal::write_journal(&paths, &journal).unwrap();

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
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-new".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open --test-passphrase"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
                    mapper: "braid-disk1".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-new".into(),
                    mapper: "braid-new".into(),
                },
                b"testpass".to_vec(),
                ok_raw_empty("cryptsetup open"),
            )
            .with_output_stdin(
                CmdRequest::CryptsetupLuksOpen {
                    device: "/dev/disk/by-id/virtio-old".into(),
                    mapper: "braid-old".into(),
                },
                b"testpass".to_vec(),
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
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("mount"),
            )
            // ── wait_for_kernel_replace_to_finish ───────────────────────
            // Realistic post-resume status: Finished. The parser routes
            // "finished on" to ReplaceState::Finished and the wait loop
            // returns immediately.
            .with_output(
                CmdRequest::BtrfsReplaceStatus {
                    mount_point: MountPoint("/mnt/storage".into()),
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
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("umount"),
            )
            // 2. scan --forget -- pool-scoped to the union mappers.
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-new".into(),
                        "/dev/mapper/braid-old".into(),
                    ],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            // 3. Close each union mapper. MapperClosingRunner removes the
            //    mapper path from StatefulMockFs and adds the name to its
            //    `closed` set after each successful close.
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-disk1".into(),
                },
                ok_raw_empty("cryptsetup close braid-disk1"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-new".into(),
                },
                ok_raw_empty("cryptsetup close braid-new"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-old".into(),
                },
                ok_raw_empty("cryptsetup close braid-old"),
            )
            // 4. Cycle re-plan: mountpoint check + LuksUuid mocks reused
            //    via MockRunner's HashMap lookup. Mappers report inactive
            //    via MapperClosingRunner's `closed` set. TestPassphrase +
            //    LuksOpen mocks above are reused for the cycle reopen.
            // 5. Cycle execute mount: same Mount mock as above.
            // ── Post-cycle probe_pool ───────────────────────────────────
            // The fix-state topology: 2 devices (disk1 + new), no phantom
            // MISSING. This is what btrfs_show_disk1_and_new() returns.
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_disk1_and_new(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-new".into(),
                },
                cryptsetup_status_active("braid-new", "/dev/vdc"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into(),
                },
                cryptsetup_uuid_ok("/dev/vdc", "33333333-3333-3333-3333-333333333333"),
            )
            // ── replay_post_mutation ────────────────────────────────────
            // Resize-to-max on the new device's devid (2). Load-bearing
            // assertion: without this mock the test fails with MissingMock,
            // proving recover actually issued the resize.
            .with_output(
                CmdRequest::BtrfsFilesystemResize {
                    devid: 2,
                    mount_point: MountPoint("/mnt/storage".into()),
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
        // Note: BtrfsBalanceStatus, BtrfsBalanceResume, and
        // BtrfsBalanceRaid1Soft are NOT mocked. This live-source replace does
        // not owe post-commit RAID1 maintenance, so any balance replay would
        // fail with MissingMock.

        // All three union mappers start closed: probe_mapper_open reports
        // inactive via MapperClosingRunner's `closed`-set fast path, so
        // plan_open_pool builds a non-empty to_unlock and the initial
        // mount runs LuksOpen for each. Successful LuksOpen removes the
        // entry from `closed` and adds the mapper path to fs; the cycle's
        // Close re-adds the entry and removes the mapper path; the cycle's
        // LuksOpen reverses again.
        let mut closed0 = std::collections::HashSet::new();
        closed0.insert("braid-disk1".to_owned());
        closed0.insert("braid-new".to_owned());
        closed0.insert("braid-old".to_owned());
        let runner = MapperClosingRunner {
            inner,
            fs_paths: fs_handle,
            closed: std::sync::Mutex::new(closed0),
        };

        // Passphrase file with the canonical "testpass" -- the mocks above
        // assert on this exact stdin payload.
        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdc", "virtio-new")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: Some(passphrase_file.path()),
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        result.expect("recover should succeed via the mount cycle and replay the resize");
        let requests = runner.inner.requests();

        let recovered = membership::load_membership(&paths).unwrap();
        assert!(
            recovered.disks.contains_key("disk1") && recovered.disks.contains_key("new"),
            "recovered membership should match the post-replace target"
        );
        assert!(
            !recovered.disks.contains_key("old"),
            "old disk must not appear in the post-replace membership"
        );

        assert!(
            !paths.pending_op_json().exists(),
            "journal must be cleared after a successful resize replay"
        );
        assert!(
            !requests.iter().any(|r| matches!(
                r,
                CmdRequest::BtrfsBalanceStatus { .. }
                    | CmdRequest::BtrfsBalanceResume { .. }
                    | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "live-source replace recovery must not replay unowed balance work: {requests:?}"
        );
    }

    /// Intent: When `cmd_recover` finds a paused balance after rebuilding
    /// pool.json, it MUST issue `btrfs balance resume` before clearing the
    /// journal. Otherwise the pool stays in reduced-redundancy state until
    /// the operator manually runs the resume.
    ///
    /// Why it exists: This closes the GAP A identified in the Pre-M11 audit
    /// for all four mutation classes. braid mounts with `skip_balance`
    /// (`cli/src/cmd.rs:271-283`) so the kernel does NOT auto-resume a paused
    /// balance, and the previous `emit_paused_balance_warning` only printed
    /// a hint, leaving the pool unprotected. The VM matrix tests M5 (RemoveMissing
    /// soft balance) and M6 (post-add RAID1 balance) explicitly trigger this
    /// scenario; without auto-resume those tests cannot assert "pool is back
    /// to a known-good state without manual intervention".
    ///
    /// Scenario: An operator ran `braid add disk3` against a 2-disk pool;
    /// UPS LB fired during the post-add `pool_balance_raid1`. Reboot leaves
    /// a paused RAID1 balance. `braid recover` runs, sees the paused state,
    /// resumes the balance to drain it, then clears the journal.
    #[test]
    fn recover_resumes_paused_balance_then_clears_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        // OpKind::Add interrupted mid-balance. Live pool already reflects
        // the target membership (disk1 + disk2) because `btrfs device add`
        // committed before the crash; only the rebalance was in flight.
        let journal = committed_two_disk_add_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            // mountpoint check -> already mounted
            .with_output(mp_req, mp_out)
            // probe_pool path
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk2".into(),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            // Balance status reports Paused (matches the post-skip_balance
            // remount with reset chunk counters from
            // `parse_btrfs_balance_status::balance_status_paused_after_remount_negative_nan_pct`).
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                RawCommandOutput {
                    cmd: "btrfs balance status".into(),
                    stdout: "Balance on '/mnt/storage' is paused\n\
                             0 out of about 0 chunks balanced (0 considered), -nan% left\n"
                        .into(),
                    stderr: String::new(),
                    exit_status: 1,
                },
            )
            // M1 replay: paused balance -> issue resume. Without this mock
            // the test fails with MissingMock, proving recover actually
            // issued the resume.
            .with_output(
                CmdRequest::BtrfsBalanceResume {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs balance resume"),
            )
            // M1 replay: unconditional soft RAID1 balance for OpKind::Add.
            // After the resume drains the paused balance, this re-runs the
            // soft balance to also catch the case where umount cancelled
            // (rather than paused) a partial balance. Idempotent: the
            // `,soft` filter skips already-RAID1 chunks.
            .with_output(
                CmdRequest::BtrfsBalanceRaid1Soft {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs balance start"),
            );

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: None,
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        result.expect("recover should succeed and resume the paused balance");

        let recovered = membership::load_membership(&paths).unwrap();
        assert!(recovered.disks.contains_key("disk1"));
        assert!(recovered.disks.contains_key("disk2"));

        assert!(
            !paths.pending_op_json().exists(),
            "journal must be cleared after the paused balance is resumed"
        );
    }

    /// Two-disk Remove journal modeling an interrupted 2->1 remove: pre =
    /// {disk1, disk2}, target = {disk1}. Shutdown landed during the
    /// pre-remove `pool_balance_single`, so the live pool still has both
    /// disks but the kernel has a paused convert-to-single balance.
    fn remove_2to1_journal() -> journal::Journal {
        let mut pre_disks = BTreeMap::new();
        pre_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        pre_disks.insert(
            "disk2".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );
        let pre = PoolMembership { disks: pre_disks };

        let mut target_disks = BTreeMap::new();
        target_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        let target = PoolMembership {
            disks: target_disks,
        };

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Remove {
                name: "disk2".to_owned(),
            },
            pre_membership: pre,
            target_membership: target,
        }
    }

    /// Intent: When `cmd_recover` finds a paused balance after rebuilding
    /// pool.json from an interrupted `OpKind::Remove`, it MUST NOT auto-resume
    /// it. The operator's recovery path is to re-run `braid remove`, which
    /// re-issues the appropriate `pool_balance_single` itself.
    ///
    /// Why it exists: `braid remove` is the only mutation whose pre-mutation
    /// phase issues a balance (the RAID1 -> single conversion in the 2->1
    /// case in `cli/src/pool.rs:310`). A shutdown landing during that
    /// pre-balance leaves the kernel with a paused convert-to-single balance
    /// against a still-2-disk pool. If `replay_post_mutation` resumed it
    /// unconditionally, recover would finish the conversion to single
    /// without ever removing the device, then clear the journal, silently
    /// halving redundancy. The matrix test `ups-lb-during-remove` only
    /// exercises a 3->2 remove, so this unit test is the regression guard
    /// for the 2->1 pre-balance path.
    ///
    /// Scenario: Operator started `braid remove disk2` against a 2-disk
    /// RAID1 pool; UPS LB fired during the pre-remove `pool_balance_single`.
    /// Pool comes up with both disks still present and a paused balance.
    /// Recover writes the recovered membership ({disk1, disk2} = pre), skips
    /// the resume + the soft RAID1 replay, clears the journal, and prints
    /// guidance to re-run `braid remove`.
    #[test]
    fn recover_skips_paused_balance_resume_for_remove() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        let journal = remove_2to1_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            // mountpoint check -> already mounted (skips the mount cycle)
            .with_output(mp_req, mp_out)
            // probe_pool path -- live pool still has both disks because the
            // pre-remove balance was in flight when shutdown hit; the device
            // was never removed.
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk2".into(),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );
        // Note: BtrfsBalanceStatus, BtrfsBalanceResume, and BtrfsBalanceRaid1Soft
        // are NOT mocked. If replay_post_mutation regresses and either probes
        // balance status, issues `btrfs balance resume`, or replays the soft
        // RAID1 balance for OpKind::Remove, the test fails with MissingMock --
        // proving recover correctly leaves the paused balance alone for the
        // remove path.

        let resolver = resolver_for(&[("/dev/vda", "virtio-disk1"), ("/dev/vdb", "virtio-disk2")]);
        let result = cmd_recover(
            &runner,
            &fs,
            &resolver,
            &RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: None,
                allow_degraded: false,
                dry_run: false,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            },
        );

        result.expect("recover should succeed without resuming the paused remove balance");

        let recovered = membership::load_membership(&paths).unwrap();
        assert!(
            recovered.disks.contains_key("disk1") && recovered.disks.contains_key("disk2"),
            "recovered membership must reflect the live pool (both disks still present)"
        );

        assert!(
            !paths.pending_op_json().exists(),
            "journal must be cleared so the operator can re-run braid remove cleanly"
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal = two_disk_journal();
        journal::write_journal(&paths, &journal).unwrap();

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

        let params = RecoverParams {
            config: &config,
            paths: &paths,
            passphrase_stdin: false,
            passphrase_file: None,
            allow_degraded: true,
            dry_run: true,
            progress: ProgressOutput::Off,
            sleep_inhibitor: &NOOP_INHIBITOR,
        };

        let rendered = plan_recover(&runner, &fs, &params)
            .result
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
     * devices in the journal union, so it proceeds to emit the
     * write/clear steps.
     */
    #[test]
    fn plan_recover_dry_run_stepful_already_mounted() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[]);

        let journal = two_disk_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_ok();
        let runner = MockRunner::default()
            .with_output(mp_req, mp_out)
            // probe_pool (dry-run reconciliation) -- live pool has both disks.
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk2".into(),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        let params = RecoverParams {
            config: &config,
            paths: &paths,
            passphrase_stdin: false,
            passphrase_file: None,
            allow_degraded: false,
            dry_run: true,
            progress: ProgressOutput::Off,
            sleep_inhibitor: &NOOP_INHIBITOR,
        };

        let rendered = plan_recover(&runner, &fs, &params)
            .result
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

    /* Intent: when the journal records OpKind::Replace and the pool is already
     * mounted at planner entry, plan_recover MUST return RecoverError::Failed
     * with safe-recovery instructions, preserving the entry banner and
     * AlreadyMounted info note on report.notes. The refusal must fire for
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
            let tmp = tempfile::TempDir::new().unwrap();
            let paths = StatePaths::custom(tmp.path().into());
            let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
            let fs = MockFs::new(&[]);

            let journal = replace_journal();
            journal::write_journal(&paths, &journal).unwrap();

            // Only mock the mountpoint check. plan_open_pool short-circuits to
            // Ok(None) before any per-disk probe, so no further mocks are
            // needed -- any subsequent CryptsetupStatus / BtrfsFilesystemShow
            // call would surface as MissingMock, proving the fail-fast fires
            // before probing.
            let (mp_req, mp_out) = mountpoint_ok();
            let runner = MockRunner::default().with_output(mp_req, mp_out);

            let params = RecoverParams {
                config: &config,
                paths: &paths,
                passphrase_stdin: false,
                passphrase_file: None,
                allow_degraded: false,
                dry_run,
                progress: ProgressOutput::Off,
                sleep_inhibitor: &NOOP_INHIBITOR,
            };

            let report = plan_recover(&runner, &fs, &params);
            let err = match report.result {
                Err(RecoverError::Failed(msg)) => msg,
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
                err.contains("sudo braid recover"),
                "dry_run={dry_run}: error must direct to `sudo braid recover`, got: {err:?}",
            );

            assert_eq!(
                report.notes.len(),
                2,
                "dry_run={dry_run}: report.notes must hold entry banner + AlreadyMounted, got: {:?}",
                report.notes,
            );
            let entry_banner = format_recover_entry(&journal);
            match &report.notes[0] {
                PreviewNote::Info(msg) => assert_eq!(
                    msg, &entry_banner,
                    "dry_run={dry_run}: notes[0] must be the entry banner",
                ),
                other => {
                    panic!("dry_run={dry_run}: notes[0] must be PreviewNote::Info, got: {other:?}")
                }
            }
            match &report.notes[1] {
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

    fn render_recover_dry_run(
        journal: journal::Journal,
        fs_paths: &[&str],
        runner: MockRunner,
        allow_degraded: bool,
    ) -> String {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(fs_paths);
        journal::write_journal(&paths, &journal).unwrap();

        let params = RecoverParams {
            config: &config,
            paths: &paths,
            passphrase_stdin: false,
            passphrase_file: None,
            allow_degraded,
            dry_run: true,
            progress: ProgressOutput::Off,
            sleep_inhibitor: &NOOP_INHIBITOR,
        };

        plan_recover(&runner, &fs, &params)
            .result
            .expect("plan_recover should render a dry-run preview")
            .preview()
            .render()
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

        let rendered =
            render_recover_dry_run(mixed_pool_mutation_add_journal(), &[], runner, false);

        assert!(
            rendered.contains(
                "reconcile journaled add targets against live pool (no replay needed: all targets already live)"
            ),
            "all-live reconcile header should be annotated: {rendered:?}",
        );
        assert!(
            rendered.contains(
                "[safe       ] replay verified returned-disk add /dev/mapper/braid-disk2 (skipped: target already live in pool)"
            ),
            "returned target should render a safe skip placeholder: {rendered:?}",
        );
        assert!(
            rendered.contains(
                "[safe       ] replay fresh add target /dev/disk/by-id/virtio-disk3 (skipped: target already live in pool)"
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
            &[],
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
    // conditional replay, including the format row.
    //
    // Why it exists
    // The planner has no mounted live-set snapshot in this shape, but the
    // executor can still open/scan a committed target before replay.
    //
    // Scenario
    // Fresh disk2 add journal with the pool offline; recover will mount disk1
    // first, then conditionally replay disk2 setup/add.
    #[test]
    fn plan_recover_dry_run_pool_mutation_not_mounted_fresh_conditional_replay_with_format_row() {
        let rendered = render_recover_dry_run(
            fresh_pool_mutation_add_journal(vec!["--label".into(), "braid-disk2".into()], None),
            &["/dev/disk/by-id/virtio-disk1"],
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
                "$ cryptsetup luksFormat --type luks2 --batch-mode '--key-file=-' --label braid-disk2 /dev/disk/by-id/virtio-disk2"
            ) && disk2_block.contains("$ cryptsetup luksHeaderBackup --header-backup-file")
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
            &["/dev/disk/by-id/virtio-disk1", "/dev/disk/by-id/virtio-new"],
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
                "LUKS open /dev/disk/by-id/virtio-disk1 → braid-disk1 (recover remount cycle)"
            ) && rendered.contains(
                "LUKS open /dev/disk/by-id/virtio-new → braid-new (recover remount cycle)"
            ),
            "missing cycle reopen steps: {rendered:?}",
        );
        assert!(
            rendered.contains("mount → /mnt/storage (recover remount cycle, degraded)"),
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
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                btrfs_show_two_disks(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
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
                    mapper: "braid-disk2".into(),
                },
                cryptsetup_status_active("braid-disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            );

        let rendered = render_recover_dry_run(two_disk_journal(), &[], runner, false);

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
            &["/dev/disk/by-id/virtio-disk1", "/dev/disk/by-id/virtio-new"],
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
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
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
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-new",
                "/dev/mapper/braid-old",
            ],
            closed_replace_dry_run_runner(),
            true,
        );

        assert!(
            rendered.contains(
                "$ btrfs device scan --forget /dev/mapper/braid-disk1 /dev/mapper/braid-new /dev/mapper/braid-old"
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
     * Verify the remount cycle reopen set excludes a by-id-present disk with
     * damaged LUKS metadata, even when its mapper path exists.
     *
     * Why it exists
     * The cycle reopen set must come from healthy probe events, not from
     * by-id path existence. A regression back to `fs.exists(by_id)` would
     * incorrectly preview reopening a disk whose LUKS header cannot be used.
     *
     * Scenario
     * Interrupted replace where old's by-id path and mapper path both exist,
     * but `cryptsetup luksUUID` fails, `isLuks` succeeds, and `luksDump`
     * fails, producing a damaged-header probe event.
     */
    #[test]
    fn plan_recover_dry_run_cycle_reopen_set_excludes_damaged_header_disk() {
        let runner = closed_two_disk_dry_run_runner()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                err_raw("cryptsetup luksUUID", 1, "LUKS metadata corrupted"),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                ok_raw_empty("cryptsetup isLuks"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-old".into(),
                },
                err_raw("cryptsetup luksDump", 1, "LUKS2 metadata corrupted"),
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
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-new",
                "/dev/disk/by-id/virtio-old",
                "/dev/mapper/braid-old",
            ],
            runner,
            true,
        );

        assert!(
            rendered.contains("[skip] disk old: LUKS header metadata damaged"),
            "test setup should classify old as damaged, got: {rendered:?}",
        );
        assert!(
            rendered.contains("close LUKS mapper braid-old (recover remount cycle)"),
            "close set should include damaged old mapper: {rendered:?}",
        );
        assert!(
            !rendered.contains(
                "LUKS open /dev/disk/by-id/virtio-old → braid-old (recover remount cycle)"
            ),
            "reopen set should not include damaged old disk: {rendered:?}",
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
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-new",
                "/dev/mapper/braid-disk1",
            ],
            runner,
            true,
        );

        let initial = "$ mount -o 'noatime,skip_balance,subvolid=5,degraded' /dev/mapper/braid-new /mnt/storage";
        let cycle = "$ mount -o 'noatime,skip_balance,subvolid=5,degraded' /dev/mapper/braid-disk1 /mnt/storage";
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
     * placeholders without command lines.
     *
     * Why it exists
     * Execution only resumes a paused balance when a post-mount probe finds
     * one, and only runs the soft RAID1 replay when the live pool has at
     * least two devices. Dry-run must describe the conditional work without
     * promising command argv rows.
     *
     * Scenario
     * Interrupted add with a not-mounted degraded open plan.
     */
    #[test]
    fn plan_recover_dry_run_add_post_mutation_placeholders_have_no_commands() {
        let rendered = render_recover_dry_run(
            two_disk_journal(),
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
            closed_two_disk_dry_run_runner(),
            true,
        );

        let write_pos = rendered
            .find("write recovered pool.json")
            .unwrap_or_else(|| panic!("missing write step: {rendered:?}"));
        let resume_pos = rendered
            .find("btrfs balance resume /mnt/storage (skipped if no paused balance)")
            .unwrap_or_else(|| panic!("missing balance resume placeholder: {rendered:?}"));
        let soft_pos = rendered
            .find("btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft /mnt/storage (skipped if pool has <2 devices)")
            .unwrap_or_else(|| panic!("missing soft balance placeholder: {rendered:?}"));
        let clear_pos = rendered
            .find("clear pending-op.json")
            .unwrap_or_else(|| panic!("missing clear step: {rendered:?}"));

        assert!(
            write_pos < resume_pos && resume_pos < soft_pos && soft_pos < clear_pos,
            "post-mutation placeholders must sit between write and clear: {rendered:?}",
        );
        assert!(
            !rendered.contains("$ btrfs balance resume")
                && !rendered.contains("$ btrfs balance start --enqueue -dconvert=raid1,soft"),
            "conditional placeholders must not render btrfs balance commands: {rendered:?}",
        );
    }

    #[test]
    fn plan_recover_dry_run_post_add_balance_only_has_no_target_replay() {
        let rendered = render_recover_dry_run(
            committed_two_disk_add_journal(),
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
            ],
            closed_two_disk_dry_run_runner(),
            false,
        );

        assert!(
            rendered.contains("btrfs balance resume /mnt/storage (skipped if no paused balance)")
                && rendered.contains(
                    "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft /mnt/storage (skipped if pool has <2 devices)"
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
            &["/dev/disk/by-id/virtio-disk1", "/dev/disk/by-id/virtio-new"],
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
            !rendered.contains("$ btrfs balance resume")
                && !rendered.contains("-dconvert=raid1,soft"),
            "live-source replace preview must not include RAID1 balance replay: {rendered:?}",
        );
    }

    /* Intent
     * Verify remove-missing dry-run previews the balance replay placeholders.
     *
     * Why it exists
     * Remove-missing can leave paused or incomplete post-mutation balancing
     * just like add and replace. The preview needs to name that follow-up
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
            &["/dev/disk/by-id/virtio-disk1"],
            closed_disk1_dry_run_runner(),
            true,
        );

        assert!(
            rendered.contains("btrfs balance resume /mnt/storage (skipped if no paused balance)")
                && rendered.contains(
                    "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft /mnt/storage (skipped if pool has <2 devices)"
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
            &["/dev/disk/by-id/virtio-disk1"],
            closed_disk1_dry_run_runner(),
            true,
        );

        assert!(
            !rendered.contains("btrfs balance resume")
                && !rendered.contains("-dconvert=raid1,soft")
                && !rendered.contains("btrfs filesystem resize <devid>:max"),
            "remove preview should omit replay placeholders: {rendered:?}",
        );
    }

    /* Intent: a degraded-refusal at the planner boundary preserves the
     * entry banner + per-disk probe notes on `RecoverPlanReport.notes`
     * in order, and routes the error as
     * `RecoverError::Mount(MountError::DegradedRefused(_))`.
     *
     * Why it exists: Shape A's preserved-context contract for recover
     * says the entry banner and accumulated probe context survive the
     * `Err` path so `cmd_recover` can render them to stderr before the
     * refusal message -- mirroring today's
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
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal = two_disk_journal();
        journal::write_journal(&paths, &journal).unwrap();

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

        let params = RecoverParams {
            config: &config,
            paths: &paths,
            passphrase_stdin: false,
            passphrase_file: None,
            allow_degraded: false,
            dry_run: true,
            progress: ProgressOutput::Off,
            sleep_inhibitor: &NOOP_INHIBITOR,
        };

        let report = plan_recover(&runner, &fs, &params);

        let entry_banner = format_recover_entry(&journal);
        assert!(
            matches!(&report.notes[0], PreviewNote::Info(msg) if msg == &entry_banner),
            "first note must be the entry banner, got: {:?}",
            report.notes,
        );

        let per_disk: Vec<&PreviewNote> = report
            .notes
            .iter()
            .filter(|n| matches!(n, PreviewNote::PerDisk { .. }))
            .collect();
        assert_eq!(
            per_disk.len(),
            3,
            "report.notes must carry one per-disk note per union disk, got: {:?}",
            report.notes,
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

        let err = report
            .result
            .expect_err("degraded refusal must surface as Err");
        assert!(
            matches!(&err, RecoverError::Mount(MountError::DegradedRefused(_))),
            "expected DegradedRefused, got: {err:?}",
        );
    }
}
