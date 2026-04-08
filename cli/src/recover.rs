use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::{self, Config};
use crate::discover;
use crate::journal::{self, Journal};
use crate::luks;
use crate::membership::{self, DiskMember, PoolMembership};
use crate::mount::{self, Credential, MountError};
use crate::parse::btrfs_filesystem_show::{classify_btrfs_probe, DeviceBtrfsProbe};
use crate::parse::{parse_btrfs_replace_status, ReplaceState};
use crate::probe::{self, Filesystem, ProbeError};
use crate::state_paths::StatePaths;
use crate::types::{ByIdPath, MountPoint};
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
}

pub fn cmd_recover<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    by_id_resolver: &dyn ByIdResolver,
    params: &RecoverParams<'_>,
) -> Result<(), RecoverError> {
    // 1. Load journal (required — nothing to recover if absent)
    let journal = match journal::load_journal(params.paths) {
        Ok(Some(j)) => j,
        Ok(None) => {
            return Err(RecoverError::Failed(
                "no pending operation journal found — nothing to recover".into(),
            ));
        }
        Err(e) => return Err(RecoverError::Journal(e.to_string())),
    };

    eprintln!(
        "Recovering from interrupted {:?} operation (started {})...",
        journal_op_label(&journal),
        journal.started_at
    );

    // 2. Open LUKS devices and mount the pool if needed
    let union = union_memberships(&journal);

    // Dry-run: probe + validate (same errors as execution), then print plan
    if params.dry_run {
        let plan = mount::plan_open_pool(
            runner,
            fs,
            params.config,
            &union,
            params.allow_degraded,
            "recover",
        )?;
        let mut steps = Vec::new();
        if let Some(plan) = &plan {
            steps.extend(mount::compile_open_steps(
                plan,
                params.config.mount_point(),
                None,
            ));
        } else {
            // Pool is already mounted — run the same read-only reconciliation
            // validation that execution does (probe_pool + membership construction).
            // This catches errors like "device X has no by-id path in either snapshot"
            // before claiming recovery is ready.
            let mount_point = params.config.mount_point();
            let pool = probe::probe_pool(runner, mount_point.as_str())?;
            for dev in &pool.devices {
                let Some(name) = config::name_from_mapper(&dev.mapper.0) else {
                    continue;
                };
                if !union.disks.contains_key(name) {
                    return Err(RecoverError::Failed(format!(
                        "device {} is in the live pool but has no by-id path in either \
                         the pre-operation or target membership snapshot.\n\
                         This must be resolved manually — provide the correct \
                         /dev/disk/by-id/ path and re-run recovery.",
                        dev.mapper.0
                    )));
                }
            }
        }
        // State recovery steps are always shown (recover writes pool.json even when mounted)
        steps.push(Step {
            risk: "safe",
            description: format!(
                "write recovered pool.json → {}",
                params.paths.pool_json().display()
            ),
            commands: vec![],
        });
        steps.push(Step {
            risk: "safe",
            description: format!(
                "clear pending-op.json → {}",
                params.paths.pending_op_json().display()
            ),
            commands: vec![],
        });
        Step::print_dry_run(&steps);
        return Ok(());
    }

    // The post-mount remount cycle (below) needs to re-supply the passphrase
    // when re-opening LUKS, so we can't lean on the lazy stdin-backed
    // Credential::Passphrase variant — that would require consuming stdin
    // twice. Read the secret once into a String and use it via
    // Credential::InMemoryPassphrase for both calls.
    //
    // The mountpoint check is the same one plan_open_pool does internally —
    // we hoist it here so we can skip the passphrase read entirely on the
    // already-mounted path (no LUKS open will happen).
    let mountpoint_check = runner
        .run(&CmdRequest::MountpointCheck {
            path: params.config.mount_point().clone(),
        })
        .map_err(|e| RecoverError::Failed(format!("recover: mountpoint check: {e}")))?;
    let already_mounted = mountpoint_check.exit_status == 0;

    let passphrase: Option<String> = if already_mounted {
        None
    } else {
        Some(
            luks::read_passphrase(params.passphrase_file, params.passphrase_stdin)
                .map_err(|e| RecoverError::Failed(format!("recover: {e}")))?,
        )
    };

    // Build the credential from a clone so the original String stays
    // available for the cycle's reopen below. The clone is consumed by
    // open_and_mount_pool; the kept copy is only dropped at end of
    // cmd_recover.
    let credential = match passphrase.as_ref() {
        Some(pp) => Credential::InMemoryPassphrase(pp.clone()),
        None => Credential::Passphrase {
            passphrase_stdin: false,
            passphrase_file: None,
        },
    };
    let just_mounted = match mount::open_and_mount_pool(
        runner,
        fs,
        params.config,
        &union,
        credential,
        params.allow_degraded,
        "recover",
    ) {
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
                    match runner.run(&CmdRequest::BtrfsFilesystemShowTarget { target: mapper }) {
                        Ok(raw) => {
                            matches!(classify_btrfs_probe(&raw), DeviceBtrfsProbe::NoBtrfs)
                        }
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
    };

    // 2.5 Force fresh kernel state before probing.
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
    // The fix is two steps:
    //
    //   1. Wait for the kernel resume to actually finish (poll
    //      `btrfs replace status` until it reports Finished or None).
    //   2. Cycle the mount end-to-end (umount + scan --forget + close
    //      LUKS + reopen + remount) so the kernel rebuilds fs_devices
    //      from the on-disk chunk tree. Empirically (see
    //      plans/wip/sharded-drifting-beaver-findings.md), `umount +
    //      scan --forget + remount` ALONE is not enough — the LUKS
    //      close+reopen is load-bearing.
    //
    // Skipped when the pool was already mounted before recover started:
    // we don't know who's using that mount and umount could fail with
    // EBUSY. The bug only manifests on the mount session that triggered
    // the kernel resume, which is one we just opened ourselves.
    if just_mounted {
        wait_for_kernel_replace_to_finish(runner, params.config.mount_point());
        // We mounted, so we read the passphrase above; it is non-None.
        let pp = passphrase
            .as_deref()
            .expect("just_mounted implies passphrase was read");
        relock_and_remount(
            runner,
            fs,
            params.config,
            &union,
            params.allow_degraded,
            pp,
        )?;
    }

    // 3. Probe live pool state
    let mount_point = params.config.mount_point().as_str();
    let pool = probe::probe_pool(runner, mount_point)?;

    // 4. Build new membership from live pool state
    let mut recovered = PoolMembership::empty();
    for dev in &pool.devices {
        let Some(name) = config::name_from_mapper(&dev.mapper.0) else {
            eprintln!("  skip: device {} has no braid- prefix", dev.mapper.0);
            continue;
        };
        // Sanity check: refuse to handle live pool members the journal never recorded.
        // The journal still records intent (which devices the operation was acting on);
        // an unknown device means something bypassed braid and we should not silently
        // adopt it.
        if !union.disks.contains_key(name) {
            return Err(RecoverError::Failed(format!(
                "device {} is in the live pool but has no by-id path in either \
                 the pre-operation or target membership snapshot.\n\
                 This must be resolved manually — provide the correct \
                 /dev/disk/by-id/ path and re-run recovery.",
                dev.mapper.0
            )));
        }
        // Resolve by_id from the live device's identity, not from the journal.
        // The journal value can be stale if hardware enumeration changed since the
        // mutation started; resolving against /dev/disk/by-id/ at recovery time
        // gives us a fresh, identity-bound symlink.
        let by_id = resolve_by_id_for_underlying(by_id_resolver, &dev.underlying)?;
        recovered.disks.insert(
            name.to_owned(),
            DiskMember::enriched(by_id, dev.luks_uuid.clone(), dev.devid),
        );
    }

    // 5. Report what changed
    let pre_names: std::collections::BTreeSet<_> = journal.pre_membership.disks.keys().collect();
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

    // 7. Clear journal
    journal::clear_journal(params.paths).map_err(|e| RecoverError::Journal(e.to_string()))?;
    eprintln!("pending-op.json cleared. Recovery complete.");

    // Best-effort: warn if a paused balance was detected (e.g. crash during
    // RAID1 conversion). skip_balance prevents kernel auto-resume.
    crate::status::emit_paused_balance_warning(runner, mount_point, &mut std::io::stderr());

    Ok(())
}

/// Block until any kernel-resumed btrfs dev_replace on `mount_point` finishes.
///
/// `btrfs_resume_dev_replace_async` runs as an unrelated kthread and is NOT
/// waited on by umount, so without this wait the relock_and_remount cycle can
/// race the resume worker and the second mount sees the same in-flight state.
///
/// Best-effort: if the status command fails for any reason, we return early
/// rather than blocking forever — relock_and_remount and probe_pool will catch
/// any remaining staleness as a clear test failure rather than a hang.
fn wait_for_kernel_replace_to_finish<R: CommandRunner>(runner: &R, mount_point: &MountPoint) {
    let mut last_pct: Option<f64> = None;
    loop {
        let raw = match runner.run(&CmdRequest::BtrfsReplaceStatus {
            mount_point: mount_point.clone(),
        }) {
            Ok(r) => r,
            Err(_) => return,
        };
        let parsed = match parse_btrfs_replace_status(&raw) {
            Ok(p) => p,
            Err(_) => return,
        };
        match parsed.state {
            ReplaceState::Finished | ReplaceState::None => return,
            ReplaceState::Running { pct } => {
                if last_pct != Some(pct) {
                    eprintln!(
                        "  waiting for kernel to finish resumed dev_replace... {pct:.1}%"
                    );
                    last_pct = Some(pct);
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
}

/// Drop all kernel state for the recovery mount and re-establish it from
/// scratch, so a subsequent probe_pool reads the post-resume on-disk topology
/// instead of the stale in-memory btrfs_fs_devices the kernel can carry
/// across a resumed dev_replace.
///
/// This mirrors what `braid lock; braid unlock` does end-to-end: umount,
/// `btrfs device scan --forget` (drop cached fs_devices), close every LUKS
/// mapper for the membership union, then re-open + remount via the standard
/// `open_and_mount_pool` helper.
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
    passphrase: &str,
) -> Result<(), RecoverError> {
    let mount_point = config.mount_point();

    // 1. Umount. The kernel waits for in-flight operations (including the
    //    dev_replace resume worker) to drain before releasing the mount.
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

    // 2. Drop cached btrfs_fs_devices. Without this, the kernel may
    //    re-attach the next mount to a still-cached structure that retains
    //    the stale post-resume topology, defeating the cycle.
    let forget = runner
        .run(&CmdRequest::BtrfsDeviceScanForget)
        .map_err(|e| RecoverError::Failed(format!("recover remount cycle: scan --forget: {e}")))?;
    if forget.exit_status != 0 {
        return Err(RecoverError::Failed(format!(
            "recover remount cycle: btrfs device scan --forget failed (exit {}): {}",
            forget.exit_status,
            forget.stderr.trim()
        )));
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
    }

    // 4. Re-open LUKS and mount via the standard helper. With the dm
    //    devices freshly recreated and the cached fs_devices dropped, the
    //    kernel reads the chunk tree from disk and rebuilds a fresh
    //    fs_devices reflecting the post-resume on-disk state.
    mount::open_and_mount_pool(
        runner,
        fs,
        config,
        membership,
        Credential::InMemoryPassphrase(passphrase.to_owned()),
        allow_degraded,
        "recover",
    )
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
                format!(
                    "add completed \u{2014} {} now in the pool.",
                    names.join(", ")
                )
            }
            journal::OpKind::Remove { name } => {
                format!("remove completed \u{2014} '{name}' is no longer in the pool.")
            }
            journal::OpKind::RemoveMissing { .. } => {
                "remove-missing completed \u{2014} missing device removed from the pool.".to_owned()
            }
            journal::OpKind::Replace {
                old_name, new_name, ..
            } => {
                format!("replace completed \u{2014} '{old_name}' replaced by '{new_name}'.")
            }
        }
    } else if recovered_names == pre_names {
        match op {
            journal::OpKind::Add { disks } => {
                let names: Vec<_> = disks.keys().map(|n| format!("'{n}'")).collect();
                format!(
                    "add did not complete \u{2014} {} not in the pool. Re-run braid add to retry.",
                    names.join(", ")
                )
            }
            journal::OpKind::Remove { name } => {
                format!(
                    "remove did not complete \u{2014} '{name}' is still in the pool. \
                     Re-run braid remove to retry."
                )
            }
            journal::OpKind::RemoveMissing { .. } => {
                "remove-missing did not complete \u{2014} device still in the pool. \
                 Re-run braid remove-missing to retry."
                    .to_owned()
            }
            journal::OpKind::Replace { old_name, .. } => {
                format!(
                    "replace did not complete \u{2014} pool still has '{old_name}'. \
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
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::journal::{self, OpKind};
    use crate::mount::MountError;
    use crate::probe::Filesystem;
    use crate::types::{ByIdPath, MountPoint};
    use std::collections::BTreeMap;

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

        fn read_to_string(&self, _path: &str) -> Result<String, std::io::Error> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
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
            resolver
                .canonicalize_results
                .insert(format!("/dev/disk/by-id/{filename}"), (*underlying).to_string());
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

    fn err_raw(cmd: &str, exit_code: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: String::new(),
            stderr: stderr.to_owned(),
            exit_status: exit_code,
        }
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

    fn findmnt_btrfs() -> RawCommandOutput {
        ok_raw(
            "findmnt --json --output TARGET,SOURCE,FSTYPE -T /mnt/storage",
            r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-toshiba","fstype":"btrfs"}]}"#,
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
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                findmnt_btrfs(),
            )
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
        let fs = MockFs::new(&[
            "/dev/disk/by-id/virtio-disk1",
            "/dev/disk/by-id/virtio-disk2",
        ]);

        let journal = two_disk_journal();
        journal::write_journal(&paths, &journal).unwrap();

        let (mp_req, mp_out) = mountpoint_fail();
        let runner = MockRunner::default()
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
            // mount helper: verify passphrase against first disk
            .with_output_stdin(
                CmdRequest::CryptsetupTestPassphrase {
                    device: "/dev/disk/by-id/virtio-disk1".into(),
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
            // remount cycle: scan --forget (drop cached fs_devices)
            .with_output(
                CmdRequest::BtrfsDeviceScanForget,
                ok_raw_empty("btrfs device scan --forget"),
            )
            // remount cycle: re-mount via the same MountWithOptions mock above
            // (MockRunner serves the same response for repeated requests)
            // probe_pool: findmnt
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                ),
            )
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
            );

        let passphrase_file = tempfile::NamedTempFile::new().unwrap();
        {
            use std::io::Write;
            passphrase_file.as_file().write_all(b"testpass").unwrap();
        }

        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk2"),
        ]);
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
            );
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
            );
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
            // probe_pool: findmnt
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                ),
            )
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
            );

        // No passphrase — pool is already mounted
        let resolver = resolver_for(&[
            ("/dev/vda", "virtio-disk1"),
            ("/dev/vdb", "virtio-disk2"),
        ]);
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
            );

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
            );

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
            );

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
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                ),
            )
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
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".into()),
                },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                ),
            )
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

        let resolved = resolve_by_id_for_underlying(&resolver, "/dev/sda")
            .expect("resolution should succeed");
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

        let resolved = resolve_by_id_for_underlying(&resolver, "/dev/sda")
            .expect("resolution should succeed");
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
            "add completed \u{2014} 'disk3' now in the pool."
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
            "add did not complete \u{2014} 'disk3' not in the pool. Re-run braid add to retry."
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
            "remove completed \u{2014} 'toshiba' is no longer in the pool."
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
            "remove did not complete \u{2014} 'toshiba' is still in the pool. Re-run braid remove to retry."
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
            "remove-missing completed \u{2014} missing device removed from the pool."
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
            "remove-missing did not complete \u{2014} device still in the pool. Re-run braid remove-missing to retry."
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
            "replace completed \u{2014} 'old' replaced by 'new'."
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
            "replace did not complete \u{2014} pool still has 'old'. Re-run braid replace to retry."
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
}
