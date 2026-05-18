use crate::alert;
use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::config_read;
use crate::confirm;
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal;
use crate::membership;
use crate::parse::parse_btrfs_device_usage;
use crate::pool::pool_remove_device_using;
use crate::preflight;
use crate::preview::{self, PerDiskStyle, PlanFailure, Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{Filesystem, ProbeError, probe_pool};
use crate::progress::{self, ProgressOutput};
use crate::state_paths::StatePaths;
use crate::status_tag::{StatusTag, color_enabled_for_stderr, status_line};
use crate::types::{DiskName, LuksUuid, MountPoint, PoolState};
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum RemoveMissingError {
    #[error("{0}")]
    Validation(String),
    /// `--missing-id <devid>` had no matching member in `pool.json` --
    /// the membership has never been enriched for that devid (or the
    /// devid is from a different pool). Pinned by the plan's
    /// "Forward `remove-missing` never-enriched refusal" test contract;
    /// the substring `no member in membership has devid {devid}` is
    /// load-bearing.
    #[error(
        "no member in membership has devid {devid} -- pool.json membership may need manual repair (run `braid status` to inspect)"
    )]
    NoMemberForDevid { devid: u64 },
    /// `pool.json` membership corruption surfaced from `by_devid` (two
    /// or more members share the same persisted devid). Wraps
    /// `MembershipError::DuplicateDevid` so the operator-facing
    /// remediation reads consistently with the rest of the membership
    /// errors.
    #[error("pool membership corruption: {0}")]
    Membership(#[from] membership::MembershipError),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("pool error: {0}")]
    Pool(#[from] crate::pool::PoolError),
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
}

/// Resolve a missing devid to a `(LuksUuid, DiskName)` pair via
/// `PoolMembership::by_devid`. Returns
/// `RemoveMissingError::NoMemberForDevid` when no member carries the
/// persisted devid (so the operator can decide whether enrichment ever
/// ran on the pool); propagates `MembershipError::DuplicateDevid` when
/// the membership is corrupt. This is the single point of identity
/// resolution for `remove-missing` -- callers thread the returned UUID
/// straight into the journal and the persisted-member removal.
fn resolve_removal_target(
    devid: u64,
    membership: &membership::PoolMembership,
) -> Result<(crate::types::LuksUuid, crate::types::DiskName), RemoveMissingError> {
    match membership.by_devid(devid)? {
        Some((uuid, member)) => Ok((uuid.clone(), member.name.clone())),
        None => Err(RemoveMissingError::NoMemberForDevid { devid }),
    }
}

pub struct RemoveMissingParams<'a> {
    pub config_path: &'a Path,
    pub missing_id: u64,
    pub dry_run: bool,
    pub yes: bool,
    pub progress: ProgressOutput,
    pub paths: &'a StatePaths,
    /// Seam for acquiring a logind sleep inhibitor before the irreversible
    /// portion of the remove-missing. Production passes `&RealSleepInhibitor`;
    /// unit tests pass `&RecordingInhibitor` to avoid spawning subprocesses.
    pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
    /// Seam for the device-remove heartbeat loop. Production passes
    /// `&progress::RealSleeper`; tests pass `&progress::NoopSleeper`
    /// so progress-path coverage does not pay real wall-clock time.
    pub sleeper: &'a dyn progress::Sleeper,
}

/// Dry-run preview source of truth for `braid remove-missing` plus the
/// execute inputs pre-computed during planning. `preview()` renders
/// accumulated notes plus steps from the semantic work plan; `execute()`
/// consumes the preflight state (`missing_id`, `missing_count`,
/// `remaining_present`) and renders any accumulated notes to stderr via
/// the shared `preview::render_notes_for_stderr` helper (canonical
/// `[warn] <body>` wording) before mutating.
pub struct RemoveMissingPlan {
    pub notes: Vec<PreviewNote>,
    work_plan: RemoveMissingWorkPlan,
}

#[derive(Debug, Clone)]
struct RemoveMissingWorkPlan {
    missing_id: u64,
    target_uuid: LuksUuid,
    target_name: DiskName,
    remaining_present: usize,
    missing_count: u64,
    mount_point: MountPoint,
}

impl RemoveMissingWorkPlan {
    fn restore_raid1_after_commit(&self) -> bool {
        self.missing_count == 1 && self.remaining_present >= 2
    }

    fn render_steps(&self) -> Vec<Step> {
        let mut steps = Vec::new();
        steps.push(Step {
            risk: "long",
            description: format!(
                "btrfs device remove {} (target specific missing device)",
                self.missing_id
            ),
            commands: vec![CmdRequest::BtrfsDeviceRemove {
                device: self.missing_id.to_string(),
                mount_point: self.mount_point.clone(),
            }],
        });
        if self.restore_raid1_after_commit() {
            steps.push(Step {
                risk: "long",
                description:
                    "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft (restore redundancy)"
                        .into(),
                commands: vec![CmdRequest::BtrfsBalanceRaid1Soft {
                    mount_point: self.mount_point.clone(),
                }],
            });
        }
        steps
    }
}

impl RemoveMissingPlan {
    /// Real-run and failure-path stderr for `remove-missing` use
    /// `Bracketed` per-disk style to match the canonical dry-run render.
    /// `remove-missing` does not emit `PerDisk` notes today, but the
    /// constant keeps the stderr-note contract uniform with the other
    /// migrated commands.
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
        params: &RemoveMissingParams<'_>,
    ) -> Result<(), RemoveMissingError> {
        // Render accumulated notes to stderr via the shared helper
        // before any mutation. Warn notes emit as the canonical
        // `[warn] <body>` (same as dry-run stdout), so both modes
        // share one render contract for plan-derived notes.
        preview::emit_notes_to_stderr(&self.notes, Self::STDERR_STYLE);

        let RemoveMissingPlan {
            notes: _,
            work_plan,
        } = self;

        let target_uuid = work_plan.target_uuid.clone();
        let name_to_remove = work_plan.target_name.clone();

        // Confirm
        if !params.yes {
            eprintln!(
                "{}",
                format_remove_missing_confirm(
                    name_to_remove.as_str(),
                    work_plan.missing_id,
                    work_plan.remaining_present,
                    work_plan.missing_count,
                )
            );
            confirm::confirm_yes().map_err(RemoveMissingError::Validation)?;
        }

        // Hold a logind sleep inhibitor for the rest of the remove-missing
        // operation -- covers the btrfs device remove (chunk relocation; can
        // run for minutes when the missing device had data allocated) and the
        // post-op maybe_restore_raid1 soft balance, which converts single-profile
        // chunks created during degraded operation back to RAID1. Suspending
        // mid-operation can leave chunks unprotected or force recovery.
        //
        // Acquired here, AFTER all interactive/reversible work (confirmation)
        // and BEFORE journal::write_journal, so that:
        //   - operator-idle prompts do not block suspend
        //   - a logind failure aborts cleanly without stranding pending-op.json
        //     and forcing the user into recovery mode for an environmental error.
        let _sleep_inhibitor_guard = params
            .sleep_inhibitor
            .acquire("removing missing device from pool")
            .map_err(|e| {
                RemoveMissingError::Validation(format!(
                    "could not acquire sleep inhibitor (is logind running?): {e}"
                ))
            })?;

        let pre_membership = membership::load_membership(params.paths).map_err(|e| {
            RemoveMissingError::Validation(format!("failed to load pool membership: {e}"))
        })?;
        // Re-resolve the requested devid against the fresh membership so
        // execution refuses pool.json drift that dry-run cannot see.
        let (fresh_uuid, _fresh_name) =
            resolve_removal_target(work_plan.missing_id, &pre_membership)?;
        if fresh_uuid != target_uuid {
            return Err(RemoveMissingError::Validation(format!(
                "membership drift between planning and execution: devid {} \
                 now resolves to a different member -- aborting to avoid \
                 removing the wrong entry",
                work_plan.missing_id,
            )));
        }

        // Build journal before btrfs operation. The member is removed
        // from `target_membership` by UUID; name is logging-only.
        let mut target_membership = pre_membership.clone();
        target_membership.remove_by_uuid(&target_uuid);
        let restore_raid1_after_commit = work_plan.restore_raid1_after_commit();
        let journal = journal::build_journal(
            pre_membership,
            target_membership.clone(),
            journal::OpKind::RemoveMissing {
                phase: journal::RemoveMissingPhase::PoolMutation,
                devid: work_plan.missing_id,
                restore_raid1_after_commit,
            },
        );
        journal::write_journal(params.paths, &journal)
            .map_err(|e| RemoveMissingError::Validation(e.to_string()))?;

        // Execute
        let color_enabled = color_enabled_for_stderr();
        eprint!(
            "{}",
            status_line(
                StatusTag::Wait,
                color_enabled,
                &format!("pool: removing missing devid {}...", work_plan.missing_id),
            )
        );
        pool_remove_device_using(
            runner,
            &work_plan.missing_id.to_string(),
            &work_plan.mount_point,
            params.progress,
            params.sleeper,
            &progress::StderrSink,
        )?;
        eprint!(
            "{}",
            status_line(
                StatusTag::Ok,
                color_enabled,
                &format!("pool: missing devid {} removed", work_plan.missing_id),
            )
        );

        // Membership committed by btrfs device remove. Persist before the
        // post-remove soft balance; the journal still covers maintenance,
        // so recovery can replay it if we crash before clear_journal.
        membership::save_membership(&target_membership, params.paths).map_err(|e| {
            RemoveMissingError::Validation(format!("failed to persist pool membership: {e}"))
        })?;

        let post_journal = journal::rewrite_journal(
            params.paths,
            &journal,
            journal::OpKind::RemoveMissing {
                phase: journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
                devid: work_plan.missing_id,
                restore_raid1_after_commit,
            },
            None,
        )
        .map_err(|e| RemoveMissingError::Validation(e.to_string()))?;

        if let journal::OpKind::RemoveMissing {
            restore_raid1_after_commit: true,
            ..
        } = post_journal.op
        {
            crate::pool::maybe_restore_raid1(
                runner,
                fs,
                &work_plan.mount_point,
                work_plan.missing_count,
                params.progress,
            )
            .map_err(RemoveMissingError::Pool)?;
        }

        // Maintenance complete -- safe to clear the journal.
        journal::clear_journal(params.paths)
            .map_err(|e| RemoveMissingError::Validation(e.to_string()))?;
        // Hygiene only -- failure is non-fatal because `cmd_add` is the
        // fail-closed correctness boundary for reused devids. See
        // docs/decisions/014-alerts.md "Acked-stats hygiene".
        if let Err(e) = alert::drop_ghost_acked_for_devids(params.paths, &[work_plan.missing_id]) {
            eprintln!("Warning: failed to update acked stats: {e}");
        }

        eprintln!("Done. Missing device removed from pool.");
        Ok(())
    }
}

/// Shared `--missing-id` classifier so dry-run, execute planning, and
/// overlap regression tests use the same btrfs-authoritative target order.
fn validate_missing_id_target(pool: &PoolState, missing_id: u64) -> Result<(), String> {
    if pool.devices.iter().any(|d| d.devid == missing_id) {
        return Err(format!(
            "devid {missing_id} is a live device, not a missing one. \
             Use 'braid remove' to remove live devices."
        ));
    }
    if pool.missing_devids.contains(&missing_id) {
        return Ok(());
    }
    if pool.null_underlying.iter().any(|d| d.devid == missing_id) {
        return Err(format!(
            "devid {missing_id} is hot-unplugged but btrfs has not yet \
             promoted it to MISSING (LUKS mapper open, backing device \
             gone). `braid remove-missing` only operates on \
             btrfs-authoritative MISSING devids. Confirm the disk is \
             truly gone, then relock and re-unlock the pool degraded \
             (`braid lock` then `braid unlock --allow-degraded`) so \
             btrfs promotes devid {missing_id}, and retry."
        ));
    }
    Err(format!(
        "devid {missing_id} is not a device in this pool. \
         Use 'braid status' to see device IDs."
    ))
}

/// Plan a `braid remove-missing` run. Owns everything above today's
/// `--dry-run` gate: pending-op preflight, config read, pool probe /
/// mounted validation, mutation preflight, UPS preflight,
/// missing-device validations, the relocation-space preflight, and
/// work-plan construction. On success, accumulated notes move into
/// `plan.notes`; on post-preflight failure, accumulated notes stay on
/// `PlanFailure::notes` so `cmd_remove_missing` can render them before
/// returning the error.
pub fn plan_remove_missing<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RemoveMissingParams<'_>,
) -> Result<RemoveMissingPlan, PlanFailure<RemoveMissingError>> {
    // Notes accumulator. Pre-preflight exits have no notes; later exits
    // preserve preflight diagnostics on `PlanFailure::notes`.
    let mut notes: Vec<PreviewNote> = Vec::new();

    if let Err(msg) = preflight::check_no_pending_operation(params.paths) {
        return Err(PlanFailure::empty(RemoveMissingError::Validation(msg)));
    }

    let config = match config_read(params.config_path) {
        Ok(c) => c,
        Err(e) => return Err(PlanFailure::empty(e.into())),
    };

    let pool = match probe_pool(runner, fs, config.mount_point()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return Err(PlanFailure::empty(RemoveMissingError::Validation(
                "pool is not mounted. Nothing to remove.".into(),
            )));
        }
        Err(e) => return Err(PlanFailure::empty(RemoveMissingError::Probe(e))),
    };

    if !pool.mounted {
        return Err(PlanFailure::empty(RemoveMissingError::Validation(
            "pool is not mounted. Nothing to remove.".into(),
        )));
    }

    // Preflight
    let fsid = pool.fsid.as_deref().expect("mounted pool must have FSID");
    match preflight::require_mutation_preflight(runner, fs, fsid, config.mount_point()) {
        Ok(preflight_notes) => notes.extend(preflight_notes),
        Err(msg) => return Err(PlanFailure::empty(RemoveMissingError::Validation(msg))),
    }
    if let Err(msg) = preflight::check_ups_not_on_battery(
        runner,
        config.ups().map(|u| u.name.as_str()),
        "remove-missing",
    ) {
        return Err(PlanFailure::with_notes(
            notes,
            RemoveMissingError::Validation(msg),
        ));
    }

    if pool.missing_count == 0 {
        return Err(PlanFailure::with_notes(
            notes,
            RemoveMissingError::Validation(format!(
                "no missing devices detected in pool (devid {} was not found among them).",
                params.missing_id
            )),
        ));
    }

    if let Err(msg) = validate_missing_id_target(&pool, params.missing_id) {
        return Err(PlanFailure::with_notes(
            notes,
            RemoveMissingError::Validation(msg),
        ));
    }

    // Pre-flight: reject the exact 2-device RAID1 + 1 missing case. The
    // kernel's btrfs_rm_device calls btrfs_check_raid_min_devices on
    // `num_devices - 1` (where num_devices is fs_devices->num_devices,
    // counting present + missing) and rejects with
    // BTRFS_ERROR_DEV_RAID1_MIN_NOT_MET when that drops below devs_min=2.
    // Per docs/decisions/012-intent-cli.md, remove-missing is cleanup-only;
    // the documented repair path for a dead disk on a 2-disk pool is
    // `braid replace --missing-id <devid>`. Pools with total_devices > 2
    // are intentionally out of scope here -- the kernel accepts those
    // calls, and reasoning about data integrity in multi-missing states
    // (where the survivor is not guaranteed to mirror every chunk under
    // btrfs RAID1's ncopies=2 layout) is left to existing/future logic.
    if pool.total_devices == 2 && pool.devices.len() == 1 && pool.missing_count == 1 {
        return Err(PlanFailure::with_notes(
            notes,
            RemoveMissingError::Validation(format!(
                "cannot remove missing devid {devid} -- this is a 2-disk \
                 RAID1 pool with one disk missing, and the kernel refuses \
                 to drop a RAID1 pool below two devices. Repair the dead \
                 disk with `braid replace --old <missing-name> \
                 --new <new-name>=/dev/disk/by-id/<...> --missing-id \
                 {devid}`, or run `braid add <new-name>=/dev/disk/by-id/<...>` \
                 first and then re-run `braid remove-missing`. \
                 Use `braid status` to see device names and IDs.",
                devid = params.missing_id,
            )),
        ));
    }

    // Resolve pool.json membership while planning so dry-run refuses the
    // same missing-devid identity failures as real execution.
    let pre_membership = match membership::load_membership(params.paths) {
        Ok(m) => m,
        Err(e) => {
            return Err(PlanFailure::with_notes(
                notes,
                RemoveMissingError::Validation(format!("failed to load pool membership: {e}")),
            ));
        }
    };
    let (target_uuid, target_name) =
        match resolve_removal_target(params.missing_id, &pre_membership) {
            Ok(p) => p,
            Err(e) => return Err(PlanFailure::with_notes(notes, e)),
        };

    // Pre-flight: reject if survivors lack space to absorb the missing
    // device's data. Without this check, btrfs will either ENOSPC or
    // crash the filesystem to read-only mid-relocation (see tests/repro/).
    //
    // Skip when only 1 present device survives: in 2-device RAID1, the
    // survivor already has all data (every chunk is mirrored). This does
    // not match the reproduced relocation-failure mode.
    if pool.devices.len() >= 2 {
        match check_relocation_space(runner, config.mount_point(), Some(params.missing_id)) {
            Ok(RelocationCheck::Proceed) => {}
            Ok(RelocationCheck::ProceedWithWarning(body)) => {
                notes.push(PreviewNote::Warn(body));
            }
            Err(e) => {
                return Err(PlanFailure::with_notes(notes, e));
            }
        }
    }

    let remaining_present = pool.devices.len();
    let work_plan = RemoveMissingWorkPlan {
        missing_id: params.missing_id,
        target_uuid,
        target_name,
        remaining_present,
        missing_count: pool.missing_count,
        mount_point: config.mount_point().clone(),
    };
    Ok(RemoveMissingPlan { notes, work_plan })
}

pub fn cmd_remove_missing<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RemoveMissingParams<'_>,
) -> Result<(), RemoveMissingError> {
    let plan = match plan_remove_missing(runner, fs, params) {
        Ok(p) => p,
        Err(PlanFailure { notes, error }) => {
            // Preserved-context failure: accumulated notes render to
            // stderr before the error via the SAME helper as the Ok
            // path (`RemoveMissingPlan::execute`), so preflight
            // diagnostics surface identically across success, failure,
            // and dry-run stdout.
            preview::emit_notes_to_stderr(&notes, RemoveMissingPlan::STDERR_STYLE);
            return Err(error);
        }
    };

    if params.dry_run {
        plan.preview().print_colored();
        return Ok(());
    }

    plan.execute(runner, fs, params)
}

/// Outcome of the relocation-space preflight. `Proceed` means either
/// the check ran and survivors have enough space, or it was skipped.
/// `ProceedWithWarning` means the check itself failed (command or
/// parse error) and the caller should surface the warning body to the
/// user but still proceed -- a bug in the safety net must not block a
/// valid operation. A hard "survivors lack space" outcome is a
/// `RemoveMissingError::Validation` instead.
#[derive(Debug)]
enum RelocationCheck {
    Proceed,
    ProceedWithWarning(String),
}

/// Check that surviving devices have enough RAID1-aware, per-type space to absorb
/// the missing device's allocations. If they don't, btrfs device remove will
/// either ENOSPC instantly or -- worse -- crash the filesystem to read-only
/// mid-relocation.
///
/// Missing devices are identified by `device_size == 0` in `btrfs device usage
/// --raw` output. This is reliable: present devices always have device_size > 0,
/// and missing devices always report 0. Their allocation lines (Data, Metadata,
/// System) are preserved and accurate.
///
/// If the check itself fails (parse error, command error), the caller receives
/// `ProceedWithWarning(body)` so it can surface the warning through the
/// preview + execute paths without this helper printing directly.
fn check_relocation_space<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    missing_id: Option<u64>,
) -> Result<RelocationCheck, RemoveMissingError> {
    let raw = match runner.run(&CmdRequest::BtrfsDeviceUsageRaw {
        mount_point: mount_point.clone(),
    }) {
        Ok(r) => r,
        Err(e) => {
            return Ok(RelocationCheck::ProceedWithWarning(format!(
                "ENOSPC pre-flight check failed: {e}; proceeding anyway"
            )));
        }
    };

    let usage = match parse_btrfs_device_usage(&raw) {
        Ok(u) => u,
        Err(e) => {
            return Ok(RelocationCheck::ProceedWithWarning(format!(
                "ENOSPC pre-flight check failed: {e}; proceeding anyway"
            )));
        }
    };

    // Partition: missing (device_size == 0, optionally filtered by devid) vs surviving (device_size > 0)
    let target: Vec<_> = usage
        .devices
        .iter()
        .filter(|d| d.device_size == 0 && (missing_id.is_none() || missing_id == Some(d.devid)))
        .collect();
    let remaining: Vec<_> = usage.devices.iter().filter(|d| d.device_size > 0).collect();

    preflight::check_raid1_relocation_space(&target, &remaining)
        .map(|()| RelocationCheck::Proceed)
        .map_err(|e| {
            RemoveMissingError::Validation(format!(
                "{e}\n\nFree up space by deleting files, or add a new device first with `braid add`."
            ))
        })
}

#[cfg(test)]
fn remove_missing_work_plan_for_test(
    missing_id: u64,
    missing_count: u64,
    remaining_present: usize,
    mount_point: &MountPoint,
) -> RemoveMissingWorkPlan {
    RemoveMissingWorkPlan {
        missing_id,
        target_uuid: LuksUuid::parse("00000000-0000-0000-0000-000000000001").unwrap(),
        target_name: DiskName::parse("disk-test").unwrap(),
        remaining_present,
        missing_count,
        mount_point: mount_point.clone(),
    }
}

// ---------------------------------------------------------------------------
// Confirmation formatter
// ---------------------------------------------------------------------------

fn format_remove_missing_confirm(
    name: &str,
    devid: u64,
    remaining_present: usize,
    missing_count: u64,
) -> String {
    let mut msg = "Remove missing device from pool:\n".to_string();
    msg.push_str(&format!(
        "  {} (devid {})  missing -- no hardware info available\n",
        name, devid
    ));
    if missing_count == 1 && remaining_present >= 2 {
        msg.push_str("  Data on remaining disks will be rebalanced.\n");
    } else if missing_count == 1 {
        msg.push_str("  Surviving disk already has all data.\n");
    } else {
        let remaining_missing = missing_count.saturating_sub(1);
        msg.push_str(&format!(
            "  Pool will remain degraded -- {} missing {} will remain.\n",
            remaining_missing,
            if remaining_missing == 1 {
                "entry"
            } else {
                "entries"
            },
        ));
    }
    if missing_count == 1 {
        msg.push_str(&format!(
            "\nPool: {} present + {} missing -> {} {}\n",
            remaining_present,
            missing_count,
            remaining_present,
            if remaining_present == 1 {
                "disk"
            } else {
                "disks"
            },
        ));
    } else {
        msg.push_str(&format!(
            "\nPool: {} present + {} missing -> {} present + {} missing\n",
            remaining_present,
            missing_count,
            remaining_present,
            missing_count.saturating_sub(1),
        ));
    }
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, MockRunner, RawCommandOutput};
    use crate::config::mapper_name;
    use crate::membership::PoolMembership;
    use crate::test_fixtures::{MockFs, PoolFixture, RemoveMissingPool, mock_ok};
    use crate::types::{NullUnderlyingDevice, PoolDevice};

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
    }

    fn target_validation_pool() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![],
            missing_count: 1,
            total_devices: 2,
            fsid: Some("cc86845b-aec3-408e-bef5-553affc1f2b1".to_owned()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    fn target_validation_device(devid: u64) -> PoolDevice {
        let name = DiskName::parse(&format!("disk{devid}")).expect("valid synthetic disk name");
        PoolDevice {
            mapper: mapper_name(&name),
            luks_uuid: LuksUuid::parse(&format!("00000000-0000-0000-0000-{devid:012x}"))
                .expect("valid synthetic UUID"),
            devid,
            underlying: format!("/dev/vd{devid}"),
        }
    }

    fn target_validation_null_underlying(devid: u64) -> NullUnderlyingDevice {
        let name = DiskName::parse(&format!("disk{devid}")).expect("valid synthetic disk name");
        NullUnderlyingDevice {
            mapper: mapper_name(&name),
            devid,
        }
    }

    struct EnospcRunner {
        device_usage_stdout: &'static str,
    }

    impl CommandRunner for EnospcRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(RawCommandOutput {
                    cmd: "btrfs device usage --raw /mnt/storage".to_owned(),
                    stdout: self.device_usage_stdout.to_owned(),
                    stderr: String::new(),
                    exit_status: 0,
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

    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    fn acked_disk(missing_acked: bool, read_io_errs: u64) -> alert::AckedDisk {
        alert::AckedDisk {
            missing_acked,
            device_stats: alert::AckedDeviceCounters {
                read_io_errs,
                ..Default::default()
            },
        }
    }

    struct HealthyPoolRunner;

    impl CommandRunner for HealthyPoolRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                )),
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_ok(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                    ),
                )),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_ok(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
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

    struct NullUnderlyingPoolRunner;

    impl CommandRunner for NullUnderlyingPoolRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_ok(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                )),
                CmdRequest::CryptsetupStatus { mapper } if mapper.as_str() == "braid-disk2" => {
                    Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  (null)\n  mode:    read/write\n"
                        ),
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_ok(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                    ),
                )),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_ok(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
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

    /*
     * Intent: cmd_remove_missing rejects the exact 2-disk RAID1 + 1-missing
     * case at preflight, with no side effects (no inhibitor acquire, no
     * journal write, no BtrfsDeviceRemove call).
     *
     * Why it exists: The kernel's
     * btrfs_check_raid_min_devices(num_devices - 1) rejects going below
     * two devices on RAID1; without this preflight braid would strand
     * pending-op.json and the sleep inhibitor for a doomed call, then
     * force the operator into `braid recover` for an operation that was
     * never going to succeed.
     *
     * Scenario: 2-disk NAS, disk2 dies. Operator reaches for
     * `braid remove-missing --missing-id 2`. braid rejects up-front and
     * names the supported repair paths.
     */
    #[test]
    fn single_survivor_rejected_at_preflight() {
        let f = PoolFixture::two_disk_devids_pinned();
        let (runner, _remove_done) =
            RemoveMissingPool::two_disk_one_missing().install(MockRunner::default());
        let params = f.remove_missing_params().missing_id(2).build();
        let result = cmd_remove_missing(&runner, &MockFs::storage(vec![]), &params);

        let err = result.expect_err("remove-missing must reject 2-disk RAID1 + 1 missing");
        let msg = match err {
            RemoveMissingError::Validation(m) => m,
            other => panic!("expected Validation, got {other:?}"),
        };
        assert!(
            msg.contains("2-disk RAID1 pool with one disk missing"),
            "error must name the rejected topology; got: {msg}"
        );
        assert!(
            msg.contains("braid replace"),
            "error must name the replace command as the repair path; got: {msg}"
        );
        assert!(
            msg.contains("--missing-id"),
            "error must name the --missing-id flag of replace; got: {msg}"
        );

        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "reject must land before the sleep inhibitor is acquired"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "reject must land before pending-op.json is written"
        );
        assert!(
            !runner
                .requests()
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. })),
            "reject must land before any btrfs device remove call"
        );
    }

    /*
     * Intent: the 2-disk RAID1 + 1-missing reject fires in dry-run too --
     * `cmd_remove_missing` runs `plan_remove_missing` first and surfaces
     * its `Err` before reaching the `if params.dry_run` branch.
     *
     * Why it exists: pins the invariant that the reject lives in
     * `plan_remove_missing`, not in `execute()`. A future refactor that
     * moved the check downstream would silently let `--dry-run` print a
     * doomed plan; this test fails first.
     *
     * Scenario: Same 2-disk NAS as the real-run case, operator runs
     * `braid remove-missing --missing-id 2 --dry-run`. braid still
     * rejects up-front -- no inhibitor, no journal, no btrfs calls.
     */
    #[test]
    fn single_survivor_rejected_in_dry_run() {
        let f = PoolFixture::two_disk_devids_pinned();
        let (runner, _remove_done) =
            RemoveMissingPool::two_disk_one_missing().install(MockRunner::default());
        let params = f
            .remove_missing_params()
            .missing_id(2)
            .dry_run(true)
            .build();
        let result = cmd_remove_missing(&runner, &MockFs::storage(vec![]), &params);

        let err =
            result.expect_err("remove-missing --dry-run must reject 2-disk RAID1 + 1 missing");
        let msg = match err {
            RemoveMissingError::Validation(m) => m,
            other => panic!("expected Validation, got {other:?}"),
        };
        assert!(
            msg.contains("2-disk RAID1 pool with one disk missing"),
            "error must name the rejected topology; got: {msg}"
        );
        assert!(
            msg.contains("braid replace"),
            "error must name the replace command as the repair path; got: {msg}"
        );
        assert!(
            msg.contains("--missing-id"),
            "error must name the --missing-id flag of replace; got: {msg}"
        );

        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "dry-run reject must not acquire the sleep inhibitor"
        );
        assert!(
            !runner
                .requests()
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. })),
            "dry-run reject must not call btrfs device remove"
        );
    }

    #[test]
    // Intent: check_relocation_space rejects when survivors lack space for the
    //   missing device's allocations.
    //
    // Why it exists: Without this pre-flight check, btrfs will either ENOSPC
    //   instantly or crash the filesystem to read-only mid-relocation.
    //
    // Scenario: 3-drive RAID1 pool, one drive dies. The dead drive has 2 GiB
    //   allocated but survivors only have 100 MiB unallocated total.
    fn check_relocation_space_rejects_insufficient_space() {
        // Missing device (devid 3): device_size=0, ~2 GiB allocated
        // Survivors (devid 1,2): 50 MiB unallocated each = 100 MiB total
        let fixture = "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Metadata,RAID1:              0
   Unallocated:            50331648

/dev/mapper/braid-disk2, ID: 2
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Metadata,RAID1:              0
   Unallocated:            50331648

<missing disk>, ID: 3
   Device size:                  0
   Device slack:                  0
   Data,RAID1:           2147483648
   Metadata,RAID1:        268435456
   System,RAID1:           33554432
   Unallocated:          1828716544

";

        let runner = EnospcRunner {
            device_usage_stdout: fixture,
        };

        let result = check_relocation_space(&runner, &mp(), None);
        let err = result.expect_err("should reject insufficient space");
        let msg = err.to_string();
        assert!(
            msg.contains("not enough space to relocate"),
            "expected 'not enough space to relocate' in: {msg}"
        );
    }

    #[test]
    // Intent: check_relocation_space passes when survivors have enough space.
    //
    // Why it exists: Ensures the check doesn't false-positive and block valid
    //   remove-missing operations.
    //
    // Scenario: Missing device has small allocations, survivors have plenty of
    //   unallocated space.
    fn check_relocation_space_passes_sufficient_space() {
        let fixture = "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:           452984832

/dev/mapper/braid-disk2, ID: 2
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:           452984832

<missing disk>, ID: 3
   Device size:                  0
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:                  0

";

        let runner = EnospcRunner {
            device_usage_stdout: fixture,
        };

        let result = check_relocation_space(&runner, &mp(), None);
        assert!(result.is_ok(), "should pass: {result:?}");
    }

    #[test]
    // Intent: check_relocation_space with --missing-id only counts allocations
    //   for the targeted devid, not all missing devices.
    //
    // Why it exists: When multiple devices are missing, removing just one may
    //   be feasible even if removing all isn't.
    //
    // Scenario: Two missing devices, but only one is targeted. The targeted
    //   device has small allocations that fit in survivors.
    fn check_relocation_space_with_missing_id_filters() {
        // Two surviving devices (4-disk pool, 2 missing). The RAID1-aware check
        // requires >= 2 surviving devices with space, which this fixture satisfies.
        let fixture = "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:           200000000

/dev/mapper/braid-disk4, ID: 4
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:           200000000

<missing disk>, ID: 2
   Device size:                  0
   Device slack:                  0
   Data,RAID1:             50000000
   Unallocated:                  0

<missing disk>, ID: 3
   Device size:                  0
   Device slack:                  0
   Data,RAID1:           5000000000
   Unallocated:                  0

";

        let runner = EnospcRunner {
            device_usage_stdout: fixture,
        };

        // Targeting devid 2 (50 MB Data) -- should pass: RAID1 capacity = 200 MB >= 50 MB
        let result = check_relocation_space(&runner, &mp(), Some(2));
        assert!(result.is_ok(), "targeting devid 2 should pass: {result:?}");

        // Targeting devid 3 (5 GB Data) -- should fail: RAID1 capacity = 200 MB < 5 GB
        let result = check_relocation_space(&runner, &mp(), Some(3));
        assert!(result.is_err(), "targeting devid 3 should fail");
    }

    #[test]
    // Intent: check_relocation_space proceeds gracefully when the command fails.
    //
    // Why it exists: A bug in the safety check shouldn't block a valid operation.
    //
    // Scenario: btrfs device usage returns an error (e.g., old kernel, permission issue).
    fn check_relocation_space_proceeds_on_command_error() {
        struct FailingRunner;
        impl CommandRunner for FailingRunner {
            fn run(&self, _request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
                Err(CmdError::MissingMock)
            }
            fn run_with_stdin(
                &self,
                request: &CmdRequest,
                _stdin: &[u8],
            ) -> Result<RawCommandOutput, CmdError> {
                self.run(request)
            }
        }

        let result = check_relocation_space(&FailingRunner, &mp(), None);
        assert!(result.is_ok(), "should proceed on error: {result:?}");
    }

    // --- work-plan render tests ---

    #[test]
    // Intent: dry-run with 1 missing + ≥2 survivors shows rebalance step.
    // Why: operator should see the soft balance step in the plan.
    // Scenario: 3-disk pool, 1 disk failed. Dry run should show the balance.
    fn work_plan_steps_show_rebalance_when_clearing_last_missing() {
        let steps = remove_missing_work_plan_for_test(3, 1, 2, &MountPoint("/mnt/storage".into()))
            .render_steps();
        assert!(
            steps
                .iter()
                .any(|s| s.description.contains("-dconvert=raid1,soft")),
            "expected soft balance step; got: {:?}",
            steps.iter().map(|s| &s.description).collect::<Vec<_>>()
        );
    }

    #[test]
    // Intent: dry-run with 1 survivor omits rebalance step.
    // Why: can't have RAID1 with only 1 device.
    // Scenario: 2-disk pool, 1 died. Only 1 survivor -- no balance.
    fn work_plan_steps_omit_rebalance_with_single_survivor() {
        let steps = remove_missing_work_plan_for_test(3, 1, 1, &MountPoint("/mnt/storage".into()))
            .render_steps();
        assert!(
            !steps
                .iter()
                .any(|s| s.description.contains("-dconvert=raid1,soft")),
            "should not show soft balance with 1 survivor; got: {:?}",
            steps.iter().map(|s| &s.description).collect::<Vec<_>>()
        );
    }

    #[test]
    // Intent: dry-run when not clearing last missing omits rebalance step.
    // Why: if more missing devices remain, balance would be premature.
    // Scenario: 4-disk pool, 2 missing, removing 1 of them.
    fn work_plan_steps_omit_rebalance_when_not_last_missing() {
        let steps = remove_missing_work_plan_for_test(3, 2, 2, &MountPoint("/mnt/storage".into()))
            .render_steps();
        assert!(
            !steps
                .iter()
                .any(|s| s.description.contains("-dconvert=raid1,soft")),
            "should not show soft balance when not clearing last missing; got: {:?}",
            steps.iter().map(|s| &s.description).collect::<Vec<_>>()
        );
    }

    // --- RecordingRunner for 3-device pool scenarios ---

    /*
     * Intent: a successful command-level `braid remove-missing` prunes the
     * acked-stats entry for the removed missing devid while preserving an
     * unrelated control entry.
     *
     * Why it exists: the cleanup callsite is after targeted btrfs device
     * remove, membership persist, optional soft balance, and journal clear.
     * Helper-level tests cannot catch a future command refactor that drops the
     * post-commit pruning.
     *
     * Scenario: the existing 3-device fixture has devid 3 reported as
     * MISSING. Removing that missing device must delete an old ghost ack for
     * key "3" and leave the present devid 1 ack unchanged.
     */
    #[test]
    fn cmd_remove_missing_prunes_acked_stats_for_removed_devid() {
        let f = PoolFixture::three_disk_devids_pinned();
        let control = acked_disk(false, 11);
        let target = acked_disk(true, 33);
        alert::save_acked_stats(
            &alert::AckedStats(std::collections::BTreeMap::from([
                ("1".to_owned(), control.clone()),
                ("3".to_owned(), target),
            ])),
            &f.paths,
        )
        .unwrap();

        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        cmd_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().build(),
        )
        .expect("remove-missing should succeed");

        let reloaded = alert::load_acked_stats(&f.paths);
        assert_eq!(
            reloaded.0.get("1"),
            Some(&control),
            "unrelated acked entry must be preserved"
        );
        assert!(
            !reloaded.0.contains_key("3"),
            "removed missing target devid must be pruned"
        );
    }

    /*
     * Intent: `plan_remove_missing` rejects a wrong `--missing-id` by
     * checking the missing devids reported by the already-probed pool.
     *
     * Why it exists: The target-validation contract must not regress
     * to a redundant `btrfs device usage --raw` probe before rejecting
     * an ID that is absent from `PoolState::missing_devids`.
     *
     * Scenario: a 3-device pool has devid 3 reported as `MISSING` by
     * `btrfs filesystem show`, but the operator passes
     * `--missing-id 99`.
     */
    #[test]
    fn plan_remove_missing_rejects_wrong_missing_id_from_pool_state() {
        let f = PoolFixture::three_disk_devids_pinned();

        struct WrongMissingIdRunner {
            log: Arc<Mutex<Vec<CmdRequest>>>,
        }

        impl CommandRunner for WrongMissingIdRunner {
            fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
                self.log.lock().unwrap().push(request.clone());
                match request {
                    CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_ok(
                        &format!("btrfs filesystem show {mount_point}"),
                        "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 3 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\tdevid    3 size 0 used 0 path MISSING\n",
                    )),
                    CmdRequest::CryptsetupStatus { mapper } => Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                        ),
                    )),
                    CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_ok(
                        "cryptsetup luksUUID",
                        "11111111-1111-1111-1111-111111111111\n",
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

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = WrongMissingIdRunner {
            log: Arc::clone(&log),
        };
        let failure = match plan_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params()
                .missing_id(99)
                .dry_run(true)
                .build(),
        ) {
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
            Err(failure) => failure,
        };

        match &failure.error {
            RemoveMissingError::Validation(msg) => assert_eq!(
                msg,
                "devid 99 is not a device in this pool. Use 'braid status' to see device IDs.",
            ),
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert!(
            !log.lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceUsageRaw { .. })),
            "wrong-id validation must not call BtrfsDeviceUsageRaw"
        );
    }

    #[test]
    // Intent: 3-disk pool, 1 missing -> soft rebalance runs after remove-missing.
    // Why: clearing the last missing device should restore RAID1 for chunks
    // written during degraded operation.
    // Scenario: 3-disk NAS, one drive dies. Operator runs remove-missing.
    // After the removal, pool is healthy with 2 survivors -> soft balance runs.
    fn three_device_pool_soft_rebalance_runs() {
        let f = PoolFixture::three_disk_devids_pinned();
        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        cmd_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().build(),
        )
        .expect("remove-missing should succeed");

        let calls = runner.requests();
        let remove_pos = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. }));
        let balance_pos = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsBalanceRaid1Soft { .. }));
        assert!(
            remove_pos.is_some(),
            "expected BtrfsDeviceRemove; calls: {calls:?}"
        );
        assert!(
            balance_pos.is_some(),
            "expected BtrfsBalanceRaid1Soft; calls: {calls:?}"
        );
        assert!(
            remove_pos.unwrap() < balance_pos.unwrap(),
            "remove-missing must happen before soft balance"
        );
        // Locks in the seam placement: a remove-missing that triggers the soft
        // balance must take the inhibitor exactly once before journal::write_journal,
        // and hold it across both the device remove and the soft balance.
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
        );
    }

    #[test]
    // Intent: 3-disk pool, 2 missing, targeting 1 -> NO rebalance (still degraded).
    // Why: running a balance while still degraded is pointless.
    // Scenario: 3-disk NAS, 2 drives die. Operator removes 1 missing entry.
    // Pool still has 1 missing device -> no rebalance.
    fn three_device_two_missing_no_rebalance() {
        let f = PoolFixture::three_disk_devids_pinned();
        let (runner, _remove_done) = RemoveMissingPool::three_disk_one_missing()
            .still_degraded_after(true)
            .install(MockRunner::default());
        cmd_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().build(),
        )
        .expect("remove-missing should succeed");

        let calls = runner.requests();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "should NOT call BtrfsBalanceRaid1Soft when still degraded; calls: {calls:?}"
        );
        // Even when no soft balance runs, the inhibitor must still be acquired
        // unconditionally before journal::write_journal -- the rule is "acquire
        // before journal", not "acquire when slow phase will run".
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once before journal::write_journal, \
             even when no soft balance runs"
        );
    }

    /*
     * Intent: cmd_remove_missing routes the device-remove phase through the
     * progress helper when progress output is enabled.
     * Why it exists: a direct runner.run or forced ProgressOutput::Off at this
     * layer would make slow `btrfs device remove <devid>` operations silent
     * again even though the pool helper itself supports heartbeats.
     * Scenario: a 3-device pool has one targeted missing device and remains
     * degraded after removal, so the only BtrfsDeviceRemove observed is the
     * remove-missing phase; the mock records which thread ran that command.
     */
    #[test]
    fn device_remove_runs_on_progress_worker_thread() {
        let f = PoolFixture::three_disk_devids_pinned();
        let (runner, remove_done) = RemoveMissingPool::three_disk_one_missing()
            .still_degraded_after(true)
            .install(MockRunner::default());
        let recorded: Arc<Mutex<Option<std::thread::ThreadId>>> = Arc::new(Mutex::new(None));
        let recorded_handler = Arc::clone(&recorded);
        let remove_done_handler = Arc::clone(&remove_done);
        let runner = runner.with_handler(move |req| match req {
            CmdRequest::BtrfsDeviceRemove { .. } => {
                *recorded_handler.lock().unwrap() = Some(std::thread::current().id());
                remove_done_handler.store(true, Ordering::SeqCst);
                Some(Ok(mock_ok("btrfs device remove", "")))
            }
            _ => None,
        });

        struct WaitForRemoveDoneSleeper {
            remove_done: Arc<AtomicBool>,
        }
        impl progress::Sleeper for WaitForRemoveDoneSleeper {
            fn sleep(&self, _duration: std::time::Duration) {
                while !self.remove_done.load(Ordering::SeqCst) {
                    std::thread::yield_now();
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        let sleeper = WaitForRemoveDoneSleeper {
            remove_done: Arc::clone(&remove_done),
        };
        let calling_thread = std::thread::current().id();

        cmd_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params()
                .progress(crate::progress::ProgressOutput::Human)
                .sleeper(&sleeper)
                .build(),
        )
        .expect("remove-missing should succeed");

        let observed = recorded
            .lock()
            .unwrap()
            .expect("BtrfsDeviceRemove must be dispatched");
        assert_ne!(
            observed, calling_thread,
            "BtrfsDeviceRemove must run on the progress helper worker thread when \
             ProgressOutput::Human is threaded through"
        );
    }

    /*
     * Intent: pending-op.json survives when the btrfs device-remove phase
     * fails before save_membership can run.
     *
     * Why it exists: remove-missing must not persist the target pool.json
     * until `btrfs device remove <devid>` succeeds. If save_membership ran
     * before pool_remove_device_using, this device-remove failure would
     * leave pool.json reconciled without the btrfs operation having
     * committed.
     *
     * Scenario: 3-disk NAS, one drive dies. Operator runs remove-missing, but
     * `btrfs device remove <devid>` fails mid-relocation with ENOSPC. The
     * journal must persist and pool.json must still contain the target disk so
     * `braid recover` can reconcile.
     */
    #[test]
    fn journal_survives_device_remove_failure() {
        let f = PoolFixture::three_disk_devids_pinned();
        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        let runner = runner.with_handler(|req| match req {
            CmdRequest::BtrfsDeviceRemove { .. } => Some(Ok(RawCommandOutput {
                cmd: "btrfs device remove 3 /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: error removing device: No space left on device".into(),
                exit_status: 1,
            })),
            _ => None,
        });
        let result = cmd_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().build(),
        );

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("btrfs device remove failed (exit 1)"),
            "remove-missing should fail from the device-remove step: {err}"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        assert!(
            membership::load_membership(&f.paths)
                .unwrap()
                .by_name(&crate::types::DiskName::parse("disk3").unwrap())
                .is_some(),
            "pool.json must still contain the target disk when device remove fails"
        );
        let calls = runner.requests();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. })),
            "expected BtrfsDeviceRemove; calls: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "soft balance must not run after device remove fails; calls: {calls:?}"
        );
        // The journal exists, which proves we got past journal::write_journal,
        // which proves the inhibitor was acquired exactly once on the way in.
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
        );
    }

    // Intent: pending-op.json survives when pool.json persistence fails after
    //   the btrfs device-remove phase has succeeded.
    // Why it exists: remove-missing must not advance the phased journal or run
    //   post-remove maintenance until the committed btrfs membership has been
    //   persisted to pool.json.
    // Scenario: 3-disk NAS, one drive dies. Operator runs remove-missing;
    //   `btrfs device remove <devid>` succeeds, but /var/lib/braid/pool.json
    //   cannot be rewritten. The journal must remain in PoolMutation so
    //   `braid recover` can reconcile from live btrfs state.
    #[test]
    fn journal_survives_save_membership_failure_after_device_remove() {
        let f = PoolFixture::three_disk_devids_pinned();
        let (runner, remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        let pool_json = f.paths.pool_json();
        let runner = runner.with_handler(move |req| match req {
            CmdRequest::BtrfsDeviceRemove { .. } => {
                remove_done.store(true, Ordering::SeqCst);
                std::fs::remove_file(&pool_json).expect("remove existing pool.json");
                std::fs::create_dir(&pool_json)
                    .expect("replace pool.json with directory to force save failure");
                Some(Ok(mock_ok("btrfs device remove", "")))
            }
            _ => None,
        });

        let result = cmd_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().build(),
        );

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("failed to persist pool membership"),
            "remove-missing should fail from the membership persist step: {err}"
        );
        assert!(
            err.contains("pool.json"),
            "membership persist failure should name pool.json: {err}"
        );
        let calls = runner.requests();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. })),
            "expected BtrfsDeviceRemove; calls: {calls:?}"
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "soft balance must not run after membership persist fails; calls: {calls:?}"
        );
        let journal = journal::load_journal(&f.paths)
            .unwrap()
            .expect("pending-op.json must survive membership persist failure");
        assert!(
            matches!(
                journal.op,
                journal::OpKind::RemoveMissing {
                    phase: journal::RemoveMissingPhase::PoolMutation,
                    ..
                }
            ),
            "journal must remain in PoolMutation until pool.json persists: {:?}",
            journal.op
        );
    }

    #[test]
    // Intent: pending-op.json survives when soft balance fails after a successful
    //   device removal.
    //
    // Why it exists: remove-missing previously cleared the journal before
    //   maybe_restore_raid1(). If the soft balance failed, the journal was already
    //   gone despite an irreversible pool change, leaving pool.json stale with
    //   no recovery path.
    //
    // Scenario: 3-disk NAS, one drive dies. Operator runs remove-missing. The
    //   device removal succeeds but the post-removal soft balance fails. The
    //   journal must persist so `braid recover` can reconcile.
    fn journal_survives_soft_balance_failure() {
        let f = PoolFixture::three_disk_devids_pinned();
        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        let runner = runner.with_handler(|req| match req {
            CmdRequest::BtrfsBalanceRaid1Soft { .. } => Some(Ok(RawCommandOutput {
                cmd: "btrfs balance start -dconvert=raid1,soft".into(),
                stdout: String::new(),
                stderr: "ERROR: error during balancing: No space left on device".into(),
                exit_status: 1,
            })),
            _ => None,
        });
        let result = cmd_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().build(),
        );

        assert!(
            result.is_err(),
            "remove-missing should fail when soft balance fails"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        let journal = journal::load_journal(&f.paths)
            .unwrap()
            .expect("journal should remain after post-remove maintenance failure");
        assert!(
            matches!(
                journal.op,
                journal::OpKind::RemoveMissing {
                    phase: journal::RemoveMissingPhase::PostRemoveMissingMaintenance,
                    ..
                }
            ),
            "journal should advance after btrfs device remove commits: {:?}",
            journal.op
        );
        // Membership commits at btrfs device remove; pool.json must reflect
        // the removed missing disk even when the post-remove soft balance
        // fails. Reverting save_membership back to its old position (after
        // maybe_restore_raid1) makes these assertions fail.
        let saved = membership::load_membership(&f.paths)
            .expect("pool.json must exist after the membership commit");
        let saved_names: Vec<&str> = saved.names().map(|n| n.as_str()).collect();
        assert!(
            saved
                .by_name(&crate::types::DiskName::parse("disk3").unwrap())
                .is_none(),
            "removed missing disk must be gone from pool.json even when the \
             post-remove soft balance fails (saved: {:?})",
            saved_names
        );
        assert!(
            saved
                .by_name(&crate::types::DiskName::parse("disk1").unwrap())
                .is_some()
                && saved
                    .by_name(&crate::types::DiskName::parse("disk2").unwrap())
                    .is_some(),
            "surviving disks must remain in pool.json (saved: {:?})",
            saved_names
        );
        // The journal exists, which proves we got past journal::write_journal,
        // which proves the inhibitor was acquired exactly once on the way in.
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
        );
    }

    #[test]
    // Intent: when soft balance fails with ENOSPC, the surfaced error includes
    //   the recovery hint with a concrete `dusage=0` command.
    // Why: the hint is appended in pool::balance_error, but it must survive
    //   PoolError -> RemoveMissingError::Pool -> Display without being lost.
    // Scenario: 3-disk NAS, one drive dies. Operator runs remove-missing. Device
    //   removal succeeds but the post-removal soft balance hits ENOSPC. The error
    //   message should guide the user to free empty block groups.
    fn enospc_hint_surfaces_through_error_chain() {
        let f = PoolFixture::three_disk_devids_pinned();
        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        let runner = runner.with_handler(|req| match req {
            CmdRequest::BtrfsBalanceRaid1Soft { .. } => Some(Ok(RawCommandOutput {
                cmd: "btrfs balance start -dconvert=raid1,soft".into(),
                stdout: String::new(),
                stderr: "ERROR: error during balancing: No space left on device".into(),
                exit_status: 1,
            })),
            _ => None,
        });
        let result = cmd_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().build(),
        );

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("hint:"),
            "error should contain recovery hint: {err}"
        );
        assert!(
            err.contains("dusage=0"),
            "error should suggest dusage=0 filter: {err}"
        );
    }

    // --- resolve_removal_target tests ---

    #[test]
    // Intent: resolve_removal_target fails when devid is known but no
    //   pool.json member has that devid enriched.
    //
    // Why it exists: If devid enrichment was skipped or failed, the lookup
    //   returns None. Previously the code silently proceeded, leaving
    //   pool.json unchanged despite the btrfs device being removed.
    //
    // Scenario: User enrolled a disk before devid enrichment existed, then
    //   the disk fails. remove-missing has a devid from btrfs but can't
    //   match it to any pool.json entry.
    fn resolve_target_fails_when_devid_not_in_membership() {
        let mut m = PoolMembership::empty();
        let (uuid, member) = crate::test_fixtures::disk_member_with(
            450,
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            None,
            None,
        );
        m.insert(uuid, member).expect("fixture insert");
        let err = resolve_removal_target(99, &m).unwrap_err();
        match &err {
            RemoveMissingError::NoMemberForDevid { devid } => assert_eq!(*devid, 99),
            other => panic!("expected NoMemberForDevid, got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("no member in membership has devid 99"),
            "expected pinned NoMemberForDevid wording; got: {msg}"
        );
    }

    #[test]
    // Intent: dry-run for targeted missing-device removal shows the devid command.
    // Why: verifies CmdRequest integration for the targeted removal path.
    // Scenario: one missing device (devid 2), last missing, 2 present -> includes balance.
    fn dry_run_render_targeted_removal_with_balance() {
        let mount_point = MountPoint("/mnt/storage".into());
        let steps = remove_missing_work_plan_for_test(2, 1, 2, &mount_point).render_steps();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // 2 steps: device remove + balance, each with 1 command = 4 lines
        assert_eq!(lines.len(), 4, "expected 4 lines, got:\n{output}");
        assert!(lines[0].contains("target specific missing device"));
        assert_eq!(
            lines[1],
            "               $ btrfs device remove --enqueue 2 /mnt/storage"
        );
        assert!(lines[2].contains("restore redundancy"));
        assert_eq!(
            lines[3],
            "               $ btrfs balance start --enqueue '-dconvert=raid1,soft' '-mconvert=raid1,soft' /mnt/storage"
        );
    }

    // --- Confirmation formatter tests ---

    #[test]
    fn remove_missing_confirm_with_rebalance() {
        let msg = format_remove_missing_confirm("toshiba", 2, 2, 1);
        assert!(msg.contains("Remove missing device from pool:"));
        assert!(msg.contains("toshiba (devid 2)"));
        assert!(msg.contains("missing"));
        assert!(msg.contains("no hardware info available"));
        assert!(msg.contains("rebalanced"));
        assert!(msg.contains("2 present + 1 missing -> 2 disks"));
    }

    #[test]
    fn remove_missing_confirm_single_survivor() {
        let msg = format_remove_missing_confirm("toshiba", 2, 1, 1);
        assert!(msg.contains("Surviving disk already has all data"));
        assert!(msg.contains("1 present + 1 missing -> 1 disk"));
    }

    // Intent: verify the confirm prompt accurately describes residual
    //   degradation and does not promise a rebalance when the pool stays
    //   degraded -- exercising both new branches added for missing_count >= 2.
    // Why it exists: regression guard against (a) the "-> X disk(s)" post-op
    //   shape that previously implied a fully-restored pool when one or more
    //   missing entries remain, and (b) the "Data on remaining disks will be
    //   rebalanced" hint that previously promised a balance step that the
    //   planner does not actually queue when missing_count > 1.
    // Scenario: pool stays degraded after remove-missing because more than
    //   one missing entry exists. The (1, 2) case models a 3-disk RAID1 with
    //   2 dead drives (total_devices = remaining_present + missing_count = 3),
    //   removing one of the two missing entries. The (2, 2) case models a
    //   4-disk RAID1 with 2 dead drives, removing the first of two missing
    //   entries -- this case is the one that catches a regression of the
    //   rebalance-hint fix.
    #[test]
    fn remove_missing_confirm_multiple_missing() {
        let cases: &[(usize, u64, &str, &str)] = &[
            (
                1,
                2,
                "1 present + 2 missing -> 1 present + 1 missing",
                "-> 1 disk",
            ),
            (
                2,
                2,
                "2 present + 2 missing -> 2 present + 1 missing",
                "-> 2 disks",
            ),
        ];

        for (rp, mc, expected_shape, forbidden_shape) in cases {
            let msg = format_remove_missing_confirm("toshiba", 2, *rp, *mc);
            assert!(
                msg.contains(expected_shape),
                "case ({rp}, {mc}): expected post-op shape {expected_shape:?} in:\n{msg}"
            );
            assert!(
                msg.contains("Pool will remain degraded"),
                "case ({rp}, {mc}): expected degraded hint in:\n{msg}"
            );
            assert!(
                !msg.contains("rebalanced"),
                "case ({rp}, {mc}): unexpected rebalance promise in:\n{msg}"
            );
            assert!(
                !msg.contains("Surviving disk already has all data"),
                "case ({rp}, {mc}): unexpected single-survivor hint in:\n{msg}"
            );
            assert!(
                !msg.contains(forbidden_shape),
                "case ({rp}, {mc}): unexpected fully-restored shape {forbidden_shape:?} in:\n{msg}"
            );
        }
    }

    // --- plan_remove_missing soft-warn tests ---

    /* Intent: when the relocation-space preflight fails with a command
     * error, `plan_remove_missing` returns a successful plan carrying
     * one `PreviewNote::Warn` with the ENOSPC soft-warn body and the
     * usual compiled steps.
     *
     * Why it exists: PR 3 moves the direct `eprintln!("warning: ...")`
     * into the preview model. Without this test, a regression that
     * dropped the note or re-added the direct stderr print from the
     * preflight helper would still pass the older "proceeds on command
     * error" test (which only asserts control flow).
     *
     * Scenario: 3-disk RAID1 pool with devid 3 missing; the
     * `btrfs device usage --raw` call from check_relocation_space fails
     * with a CmdError so the planner routes the soft-warn into
     * plan.notes instead of stderr.
     */
    #[test]
    fn plan_remove_missing_surfaces_soft_warn_on_command_error() {
        let f = PoolFixture::three_disk_devids_pinned();
        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        let runner = runner.with_handler(|req| match req {
            CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Err(CmdError::MissingMock)),
            _ => None,
        });
        let plan = plan_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().dry_run(true).build(),
        )
        .expect("planning should succeed");
        assert_eq!(plan.notes.len(), 1, "expected exactly one soft-warn note");
        match &plan.notes[0] {
            PreviewNote::Warn(body) => {
                assert!(
                    body.starts_with("ENOSPC pre-flight check failed: "),
                    "warning body must start with the canonical prefix; got {body:?}"
                );
                assert!(
                    body.ends_with("; proceeding anyway"),
                    "warning body must end with the canonical suffix; got {body:?}"
                );
                assert!(
                    !body.starts_with("warning:"),
                    "warn body must not carry the legacy 'warning:' prefix; got {body:?}"
                );
            }
            other => panic!("expected PreviewNote::Warn, got {other:?}"),
        }
        // Steps still render: devid 3 is the last missing in a
        // 2-survivor pool so the soft balance step must be present.
        let preview = plan.preview();
        let descriptions: Vec<&str> = preview
            .steps
            .iter()
            .map(|s| s.description.as_str())
            .collect();
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("target specific missing device")),
            "expected device remove step; got {descriptions:?}"
        );
        assert!(
            descriptions
                .iter()
                .any(|d| d.contains("restore redundancy")),
            "expected soft balance step; got {descriptions:?}"
        );
    }

    /* Intent: when the relocation-space preflight's parse fails, the
     * planner still returns a successful plan carrying the soft-warn
     * note, same as the command-error branch.
     *
     * Why it exists: `check_relocation_space` has two soft-warn
     * branches (command error, parse error). Both used to share one
     * `eprintln!` site; after the refactor, each builds a warn body
     * independently. This test is the parse-error twin of the
     * command-error test above and guards against drift between the
     * two bodies.
     *
     * Scenario: same 3-disk pool; the `btrfs device usage --raw` call
     * returns unparseable stdout, triggering
     * `parse_btrfs_device_usage` to error.
     */
    #[test]
    fn plan_remove_missing_surfaces_soft_warn_on_parse_error() {
        let f = PoolFixture::three_disk_devids_pinned();
        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        let runner = runner.with_handler(|req| match req {
            CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(RawCommandOutput {
                cmd: "btrfs device usage --raw".to_owned(),
                stdout: String::new(),
                stderr: "boom".to_owned(),
                exit_status: 1,
            })),
            _ => None,
        });
        let report = plan_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().dry_run(true).build(),
        );
        let plan = report.expect("planning should succeed");
        assert_eq!(plan.notes.len(), 1, "expected exactly one soft-warn note");
        match &plan.notes[0] {
            PreviewNote::Warn(body) => {
                assert!(
                    body.starts_with("ENOSPC pre-flight check failed: "),
                    "warning body must start with the canonical prefix; got {body:?}"
                );
                assert!(
                    body.ends_with("; proceeding anyway"),
                    "warning body must end with the canonical suffix; got {body:?}"
                );
            }
            other => panic!("expected PreviewNote::Warn, got {other:?}"),
        }
        assert!(!plan.preview().steps.is_empty(), "steps must still render");
    }

    /* Intent: `plan.preview().render()` places the ENOSPC soft-warn
     * line above the step block and uses the canonical `[warn] <body>`
     * shape (no legacy `warning:` prefix).
     *
     * Why it exists: the dry-run stdout contract for remove-missing in
     * PR 3 is "warn note(s) render before steps" and the warn body is
     * body-only. Without a preview-boundary test, a regression that
     * rendered the warn inline with steps, dropped the `[warn] `
     * prefix, or re-added the `warning:` prefix would only surface in
     * the VM stream-routing test -- adding a unit guardrail here is
     * cheap and catches drift before the VM layer.
     *
     * Scenario: a hand-built plan with one soft-warn note and the
     * compiled steps for devid-3 removal on a 2-survivor pool; assert
     * the rendered byte sequence starts with the warn line and is
     * followed by the dry-run step lines.
     */
    #[test]
    fn plan_preview_renders_warn_above_steps() {
        let work_plan =
            remove_missing_work_plan_for_test(3, 1, 2, &MountPoint("/mnt/storage".into()));
        let plan = RemoveMissingPlan {
            notes: vec![PreviewNote::Warn(
                "ENOSPC pre-flight check failed: boom; proceeding anyway".into(),
            )],
            work_plan,
        };
        let rendered = plan.preview().render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines[0], "[warn] ENOSPC pre-flight check failed: boom; proceeding anyway",
            "warn note must render first; got full output:\n{rendered}"
        );
        assert!(
            lines[1].starts_with("[long"),
            "step block must follow the warn line; got lines[1]={:?}",
            lines[1]
        );
        assert!(
            lines[1].contains("target specific missing device"),
            "first step must be the device-remove step; got lines[1]={:?}",
            lines[1]
        );
    }

    /* Intent: plan-derived Warn notes for `remove-missing` render
     * through the shared `preview::render_notes_for_stderr` helper as
     * the canonical `[warn] <body>\n` shape -- the same shape that
     * `Preview::render` emits on dry-run stdout. Legacy `warning: `
     * prefixes do not appear.
     * Why it exists: this follow-up removes the direct
     * `eprintln!("warning: {body}")` replay from
     * `RemoveMissingPlan::execute` so real-run stderr now uses the
     * canonical form. A regression that reintroduces the legacy
     * prefix -- either in execute's replay or by re-wrapping the body
     * -- fails here.
     * Scenario: hand-built notes vec with one soft-warn body; render
     * via `RemoveMissingPlan::STDERR_STYLE` and assert byte-exact
     * output with no `warning:` substring.
     */
    #[test]
    fn remove_missing_warn_notes_render_canonical_bracketed_form() {
        let notes = vec![PreviewNote::Warn(
            "ENOSPC pre-flight check failed: boom; proceeding anyway".into(),
        )];
        let rendered = preview::render_notes_for_stderr(&notes, RemoveMissingPlan::STDERR_STYLE);
        assert_eq!(
            rendered,
            "[warn] ENOSPC pre-flight check failed: boom; proceeding anyway\n",
        );
        assert!(
            !rendered.contains("warning:"),
            "legacy `warning:` prefix must be gone from remove-missing's render;\n{rendered}",
        );
    }

    /* Intent: plan_remove_missing surfaces an in-flight exclusive op
     * as a PreviewNote::Info on `plan.notes`, and the rendered preview
     * contains the "waiting for in-flight <op>" line.
     * Why it exists: PR 7 moves the busy-op diagnostic from a direct
     * stderr eprintln! into plan.notes. A regression that leaked the
     * wording back to stderr would break the dry-run stdout-only
     * contract.
     * Scenario: 3-device pool with 1 missing device, sysfs reports
     * "device add". Operator runs `braid remove-missing --missing-id 3
     * --dry-run`.
     */
    #[test]
    fn plan_remove_missing_preflight_busy_op_becomes_info_note() {
        let f = PoolFixture::three_disk_devids_pinned();
        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        let report = plan_remove_missing(
            &runner,
            &MockFs::storage(vec![]).with_excl_op("device add\n"),
            &f.remove_missing_params().dry_run(true).build(),
        );
        let plan = report.expect("plan_remove_missing should succeed with 1 missing + busy op");
        let info_notes: Vec<&String> = plan
            .notes
            .iter()
            .filter_map(|n| match n {
                PreviewNote::Info(b) => Some(b),
                _ => None,
            })
            .collect();
        assert_eq!(
            info_notes.len(),
            1,
            "expected exactly one Info note, got: {:?}",
            plan.notes
        );
        assert!(
            info_notes[0].contains("waiting for in-flight"),
            "info body={:?}",
            info_notes[0],
        );
        assert!(
            info_notes[0].contains("device add"),
            "info body={:?}",
            info_notes[0],
        );
        let rendered = plan.preview().render();
        assert!(
            rendered.contains("waiting for in-flight device add"),
            "rendered preview must carry the busy-op Info line, got:\n{rendered}",
        );
    }

    /* Intent: when plan_remove_missing accumulates a preflight Info
     * note and then fails on the "no missing devices" validation
     * (missing_count == 0), the accumulated notes survive on
     * `PlanFailure::notes`.
     * Why it exists: `PlanFailure::notes` promises preserved context; a
     * spurious remove-missing invocation during an in-flight balance
     * must not hide the busy-op context from the operator.
     * Scenario: 2-device healthy pool (zero missing), sysfs reports
     * "device add". Operator runs `braid remove-missing --missing-id
     * 999 --dry-run`.
     */
    #[test]
    fn plan_remove_missing_preserves_preflight_notes_on_no_missing_devices() {
        let f = PoolFixture::two_disk_devids_pinned();
        let runner = HealthyPoolRunner;
        let failure = match plan_remove_missing(
            &runner,
            &MockFs::storage(vec![]).with_excl_op("device add\n"),
            &f.remove_missing_params()
                .missing_id(999)
                .dry_run(true)
                .build(),
        ) {
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
            Err(failure) => failure,
        };
        match &failure.error {
            RemoveMissingError::Validation(msg) => {
                assert!(
                    msg.contains("no missing devices detected"),
                    "expected 'no missing devices detected' in: {msg}"
                );
                assert!(
                    msg.contains("devid 999"),
                    "expected requested devid in no-missing validation: {msg}"
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert_eq!(
            failure.notes.len(),
            1,
            "busy-op Info note must survive the no-missing failure, got: {:?}",
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

    /* Intent: a healthy pool with a live-device `--missing-id` still fails
     * with the zero-missing validation before live-device validation.
     * Why it exists: the explicit `missing_count == 0` branch preserves
     * existing error precedence; moving that wording into the membership
     * check would change the user-facing error for live devids in healthy
     * pools.
     * Scenario: 2-device healthy pool. Operator accidentally runs
     * `braid remove-missing --missing-id 1 --dry-run`.
     */
    #[test]
    fn plan_remove_missing_zero_missing_precedes_live_device_validation() {
        let f = PoolFixture::two_disk_devids_pinned();
        let failure = match plan_remove_missing(
            &HealthyPoolRunner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params()
                .missing_id(1)
                .dry_run(true)
                .build(),
        ) {
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
            Err(failure) => failure,
        };

        match &failure.error {
            RemoveMissingError::Validation(msg) => {
                assert!(
                    msg.contains("no missing devices detected"),
                    "expected no-missing validation, got: {msg}"
                );
                assert!(
                    !msg.contains("live device"),
                    "zero-missing validation must precede live-device validation: {msg}"
                );
                assert!(
                    msg.contains("devid 1"),
                    "expected requested devid in no-missing validation: {msg}"
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
    }

    /* Intent: a null-underlying hot-unplugged device is not reported as
     * "no missing devices" just because `PoolState::missing_devids` is empty.
     * Why it exists: `remove-missing` must key the no-missing wording on
     * `missing_count == 0`, not `missing_devids.is_empty()`. Hot-unplugged
     * mapper-present devices can yield `missing_count > 0` while btrfs has
     * not emitted a `MISSING` sentinel devid.
     * Scenario: 2-device pool where btrfs still lists both mapper paths, but
     * `cryptsetup status braid-disk2` reports `device: (null)`. Operator tries
     * `braid remove-missing --missing-id 2 --dry-run`.
     */
    #[test]
    fn plan_remove_missing_null_underlying_empty_missing_devids_not_no_missing() {
        let f = PoolFixture::two_disk_devids_pinned();
        let failure = match plan_remove_missing(
            &NullUnderlyingPoolRunner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params()
                .missing_id(2)
                .dry_run(true)
                .build(),
        ) {
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
            Err(failure) => failure,
        };

        match &failure.error {
            RemoveMissingError::Validation(msg) => {
                assert!(
                    !msg.contains("no missing devices detected"),
                    "null-underlying pool must not use no-missing wording: {msg}"
                );
                assert_eq!(
                    msg,
                    "devid 2 is hot-unplugged but btrfs has not yet promoted it to MISSING \
                     (LUKS mapper open, backing device gone). `braid remove-missing` only \
                     operates on btrfs-authoritative MISSING devids. Confirm the disk is truly \
                     gone, then relock and re-unlock the pool degraded (`braid lock` then \
                     `braid unlock --allow-degraded`) so btrfs promotes devid 2, and retry.",
                );
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
    }

    // Intent: live devids are classified before missing or null-underlying
    // state.
    // Why it exists: `remove-missing` must never treat a present pool member
    // as a destructive missing-device target.
    // Scenario: the operator passes a live btrfs devid to
    // `braid remove-missing --missing-id`.
    #[test]
    fn validate_missing_id_target_live_rejected() {
        let mut pool = target_validation_pool();
        pool.devices.push(target_validation_device(2));
        pool.missing_devids.push(2);
        pool.null_underlying
            .push(target_validation_null_underlying(2));

        let msg = validate_missing_id_target(&pool, 2).unwrap_err();
        assert_eq!(
            msg,
            "devid 2 is a live device, not a missing one. Use 'braid remove' to remove live devices."
        );
    }

    // Intent: btrfs-authoritative MISSING devids are accepted.
    // Why it exists: the helper is the sole per-target gate before
    // destructive remove-missing planning.
    // Scenario: btrfs has promoted devid 2 to MISSING and the operator
    // supplies that exact devid.
    #[test]
    fn validate_missing_id_target_authoritative_missing_accepted() {
        let mut pool = target_validation_pool();
        pool.missing_devids.push(2);

        validate_missing_id_target(&pool, 2).expect("authoritative missing devid should pass");
    }

    // Intent: null-underlying-only devids get the hot-unplug diagnostic.
    // Why it exists: a hot-unplugged mapper contributes to status alerts but
    // is not yet a btrfs-authoritative remove-missing target.
    // Scenario: cryptsetup reports `device: (null)` for devid 2 while btrfs
    // has not promoted that devid to MISSING.
    #[test]
    fn validate_missing_id_target_null_underlying_only_rejected() {
        let mut pool = target_validation_pool();
        pool.null_underlying
            .push(target_validation_null_underlying(2));

        let msg = validate_missing_id_target(&pool, 2).unwrap_err();
        assert_eq!(
            msg,
            "devid 2 is hot-unplugged but btrfs has not yet promoted it to MISSING \
             (LUKS mapper open, backing device gone). `braid remove-missing` only \
             operates on btrfs-authoritative MISSING devids. Confirm the disk is truly \
             gone, then relock and re-unlock the pool degraded (`braid lock` then \
             `braid unlock --allow-degraded`) so btrfs promotes devid 2, and retry."
        );
    }

    // Intent: the overlap of btrfs MISSING and null-underlying is accepted.
    // Why it exists: status alerting deduplicates this defensive state, and
    // command validation must keep btrfs-authoritative MISSING ahead of the
    // null-underlying refusal.
    // Scenario: btrfs has promoted a hot-unplugged devid to MISSING while
    // its mapper still reports a null backing device.
    #[test]
    fn validate_missing_id_target_missing_and_null_underlying_accepted() {
        let mut pool = target_validation_pool();
        pool.missing_devids.push(2);
        pool.null_underlying
            .push(target_validation_null_underlying(2));

        validate_missing_id_target(&pool, 2)
            .expect("authoritative missing devid should win over null-underlying");
    }

    // ---------------------------------------------------------------------
    // UUID-identity boundary tests (Phase 3a)
    //
    // Test-module seed allocation note: remove_missing.rs uses 450-499 for
    // new UUID-identity tests, leaving 100-199 to membership.rs, 200 to
    // luks.rs, 201-299 to journal.rs, 300-399 to cmd.rs, 400-449 to
    // remove.rs.
    // ---------------------------------------------------------------------

    use crate::membership::DiskMember;
    use crate::test_fixtures::test_uuid;
    use crate::types::{ByIdPath, DiskName, LuksUuid};

    // Intent: cmd_remove_missing for the persisted devid resolves to a
    //   single UUID via membership.by_devid, removes that UUID from
    //   target_membership, and issues ZERO `cryptsetup luksUUID` requests
    //   for the missing target (no backing device to probe).
    //
    // Why: this is the positive-path UUID ownership contract. A regression that
    //   probed cryptsetup for the missing device's by_id would fail
    //   because the device is gone; a regression that used name-keyed
    //   removal would silently no-op when the persisted name drifted
    //   from the live mapper name.
    //
    // Scenario: 3-disk pool, devid 3 missing. Membership pins three
    //   members with disk-number-mirroring UUIDs.
    #[test]
    fn cmd_remove_missing_resolves_devid_to_uuid_and_issues_no_luks_uuid_probes() {
        let f = PoolFixture::three_disk_devids_pinned();
        // Snapshot membership so we can verify which UUID got removed.
        let pre = membership::load_membership(&f.paths).unwrap();
        let (target_uuid, _target_member) = pre
            .by_devid(3)
            .unwrap()
            .expect("devid 3 must resolve in the fixture");
        let target_uuid = target_uuid.clone();

        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        cmd_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().build(),
        )
        .expect("remove-missing should succeed");

        // Membership: target UUID is gone; the other two remain.
        let saved = membership::load_membership(&f.paths).unwrap();
        assert!(
            saved.by_uuid(&target_uuid).is_none(),
            "target UUID must be removed by uuid"
        );
        assert_eq!(saved.len(), 2, "two surviving members expected");

        // Recording runner saw zero CryptsetupLuksUuid probes for the
        // missing target's by-id path. (The pool topology probes for
        // live devices' UUIDs still fire -- that's part of probe_pool's
        // contract, not the missing-target identity flow.)
        let calls = runner.requests();
        let probes_for_missing_byid = calls
            .iter()
            .filter(|c| {
                matches!(c, CmdRequest::CryptsetupLuksUuid { device }
                if device == "/dev/disk/by-id/virtio-disk3")
            })
            .count();
        assert_eq!(
            probes_for_missing_byid, 0,
            "remove-missing must NOT probe the missing target's by-id path"
        );
    }

    // Intent: when two members carry distinct devids, remove-missing
    //   removes ONLY the UUID whose persisted devid matches the btrfs
    //   missing devid; the other entry is byte-for-byte unchanged.
    //
    // Why: by-id and disk-name are operator-cosmetic decoys; only the
    //   persisted devid drives the missing-target member selection.
    //   A regression that fell back to name- or by-id-keyed lookup
    //   would pick the wrong member.
    //
    // Scenario: 3-disk pool, devid 3 missing. Membership has two
    //   "decoy" entries (`misleading-label` and `decoy`) with the
    //   true target name placed differently from its by-id basename.
    #[test]
    fn cmd_remove_missing_decoy_regression_selects_by_devid_only() {
        let f = PoolFixture::three_disk_devids_pinned();
        // Replace membership with a decoy-laced version: U_R holds the
        // member with persisted devid matching the missing one (3);
        // U_D holds a decoy with a different devid and a misleading
        // disk name.
        let u_r = test_uuid(450);
        let u_d = test_uuid(451);
        // The fixture's RemoveMissingPool flips the missing devid to 3,
        // so U_R must carry devid Some(3). U_D carries 99 -- a value
        // present in NEITHER live nor missing devids.
        let mut m = PoolMembership::empty();
        // Surviving disk1 + disk2 must be present with their fixture
        // UUIDs so probe_pool can correlate them to live devices.
        let disk1_uuid = LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap();
        m.insert(
            disk1_uuid,
            DiskMember {
                name: DiskName::parse("disk1").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
                devid: Some(1),
                added_at: None,
            },
        )
        .unwrap();
        let disk2_uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        m.insert(
            disk2_uuid,
            DiskMember {
                name: DiskName::parse("disk2").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
                devid: Some(2),
                added_at: None,
            },
        )
        .unwrap();
        // U_R: true missing target. Persisted name and by-id are operator
        // labels chosen to mislead; only persisted devid 3 selects it.
        m.insert(
            u_r.clone(),
            DiskMember {
                name: DiskName::parse("misleading-label").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/virtio-right").unwrap(),
                devid: Some(3),
                added_at: None,
            },
        )
        .unwrap();
        // U_D: decoy. Different devid (99). The by-id basename is the
        // operator-typed name on purpose -- a buggy by-id-keyed lookup
        // would pick this row.
        m.insert(
            u_d.clone(),
            DiskMember {
                name: DiskName::parse("decoy").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/virtio-misleading-label").unwrap(),
                devid: Some(99),
                added_at: None,
            },
        )
        .unwrap();
        membership::save_membership(&m, &f.paths).unwrap();

        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        cmd_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().missing_id(3).build(),
        )
        .expect("remove-missing should succeed");

        let saved = membership::load_membership(&f.paths).unwrap();
        assert!(
            saved.by_uuid(&u_r).is_none(),
            "U_R (devid==3) must be removed by uuid"
        );
        let decoy_after = saved
            .by_uuid(&u_d)
            .expect("U_D decoy must remain untouched");
        assert_eq!(
            decoy_after.devid,
            Some(99),
            "U_D's persisted devid must not be perturbed by the remove-missing"
        );
        let calls = runner.requests();
        let probes_for_missing_byid = calls
            .iter()
            .filter(|c| {
                matches!(c, CmdRequest::CryptsetupLuksUuid { device }
                if device == "/dev/disk/by-id/virtio-right"
                    || device == "/dev/disk/by-id/virtio-misleading-label")
            })
            .count();
        assert_eq!(
            probes_for_missing_byid, 0,
            "remove-missing must NOT probe the missing target's by-id path"
        );
    }

    // Intent: when membership has no member with the requested devid
    //   (enrichment never ran for any member, or the devid is foreign),
    //   `cmd_remove_missing` returns RemoveMissingError::NoMemberForDevid
    //   with the pinned `no member in membership has devid {devid}`
    //   substring, AND issues zero mutating requests of any shape.
    //
    // Why: this is the forward never-enriched refusal contract. A
    //   regression that fell through to remove an arbitrary entry
    //   would pass the positive-path and decoy tests but fail this one.
    //
    // Scenario: 3-disk pool with missing devid 3, but membership has
    //   every member WITHOUT enrichment (devid: None on all entries).
    #[test]
    fn cmd_remove_missing_never_enriched_refusal_returns_structured_error() {
        let f = PoolFixture::three_disk_devids_pinned();
        // Replace membership with one that has NO devid enrichment on
        // any member. Keep UUIDs aligned with the live fixture so the
        // only failure is the missing persisted-devid binding.
        let mut m = PoolMembership::empty();
        for (uuid, name) in [
            ("11111111-1111-1111-1111-111111111111", "disk1"),
            ("22222222-2222-2222-2222-222222222222", "disk2"),
            ("33333333-3333-3333-3333-333333333333", "disk3"),
        ] {
            m.insert(
                LuksUuid::parse(uuid).unwrap(),
                DiskMember {
                    name: DiskName::parse(name).unwrap(),
                    by_id: ByIdPath::parse(&format!("/dev/disk/by-id/virtio-{name}")).unwrap(),
                    devid: None,
                    added_at: None,
                },
            )
            .unwrap();
        }
        membership::save_membership(&m, &f.paths).unwrap();
        let pre_bytes = std::fs::read(f.paths.pool_json()).unwrap();

        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        let err = cmd_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().missing_id(3).build(),
        )
        .unwrap_err();
        match &err {
            RemoveMissingError::NoMemberForDevid { devid } => assert_eq!(*devid, 3),
            other => panic!("expected NoMemberForDevid, got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("no member in membership has devid 3"),
            "expected pinned NoMemberForDevid wording; got: {msg}"
        );
        // Membership file is byte-for-byte unchanged.
        let post_bytes = std::fs::read(f.paths.pool_json()).unwrap();
        assert_eq!(
            pre_bytes, post_bytes,
            "cmd_remove_missing must not perturb pool.json on never-enriched refusal"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "never-enriched refusal must happen before acquiring the sleep inhibitor"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "never-enriched refusal must happen before pending-op.json is written"
        );
        let calls = runner.requests();
        assert!(
            calls.iter().all(|c| !matches!(
                c,
                CmdRequest::BtrfsDeviceRemove { .. }
                    | CmdRequest::BtrfsBalanceRaid1Soft { .. }
                    | CmdRequest::CryptsetupClose { .. }
                    | CmdRequest::BtrfsDeviceScanForget { .. }
            )),
            "never-enriched refusal must issue zero mutating requests; calls: {calls:?}"
        );
    }

    // Intent: when membership has no member with the requested devid,
    //   dry-run remove-missing must surface the pinned
    //   RemoveMissingError::NoMemberForDevid refusal.
    //
    // Why it exists: pins the doc 022 dry-run contract for the exact
    //   UUID-identity migration bug where identity was resolved only
    //   in execute(), letting dry-run render a successful preview for
    //   inputs a real run refused.
    //
    // Scenario: 3-disk pool with missing devid 3; membership has every
    //   member but with `devid: None`. Dry-run must refuse with the
    //   pinned wording and emit zero mutating requests.
    #[test]
    fn cmd_remove_missing_never_enriched_refusal_in_dry_run() {
        let f = PoolFixture::three_disk_devids_pinned();
        let mut m = PoolMembership::empty();
        for (uuid, name) in [
            ("11111111-1111-1111-1111-111111111111", "disk1"),
            ("22222222-2222-2222-2222-222222222222", "disk2"),
            ("33333333-3333-3333-3333-333333333333", "disk3"),
        ] {
            m.insert(
                LuksUuid::parse(uuid).unwrap(),
                DiskMember {
                    name: DiskName::parse(name).unwrap(),
                    by_id: ByIdPath::parse(&format!("/dev/disk/by-id/virtio-{name}")).unwrap(),
                    devid: None,
                    added_at: None,
                },
            )
            .unwrap();
        }
        membership::save_membership(&m, &f.paths).unwrap();
        let pre_bytes = std::fs::read(f.paths.pool_json()).unwrap();

        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        let err = cmd_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params()
                .missing_id(3)
                .dry_run(true)
                .build(),
        )
        .unwrap_err();
        match &err {
            RemoveMissingError::NoMemberForDevid { devid } => assert_eq!(*devid, 3),
            other => panic!("expected NoMemberForDevid, got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("no member in membership has devid 3"),
            "expected pinned NoMemberForDevid wording; got: {msg}"
        );
        let post_bytes = std::fs::read(f.paths.pool_json()).unwrap();
        assert_eq!(
            pre_bytes, post_bytes,
            "cmd_remove_missing --dry-run must not perturb pool.json on never-enriched refusal"
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "dry-run never-enriched refusal must not acquire the sleep inhibitor"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "dry-run never-enriched refusal must not write pending-op.json"
        );
        let calls = runner.requests();
        assert!(
            calls.iter().all(|c| !matches!(
                c,
                CmdRequest::BtrfsDeviceRemove { .. }
                    | CmdRequest::BtrfsBalanceRaid1Soft { .. }
                    | CmdRequest::CryptsetupClose { .. }
                    | CmdRequest::BtrfsDeviceScanForget { .. }
            )),
            "dry-run never-enriched refusal must issue zero mutating requests; calls: {calls:?}"
        );
    }

    // Intent: when pool.json is mutated between planning and execution
    //   such that the target devid now resolves to a different UUID,
    //   execute() must abort before btrfs mutation, journal write, or
    //   membership save.
    //
    // Why it exists: the persisted devid is the authorized identity
    //   binding for missing devices. A UUID-existence-only recheck
    //   would let a stale plan remove the wrong member from pool.json.
    //
    // Scenario: 3-disk pool with missing devid 3 bound to disk3's UUID.
    //   Plan resolves disk3, then pool.json is rewritten so devid 3
    //   belongs to disk2. Execute must report membership drift and
    //   leave recovery state untouched.
    #[test]
    fn execute_aborts_when_devid_rebinds_between_plan_and_execute() {
        let f = PoolFixture::three_disk_devids_pinned();
        let (runner, _remove_done) =
            RemoveMissingPool::three_disk_one_missing().install(MockRunner::default());
        let plan = plan_remove_missing(
            &runner,
            &MockFs::storage(vec![]),
            &f.remove_missing_params().missing_id(3).build(),
        )
        .expect("planning should succeed for valid devid 3 -> disk3");

        let mut m = membership::load_membership(&f.paths).unwrap();
        let disk2_uuid = m
            .by_name(&DiskName::parse("disk2").unwrap())
            .expect("disk2 must exist")
            .0
            .clone();
        let disk3_uuid = m
            .by_name(&DiskName::parse("disk3").unwrap())
            .expect("disk3 must exist")
            .0
            .clone();
        m.by_uuid_mut(&disk2_uuid).unwrap().devid = Some(3);
        m.by_uuid_mut(&disk3_uuid).unwrap().devid = None;
        membership::save_membership(&m, &f.paths).unwrap();
        let drifted_bytes = std::fs::read(f.paths.pool_json()).unwrap();

        let err = plan
            .execute(
                &runner,
                &MockFs::storage(vec![]),
                &f.remove_missing_params().missing_id(3).build(),
            )
            .unwrap_err();
        match &err {
            RemoveMissingError::Validation(msg) => assert!(
                msg.contains("membership drift"),
                "expected drift wording; got: {msg}"
            ),
            other => panic!("expected Validation(drift), got: {other:?}"),
        }
        let calls = runner.requests();
        assert!(
            calls.iter().all(|c| !matches!(
                c,
                CmdRequest::BtrfsDeviceRemove { .. } | CmdRequest::BtrfsBalanceRaid1Soft { .. }
            )),
            "drift recheck must fire before any btrfs mutation; calls: {calls:?}"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "drift recheck must fire before pending-op.json is written"
        );
        let post_bytes = std::fs::read(f.paths.pool_json()).unwrap();
        assert_eq!(
            drifted_bytes, post_bytes,
            "drift recheck must fire before save_membership; pool.json \
             must remain byte-for-byte the drifted state"
        );
    }
}
