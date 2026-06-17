use crate::alert;
use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::Config;
use crate::confirm;
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal;
use crate::mapper_close::close_mapper_best_effort;
use crate::membership;
use crate::parse::{ParseError, parse_btrfs_device_usage, parse_btrfs_df_json};
use crate::pool::{
    DeviceIdentity, pool_balance_single, pool_remove_device, validate_pool_topology,
};
use crate::preflight;
use crate::preview::{self, PerDiskStyle, PlanFailure, Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{Filesystem, ProbeError, probe_pool};
use crate::probe_mapper_uuid::{
    MapperOwnership, probe_observed_mapper_uuid, warn_close_skipped_inactive,
};
use crate::progress::{self, ProgressOutput};
use crate::repair_hint;
use crate::state_paths::StatePaths;
use crate::status_tag::{StatusTag, color_enabled_for_stderr, emit_status, status_line};
use crate::types::*;
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
    #[error("{0}")]
    Validation(String),
    #[error(
        "pool was modified but membership persist failed: {0}\n\
         pool.json may be stale -- run `braid recover` to reconcile from live state."
    )]
    MembershipPersistFailure(String),
    #[error(
        "pool was modified and membership persisted, but journal clear failed: {0}\n\
         Recovery mode remains active until pending-op.json is cleared -- \
         run `braid recover`."
    )]
    JournalClearFailure(String),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("pool error: {0}")]
    Pool(#[from] crate::pool::PoolError),
}

/// Classify a `save_membership` failure that occurs *after* the irreversible
/// btrfs device-remove has returned. Callers pass this to `.map_err` on the
/// post-commit `save_membership` call; tests call it directly on a real
/// `MembershipError` so a classification regression inside the helper fails
/// the test.
fn map_membership_persist_failure(e: membership::MembershipError) -> RemoveError {
    RemoveError::MembershipPersistFailure(format!("failed to persist pool membership: {e}"))
}

/// Classify a `clear_journal` failure that occurs after the pool has been
/// modified and pool.json has already been rewritten. Same testing seam as
/// `map_membership_persist_failure` above.
fn map_journal_clear_failure(e: journal::JournalError) -> RemoveError {
    RemoveError::JournalClearFailure(e.to_string())
}

/// Shared pool.json drift error so the planner and executor reject the
/// same absent-name state before journaling can record a misleading remove.
fn absent_from_membership_error(name: &str) -> RemoveError {
    RemoveError::Validation(format!(
        "'{name}' not found in pool.json membership -- \
         no disk entry has this name. Pool membership may need manual repair."
    ))
}

/// Resolve a user-typed `--name` argument to its `(LuksUuid, DiskName)`
/// pair via `PoolMembership::by_name`. Used at the planning and execute
/// boundaries so identity decisions flow through `LuksUuid` and the
/// display name follows from the persisted member, not from raw CLI bytes.
fn resolve_target_in_membership(
    membership: &membership::PoolMembership,
    raw_name: &str,
) -> Result<(LuksUuid, DiskName), RemoveError> {
    let parsed = DiskName::parse(raw_name).map_err(|e| {
        RemoveError::Validation(format!("'{raw_name}' is not a valid disk name: {e}"))
    })?;
    let (uuid, member) = membership
        .by_name(&parsed)
        .ok_or_else(|| absent_from_membership_error(raw_name))?;
    Ok((uuid.clone(), member.name.clone()))
}

pub struct RemoveParams<'a> {
    pub config: &'a Config,
    pub name: &'a str,
    pub dry_run: bool,
    pub yes: bool,
    pub progress: ProgressOutput,
    pub paths: &'a StatePaths,
    /// Seam for acquiring a logind sleep inhibitor before the irreversible
    /// portion of the remove. Production passes `&RealSleepInhibitor`;
    /// unit tests pass `&RecordingInhibitor` to avoid spawning subprocesses.
    pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
    /// Seam for the operator go/no-go prompt. Production prints the
    /// assembled prompt and reads from the tty; tests record the prompt
    /// and provide a deterministic verdict.
    pub confirm: &'a dyn confirm::Confirm,
    /// Sleeper seam for retrying transiently-busy mapper closes without
    /// slowing unit tests.
    pub sleeper: &'a dyn progress::Sleeper,
}

/// Dry-run preview source of truth for `braid remove` plus the execute
/// inputs pre-computed during planning. `preview()` renders accumulated
/// notes plus steps from the semantic work plan; `execute()` consumes
/// the preflight state (target device, remaining/total counts, mount
/// point) and renders any accumulated notes to stderr via the shared
/// `preview::render_notes_for_stderr` helper (canonical `[warn] <body>`
/// wording) before mutating. The 1-disk `WARNING:` (no RAID1 redundancy)
/// is confirmation UI, not a `PreviewNote`: it stays behind the
/// `!params.yes` gate and never appears in `--dry-run` or `--yes` runs.
pub struct RemovePlan {
    pub notes: Vec<PreviewNote>,
    work_plan: RemoveWorkPlan,
}

#[derive(Debug, Clone)]
struct RemoveWorkPlan {
    /// Persisted disk name resolved from the user-typed `--name` via
    /// `PoolMembership::by_name`. Used for log/progress rendering and as
    /// the journaled `OpKind::Remove.name` field; identity decisions
    /// flow through `target_uuid` exclusively.
    name: DiskName,
    /// Persistent LUKS identity for the target, resolved once at the
    /// planning boundary and threaded through executor + journal.
    target_uuid: LuksUuid,
    target_devid: Devid,
    /// Observed mapper from `PoolDevice.mapper` at planning time. NEVER
    /// reconstructed via `mapper_name(&name)` -- the close-time
    /// `CryptsetupClose` consumes this byte-identically so operator
    /// drift between plan and post-commit close still targets the right
    /// dm slot. See plan section "remove.rs" for the parallel with
    /// lock.rs's "close observed, not reconstructed" doctrine.
    target_mapper: MapperName,
    target_underlying: String,
    remaining: usize,
    total: usize,
    mount_point: MountPoint,
    /// Identity snapshot of every present pool device at planning time,
    /// consumed by `validate_pool_topology` at execute time (pre- and
    /// post-journal). Full identity (mapper, devid, luks_uuid) -- not just
    /// cardinality or mapper name -- so a same-count survivor swap or a
    /// same-mapper replacement both fail the validation rather than
    /// slipping through with stale capacity-preflight assumptions.
    expected_present_identities: BTreeMap<MapperName, DeviceIdentity>,
}

impl RemoveWorkPlan {
    fn new(
        name: DiskName,
        target_uuid: LuksUuid,
        target: &PoolDevice,
        devices: &[PoolDevice],
        mount_point: MountPoint,
    ) -> Result<Self, RemoveError> {
        let total = devices.len();
        let remaining = total - 1;
        if remaining == 0 {
            return Err(RemoveError::Validation(
                "cannot remove the last disk from the pool".into(),
            ));
        }
        let expected_present_identities: BTreeMap<MapperName, DeviceIdentity> = devices
            .iter()
            .map(|d| {
                (
                    d.mapper.clone(),
                    DeviceIdentity {
                        devid: d.devid,
                        luks_uuid: d.luks_uuid.clone(),
                    },
                )
            })
            .collect();
        Ok(Self {
            name,
            target_uuid,
            target_devid: target.devid,
            target_mapper: target.mapper.clone(),
            target_underlying: target.underlying.clone(),
            remaining,
            total,
            mount_point,
            expected_present_identities,
        })
    }

    fn render_steps(&self) -> Vec<Step> {
        let mapper_path = self.target_mapper.dev_path();
        let mut steps = Vec::new();
        if self.remaining == 1 {
            steps.push(Step {
                risk: "long",
                description: "btrfs balance -dconvert=single -mconvert=dup -f (RAID1 -> single)"
                    .into(),
                commands: vec![CmdRequest::BtrfsBalanceSingle {
                    mount_point: self.mount_point.clone(),
                }],
            });
        }
        steps.push(Step {
            risk: "long",
            description: format!(
                "btrfs device remove {} (data migrates off disk)",
                self.target_mapper.dev_path()
            ),
            commands: vec![CmdRequest::BtrfsDeviceRemove {
                device: mapper_path,
                mount_point: self.mount_point.clone(),
            }],
        });
        steps.push(Step {
            risk: "safe",
            description: format!("cryptsetup close {}", self.target_mapper),
            commands: vec![CmdRequest::CryptsetupClose {
                mapper: self.target_mapper.clone(),
            }],
        });
        steps
    }
}

impl RemovePlan {
    /// Build a `Preview` carrying any plan-derived notes. The 1-disk
    /// `WARNING:` line stays in `execute()` behind the `!params.yes`
    /// gate and does not appear here.
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
        params: &RemoveParams<'_>,
    ) -> Result<(), RemoveError> {
        // Render accumulated notes to stderr via the shared helper
        // before any mutation. Warn notes emit as the canonical
        // `[warn] <body>` (same as dry-run stdout), so both modes
        // share one render contract for plan-derived notes.
        preview::emit_notes_to_stderr(&self.notes, PerDiskStyle::Bracketed);

        let RemovePlan {
            notes: _,
            work_plan,
        } = self;

        // Confirm
        if !params.yes {
            let hw = confirm::query_disk_hw_info(runner, &work_plan.target_underlying);
            let mut prompt = format!(
                "{}\n",
                format_remove_confirm(
                    &RemoveConfirmDisk {
                        name: work_plan.name.as_str(),
                        hw: Some(&hw),
                        devid: work_plan.target_devid,
                    },
                    work_plan.remaining,
                    work_plan.total,
                )
            );
            if work_plan.remaining == 1 {
                // Confirmation UI only: this is the go/no-go gate. The
                // dry-run preview already shows the consequence as the
                // `RAID1 -> single` balance step; see `preview()`.
                prompt.push_str("WARNING: Pool will have 1 disk -- no RAID1 redundancy.\n\n");
            }
            params
                .confirm
                .confirm(&prompt)
                .map_err(RemoveError::Validation)?;
        }

        // Hold a logind sleep inhibitor for the rest of the remove operation --
        // covers the optional pre-remove RAID1->single balance, the long-running
        // btrfs device remove (data migration), and the post-op LUKS close +
        // membership persist. Suspending mid-remove can leave the kernel-side
        // device-remove state machine in a partially-relocated state requiring
        // recovery.
        //
        // Acquired here, AFTER all interactive/reversible work (confirmation)
        // and BEFORE journal::write_journal, so that:
        //   - operator-idle prompts do not block suspend
        //   - a logind failure aborts cleanly without stranding pending-op.json
        //     and forcing the user into recovery mode for an environmental error.
        let _sleep_inhibitor_guard = params
            .sleep_inhibitor
            .acquire("removing disk from pool")
            .map_err(|e| {
                RemoveError::Validation(format!(
                    "could not acquire sleep inhibitor (is logind running?): {e}"
                ))
            })?;

        // (Pre-journal) topology drift validation -- clean failure if the
        // world changed between plan_remove and here. Above journal::write_journal
        // so failure does NOT strand pending-op.json (principle 3,
        // docs/design/principles.md#3-safe-by-construction-operations). Hot-unplug variant surfaces a journal-free
        // recovery sequence (re-plug, OR close + reopen the stale mapper via
        // lock/unlock or reboot, then re-run); `braid recover` is intentionally
        // NOT mentioned here because it would fail with no pending journal.
        validate_pool_topology(
            runner,
            fs,
            &work_plan.mount_point,
            work_plan.target_mapper.as_str(),
            &work_plan.expected_present_identities,
        )
        .map_err(|drift| {
            let detail = drift.detail();
            let suffix = if drift.is_target_hot_unplug() {
                "cryptsetup reports `device: (null)` (hot-unplug). \
                 The remove did not start. Resolve the hot-unplug by re-plugging \
                 the disk, OR by closing + reopening the stale mapper \
                 (`braid lock` then `braid unlock`, or reboot then `braid unlock`), \
                 then re-run `braid remove`."
            } else {
                "Resolve the drift and re-run `braid remove`."
            };
            RemoveError::Validation(format!("{detail}. {suffix}"))
        })?;

        // (Pre-journal) survivor-capacity re-check for the fail-closed 2->1 branch.
        // Capacity validated at plan time can go stale across the confirmation prompt +
        // inhibitor-acquire window while the pool keeps taking writes. Re-running it here
        // -- above journal::write_journal -- catches an over-committed survivor before the
        // irreversible `-f` balance and fails CLEAN (no stranded pending-op.json), because
        // no mutation has happened yet (principle 3,
        // docs/design/principles.md#3-safe-by-construction-operations). The >=2-survivor
        // branch is intentionally NOT re-checked: `btrfs device remove` ENOSPCs cleanly
        // there (see check_eviction_space docstring).
        if work_plan.remaining == 1 {
            check_single_survivor(runner, &work_plan.mount_point, work_plan.target_devid)?;
        }

        // Build target membership and write journal before irreversible disk op.
        let pre_membership = membership::load_membership(params.paths)
            .map_err(|e| RemoveError::Validation(format!("failed to load pool membership: {e}")))?;
        // (Confirm/inhibitor-window guard) This fresh load is the journal's
        // pre_membership below, so re-check the target still exists: a concurrent
        // pool.json rewrite during the confirmation prompt or inhibitor acquire
        // would otherwise let remove_by_uuid silently no-op and journal a
        // misleading "removed nothing." Pinned by
        // execute_rejects_when_pool_json_drifts_after_planning.
        if pre_membership.by_uuid(&work_plan.target_uuid).is_none() {
            return Err(absent_from_membership_error(work_plan.name.as_str()));
        }
        // Pin every live member's btrfs devid into the journal. Recovery is
        // allowed to use persisted devid as the fallback binding for
        // null-underlying or MISSING btrfs devices, but must not fall back to
        // mapper-name correlation when the LUKS UUID is no longer observable.
        let mut pre_membership = pre_membership;
        for identity in work_plan.expected_present_identities.values() {
            if let Some(member) = pre_membership.by_uuid_mut(&identity.luks_uuid) {
                member.devid = Some(identity.devid);
            }
        }
        let mut target_membership = pre_membership.clone();
        target_membership.remove_by_uuid(&work_plan.target_uuid);
        let journal = journal::build_journal(
            pre_membership,
            target_membership.clone(),
            journal::OpKind::Remove {
                luks_uuid: work_plan.target_uuid.clone(),
                name: work_plan.name.clone(),
            },
        );
        journal::write_journal(params.paths, &journal)
            .map_err(|e| RemoveError::Validation(e.to_string()))?;

        // (Post-journal) last-moment safety gate: catch drift in the small
        // window between the pre-journal probe and pool_balance_single.
        // BtrfsBalanceSingle ships -f, which skips btrfs-progs' missing-device
        // safety timeout (reference/btrfs-progs/cmds/balance.c:558-561).
        // Without this gate, a disk going MISSING here could subject the pool
        // to a dangerous profile conversion. Failure here keeps the journal
        // in place because we are below journal::write_journal and above
        // journal::clear_journal -- standard "preserved for recover" semantics.
        validate_pool_topology(
            runner,
            fs,
            &work_plan.mount_point,
            work_plan.target_mapper.as_str(),
            &work_plan.expected_present_identities,
        )
        .map_err(|drift| {
            let detail = drift.detail();
            let suffix = if drift.is_target_hot_unplug() {
                "cryptsetup reports `device: (null)` (hot-unplug). \
                 Run `braid recover` to reconcile pool.json. \
                 The broken mapper does not self-heal on replug; if \
                 `cryptsetup status` still reports `device: (null)` after \
                 recover, close + reopen the mappers (`braid lock` then \
                 `braid unlock`, or reboot then `braid unlock`) before \
                 retrying the remove."
            } else {
                "Run `braid recover` to reconcile."
            };
            RemoveError::Validation(format!("{detail}. {suffix}"))
        })?;

        // Execute. The trailing close is gated on the
        // `probe_observed_mapper_uuid` check below -- a defense-in-depth
        // re-probe of the journaled identity at the observed mapper, so
        // we don't tear down a foreign dm slot an operator opened under
        // the same mapper between plan and execute.
        let color_enabled = color_enabled_for_stderr();
        if work_plan.remaining == 1 {
            emit_status(&status_line(
                StatusTag::Wait,
                color_enabled,
                "pool: balancing RAID1 to single profile...",
            ));
            pool_balance_single(runner, &work_plan.mount_point, params.progress)?;
            emit_status(&status_line(
                StatusTag::Ok,
                color_enabled,
                "pool: balanced to single profile",
            ));
        }
        let device_path = work_plan.target_mapper.dev_path();
        emit_status(&status_line(
            StatusTag::Wait,
            color_enabled,
            &format!("pool: removing {}...", work_plan.name),
        ));
        pool_remove_device(
            runner,
            &device_path,
            &work_plan.mount_point,
            params.progress,
        )?;
        emit_status(&status_line(
            StatusTag::Ok,
            color_enabled,
            &format!("pool: {} removed", work_plan.name),
        ));

        // Defense-in-depth: probe the journaled identity at the
        // observed mapper before close. On mismatch or unverifiable
        // state, demote the close to a logged-warning skip so we don't
        // tear down a foreign dm slot that the operator opened under
        // the same mapper between plan and this point. Inactive is a
        // distinct caller-classified outcome.
        match probe_observed_mapper_uuid(runner, &work_plan.target_mapper, &work_plan.target_uuid) {
            MapperOwnership::Owned => {
                close_mapper_best_effort(
                    runner,
                    params.sleeper,
                    &work_plan.target_mapper,
                    &work_plan.name,
                    color_enabled,
                );
            }
            MapperOwnership::Inactive => {
                warn_close_skipped_inactive(&work_plan.target_mapper, &work_plan.target_uuid);
            }
            MapperOwnership::Unverified => {}
        }

        // Post-commit: write pool.json and clear journal.
        membership::save_membership(&target_membership, params.paths)
            .map_err(map_membership_persist_failure)?;
        journal::clear_journal(params.paths).map_err(map_journal_clear_failure)?;
        // Hygiene only -- failure is non-fatal because `cmd_add` is the
        // fail-closed correctness boundary for reused devids. See
        // docs/design/decisions/014-alerts.md "Acked-stats hygiene".
        if let Err(e) = alert::drop_ghost_acked_for_devids(params.paths, &[work_plan.target_devid])
        {
            eprintln!("Warning: failed to update acked stats: {e}");
        }

        eprintln!("Done. Disk '{}' removed from pool.", work_plan.name);
        Ok(())
    }
}

/// Plan a `braid remove` run after dispatch has already checked for a pending
/// operation and loaded config under the pool lock. Owns pool probe / mounted
/// validation, mutation preflight, UPS preflight, target device lookup,
/// missing-device guard, work-plan construction, and the eviction-space
/// preflight. On success, accumulated notes move into `plan.notes`; on
/// post-preflight failure, accumulated notes stay on `PlanFailure::notes` so
/// `cmd_remove` can render them before returning the error.
pub fn plan_remove<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RemoveParams<'_>,
) -> Result<RemovePlan, PlanFailure<RemoveError>> {
    // Notes accumulator. Pre-preflight exits have no notes; later exits
    // preserve preflight diagnostics on `PlanFailure::notes`.
    let mut notes: Vec<PreviewNote> = Vec::new();

    let config = params.config;

    let pool = match probe_pool(runner, fs, config.mount_point()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return Err(PlanFailure::empty(RemoveError::Validation(
                "pool is not mounted. Nothing to remove.".into(),
            )));
        }
        Err(e) => return Err(PlanFailure::empty(RemoveError::Probe(e))),
    };

    if !pool.mounted {
        return Err(PlanFailure::empty(RemoveError::Validation(
            "pool is not mounted. Nothing to remove.".into(),
        )));
    }

    // Preflight
    let fsid = pool.fsid.as_ref().expect("mounted pool must have FSID");
    match preflight::require_mutation_preflight(fs, fsid, config.mount_point()) {
        Ok(preflight_notes) => notes.extend(preflight_notes),
        Err(msg) => return Err(PlanFailure::empty(RemoveError::Validation(msg))),
    }
    if let Err(msg) =
        preflight::check_ups_not_on_battery(runner, config.ups().map(|u| u.name.as_str()), "remove")
    {
        return Err(PlanFailure::with_notes(notes, RemoveError::Validation(msg)));
    }

    // Resolve user-typed name to UUID against persisted membership FIRST.
    // pool.json is the identity source of truth; the live pool probe is
    // only used to locate the matching live device by UUID below. This
    // ordering implements the boundary contract from the plan's "Shared
    // Patterns": user name -> by_name -> UUID, then UUID -> live device.
    let pre_membership = match membership::load_membership(params.paths) {
        Ok(m) => m,
        Err(e) => {
            return Err(PlanFailure::with_notes(
                notes,
                RemoveError::Validation(format!("failed to load pool membership: {e}")),
            ));
        }
    };
    let (target_uuid, target_name) =
        match resolve_target_in_membership(&pre_membership, params.name) {
            Ok(p) => p,
            Err(e) => return Err(PlanFailure::with_notes(notes, e)),
        };

    // Is the disk present in the live pool under that UUID?
    let target = match pool.devices.iter().find(|d| d.luks_uuid == target_uuid) {
        Some(d) => d,
        None => {
            let mut msg = format!("disk '{}' not found in pool.", params.name);
            if pool.missing_count > 0 {
                let repair_command = repair_hint::missing_replace_command(None);
                msg.push_str(&format!(
                    " ({} missing device{} detected. \
                     To repair onto a new disk, use `{repair_command}`. \
                     To forget the entry, use `braid remove-missing`. \
                     Use `braid status` to see the missing disk's name and device IDs.)",
                    pool.missing_count,
                    if pool.missing_count == 1 { "" } else { "s" }
                ));
            }
            return Err(PlanFailure::with_notes(notes, RemoveError::Validation(msg)));
        }
    };

    if let Err(msg) =
        preflight::check_no_missing_devices(pool.missing_count, "remove a live disk from the pool")
    {
        return Err(PlanFailure::with_notes(notes, RemoveError::Validation(msg)));
    }

    // RemoveWorkPlan::new owns the remaining == 0 rejection (last-disk
    // gate). Run it first so that `check_eviction_space` is always reached
    // with `remaining >= 1`; the capacity helper does not need to handle
    // the 0-case. `target.mapper` is the observed mapper; the work plan
    // stores it as-is so post-commit close still targets the right dm
    // slot under benign mapper drift.
    let work_plan = match RemoveWorkPlan::new(
        target_name,
        target_uuid,
        target,
        &pool.devices,
        config.mount_point().clone(),
    ) {
        Ok(plan) => plan,
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, e));
        }
    };

    // Pre-flight: reject if the surviving devices lack space to absorb
    // data from the device being removed. Without this, btrfs will
    // either ENOSPC instantly or crash the filesystem to read-only
    // mid-relocation (see tests/repro/). The helper dispatches on
    // `remaining` -- the >=2-survivor path and the 1-survivor path use
    // different models and different error policies. A soft-warn on
    // the >=2 path becomes a `PreviewNote::Warn`; any hard failure
    // returns `Err`.
    match check_eviction_space(runner, config.mount_point(), target, work_plan.remaining) {
        Ok(EvictionCheck::Proceed) => {}
        Ok(EvictionCheck::ProceedWithWarning(body)) => {
            notes.push(PreviewNote::Warn(body));
        }
        Err(e) => {
            return Err(PlanFailure::with_notes(notes, e));
        }
    }

    let plan = RemovePlan { notes, work_plan };

    Ok(plan)
}

pub fn cmd_remove<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RemoveParams<'_>,
) -> Result<(), RemoveError> {
    let plan = match plan_remove(runner, fs, params) {
        Ok(p) => p,
        Err(PlanFailure { notes, error }) => {
            // Preserved-context failure: accumulated notes render to
            // stderr before the error via the SAME helper as the Ok
            // path (`RemovePlan::execute`), so preflight diagnostics
            // surface identically across success, failure, and dry-run
            // stdout.
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

/// Outcome of the eviction-space preflight's `remaining >= 2` branch.
/// `Proceed` means either the check ran and survivors have enough
/// space, or the check wasn't needed. `ProceedWithWarning(body)` means
/// the check itself failed (spawn error or non-`CommandFailed` parse
/// error) and the caller should surface the warning body to the user
/// but still proceed -- a bug in this best-effort safety net must not
/// block a valid operation, because `btrfs device remove` will ENOSPC
/// cleanly when there are `>= 2` survivors. A hard "survivors lack
/// space" outcome, or a `CommandFailed` parse (btrfs itself refused),
/// is a `RemoveError::Validation` instead.
///
/// Note: the `remaining == 1` branch (`check_single_survivor`) does
/// not use this enum -- it is fail-closed on every input uncertainty
/// and returns `Err` directly. Do **not** unify the two branches.
#[derive(Debug)]
pub(crate) enum EvictionCheck {
    Proceed,
    ProceedWithWarning(String),
}

/// Check that the surviving device(s) have enough space to absorb data from
/// the device being removed. If they don't, `btrfs device remove` will either
/// ENOSPC instantly or crash the filesystem to read-only mid-relocation.
///
/// Two branches with **different error policies**:
///
/// - `remaining >= 2`: RAID1-aware per-type check via
///   `check_raid1_relocation_space`. Input uncertainty (spawn errors,
///   non-`CommandFailed` parse errors) is *warn-and-proceed* -- the caller
///   receives `EvictionCheck::ProceedWithWarning(body)` and surfaces the
///   warning through the preview + execute paths. A best-effort preflight
///   miss here falls through to `btrfs device remove`, which ENOSPCs cleanly
///   without corrupting the filesystem. Only a `CommandFailed` parse error
///   (btrfs itself refused) is surfaced as a validation error.
///
/// - `remaining == 1`: single-survivor capacity check. Every input uncertainty
///   -- spawn error, parser-shape error, `CommandFailed`, or "survivor entry
///   missing from `btrfs device usage`" -- is a hard `RemoveError::Validation`.
///   The post-balance + post-remove state for a lone survivor has no safety
///   net: a missed capacity refusal lets `btrfs device remove` crash the fs
///   read-only mid-migration with `pending-op.json` already on disk. Any
///   uncertainty is fail-closed here by design. Do **not** unify the two
///   error policies -- the asymmetry is the point.
///
/// `remaining == 0` is not a valid input; `RemoveWorkPlan::new` has already
/// rejected the last-disk case upstream.
pub(crate) fn check_eviction_space<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    target: &PoolDevice,
    remaining: usize,
) -> Result<EvictionCheck, RemoveError> {
    if remaining == 1 {
        return check_single_survivor(runner, mount_point, target.devid)
            .map(|()| EvictionCheck::Proceed);
    }

    // remaining >= 2: existing warn-and-proceed policy. Instead of
    // printing directly, we surface the warning body to the caller so
    // the preview + execute paths own the rendering. See the
    // `EvictionCheck` docstring for the rationale.
    let raw = match runner.run(&CmdRequest::BtrfsDeviceUsageRaw {
        mount_point: mount_point.clone(),
    }) {
        Ok(r) => r,
        Err(e) => {
            return Ok(EvictionCheck::ProceedWithWarning(format!(
                "ENOSPC pre-flight check failed: {e}; proceeding anyway"
            )));
        }
    };

    let usage = match parse_btrfs_device_usage(&raw) {
        Ok(u) => u,
        Err(ParseError::CommandFailed {
            exit_code, stderr, ..
        }) => {
            return Err(RemoveError::Validation(format!(
                "btrfs device usage failed (exit {exit_code}): {stderr}"
            )));
        }
        Err(e) => {
            return Ok(EvictionCheck::ProceedWithWarning(format!(
                "ENOSPC pre-flight check failed: {e}; proceeding anyway"
            )));
        }
    };

    let target_devs: Vec<_> = usage
        .devices
        .iter()
        .filter(|d| d.devid == target.devid)
        .collect();
    let remaining_devs: Vec<_> = usage
        .devices
        .iter()
        .filter(|d| d.devid != target.devid)
        .collect();

    preflight::check_raid1_relocation_space(&target_devs, &remaining_devs)
        .map(|()| EvictionCheck::Proceed)
        .map_err(|e| {
            RemoveError::Validation(format!(
                "{e}\n\nFree up space by deleting files, or add a new device first with `braid add`."
            ))
        })
}

/// Shared single-survivor (2->1) capacity helper, invoked at both the
/// planning preflight (`check_eviction_space`) and the pre-journal execute
/// gate (`RemovePlan::execute`). Takes `target_devid: Devid` rather than a
/// live `PoolDevice` so the executor -- which holds only
/// `work_plan.target_devid`, not a probed device -- can re-run the exact same
/// check across the plan/execute gap. Fail-closed on every input uncertainty;
/// see `check_eviction_space` docstring for the rationale.
fn check_single_survivor<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    target_devid: Devid,
) -> Result<(), RemoveError> {
    let usage_raw = runner
        .run(&CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| {
            RemoveError::Validation(format!(
                "ENOSPC pre-flight (2->1): btrfs device usage spawn failed: {e}. \
                 Refusing to start remove without a validated survivor capacity."
            ))
        })?;
    let usage = parse_btrfs_device_usage(&usage_raw).map_err(|e| match e {
        ParseError::CommandFailed {
            exit_code, stderr, ..
        } => RemoveError::Validation(format!(
            "btrfs device usage failed (exit {exit_code}): {stderr}"
        )),
        other => RemoveError::Validation(format!(
            "ENOSPC pre-flight (2->1): btrfs device usage output unparseable: {other}. \
             Refusing to start remove without a validated survivor capacity."
        )),
    })?;

    let df_raw = runner
        .run(&CmdRequest::BtrfsFilesystemDfJson {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| {
            RemoveError::Validation(format!(
                "ENOSPC pre-flight (2->1): btrfs filesystem df spawn failed: {e}. \
                 Refusing to start remove without a validated survivor capacity."
            ))
        })?;
    let df = parse_btrfs_df_json(&df_raw).map_err(|e| match e {
        ParseError::CommandFailed {
            exit_code, stderr, ..
        } => RemoveError::Validation(format!(
            "btrfs filesystem df failed (exit {exit_code}): {stderr}"
        )),
        other => RemoveError::Validation(format!(
            "ENOSPC pre-flight (2->1): btrfs filesystem df output unparseable: {other}. \
             Refusing to start remove without a validated survivor capacity."
        )),
    })?;

    let survivor = usage
        .devices
        .iter()
        .find(|d| d.devid != target_devid)
        .ok_or_else(|| {
            RemoveError::Validation(format!(
                "ENOSPC pre-flight (2->1): btrfs device usage did not list the \
                 surviving device (target devid {target_devid}). Refusing to start remove \
                 without a validated survivor capacity."
            ))
        })?;

    preflight::check_single_survivor_capacity(&df, survivor).map_err(RemoveError::Validation)
}

// ---------------------------------------------------------------------------
// Confirmation formatter
// ---------------------------------------------------------------------------

struct RemoveConfirmDisk<'a> {
    name: &'a str,
    hw: Option<&'a confirm::DiskHwInfo>,
    devid: Devid,
}

fn format_remove_confirm(disk: &RemoveConfirmDisk, remaining: usize, total: usize) -> String {
    let mut msg = "Remove from pool:\n".to_string();
    let hw_line = disk.hw.and_then(confirm::format_hw_info_line);
    if let Some(hw) = &hw_line {
        msg.push_str(&format!("  {}  {}\n", disk.name, hw));
    } else {
        msg.push_str(&format!("  {}\n", disk.name));
    }
    let migrate_word = if remaining == 1 { "disk" } else { "disks" };
    msg.push_str(&format!(
        "  {:width$}devid {} | data will migrate to remaining {}\n",
        "",
        disk.devid,
        migrate_word,
        width = disk.name.len() + 2,
    ));
    msg.push_str(&format!(
        "\nPool: {} {} -> {} {}\n",
        total,
        if total == 1 { "disk" } else { "disks" },
        remaining,
        if remaining == 1 { "disk" } else { "disks" },
    ));
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, CmdRequest, MockRunner, RawCommandOutput};
    use crate::membership::PoolMembership;
    use crate::state_paths::StatePaths;
    use crate::test_fixtures::{
        DeviceUsageSpec, MockFs, PoolFixture, RemovalPool, btrfs_remove_path_error,
        canonical_luks_uuid, device_usage_raw_body, mock_ok, overcommitted_survivor_df_json,
        overcommitted_survivor_usage_stdout, target_device, valid_three_disk_df_json,
        valid_three_disk_usage_stdout, valid_two_disk_df_json, valid_two_disk_usage_stdout,
        with_lsblk_hw_info,
    };
    use std::collections::BTreeMap;

    fn acked_disk(missing_acked: bool, read_io_errs: u64) -> alert::AckedDisk {
        alert::AckedDisk {
            missing_acked,
            device_stats: alert::AckedDeviceCounters {
                read_io_errs,
                ..Default::default()
            },
        }
    }

    /// pool.json drift for the membership-drift rejection tests: the
    /// `three_disk_healthy` membership with `disk1` removed. Derived from the
    /// fixture's own saved pool.json, so the surviving disk2/disk3 keep the
    /// canonical LUKS UUIDs the live `RemovalPool` probe returns -- the drift
    /// cannot re-encode disk identity under a second UUID convention.
    fn three_disk_healthy_without_disk1(paths: &StatePaths) -> PoolMembership {
        let mut m = membership::load_membership(paths).expect("three_disk_healthy pool.json");
        let (uuid, _) = m
            .by_name(&DiskName::parse("disk1").expect("valid fixture name"))
            .expect("disk1 present in three_disk_healthy");
        let uuid = uuid.clone();
        m.remove_by_uuid(&uuid);
        m
    }

    // Intent: the drift fixture keeps its surviving disks under the SAME
    //   canonical LUKS UUIDs `three_disk_healthy` assigns -- it derives the
    //   drift from the saved pool, never re-encoding disk identity under a
    //   second UUID convention.
    // Why it exists: the drift-rejection tests remove `disk1`, so they stay
    //   green even when disk2/disk3 are keyed under the wrong UUIDs -- the
    //   incidental-pass bug this change fixes. This contract test fails closed
    //   on that regression: revert `three_disk_healthy_without_disk1` to a
    //   hand-built `test_uuid(2/3)` membership and this is the only test red.
    // Scenario: `three_disk_healthy` saves disk1+disk2+disk3; the drift drops
    //   disk1; disk2/disk3 must remain keyed by canonical_luks_uuid(2/3).
    #[test]
    fn drift_fixture_keeps_survivors_under_canonical_uuids() {
        let f = PoolFixture::three_disk_healthy();
        let drift = three_disk_healthy_without_disk1(&f.paths);

        assert_eq!(drift.len(), 2, "drift drops exactly disk1");
        assert!(
            drift.by_name(&DiskName::parse("disk1").unwrap()).is_none(),
            "disk1 must be absent from the drift",
        );
        for n in [2u64, 3] {
            let member = drift.by_uuid(&canonical_luks_uuid(n));
            assert!(
                member.is_some_and(|m| m.name.as_str() == format!("disk{n}")),
                "disk{n} must be keyed under canonical_luks_uuid({n}), as in three_disk_healthy",
            );
        }
    }

    #[test]
    // Intent: cmd_remove invokes the 2->1 survivor-capacity preflight before
    //   committing any mutation, and proceeds when the survivor has room.
    //
    // Why: A 2-disk RAID1 with a smaller survivor can fit the data in RAID1
    //   (min of the two) yet fail post-balance once metadata is doubled to
    //   DUP on one device. The fix calls check_single_survivor_capacity on
    //   every 2->1 remove so btrfs device remove cannot crash the fs to RO
    //   mid-migration. This test locks in both preflight calls
    //   (BtrfsDeviceUsageRaw + BtrfsFilesystemDfJson) run BEFORE the
    //   balance/device-remove steps, so a regression that reintroduces the
    //   old remaining == 1 skip fails here.
    //
    // Scenario: User removes one disk from a healthy 2-disk pool whose live
    //   data (50 MiB data + 10 MiB metadata) fits comfortably on the survivor.
    //   Preflight runs, reports pass, and the operation proceeds to balance
    //   + device remove. Pre-fix, the preflight calls would be absent.
    fn two_to_one_remove_invokes_survivor_capacity_preflight() {
        let f = PoolFixture::two_disk_healthy();
        let runner = RemovalPool::two_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]);
        cmd_remove(&runner, &fs, &f.remove_params().build()).expect("remove should succeed");

        let calls = runner.requests();
        let usage_idx = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsDeviceUsageRaw { .. }))
            .expect("2->1 preflight must call btrfs device usage; calls: {calls:?}");
        let df_idx = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsFilesystemDfJson { .. }))
            .expect("2->1 preflight must call btrfs filesystem df; calls: {calls:?}");
        let balance_idx = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsBalanceSingle { .. }))
            .expect("2->1 remove must balance; calls: {calls:?}");
        assert!(
            usage_idx < balance_idx && df_idx < balance_idx,
            "preflight calls must precede the RAID1->single balance; calls: {calls:?}"
        );
        // Locks in the seam placement: a successful 2->1 remove must take the
        // inhibitor exactly once before journal::write_journal.
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
        );
    }

    // Intent: a declined remove confirmation aborts before irreversible
    //   side effects.
    // Why it exists: the interactive gate must remain before the sleep
    //   inhibitor and journal write so a "no" cannot strand recovery state.
    // Scenario: an operator starts a 2->1 remove, sees the warning prompt,
    //   and declines before the disk remove begins.
    #[test]
    fn cmd_remove_declined_confirm_aborts_before_side_effects() {
        let f = PoolFixture::two_disk_healthy();
        f.confirm.decline();
        let runner = RemovalPool::two_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]);

        let err = cmd_remove(&runner, &fs, &f.remove_params().yes(false).build())
            .expect_err("declined confirm should abort");

        assert_eq!(err.to_string(), "aborted by user");
        assert_eq!(f.inhibitor.acquire_count(), 0);
        assert!(journal::load_journal(&f.paths).unwrap().is_none());
        let calls = runner.requests();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. })),
            "declined confirm must not issue BtrfsDeviceRemove: {calls:?}"
        );
    }

    /// The single-survivor warning sentence emitted on a 2->1 remove confirm
    /// (see `RemovePlan::execute`). Pinned here so the present/absent
    /// assertions in the two confirm tests below stay consistent. Sentence
    /// only (no trailing newlines) so the checks are robust to
    /// surrounding-whitespace changes while still pinning the wording; an
    /// independent copy of production's literal, so a production wording
    /// change still fails the test.
    const SINGLE_SURVIVOR_WARNING: &str = "WARNING: Pool will have 1 disk -- no RAID1 redundancy.";

    // Intent: a 2->1 remove confirm shows the single-survivor warning exactly
    //   once, on the named target's prompt with the correct pool transition.
    // Why it exists: this is the ONLY coverage of the warning prompt -- the VM
    //   suite's only redundancy-reducing remove runs `--yes`, which bypasses
    //   the prompt. Asserting behavior (warning present once, correct target,
    //   correct transition) instead of byte-exact assembly keeps the test
    //   pinned to the contract, not to cosmetic prompt layout, while the
    //   literal still catches wording regressions.
    // Scenario: removing disk2 from a two-disk pool leaves one disk, so the
    //   operator sees the normal remove prompt and the no-RAID1 warning.
    #[test]
    fn cmd_remove_accepted_confirm_records_prompt_with_warning() {
        let f = PoolFixture::two_disk_healthy();
        f.confirm.accept();
        let runner = RemovalPool::two_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]);

        cmd_remove(&runner, &fs, &f.remove_params().yes(false).build())
            .expect("accepted confirm should proceed");

        let prompts = f.confirm.prompts();
        assert_eq!(
            prompts.len(),
            1,
            "confirm must be invoked exactly once: {prompts:?}"
        );
        let prompt = &prompts[0];
        assert_eq!(
            prompt.matches(SINGLE_SURVIVOR_WARNING).count(),
            1,
            "single-survivor warning must appear exactly once: {prompt:?}"
        );
        assert!(
            prompt.contains("disk2"),
            "prompt must name the target disk: {prompt:?}"
        );
        assert!(
            prompt.contains("devid 2"),
            "prompt must name the target devid: {prompt:?}"
        );
        assert!(
            prompt.contains("2 disks -> 1 disk"),
            "prompt must show the 2->1 pool transition: {prompt:?}"
        );
    }

    // Intent: a 2->1 remove confirm resolves its hw line from the present
    //   target's LIVE backing path (/dev/vdc for disk2), never a persisted
    //   by-id handle, the mapper path, or an empty string.
    // Why it exists: decision 024 ("Present-device probes use live paths")
    //   governs which device `query_disk_hw_info` is handed, but nothing pinned
    //   the routing through execute() -- the only hw-line tests were pure
    //   formatter tests, and the other execute-level confirm tests run against
    //   runners with no LsblkField handler, so `get_lsblk_field`'s `.ok()?`
    //   swallow of `MissingMock` silently blanks the line regardless of which
    //   device was queried. Registering hw ONLY on /dev/vdc makes the model and
    //   serial appear iff the probe hit the live backing path; a regression to
    //   the mapper path or a by-id handle leaves the line blank and fails.
    // Scenario: removing disk2 from a two-disk pool, the operator's confirm
    //   prompt shows disk2's real model and serial -- queried from /dev/vdc.
    #[test]
    fn cmd_remove_confirm_hw_line_resolves_from_live_backing_path() {
        const MODEL: &str = "Toshiba MN07ACA12T";
        const SERIAL: &str = "REMOVE2SERIAL";
        const SIZE: u64 = 12_000_138_625_024;

        let f = PoolFixture::two_disk_healthy();
        f.confirm.accept();
        let runner = with_lsblk_hw_info(
            RemovalPool::two_disk().install(MockRunner::default()),
            "/dev/vdc",
            MODEL,
            SERIAL,
            SIZE,
        );
        let fs = MockFs::storage(vec![]);

        cmd_remove(&runner, &fs, &f.remove_params().yes(false).build())
            .expect("accepted confirm should proceed");

        let prompts = f.confirm.prompts();
        assert_eq!(
            prompts.len(),
            1,
            "confirm must be invoked exactly once: {prompts:?}"
        );
        let prompt = &prompts[0];
        assert!(
            prompt.contains(MODEL),
            "hw line must show the model probed from /dev/vdc: {prompt:?}"
        );
        assert!(
            prompt.contains(&format!("serial {SERIAL}")),
            "hw line must show the serial probed from /dev/vdc: {prompt:?}"
        );
    }

    // Intent: a redundancy-preserving remove (3->2) shows the normal confirm
    //   prompt WITHOUT the single-survivor warning.
    // Why it exists: the warning is gated on `remaining == 1`; the negative
    //   side of that gate was untested, so a regression that always (or never)
    //   appended the warning would pass the 2->1 test alone.
    // Scenario: removing disk2 from a three-disk pool leaves two disks, so the
    //   operator sees the remove prompt but no no-RAID1 warning.
    #[test]
    fn cmd_remove_3to2_confirm_omits_redundancy_warning() {
        let f = PoolFixture::three_disk_healthy();
        f.confirm.accept();
        let runner = RemovalPool::three_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]);

        cmd_remove(&runner, &fs, &f.remove_params().yes(false).build())
            .expect("accepted confirm should proceed");

        let prompts = f.confirm.prompts();
        assert_eq!(
            prompts.len(),
            1,
            "confirm must be invoked exactly once: {prompts:?}"
        );
        let prompt = &prompts[0];
        // Positive: it is the real 3->2 remove prompt for the named target...
        assert!(
            prompt.contains("disk2"),
            "prompt must name the target disk: {prompt:?}"
        );
        assert!(
            prompt.contains("devid 2"),
            "prompt must name the target devid: {prompt:?}"
        );
        assert!(
            prompt.contains("3 disks -> 2 disks"),
            "prompt must show the 3->2 pool transition: {prompt:?}"
        );
        // ...negative: but no single-survivor warning, because two disks remain.
        assert!(
            !prompt.contains(SINGLE_SURVIVOR_WARNING),
            "3->2 remove must not show the no-RAID1 warning: {prompt:?}"
        );
    }

    // Intent: accepted remove confirmation does not block the mutation.
    // Why it exists: the seam must preserve the happy path, not just the
    //   declined abort path.
    // Scenario: the operator accepts the 2->1 remove prompt and braid issues
    //   the btrfs device remove command.
    #[test]
    fn cmd_remove_accepted_confirm_proceeds_to_device_remove() {
        let f = PoolFixture::two_disk_healthy();
        f.confirm.accept();
        let runner = RemovalPool::two_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]);

        cmd_remove(&runner, &fs, &f.remove_params().yes(false).build())
            .expect("accepted confirm should proceed");

        let calls = runner.requests();
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. })),
            "accepted confirm must reach BtrfsDeviceRemove: {calls:?}"
        );
    }

    /*
     * Intent: a successful command-level `braid remove` prunes the acked-stats
     * entry for the removed target devid while preserving unrelated ack state.
     *
     * Why it exists: the cleanup callsite lives after the irreversible remove,
     * membership save, and journal clear. Helper-level tests cannot catch a
     * future edit that removes or moves that command wiring.
     *
     * Scenario: a healthy two-disk pool removes disk2 (devid 2). An old ghost
     * ack for devid 2 must disappear; the surviving disk1 ack must stay
     * byte-equivalent at the value layer.
     */
    #[test]
    fn cmd_remove_prunes_acked_stats_for_removed_devid() {
        let f = PoolFixture::two_disk_healthy();
        let control = acked_disk(false, 11);
        let target = acked_disk(true, 22);
        let mut acked = BTreeMap::new();
        acked.insert("1".to_owned(), control.clone());
        acked.insert("2".to_owned(), target);
        alert::save_acked_stats(&alert::AckedStats(acked), &f.paths).unwrap();

        let runner = RemovalPool::two_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]);
        cmd_remove(&runner, &fs, &f.remove_params().build()).expect("remove should succeed");

        let reloaded = alert::load_acked_stats(&f.paths);
        assert_eq!(
            reloaded.0.get("1"),
            Some(&control),
            "unrelated acked entry must be preserved"
        );
        assert!(
            !reloaded.0.contains_key("2"),
            "removed target devid must be pruned"
        );
    }

    // Intent
    // `cmd_remove` writes every live member's btrfs devid into the journal's
    // pre_membership before mutating the pool.
    //
    // Why it exists
    // Recovery uses the journaled devid as its only fallback binding when
    // btrfs later reports a null-underlying or MISSING device without an
    // observable LUKS UUID. pool.json entries written from by-id discovery may
    // not carry devids yet.
    //
    // Scenario
    // Starting from a healthy two-disk pool.json with no devids, device remove
    // fails after journal write, leaving the journal inspectable.
    #[test]
    fn remove_journal_pre_membership_carries_live_member_devids() {
        let f = PoolFixture::two_disk_healthy();
        let runner = RemovalPool::two_disk()
            .install(MockRunner::default())
            .with_handler(|req| match req {
                CmdRequest::BtrfsDeviceRemove { .. } => Some(Ok(RawCommandOutput {
                    cmd: "btrfs device remove".into(),
                    stdout: String::new(),
                    stderr: btrfs_remove_path_error(
                        "/dev/mapper/braid-disk2",
                        "No space left on device",
                    ),
                    exit_status: 1,
                })),
                _ => None,
            });
        let fs = MockFs::storage(vec![]);

        let result = cmd_remove(&runner, &fs, &f.remove_params().build());

        assert!(result.is_err(), "remove should fail after journal write");
        let journal = journal::load_journal(&f.paths)
            .unwrap()
            .expect("failed device remove should preserve pending journal");
        let disk1_uuid = LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap();
        let disk1 = journal
            .pre_membership
            .by_uuid(&disk1_uuid)
            .expect("pre_membership must still carry disk1's UUID");
        assert_eq!(
            disk1.devid,
            Some(Devid::new(1)),
            "journaled pre_membership must pin disk1's live devid"
        );

        let disk2_uuid = LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap();
        let disk2 = journal
            .pre_membership
            .by_uuid(&disk2_uuid)
            .expect("pre_membership must still carry disk2's UUID");
        assert_eq!(
            disk2.devid,
            Some(Devid::new(2)),
            "journaled pre_membership must pin disk2's live devid"
        );
        assert!(
            journal.target_membership.by_uuid(&disk2_uuid).is_none(),
            "target membership should still remove disk2"
        );
    }

    // Intent: plan_remove must reject pool.json drift before the dry-run
    // gate, so --dry-run never prints a successful plan that the real run
    // would later refuse.
    //
    // Why it exists: 022-dry-run-preview-model.md puts state loading and
    // preflight in plan_*(). Without a planner-visible membership check,
    // dry-run drifts from real-run on the same input.
    //
    // Scenario: live btrfs reports disk1+disk2+disk3, but pool.json only
    // contains disk2+disk3. `braid remove --dry-run disk1` must fail with
    // no inhibitor acquired, no journal written, and no pool.json rewrite.
    #[test]
    fn cmd_remove_dry_run_rejects_when_target_absent_from_pool_json() {
        let f = PoolFixture::three_disk_healthy();
        let drifted = three_disk_healthy_without_disk1(&f.paths);
        membership::save_membership(&drifted, &f.paths).unwrap();

        let runner = RemovalPool::three_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]);
        let result = cmd_remove(
            &runner,
            &fs,
            &f.remove_params()
                .name("disk1")
                .dry_run(true)
                .yes(true)
                .build(),
        );

        match result {
            Err(RemoveError::Validation(msg)) => {
                assert!(
                    msg.contains("not found in pool.json membership"),
                    "expected pool.json membership error: {msg}"
                );
                assert!(msg.contains("disk1"), "expected disk1 in error: {msg}");
            }
            Err(other) => panic!("expected Validation error, got: {other:?}"),
            Ok(()) => panic!("expected dry-run drift rejection"),
        }
        assert!(
            !f.paths.pending_op_json().exists(),
            "dry-run drift rejection must not write pending-op.json",
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "planner rejection must not acquire the sleep inhibitor",
        );
        assert_eq!(
            membership::load_membership(&f.paths).unwrap(),
            drifted,
            "rejection must leave the drifted pool.json unchanged",
        );
    }

    // Intent: RemovePlan::execute must reject pool.json drift introduced
    // between plan_remove and execute, before journal::build_journal, even
    // after the inhibitor has been acquired.
    //
    // Why it exists: a concurrent pool.json rewrite in the confirmation or
    // inhibitor window would otherwise let target_membership.disks.remove
    // silently no-op and write a misleading journal.
    //
    // Scenario: plan_remove sees disk1+disk2+disk3 in pool.json and live
    // btrfs, then pool.json is rewritten to disk2+disk3 before execute.
    // execute must fail with no journal and no pool.json rewrite.
    #[test]
    fn execute_rejects_when_pool_json_drifts_after_planning() {
        let f = PoolFixture::three_disk_healthy();
        let runner = RemovalPool::three_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]);
        let params = f.remove_params().name("disk1").yes(true).build();

        let plan = plan_remove(&runner, &fs, &params)
            .expect("initial plan must succeed before pool.json drift");
        let drifted = three_disk_healthy_without_disk1(&f.paths);
        membership::save_membership(&drifted, &f.paths).unwrap();

        let result = plan.execute(&runner, &fs, &params);

        match result {
            Err(RemoveError::Validation(msg)) => {
                assert!(
                    msg.contains("not found in pool.json membership"),
                    "expected pool.json membership error: {msg}"
                );
                assert!(msg.contains("disk1"), "expected disk1 in error: {msg}");
            }
            Err(other) => panic!("expected Validation error, got: {other:?}"),
            Ok(()) => panic!("expected execute-time drift rejection"),
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

    // Intent: RemovePlan::execute re-runs the 2->1 single-survivor capacity
    // check before journaling, so a survivor that had room at plan time but
    // was over-committed by execute time is refused cleanly.
    //
    // Why it exists: the plan-time check (check_eviction_space) is the only
    // capacity gate today; the confirmation prompt + inhibitor-acquire window
    // lets the pool keep taking writes, so a survivor can drift over capacity
    // between plan and the irreversible `-f` balance. Without a pre-journal
    // re-check, the balance crashes the fs read-only mid-migration with
    // pending-op.json already on disk -- the exact failure
    // tests/repro/remove-2to1-undersized-survivor.py guards at the plan
    // boundary, here guarded at the execute seam (no in-process write
    // injection is possible, so this is the faithful structure-insensitive
    // guard, mirroring execute_rejects_when_pool_json_drifts_after_planning).
    //
    // Scenario: an operator plans a 2->1 remove against a healthy survivor,
    // then a backup job fills the survivor while they sit at the warning
    // prompt. execute must refuse before writing the journal -- no
    // pending-op.json, inhibitor acquired then released (post-inhibitor gate).
    #[test]
    fn execute_rechecks_survivor_capacity_before_journal() {
        let f = PoolFixture::two_disk_healthy();
        let fs = MockFs::storage(vec![]);
        let params = f.remove_params().build();

        let healthy = RemovalPool::two_disk().install(MockRunner::default());
        let plan =
            plan_remove(&healthy, &fs, &params).expect("plan succeeds with healthy survivor");

        // Over-committed runner: healthy probe topology (so validate_pool_topology
        // passes) but the survivor usage + df report it over capacity.
        let overcommitted = RemovalPool::two_disk()
            .install(MockRunner::default())
            .with_handler(|req| match req {
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(mock_ok(
                    "btrfs device usage --raw /mnt/storage",
                    &overcommitted_survivor_usage_stdout(),
                ))),
                CmdRequest::BtrfsFilesystemDfJson { .. } => Some(Ok(mock_ok(
                    "btrfs --format json filesystem df /mnt/storage",
                    overcommitted_survivor_df_json(),
                ))),
                _ => None,
            });

        let result = plan.execute(&overcommitted, &fs, &params);

        match result {
            Err(RemoveError::Validation(msg)) => {
                assert!(
                    msg.contains("not enough space on surviving device"),
                    "expected survivor-capacity refusal: {msg}"
                );
            }
            Err(other) => panic!("expected Validation error, got: {other:?}"),
            Ok(()) => panic!("expected execute-time survivor-capacity rejection"),
        }
        assert!(
            !f.paths.pending_op_json().exists(),
            "pre-journal capacity rejection must not write pending-op.json",
        );
        assert_eq!(
            f.inhibitor.acquire_count(),
            1,
            "post-inhibitor gate must acquire the sleep inhibitor exactly once",
        );
        let calls = overcommitted.requests();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsBalanceSingle { .. })),
            "capacity rejection must abort before the RAID1->single balance: {calls:?}"
        );
    }

    // Intent: a redundancy-preserving remove (3->2, remaining >= 2) issues no
    // execute-time survivor-capacity probe -- the >= 2 path is intentionally
    // not re-checked, unlike the fail-closed 2->1 path.
    //
    // Why it exists: the execute-time capacity re-check is gated on
    // remaining == 1 and is fail-closed by design; the >= 2 branch is
    // warn-and-proceed and leans on `btrfs device remove` ENOSPCing cleanly.
    // The positive half is pinned by execute_rechecks_survivor_capacity_before_journal;
    // this pins the negative half. Without it, a consistency refactor could add
    // a hard re-check to the >= 2 execute path and refuse valid removes on
    // transient probe errors. If placed after journal::write_journal, it would
    // also strand pending-op.json, with no test to catch it.
    //
    // Scenario: an operator removes one disk from a healthy three-disk pool.
    // Two survivors remain, so execute proceeds from the pre-journal topology
    // gate straight to `btrfs device remove` with no survivor-capacity probe;
    // the journal is written and then cleared on success, nothing stranded.
    #[test]
    fn execute_skips_survivor_capacity_recheck_for_multi_survivor() {
        let f = PoolFixture::three_disk_healthy();
        let fs = MockFs::storage(vec![]);
        let params = f.remove_params().build();

        let plan_runner = RemovalPool::three_disk().install(MockRunner::default());
        let plan =
            plan_remove(&plan_runner, &fs, &params).expect("plan succeeds on healthy 3-disk pool");

        // Use a separate fresh runner so requests() captures only execute-phase
        // requests; the plan-time >= 2 capacity probe stays on plan_runner.
        let exec_runner = RemovalPool::three_disk().install(MockRunner::default());
        plan.execute(&exec_runner, &fs, &params)
            .expect("3->2 execute succeeds on a healthy pool");

        let calls = exec_runner.requests();
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                CmdRequest::BtrfsDeviceUsageRaw { .. } | CmdRequest::BtrfsFilesystemDfJson { .. }
            )),
            "the remaining >= 2 execute path must not re-probe survivor capacity \
             (it relies on btrfs device remove ENOSPCing cleanly): {calls:?}"
        );
        assert!(
            calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. })),
            "the 3->2 remove must actually reach btrfs device remove: {calls:?}"
        );
        assert!(
            !f.paths.pending_op_json().exists(),
            "a successful 3->2 remove must clear the journal -- nothing stranded",
        );
    }

    #[test]
    // Intent:
    // - What behavior this test (tries to) verify.
    //   - `braid remove` converts RAID1 to single before removing a device when only one disk remains.
    //
    // Why it exists:
    // - What risk/regression this protects against.
    //   - Prevents command-order regressions that make `btrfs device remove` fail under RAID1 minimum-device constraints.
    //
    // Scenario:
    // - Real-world situation this models (user/system story). Especially the
    //   specific scenario that inspired this test (like a real world bug).
    //   - Operator removes one disk from a healthy two-disk pool and expects the operation to succeed end-to-end.
    fn remove_two_disk_pool_balances_single_before_device_remove() {
        let f = PoolFixture::two_disk_healthy();
        let runner = RemovalPool::two_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]);
        cmd_remove(&runner, &fs, &f.remove_params().build()).expect("remove should succeed");

        let calls = runner.requests();
        let balance_idx = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsBalanceSingle { .. }))
            .expect("expected balance-to-single request");
        let remove_idx = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. }))
            .expect("expected device-remove request");

        assert!(
            balance_idx < remove_idx,
            "expected balance-to-single before device-remove; calls: {calls:?}"
        );
    }

    #[test]
    // Intent: pending-op.json survives when eviction fails after journal write.
    //
    // Why it exists: JournalGuard previously cleared the journal on any exit,
    //   including error returns. This left pool.json potentially stale with no
    //   recovery path after a failed btrfs device remove.
    //
    // Scenario: 2-disk pool, btrfs device remove fails mid-eviction. The journal
    //   must persist so `braid recover` can reconcile pool.json from live state.
    fn journal_survives_evict_failure() {
        let f = PoolFixture::two_disk_healthy();
        let runner = RemovalPool::two_disk()
            .install(MockRunner::default())
            .with_handler(|req| match req {
                CmdRequest::BtrfsDeviceRemove { .. } => Some(Ok(RawCommandOutput {
                    cmd: "btrfs device remove".into(),
                    stdout: String::new(),
                    stderr: btrfs_remove_path_error(
                        "/dev/mapper/braid-disk2",
                        "No space left on device",
                    ),
                    exit_status: 1,
                })),
                _ => None,
            });
        let fs = MockFs::storage(vec![]);
        let result = cmd_remove(&runner, &fs, &f.remove_params().build());

        assert!(result.is_err(), "remove should fail when eviction fails");
        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        let calls = runner.requests();
        assert!(
            matches!(calls.last(), Some(CmdRequest::BtrfsDeviceRemove { .. })),
            "request sequence must stop at failed device-remove; calls: {calls:?}",
        );
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::CryptsetupClose { .. })),
            "cryptsetup close must not run after failed device-remove; calls: {calls:?}",
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
    // Intent: the 2->1 dry-run preview flows through
    //   `plan_remove(...).preview().render()`, pins the exact balance /
    //   device-remove / close command strings it renders, and keeps the
    //   confirmation-only 1-disk `WARNING:` line out of `--dry-run` stdout.
    //
    // Why it exists: `braid remove --dry-run` routes through
    //   `RemovePlan::preview()` instead of `Step::print_dry_run(&steps)`.
    //   A regression that surfaces the confirmation-only `WARNING:` line as
    //   a `PreviewNote` would change the bytes an operator sees. The exact
    //   argv of `BtrfsBalanceSingle` / `BtrfsDeviceRemove` / `CryptsetupClose`
    //   -- including the `shell_words` quoting of the balance args -- is
    //   pinned only here on the production path; no `cmd.rs` test covers it.
    //
    // Scenario: operator previews removing disk2 from a healthy 2-disk
    //   pool. The preview must show the RAID1 -> single balance step, while
    //   keeping the go/no-go redundancy warning exclusive to confirmation.
    fn plan_remove_2to1_preview_omits_confirmation_only_redundancy_warning() {
        let f = PoolFixture::two_disk_healthy();
        let runner = RemovalPool::two_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]);
        let params = f.remove_params().dry_run(true).build();
        let plan =
            plan_remove(&runner, &fs, &params).expect("plan_remove should succeed on 2->1 fixture");

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
            "plan.preview().render() must be byte-equivalent to Step::render_dry_run(&plan.preview().steps) for the 2->1 path",
        );

        assert!(
            rendered.contains("RAID1 -> single"),
            "2->1 preview must still show the redundancy-loss balance step, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("WARNING:"),
            "2->1 dry-run preview must not leak confirmation-only WARNING lines, got:\n{rendered}",
        );

        // Exact command strings on the production preview path: the 2->1
        // case is the only place the balance argv (with its shell_words
        // quoting) plus the device-remove and close argv are pinned
        // end-to-end. Subsumes the deleted
        // `dry_run_render_2disk_removal_includes_balance`.
        let lines: Vec<&str> = rendered.lines().collect();
        assert_eq!(
            lines.len(),
            6,
            "2->1 preview must render balance + device-remove + close (6 lines), got:\n{rendered}",
        );
        assert_eq!(
            lines[1],
            "$ btrfs balance start --enqueue '-dconvert=single' '-mconvert=dup' -f /mnt/storage",
        );
        assert_eq!(
            lines[3],
            "$ btrfs device remove --enqueue /dev/mapper/braid-disk2 /mnt/storage",
        );
        assert_eq!(lines[5], "$ cryptsetup close braid-disk2");
    }

    #[test]
    // Intent: a clean 3->2 dry-run preview through `plan_remove(...)` emits
    //   exactly the device-remove + close steps -- no RAID1 -> single balance
    //   and no preflight notes.
    //
    // Why it exists: the balance step is conditional on `remaining == 1`. A
    //   regression that emitted it on a 3->2 removal (still redundant after
    //   eviction) would scare operators with a needless single-profile
    //   conversion. Pairs with the 2->1 test that pins the balance present;
    //   together they cover both arms of `render_steps`' `remaining == 1`
    //   branch through the production planner. Subsumes the deleted
    //   `dry_run_render_3disk_removal`.
    //
    // Scenario: operator previews removing disk2 from a healthy 3-disk pool;
    //   two survivors keep RAID1, so no balance is needed.
    fn plan_remove_3to2_preview_omits_balance_step() {
        let f = PoolFixture::three_disk_healthy();
        let runner = RemovalPool::three_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]);
        let params = f.remove_params().dry_run(true).build(); // removes disk2
        let plan = plan_remove(&runner, &fs, &params).expect("clean 3->2 plan");

        assert!(
            plan.notes.is_empty(),
            "healthy 3->2 preflight must produce zero notes; got {:?}",
            plan.notes,
        );

        let preview = plan.preview();
        assert_eq!(
            preview.steps.len(),
            2,
            "3->2 remove must emit device-remove + close only; got {:?}",
            preview.steps,
        );
        let rendered = preview.render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(
            !rendered.contains("RAID1 -> single"),
            "3->2 preview must NOT render the balance-to-single step, got:\n{rendered}",
        );
        assert_eq!(
            lines[1],
            "$ btrfs device remove --enqueue /dev/mapper/braid-disk2 /mnt/storage",
        );
        assert_eq!(lines[3], "$ cryptsetup close braid-disk2");
    }

    #[test]
    // Intent: the dry-run preview renders the target's OBSERVED mapper, never
    //   one reconstructed from the persisted disk name -- the "close observed,
    //   NEVER reconstructed via `mapper_name(&name)`" doctrine
    //   (`RemoveWorkPlan.target_mapper`) carried all the way into
    //   `render_steps`.
    //
    // Why it exists: every other production remove test has observed mapper ==
    //   name-derived mapper (RemovalPool hardcodes `braid-disk{n}`), so none of
    //   them can catch a `render_steps` that rebuilds the mapper from the name.
    //   The execute-time `drifted_member_remove_closes_observed_mapper` pins
    //   `target_mapper` and the executor's close request under drift, but never
    //   reaches the preview-only `render_steps`. This is the sole test proving
    //   the PREVIEW honors the observed mapper.
    //
    // Scenario: pool.json names the target "disk1" -> uuid1; the live pool
    //   observes that uuid under a drifted mapper "braid-renamed" (devid 1).
    //   The previewed device-remove and close must target braid-renamed, with
    //   no trace of the name-derived braid-disk1.
    fn plan_remove_renders_observed_mapper_under_drift() {
        let f = PoolFixture::two_disk_healthy(); // pool.json: disk1->uuid1, disk2->uuid2
        let runner = RemovalPool::two_disk()
            .install(MockRunner::default())
            .with_handler(|req| match req {
                // Rename only devid 1's observed mapper: braid-disk1 -> braid-renamed.
                CmdRequest::BtrfsFilesystemShow { .. } => Some(Ok(mock_ok(
                    "btrfs filesystem show",
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
                     \tTotal devices 2 FS bytes used 16.17MiB\n\
                     \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-renamed\n\
                     \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                ))),
                // The renamed mapper resolves to /dev/vdb; the default
                // RemovalPool handler already maps /dev/vdb -> uuid1 via
                // luks_uuid_for_device, so CryptsetupLuksUuid needs no override.
                CmdRequest::CryptsetupStatus { mapper } if mapper.as_str() == "braid-renamed" => {
                    Some(Ok(mock_ok(
                        "cryptsetup status braid-renamed",
                        "braid-renamed is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n",
                    )))
                }
                _ => None, // everything else falls through to RemovalPool::install
            });
        let fs = MockFs::storage(vec![]);
        let params = f.remove_params().name("disk1").dry_run(true).build();
        let plan = plan_remove(&runner, &fs, &params).expect("drift must not block planning");
        let rendered = plan.preview().render();

        assert!(
            rendered
                .contains("$ btrfs device remove --enqueue /dev/mapper/braid-renamed /mnt/storage"),
            "device-remove must target the observed mapper, got:\n{rendered}",
        );
        assert!(
            rendered.contains("$ cryptsetup close braid-renamed"),
            "close must target the observed mapper, got:\n{rendered}",
        );
        assert!(
            !rendered.contains("braid-disk1"),
            "preview must never reconstruct the name-derived mapper braid-disk1, got:\n{rendered}",
        );
    }

    #[test]
    // Intent: `braid remove` fails fast when a balance is paused.
    // Why: a paused balance holds the exclusive lock and never clears on its own.
    //   --enqueue would hang forever waiting for it.
    // Scenario: operator paused a balance and forgot, then runs `braid remove`.
    fn remove_fails_fast_on_paused_balance() {
        let f = PoolFixture::two_disk_healthy();
        let runner = RemovalPool::two_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]).with_excl_op("balance paused\n");
        let err = cmd_remove(&runner, &fs, &f.remove_params().name("disk1").build())
            .expect_err("should fail -- balance is paused");
        let msg = err.to_string();
        assert!(msg.contains("paused"), "expected 'paused' in error: {msg}");
        // Preflight failure must NOT acquire the inhibitor -- the failure is
        // reversible and the user should not be stranded in a state where
        // logind unavailability and a paused balance both have to clear before
        // the same braid command can run.
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "preflight failure (paused balance) must NOT acquire the sleep inhibitor"
        );
    }

    #[test]
    // Intent: `braid remove` warns but proceeds when an active op is running.
    // Why: --enqueue on the btrfs command will block until the slot frees;
    //   braid prints a wait message so the user knows what's happening.
    // Scenario: a device remove is already in progress, operator runs `braid remove`.
    //   The preflight detects the active op, prints a warning, and proceeds.
    fn remove_warns_and_proceeds_on_active_op() {
        let f = PoolFixture::three_disk_healthy();
        let runner = RemovalPool::three_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]).with_excl_op("balance\n");
        // With an active balance, cmd_remove should NOT error on the preflight --
        // it prints a warning and proceeds.
        let result = cmd_remove(&runner, &fs, &f.remove_params().dry_run(true).build());
        // dry_run should succeed (no actual btrfs commands executed)
        assert!(
            result.is_ok(),
            "expected dry_run to proceed past active-op preflight, got: {result:?}"
        );
        // dry-run must NOT acquire the inhibitor -- it has no irreversible work
        // to protect.
        assert_eq!(
            f.inhibitor.acquire_count(),
            0,
            "dry-run must NOT acquire the sleep inhibitor"
        );
    }

    // --- Confirmation formatter tests ---

    #[test]
    fn remove_confirm_normal() {
        let hw = confirm::DiskHwInfo {
            model: Some("Toshiba MN07ACA12T".into()),
            serial: Some("1234ABCD".into()),
            size: Some(12_000_138_625_024),
        };
        let msg = format_remove_confirm(
            &RemoveConfirmDisk {
                name: "toshiba",
                hw: Some(&hw),
                devid: Devid::new(2),
            },
            2,
            3,
        );
        assert!(msg.contains("Remove from pool:"));
        assert!(msg.contains("toshiba"));
        assert!(msg.contains("Toshiba MN07ACA12T"));
        assert!(msg.contains("serial 1234ABCD"));
        assert!(msg.contains("devid 2"));
        assert!(msg.contains("remaining disks"));
        assert!(msg.contains("3 disks -> 2 disks"));
    }

    #[test]
    fn remove_confirm_degraded() {
        let hw = confirm::DiskHwInfo {
            model: Some("Toshiba MN07ACA12T".into()),
            serial: None,
            size: Some(12_000_138_625_024),
        };
        let msg = format_remove_confirm(
            &RemoveConfirmDisk {
                name: "toshiba",
                hw: Some(&hw),
                devid: Devid::new(2),
            },
            1,
            2,
        );
        assert!(
            msg.contains("remaining disk"),
            "singular 'disk' when 1 remaining"
        );
        assert!(msg.contains("2 disks -> 1 disk"));
    }

    #[test]
    fn remove_confirm_no_hw_info() {
        let msg = format_remove_confirm(
            &RemoveConfirmDisk {
                name: "toshiba",
                hw: None,
                devid: Devid::new(2),
            },
            2,
            3,
        );
        assert!(msg.contains("toshiba"));
        assert!(msg.contains("devid 2"));
        assert!(!msg.contains("| |"), "no double separators when hw missing");
    }

    #[test]
    // Intent: the real post-commit mapping function classifies a
    //   save_membership failure as MembershipPersistFailure with remediation
    //   text that names pool.json as the stale artifact.
    //
    // Why: previously wrapped as RemoveError::Validation, which reads like a
    //   pre-flight rejection. A regression inside map_membership_persist_failure
    //   that returns the wrong variant or wrong remediation text fails this
    //   test -- the production post-commit save_membership call passes this
    //   same helper to .map_err, so the test binds to the real mapping.
    //
    // Scenario: `braid remove` succeeds at the btrfs layer, but the atomic
    //   write of pool.json fails (disk full in /var/lib/braid, stale NFS
    //   mount, etc.). Forced here by writing to a path whose parent
    //   directory does not exist.
    fn save_membership_failure_classified_as_membership_persist() {
        let tmp = tempfile::tempdir().unwrap();
        // Force the atomic write to fail: place a regular file where
        // `save_membership_to` expects a directory component. `create_dir_all`
        // in atomic_write will then error with NotADirectory.
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let bad_path = blocker.join("pool.json");
        let m = PoolMembership::empty();
        let underlying = membership::save_membership_to(&m, &bad_path)
            .expect_err("write under a non-directory path component must fail");
        let classified = map_membership_persist_failure(underlying);
        assert!(
            matches!(classified, RemoveError::MembershipPersistFailure(_)),
            "variant mismatch: {classified:?}"
        );
        let display = classified.to_string();
        assert!(display.contains("pool was modified"), "got: {display}");
        assert!(display.contains("pool.json may be stale"), "got: {display}");
        assert!(display.contains("braid recover"), "got: {display}");
    }

    #[test]
    // Intent: the real post-commit mapping function classifies a
    //   clear_journal failure as JournalClearFailure with remediation text
    //   that names recovery mode / pending-op.json as the latched artifact.
    //
    // Why: this is the only post-commit mode where pool.json is already
    //   correct and the *journal* is keeping the system in recovery mode. A
    //   regression that reused the membership message would tell the user to
    //   reconcile pool.json when pool.json is fine.
    //
    // Scenario: `braid remove` succeeds, pool.json is rewritten, but
    //   clear_journal fails (forced here by making pending-op.json a
    //   non-empty directory so fs::remove_file errors).
    fn clear_journal_failure_classified_as_journal_clear() {
        use crate::journal;
        let tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let pending = paths.pending_op_json();
        std::fs::create_dir_all(&pending).unwrap();
        std::fs::write(pending.join("child"), b"x").unwrap();
        let underlying = journal::clear_journal(&paths)
            .expect_err("remove_file on a non-empty directory must fail");
        let classified = map_journal_clear_failure(underlying);
        assert!(
            matches!(classified, RemoveError::JournalClearFailure(_)),
            "variant mismatch: {classified:?}"
        );
        let display = classified.to_string();
        assert!(
            display.contains("pool was modified and membership persisted"),
            "got: {display}"
        );
        assert!(display.contains("journal clear failed"), "got: {display}");
        assert!(
            display.contains("Recovery mode remains active"),
            "got: {display}"
        );
        assert!(display.contains("pending-op.json"), "got: {display}");
        assert!(display.contains("braid recover"), "got: {display}");
    }

    #[test]
    // Intent: check_eviction_space surfaces a non-zero btrfs exit as a hard
    //   validation error instead of swallowing it into warn-and-proceed.
    // Why: btrfs exiting non-zero during pre-flight is a real "cannot read the
    //   filesystem" signal. If the preflight tool itself has failed, a 3->2
    //   remove must not proceed into the irreversible btrfs device-remove
    //   step.
    // Scenario: 3->2 remove on a filesystem that returns EIO (or similar) to
    //   `btrfs device usage --raw`. Before this fix the warning was printed
    //   and remove proceeded; after the fix, remove stops at validation.
    fn check_eviction_space_surfaces_command_failed_as_validation() {
        let runner = MockRunner::default().with_handler(|req| match req {
            CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(RawCommandOutput {
                cmd: "btrfs device usage --raw /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: not a btrfs filesystem: /mnt/storage".into(),
                exit_status: 1,
            })),
            _ => None,
        });
        let mount = MountPoint::new("/mnt/storage".to_owned());
        let target = target_device("disk1");
        // remaining: 2 exercises the >= 2 branch (3->2 remove), which is the
        // scenario the CommandFailed surfacing was written for.
        let err = check_eviction_space(&runner, &mount, &target, 2)
            .expect_err("non-zero btrfs exit must surface as validation error");
        match err {
            RemoveError::Validation(msg) => {
                assert!(msg.contains("btrfs device usage failed"), "got: {msg}");
                assert!(msg.contains("exit 1"), "got: {msg}");
                assert!(msg.contains("not a btrfs filesystem"), "got: {msg}");
            }
            other => panic!("expected RemoveError::Validation, got {other:?}"),
        }
    }

    #[test]
    // Intent: the 2->1 branch fails closed when `btrfs device usage --raw`
    //   cannot be spawned.
    // Why: a runner/spawn failure means survivor capacity is unknown. The
    //   single-survivor path must not fall through to the irreversible remove.
    // Scenario: 2->1 remove where the preflight cannot invoke `btrfs device
    //   usage --raw` at all.
    fn check_eviction_space_2to1_fails_closed_on_device_usage_spawn_error() {
        let runner = MockRunner::default();
        let mount = MountPoint::new("/mnt/storage".to_owned());
        let target = target_device("disk1");
        let err = check_eviction_space(&runner, &mount, &target, 1)
            .expect_err("2->1 preflight must fail closed on usage spawn error");
        match err {
            RemoveError::Validation(msg) => {
                assert!(msg.contains("ENOSPC pre-flight (2->1)"), "got: {msg}");
                assert!(
                    msg.contains("btrfs device usage spawn failed"),
                    "got: {msg}"
                );
                assert!(msg.contains("validated survivor capacity"), "got: {msg}");
            }
            other => panic!("expected RemoveError::Validation, got {other:?}"),
        }
    }

    #[test]
    // Intent: the 2->1 branch fails closed when `btrfs device usage --raw`
    //   returns malformed output.
    // Why: parser-shape uncertainty on the single-survivor path must not
    //   degrade into warn-and-proceed.
    // Scenario: 2->1 remove where `btrfs device usage --raw` exits 0 but the
    //   output cannot be parsed.
    fn check_eviction_space_2to1_fails_closed_on_device_usage_parse_error() {
        let runner = MockRunner::default().with_handler(|req| match req {
            CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(mock_ok(
                "btrfs device usage --raw /mnt/storage",
                "/dev/mapper/braid-disk1, ID: 1\n\
                 \x20  Device size:         1073741824\n",
            ))),
            _ => None,
        });
        let mount = MountPoint::new("/mnt/storage".to_owned());
        let target = target_device("disk1");
        let err = check_eviction_space(&runner, &mount, &target, 1)
            .expect_err("2->1 preflight must fail closed on usage parse error");
        match err {
            RemoveError::Validation(msg) => {
                assert!(msg.contains("ENOSPC pre-flight (2->1)"), "got: {msg}");
                assert!(
                    msg.contains("btrfs device usage output unparseable"),
                    "got: {msg}"
                );
            }
            other => panic!("expected RemoveError::Validation, got {other:?}"),
        }
    }

    #[test]
    // Intent: the 2->1 branch fails closed when `btrfs filesystem df` cannot
    //   be spawned.
    // Why: without logical used bytes, the single-survivor capacity model
    //   cannot run, so remove must stop.
    // Scenario: valid device-usage output is available, but the df command
    //   itself cannot be invoked.
    fn check_eviction_space_2to1_fails_closed_on_df_spawn_error() {
        let runner = MockRunner::default().with_handler(|req| match req {
            CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(mock_ok(
                "btrfs device usage --raw /mnt/storage",
                &valid_two_disk_usage_stdout(),
            ))),
            _ => None,
        });
        let mount = MountPoint::new("/mnt/storage".to_owned());
        let target = target_device("disk1");
        let err = check_eviction_space(&runner, &mount, &target, 1)
            .expect_err("2->1 preflight must fail closed on df spawn error");
        match err {
            RemoveError::Validation(msg) => {
                assert!(msg.contains("ENOSPC pre-flight (2->1)"), "got: {msg}");
                assert!(
                    msg.contains("btrfs filesystem df spawn failed"),
                    "got: {msg}"
                );
                assert!(msg.contains("validated survivor capacity"), "got: {msg}");
            }
            other => panic!("expected RemoveError::Validation, got {other:?}"),
        }
    }

    #[test]
    // Intent: the 2->1 branch fails closed when `btrfs filesystem df`
    //   returns malformed JSON.
    // Why: parser-shape uncertainty on df output is part of the
    //   single-survivor risk surface and must be rejected.
    // Scenario: valid device-usage output, but df exits 0 with malformed JSON.
    fn check_eviction_space_2to1_fails_closed_on_df_parse_error() {
        let runner = MockRunner::default().with_handler(|req| match req {
            CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(mock_ok(
                "btrfs device usage --raw /mnt/storage",
                &valid_two_disk_usage_stdout(),
            ))),
            CmdRequest::BtrfsFilesystemDfJson { .. } => Some(Ok(mock_ok(
                "btrfs --format json filesystem df /mnt/storage",
                "{\"filesystem-df\":",
            ))),
            _ => None,
        });
        let mount = MountPoint::new("/mnt/storage".to_owned());
        let target = target_device("disk1");
        let err = check_eviction_space(&runner, &mount, &target, 1)
            .expect_err("2->1 preflight must fail closed on df parse error");
        match err {
            RemoveError::Validation(msg) => {
                assert!(msg.contains("ENOSPC pre-flight (2->1)"), "got: {msg}");
                assert!(
                    msg.contains("btrfs filesystem df output unparseable"),
                    "got: {msg}"
                );
            }
            other => panic!("expected RemoveError::Validation, got {other:?}"),
        }
    }

    #[test]
    // Intent: the 2->1 branch fails closed when `btrfs device usage` does not
    //   include a surviving device entry.
    // Why: survivor resolution is load-bearing for the capacity check. Missing
    //   survivor data must refuse, not proceed.
    // Scenario: valid df output is available, but usage output only lists the
    //   target device.
    fn check_eviction_space_2to1_fails_closed_when_survivor_missing() {
        let runner = MockRunner::default().with_handler(|req| match req {
            CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(mock_ok(
                "btrfs device usage --raw /mnt/storage",
                &device_usage_raw_body(&[DeviceUsageSpec::live(
                    "/dev/mapper/braid-disk1",
                    1,
                    1_073_741_824,
                    &[
                        ("Data", "RAID1", 52_428_800),
                        ("Metadata", "RAID1", 10_485_760),
                        ("System", "RAID1", 32_768),
                    ],
                    1_010_794_496,
                )]),
            ))),
            CmdRequest::BtrfsFilesystemDfJson { .. } => Some(Ok(mock_ok(
                "btrfs --format json filesystem df /mnt/storage",
                valid_two_disk_df_json(),
            ))),
            _ => None,
        });
        let mount = MountPoint::new("/mnt/storage".to_owned());
        let target = target_device("disk1");
        let err = check_eviction_space(&runner, &mount, &target, 1)
            .expect_err("2->1 preflight must fail closed when survivor is missing");
        match err {
            RemoveError::Validation(msg) => {
                assert!(
                    msg.contains("did not list the surviving device"),
                    "got: {msg}"
                );
                assert!(msg.contains("target devid"), "got: {msg}");
            }
            other => panic!("expected RemoveError::Validation, got {other:?}"),
        }
    }

    #[test]
    // Intent: the 2->1 branch surfaces a non-zero `btrfs filesystem df`
    //   exit as a validation error.
    // Why: this is the df-side equivalent of the existing CommandFailed test;
    //   if btrfs itself refuses, the remove must stop.
    // Scenario: valid usage output is available, but `btrfs filesystem df`
    //   exits non-zero.
    fn check_eviction_space_2to1_surfaces_df_command_failed_as_validation() {
        let runner = MockRunner::default().with_handler(|req| match req {
            CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(mock_ok(
                "btrfs device usage --raw /mnt/storage",
                &valid_two_disk_usage_stdout(),
            ))),
            CmdRequest::BtrfsFilesystemDfJson { .. } => Some(Ok(RawCommandOutput {
                cmd: "btrfs --format json filesystem df /mnt/storage".into(),
                stdout: String::new(),
                stderr: "ERROR: filesystem is read-only".into(),
                exit_status: 1,
            })),
            _ => None,
        });
        let mount = MountPoint::new("/mnt/storage".to_owned());
        let target = target_device("disk1");
        let err = check_eviction_space(&runner, &mount, &target, 1)
            .expect_err("2->1 preflight must surface df command failure");
        match err {
            RemoveError::Validation(msg) => {
                assert!(msg.contains("btrfs filesystem df failed"), "got: {msg}");
                assert!(msg.contains("exit 1"), "got: {msg}");
                assert!(msg.contains("filesystem is read-only"), "got: {msg}");
            }
            other => panic!("expected RemoveError::Validation, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // Planner-level tests for the soft-warn branch on `remaining >= 2`.
    //
    // These exercise the new `EvictionCheck::ProceedWithWarning` outcome
    // and its surfacing as a `PreviewNote::Warn` on `RemovePlan.notes`.
    // ---------------------------------------------------------------

    /// Canonical stdout used to trigger the parse-shape soft-warn branch.
    /// A single device header with no `Device size` / `Device slack` /
    /// `Unallocated` lines triggers `ParseError::MissingField` when
    /// the parser finalizes the partial device.
    const PARSE_SHAPE_ERROR_STDOUT: &str = "/dev/mapper/braid-disk1, ID: 1\n";

    /* Intent: `check_eviction_space` returns `ProceedWithWarning(body)`
     * on the `remaining >= 2` path when the underlying `btrfs device
     * usage --raw` command cannot be spawned.
     * Why it exists: PR 4 of the Preview migration removes the direct
     * `eprintln!` from the helper. The soft-warn must now surface as
     * a body-only string the planner wraps in a `PreviewNote::Warn`.
     * A regression that falls through to `Proceed` (swallowing the
     * warning) or to `Err` (hard-rejecting a recoverable case) fails
     * this test.
     * Scenario: 3->2 remove on a host where the runner cannot invoke
     * btrfs; the helper returns `ProceedWithWarning` with the
     * canonical body `ENOSPC pre-flight check failed: ...;
     * proceeding anyway` (no `warning:` prefix).
     */
    #[test]
    fn check_eviction_space_ge2_soft_warns_on_usage_spawn_error() {
        let runner = MockRunner::default();
        let mount = MountPoint::new("/mnt/storage".to_owned());
        let target = target_device("disk1");
        let outcome = check_eviction_space(&runner, &mount, &target, 2)
            .expect("soft-warn branch must not return Err");
        match outcome {
            EvictionCheck::ProceedWithWarning(body) => {
                assert!(
                    body.starts_with("ENOSPC pre-flight check failed:"),
                    "body must lead with canonical prefix; got: {body}",
                );
                assert!(
                    body.ends_with("; proceeding anyway"),
                    "body must end with canonical suffix; got: {body}",
                );
                assert!(
                    !body.starts_with("warning:"),
                    "body must NOT carry the legacy `warning:` prefix; got: {body}",
                );
            }
            other => panic!("expected ProceedWithWarning, got {other:?}"),
        }
    }

    /* Intent: `check_eviction_space` returns `ProceedWithWarning(body)`
     * on the `remaining >= 2` path when `btrfs device usage --raw`
     * exits 0 with unparseable output.
     * Why it exists: this is the non-`CommandFailed` parse-error
     * branch of the helper. The existing
     * `check_eviction_space_surfaces_command_failed_as_validation`
     * test pins the hard-reject case (exit non-zero = `CommandFailed`
     * -> `Err`); this new test pins the symmetric soft-warn case
     * (exit 0 but shape wrong) so PR 4's refactor preserves the
     * asymmetry between parse-error variants.
     * Scenario: 3->2 remove where btrfs printed something unexpected
     * but returned 0; the helper returns `ProceedWithWarning` with
     * the canonical body.
     */
    #[test]
    fn check_eviction_space_ge2_soft_warns_on_parse_shape_error() {
        let runner = MockRunner::default().with_handler(|req| match req {
            CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(mock_ok(
                "btrfs device usage --raw /mnt/storage",
                PARSE_SHAPE_ERROR_STDOUT,
            ))),
            _ => None,
        });
        let mount = MountPoint::new("/mnt/storage".to_owned());
        let target = target_device("disk1");
        let outcome = check_eviction_space(&runner, &mount, &target, 2)
            .expect("soft-warn branch must not return Err");
        match outcome {
            EvictionCheck::ProceedWithWarning(body) => {
                assert!(
                    body.starts_with("ENOSPC pre-flight check failed:"),
                    "body must lead with canonical prefix; got: {body}",
                );
                assert!(
                    body.ends_with("; proceeding anyway"),
                    "body must end with canonical suffix; got: {body}",
                );
            }
            other => panic!("expected ProceedWithWarning, got {other:?}"),
        }
    }

    /* Intent: `plan_remove` returns `Ok(RemovePlan)` carrying exactly
     * one `PreviewNote::Warn` and the expected steps when the
     * `remaining >= 2` eviction preflight hits a spawn-error
     * soft-warn.
     * Why it exists: this is the planner-level contract for the
     * soft-warn path. A regression that drops the warning on the
     * floor (no note) or hard-rejects (Err) or misroutes the body
     * (wrong note variant) fails this test. It complements the
     * unit-level `EvictionCheck` tests by also exercising the wiring
     * inside `plan_remove` that converts the outcome into a note.
     * Scenario: 3-disk RAID1 pool; removing disk3 would normally
     * succeed, but btrfs can't be spawned for the preflight check.
     */
    #[test]
    fn plan_remove_surfaces_soft_warn_as_preview_note_on_spawn_error() {
        let f = PoolFixture::three_disk_healthy();
        let runner = RemovalPool::three_disk()
            .install(MockRunner::default())
            .with_handler(|req| match req {
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Err(CmdError::MissingMock)),
                _ => None,
            });
        let fs = MockFs::storage(vec![]);
        let params = f.remove_params().name("disk3").dry_run(true).build();

        let plan =
            plan_remove(&runner, &fs, &params).expect("soft-warn case must produce an Ok plan");
        assert_eq!(
            plan.notes.len(),
            1,
            "soft-warn must produce exactly one note; got {:?}",
            plan.notes,
        );
        match &plan.notes[0] {
            PreviewNote::Warn(body) => {
                assert!(
                    body.starts_with("ENOSPC pre-flight check failed:"),
                    "body must lead with canonical prefix; got: {body}",
                );
                assert!(
                    !body.starts_with("warning:"),
                    "body must NOT carry the legacy `warning:` prefix; got: {body}",
                );
            }
            other => panic!("expected PreviewNote::Warn, got {other:?}"),
        }
        // Steps for a 3->2 remove are: device remove + cryptsetup close.
        // The planner still emits the full step list even when the
        // preflight soft-warned.
        let steps = plan.preview().steps;
        assert_eq!(
            steps.len(),
            2,
            "3->2 remove plan must emit 2 steps; got {:?}",
            steps,
        );
    }

    /* Intent: `plan_remove` returns `Ok(RemovePlan)` carrying exactly
     * one `PreviewNote::Warn` when the `remaining >= 2` eviction
     * preflight hits a non-`CommandFailed` parse error.
     * Why it exists: symmetric guardrail to
     * `plan_remove_surfaces_soft_warn_as_preview_note_on_spawn_error`
     * for the parse-shape branch of `check_eviction_space`. Without
     * this test, a regression that specifically breaks the parse
     * branch (e.g. swallowing the body) would be missed.
     * Scenario: 3-disk RAID1 pool; removing disk3 where `btrfs
     * device usage --raw` returns exit 0 with unparseable stdout.
     */
    #[test]
    fn plan_remove_surfaces_soft_warn_as_preview_note_on_parse_error() {
        let f = PoolFixture::three_disk_healthy();
        let runner = RemovalPool::three_disk()
            .install(MockRunner::default())
            .with_handler(|req| match req {
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(mock_ok(
                    "btrfs device usage --raw /mnt/storage",
                    PARSE_SHAPE_ERROR_STDOUT,
                ))),
                _ => None,
            });
        let fs = MockFs::storage(vec![]);
        let params = f.remove_params().name("disk3").dry_run(true).build();

        let report = plan_remove(&runner, &fs, &params);
        let plan = report.expect("soft-warn case must produce an Ok plan");
        assert_eq!(
            plan.notes.len(),
            1,
            "soft-warn must produce exactly one note; got {:?}",
            plan.notes,
        );
        match &plan.notes[0] {
            PreviewNote::Warn(body) => {
                assert!(
                    body.starts_with("ENOSPC pre-flight check failed:"),
                    "body must lead with canonical prefix; got: {body}",
                );
            }
            other => panic!("expected PreviewNote::Warn, got {other:?}"),
        }
        let steps = plan.preview().steps;
        assert_eq!(
            steps.len(),
            2,
            "3->2 remove plan must emit 2 steps; got {:?}",
            steps,
        );
    }

    /* Intent: `plan.preview().render()` places the soft-warn note
     * above the dry-run step block using the canonical `[warn]
     * <body>` form.
     * Why it exists: this is the preview boundary test -- it pins the
     * full rendered string so a regression in `Preview::render`'s
     * notes-before-steps contract, or in the body wording, fails
     * here. Unit tests on `check_eviction_space` cannot catch
     * rendering bugs; this test can.
     * Scenario: same 3-disk soft-warn case; assert the rendered
     * string starts with `[warn] ENOSPC pre-flight check failed:
     * ...; proceeding anyway\n` and continues into the step rows.
     */
    #[test]
    fn plan_preview_renders_soft_warn_above_dry_run_steps() {
        let f = PoolFixture::three_disk_healthy();
        let runner = RemovalPool::three_disk()
            .install(MockRunner::default())
            .with_handler(|req| match req {
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Err(CmdError::MissingMock)),
                _ => None,
            });
        let fs = MockFs::storage(vec![]);
        let params = f.remove_params().name("disk3").dry_run(true).build();

        let plan =
            plan_remove(&runner, &fs, &params).expect("soft-warn case must produce an Ok plan");
        let rendered = plan.preview().render();
        let lines: Vec<&str> = rendered.lines().collect();
        assert!(
            !lines.is_empty(),
            "rendered preview must not be empty; got: {rendered:?}",
        );
        assert_eq!(
            lines[0],
            format!(
                "[warn] ENOSPC pre-flight check failed: {}; proceeding anyway",
                CmdError::MissingMock
            ),
            "warning must be the first line of the rendered preview; got: {rendered:?}",
        );
        // The remaining lines are the step block. Spot-check the
        // device-remove row is present after the warning.
        assert!(
            rendered.contains("btrfs device remove"),
            "step block must follow the warning; got: {rendered:?}",
        );
    }

    /* Intent: plan-derived Warn notes for `remove` render through the
     * shared `preview::render_notes_for_stderr` helper as the canonical
     * `[warn] <body>\n` shape -- the same shape that `Preview::render`
     * emits on dry-run stdout. Legacy `warning: ` prefixes do not
     * appear.
     * Why it exists: this follow-up removes the direct
     * `eprintln!("warning: {body}")` replay from `RemovePlan::execute`
     * so real-run stderr now uses the canonical form. A regression
     * that reintroduces the legacy prefix -- either in execute's
     * replay or by re-wrapping the body -- fails here.
     * Scenario: hand-built notes vec with one soft-warn body; render
     * via `PerDiskStyle::Bracketed` and assert byte-exact output with
     * no `warning:` substring.
     */
    #[test]
    fn remove_warn_notes_render_canonical_bracketed_form() {
        let notes = vec![PreviewNote::Warn(
            "ENOSPC pre-flight check failed: boom; proceeding anyway".into(),
        )];
        let rendered = preview::render_notes_for_stderr(&notes, PerDiskStyle::Bracketed);
        assert_eq!(
            rendered,
            "[warn] ENOSPC pre-flight check failed: boom; proceeding anyway\n",
        );
        assert!(
            !rendered.contains("warning:"),
            "legacy `warning:` prefix must be gone from remove's render;\n{rendered}",
        );
    }

    /* Intent: plan_remove surfaces an in-flight exclusive op as a
     * PreviewNote::Info on `plan.notes`, and the rendered preview
     * contains the "waiting for in-flight <op>" line.
     * Why it exists: PR 7 moves the busy-op diagnostic from a direct
     * stderr eprintln! into plan.notes. A regression that leaked the
     * wording back to stderr would break the dry-run stdout-only
     * contract.
     * Scenario: sysfs reports "device add" while the operator runs
     * `braid remove disk2 --dry-run` against a healthy 3-disk pool.
     */
    #[test]
    fn plan_remove_preflight_busy_op_becomes_info_note() {
        let f = PoolFixture::three_disk_healthy();
        let runner = RemovalPool::three_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]).with_excl_op("device add\n");
        let params = f.remove_params().dry_run(true).build();
        let report = plan_remove(&runner, &fs, &params);
        let plan = report.expect("plan_remove should succeed on healthy pool + busy op");
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
    }

    /* Intent: when plan_remove accumulates a preflight Info note and
     * then fails on the "disk not found in pool" validation, the
     * accumulated notes survive on `PlanFailure::notes`.
     * Why it exists: a misspelled disk name during an in-flight balance
     * must not hide the busy-op context from the operator.
     * Scenario: sysfs reports "device add" (enqueueable busy), operator
     * runs `braid remove typo-name` against a healthy 3-disk pool
     * whose members are disk1/disk2/disk3.
     */
    #[test]
    fn plan_remove_preserves_preflight_notes_on_disk_not_found() {
        let f = PoolFixture::three_disk_healthy();
        let runner = RemovalPool::three_disk().install(MockRunner::default());
        let fs = MockFs::storage(vec![]).with_excl_op("device add\n");
        let params = f.remove_params().name("typo-name").dry_run(true).build();
        let failure = match plan_remove(&runner, &fs, &params) {
            Ok(_) => panic!("expected Err(Validation), got Ok(_)"),
            Err(failure) => failure,
        };
        match &failure.error {
            RemoveError::Validation(msg) => {
                assert!(msg.contains("not found"), "expected 'not found' in: {msg}");
            }
            other => panic!("expected Validation, got: {other:?}"),
        }
        assert_eq!(
            failure.notes.len(),
            1,
            "busy-op Info note must survive the disk-not-found failure, got: {:?}",
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

    // ---------------------------------------------------------------------
    // Drift-detection regression tests for validate_pool_topology, wired
    // pre-journal and post-journal in RemovePlan::execute.
    // ---------------------------------------------------------------------

    /// Three-disk `btrfs filesystem show` baseline; matches the planning
    /// probe so the planner captures the expected live topology snapshot.
    const THREE_DISK_SHOW_BASE: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
        \tTotal devices 3 FS bytes used 16.17MiB\n\
        \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
        \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\
        \tdevid    3 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n";

    /// Build a `with_handler` override that swaps `BtrfsFilesystemShow`
    /// output per call, given a sequence of stdouts. Calls past the end of
    /// the sequence repeat the last entry. Per-test handlers are registered
    /// AFTER `RemovalPool::install`; `MockRunner::with_handler` runs them
    /// in LIFO order, so this override shadows the constant-show handler.
    fn show_sequence_handler(
        outputs: Vec<&'static str>,
    ) -> impl Fn(&CmdRequest) -> Option<Result<RawCommandOutput, CmdError>> + Send + Sync + 'static
    {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        move |req| match req {
            CmdRequest::BtrfsFilesystemShow { .. } => {
                let idx = counter.fetch_add(1, Ordering::SeqCst);
                let pick = outputs
                    .get(idx)
                    .copied()
                    .unwrap_or_else(|| *outputs.last().expect("at least one show output required"));
                Some(Ok(mock_ok("btrfs filesystem show", pick)))
            }
            _ => None,
        }
    }

    /// Add cryptsetup status / luksUUID overrides so probe_pool can
    /// process every mapper that might appear in `show` outputs --
    /// including a synthetic "braid-disk4" used in same-count-swap and
    /// device-count-grew drift scenarios. The base `RemovalPool::install`
    /// already covers disk1/disk2/disk3.
    fn extra_disk_handler()
    -> impl Fn(&CmdRequest) -> Option<Result<RawCommandOutput, CmdError>> + Send + Sync + 'static
    {
        |req| match req {
            CmdRequest::CryptsetupStatus { mapper } if mapper.as_str() == "braid-disk4" => {
                Some(Ok(mock_ok(
                    "cryptsetup status braid-disk4",
                    "braid-disk4 is active and is in use.\n  type:    LUKS2\n  device:  /dev/vde\n  mode:    read/write\n",
                )))
            }
            CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vde" => Some(Ok(mock_ok(
                "cryptsetup luksUUID /dev/vde",
                "44444444-4444-4444-4444-444444444444\n",
            ))),
            _ => None,
        }
    }

    /* Intent: pre-journal validate_pool_topology rejects topology drift
     * detected before journal::write_journal -- the command exits cleanly,
     * no mutation runs, no pending-op.json is written, and the error tells
     * the user to re-run `braid remove`.
     *
     * Why it exists: pins (a) ADR 022 -- if the helper re-introduces its
     * own probe, the no-mutation assertion flips; (b) principle 3 -- if
     * validation moves below journal::write_journal, the pending-op.json
     * assertion flips; (c) drift detection -- if the topology-match check
     * is dropped, is_err() flips.
     *
     * Scenario: while the user paused at the `yes` prompt, a third disk
     * went MISSING (here: shrank from 3-disk show to 2-disk show between
     * planning and execute). The planner would have rejected this case at
     * check_no_missing_devices; the execute-time gate enforces it.
     */
    #[test]
    fn pre_journal_drift_fails_clean_without_journal() {
        const TWO_DISK_DRIFT: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
             \tTotal devices 3 FS bytes used 16.17MiB\n\
             \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\
             \t*** Some devices missing\n";
        let f = PoolFixture::three_disk_healthy();
        let runner = RemovalPool::three_disk()
            .install(MockRunner::default())
            // Planning probe sees the healthy 3-disk pool; the next probe
            // (pre-journal validation) sees disk3 MISSING.
            .with_handler(show_sequence_handler(vec![
                THREE_DISK_SHOW_BASE,
                TWO_DISK_DRIFT,
            ]));
        let fs = MockFs::storage(vec![]);

        let result = cmd_remove(&runner, &fs, &f.remove_params().build());
        let err = result.expect_err("pre-journal drift must fail the command");
        let msg = err.to_string();

        let calls = runner.requests();
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                CmdRequest::BtrfsBalanceSingle { .. }
                    | CmdRequest::BtrfsDeviceRemove { .. }
                    | CmdRequest::CryptsetupClose { .. }
            )),
            "validation must reject drift before any mutation; calls: {calls:?}",
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "pre-journal validation failure must NOT leave a pending-op.json",
        );
        assert!(
            msg.contains("re-run `braid remove`"),
            "pre-journal drift error must direct user to re-run remove; got: {msg}",
        );
    }

    /* Intent: post-journal validate_pool_topology rejects topology drift
     * detected after journal::write_journal but before any mutation --
     * the command fails, zero mutation commands run (critically, no
     * `btrfs balance ... -f`), the journal survives for `braid recover`,
     * and the error points to recover (not to re-run remove, which would
     * be blocked by the dispatch-level pending-operation preflight).
     *
     * Why: pins the post-journal safety gate. BtrfsBalanceSingle ships -f
     * (cli/src/cmd.rs), which skips btrfs-progs' missing-device timeout
     * (balance.c:558-561). Without this gate, a disk going MISSING here
     * could subject the pool to a dangerous profile conversion.
     *
     * Scenario: pre-journal validation passed; between then and the
     * balance command, a previously flapping disk went MISSING. The
     * post-journal gate aborts the balance and preserves the journal.
     */
    #[test]
    fn post_journal_drift_preserves_journal_and_blocks_mutation() {
        const POST_JOURNAL_DRIFT: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
             \tTotal devices 3 FS bytes used 16.17MiB\n\
             \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\
             \t*** Some devices missing\n";
        let f = PoolFixture::three_disk_healthy();
        // Probe 1 = planning probe (3 disks). Probe 2 = pre-journal
        // validation probe (3 disks, passes). Probe 3 = post-journal
        // validation probe (disk3 went MISSING).
        let runner = RemovalPool::three_disk()
            .install(MockRunner::default())
            .with_handler(show_sequence_handler(vec![
                THREE_DISK_SHOW_BASE,
                THREE_DISK_SHOW_BASE,
                POST_JOURNAL_DRIFT,
            ]));
        let fs = MockFs::storage(vec![]);

        let result = cmd_remove(&runner, &fs, &f.remove_params().build());
        let err = result.expect_err("post-journal drift must fail the command");
        let msg = err.to_string();

        let calls = runner.requests();
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                CmdRequest::BtrfsBalanceSingle { .. }
                    | CmdRequest::BtrfsDeviceRemove { .. }
                    | CmdRequest::CryptsetupClose { .. }
            )),
            "post-journal validation must reject drift before any mutation; calls: {calls:?}",
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "post-journal validation failure must preserve pending-op.json so braid recover can reconcile",
        );
        assert!(
            msg.contains("`braid recover`"),
            "post-journal drift error must direct user to recover; got: {msg}",
        );
        assert!(
            !msg.contains("re-run `braid remove`"),
            "post-journal drift error must NOT direct user to re-run remove; got: {msg}",
        );
    }

    /* Intent: same-count survivor swap (one mapper added, one removed
     * between plan and execute) is rejected by pre-journal validation.
     *
     * Why: pins the mapper-set-drift case that a cardinality-only check
     * would have missed. The plan's identity snapshot includes
     * braid-disk3; the validation probe sees braid-disk4 instead.
     *
     * Scenario: between plan and execute, disk3 was unplugged and a new
     * disk4 was plugged in and auto-unlocked. Same count, different set.
     */
    #[test]
    fn pre_journal_same_count_swap_rejected() {
        const SWAP_SHOW: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
            \tTotal devices 3 FS bytes used 16.17MiB\n\
            \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
            \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\
            \tdevid    4 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk4\n";
        let f = PoolFixture::three_disk_healthy();
        let runner = RemovalPool::three_disk()
            .install(MockRunner::default())
            .with_handler(show_sequence_handler(vec![THREE_DISK_SHOW_BASE, SWAP_SHOW]))
            .with_handler(extra_disk_handler());
        let fs = MockFs::storage(vec![]);

        let err = cmd_remove(&runner, &fs, &f.remove_params().build())
            .expect_err("same-count swap must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-disk3"),
            "expected disk3 in error: {msg}"
        );
        assert!(
            msg.contains("braid-disk4"),
            "expected disk4 in error: {msg}"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "pre-journal swap failure must NOT leave a pending-op.json",
        );
        let calls = runner.requests();
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                CmdRequest::BtrfsBalanceSingle { .. }
                    | CmdRequest::BtrfsDeviceRemove { .. }
                    | CmdRequest::CryptsetupClose { .. }
            )),
            "no mutation on swap drift; calls: {calls:?}",
        );
    }

    /* Intent: same-mapper replacement (mapper name unchanged, devid or
     * luks_uuid differs) is rejected by pre-journal validation.
     *
     * Why: pins the identity-drift case that a `BTreeSet<MapperName>`
     * comparison would have missed; `BTreeMap<MapperName, DeviceIdentity>`
     * catches it on the value-equality flip.
     *
     * Scenario: operator ran `cryptsetup close` + `cryptsetup open` on a
     * different LUKS device under the same `braid-disk3` mapper between
     * plan and execute, flipping devid and luks_uuid for that mapper.
     */
    #[test]
    fn pre_journal_same_mapper_replacement_rejected() {
        // The validation probe sees braid-disk3 mapped to devid 99 with
        // a different underlying / luks_uuid.
        const REPLACED_SHOW: &str = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
            \tTotal devices 3 FS bytes used 16.17MiB\n\
            \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
            \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n\
            \tdevid    99 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n";
        let f = PoolFixture::three_disk_healthy();
        // Override CryptsetupStatus + LuksUuid for braid-disk3 on the
        // second pass so the LUKS identity differs from the planner snapshot.
        let runner = RemovalPool::three_disk()
            .install(MockRunner::default())
            .with_handler(show_sequence_handler(vec![
                THREE_DISK_SHOW_BASE,
                REPLACED_SHOW,
            ]));
        let fs = MockFs::storage(vec![]);

        let err = cmd_remove(&runner, &fs, &f.remove_params().build())
            .expect_err("same-mapper replacement must be rejected");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-disk3"),
            "expected disk3 in error: {msg}"
        );
        assert!(
            msg.contains("identity changed") || msg.contains("devid"),
            "error must signal identity drift: {msg}"
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "pre-journal replacement failure must NOT leave a pending-op.json",
        );
        let calls = runner.requests();
        assert!(
            !calls.iter().any(|c| matches!(
                c,
                CmdRequest::BtrfsBalanceSingle { .. }
                    | CmdRequest::BtrfsDeviceRemove { .. }
                    | CmdRequest::CryptsetupClose { .. }
            )),
            "no mutation on identity drift; calls: {calls:?}",
        );
    }

    /* Intent: pre-journal target hot-unplug surfaces the journal-free
     * remediation, NOT a `braid recover` hint. The detect-recover
     * message would mislead the user because no journal exists -- recover
     * would fail with "no pending operation journal found".
     *
     * Why: this is the pre-journal clean-failure path, so it must not
     * preserve or reference recovery state.
     *
     * Scenario: between plan_remove and the pre-journal probe, the target
     * disk was hot-unplugged -- cryptsetup reports `device: (null)`.
     */
    #[test]
    fn pre_journal_target_hot_unplug_message() {
        let f = PoolFixture::three_disk_healthy();
        // Two show calls: planning, pre-journal validation. Flip the
        // target's cryptsetup status to (null) for the second probe only.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let show_counter = std::sync::Arc::new(AtomicUsize::new(0));
        let show_counter_for_status = std::sync::Arc::clone(&show_counter);
        let runner = RemovalPool::three_disk()
            .install(MockRunner::default())
            .with_handler(move |req| match req {
                CmdRequest::BtrfsFilesystemShow { .. } => {
                    show_counter.fetch_add(1, Ordering::SeqCst);
                    Some(Ok(mock_ok("btrfs filesystem show", THREE_DISK_SHOW_BASE)))
                }
                _ => None,
            })
            .with_handler(move |req| match req {
                CmdRequest::CryptsetupStatus { mapper } if mapper.as_str() == "braid-disk2" => {
                    let phase = show_counter_for_status.load(Ordering::SeqCst);
                    let device = if phase >= 2 { "(null)" } else { "/dev/vdc" };
                    Some(Ok(mock_ok(
                        "cryptsetup status braid-disk2",
                        &format!(
                            "braid-disk2 is active and is in use.\n  type:    LUKS2\n  device:  {device}\n  mode:    read/write\n"
                        ),
                    )))
                }
                _ => None,
            });
        let fs = MockFs::storage(vec![]);

        let err = cmd_remove(&runner, &fs, &f.remove_params().build())
            .expect_err("pre-journal hot-unplug must fail");
        let msg = err.to_string();

        assert!(msg.contains("braid-disk2"), "expected target mapper: {msg}");
        assert!(
            msg.contains("device: (null)"),
            "expected null marker: {msg}"
        );
        assert!(msg.contains("hot-unplug"), "expected hot-unplug: {msg}");
        assert!(msg.contains("braid lock"), "expected braid lock: {msg}");
        assert!(msg.contains("braid unlock"), "expected braid unlock: {msg}");
        assert!(msg.contains("reboot"), "expected reboot alt: {msg}");
        assert!(
            msg.contains("re-run `braid remove`"),
            "pre-journal hot-unplug must point at re-run remove: {msg}"
        );
        assert!(
            !msg.contains("braid recover"),
            "pre-journal hot-unplug must NOT mention braid recover (would fail with no journal): {msg}",
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_none(),
            "pre-journal hot-unplug must NOT leave a pending-op.json",
        );
    }

    /* Intent: post-journal target hot-unplug surfaces the journal-bearing
     * remediation -- mentions `braid recover` and preserves the journal
     * for it. Mirrors the legacy in-helper wording in `device_remove_error`,
     * which is what the existing operator muscle-memory expects.
     *
     * Why: complements the pre-journal hot-unplug regression so callers
     * see the same rich UX but routed by call position.
     *
     * Scenario: pre-journal validation passed; between then and the
     * post-journal validation, the target's underlying was hot-unplugged.
     */
    #[test]
    fn post_journal_target_hot_unplug_message() {
        let f = PoolFixture::three_disk_healthy();
        // Three show calls: planning, pre-journal validation, post-journal
        // validation. We flip cryptsetup status to (null) for braid-disk2
        // ONLY after the second probe has finished.
        use std::sync::atomic::{AtomicUsize, Ordering};
        let show_counter = std::sync::Arc::new(AtomicUsize::new(0));
        let show_counter_for_status = std::sync::Arc::clone(&show_counter);
        let runner = RemovalPool::three_disk()
            .install(MockRunner::default())
            .with_handler(move |req| match req {
                CmdRequest::BtrfsFilesystemShow { .. } => {
                    show_counter.fetch_add(1, Ordering::SeqCst);
                    Some(Ok(mock_ok("btrfs filesystem show", THREE_DISK_SHOW_BASE)))
                }
                _ => None,
            })
            .with_handler(move |req| match req {
                CmdRequest::CryptsetupStatus { mapper } if mapper.as_str() == "braid-disk2" => {
                    // First two probes (planning + pre-journal): healthy.
                    // Probe #3 (post-journal): hot-unplugged.
                    let phase = show_counter_for_status.load(Ordering::SeqCst);
                    let device = if phase >= 3 { "(null)" } else { "/dev/vdc" };
                    Some(Ok(mock_ok(
                        "cryptsetup status braid-disk2",
                        &format!(
                            "braid-disk2 is active and is in use.\n  type:    LUKS2\n  device:  {device}\n  mode:    read/write\n"
                        ),
                    )))
                }
                _ => None,
            });
        let fs = MockFs::storage(vec![]);

        let err = cmd_remove(&runner, &fs, &f.remove_params().build())
            .expect_err("post-journal hot-unplug must fail");
        let msg = err.to_string();

        assert!(msg.contains("braid-disk2"), "expected target mapper: {msg}");
        assert!(
            msg.contains("device: (null)"),
            "expected null marker: {msg}"
        );
        assert!(msg.contains("hot-unplug"), "expected hot-unplug: {msg}");
        assert!(
            msg.contains("braid recover"),
            "expected braid recover: {msg}"
        );
        assert!(msg.contains("braid lock"), "expected braid lock: {msg}");
        assert!(msg.contains("braid unlock"), "expected braid unlock: {msg}");
        assert!(msg.contains("reboot"), "expected reboot alt: {msg}");
        assert!(
            !msg.contains("re-run `braid remove`"),
            "post-journal hot-unplug must NOT direct at re-run remove (blocked by pending-op): {msg}",
        );
        assert!(
            journal::load_journal(&f.paths).unwrap().is_some(),
            "post-journal hot-unplug must preserve pending-op.json for recover",
        );
    }

    // ---------------------------------------------------------------------
    // UUID-identity boundary tests (Phase 3a)
    //
    // Test-module seed allocation note: remove.rs uses 400-449 for new
    // UUID-identity tests, leaving 100-199 to membership.rs, 200 to
    // luks.rs, 201-299 to journal.rs, 300-399 to cmd.rs.
    // ---------------------------------------------------------------------

    use crate::test_fixtures::{disk_member_with, test_uuid};

    /// Build a fresh 3-disk pool fixture whose membership pins one
    /// specific (uuid, name, by_id, devid) -> entry. Other entries
    /// (`disk1`, `disk3`) use `canonical_luks_uuid(1/3)` and devids that
    /// line up with the `RemovalPool::three_disk` topology mocks (UUIDs
    /// `11111111...`, `33333333...`).
    fn three_disk_membership_with_pinned_disk2(target_uuid: &LuksUuid) -> PoolMembership {
        let mut m = PoolMembership::empty();
        // disk1: canonical UUID 11111111..., devid 1.
        m.insert(
            canonical_luks_uuid(1),
            membership::DiskMember {
                name: DiskName::parse("disk1").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk1").unwrap(),
                devid: Some(Devid::new(1)),
                added_at: None,
            },
        )
        .unwrap();
        // disk2: pinned UUID under the operator-typed name "disk2",
        // devid 2 to match the RemovalPool topology.
        m.insert(
            target_uuid.clone(),
            membership::DiskMember {
                name: DiskName::parse("disk2").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
                devid: Some(Devid::new(2)),
                added_at: None,
            },
        )
        .unwrap();
        // disk3: canonical UUID 33333333..., devid 3.
        m.insert(
            canonical_luks_uuid(3),
            membership::DiskMember {
                name: DiskName::parse("disk3").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk3").unwrap(),
                devid: Some(Devid::new(3)),
                added_at: None,
            },
        )
        .unwrap();
        m
    }

    // Intent: cmd_remove resolves the user-typed name to UUID via
    //   membership.by_name at the boundary, journals OpKind::Remove with
    //   that UUID, and removes the member by UUID from target_membership.
    //
    // Why: this is the load-bearing boundary contract -- a regression
    //   that found by mapper or by name in target_membership would
    //   silently skip the remove (no-op) on benign drift.
    //
    // Scenario: 3-disk healthy pool. User runs `braid remove disk2`.
    //   Membership has disk2 pinned under U_R. Pool.devices reports
    //   disk2 with that UUID. Assert: journal records OpKind::Remove
    //   { luks_uuid: U_R, name: "disk2" }; target_membership on disk
    //   no longer has U_R; surviving disks unchanged.
    #[test]
    fn cmd_remove_resolves_name_to_uuid_and_journals_uuid() {
        let f = PoolFixture::three_disk_healthy();
        // Override disk2's UUID to a sentinel value pinned by this test.
        let u_r = test_uuid(400);
        let m = three_disk_membership_with_pinned_disk2(&u_r);
        membership::save_membership(&m, &f.paths).unwrap();

        // The RemovalPool topology returns canonical UUIDs for disk1/2/3.
        // Override disk2's UUID probe to return the pinned u_r so the
        // pool.devices entry matches the membership UUID we just pinned.
        let runner = RemovalPool::three_disk()
            .install(MockRunner::default())
            .with_handler({
                let u_r = u_r.clone();
                move |req| match req {
                    CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vdc" => Some(Ok(
                        mock_ok("cryptsetup luksUUID /dev/vdc", &format!("{u_r}\n")),
                    )),
                    _ => None,
                }
            });
        let fs = MockFs::storage(vec![]);

        // Wedge the journal-write step: force btrfs device remove to
        // fail so the journal survives for our assertion.
        let runner = runner.with_handler(|req| match req {
            CmdRequest::BtrfsDeviceRemove { .. } => Some(Ok(RawCommandOutput {
                cmd: "btrfs device remove".into(),
                stdout: String::new(),
                stderr: btrfs_remove_path_error(
                    "/dev/mapper/braid-disk2",
                    "No space left on device",
                ),
                exit_status: 1,
            })),
            _ => None,
        });

        let result = cmd_remove(
            &runner,
            &fs,
            &f.remove_params().name("disk2").yes(true).build(),
        );
        assert!(result.is_err(), "device-remove fault must surface");

        let journal = journal::load_journal(&f.paths)
            .unwrap()
            .expect("journal must survive device-remove failure");
        match journal.op {
            journal::OpKind::Remove { luks_uuid, name } => {
                assert_eq!(luks_uuid, u_r, "journaled UUID must be the resolved one");
                assert_eq!(
                    name.as_str(),
                    "disk2",
                    "journaled name must be the persisted one"
                );
            }
            other => panic!("expected OpKind::Remove, got: {other:?}"),
        }
        assert!(
            journal.target_membership.by_uuid(&u_r).is_none(),
            "target_membership must remove the UUID, not a name-keyed entry"
        );
    }

    // Intent: drifted-member remove preserves the observed PoolDevice.mapper
    //   on RemoveWorkPlan.target_mapper (not the reconstructed
    //   mapper_name(&name)) so the post-commit CryptsetupClose still targets
    //   the right dm slot under benign mapper drift.
    //
    // Why: the post-commit close consumes target_mapper byte-for-byte;
    //   reconstructing it from the disk name re-opens the same drift hazard
    //   the lock.rs migration closes.
    //
    // Scenario: membership has U_R -> { name: "disk2", devid: 2 }. PoolState
    //   reports a PoolDevice for U_R with mapper = "braid-WRONG" (drifted).
    //   Plan and execute remove name = "disk2"; the post-commit close
    //   request must be CryptsetupClose { mapper: "braid-WRONG" }.
    #[test]
    fn drifted_member_remove_closes_observed_mapper() {
        let f = PoolFixture::empty();
        let u_r = test_uuid(401);
        let mut m = PoolMembership::empty();
        let (_, m1) = disk_member_with(
            410,
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            Some(Devid::new(1)),
            None,
        );
        m.insert(canonical_luks_uuid(1), m1).unwrap();
        m.insert(
            u_r.clone(),
            membership::DiskMember {
                name: DiskName::parse("disk2").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/virtio-disk2").unwrap(),
                devid: Some(Devid::new(2)),
                added_at: None,
            },
        )
        .unwrap();
        let (_, m3) = disk_member_with(
            411,
            "disk3",
            "/dev/disk/by-id/virtio-disk3",
            Some(Devid::new(3)),
            None,
        );
        m.insert(canonical_luks_uuid(3), m3).unwrap();
        membership::save_membership(&m, &f.paths).unwrap();

        // Live pool: U_R observed under MAPPER "braid-WRONG", not "braid-right".
        let show = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
             \tTotal devices 3 FS bytes used 16.17MiB\n\
             \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-WRONG\n\
             \tdevid    3 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n"
            .to_string();
        let u_r_for_probe = u_r.clone();
        let runner = MockRunner::default().with_handler({
            move |req| match req {
                CmdRequest::BtrfsFilesystemShow { .. } => {
                    Some(Ok(mock_ok("btrfs filesystem show", &show)))
                }
                CmdRequest::CryptsetupStatus { mapper } => {
                    let dev = match mapper.as_str() {
                        "braid-disk1" => "/dev/vdb",
                        "braid-WRONG" => "/dev/vdc",
                        "braid-disk3" => "/dev/vdd",
                        _ => return None,
                    };
                    Some(Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"
                        ),
                    )))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = match device.as_str() {
                        "/dev/vdb" => "11111111-1111-1111-1111-111111111111",
                        // /dev/vdc backs braid-WRONG -> U_R
                        "/dev/vdc" => return Some(Ok(mock_ok(
                            "cryptsetup luksUUID /dev/vdc",
                            &format!("{u_r_for_probe}\n"),
                        ))),
                        "/dev/vdd" => "33333333-3333-3333-3333-333333333333",
                        _ => return None,
                    };
                    Some(Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                    )))
                }
                CmdRequest::BtrfsBalanceStatus { .. } => Some(Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                ))),
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(mock_ok(
                    "btrfs device usage --raw /mnt/storage",
                    &valid_three_disk_usage_stdout(),
                ))),
                CmdRequest::BtrfsFilesystemDfJson { .. } => Some(Ok(mock_ok(
                    "btrfs --format json filesystem df /mnt/storage",
                    valid_three_disk_df_json(),
                ))),
                CmdRequest::BtrfsDeviceRemove { .. } => Some(Ok(mock_ok("btrfs device remove", ""))),
                CmdRequest::CryptsetupClose { .. } => Some(Ok(mock_ok("cryptsetup close", ""))),
                _ => None,
            }
        });
        let fs = MockFs::storage(vec![]);
        let params = f.remove_params().name("disk2").yes(true).build();

        // Plan first to assert target_mapper preservation directly.
        let plan = plan_remove(&runner, &fs, &params).expect("plan succeeds");
        assert_eq!(
            plan.work_plan.target_mapper.as_str(),
            "braid-WRONG",
            "target_mapper must be the observed PoolDevice.mapper, not mapper_name(&name)"
        );

        // Now execute; the post-commit close must target "braid-WRONG".
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            plan.execute(&runner, &fs, &params)
                .expect("remove succeeds");
        });
        let calls = runner.requests();
        let close_calls: Vec<&CmdRequest> = calls
            .iter()
            .filter(|c| matches!(c, CmdRequest::CryptsetupClose { .. }))
            .collect();
        assert_eq!(
            close_calls.len(),
            1,
            "exactly one CryptsetupClose expected; got: {close_calls:?}"
        );
        match close_calls[0] {
            CmdRequest::CryptsetupClose { mapper } => {
                assert_eq!(
                    mapper.as_str(),
                    "braid-WRONG",
                    "close must target the observed mapper, not the reconstructed one"
                );
            }
            other => panic!("unexpected: {other:?}"),
        }
        let remove_wait = "[wait] pool: removing disk2...";
        let remove_ok = "[ok]   pool: disk2 removed";
        let close_wait = "[wait] disk disk2: locking...";
        let close_ok = "[ok]   disk disk2: locked";
        assert!(
            captured.contains(remove_wait) && captured.contains(remove_ok),
            "remove progress must use the disk name: {captured:?}"
        );
        assert!(
            captured.contains(close_wait) && captured.contains(close_ok),
            "close trailer must use the disk name: {captured:?}"
        );
        assert!(
            captured.find(remove_wait) < captured.find(remove_ok),
            "remove wait must precede ok, got: {captured:?}"
        );
        assert!(
            captured.find(close_wait) < captured.find(close_ok),
            "close wait must precede ok, got: {captured:?}"
        );
        assert!(
            !captured.contains("WRONG"),
            "remove output must not echo drifted mapper basename: {captured:?}"
        );
    }

    // Intent: post-commit close UUID-probe demotes to a skip when the
    //   observed mapper now holds a different LUKS UUID (operator
    //   double-drift between plan and execute). The control arm with a
    //   matching UUID still issues the close.
    //
    // Why: defense-in-depth. Journaling the observed mapper closes the
    //   single-drift gap; this probe closes the double-drift gap where
    //   the operator reopens a foreign disk under the same mapper.
    //
    // Scenario: same drifted setup as the previous test but after btrfs
    //   commit, `cryptsetup status braid-WRONG` resolves to a foreign
    //   backing device whose LUKS UUID is U_FOREIGN != U_R.
    //   Assert: the post-commit probe follows the active mapper to that
    //   backing device; zero CryptsetupClose for braid-WRONG.
    #[test]
    fn post_commit_close_uuid_probe_demotes_to_skip_on_mismatch() {
        let f = PoolFixture::empty();
        let u_r = test_uuid(402);
        let u_foreign = test_uuid(403);
        let mut m = PoolMembership::empty();
        let (_, m1) = disk_member_with(
            420,
            "disk1",
            "/dev/disk/by-id/virtio-disk1",
            Some(Devid::new(1)),
            None,
        );
        m.insert(canonical_luks_uuid(1), m1).unwrap();
        m.insert(
            u_r.clone(),
            membership::DiskMember {
                name: DiskName::parse("right").unwrap(),
                by_id: ByIdPath::parse("/dev/disk/by-id/virtio-right").unwrap(),
                devid: Some(Devid::new(2)),
                added_at: None,
            },
        )
        .unwrap();
        let (_, m3) = disk_member_with(
            421,
            "disk3",
            "/dev/disk/by-id/virtio-disk3",
            Some(Devid::new(3)),
            None,
        );
        m.insert(canonical_luks_uuid(3), m3).unwrap();
        membership::save_membership(&m, &f.paths).unwrap();

        let show = "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\
             \tTotal devices 3 FS bytes used 16.17MiB\n\
             \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\
             \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-WRONG\n\
             \tdevid    3 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk3\n";
        let u_r_for_plan = u_r.clone();
        let u_foreign_for_probe = u_foreign.clone();
        let remove_committed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let remove_committed_for_status = std::sync::Arc::clone(&remove_committed);
        let remove_committed_for_remove = std::sync::Arc::clone(&remove_committed);
        let runner = MockRunner::default().with_handler({
            move |req| match req {
                CmdRequest::BtrfsFilesystemShow { .. } => {
                    Some(Ok(mock_ok("btrfs filesystem show", show)))
                }
                CmdRequest::CryptsetupStatus { mapper } => {
                    let dev = match mapper.as_str() {
                        "braid-disk1" => "/dev/vdb",
                        "braid-WRONG" => {
                            if remove_committed_for_status.load(std::sync::atomic::Ordering::SeqCst)
                            {
                                "/dev/vdf"
                            } else {
                                "/dev/vdc"
                            }
                        }
                        "braid-disk3" => "/dev/vdd",
                        _ => return None,
                    };
                    Some(Ok(mock_ok(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"
                        ),
                    )))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let body = match device.as_str() {
                        "/dev/vdb" => "11111111-1111-1111-1111-111111111111".to_string(),
                        // planning-time backing probe: U_R is still here
                        "/dev/vdc" => format!("{u_r_for_plan}"),
                        "/dev/vdd" => "33333333-3333-3333-3333-333333333333".to_string(),
                        // Post-commit probe follows braid-WRONG's active
                        // mapper to the backing disk now holding U_FOREIGN.
                        "/dev/vdf" => format!("{u_foreign_for_probe}"),
                        _ => return None,
                    };
                    Some(Ok(mock_ok(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{body}\n"),
                    )))
                }
                CmdRequest::BtrfsBalanceStatus { .. } => Some(Ok(mock_ok(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                ))),
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Some(Ok(mock_ok(
                    "btrfs device usage --raw /mnt/storage",
                    &valid_three_disk_usage_stdout(),
                ))),
                CmdRequest::BtrfsFilesystemDfJson { .. } => Some(Ok(mock_ok(
                    "btrfs --format json filesystem df /mnt/storage",
                    valid_three_disk_df_json(),
                ))),
                CmdRequest::BtrfsDeviceRemove { .. } => {
                    remove_committed_for_remove.store(true, std::sync::atomic::Ordering::SeqCst);
                    Some(Ok(mock_ok("btrfs device remove", "")))
                }
                CmdRequest::CryptsetupClose { .. } => Some(Ok(mock_ok("cryptsetup close", ""))),
                _ => None,
            }
        });
        let fs = MockFs::storage(vec![]);
        cmd_remove(
            &runner,
            &fs,
            &f.remove_params().name("right").yes(true).build(),
        )
        .expect("remove command must complete -- close skip is logged-warning, not error");

        let calls = runner.requests();
        let probe_for_foreign_backing = calls
            .iter()
            .filter(
                |c| matches!(c, CmdRequest::CryptsetupLuksUuid { device } if device == "/dev/vdf"),
            )
            .count();
        assert_eq!(
            probe_for_foreign_backing, 1,
            "exactly one post-commit UUID probe against braid-WRONG's foreign backing device"
        );
        let closes_for_wrong = calls
            .iter()
            .filter(
                |c| matches!(c, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-WRONG"),
            )
            .count();
        assert_eq!(
            closes_for_wrong, 0,
            "zero CryptsetupClose against braid-WRONG -- probe mismatch must demote to skip"
        );
    }

    // Intent: remove warns and skips the target-mapper close when the
    //   close-time UUID probe reports an inactive mapper.
    // Why it exists: inactive is now caller-classified; the helper returns it
    //   silently, so the remove execute path must keep its operator warning.
    // Scenario: disk2 is removed from a healthy pool, but braid-disk2 has
    //   already been closed before the post-commit best-effort close runs.
    #[test]
    fn post_commit_close_inactive_warns_and_skips_close() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let f = PoolFixture::two_disk_healthy();
        let remove_committed = Arc::new(AtomicBool::new(false));
        let runner = RemovalPool::two_disk()
            .install(MockRunner::default())
            .with_handler({
                let remove_committed = Arc::clone(&remove_committed);
                move |req| match req {
                    CmdRequest::BtrfsDeviceRemove { .. } => {
                        remove_committed.store(true, Ordering::SeqCst);
                        Some(Ok(mock_ok("btrfs device remove", "")))
                    }
                    CmdRequest::CryptsetupStatus { mapper }
                        if mapper.as_str() == "braid-disk2"
                            && remove_committed.load(Ordering::SeqCst) =>
                    {
                        Some(Ok(RawCommandOutput {
                            cmd: "cryptsetup status braid-disk2".into(),
                            stdout: String::new(),
                            stderr: "/dev/mapper/braid-disk2 is inactive.\n".into(),
                            exit_status: 4,
                        }))
                    }
                    _ => None,
                }
            });
        let fs = MockFs::storage(vec![]);

        let captured = crate::status_tag::testing::capture_with_color(false, || {
            cmd_remove(&runner, &fs, &f.remove_params().build())
                .expect("inactive close skip must not fail remove");
        });

        assert!(
            captured.contains(
                "Warning: post-commit close skipped for mapper braid-disk2: \
                 probe failed (mapper is inactive); expected LUKS UUID \
                 22222222-2222-2222-2222-222222222222\n"
            ),
            "inactive target-mapper close must warn: {captured:?}"
        );
        assert!(
            !runner.requests().iter().any(|request| {
                matches!(request, CmdRequest::CryptsetupClose { mapper } if mapper.as_str() == "braid-disk2")
            }),
            "inactive target-mapper probe must skip close"
        );
    }
}
