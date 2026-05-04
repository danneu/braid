use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::{self, Config};
use crate::discover;
use crate::journal::{self, Journal};
use crate::membership::{self, DiskMember, PoolMembership};
use crate::mount::{self, MountError, OpenCredential, OpenPlan};
use crate::parse::btrfs_filesystem_show::{DeviceBtrfsProbe, classify_btrfs_probe};
use crate::parse::{ReplaceState, parse_btrfs_replace_status};
use crate::preview::{self, PerDiskStyle, Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{self, Filesystem, ProbeError};
use crate::progress::{self, ProgressOutput, Sleeper};
use crate::state_paths::StatePaths;
use crate::status::{BalanceReport, get_balance_report};
use crate::status_tag::{StatusTag, color_enabled_for_stderr, emit_status, status_line};
use crate::types::{ByIdPath, MountPoint, PoolState};
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RecoverError {
    #[error("{0}")]
    Probe(#[from] ProbeError),
    #[error("journal error: {0}")]
    Journal(String),
    #[error("membership error: {0}")]
    Membership(#[from] membership::MembershipError),
    #[error("{0}")]
    Mount(#[from] MountError),
    #[error("{0}")]
    Failed(String),
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
}

/// Dry-run preview source of truth for `braid recover` plus the
/// execute inputs pre-computed during planning. `notes` + `steps` are
/// both rendered by `preview()`; `execute()` renders `notes` to stderr
/// with `STDERR_STYLE` before any mutation, preserving today's
/// "entry banner then probe context then work" real-run sequence.
///
/// `open_plan` is `None` when the pool was already mounted at probe
/// time. `notes` carries the entry-banner `Info` note first, then the
/// `ProbeEvent`-derived notes (including `AlreadyMounted`) so both
/// `preview()` and `execute()` surface them in order.
#[derive(Debug)]
pub struct RecoverPlan {
    pub notes: Vec<PreviewNote>,
    pub steps: Vec<Step>,
    pub open_plan: Option<OpenPlan>,
    pub journal: Journal,
    pub union: PoolMembership,
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

/// Format the entry-banner line that both dry-run preview and real-run
/// stderr share. Wording is pinned byte-for-byte against the pre-PR 6
/// `eprintln!` that announced the start of recovery.
pub fn format_recover_entry(journal: &Journal) -> String {
    format!(
        "Recovering from interrupted {:?} operation (started {})...",
        journal_op_label(journal),
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
            steps: self.steps.clone(),
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
            steps: _,
            open_plan,
            journal,
            union,
        } = self;

        let color_enabled = color_enabled_for_stderr();

        // Recover-specific gate: resolve a credential whenever we have
        // an initial mount plan (i.e. the pool is not already mounted).
        // This is EAGER on purpose -- even if the initial plan's
        // `to_unlock` is empty (every mapper already open), the
        // post-mount relock cycle below closes every mapper and must
        // reopen them, so a credential is required regardless. The
        // resolved credential lives in this local across both execute
        // calls (initial mount and the cycle remount).
        let credential = match open_plan.as_ref() {
            Some(_) => Some(
                mount::resolve_credential(
                    params.passphrase_stdin,
                    params.passphrase_file,
                    None, // recover does not expose --key-file today
                )
                .map_err(|e| RecoverError::Failed(format!("recover: {e}")))?,
            ),
            None => None, // already mounted, no cycle, no credential needed
        };

        // Initial mount. Dispatch on `p.to_unlock.is_empty()` to the
        // matching execute entry point. The `expect` in the else arm
        // is load-bearing: the match above guarantees `credential` is
        // `Some` whenever `open_plan` is `Some`.
        let just_mounted = match open_plan.as_ref() {
            None => false, // already mounted
            Some(p) => {
                let res = if p.to_unlock.is_empty() {
                    mount::execute_mount_only(runner, fs, params.config, p)
                } else {
                    let cred = credential
                        .as_ref()
                        .expect("credential resolved above when open_plan is Some");
                    mount::execute_unlock_and_mount(runner, fs, params.config, p, cred)
                };
                match res {
                    Ok(b) => b,
                    Err(e) => {
                        // Bootstrap mount failure: probe the target devices to confirm no btrfs
                        // superblock exists — only then is it safe to advise wiping.
                        if journal.pre_membership.disks.is_empty()
                            && let mount::MountError::MountFailed(_) = &e
                            && let journal::OpKind::Add { ref disks } = journal.op
                        {
                            let all_no_btrfs = disks.keys().all(|name| {
                                let mapper = format!("/dev/mapper/{}", config::mapper_name(name).0);
                                match runner
                                    .run(&CmdRequest::BtrfsFilesystemShowTarget { target: mapper })
                                {
                                    Ok(raw) => matches!(
                                        classify_btrfs_probe(&raw),
                                        DeviceBtrfsProbe::NoBtrfs
                                    ),
                                    Err(_) => false,
                                }
                            });
                            if all_no_btrfs {
                                let disk_list: Vec<_> = union
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
                        }
                        return Err(e.into());
                    }
                }
            }
        };

        // Force fresh kernel state before probing when we just mounted.
        //
        // When the kernel resumes an interrupted btrfs replace during the
        // mount call above, two problems compound:
        //
        //   (a) The resume worker (btrfs_resume_dev_replace_async) runs
        //       asynchronously in a kthread, so umount does NOT wait for it
        //       to finish. probe_pool can run while the resume is still in
        //       progress and read transient mid-resume topology.
        //   (b) The kernel commits the post-completion devid swap to disk
        //       correctly but the in-memory `btrfs_fs_devices` for this
        //       mount session retains the pre-resume topology — including a
        //       phantom MISSING devid for the temporary replace target —
        //       even after the resume finishes.
        //
        // Skipped when the pool was already mounted before recover started:
        // we don't know who's using that mount and umount could fail with
        // EBUSY.
        //
        // Pre-condition for reaching this branch with `just_mounted == false`:
        // the planner-level fail-fast in `plan_recover` rejects the
        // (already-mounted, OpKind::Replace) combination outright, directing
        // the operator to `braid lock; braid recover`. So when execute()
        // reaches an already-mounted pool here, the journal op is Add /
        // Remove / RemoveMissing -- none of which trigger the kernel
        // dev_replace resume bug -- and skipping the cycle is sound.
        if just_mounted {
            wait_for_kernel_replace_to_finish(
                runner,
                params.config.mount_point(),
                &progress::RealSleeper,
                color_enabled,
            );
            // We mounted, so open_plan was Some, so credential was eagerly resolved
            // above (recover always reads the passphrase when not already mounted).
            let cred = credential
                .as_ref()
                .expect("just_mounted implies open_plan was Some and credential was resolved");
            relock_and_remount(
                runner,
                fs,
                params.config,
                &union,
                params.allow_degraded,
                cred,
            )?;
        }

        // 3. Probe live pool state
        let mount_point = params.config.mount_point();
        let pool = probe::probe_pool(runner, fs, mount_point)?;

        // 4. Build new membership from live pool state
        let prior = membership::load_membership(params.paths).ok();
        let mut recovered = PoolMembership::empty();
        for dev in &pool.devices {
            let Some(name) = config::name_from_mapper(&dev.mapper.0) else {
                eprintln!("  skip: device {} has no braid- prefix", dev.mapper.0);
                continue;
            };
            // Sanity check: refuse to handle live pool members the journal never recorded.
            if !union.disks.contains_key(name) {
                return Err(RecoverError::Failed(format!(
                    "device {} is in the live pool but has no by-id path in either \
                     the pre-operation or target membership snapshot.\n\
                     This must be resolved manually -- provide the correct \
                     /dev/disk/by-id/ path and re-run recovery.",
                    dev.mapper.0
                )));
            }
            // Resolve by_id from the live device's identity, not from the journal.
            let by_id = resolve_by_id_for_underlying(by_id_resolver, &dev.underlying)?;
            // Source priority for added_at: current pool.json (mid-op enrich may
            // already have stamped new disks), then journal union, then fresh
            // stamp from enrich_from_pool_state for genuine bootstrap recovery.
            let added_at = prior
                .as_ref()
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
        membership::enrich_from_pool_state(&pool, &mut recovered);

        // 5. Report what changed
        let pre_names: std::collections::BTreeSet<_> =
            journal.pre_membership.disks.keys().collect();
        let target_names: std::collections::BTreeSet<_> =
            journal.target_membership.disks.keys().collect();
        let recovered_names: std::collections::BTreeSet<_> = recovered.disks.keys().collect();

        eprintln!("  pre-operation membership:  {:?}", pre_names);
        eprintln!("  target membership:         {:?}", target_names);
        eprintln!("  recovered (live pool):     {:?}", recovered_names);

        eprintln!(
            "note: {}",
            recovery_guidance(&journal.op, &pre_names, &target_names, &recovered_names)
        );

        // 6. Write recovered membership
        membership::save_membership(&recovered, params.paths)?;
        eprintln!("pool.json written from live pool state.");

        // 7. Replay any post-mutation steps the original command would have run
        //    after its slow phase but before clearing the journal.
        replay_post_mutation(runner, mount_point, &journal.op, &pool, params.progress)?;

        // 8. Clear journal LAST.
        journal::clear_journal(params.paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
        eprintln!("pending-op.json cleared. Recovery complete.");

        Ok(())
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

    let report = mount::plan_open_pool(
        runner,
        fs,
        params.config,
        &union,
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
    if open_plan.is_none() && matches!(&journal.op, journal::OpKind::Replace { .. }) {
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

    // Build dry-run steps.
    let mut steps = Vec::new();
    if let Some(op) = &open_plan {
        steps.extend(mount::compile_open_steps(
            op,
            params.config.mount_point(),
            None,
        ));
    } else if params.dry_run {
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
        for dev in &pool.devices {
            let Some(name) = config::name_from_mapper(&dev.mapper.0) else {
                continue;
            };
            if !union.disks.contains_key(name) {
                return RecoverPlanReport {
                    notes,
                    result: Err(RecoverError::Failed(format!(
                        "device {} is in the live pool but has no by-id path in either \
                         the pre-operation or target membership snapshot.\n\
                         This must be resolved manually -- provide the correct \
                         /dev/disk/by-id/ path and re-run recovery.",
                        dev.mapper.0
                    ))),
                };
            }
        }
    }

    if matches!(&journal.op, journal::OpKind::Replace { .. }) && open_plan.is_some() {
        steps.push(Step {
            risk: "long",
            description: "wait for kernel dev_replace to finish (skipped if no running replace)"
                .into(),
            commands: vec![],
        });
    }

    if let Some(initial_open_plan) = &open_plan {
        let mount_point = params.config.mount_point();
        steps.push(Step {
            risk: "safe",
            description: format!("unmount {mount_point} (recover remount cycle)"),
            commands: vec![CmdRequest::Umount {
                mount_point: mount_point.clone(),
            }],
        });

        let forget_devs: Vec<String> = cycle_close_names
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

        for name in &cycle_close_names {
            let mn = config::mapper_name(name);
            steps.push(Step {
                risk: "safe",
                description: format!("close LUKS mapper {} (recover remount cycle)", mn),
                commands: vec![CmdRequest::CryptsetupClose {
                    mapper: mn.0.clone(),
                }],
            });
        }

        for name in &cycle_reopen_names {
            let member = match union.disks.get(name) {
                Some(member) => member,
                None => {
                    return RecoverPlanReport {
                        notes,
                        result: Err(RecoverError::Failed(format!(
                            "recover remount cycle preview: disk '{name}' missing from membership union"
                        ))),
                    };
                }
            };
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

        let Some(first_reopen_name) = cycle_reopen_names.first() else {
            return RecoverPlanReport {
                notes,
                result: Err(RecoverError::Failed(
                    "recover remount cycle preview: no disks available to reopen".into(),
                )),
            };
        };
        let mount_device = format!("/dev/mapper/{}", config::mapper_name(first_reopen_name).0);
        if initial_open_plan.any_missing_member {
            steps.push(Step {
                risk: "safe",
                description: format!("mount → {mount_point} (recover remount cycle, degraded)"),
                commands: vec![CmdRequest::MountWithOptions {
                    device: mount_device,
                    mount_point: mount_point.clone(),
                    options: vec!["degraded".to_owned()],
                }],
            });
        } else {
            steps.push(Step {
                risk: "safe",
                description: format!("mount → {mount_point} (recover remount cycle)"),
                commands: vec![CmdRequest::Mount {
                    device: mount_device,
                    mount_point: mount_point.clone(),
                }],
            });
        }
    }

    // State recovery steps are always shown (recover writes pool.json
    // even when mounted).
    steps.push(Step {
        risk: "safe",
        description: format!(
            "write recovered pool.json → {}",
            params.paths.pool_json().display()
        ),
        commands: vec![],
    });

    match &journal.op {
        journal::OpKind::Replace { new_name, .. } => {
            steps.push(Step {
                risk: "safe",
                description: format!(
                    "btrfs filesystem resize <devid>:max {} \
                     (skipped if replacement did not commit; devid for '{}' resolved post-mount)",
                    params.config.mount_point(),
                    new_name,
                ),
                commands: vec![],
            });
        }
        journal::OpKind::Add { .. } | journal::OpKind::RemoveMissing { .. } => {}
        journal::OpKind::Remove { .. } => {}
    }

    match &journal.op {
        journal::OpKind::Add { .. }
        | journal::OpKind::RemoveMissing { .. }
        | journal::OpKind::Replace { .. } => {
            let mount_point = params.config.mount_point();
            steps.push(Step {
                risk: "long",
                description: format!(
                    "btrfs balance resume {mount_point} (skipped if no paused balance)"
                ),
                commands: vec![],
            });
            steps.push(Step {
                risk: "long",
                description: format!(
                    "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft {mount_point} \
                     (skipped if pool has <2 devices)"
                ),
                commands: vec![],
            });
        }
        journal::OpKind::Remove { .. } => {}
    }

    steps.push(Step {
        risk: "safe",
        description: format!(
            "clear pending-op.json → {}",
            params.paths.pending_op_json().display()
        ),
        commands: vec![],
    });

    RecoverPlanReport {
        notes: Vec::new(),
        result: Ok(RecoverPlan {
            notes,
            steps,
            open_plan,
            journal,
            union,
        }),
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

/// Re-issue the per-op steps that originally run after the long phase but
/// before `clear_journal`. Called from `cmd_recover` once pool.json has been
/// rewritten and before the journal is cleared.
///
/// Steps, in order:
///
/// 1. **Replace-only**: replay `pool_resize_device` on the new disk's devid.
///    The original command issues the resize in both the Live and Missing
///    arms after `pool_replace_device` succeeds. If shutdown lands between
///    the kernel-resumed dev_replace and the resize, the new disk reports
///    the source disk's old size instead of its full capacity. Resize-to-max
///    is idempotent at the btrfs layer.
///
/// 2. **Per-op resume + balance replay** (Add / RemoveMissing / Replace
///    only): if the kernel left a paused BALANCE_ITEM on umount, drain
///    it; then re-run the post-mutation balance with `,soft` semantics so
///    any non-target-profile chunks left behind by a cancelled-mid-flight
///    balance get converted. The kernel may CANCEL rather than PAUSE the
///    balance during the umount triggered by `systemctl poweroff`, in
///    which case the resume is a no-op and the chunks that had not yet
///    been converted stay single-profile. The `,soft` filter skips chunks
///    already in the target profile, so this is idempotent if the
///    original balance completed naturally.
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
///    `braid add` would refuse on rerun) and for `OpKind::RemoveMissing`
///    the missing device is already gone (so `braid remove-missing`
///    would refuse), so those need the recover-side replay to avoid
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
    if let journal::OpKind::Replace { new_name, .. } = op {
        let new_mn = config::mapper_name(new_name);
        if let Some(dev) = pool.devices.iter().find(|d| d.mapper == new_mn) {
            eprintln!(
                "Replaying post-replace resize on devid {} (new disk '{}')...",
                dev.devid, new_name
            );
            crate::pool::pool_resize_device(runner, dev.devid, mount_point)
                .map_err(|e| RecoverError::Failed(format!("recover replace resize: {e}")))?;
        }
    }

    match op {
        journal::OpKind::Add { .. }
        | journal::OpKind::RemoveMissing { .. }
        | journal::OpKind::Replace { .. } => {
            if let BalanceReport::Paused { .. } = get_balance_report(runner, mount_point) {
                eprint!(
                    "{}",
                    status_line(
                        StatusTag::Wait,
                        color_enabled,
                        &format!(
                            "pool: resuming paused balance left by interrupted {}...",
                            journal_op_label_for_op(op)
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
                            journal_op_label_for_op(op)
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
    }

    Ok(())
}

/// `journal_op_label` works on the whole `Journal`; this thin wrapper is the
/// version `replay_post_mutation` needs (it already has the `OpKind`).
fn journal_op_label_for_op(op: &journal::OpKind) -> &'static str {
    match op {
        journal::OpKind::Add { .. } => "add",
        journal::OpKind::Remove { .. } => "remove",
        journal::OpKind::RemoveMissing { .. } => "remove-missing",
        journal::OpKind::Replace { .. } => "replace",
    }
}

const REPLACE_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(200);
const REPLACE_WAIT_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// Block until any kernel-resumed btrfs dev_replace on `mount_point` finishes.
///
/// `btrfs_resume_dev_replace_async` runs as an unrelated kthread and is NOT
/// waited on by umount, so without this wait the relock_and_remount cycle can
/// race the resume worker and the second mount sees the same in-flight state.
///
/// Best-effort: if the status command fails for any reason, we return early
/// rather than blocking forever — relock_and_remount and probe_pool will catch
/// any remaining staleness as a clear test failure rather than a hang.
fn wait_for_kernel_replace_to_finish<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    sleeper: &dyn Sleeper,
    color_enabled: bool,
) {
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
                if wait_emitted {
                    emit_status(&status_line(
                        StatusTag::Warn,
                        color_enabled,
                        "pool: kernel dev_replace status check failed -- proceeding",
                    ));
                }
                return;
            }
        };
        let parsed = match parse_btrfs_replace_status(&raw) {
            Ok(p) => p,
            Err(_) => {
                if wait_emitted {
                    emit_status(&status_line(
                        StatusTag::Warn,
                        color_enabled,
                        "pool: kernel dev_replace status check failed -- proceeding",
                    ));
                }
                return;
            }
        };
        match parsed.state {
            ReplaceState::Finished | ReplaceState::None => {
                if wait_emitted {
                    emit_status(&status_line(
                        StatusTag::Ok,
                        color_enabled,
                        "pool: kernel dev_replace finished",
                    ));
                }
                return;
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
/// `btrfs device scan --forget` (drop cached fs_devices), close every LUKS
/// mapper for the membership union, then re-plan + re-open + remount via
/// the standard `plan_open_pool` + `execute_unlock_and_mount` flow.
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
    //    Scope to the pool's own mapper paths (the close set for this
    //    cycle). The no-arg form is kernel-global and would invalidate
    //    unrelated btrfs scan entries; per-device forget is sufficient
    //    here because the cycle only closes and re-opens membership
    //    mappers.
    let forget_devs: Vec<String> = membership
        .disks
        .keys()
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

    // 3. Close every LUKS mapper from the union. The dm devices must be
    //    destroyed (not just unmounted) for the next mount to bypass the
    //    kernel's stale fs_devices cache.
    for name in membership.disks.keys() {
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
    // The cycle just closed every mapper, so the cycle's plan ALWAYS has
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
    mount::execute_unlock_and_mount(runner, fs, config, &cycle_plan, credential)
        .map_err(|e| RecoverError::Failed(format!("recover remount cycle: re-mount: {e}")))?;

    Ok(())
}

fn journal_op_label(journal: &Journal) -> &'static str {
    match &journal.op {
        journal::OpKind::Add { .. } => "add",
        journal::OpKind::Remove { .. } => "remove",
        journal::OpKind::RemoveMissing { .. } => "remove-missing",
        journal::OpKind::Replace { .. } => "replace",
    }
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
            journal::OpKind::Add { disks } => {
                let names: Vec<_> = disks.keys().map(|n| format!("'{n}'")).collect();
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
            journal::OpKind::Add { disks } => {
                let names: Vec<_> = disks.keys().map(|n| format!("'{n}'")).collect();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, MockRunner, RawCommandOutput};
    use crate::journal::{self, OpKind};
    use crate::mount::MountError;
    use crate::preview::NoteLevel;
    use crate::probe::Filesystem;
    use crate::types::{ByIdPath, MountPoint};
    use std::collections::{BTreeMap, VecDeque};
    use std::sync::Mutex;

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
            wait_for_kernel_replace_to_finish(&runner, &mount_point, &progress::NoopSleeper, false);
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
            wait_for_kernel_replace_to_finish(&runner, &mount_point, &progress::NoopSleeper, false);
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
            wait_for_kernel_replace_to_finish(&runner, &mount_point, &progress::NoopSleeper, false);
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
            wait_for_kernel_replace_to_finish(&runner, &mount_point, &progress::NoopSleeper, false);
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
            wait_for_kernel_replace_to_finish(&runner, &mount_point, &progress::NoopSleeper, false);
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
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );
        pre_disks.insert(
            "disk2".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk2".into())),
        );

        let mut target_disks = BTreeMap::new();
        target_disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".into())),
        );

        journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::RemoveMissing { devid: 2 },
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
            op: OpKind::Add { disks: add_disks },
            pre_membership: pre,
            target_membership: target,
        }
    }

    /// If a live pool device is absent from both the pre-operation and target
    /// membership snapshots, recovery must fail rather than fabricating a bogus
    /// by_id path. This protects against writing corrupt pool.json entries that
    /// would break subsequent unlock/lock cycles.
    ///
    /// Scenario: an interrupted add left a device in the btrfs pool that
    /// somehow appears in neither journal snapshot. Recovery should refuse to
    /// write pool.json and leave the journal intact so the user can intervene.
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

        // Op is adding "mystery" — but neither snapshot contains it
        let mut add_disks = BTreeMap::new();
        add_disks.insert(
            "mystery".to_owned(),
            ByIdPath("/dev/disk/by-id/ata-MYSTERY".into()),
        );
        let journal = journal::Journal {
            started_at: "2026-01-01T00:00:00Z".into(),
            op: OpKind::Add { disks: add_disks },
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

        // Toshiba (devid 1) iterates first; resolver must succeed for /dev/vda
        // so the loop reaches the unknown 'mystery' device.
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
            },
        );

        // Must fail with an error mentioning the unknown device
        let err = result.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("braid-mystery"),
            "error should name the unknown device, got: {msg}"
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

        let journal = two_disk_journal();
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

    /// Intent: Recover MUST resolve a credential up-front whenever the pool
    /// is not already mounted, even if every LUKS mapper happens to be open
    /// at probe time. The post-mount relock/remount cycle closes every
    /// mapper and must reopen them with the same credential — so the cycle
    /// only works if the credential was eagerly resolved before the initial
    /// mount, NOT lazily based on `plan.to_unlock`.
    ///
    /// Why it exists: A natural-looking refactor of `resolve_credential`
    /// (gating the read on `plan.to_unlock.is_empty()`, the way `cmd_unlock`
    /// does) silently breaks this path. The initial plan would see
    /// `to_unlock.is_empty()` and skip the read; recover would later try to
    /// hand a `None` credential to `relock_and_remount` and panic. The cycle
    /// itself works fine in production because cryptsetup close empties
    /// `to_unlock` for the cycle's plan — but a unit test needs an
    /// interior-mutable `StatefulMockFs` + `MapperClosingRunner` to model
    /// the post-close world correctly. Without this test the credential-flow
    /// regression can pass `cargo test` while leaving recover unable to
    /// complete its cycle in production.
    ///
    /// Scenario: 2-disk RAID1, interrupted add of disk3, both disk1 and
    /// disk2 LUKS mappers manually opened by an operator (`cryptsetup open`
    /// outside braid) before recovery is invoked. The pool is NOT mounted.
    /// Recover must (1) read the passphrase upfront, (2) reach the initial
    /// mount with `to_unlock` empty (mount-only path), (3) run the cycle
    /// which closes both mappers and reopens them with the same passphrase,
    /// (4) complete recovery successfully.
    #[test]
    fn recover_with_all_mappers_open_still_resolves_credential_for_cycle() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let config = Config::new(MountPoint("/mnt/storage".into())).unwrap();

        // StatefulMockFs starts with both by-id paths AND both mapper paths.
        // The MapperClosingRunner removes mapper paths when CryptsetupClose
        // succeeds, modeling the post-close kernel state.
        let fs = StatefulMockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
            "/dev/mapper/braid-disk1",
            "/dev/mapper/braid-disk2",
        ]);
        let fs_handle = fs.handle();

        let journal = two_disk_journal();
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
            // ── relock_and_remount cycle ────────────────────────────────
            // 1. Umount
            .with_output(
                CmdRequest::Umount {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("umount"),
            )
            // 2. scan --forget -- pool-scoped to the membership mappers.
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-disk1".into(),
                        "/dev/mapper/braid-disk2".into(),
                    ],
                },
                ok_raw_empty("btrfs device scan --forget"),
            )
            // 3. close each mapper. The wrapper runner removes the mapper
            //    path from the StatefulMockFs after each successful close.
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
            // 4. cycle re-plan: mountpoint check → not mounted (same mock as above
            //    is reused via MockRunner's HashMap lookup)
            // 4. cycle re-plan: probe disk1, disk2 LUKS UUIDs (same mocks reused).
            //    NOW mapper_open=false because the close hooks removed the mapper
            //    paths from the StatefulMockFs.
            // 5. cycle execute: verify passphrase against both disks, then open both.
            //    This is the LOAD-BEARING assertion: if the credential was not
            //    eagerly resolved before the initial mount, this stdin-bearing
            //    mock would never be called and the test would error with
            //    MissingMock or panic in cmd_recover's `expect`.
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
            // probe_pool after the cycle
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
            },
        );

        // If credential resolution were lazily gated on plan.to_unlock the
        // initial plan would skip the read, the cycle would have no
        // credential, and cmd_recover would panic on `credential.as_ref()
        // .expect(...)` — failing the test with an unwrap-on-None message.
        result.expect(
            "recover should resolve credential eagerly and complete the relock cycle, \
             even though the initial plan has nothing to unlock",
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
    /// Scenario: 2-disk RAID1 with disk3 absent (interrupted add). LUKS opens
    /// and the first mount succeeds, but the cycle's umount returns EBUSY.
    #[test]
    fn recover_remount_cycle_umount_failure_aborts_before_pool_json() {
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
            .with_output(
                CmdRequest::MountWithOptions {
                    device: "/dev/mapper/braid-disk1".into(),
                    mount_point: MountPoint("/mnt/storage".into()),
                    options: vec!["degraded".to_owned()],
                },
                ok_raw_empty("mount"),
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
                "/dev/disk/by-id/virtio-disk2",
            ])
            .with_mappers_closed(&["braid-disk1", "braid-disk2"]);
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

        let journal = two_disk_journal();
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
            op: OpKind::Add { disks: add_disks },
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
            op: OpKind::Add { disks: add_disks },
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

        let journal = two_disk_journal();
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

        let journal = two_disk_journal();
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
        let op = OpKind::Add { disks: add_disks };

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
        let op = OpKind::Add { disks: add_disks };

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
        let op = OpKind::RemoveMissing { devid: 2 };

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
        let op = OpKind::RemoveMissing { devid: 2 };

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
            old_name: "old".to_owned(),
            new_name: "new".to_owned(),
            new_by_id: ByIdPath("/dev/disk/by-id/x".into()),
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
            old_name: "old".to_owned(),
            new_name: "new".to_owned(),
            new_by_id: ByIdPath("/dev/disk/by-id/x".into()),
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
        let op = OpKind::Add { disks: add_disks };

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
                old_name: "old".to_owned(),
                new_name: "new".to_owned(),
                new_by_id: ByIdPath("/dev/disk/by-id/virtio-new".into()),
            },
            pre_membership: pre,
            target_membership: target,
        }
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
            // Per-op soft RAID1 balance to drain any chunks a
            // cancelled-mid-flight balance worker left non-RAID1.
            .with_output(
                CmdRequest::BtrfsBalanceRaid1Soft {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw_empty("btrfs balance start"),
            )
            // LUKS dump used by probe_config_disk to classify each by-id
            // path as a real LUKS device.
            .with_luks_dump_text_luks2_for(&[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-old",
                "/dev/disk/by-id/virtio-new",
            ]);
        // Note: BtrfsBalanceStatus is NOT mocked. get_balance_report swallows
        // MissingMock as BalanceReport::Unknown, which cleanly skips the
        // balance-resume branch -- this test exercises both the resize
        // replay (mocked above) and the unconditional soft-balance replay
        // (mocked above).

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
            },
        );

        result.expect("recover should succeed via the mount cycle and replay the resize");

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
        let journal = two_disk_journal();
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
        };

        plan_recover(&runner, &fs, &params)
            .result
            .expect("plan_recover should render a dry-run preview")
            .preview()
            .render()
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

    /* Intent
     * Verify a not-mounted recover dry-run previews the full remount cycle.
     *
     * Why it exists
     * Execution unmounts, forgets btrfs scan state, closes mappers, reopens
     * LUKS, scans, and remounts before probing the recovered pool. The
     * dry-run preview must not hide that offline window.
     *
     * Scenario
     * Interrupted add with disk1 and disk2 present, disk3 absent, and
     * --allow-degraded supplied so the initial mount plan succeeds.
     */
    #[test]
    fn plan_recover_dry_run_includes_remount_cycle_when_not_mounted() {
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
            rendered.contains("unmount /mnt/storage (recover remount cycle)"),
            "missing cycle unmount step: {rendered:?}",
        );
        assert!(
            rendered.contains(
                "$ btrfs device scan --forget /dev/mapper/braid-disk1 /dev/mapper/braid-disk2"
            ),
            "missing pool-scoped scan --forget command: {rendered:?}",
        );
        assert!(
            rendered.contains("close LUKS mapper braid-disk1 (recover remount cycle)")
                && rendered.contains("close LUKS mapper braid-disk2 (recover remount cycle)"),
            "missing cycle close steps: {rendered:?}",
        );
        assert!(
            rendered.contains(
                "LUKS open /dev/disk/by-id/virtio-disk1 → braid-disk1 (recover remount cycle)"
            ) && rendered.contains(
                "LUKS open /dev/disk/by-id/virtio-disk2 → braid-disk2 (recover remount cycle)"
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
     * Interrupted add where disk3's by-id path is absent but
     * /dev/mapper/braid-disk3 exists at recover planning time.
     */
    #[test]
    fn plan_recover_dry_run_cycle_close_set_includes_absent_open_mapper() {
        let rendered = render_recover_dry_run(
            two_disk_journal(),
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/mapper/braid-disk3",
            ],
            closed_two_disk_dry_run_runner(),
            true,
        );

        assert!(
            rendered.contains(
                "$ btrfs device scan --forget /dev/mapper/braid-disk1 /dev/mapper/braid-disk2 /dev/mapper/braid-disk3"
            ),
            "scan --forget should include absent-but-open disk3 mapper: {rendered:?}",
        );
        assert!(
            rendered.contains("close LUKS mapper braid-disk3 (recover remount cycle)"),
            "close set should include disk3 mapper: {rendered:?}",
        );
        assert!(
            !rendered.contains("LUKS open /dev/disk/by-id/virtio-disk3"),
            "reopen set should not include absent disk3: {rendered:?}",
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
     * Interrupted add where disk3's by-id path and mapper path both exist,
     * but `cryptsetup luksUUID` fails, `isLuks` succeeds, and `luksDump`
     * fails, producing a damaged-header probe event.
     */
    #[test]
    fn plan_recover_dry_run_cycle_reopen_set_excludes_damaged_header_disk() {
        let runner = closed_two_disk_dry_run_runner()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/virtio-disk3".into(),
                },
                err_raw("cryptsetup luksUUID", 1, "LUKS metadata corrupted"),
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/virtio-disk3".into(),
                },
                ok_raw_empty("cryptsetup isLuks"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/virtio-disk3".into(),
                },
                err_raw("cryptsetup luksDump", 1, "LUKS2 metadata corrupted"),
            );

        let rendered = render_recover_dry_run(
            two_disk_journal(),
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/disk/by-id/virtio-disk3",
                "/dev/mapper/braid-disk3",
            ],
            runner,
            true,
        );

        assert!(
            rendered.contains("[skip] disk disk3: LUKS header metadata damaged"),
            "test setup should classify disk3 as damaged, got: {rendered:?}",
        );
        assert!(
            rendered.contains("close LUKS mapper braid-disk3 (recover remount cycle)"),
            "close set should include damaged disk3 mapper: {rendered:?}",
        );
        assert!(
            !rendered.contains(
                "LUKS open /dev/disk/by-id/virtio-disk3 → braid-disk3 (recover remount cycle)"
            ),
            "reopen set should not include damaged disk3: {rendered:?}",
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
     * disk1 mapper is already open, disk2 is closed, disk3 is absent. The
     * initial mount uses braid-disk2, while the cycle mount uses braid-disk1.
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
            .with_mapper_open(
                "braid-disk1",
                "/dev/vda",
                "11111111-1111-1111-1111-111111111111",
            )
            .with_mapper_closed("braid-disk2");

        let rendered = render_recover_dry_run(
            two_disk_journal(),
            &[
                "/dev/disk/by-id/virtio-disk1",
                "/dev/disk/by-id/virtio-disk2",
                "/dev/mapper/braid-disk1",
            ],
            runner,
            true,
        );

        let initial = "$ mount -o 'noatime,skip_balance,subvolid=5,degraded' /dev/mapper/braid-disk2 /mnt/storage";
        let cycle = "$ mount -o 'noatime,skip_balance,subvolid=5,degraded' /dev/mapper/braid-disk1 /mnt/storage";
        let initial_pos = rendered
            .find(initial)
            .unwrap_or_else(|| panic!("initial mount should use braid-disk2: {rendered:?}"));
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

    /* Intent
     * Verify replace dry-run previews post-mutation resize and balance
     * replay as conditional placeholders without command lines.
     *
     * Why it exists
     * Execution resolves the replacement disk devid from the post-mount live
     * pool, skips resize if replacement did not commit, and conditionally
     * resumes or replays balance work based on post-mount probes. Dry-run
     * cannot know that runtime state yet.
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
            rendered.contains("btrfs balance resume /mnt/storage (skipped if no paused balance)")
                && rendered.contains(
                    "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft /mnt/storage (skipped if pool has <2 devices)"
                ),
            "replace preview should include balance replay placeholders: {rendered:?}",
        );
        assert!(
            !rendered.contains("$ btrfs balance resume")
                && !rendered.contains("$ btrfs balance start --enqueue -dconvert=raid1,soft"),
            "conditional balance placeholders must not render commands: {rendered:?}",
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
