use crate::cmd::{CmdError, CmdRequest, CommandRunner, Step};
use crate::config::{Config, braid_disk_name};
use crate::dm_identity::{UnprovenMapper, mapper_luks_uuid};
use crate::filesystem::Filesystem;
use crate::mapper_close::{
    CloseMapperError, close_mapper_with_retry, forget_existing_scanned_devices_best_effort,
};
use crate::membership::PoolMembership;
use crate::online_state::{
    BRAID_ONLINE_UNIT, OnlineError, OnlineStateOps, RealOnlineStateOps, mark_offline,
    observe_unmounted,
};
use crate::pool_lock::StopCoordinatorGuard;
use crate::preflight::{self, LockExclop};
use crate::preview::{self, PerDiskStyle, Preview, PreviewNote};
use crate::probe::{ProbeError, probe_fsid, probe_pool};
use crate::progress::{RealSleeper, Sleeper};
use crate::status_tag::{StatusTag, color_enabled_for_stderr, emit_status, status_line};
use crate::types::{DiskName, Fsid, LuksUuid, MapperName, MountPoint, PoolState};
use crate::util::detail_suffix;
use std::collections::HashSet;
use std::io::{self, Write};

const UMOUNT_RETRY_ATTEMPTS: u32 = 3;
/// Longer transient-busy umount retry for the systemd-stop path. During
/// shutdown the Rust mutator can die before its blocking btrfs-progs balance
/// child releases the mount fd, so this count must outlast that transient
/// hold. It is a lower-bound heuristic, not an upper-bound coupling: it is not
/// gated by `--deadline-secs` (which bounds only lock acquisition) and need
/// not fit under `TimeoutStopSec` -- post-lock cleanup is backstopped by
/// systemd's SIGKILL, not by this count. See ADR 018
/// `docs/design/decisions/018-systemd-lifecycle.md#execstop-bounded-wait-pattern`.
const SYSTEMD_STOP_UMOUNT_RETRY_ATTEMPTS: u32 = 60;
const UMOUNT_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(500);

#[derive(Debug, thiserror::Error)]
pub enum LockError {
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Failed(String),
    #[error("device busy: {0}")]
    DeviceBusy(String),
}

impl From<CloseMapperError> for LockError {
    fn from(value: CloseMapperError) -> Self {
        match value {
            CloseMapperError::Cmd(e) => LockError::Cmd(e),
            CloseMapperError::Failed(msg) => LockError::Failed(msg),
            CloseMapperError::DeviceBusy(msg) => LockError::DeviceBusy(msg),
        }
    }
}

/// Plain-lock orchestration preserves `cmd_lock`'s typed failure while
/// distinguishing it from the coordinator marker write path.
#[derive(Debug)]
pub enum LockOrchestrateError {
    CmdLock(LockError),
    MarkDone(io::Error),
}

impl std::fmt::Display for LockOrchestrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CmdLock(e) => write!(f, "{e}"),
            Self::MarkDone(e) => write!(f, "failed to mark lock cleanup done: {e}"),
        }
    }
}

/// Snapshot of the pool's live state at lock-planning time. Variants
/// encode the three real branches: a successful per-device probe, a
/// mounted pool whose per-device probe failed (FSID only keys the
/// exclusive-op preflight), and an unmounted pool that bypasses
/// mounted-pool probing and FSID preflight (per-candidate UUID probing
/// still runs during mapper cleanup).
enum Snapshot {
    /// Per-device probe succeeded; close-set classification routes
    /// through observed LUKS UUIDs.
    Probed(PoolState),
    /// Pool is mounted (btrfs occupies the mount point); per-device
    /// probing failed. `fsid` is read only to key the exclusive-op
    /// preflight -- it is not compared to any persisted pool identity
    /// (braid persists none); `probe_error` is quoted in the fallback
    /// warning.
    ProbeFailed { fsid: Fsid, probe_error: ProbeError },
    /// Pool is not mounted. Skips the mounted-pool `probe_pool` call
    /// and the FSID preflight gate; UUID-scanned mapper cleanup still
    /// runs via `build_close_sets_uuid_scanned_fallback` to close any
    /// braid-* mappers left open (each candidate is verified by the LUKS
    /// UUID its dm UUID records before being added to the close set).
    Unmounted,
}

/// Lock planning mode selects what lock does about a running balance (plain
/// lock refuses it, shutdown pauses it) and the umount retry budget. Both
/// modes share every other exclusive-op rule; see
/// `cli/src/preflight.rs#require_lock_preflight`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LockMode {
    User,
    SystemdStop,
}

/// Centralizes the lock-mode policy gate so mounted probe success and mounted
/// probe fallback cannot drift on whether a balance is refused, paused before
/// teardown, or left paused with a note. Returns whether execute must pause a
/// running balance before unmount.
///
/// The mode decides only the running balance. A paused balance proceeds in
/// both modes, because unmount keeps it paused on disk; see
/// [`preflight::LockExclop::BalancePaused`].
fn lock_preflight_pause_decision<F: Filesystem + ?Sized>(
    fs: &F,
    mode: LockMode,
    fsid: &Fsid,
    notes: &mut Vec<PreviewNote>,
) -> Result<bool, String> {
    match preflight::require_lock_preflight(fs, fsid)? {
        LockExclop::Idle => Ok(false),
        LockExclop::BalancePaused => {
            notes.push(PreviewNote::Warn(PAUSED_BALANCE_NOTE.to_owned()));
            Ok(false)
        }
        LockExclop::BalanceRunning => match mode {
            LockMode::User => Err(preflight::running_balance_lock_refusal()),
            LockMode::SystemdStop => Ok(true),
        },
    }
}

/// Tells the operator that lock proceeds under a paused balance and leaves it
/// for them to resume or cancel later. It names no resume command: resuming a
/// crash-paused balance can be unsafe
/// (`docs/internals/btrfs/balance-soft.md#recover-replay`),
/// and unlock and doctor already carry the advice.
const PAUSED_BALANCE_NOTE: &str = "btrfs balance is paused -- lock leaves it paused, \
                                   and the next braid unlock and braid doctor will report it";

/// A mapper to close at lock execution. `mapper` is the observed name,
/// while `kind` carries the member/orphan status behavior decided at
/// plan time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockMapperClose {
    /// Observed mapper to close; may differ from `mapper_name(display_name)`.
    pub mapper: MapperName,
    /// Status-output and error-behavior class for this planned close.
    pub kind: LockMapperCloseKind,
}

/// Identity class for a planned lock close.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LockMapperCloseKind {
    /// Mapper proven to belong to a pool member; carries the validated
    /// membership disk name used in status output.
    MemberOwned { display_name: DiskName },
    /// Braid-prefixed mapper not proven to be a member; carries the raw
    /// basename so malformed names remain closable and reportable.
    Orphan { disk_name: String },
}

impl LockMapperClose {
    fn is_orphan(&self) -> bool {
        matches!(self.kind, LockMapperCloseKind::Orphan { .. })
    }

    fn disk_label(&self) -> &str {
        match &self.kind {
            LockMapperCloseKind::MemberOwned { display_name } => display_name.as_str(),
            LockMapperCloseKind::Orphan { disk_name } => disk_name.as_str(),
        }
    }
}

/// Ordered close set produced by lock planning and consumed by dry-run
/// preview, btrfs forget, and real mapper close execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockCloseSet {
    entries: Vec<LockMapperClose>,
}

impl LockCloseSet {
    /// Combine already-classified member-owned and orphan closes into the
    /// lock ordering contract: all members first, then all orphans.
    pub fn from_classified(members: Vec<LockMapperClose>, orphans: Vec<LockMapperClose>) -> Self {
        let entries = members.into_iter().chain(orphans).collect();
        Self { entries }
    }

    /// Borrow the ordered entries so dry-run and execute loops cannot
    /// re-sort or rebuild the planned close set.
    pub fn entries(&self) -> &[LockMapperClose] {
        &self.entries
    }

    /// Report whether lock has any mapper close work to do.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Render the exact ordered `/dev/mapper/...` paths handed to
    /// `btrfs device scan --forget`.
    pub fn forget_paths(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|entry| entry.mapper.dev_path())
            .collect()
    }
}

/// Plan-level close-set outputs accumulated by the classification helpers so
/// the planner owns one sink. `cleanup_uncertain` records a skipped mapper or
/// a failed `/dev/mapper` scan: either could hide a live member, so it also
/// withholds every already-closed claim.
#[derive(Default)]
struct CloseSetAccumulator {
    notes: Vec<PreviewNote>,
    members_potentially_present: HashSet<DiskName>,
    cleanup_uncertain: bool,
}

/// Classify one open mapper by the LUKS UUID it carries: a membership key
/// makes it member-owned, any other LUKS UUID an orphan. Shared by every
/// pass, so a mapper's identity comes from the mapper itself (its dm UUID)
/// whether its backing disk is present, gone, or unreadable.
fn classify_by_uuid(
    mapper: &MapperName,
    uuid: &LuksUuid,
    membership: &PoolMembership,
) -> LockMapperCloseKind {
    match membership.by_uuid(uuid) {
        Some(member) => LockMapperCloseKind::MemberOwned {
            display_name: member.name.clone(),
        },
        None => LockMapperCloseKind::Orphan {
            disk_name: orphan_disk_label(mapper),
        },
    }
}

/// Enumerate existing `/dev/mapper/braid-*` basenames that are not
/// already proven by full-pool probing. The prefix is only a cleanup
/// namespace; ownership is decided later by the LUKS UUID each candidate's
/// dm UUID records.
fn scan_braid_mapper_candidates<F: Filesystem + ?Sized>(
    fs: &F,
    already_observed: &HashSet<&str>,
) -> Result<Vec<MapperName>, std::io::Error> {
    let entries = fs.list_dir("/dev/mapper")?;
    let mut candidates = Vec::new();
    for entry in entries {
        // Keep only `braid-<name>` mappers with a non-empty disk name. A
        // bare `braid-` is never braid-created (DiskName is never empty),
        // so it is not a cleanup candidate; ownership stays UUID-gated
        // downstream.
        if braid_disk_name(&entry).is_none() {
            continue;
        }
        if already_observed.contains(entry.as_str()) {
            continue;
        }
        let mapper = MapperName::from_basename(entry);
        if !fs.exists(&mapper.dev_path()) {
            continue;
        }
        candidates.push(mapper);
    }
    Ok(candidates)
}

/// Display label for an orphan mapper close: the braid disk name when
/// present, else the full mapper basename. Guarantees an orphan status row
/// (`disk <label>: locking (orphan)...`) never renders a blank label, even
/// for a degenerate bare `braid-` mapper.
fn orphan_disk_label(mapper: &MapperName) -> String {
    braid_disk_name(mapper.as_str())
        .unwrap_or(mapper.as_str())
        .to_owned()
}

/// Message body (no `[warn]` prefix) for a failed /dev/mapper scan.
/// Shared between the dry-run preview and the real-run stderr warn so
/// both branches use identical wording.
fn mapper_scan_warn_body(e: &std::io::Error) -> String {
    format!("could not scan /dev/mapper for braid mappers: {e} (skipping)")
}

/// Message body (no `[warn]` prefix) for a per-orphan mapper note.
/// Shared between the dry-run preview and the real-run prelude so both
/// branches use identical wording. Accepts the typed `MapperName` so
/// the wire format renders through `MapperName::Display` -- which is
/// byte-identical to today's `String` rendering for a newtype over
/// `String`.
fn orphan_mapper_warn_body(entry: &MapperName) -> String {
    format!("orphaned mapper {entry} (not in pool.json -- likely a prior crash)")
}

/// Message body for a braid-prefixed mapper that carries no provable LUKS
/// UUID. The mapper is left open because name evidence is insufficient for
/// either member or orphan cleanup.
fn skipped_mapper_warn_body(entry: &MapperName, detail: &UnprovenMapper) -> String {
    format!("skipping mapper {entry}: cannot verify backing LUKS UUID ({detail})")
}

/// Shared close-set append so the probed pool devices and the stranded scan
/// record members and orphans, and render orphan warnings, through one site.
fn push_classified_close(
    mapper: MapperName,
    kind: LockMapperCloseKind,
    member_owned: &mut Vec<LockMapperClose>,
    orphan_mappers: &mut Vec<LockMapperClose>,
    acc: &mut CloseSetAccumulator,
) {
    match &kind {
        LockMapperCloseKind::MemberOwned { display_name } => {
            acc.members_potentially_present.insert(display_name.clone());
            member_owned.push(LockMapperClose { mapper, kind });
        }
        LockMapperCloseKind::Orphan { .. } => {
            acc.notes
                .push(PreviewNote::Warn(orphan_mapper_warn_body(&mapper)));
            orphan_mappers.push(LockMapperClose { mapper, kind });
        }
    }
}

/// Message body (no `[warn]` prefix) for the mounted fallback warning.
/// The unmount is licensed by mount-point ownership; the destructive
/// close stays UUID-gated, so only verified braid-* mappers are closed
/// and unverified candidates are skipped. The FSID only keys the
/// exclusive-op preflight, not an ownership check.
fn uuid_scanned_fallback_warn_body(probe_error: &ProbeError) -> String {
    format!(
        "per-device probe failed ({probe_error}); falling back to UUID-scanned mapper cleanup. \
         Only braid-* mappers with a verified backing LUKS UUID will be closed; unverified candidates are skipped."
    )
}

/// Classify one scanned candidate by the LUKS UUID its dm UUID records: push
/// a member/orphan close entry, or warn and mark cleanup uncertain when the
/// mapper carries no provable LUKS UUID. Keeping this shared between full and
/// fallback planning prevents stranded mapper handling from drifting back to
/// name inference.
fn push_uuid_classified_candidate<F: Filesystem + ?Sized>(
    fs: &F,
    mapper: MapperName,
    membership: &PoolMembership,
    member_owned: &mut Vec<LockMapperClose>,
    orphan_mappers: &mut Vec<LockMapperClose>,
    acc: &mut CloseSetAccumulator,
) {
    match mapper_luks_uuid(fs, &mapper) {
        Ok(uuid) => {
            let kind = classify_by_uuid(&mapper, &uuid, membership);
            push_classified_close(mapper, kind, member_owned, orphan_mappers, acc);
        }
        Err(unproven) => {
            acc.notes.push(PreviewNote::Warn(skipped_mapper_warn_body(
                &mapper, &unproven,
            )));
            acc.cleanup_uncertain = true;
        }
    }
}

/// Classify umount EBUSY from libmount's diagnostic segment.
/// Exit 32 is generic for umount syscall failures, so the lock hint must
/// not rely on exit status alone.
fn umount_stderr_is_busy(stderr: &str) -> bool {
    // util-linux prints `"<target>: <diagnostic>."`; with LC_ALL=C, EBUSY's
    // diagnostic is exactly "target is busy". Match the segment ending so a
    // path containing that phrase does not false-positive.
    let s = stderr.trim().trim_end_matches('.');
    s == "target is busy" || s.ends_with(": target is busy")
}

/// Centralize the umount failure message and lsof/fuser hint so retry
/// exhaustion and non-busy failures preserve one operator-facing contract.
fn build_umount_error(mount_point: &MountPoint, exit_status: i32, stderr: &str) -> LockError {
    let mut msg = format!("umount {mount_point} failed (exit {exit_status}): {stderr}");
    if umount_stderr_is_busy(stderr) {
        msg.push_str(&format!(
            "\nhint: a process may be using files on the mount. \
             Run 'lsof {mount_point}' or 'fuser -vm {mount_point}' to identify it."
        ));
    }
    LockError::Failed(msg)
}

/// Retry transient EBUSY from the kernel-side file-descriptor release race
/// after lifecycle consumers such as SMB/NFS have already stopped.
fn umount_with_retry<R, S>(
    runner: &R,
    sleeper: &S,
    mount_point: &MountPoint,
    color_enabled: bool,
    attempts: u32,
) -> Result<(), LockError>
where
    R: CommandRunner,
    S: Sleeper + ?Sized,
{
    for attempt in 1..=attempts {
        let result = runner.run(&CmdRequest::Umount {
            mount_point: mount_point.clone(),
        })?;
        if result.exit_status == 0 {
            return Ok(());
        }
        let stderr = result.stderr.trim();
        if !umount_stderr_is_busy(stderr) {
            return Err(build_umount_error(mount_point, result.exit_status, stderr));
        }
        if attempt == attempts {
            return Err(build_umount_error(mount_point, result.exit_status, stderr));
        }
        emit_status(&status_line(
            StatusTag::Warn,
            color_enabled,
            &format!("umount {mount_point} busy, retrying ({attempt}/{attempts})..."),
        ));
        sleeper.sleep(UMOUNT_RETRY_DELAY);
    }
    unreachable!()
}

/// Shared close-and-aggregate state for the membership and orphan
/// loop in `LockPlan::execute`. Bundles the loop-invariant inputs
/// (runner, sleeper, color, umount-busy suppression flag) with the
/// `&mut first_mapper_error` accumulator so status formatting and
/// error-aggregation cannot drift by close kind.
struct CloseMapperCtx<'a, R, S>
where
    R: CommandRunner,
    S: Sleeper,
{
    runner: &'a R,
    sleeper: &'a S,
    color_enabled: bool,
    umount_error: &'a Option<LockError>,
    first_mapper_error: &'a mut Option<LockError>,
}

impl<R, S> CloseMapperCtx<'_, R, S>
where
    R: CommandRunner,
    S: Sleeper,
{
    fn close_one(&mut self, mapper: &MapperName, disk_label: &str, is_orphan: bool) {
        let color_enabled = self.color_enabled;
        let line = |t, body: &str| status_line(t, color_enabled, body);
        let paren = if is_orphan { " (orphan)" } else { "" };

        emit_status(&line(
            StatusTag::Wait,
            &format!("disk {disk_label}: locking{paren}..."),
        ));
        match close_mapper_with_retry(self.runner, self.sleeper, mapper, color_enabled) {
            Ok(()) => {
                emit_status(&line(
                    StatusTag::Ok,
                    &format!("disk {disk_label}: locked{paren}"),
                ));
            }
            Err(CloseMapperError::DeviceBusy(msg)) if self.umount_error.is_some() => {
                let phrase = if is_orphan {
                    "orphan close failed"
                } else {
                    "close failed"
                };
                emit_status(&line(
                    StatusTag::Warn,
                    &format!("disk {disk_label}: {phrase} (umount was stuck): {msg}"),
                ));
            }
            Err(e) => {
                let err = LockError::from(e);
                let prefix = if is_orphan { "orphan: " } else { "" };
                emit_status(&line(
                    StatusTag::Fail,
                    &format!("disk {disk_label}: {prefix}{err}"),
                ));
                if self.first_mapper_error.is_none() {
                    *self.first_mapper_error = Some(err);
                }
            }
        }
    }
}

/// Compile dry-run steps for lock from the same unit stops and ordered
/// close set that `execute` consumes, in execute's order, keeping forget
/// paths, dry-run steps, and real stop and close calls byte-identical on
/// identical inputs.
fn compile_lock_steps(
    pool_was_mounted: bool,
    pause_balance_before_unmount: bool,
    unit_stops: &[LockUnitStop],
    close_set: &LockCloseSet,
    mount_point: &MountPoint,
) -> Vec<Step> {
    let mut steps = Vec::new();

    if pool_was_mounted && pause_balance_before_unmount {
        steps.push(Step {
            risk: "safe",
            description: "pause btrfs balance".into(),
            commands: vec![CmdRequest::BtrfsBalancePause {
                mount_point: mount_point.clone(),
            }],
        });
    }

    for stop in unit_stops {
        steps.push(Step {
            risk: "safe",
            description: format!("stop {}", stop.unit()),
            commands: vec![CmdRequest::SystemctlStop {
                unit: stop.unit().to_owned(),
                no_block: false,
            }],
        });
    }

    if pool_was_mounted {
        steps.push(Step {
            risk: "safe",
            description: format!("unmount {}", mount_point),
            commands: vec![CmdRequest::Umount {
                mount_point: mount_point.clone(),
            }],
        });
        let forget_devs = close_set.forget_paths();
        if !forget_devs.is_empty() {
            steps.push(Step {
                risk: "safe",
                description: "btrfs device scan --forget".into(),
                commands: vec![CmdRequest::BtrfsDeviceScanForget {
                    devices: forget_devs,
                }],
            });
        }
    }

    for entry in close_set.entries() {
        let orphan_suffix = if entry.is_orphan() { " (orphan)" } else { "" };
        steps.push(Step {
            risk: "safe",
            description: format!("close LUKS mapper {}{orphan_suffix}", entry.mapper),
            commands: vec![CmdRequest::CryptsetupClose {
                mapper: entry.mapper.clone(),
            }],
        });
    }

    steps
}

/// Planner-derived member prelude source so execution never reconstructs
/// mapper names to infer absence after scan or classification uncertainty.
fn members_known_closed(
    membership: &PoolMembership,
    members_potentially_present: &HashSet<DiskName>,
    cleanup_uncertain: bool,
) -> Vec<DiskName> {
    if cleanup_uncertain {
        return Vec::new();
    }

    membership
        .iter_by_name()
        .into_iter()
        .filter_map(|(_, member)| {
            (!members_potentially_present.contains(&member.name)).then_some(member.name.clone())
        })
        .collect()
}

/// The dry-run preview source of truth for `braid lock` and the
/// close set pre-computed during planning. `fs.exists` during execute
/// is only a disappearance guard before mutating an already-planned
/// mapper.
pub struct LockPlan {
    pub notes: Vec<PreviewNote>,
    pub pool_was_mounted: bool,
    pause_balance_before_unmount: bool,
    umount_retry_attempts: u32,
    /// Ordered unit stops consumed by preview and execute; empty without
    /// `systemd_lifecycle`.
    unit_stops: Vec<LockUnitStop>,
    /// Ordered mapper closes consumed by preview, forget, and execute.
    pub close_set: LockCloseSet,
    /// Planner-derived members confidently absent from every observed live
    /// state; execute renders this instead of reconstructing mapper names.
    pub members_known_closed: Vec<DiskName>,
    /// True when cleanup may be incomplete even if no close step exists.
    pub cleanup_uncertain: bool,
    pub mount_point: MountPoint,
}

impl LockPlan {
    pub fn preview(&self) -> Preview {
        let mut notes = self.notes.clone();
        if self.cleanup_uncertain {
            notes.push(PreviewNote::Info(
                "cleanup incomplete: some braid mappers could not be verified".into(),
            ));
        }
        Preview {
            notes,
            steps: compile_lock_steps(
                self.pool_was_mounted,
                self.pause_balance_before_unmount,
                &self.unit_stops,
                &self.close_set,
                &self.mount_point,
            ),
        }
    }

    pub(crate) fn execute<R, F, S>(self, runner: &R, fs: &F, sleeper: &S) -> Result<(), LockError>
    where
        R: CommandRunner,
        F: Filesystem + ?Sized,
        S: Sleeper,
    {
        let color_enabled = color_enabled_for_stderr();
        let line = |t, body: &str| status_line(t, color_enabled, body);

        // Render accumulated plan notes to stderr through the shared preview
        // renderer before any mutation. Dry-run and real-run share one
        // renderer, so future Info, Skip, or PerDisk notes also surface here.
        preview::emit_notes_to_stderr(&self.notes, PerDiskStyle::Bracketed);

        let mount_point = &self.mount_point;

        // 1. Shutdown-only balance pause.
        if self.pool_was_mounted && self.pause_balance_before_unmount {
            emit_status(&line(StatusTag::Wait, "pool: pausing btrfs balance..."));
            let pause_result = runner.run(&CmdRequest::BtrfsBalancePause {
                mount_point: mount_point.clone(),
            })?;
            if pause_result.exit_status == 0 {
                emit_status(&line(StatusTag::Ok, "pool: balance paused"));
            } else if pause_result.exit_status == 2 {
                // btrfs-progs v6.19.1, cmds/balance.c
                // (cmd_balance_pause) maps only ENOTCONN to exit 2;
                // every other failure returns 1. The wording paired with
                // ENOTCONN is diagnostic, not part of this classifier.
                emit_status(&line(
                    StatusTag::Warn,
                    "pool: balance was no longer running -- continuing",
                ));
            } else {
                let stderr = pause_result.stderr.trim();
                return Err(LockError::Failed(format!(
                    "btrfs balance pause {mount_point} failed (exit {}): {stderr}",
                    pause_result.exit_status
                )));
            }
        }

        // 2. Stop the planned units right before unmount. Every refusal
        // lives in the planner, so a refused lock never stops a unit.
        run_unit_stops(
            &self.unit_stops,
            &RealOnlineStateOps::new(runner),
            &mut io::stderr(),
        );

        // 3. If mounted → unmount
        let mut umount_error: Option<LockError> = None;
        let mut first_mapper_error: Option<LockError> = None;
        if self.pool_was_mounted {
            emit_status(&line(
                StatusTag::Wait,
                &format!("pool: unmounting {mount_point}..."),
            ));
            match umount_with_retry(
                runner,
                sleeper,
                mount_point,
                color_enabled,
                self.umount_retry_attempts,
            ) {
                Ok(()) => {
                    emit_status(&line(
                        StatusTag::Ok,
                        &format!("pool: unmounted {mount_point}"),
                    ));

                    forget_existing_scanned_devices_best_effort(
                        runner,
                        fs,
                        self.close_set.forget_paths(),
                        color_enabled,
                    );
                }
                Err(err @ LockError::Cmd(_)) => return Err(err),
                Err(err) => {
                    emit_status(&line(StatusTag::Fail, &format!("{err}")));
                    emit_status(&line(
                        StatusTag::Warn,
                        "attempting to close LUKS mappers despite umount failure...",
                    ));
                    umount_error = Some(err);
                }
            }
        }

        // 4. Close each planned mapper. The membership-side prelude is
        // planner-derived so execute never infers absence from reconstructed
        // mapper names after scan or classification uncertainty.
        let mut all_already_closed = true;
        {
            let mut close_ctx = CloseMapperCtx {
                runner,
                sleeper,
                color_enabled,
                umount_error: &umount_error,
                first_mapper_error: &mut first_mapper_error,
            };
            for name in &self.members_known_closed {
                emit_status(&line(
                    StatusTag::Ok,
                    &format!("disk {name}: already closed"),
                ));
            }
            // Planned closes, observed-mapper-first.
            for entry in self.close_set.entries() {
                let mapper_path = entry.mapper.dev_path();
                if !fs.exists(&mapper_path) {
                    if entry.is_orphan() {
                        continue;
                    }
                    emit_status(&line(
                        StatusTag::Ok,
                        &format!("disk {}: already closed", entry.disk_label()),
                    ));
                    continue;
                }
                // Accepted risk: in-process member-owned close
                // double-drift -- see plan section "Accepted risk:
                // in-process member-owned close double-drift" for
                // rationale.
                close_ctx.close_one(&entry.mapper, entry.disk_label(), entry.is_orphan());
                all_already_closed = false;
            }
        }

        // 5. Return first fatal mapper error if any, otherwise deferred umount error
        if let Some(e) = first_mapper_error {
            return Err(e);
        }
        if let Some(e) = umount_error {
            return Err(e);
        }

        // 6. If nothing was done → short message
        if !self.pool_was_mounted && all_already_closed && !self.cleanup_uncertain {
            emit_status("pool already locked\n");
        }

        Ok(())
    }
}

/// Plan a `braid lock` run. Owns the mountpoint probe, preflight,
/// per-device probe (for UUID classification), close-set assembly,
/// the unit-stop list, step compilation, and any `PreviewNote::Warn`
/// notes: one per detected orphan mapper, per skipped candidate, the
/// mounted fallback warning when per-device probe failed, or a single
/// warn from a failed candidate scan. It is lock's only refusal site and
/// mutates nothing. The returned `LockPlan` is the single source of
/// truth for both `--dry-run` preview and real execution.
pub fn plan_lock<R, F>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    mode: LockMode,
) -> Result<LockPlan, LockError>
where
    R: CommandRunner,
    F: Filesystem + ?Sized,
{
    let mount_point = config.mount_point().clone();

    // 1. Check if pool is mounted
    let mp_result = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone().into(),
    })?;
    let pool_was_mounted = mp_result.exit_status == 0;

    // 2. Try per-device probe; on success take the Probed path so
    // close-set classification routes through observed UUIDs.
    // Per-device failures fall back to FSID preflight plus UUID-scanned
    // mapper cleanup; NotBtrfs aborts to preserve today's
    // mounted-non-btrfs refusal.
    let snapshot = if pool_was_mounted {
        match probe_pool(runner, fs, &mount_point) {
            Ok(pool) => Snapshot::Probed(pool),
            // Explicit per-variant routing. NotBtrfs aborts; every
            // other variant falls back to probe_fsid + UUID-scanned cleanup.
            // No catch-all -- if a future ProbeError variant lands,
            // it must opt in explicitly here so a real configuration
            // error cannot be silently masked by the ProbeFailed path.
            Err(ProbeError::NotBtrfs {
                mount_point: mp,
                fstype,
            }) => {
                return Err(LockError::Failed(format!(
                    "{mp} is mounted but fstype is {fstype}, not btrfs"
                )));
            }
            Err(
                probe_error @ (ProbeError::Cmd(_)
                | ProbeError::Parse(_)
                | ProbeError::PoolDevice { .. }
                | ProbeError::UnsupportedLuksVersion { .. }
                | ProbeError::MapperOwnership(_)
                | ProbeError::MountInfo(_)),
            ) => {
                let fsid = probe_fsid(runner, fs, &mount_point)
                    .map_err(|e| LockError::Failed(format!("cannot probe pool: {e}")))?;
                Snapshot::ProbeFailed { fsid, probe_error }
            }
        }
    } else {
        Snapshot::Unmounted
    };

    let mut acc = CloseSetAccumulator::default();
    let mut pause_balance_before_unmount = false;
    let close_set = match &snapshot {
        Snapshot::Probed(pool) => {
            if let Some(fsid) = &pool.fsid {
                pause_balance_before_unmount =
                    lock_preflight_pause_decision(fs, mode, fsid, &mut acc.notes)
                        .map_err(LockError::Failed)?;
            }
            build_close_sets_full(fs, pool, membership, &mut acc)
        }
        Snapshot::ProbeFailed { fsid, probe_error } => {
            acc.notes
                .push(PreviewNote::Warn(uuid_scanned_fallback_warn_body(
                    probe_error,
                )));
            pause_balance_before_unmount =
                lock_preflight_pause_decision(fs, mode, fsid, &mut acc.notes)
                    .map_err(LockError::Failed)?;
            build_close_sets_uuid_scanned_fallback(fs, membership, &mut acc)
        }
        Snapshot::Unmounted => build_close_sets_uuid_scanned_fallback(fs, membership, &mut acc),
    };
    let members_known_closed = members_known_closed(
        membership,
        &acc.members_potentially_present,
        acc.cleanup_uncertain,
    );
    // Resolve the unit stops last, after every refusal check above, so a
    // refused lock does not even look up consumers. The stops run in execute.
    let unit_stops = plan_unit_stops(config, &RealOnlineStateOps::new(runner));

    Ok(LockPlan {
        notes: acc.notes,
        pool_was_mounted,
        pause_balance_before_unmount,
        umount_retry_attempts: match mode {
            LockMode::User => UMOUNT_RETRY_ATTEMPTS,
            LockMode::SystemdStop => SYSTEMD_STOP_UMOUNT_RETRY_ATTEMPTS,
        },
        unit_stops,
        close_set,
        members_known_closed,
        cleanup_uncertain: acc.cleanup_uncertain,
        mount_point,
    })
}

/// Close-set construction for the `Snapshot::Probed` arm. Drives the
/// member-owned classification through observed `PoolDevice.mapper`
/// strings so the close + forget + dry-run preview all share one
/// observed-mapper source-of-truth. Pool devices with and without a backing
/// disk both carry the LUKS UUID their dm UUID records, so they classify the
/// same way; stranded `braid-*` mappers read theirs from sysfs.
fn build_close_sets_full<F: Filesystem + ?Sized>(
    fs: &F,
    pool: &PoolState,
    membership: &PoolMembership,
    acc: &mut CloseSetAccumulator,
) -> LockCloseSet {
    let mut member_owned: Vec<LockMapperClose> = Vec::new();
    let mut orphan_mappers: Vec<LockMapperClose> = Vec::new();

    // Pass 1: every probed pool device, backing disk present or not.
    let probed = pool
        .devices
        .iter()
        .map(|d| (&d.mapper, &d.luks_uuid))
        .chain(
            pool.null_underlying
                .iter()
                .map(|n| (&n.mapper, &n.luks_uuid)),
        );
    for (mapper, uuid) in probed {
        let kind = classify_by_uuid(mapper, uuid, membership);
        push_classified_close(
            mapper.clone(),
            kind,
            &mut member_owned,
            &mut orphan_mappers,
            acc,
        );
    }

    // Pass 2: stranded `braid-*` slots in /dev/mapper that did NOT
    // appear in the probed pool. Each one is classified by the LUKS UUID
    // its dm UUID records. Per-mapper failures warn and skip.
    let stranded = {
        let already_observed: HashSet<&str> = pool
            .devices
            .iter()
            .map(|d| d.mapper.as_str())
            .chain(pool.null_underlying.iter().map(|nu| nu.mapper.as_str()))
            .collect();

        match scan_braid_mapper_candidates(fs, &already_observed) {
            Ok(entries) => entries,
            Err(e) => {
                acc.notes.push(PreviewNote::Warn(mapper_scan_warn_body(&e)));
                acc.cleanup_uncertain = true;
                // Preserve best-effort semantics: with no /dev/mapper
                // listing, return what we have. Pass 1 is still valid.
                return LockCloseSet::from_classified(member_owned, orphan_mappers);
            }
        }
    };

    for mapper in stranded {
        push_uuid_classified_candidate(
            fs,
            mapper,
            membership,
            &mut member_owned,
            &mut orphan_mappers,
            acc,
        );
    }

    LockCloseSet::from_classified(member_owned, orphan_mappers)
}

/// Close-set construction for fallback cleanup. The mounted variant has
/// only the filesystem FSID (it keys the exclusive-op preflight, not an
/// ownership check), and the unmounted variant has no btrfs probe at all,
/// so every candidate must prove ownership or orphan status by the LUKS
/// UUID its dm UUID records before it enters the close set.
fn build_close_sets_uuid_scanned_fallback<F: Filesystem + ?Sized>(
    fs: &F,
    membership: &PoolMembership,
    acc: &mut CloseSetAccumulator,
) -> LockCloseSet {
    let mut member_owned: Vec<LockMapperClose> = Vec::new();
    let mut orphan_mappers: Vec<LockMapperClose> = Vec::new();
    let already_observed: HashSet<&str> = HashSet::new();
    let candidates = match scan_braid_mapper_candidates(fs, &already_observed) {
        Ok(entries) => entries,
        Err(e) => {
            acc.notes.push(PreviewNote::Warn(mapper_scan_warn_body(&e)));
            acc.cleanup_uncertain = true;
            return LockCloseSet::from_classified(member_owned, orphan_mappers);
        }
    };

    for mapper in candidates {
        push_uuid_classified_candidate(
            fs,
            mapper,
            membership,
            &mut member_owned,
            &mut orphan_mappers,
            acc,
        );
    }

    LockCloseSet::from_classified(member_owned, orphan_mappers)
}

/// The behavioral knobs that distinguish braid's lock entry points -- a user
/// dry-run/exec versus the systemd ExecStop shutdown path. Bundled so the
/// shared lock body stays under clippy's argument-count limit while the
/// load-bearing DI handles and the pool's config/membership stay explicit.
struct LockOptions {
    dry_run: bool,
    extra_notes: Vec<PreviewNote>,
    mode: LockMode,
}

/// User lock entry point; systemd ExecStop goes through `cmd_lock_systemd_stop`.
/// Runs the shared lock implementation in `LockMode::User`, so exclusive ops
/// such as a running balance block the lock instead of being paused for shutdown.
pub fn cmd_lock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    dry_run: bool,
    extra_notes: Vec<PreviewNote>,
) -> Result<(), LockError> {
    cmd_lock_impl_with_notes(
        runner,
        fs,
        &RealSleeper,
        config,
        membership,
        LockOptions {
            dry_run,
            extra_notes,
            mode: LockMode::User,
        },
    )
}

/// Systemd ExecStop lock entry point with shutdown-specific preflight.
///
/// Unlike user-initiated lock, this also permits a running balance: it pauses
/// the balance so shutdown can persist it as paused before closing LUKS.
pub fn cmd_lock_systemd_stop<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
) -> Result<(), LockError> {
    cmd_lock_impl_with_notes(
        runner,
        fs,
        &RealSleeper,
        config,
        membership,
        LockOptions {
            dry_run: false,
            extra_notes: Vec::new(),
            mode: LockMode::SystemdStop,
        },
    )
}

/// Plain lock's offline finalizer, the mirror of
/// `online_state::run_with_online_marker`: after every `cmd_lock` attempt,
/// success or failure, probe the mount once. Only an observed unmount writes
/// `done\n` and then stops `braid-online.service`, so no lock error after
/// umount leaves the unit active over an unmounted pool, and the ExecStop
/// reentry never reads `done\n` while the pool is still mounted.
pub fn cmd_lock_orchestrate<R, F, O>(
    runner: &R,
    fs: &F,
    online_ops: &O,
    config: &Config,
    membership: &PoolMembership,
    coordinator_guard: &StopCoordinatorGuard,
) -> Result<(), LockOrchestrateError>
where
    R: CommandRunner,
    F: Filesystem + ?Sized,
    O: OnlineStateOps,
{
    cmd_lock_orchestrate_impl(
        online_ops,
        config,
        || cmd_lock(runner, fs, config, membership, false, Vec::new()),
        || coordinator_guard.mark_done(),
    )
}

fn cmd_lock_orchestrate_impl<O, CL, MD>(
    online_ops: &O,
    config: &Config,
    cmd_lock_fn: CL,
    mark_done_fn: MD,
) -> Result<(), LockOrchestrateError>
where
    O: OnlineStateOps,
    CL: FnOnce() -> Result<(), LockError>,
    MD: FnOnce() -> io::Result<()>,
{
    let lock_result = cmd_lock_fn().map_err(LockOrchestrateError::CmdLock);
    let Some(unmounted) = observe_unmounted(config, online_ops) else {
        return lock_result;
    };
    if let Err(e) = mark_done_fn() {
        // Without `done\n`, a synchronous stop of braid-online would leave
        // its ExecStop reentry polling the coordinator this process holds
        // until its deadline, so skip the stop. A lock error stays the
        // reported failure; warn so the marker error is not lost.
        return match lock_result {
            Ok(()) => Err(LockOrchestrateError::MarkDone(e)),
            Err(lock_err) => {
                writeln!(
                    io::stderr(),
                    "braid: WARNING: failed to mark lock cleanup done ({e}) -- braid-online.service stays active"
                )
                .ok();
                Err(lock_err)
            }
        };
    }
    mark_offline(unmounted, config, online_ops);
    lock_result
}

#[cfg(test)]
fn cmd_lock_impl<R, F, S>(
    runner: &R,
    fs: &F,
    sleeper: &S,
    config: &Config,
    membership: &PoolMembership,
    dry_run: bool,
) -> Result<(), LockError>
where
    R: CommandRunner,
    F: Filesystem + ?Sized,
    S: Sleeper,
{
    cmd_lock_impl_with_notes(
        runner,
        fs,
        sleeper,
        config,
        membership,
        LockOptions {
            dry_run,
            extra_notes: Vec::new(),
            mode: LockMode::User,
        },
    )
}

/// Shared lock command body so dispatch-supplied diagnostics can join dry-run
/// preview notes without changing the test-facing helper arity.
fn cmd_lock_impl_with_notes<R, F, S>(
    runner: &R,
    fs: &F,
    sleeper: &S,
    config: &Config,
    membership: &PoolMembership,
    opts: LockOptions,
) -> Result<(), LockError>
where
    R: CommandRunner,
    F: Filesystem + ?Sized,
    S: Sleeper,
{
    let mut plan = plan_lock(runner, fs, config, membership, opts.mode)?;
    plan.notes.splice(0..0, opts.extra_notes);
    if opts.dry_run {
        plan.preview().print_colored();
        return Ok(());
    }
    plan.execute(runner, fs, sleeper)
}

/// Scrub units lock stops, timer first so it cannot re-trigger the service,
/// then the service, whose stop cancels an in-flight scrub.
const SCRUB_UNITS: [&str; 2] = ["braid-scrub.timer", "braid-scrub.service"];

/// A unit that lock stops right before unmount. The planner picks each entry
/// and its failure policy, so execute stops exactly the units the preview
/// showed and never looks the list up again.
#[derive(Debug, PartialEq, Eq)]
enum LockUnitStop {
    /// A scrub unit. A failed stop is silent.
    Scrub(&'static str),
    /// A `BoundBy braid-online.service` consumer such as smbd. A failed stop
    /// warns.
    BoundConsumer(String),
}

impl LockUnitStop {
    fn unit(&self) -> &str {
        match self {
            Self::Scrub(unit) => unit,
            Self::BoundConsumer(unit) => unit,
        }
    }
}

/// Resolve lock's unit stops for both plain `braid lock` and the
/// `--systemd-stop` ExecStop path: the scrub units, then each `BoundBy
/// braid-online.service` consumer. Planned unconditionally so teardown is
/// code-owned regardless of systemd's cascade ordering; decision 018 covers
/// when these ExecStop stops are no-ops vs. load-bearing. It only reads
/// systemd. A failed BoundBy lookup quietly plans no consumer stops.
fn plan_unit_stops(cfg: &Config, online_ops: &dyn OnlineStateOps) -> Vec<LockUnitStop> {
    if !cfg.systemd_lifecycle() {
        return Vec::new();
    }

    let consumers = online_ops
        .list_bound_by(BRAID_ONLINE_UNIT)
        .unwrap_or_default()
        .into_iter()
        .filter(|unit| !SCRUB_UNITS.contains(&unit.as_str()))
        .map(LockUnitStop::BoundConsumer);
    SCRUB_UNITS
        .into_iter()
        .map(LockUnitStop::Scrub)
        .chain(consumers)
        .collect()
}

/// Execute's unit-stop step: stop each planned unit in order under its
/// failure policy. A failed stop never aborts lock, because a consumer that
/// is still running surfaces as a busy umount with its own retry and hint.
fn run_unit_stops(stops: &[LockUnitStop], online_ops: &dyn OnlineStateOps, out: &mut dyn Write) {
    for stop in stops {
        match stop {
            LockUnitStop::Scrub(unit) => stop_unit_silent(online_ops, unit),
            LockUnitStop::BoundConsumer(unit) => stop_unit_warn_on_error(online_ops, out, unit),
        }
    }
}

/// Swallow scrub stop failures because autoScrub-disabled configs do not
/// define these units, and lock should not warn on every such host.
fn stop_unit_silent(online_ops: &dyn OnlineStateOps, unit: &str) {
    let _ = online_ops.systemctl_stop(unit, false);
}

/// Warn when user-declared BoundBy consumers fail to stop so operators know
/// the following umount may still be blocked by services like SMB or NFS.
fn stop_unit_warn_on_error(online_ops: &dyn OnlineStateOps, out: &mut dyn Write, unit: &str) {
    if let Err(e) = online_ops.systemctl_stop(unit, false) {
        match e {
            OnlineError::SystemctlStop {
                exit_code, stderr, ..
            } => {
                writeln!(
                    out,
                    "braid: WARNING: failed to stop {unit} (exit {exit_code}{}) -- continuing; umount may fail",
                    detail_suffix(&stderr)
                )
                .ok();
            }
            other => {
                writeln!(
                    out,
                    "braid: WARNING: failed to stop {unit} ({other}) -- continuing; umount may fail"
                )
                .ok();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::mapper_close::{CLOSE_RETRY_ATTEMPTS, CLOSE_RETRY_DELAY};
    use crate::online_state::{BRAID_ONLINE_UNIT, RecordingOnlineStateOps, StagedOnlineFailure};
    use crate::pool_lock::RealStopCoordinator;
    use crate::test_fixtures::{
        LockNoopSleeper, LockRecordingRunner, assert_lines_in_order, disk_member, line_index,
        lock_count_forget_steps, lock_err_raw, lock_forget_step_devices, lock_fs, lock_member_fs,
        lock_mounted_runner, lock_ok_raw, lock_test_config, lock_test_membership,
        lock_umount_failed_runner, lock_with_fsid_probe_mocks, test_uuid,
    };
    use crate::types::Devid;
    use std::sync::Mutex;
    use std::time::Duration;

    fn cryptsetup_status_active(mapper: &str, device: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup status {mapper}"),
            stdout: format!(
                "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {device}\n  mode:    read/write\n"
            ),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    const AAA_UUID: &str = "00000000-0000-0000-0000-0000000002bc";
    const BBB_UUID: &str = "00000000-0000-0000-0000-0000000002bd";
    const ORPHAN_UUID: &str = "00000000-0000-0000-0000-0000000002ff";

    fn lock_test_membership_with_ccc() -> PoolMembership {
        let mut membership = lock_test_membership();
        let (uuid, member) = disk_member(702, "ccc", "/dev/disk/by-id/c");
        membership.insert(uuid, member).unwrap();
        membership
    }

    /// Bodies of every `Warn` note, in order; any other note kind fails.
    fn warn_bodies(notes: &[PreviewNote]) -> Vec<&str> {
        notes
            .iter()
            .map(|note| match note {
                PreviewNote::Warn(body) => body.as_str(),
                other => panic!("expected only Warn notes, got {other:?}"),
            })
            .collect()
    }

    fn known_closed_names(plan: &LockPlan) -> Vec<&str> {
        plan.members_known_closed
            .iter()
            .map(DiskName::as_str)
            .collect()
    }

    fn umount_request() -> CmdRequest {
        CmdRequest::Umount {
            mount_point: MountPoint::new("/mnt/storage".to_owned()),
        }
    }

    fn umount_busy_output() -> RawCommandOutput {
        lock_err_raw("umount /mnt/storage", 32, "target is busy")
    }

    fn umount_request_count(runner: &MockRunner) -> usize {
        runner
            .requests()
            .iter()
            .filter(|request| matches!(request, CmdRequest::Umount { .. }))
            .count()
    }

    fn cryptsetup_close_request_count(runner: &MockRunner) -> usize {
        runner
            .requests()
            .iter()
            .filter(|request| matches!(request, CmdRequest::CryptsetupClose { .. }))
            .count()
    }

    fn balance_pause_request_count(runner: &MockRunner) -> usize {
        runner
            .requests()
            .iter()
            .filter(|request| matches!(request, CmdRequest::BtrfsBalancePause { .. }))
            .count()
    }

    fn forget_requests(runner: &MockRunner) -> Vec<Vec<String>> {
        runner
            .requests()
            .into_iter()
            .filter_map(|request| match request {
                CmdRequest::BtrfsDeviceScanForget { devices } => Some(devices),
                _ => None,
            })
            .collect()
    }

    fn btrfs_show_for_lock_mappers(mapper_aaa: &str, mapper_bbb: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "btrfs filesystem show /mnt/storage".into(),
            stdout: format!(
                "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                 \tTotal devices 2 FS bytes used 16.00MiB\n\
                 \tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/{mapper_aaa}\n\
                 \tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/{mapper_bbb}\n"
            ),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn mounted_systemd_stop_runner() -> MockRunner {
        lock_mounted_runner()
            .with_output(
                CmdRequest::BtrfsBalancePause {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                lock_ok_raw("btrfs balance pause /mnt/storage"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            )
    }

    fn cfg(raw: &str) -> Config {
        serde_json::from_str(raw).expect("config should parse")
    }

    fn lifecycle_config() -> Config {
        cfg(r#"{"mount_point":"/mnt/storage","systemd_lifecycle":true}"#)
    }

    fn offline_lifecycle_ops() -> RecordingOnlineStateOps {
        let ops = RecordingOnlineStateOps::new();
        ops.set_mounted(false);
        ops
    }

    fn stop_online_call() -> String {
        format!("stop {BRAID_ONLINE_UNIT} no_block=false")
    }

    fn synthetic_lock_failure() -> Result<(), LockError> {
        Err(LockError::DeviceBusy("synthetic close failure".into()))
    }

    // Intent: a lock that fails after the pool is unmounted still writes
    //   `done\n` and then stops braid-online.service once, and returns the
    //   lock error.
    // Why it exists: lock stops every bound consumer and the scrub timer
    //   before umount. If braid-online stayed active over the unmounted pool,
    //   the next unlock would see it active and start nothing, leaving shares
    //   and scrub off.
    // Scenario: `braid lock` unmounts the pool, then a busy mapper makes
    //   `cryptsetup close` fail.
    #[test]
    fn cmd_lock_failure_after_unmount_writes_done_then_stops_online() {
        let tmp = tempfile::tempdir().unwrap();
        let coord_path = tmp.path().join("coord");
        let coordinator = RealStopCoordinator::new(coord_path.clone());
        let coordinator_guard = coordinator.acquire().unwrap();
        let ops = RecordingOnlineStateOps::new().with_coord_file(coord_path.clone());
        ops.set_mounted(false);
        let config = lifecycle_config();

        let result = cmd_lock_orchestrate_impl(&ops, &config, synthetic_lock_failure, || {
            coordinator_guard.mark_done()
        });

        assert!(matches!(
            result,
            Err(LockOrchestrateError::CmdLock(LockError::DeviceBusy(_)))
        ));
        let calls = ops.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == &stop_online_call())
                .count(),
            1,
            "expected exactly one braid-online stop call, got {calls:?}"
        );
        assert_eq!(ops.coord_snapshots(), vec![b"done\n".to_vec()]);
    }

    // Intent: a lock that fails while the pool is still mounted writes no
    //   `done\n` and does not stop braid-online.service.
    // Why it exists: ExecStop reentry treats `done\n` as authoritative, so a
    //   marker over a mounted pool would let shutdown exit 0 with LUKS open.
    // Scenario: plain `braid lock` fails its umount while a concurrent
    //   ExecStop path is waiting on the coordinator file.
    #[test]
    fn cmd_lock_failure_while_mounted_writes_no_done_and_keeps_online() {
        let ops = RecordingOnlineStateOps::new();
        ops.set_mounted(true);
        let config = lifecycle_config();

        let result = cmd_lock_orchestrate_impl(
            &ops,
            &config,
            synthetic_lock_failure,
            || -> io::Result<()> { panic!("mark_done must not run while the pool is mounted") },
        );

        assert!(matches!(result, Err(LockOrchestrateError::CmdLock(_))));
        assert!(!ops.calls().contains(&stop_online_call()));
    }

    // Intent: when the post-lock mountpoint check fails, plain lock writes no
    //   `done\n` and does not stop braid-online.service, even after a lock
    //   that succeeded.
    // Why it exists: the probe runs before the marker, so `done\n` is only
    //   ever written over an observed unmount. An unknown mount state counts
    //   as still mounted.
    // Scenario: plain `braid lock` finishes, then the mountpoint check
    //   returns a Spawn error.
    #[test]
    fn mountpoint_check_failure_writes_no_done_and_keeps_online() {
        let ops = RecordingOnlineStateOps::new();
        ops.set_mountpoint_err(StagedOnlineFailure::Spawn(
            "mountpoint spawn failure".into(),
        ));
        let config = lifecycle_config();

        let result = cmd_lock_orchestrate_impl(
            &ops,
            &config,
            || Ok(()),
            || -> io::Result<()> {
                panic!("mark_done must not run when the mount state is unknown")
            },
        );

        assert!(result.is_ok());
        assert!(!ops.calls().contains(&stop_online_call()));
    }

    // Intent: cmd_lock_orchestrate writes `done\n` before mark_offline runs.
    // Why it exists: ExecStop reentry must never observe braid-online.service
    // stopping before the coordinator marker proves plain lock cleanup is done.
    // Scenario: plain `braid lock` succeeds and then deactivates the module's
    // online unit for the now-offline pool.
    #[test]
    fn cmd_lock_success_writes_done_then_calls_mark_offline_in_order() {
        let tmp = tempfile::tempdir().unwrap();
        let coord_path = tmp.path().join("coord");
        let coordinator = RealStopCoordinator::new(coord_path.clone());
        let coordinator_guard = coordinator.acquire().unwrap();
        let ops = RecordingOnlineStateOps::new().with_coord_file(coord_path.clone());
        ops.set_mounted(false);
        let config = lifecycle_config();

        let result =
            cmd_lock_orchestrate_impl(&ops, &config, || Ok(()), || coordinator_guard.mark_done());

        assert!(result.is_ok());
        let calls = ops.calls();
        assert_eq!(
            calls
                .iter()
                .filter(|call| *call == &stop_online_call())
                .count(),
            1,
            "expected exactly one braid-online stop call, got {calls:?}"
        );
        assert_eq!(ops.coord_snapshots(), vec![b"done\n".to_vec()]);
        assert_eq!(std::fs::read(coord_path).unwrap(), b"done\n");
    }

    // Intent: cmd_lock_orchestrate must not stop braid-online.service after
    //   mark_done fails. A lock error stays the reported failure; otherwise
    //   the marker error is.
    // Why it exists: a synchronous stop without `done\n` would leave the
    //   ExecStop reentry polling a coordinator plain lock still holds.
    // Scenario: plain `braid lock` unmounts the pool but cannot write the
    //   stop-coordinator done marker due to an I/O error, once after a lock
    //   that succeeded and once after one that failed a close.
    #[test]
    fn mark_done_failure_does_not_call_mark_offline() {
        let config = lifecycle_config();
        let mark_done_failure = || Err(io::Error::other("synthetic mark_done failure"));

        let ops = offline_lifecycle_ops();
        let result = cmd_lock_orchestrate_impl(&ops, &config, || Ok(()), mark_done_failure);
        assert!(matches!(result, Err(LockOrchestrateError::MarkDone(_))));
        assert!(!ops.calls().contains(&stop_online_call()));

        let ops = offline_lifecycle_ops();
        let result =
            cmd_lock_orchestrate_impl(&ops, &config, synthetic_lock_failure, mark_done_failure);
        assert!(matches!(result, Err(LockOrchestrateError::CmdLock(_))));
        assert!(!ops.calls().contains(&stop_online_call()));
    }

    fn mounted_runner_with_btrfs_show(mapper_aaa: &str, mapper_bbb: &str) -> MockRunner {
        MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint::new("/mnt/storage".to_owned()).into(),
                },
                lock_ok_raw("mountpoint -q /mnt/storage"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                btrfs_show_for_lock_mappers(mapper_aaa, mapper_bbb),
            )
            .with_mapper_open(mapper_aaa, "/dev/disk/by-id/a", AAA_UUID)
            .with_mapper_open(mapper_bbb, "/dev/disk/by-id/b", BBB_UUID)
    }

    fn bound_by_output(units: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "systemctl show -P BoundBy braid-online.service".into(),
            stdout: format!("{units}\n"),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn stop_request(unit: &str) -> CmdRequest {
        CmdRequest::SystemctlStop {
            unit: unit.into(),
            no_block: false,
        }
    }

    fn bound_by_request() -> CmdRequest {
        CmdRequest::SystemctlShowBoundBy {
            unit: BRAID_ONLINE_UNIT.into(),
        }
    }

    /// The stop list `plan_unit_stops` yields for the scrub units plus smbd.
    fn scrub_and_smbd_stops() -> Vec<LockUnitStop> {
        vec![
            LockUnitStop::Scrub("braid-scrub.timer"),
            LockUnitStop::Scrub("braid-scrub.service"),
            LockUnitStop::BoundConsumer("smbd.service".into()),
        ]
    }

    // Intent: cmd_lock plans and runs no unit stops without the
    //   systemd_lifecycle capability.
    // Why it exists: standalone CLI installs do not define braid-online or
    //   scrub units, so lock must not spawn systemctl for them.
    // Scenario: CLI-only host locks an already-unmounted pool.
    #[test]
    fn cmd_lock_skips_unit_stops_when_lifecycle_disabled() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = lock_fs(&[]);
        let config = cfg(r#"{"mount_point":"/mnt/storage"}"#);
        let membership = lock_test_membership();

        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed");

        let requests = runner.requests();
        assert!(
            !requests.iter().any(|request| matches!(
                request,
                CmdRequest::SystemctlStop { .. } | CmdRequest::SystemctlShowBoundBy { .. }
            )),
            "unexpected systemctl request: {requests:?}"
        );
    }

    // Intent: on a lifecycle host, a real lock stops the scrub timer, the
    //   scrub service, then each BoundBy consumer, after planning and right
    //   before umount.
    // Why it exists: module-managed lock must stop scrub and the shares that
    //   hold the mount before it unmounts, and only once the planner has
    //   decided not to refuse.
    // Scenario: smbd.service is bound to braid-online.service and the
    //   operator runs `braid lock` on a mounted pool.
    #[test]
    fn cmd_lock_stops_units_after_planning_and_before_umount() {
        let runner = lock_mounted_runner()
            .with_output(bound_by_request(), bound_by_output("smbd.service"))
            .with_output(
                stop_request("braid-scrub.timer"),
                lock_ok_raw("systemctl stop braid-scrub.timer"),
            )
            .with_output(
                stop_request("braid-scrub.service"),
                lock_ok_raw("systemctl stop braid-scrub.service"),
            )
            .with_output(
                stop_request("smbd.service"),
                lock_ok_raw("systemctl stop smbd.service"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lifecycle_config();
        let membership = lock_test_membership();

        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed");

        let requests = runner.requests();
        assert!(
            matches!(requests.first(), Some(CmdRequest::MountpointCheck { .. })),
            "planning must come first: {requests:?}"
        );
        let lifecycle: Vec<&CmdRequest> = requests
            .iter()
            .filter(|request| {
                matches!(
                    request,
                    CmdRequest::SystemctlShowBoundBy { .. }
                        | CmdRequest::SystemctlStop { .. }
                        | CmdRequest::Umount { .. }
                )
            })
            .collect();
        assert_eq!(
            lifecycle,
            vec![
                &bound_by_request(),
                &stop_request("braid-scrub.timer"),
                &stop_request("braid-scrub.service"),
                &stop_request("smbd.service"),
                &umount_request(),
            ]
        );
    }

    // Intent: a lock that refuses stops no unit and does not even look up
    //   BoundBy consumers; every refusal comes before any unit stop.
    // Why it exists: lock used to stop the scrub units and every BoundBy
    //   consumer before plan_lock ran its refusal checks. A refusal then left
    //   the pool mounted and braid-online.service active with shares and the
    //   scrub timer down, and `braid unlock` could not bring them back.
    // Scenario: on a module install, a balance is running. The operator runs
    //   `braid lock`, which refuses on that exclusive op.
    #[test]
    fn refused_lock_stops_no_units() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ));
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_excl_op("balance");
        let config = lifecycle_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should refuse -- balance is running");

        assert!(
            err.to_string().contains("in progress"),
            "expected running-balance refusal, got: {err}"
        );
        let requests = runner.requests();
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, CmdRequest::SystemctlStop { .. })),
            "a refused lock must stop no unit: {requests:?}"
        );
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, CmdRequest::SystemctlShowBoundBy { .. })),
            "a refused lock must not look up BoundBy consumers: {requests:?}"
        );
    }

    // Intent: on a lifecycle host, the `braid lock --dry-run` preview shows a
    //   stop step per planned unit, before the unmount, and planning stops
    //   nothing.
    // Why it exists: the stops used to run outside the plan, so the preview
    //   never showed that lock would take shares down, breaking ADR 022's
    //   rule that the preview shows the plan execute consumes.
    // Scenario: smbd.service is bound to braid-online.service and the
    //   operator previews `braid lock --dry-run` on a mounted pool.
    #[test]
    fn dry_run_preview_shows_unit_stops_before_umount() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(bound_by_request(), bound_by_output("smbd.service"));
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lifecycle_config();

        let plan = plan_lock(
            &runner,
            &fs,
            &config,
            &lock_test_membership(),
            LockMode::User,
        )
        .expect("plan_lock should succeed on mounted pool");
        let output = plan.preview().render();

        assert_lines_in_order(
            &output,
            &[
                "[safe] stop braid-scrub.timer",
                "$ systemctl stop braid-scrub.timer",
                "[safe] stop braid-scrub.service",
                "$ systemctl stop braid-scrub.service",
                "[safe] stop smbd.service",
                "$ systemctl stop smbd.service",
                "[safe] unmount /mnt/storage",
            ],
        );
        let requests = runner.requests();
        assert!(
            !requests
                .iter()
                .any(|request| matches!(request, CmdRequest::SystemctlStop { .. })),
            "planning must stop no unit: {requests:?}"
        );
    }

    // Intent: plan_unit_stops plans the scrub units, then each BoundBy
    //   consumer other than the scrub units, and only reads systemd to do it.
    // Why it exists: the scrub units carry their own silent stop policy and
    //   must not be stopped twice, and the planner may not stop anything.
    // Scenario: braid-online.service has the scrub units plus SMB/NFS
    //   consumers bound to it while lock plans.
    #[test]
    fn plan_unit_stops_skips_scrub_units_in_bound_by() {
        let config = lifecycle_config();
        let ops = RecordingOnlineStateOps::new();
        ops.set_bound_by_ok(vec![
            "braid-scrub.timer".into(),
            "braid-scrub.service".into(),
            "smbd.service".into(),
            "nfs-server.service".into(),
        ]);

        let stops = plan_unit_stops(&config, &ops);

        assert_eq!(
            stops,
            vec![
                LockUnitStop::Scrub("braid-scrub.timer"),
                LockUnitStop::Scrub("braid-scrub.service"),
                LockUnitStop::BoundConsumer("smbd.service".into()),
                LockUnitStop::BoundConsumer("nfs-server.service".into()),
            ]
        );
        assert_eq!(ops.calls(), vec!["list_bound_by braid-online.service"]);
    }

    // Intent: a failed BoundBy lookup still plans the scrub stops and plans
    //   no consumer stop, without failing the lock.
    // Why it exists: the BoundBy lookup is best-effort; a failed systemctl
    //   show must not block lock or abort the scrub stops.
    // Scenario: systemctl cannot read braid-online.service BoundBy while lock
    //   plans.
    #[test]
    fn plan_unit_stops_keeps_scrub_stops_when_bound_by_lookup_fails() {
        let config = lifecycle_config();
        let ops = RecordingOnlineStateOps::new();
        ops.set_bound_by_err(StagedOnlineFailure::SystemctlShow {
            unit: "braid-online.service".into(),
            exit_code: 1,
            stderr: String::new(),
        });

        let stops = plan_unit_stops(&config, &ops);

        assert_eq!(
            stops,
            vec![
                LockUnitStop::Scrub("braid-scrub.timer"),
                LockUnitStop::Scrub("braid-scrub.service"),
            ]
        );
    }

    // Intent: run_unit_stops reports the exit code and stderr from a nonzero
    //   consumer stop and goes on.
    // Why it exists: operators need systemctl's diagnostic when a BoundBy
    //   consumer may still hold the mount busy.
    // Scenario: smbd.service is bound to braid-online.service but systemctl
    //   stop returns a nonzero status during lock.
    #[test]
    fn unit_stop_warns_on_nonzero_consumer_stop() {
        let ops = RecordingOnlineStateOps::new();
        ops.set_systemctl_stop_err(
            "smbd.service",
            StagedOnlineFailure::SystemctlStop {
                unit: "smbd.service".into(),
                exit_code: 5,
                stderr: "stop failed".into(),
            },
        );
        let mut out = Vec::new();

        run_unit_stops(&scrub_and_smbd_stops(), &ops, &mut out);

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "braid: WARNING: failed to stop smbd.service (exit 5: stop failed) -- continuing; umount may fail\n"
        );
        assert_eq!(
            ops.calls(),
            vec![
                "stop braid-scrub.timer no_block=false",
                "stop braid-scrub.service no_block=false",
                "stop smbd.service no_block=false",
            ]
        );
    }

    // Intent: a nonzero consumer stop with empty stderr omits the diagnostic
    // separator while retaining the exit code and operational consequence.
    // Why it exists: systemctl may fail silently, and warning formatting must
    // not imply that diagnostic text was accidentally truncated.
    // Scenario: smbd.service remains active but systemctl stop emits no text.
    #[test]
    fn unit_stop_warning_omits_empty_stderr_suffix() {
        let ops = RecordingOnlineStateOps::new();
        ops.set_systemctl_stop_err(
            "smbd.service",
            StagedOnlineFailure::SystemctlStop {
                unit: "smbd.service".into(),
                exit_code: 5,
                stderr: String::new(),
            },
        );
        let mut out = Vec::new();

        run_unit_stops(&scrub_and_smbd_stops(), &ops, &mut out);

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "braid: WARNING: failed to stop smbd.service (exit 5) -- continuing; umount may fail\n"
        );
    }

    // Intent: run_unit_stops warns byte-exactly for generic stop errors.
    // Why it exists: the Display form preserves command spawn failure detail
    // rather than collapsing every failure into an exit-code message.
    // Scenario: smbd.service is bound to braid-online.service but spawning
    // systemctl fails before an exit status exists.
    #[test]
    fn unit_stop_warns_on_consumer_spawn_error() {
        let ops = RecordingOnlineStateOps::new();
        ops.set_systemctl_stop_err("smbd.service", StagedOnlineFailure::Spawn("boom".into()));
        let mut out = Vec::new();

        run_unit_stops(&scrub_and_smbd_stops(), &ops, &mut out);

        assert_eq!(
            String::from_utf8(out).unwrap(),
            "braid: WARNING: failed to stop smbd.service (command failed: boom) -- continuing; umount may fail\n"
        );
    }

    // Intent: run_unit_stops swallows scrub-unit stop failures and still
    //   stops the consumers after them.
    // Why it exists: autoScrub-disabled module configs may not define scrub
    //   units, while BoundBy consumers still need stopping before unmount.
    // Scenario: braid-scrub.timer is absent, but smbd.service is bound to
    //   braid-online.service and still needs to be stopped before unmount.
    #[test]
    fn unit_stop_swallows_missing_scrub_unit() {
        let ops = RecordingOnlineStateOps::new();
        ops.set_systemctl_stop_err(
            "braid-scrub.timer",
            StagedOnlineFailure::SystemctlStop {
                unit: "braid-scrub.timer".into(),
                exit_code: 5,
                stderr: "Unit braid-scrub.timer not loaded.".into(),
            },
        );
        let mut out = Vec::new();

        run_unit_stops(&scrub_and_smbd_stops(), &ops, &mut out);

        assert_eq!(String::from_utf8(out).unwrap(), "");
        assert_eq!(
            ops.calls(),
            vec![
                "stop braid-scrub.timer no_block=false",
                "stop braid-scrub.service no_block=false",
                "stop smbd.service no_block=false",
            ]
        );
    }

    fn cryptsetup_status_active_null(mapper: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup status {mapper}"),
            stdout: format!(
                "/dev/mapper/{mapper} is active and is in use.\n\
                 \ttype:    LUKS2\n\
                 \tdevice:  (null)\n"
            ),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    // Intent: planner-derived members_known_closed lists members in
    //   DiskName order regardless of underlying UUID order.
    // Why it exists: the executor prelude consumes this field directly,
    //   so order must be pinned at the planner source of truth.
    // Scenario: a two-disk pool where UUID order is opposite name order;
    //   no live mappers are observed, so both members are confidently closed.
    #[test]
    fn members_known_closed_returned_in_name_order_independent_of_uuid_order() {
        let mut membership = PoolMembership::empty();
        let (_, zeta) = disk_member(700, "zeta", "/dev/disk/by-id/ata-Z");
        let (_, alpha) = disk_member(701, "alpha", "/dev/disk/by-id/ata-A");
        membership.insert(test_uuid(700), zeta).unwrap();
        membership.insert(test_uuid(701), alpha).unwrap();
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = lock_fs(&[]);
        let config = lock_test_config();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::User)
            .expect("plan should succeed");

        assert_eq!(known_closed_names(&plan), vec!["alpha", "zeta"]);
    }

    #[test]
    fn lock_happy_path_unmounts_and_closes() {
        let runner = lock_mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed");
    }

    // Intent: LockPlan::execute honors the planned open_mappers set
    //   and does not close a membership mapper that appeared in
    //   /dev/mapper only after planning.
    // Why it exists: closing a mapper that was not in the plan reopens
    //   the cryptsetup-close-btrfs-held race because the forget call's
    //   argv is plan-derived.
    // Scenario: plan_lock runs while the pool is unmounted and
    //   braid-aaa is closed; between plan and execute braid-aaa
    //   reappears. Execute must not issue CryptsetupClose for that
    //   unplanned mapper.
    #[test]
    fn execute_does_not_close_membership_mapper_absent_from_plan() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".into()).into(),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let plan_fs = lock_fs(&[]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let plan = plan_lock(&runner, &plan_fs, &config, &membership, LockMode::User)
            .expect("plan_lock should succeed");
        assert!(
            plan.close_set.is_empty(),
            "precondition: plan should record no membership opens"
        );

        let execute_fs = lock_fs(&["/dev/mapper/braid-aaa"]);
        let recording = LockRecordingRunner::new(runner);
        plan.execute(&recording, &execute_fs, &LockNoopSleeper)
            .expect("execute should succeed without closing the unplanned mapper");

        assert!(
            recording.close_calls().is_empty(),
            "execute must not close mappers absent from member_owned; got {:?}",
            recording.close_calls()
        );
        assert!(
            recording.forget_calls().is_empty(),
            "execute must not invoke forget when member_owned is empty; got {:?}",
            recording.forget_calls()
        );
    }

    // Intent: lock reports the documented terminal line when the pool is
    //   already unmounted and every mapper is already closed.
    // Why it exists: the terminal line is plain, not tagged, and should stay
    //   observable through the fast capture sink.
    // Scenario: a user runs `braid lock` against an already-offline pool.
    #[test]
    fn lock_already_locked() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = lock_fs(&[]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_lock_impl(
                &runner,
                &fs,
                &LockNoopSleeper,
                &config,
                &membership,
                false,
            ));
        });

        result
            .expect("lock result should be captured")
            .expect("lock should succeed (already locked)");
        assert_eq!(captured.lines().last(), Some("pool already locked"));
    }

    // Intent: lock suppresses the already-locked terminal line when it had to
    //   close a real mapper, while still emitting close progress rows.
    // Why it exists: the all-already-closed guard and the close progress rows
    //   must both stay visible to fast capture tests.
    // Scenario: the pool is unmounted, but disk `aaa` still has an open mapper.
    #[test]
    fn lock_partial_state() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint::new("/mnt/storage".to_owned()).into(),
                },
                lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            );
        let fs = lock_fs(&["/dev/mapper/braid-aaa"]).with_luks_dm("braid-aaa", AAA_UUID);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_lock_impl(
                &runner,
                &fs,
                &LockNoopSleeper,
                &config,
                &membership,
                false,
            ));
        });

        result
            .expect("lock result should be captured")
            .expect("lock should succeed (partial)");
        assert!(!captured.contains("pool already locked"));
        assert!(captured.contains("[wait] disk aaa: locking..."));
        assert!(captured.contains("[ok]   disk aaa: locked"));
    }

    // Intent: lock fails when umount reports the mount is busy.
    // Why it exists: a busy mount means the pool cannot be cleanly locked;
    //   reporting success would be a lie.
    // Scenario: a process holds a file open on /mnt/storage; umount returns
    //   "target is busy". lock still attempts mapper close (best-effort), but
    //   ultimately returns the umount error.
    #[test]
    fn lock_umount_busy_fails() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output_sequence(
            umount_request(),
            vec![
                umount_busy_output(),
                umount_busy_output(),
                umount_busy_output(),
            ],
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_err_raw(
                "cryptsetup close braid-aaa",
                5,
                "Device braid-aaa is still in use.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-bbb".into()),
            },
            lock_err_raw(
                "cryptsetup close braid-bbb",
                5,
                "Device braid-bbb is still in use.",
            ),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail on busy");
        assert!(err.to_string().contains("target is busy"));
        assert_eq!(
            umount_request_count(&runner),
            UMOUNT_RETRY_ATTEMPTS as usize,
            "busy umount should exhaust retry attempts"
        );
    }

    // Intent: a failed unmount skips the BtrfsDeviceScanForget request and
    //   proceeds straight to mapper close.
    // Why it exists: the lock cookbook documents this contract; without a
    //   pin, a refactor that always called forget would only surface a runtime
    //   warn because the forget error path is non-fatal.
    // Scenario: umount fails three times with "target is busy"; lock issues
    //   zero forget requests, still attempts each member mapper close, and
    //   returns the umount error.
    #[test]
    fn lock_umount_failure_skips_forget() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output_sequence(
            umount_request(),
            vec![
                umount_busy_output(),
                umount_busy_output(),
                umount_busy_output(),
            ],
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_ok_raw("cryptsetup close braid-aaa"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-bbb".into()),
            },
            lock_ok_raw("cryptsetup close braid-bbb"),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("umount failure should be reported after best-effort close");

        assert!(
            err.to_string().contains("target is busy"),
            "expected umount stderr in error, got: {err}"
        );
        assert!(
            forget_requests(&runner).is_empty(),
            "umount failure must not issue btrfs forget"
        );
        assert_eq!(
            cryptsetup_close_request_count(&runner),
            2,
            "umount failure should still attempt each member mapper close"
        );
    }

    // Intent: the umount-busy error message includes actionable diagnostic hints.
    // Why it exists: users need to know how to find the blocking process so
    //   they can kill it and retry lock.
    // Scenario: umount fails with "target is busy"; the error message suggests
    //   running lsof or fuser to identify the holder.
    #[test]
    fn lock_umount_busy_includes_hint() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output_sequence(
            umount_request(),
            vec![
                umount_busy_output(),
                umount_busy_output(),
                umount_busy_output(),
            ],
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_err_raw(
                "cryptsetup close braid-aaa",
                5,
                "Device braid-aaa is still in use.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-bbb".into()),
            },
            lock_err_raw(
                "cryptsetup close braid-bbb",
                5,
                "Device braid-bbb is still in use.",
            ),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail on busy");
        let msg = err.to_string();
        assert!(
            msg.contains("lsof") && msg.contains("fuser"),
            "expected lsof/fuser hint in error, got: {msg}"
        );
    }

    // Intent: transient umount EBUSY retries and succeeds when a later attempt
    //   observes the kernel-side holder release.
    // Why it exists: SMB/NFS lifecycle consumers can stop successfully while
    //   the kernel has not yet released the last file descriptors, so one
    //   immediate umount failure must not abort an otherwise clean lock.
    // Scenario: first umount returns "target is busy", the second succeeds,
    //   then lock forgets btrfs scan state and closes both member mappers.
    #[test]
    fn lock_umount_busy_retry_succeeds_on_second_attempt() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output_sequence(
            umount_request(),
            vec![umount_busy_output(), lock_ok_raw("umount /mnt/storage")],
        )
        .with_output(
            CmdRequest::BtrfsDeviceScanForget {
                devices: vec![
                    "/dev/mapper/braid-aaa".into(),
                    "/dev/mapper/braid-bbb".into(),
                ],
            },
            lock_ok_raw("btrfs device scan --forget"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_ok_raw("cryptsetup close braid-aaa"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-bbb".into()),
            },
            lock_ok_raw("cryptsetup close braid-bbb"),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_lock_impl(
                &runner,
                &fs,
                &LockNoopSleeper,
                &config,
                &membership,
                false,
            ));
        });

        result
            .expect("lock result should be captured")
            .expect("lock should succeed after transient umount busy");
        assert_eq!(
            umount_request_count(&runner),
            2,
            "second umount attempt should succeed"
        );
        assert_eq!(
            cryptsetup_close_request_count(&runner),
            2,
            "lock should close both member mappers after successful umount retry"
        );
        let warn = "[warn] umount /mnt/storage busy, retrying (1/3)...";
        assert!(
            captured.contains(warn),
            "missing umount retry warning {warn:?} in {captured:?}"
        );
        assert!(
            captured.contains("[ok]   pool: unmounted /mnt/storage"),
            "missing unmount success row in {captured:?}"
        );
    }

    // Intent: non-busy umount failures fail without retrying.
    // Why it exists: exit 32 is generic for libmount syscall failures; retry
    //   must be reserved for the EBUSY diagnostic so unrelated failures are
    //   not delayed or mislabeled as transient holders.
    // Scenario: umount reports "device not configured"; lock records one
    //   umount request, emits no retry warning, and returns the umount error.
    #[test]
    fn lock_umount_non_busy_failure_does_not_retry() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            umount_request(),
            lock_err_raw(
                "umount /mnt/storage",
                32,
                "umount: /mnt/storage: device not configured.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_ok_raw("cryptsetup close braid-aaa"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-bbb".into()),
            },
            lock_ok_raw("cryptsetup close braid-bbb"),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_lock_impl(
                &runner,
                &fs,
                &LockNoopSleeper,
                &config,
                &membership,
                false,
            ));
        });

        let err = result
            .expect("lock result should be captured")
            .expect_err("non-busy umount failure should fail lock");
        assert!(
            err.to_string().contains("device not configured"),
            "expected non-busy umount stderr in error, got: {err}"
        );
        assert_eq!(
            umount_request_count(&runner),
            1,
            "non-busy umount failure must not retry"
        );
        assert!(
            !captured.contains("retrying"),
            "non-busy umount failure should not emit retry warning: {captured:?}"
        );
    }

    // Intent: command-runner failures from umount bubble out immediately.
    // Why it exists: `runner.run` errors mean braid could not execute umount
    //   at all, so continuing into mapper close would hide a command
    //   execution failure behind best-effort cleanup behavior.
    // Scenario: the mounted-pool plan reaches umount, but the runner has no
    //   umount mock and returns MissingMock; lock returns LockError::Cmd and
    //   never issues a CryptsetupClose request.
    #[test]
    fn lock_umount_cmd_error_bubbles_immediately_without_mapper_close() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ));
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("missing umount mock should return command error");
        assert!(
            matches!(err, LockError::Cmd(CmdError::MissingMock)),
            "expected umount MissingMock to bubble as LockError::Cmd, got: {err:?}"
        );
        assert_eq!(
            cryptsetup_close_request_count(&runner),
            0,
            "command-execution failure from umount must not attempt mapper close"
        );
    }

    // Intent: non-busy umount failures omit the lsof/fuser hint because it
    //   would send users hunting for a holder that does not exist.
    // Why it exists: libmount routes every umount syscall errno through exit
    //   32, so exit-code gating cannot distinguish busy from non-busy. The
    //   hint must be gated on the EBUSY diagnostic "target is busy" instead.
    // Scenario: umount returns exit 32 with stderr ending in "can't write
    //   superblock" from a failing disk; LockError echoes that stderr but does
    //   not suggest lsof/fuser.
    #[test]
    fn lock_umount_non_busy_omits_hint() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint::new("/mnt/storage".to_owned()),
            },
            lock_err_raw(
                "umount /mnt/storage",
                32,
                "umount: /mnt/storage: can't write superblock.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_ok_raw("cryptsetup close braid-aaa"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-bbb".into()),
            },
            lock_ok_raw("cryptsetup close braid-bbb"),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail on non-busy umount");
        let msg = err.to_string();
        assert!(
            msg.contains("can't write superblock"),
            "expected raw stderr in msg, got: {msg}"
        );
        assert!(
            !msg.contains("lsof") && !msg.contains("fuser"),
            "expected no lsof/fuser hint for non-busy failure, got: {msg}"
        );
    }

    // Intent: a mount path containing the literal phrase "target is busy" does
    //   not trip the hint gate when the actual diagnostic is non-busy.
    // Why it exists: util-linux formats umount stderr as
    //   "<target>: <diagnostic>.". MountPoint only rejects empty strings, so a
    //   path containing "target is busy" is structurally legal. A naive
    //   stderr.contains("target is busy") would emit the hint even on EIO.
    // Scenario: stderr's path component contains the phrase but the diagnostic
    //   at the end is "can't write superblock"; hint must not appear.
    #[test]
    fn lock_umount_path_containing_busy_phrase_omits_hint() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint::new("/mnt/storage".to_owned()),
            },
            lock_err_raw(
                "umount /mnt/storage",
                32,
                "umount: /mnt/has target is busy here/storage: can't write superblock.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_ok_raw("cryptsetup close braid-aaa"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-bbb".into()),
            },
            lock_ok_raw("cryptsetup close braid-bbb"),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail on non-busy umount with busy-phrase path");
        let msg = err.to_string();
        assert!(
            !msg.contains("lsof") && !msg.contains("fuser"),
            "expected no lsof/fuser hint when only the path contains the phrase, got: {msg}"
        );
    }

    #[test]
    fn lock_adds_forget_after_umount() {
        let runner = lock_mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        // If BtrfsDeviceScanForget were not called, MockRunner would return
        // MissingMock and the test would fail.
        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed with forget");
    }

    // Intent: execute drops planned close-set mappers whose /dev/mapper path
    //   disappeared between plan and execute before issuing BtrfsDeviceScanForget.
    // Why it exists: the lock cookbook documents this filter; preview-side
    //   helpers only pin compile_lock_steps, which has no execute-time
    //   fs.exists filter.
    // Scenario: the mounted plan observes braid-aaa and braid-bbb, but
    //   /dev/mapper/braid-bbb has disappeared before execution; lock forgets
    //   and closes only braid-aaa, treating braid-bbb as already closed.
    #[test]
    fn lock_execute_forget_filters_disappeared_mapper() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint::new("/mnt/storage".to_owned()),
            },
            lock_ok_raw("umount /mnt/storage"),
        )
        .with_output(
            CmdRequest::BtrfsDeviceScanForget {
                devices: vec!["/dev/mapper/braid-aaa".into()],
            },
            lock_ok_raw("btrfs device scan --forget"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_ok_raw("cryptsetup close braid-aaa"),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed when one planned mapper disappeared");

        assert_eq!(
            forget_requests(&runner),
            vec![vec!["/dev/mapper/braid-aaa".to_owned()]],
            "forget should only receive still-existing mapper paths"
        );

        let close_requests: Vec<String> = runner
            .requests()
            .into_iter()
            .filter_map(|request| match request {
                CmdRequest::CryptsetupClose { mapper } => Some(mapper.as_str().to_owned()),
                _ => None,
            })
            .collect();
        assert_eq!(
            close_requests,
            vec!["braid-aaa".to_owned()],
            "execute should not close the disappeared braid-bbb mapper"
        );
    }

    #[test]
    fn lock_forget_failure_is_nonfatal() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint::new("/mnt/storage".to_owned()),
            },
            lock_ok_raw("umount /mnt/storage"),
        )
        .with_output(
            CmdRequest::BtrfsDeviceScanForget {
                devices: vec![
                    "/dev/mapper/braid-aaa".into(),
                    "/dev/mapper/braid-bbb".into(),
                ],
            },
            lock_err_raw("btrfs device scan --forget", 1, "some error"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_ok_raw("cryptsetup close braid-aaa"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-bbb".into()),
            },
            lock_ok_raw("cryptsetup close braid-bbb"),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed even when forget fails");
    }

    // Intent: orphaned braid-* mappers from prior crashes are cleaned up
    //   during lock.
    // Why it exists: a crash between cryptsetup open and journal/pool.json
    //   write leaves a mapper outside pool.json that the membership loop
    //   won't close.
    // Scenario: power loss during `braid add` after LUKS open but before
    //   pool.json write; next `braid lock` must still close the orphan.
    #[test]
    fn lock_closes_orphaned_mapper() {
        let runner = lock_mounted_runner()
            // Override forget mock: with an orphan present, the forget
            // set must include it (close-set-scoped).
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-aaa".into(),
                        "/dev/mapper/braid-bbb".into(),
                        "/dev/mapper/braid-ccc".into(),
                    ],
                },
                lock_ok_raw("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-ccc".into()),
                },
                lock_ok_raw("cryptsetup close braid-ccc"),
            );
        // ccc is not in membership but exists as a mapper → orphan
        let fs = lock_member_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ])
        .with_luks_dm("braid-ccc", ORPHAN_UUID);
        let config = lock_test_config();
        let membership = lock_test_membership();

        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should close orphan too");
    }

    // Intent: I/O errors scanning /dev/mapper don't prevent closing known
    //   mappers.
    // Why it exists: /dev/mapper may be unreadable in degraded environments;
    //   the safety-net scan shouldn't break the primary lock path.
    // Scenario: containerized environment where /dev/mapper has restricted
    //   permissions; lock must still close membership-known mappers.
    #[test]
    fn lock_orphan_scan_failure_is_nonfatal() {
        let runner = lock_mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_dev_mapper_error();
        let config = lock_test_config();
        let membership = lock_test_membership();

        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed despite list_dir failure");
    }

    /*
     * Intent: `braid lock --dry-run` preview surfaces a `[warn]` line when
     *   /dev/mapper cannot be scanned for orphans.
     * Why it exists: the dry-run branch previously used
     *   `if let Ok(entries) = fs.list_dir(...)`, silently swallowing the
     *   error while the real run warned -- violating the dry-run contract
     *   of "preview what the real command will do."
     * Scenario: containerized environment where /dev/mapper is unreadable;
     *   the user runs `braid lock --dry-run` to preview the shutdown and
     *   must see the scan failure, not a falsely-clean preview.
     */
    #[test]
    fn dry_run_preview_warns_when_list_dir_fails() {
        // Pool is not mounted -- mountpoint check returns non-zero.
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs =
            lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]).with_dev_mapper_error();
        let config = lock_test_config();
        let plan = plan_lock(
            &runner,
            &fs,
            &config,
            &lock_test_membership(),
            LockMode::User,
        )
        .expect("plan_lock should succeed with list_dir failure");
        let output = plan.preview().render();

        assert!(
            plan.members_known_closed.is_empty(),
            "unscannable fallback cannot prove any member closed"
        );
        assert!(
            output.starts_with(
                "[warn] could not scan /dev/mapper for braid mappers: permission denied (skipping)\n"
            ),
            "preview must start with the exact [warn] line, got:\n{output}"
        );
        assert!(
            output.contains("cleanup incomplete: some braid mappers could not be verified"),
            "preview must carry cleanup-uncertain info, got:\n{output}"
        );
        assert!(
            !output.contains("close LUKS mapper"),
            "fallback scan failure must not render name-derived close steps, got:\n{output}"
        );
        assert!(
            !output.contains("nothing to do."),
            "warning-only uncertain cleanup must not render a clean no-op, got:\n{output}"
        );
    }

    /*
     * Intent: `braid lock --dry-run` preview surfaces a `[warn]` line per
     *   orphan mapper found in /dev/mapper.
     * Why it exists: the dry-run branch previously omitted the per-orphan
     *   warn that the real run prints, so users could not see why an
     *   `(orphan)` close step was about to run from the preview alone.
     * Scenario: prior crash left braid-ccc as an orphan; user runs
     *   `braid lock --dry-run` and must see the explanatory warn body
     *   above the orphan close step, identical to the real-run wording.
     */
    #[test]
    fn dry_run_preview_warns_per_orphan_mapper() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ));
        let fs = lock_member_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ])
        .with_luks_dm("braid-ccc", ORPHAN_UUID);
        let config = lock_test_config();
        let plan = plan_lock(
            &runner,
            &fs,
            &config,
            &lock_test_membership(),
            LockMode::User,
        )
        .expect("plan_lock should succeed with one orphan mapper");
        let output = plan.preview().render();
        let warn_line =
            "[warn] orphaned mapper braid-ccc (not in pool.json -- likely a prior crash)\n";

        assert!(
            output.starts_with(warn_line),
            "preview must start with the exact per-orphan [warn] line, got:\n{output}"
        );
        assert!(
            output.contains("close LUKS mapper braid-ccc (orphan)"),
            "preview must still render the orphan close step, got:\n{output}"
        );
    }

    /*
     * Intent: `plan_lock(...).preview().render()` produces the full
     *   happy-path preview -- umount + scoped forget + per-mapper
     *   closes (membership and orphan).
     * Why it exists: the plan's preview is the sole boundary between
     *   `cmd_lock` dry-run and the user; a refactor that drops any of
     *   these steps must fail a test, not only `compile_lock_steps`'
     *   isolated tests.
     * Scenario: pool mounted, both membership mappers open, one orphan
     *   (braid-ccc) left by a prior crash; user previews `braid lock
     *   --dry-run` to confirm the shutdown plan before running it.
     */
    #[test]
    fn dry_run_preview_mounted_happy_path() {
        // Mounted pool needs MountpointCheck ok + probe_fsid mocks for
        // preflight. Plan-only path -- no umount/forget/close mocks.
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ));
        let fs = lock_member_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ])
        .with_luks_dm("braid-ccc", ORPHAN_UUID);
        let config = lock_test_config();
        let plan = plan_lock(
            &runner,
            &fs,
            &config,
            &lock_test_membership(),
            LockMode::User,
        )
        .expect("plan_lock should succeed on mounted pool");
        let output = plan.preview().render();
        let warn_line =
            "[warn] orphaned mapper braid-ccc (not in pool.json -- likely a prior crash)\n";

        assert!(
            output.contains(warn_line),
            "preview must include the orphan warning, got:\n{output}"
        );
        let warn_pos = output
            .find(warn_line)
            .expect("orphan warning should be present");
        let first_step_pos = output
            .find("[safe]")
            .expect("preview should include at least one step");
        assert!(
            warn_pos < first_step_pos,
            "orphan warning must render before the first step row, got:\n{output}"
        );
        assert!(
            output.contains("unmount /mnt/storage"),
            "preview must include unmount step, got:\n{output}"
        );
        assert!(
            output.contains("btrfs device scan --forget"),
            "preview must include scoped forget step, got:\n{output}"
        );
        assert!(
            output.contains("close LUKS mapper braid-aaa"),
            "preview must include membership close for braid-aaa, got:\n{output}"
        );
        assert!(
            output.contains("close LUKS mapper braid-bbb"),
            "preview must include membership close for braid-bbb, got:\n{output}"
        );
        assert!(
            output.contains("close LUKS mapper braid-ccc (orphan)"),
            "preview must include orphan close for braid-ccc, got:\n{output}"
        );
    }

    /*
     * Intent: `plan_lock(...).preview().render()` emits exactly
     *   `nothing to do.\n` when the pool is unmounted with no open
     *   membership or orphan mappers.
     * Why it exists: the no-op branch is easy to regress silently -- a
     *   helper refactor could drop or alter the line and all other tests
     *   would stay green.
     * Scenario: user re-runs `braid lock --dry-run` on an already-locked
     *   pool and expects a short, deterministic confirmation.
     */
    #[test]
    fn dry_run_preview_nothing_to_do() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = lock_fs(&[]);
        let config = lock_test_config();
        let plan = plan_lock(
            &runner,
            &fs,
            &config,
            &lock_test_membership(),
            LockMode::User,
        )
        .expect("plan_lock should succeed on already-locked pool");
        let output = plan.preview().render();

        assert_eq!(output, "nothing to do.\n", "unexpected preview: {output:?}");
    }

    // Intent: mounted `probe_pool` failure still plans useful cleanup
    // for a mapper whose LUKS UUID matches membership.
    // Why it exists: the fallback must remain best-effort without
    // returning to name-derived member ownership.
    // Scenario: per-device probing fails before close-set construction,
    // but the mounted FSID preflight succeeds and `/dev/mapper/braid-aaa`
    // can be reclassified by the LUKS UUID its dm UUID records.
    #[test]
    fn mounted_probe_failure_fallback_closes_uuid_verified_member() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_err_raw("cryptsetup status braid-aaa", 5, "transient status failure"),
        );
        let fs = lock_fs(&["/dev/mapper/braid-aaa"]).with_luks_dm("braid-aaa", AAA_UUID);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::User)
            .expect("fallback should plan");

        assert!(
            plan.notes.iter().any(|note| matches!(
                note,
                PreviewNote::Warn(body)
                    if body.contains("falling back to UUID-scanned mapper cleanup")
            )),
            "fallback warning expected, got: {:?}",
            plan.notes,
        );
        assert_eq!(
            member_summaries(&plan.close_set),
            vec![("braid-aaa".to_owned(), "aaa".to_owned())],
        );
        assert!(!plan.cleanup_uncertain);
    }

    // Intent: systemd-stop lock under the mounted ProbeFailed fallback pauses a
    //   running balance before teardown instead of refusing it.
    // Why it exists: the ProbeFailed arm used to carry its own LockMode
    //   dispatch; routing it to the user preflight would make ExecStop refuse
    //   to lock during a balance and strand LUKS open across shutdown while
    //   healthy-probe and user-mode tests stayed green.
    // Scenario: btrfs reports a non-/dev/mapper/ path for braid's mounted pool
    //   (modeled here by a per-device probe failure) while UPS low-battery
    //   shutdown interrupts a running balance and ExecStop runs lock cleanup.
    #[test]
    fn systemd_stop_probe_failed_fallback_pauses_running_balance() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_err_raw("cryptsetup status braid-aaa", 5, "transient status failure"),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_excl_op("balance");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::SystemdStop)
            .expect("probe-failed fallback should plan under systemd-stop");

        assert!(
            plan.notes.iter().any(|note| matches!(
                note,
                PreviewNote::Warn(body)
                    if body.contains("falling back to UUID-scanned mapper cleanup")
            )),
            "expected the ProbeFailed fallback warning, got: {:?}",
            plan.notes,
        );
        assert!(
            plan.pause_balance_before_unmount,
            "systemd-stop fallback must pause a running balance before unmount"
        );
        assert_eq!(
            member_summaries(&plan.close_set),
            vec![
                ("braid-aaa".to_owned(), "aaa".to_owned()),
                ("braid-bbb".to_owned(), "bbb".to_owned()),
            ],
        );
        assert!(!plan.cleanup_uncertain);
    }

    // Intent: an unmounted user lock does not consult the exclusive-op preflight;
    //   a stale `balance` reading must not make the plan refuse.
    // Why it exists: "skip the preflight when not mounted" is an invariant
    //   (ADR-024 item 7; lock.md step 2). The Snapshot::Unmounted arm bypasses
    //   lock_preflight_pause_decision entirely. A refactor routing it through the
    //   user preflight (e.g. by re-probing an FSID) would make an unmounted lock
    //   spuriously fail during a balance -- and every existing test would stay
    //   green because all unmounted tests leave exclusive_operation at "none".
    //   In user mode that regression surfaces as the `.expect()` below panicking
    //   (the preflight returns Err, so plan_lock fails), not as the pause
    //   assertion failing; the pause assertion is the direct discriminator in the
    //   systemd-stop sibling test, where the preflight returns Ok(true).
    // Scenario: pool is not mounted but sysfs still reports a stale `balance`
    //   exclusive op (a state only constructible in a unit test); operator runs
    //   `braid lock`.
    #[test]
    fn unmounted_user_lock_ignores_active_exclusive_op() {
        // FSID-probe mocks armed so a re-probe regression reaches the preflight
        // (and bites on the seeded op) instead of dying on a missing BtrfsFilesystemShow.
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        ));
        let fs = lock_member_fs(&[]).with_excl_op("balance");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::User)
            .expect("unmounted lock must plan without consulting the exclusive-op preflight");

        assert!(
            !plan.pool_was_mounted,
            "test must exercise the Unmounted arm"
        );
        assert!(
            !plan.pause_balance_before_unmount,
            "unmounted lock must not pause for a balance: the preflight is skipped when not mounted"
        );
    }

    // Intent: same invariant under the systemd-stop preflight contract -- an
    //   unmounted ExecStop lock does not pause for a stale running balance.
    // Why it exists: SystemdStop's preflight returns Ok(true) for a running
    //   balance, so routing the Unmounted arm through it would set
    //   pause_balance_before_unmount = true and make ExecStop try to pause a
    //   balance on a pool that is not mounted -- silently, with all tests green.
    // Scenario: shutdown ExecStop runs lock on an already-unmounted pool while
    //   sysfs still reports a stale `balance` op.
    #[test]
    fn unmounted_systemd_stop_lock_ignores_active_exclusive_op() {
        // FSID-probe mocks armed so a re-probe regression reaches the preflight
        // (and bites on the seeded op) instead of dying on a missing BtrfsFilesystemShow.
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        ));
        let fs = lock_member_fs(&[]).with_excl_op("balance");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::SystemdStop)
            .expect("unmounted systemd-stop lock must plan without consulting the preflight");

        assert!(
            !plan.pool_was_mounted,
            "test must exercise the Unmounted arm"
        );
        assert!(
            !plan.pause_balance_before_unmount,
            "unmounted systemd-stop lock must not pause for a balance: preflight is skipped when not mounted"
        );
    }

    // Intent: a mapper named like a member but backed by a different
    // readable UUID is not classified as that member.
    // Why it exists: `braid-<DiskName>` is a cleanup namespace, not
    // identity proof; a swapped mapper must not receive member status.
    // Scenario: pool is unmounted and `/dev/mapper/braid-aaa`'s dm UUID
    // records a LUKS UUID that is absent from pool.json.
    #[test]
    fn fallback_member_named_mapper_with_different_uuid_is_orphan() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = lock_fs(&["/dev/mapper/braid-aaa"]).with_luks_dm("braid-aaa", ORPHAN_UUID);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::User)
            .expect("fallback should plan");

        assert!(member_summaries(&plan.close_set).is_empty());
        assert_eq!(
            orphan_summaries(&plan.close_set),
            vec![("braid-aaa".to_owned(), "aaa".to_owned())],
        );
        assert!(!plan.cleanup_uncertain);
    }

    // Intent: non-dry-run lock with empty membership still closes every
    // observed braid-prefixed mapper whose dm UUID records a LUKS UUID.
    // Why it exists: lock dispatch may intentionally fall back to empty
    // membership when pool.json is missing or corrupt during recovery.
    // Scenario: the pool is unmounted, `/dev/mapper/braid-aaa` and
    // `/dev/mapper/braid-bbb` are open, and pool.json contributes no members.
    #[test]
    fn cmd_lock_with_empty_membership_closes_observed_orphan_mappers() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint::new("/mnt/storage".to_owned()).into(),
                },
                lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = PoolMembership::empty();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::User)
            .expect("empty membership should still produce a close plan");
        assert!(member_summaries(&plan.close_set).is_empty());
        assert_eq!(
            orphan_summaries(&plan.close_set),
            vec![
                ("braid-aaa".to_owned(), "aaa".to_owned()),
                ("braid-bbb".to_owned(), "bbb".to_owned())
            ],
        );
        assert!(plan.members_known_closed.is_empty());

        let recording = LockRecordingRunner::new(runner.clone());
        cmd_lock_impl(
            &recording,
            &fs,
            &LockNoopSleeper,
            &config,
            &membership,
            false,
        )
        .expect("lock should close UUID-verified orphan mappers");

        assert_eq!(
            recording.close_calls(),
            vec!["braid-aaa".to_owned(), "braid-bbb".to_owned()]
        );
        assert!(recording.forget_calls().is_empty());
    }

    // Intent: unmounted lock closes a UUID-verified member mapper even
    // when the observed mapper name has drifted.
    // Why it exists: the unmounted fallback has no btrfs pool.devices
    // evidence, so the dm UUID scan is the only safe way to preserve
    // observed-mapper cleanup.
    // Scenario: `/dev/mapper/braid-WRONG` is open for member `aaa` while
    // the pool is not mounted.
    #[test]
    fn unmounted_fallback_closes_uuid_verified_drifted_member() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = lock_fs(&["/dev/mapper/braid-WRONG"]).with_luks_dm("braid-WRONG", AAA_UUID);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::User)
            .expect("fallback should plan");

        assert_eq!(
            member_summaries(&plan.close_set),
            vec![("braid-WRONG".to_owned(), "aaa".to_owned())],
        );
        assert!(
            plan.preview()
                .render()
                .contains("close LUKS mapper braid-WRONG")
        );
    }

    // Intent: unverified braid-prefixed candidates are warned and skipped,
    // never closed as orphan by name.
    // Why it exists: a mapper-name match with no readable backing UUID is
    // insufficient proof for either member-owned or orphan cleanup.
    // Scenario: pool is unmounted and `/dev/mapper/braid-aaa` exists, but
    // `cryptsetup status braid-aaa` cannot be verified.
    #[test]
    fn unverified_fallback_candidate_is_warned_and_skipped() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = lock_fs(&["/dev/mapper/braid-aaa"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::User)
            .expect("fallback should plan");
        let output = plan.preview().render();

        assert!(plan.close_set.is_empty());
        assert!(plan.cleanup_uncertain);
        assert!(
            output.starts_with("[warn] skipping mapper braid-aaa: cannot verify backing LUKS UUID"),
            "skip warning must render first, got:\n{output}"
        );
        assert!(
            output.contains("cleanup incomplete: some braid mappers could not be verified"),
            "cleanup info expected, got:\n{output}"
        );
        assert!(
            !output.contains("close LUKS mapper") && !output.contains("btrfs device scan --forget"),
            "skipped mapper must not enter close/forget steps, got:\n{output}"
        );
        assert!(
            !output.contains("nothing to do."),
            "uncertain warning-only preview must not render clean no-op, got:\n{output}"
        );
    }

    // Intent: real lock suppresses the already-locked terminal line when
    //   cleanup is uncertain.
    // Why it exists: skipped unverified mappers mean cleanup may be incomplete,
    //   so reporting a clean already-locked state would mislead operators.
    // Scenario: the pool is unmounted and `/dev/mapper/braid-aaa` exists, but
    //   its backing LUKS UUID cannot be verified before execution.
    #[test]
    fn lock_uncertain_cleanup_warns_without_already_locked_line() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = lock_fs(&["/dev/mapper/braid-aaa"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_lock_impl(
                &runner,
                &fs,
                &LockNoopSleeper,
                &config,
                &membership,
                false,
            ));
        });

        result
            .expect("lock result should be captured")
            .expect("lock should succeed with uncertain cleanup");
        assert!(
            captured.contains("[warn] skipping mapper braid-aaa: cannot verify backing LUKS UUID"),
            "missing uncertain cleanup warning in {captured:?}"
        );
        assert!(
            !captured.contains("pool already locked"),
            "uncertain cleanup must not report a clean already-locked state: {captured:?}"
        );
    }

    // Intent: when umount fails, lock still attempts to close LUKS mappers
    //   and returns the umount error (not a mapper error).
    // Why it exists: the original code returned immediately on umount failure,
    //   leaving all LUKS mappers open — a security gap during shutdown.
    // Scenario: umount fails with "target is busy"; both mapper closes succeed
    //   anyway (e.g. kernel released references between umount and close).
    //   The function still fails with the umount error because the mount is
    //   in an inconsistent state.
    #[test]
    fn lock_umount_fails_but_mappers_close_successfully() {
        let runner = lock_umount_failed_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail — umount error is the root cause");
        let msg = err.to_string();
        assert!(
            msg.contains("umount") && msg.contains("target is busy"),
            "expected umount error, got: {msg}"
        );
    }

    // Intent: busy mapper close errors are suppressed (as warnings) when
    //   umount already failed, and the umount error is returned.
    // Why it exists: busy mapper close after a stuck umount is expected —
    //   the filesystem still holds the devices. Surfacing the mapper error
    //   instead of the umount error would obscure the root cause.
    // Scenario: umount fails; both mapper closes fail with "in use" (DeviceBusy).
    //   The returned error is the umount error, not a mapper close error.
    #[test]
    fn lock_umount_fails_busy_mapper_is_warning() {
        let runner = lock_umount_failed_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Device braid-aaa is still in use.",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_err_raw(
                    "cryptsetup close braid-bbb",
                    5,
                    "Device braid-bbb is still in use.",
                ),
            );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail with umount error");
        let msg = err.to_string();
        assert!(
            msg.contains("umount") && msg.contains("target is busy"),
            "expected umount error (not mapper error), got: {msg}"
        );
    }

    // Intent: unexpected (non-busy) mapper close errors remain fatal even when
    //   umount already failed — only DeviceBusy is suppressed.
    // Why it exists: suppressing all mapper close errors after umount failure
    //   would hide real problems like permission errors or missing devices.
    //   Only the expected busy/in-use errors should be downgraded to warnings.
    // Scenario: umount fails; mapper aaa close fails with "Device is not
    //   active." (not a busy error). Remaining mappers are still attempted,
    //   then the non-busy mapper error is returned (takes precedence over
    //   the umount error).
    #[test]
    fn lock_umount_fails_unexpected_mapper_error_is_fatal() {
        let runner = lock_umount_failed_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_err_raw("cryptsetup close braid-aaa", 4, "Device is not active."),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail with mapper error");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-aaa") && msg.contains("not active"),
            "expected mapper error (not umount error), got: {msg}"
        );
    }

    // Intent: mapper close errors remain fatal when umount succeeded.
    // Why it exists: regression guard — the umount-failure fix must not
    //   accidentally suppress mapper close errors on the normal path.
    // Scenario: umount succeeds; aaa mapper close fails with a non-busy error.
    //   Remaining mappers are still attempted, then the mapper error is returned.
    #[test]
    fn lock_mapper_close_fatal_when_umount_succeeded() {
        let runner = lock_mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_err_raw("cryptsetup close braid-aaa", 4, "Device is not active."),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail on mapper close");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-aaa"),
            "expected mapper error, got: {msg}"
        );
    }

    // Intent: busy orphan mapper close errors are suppressed when umount
    //   already failed, same as for membership mappers.
    // Why it exists: the membership and orphan close loops are separate code
    //   paths; a bug in orphan handling could slip through even if the
    //   membership tests pass.
    // Scenario: umount fails; membership mappers close ok; orphan mapper
    //   close fails with "in use" (DeviceBusy). The returned error is the
    //   umount error.
    #[test]
    fn lock_umount_fails_orphan_busy_is_warning() {
        let runner = lock_umount_failed_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-ccc".into()),
                },
                lock_err_raw(
                    "cryptsetup close braid-ccc",
                    5,
                    "Device braid-ccc is still in use.",
                ),
            );
        let fs = lock_member_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ])
        .with_luks_dm("braid-ccc", ORPHAN_UUID);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail with umount error");
        let msg = err.to_string();
        assert!(
            msg.contains("umount") && msg.contains("target is busy"),
            "expected umount error (not orphan error), got: {msg}"
        );
    }

    // Intent: unexpected (non-busy) orphan mapper close errors remain fatal
    //   even when umount already failed.
    // Why it exists: the orphan branch must have the same precise suppression
    //   as the membership branch — only DeviceBusy is suppressed.
    // Scenario: umount fails; membership mappers close ok; orphan mapper
    //   close fails with "Device is not active." (non-busy). All mappers are
    //   still attempted, then the orphan error is returned (takes precedence
    //   over the umount error).
    #[test]
    fn lock_umount_fails_orphan_unexpected_error_is_fatal() {
        let runner = lock_umount_failed_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-ccc".into()),
                },
                lock_err_raw("cryptsetup close braid-ccc", 4, "Device is not active."),
            );
        let fs = lock_member_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ])
        .with_luks_dm("braid-ccc", ORPHAN_UUID);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail with orphan error");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-ccc") && msg.contains("not active"),
            "expected orphan mapper error (not umount error), got: {msg}"
        );
    }

    // Intent: if an orphan mapper is detected but can't be closed, lock must
    //   fail rather than silently leaving LUKS open.
    // Why it exists: a stray open LUKS mapper is a security concern —
    //   reporting success while leaving it open is worse than failing.
    // Scenario: orphan mapper is held open by a leaked process; lock must
    //   surface the failure.
    #[test]
    fn lock_orphan_close_failure_is_fatal() {
        let runner = lock_mounted_runner()
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-aaa".into(),
                        "/dev/mapper/braid-bbb".into(),
                        "/dev/mapper/braid-orphan".into(),
                    ],
                },
                lock_ok_raw("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-orphan".into()),
                },
                lock_err_raw("cryptsetup close braid-orphan", 4, "Device is not active."),
            );
        let fs = lock_member_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-orphan",
        ])
        .with_luks_dm("braid-orphan", ORPHAN_UUID);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail on orphan close");
        assert!(
            err.to_string().contains("braid-orphan"),
            "error should mention the orphan mapper, got: {err}"
        );
    }

    // Intent: when a mapper close fails with a non-busy error, remaining
    //   mappers are still attempted.
    // Why it exists: guards against the original bug where a non-busy error
    //   caused an early return, skipping remaining mappers and leaving LUKS
    //   devices open.
    // Scenario: umount succeeds; aaa mapper close fails with "Device is not
    //   active"; bbb mapper close succeeds. Both mappers were attempted.
    #[test]
    fn lock_continues_closing_after_mapper_error() {
        let inner = lock_mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_err_raw("cryptsetup close braid-aaa", 4, "Device is not active."),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let runner = LockRecordingRunner::new(inner);
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail with mapper error");
        assert!(
            err.to_string().contains("braid-aaa"),
            "expected aaa error, got: {err}"
        );
        let calls = runner.close_calls();
        assert!(
            calls.contains(&"braid-aaa".to_string()) && calls.contains(&"braid-bbb".to_string()),
            "expected both mappers attempted, got: {calls:?}"
        );
    }

    // Intent: when multiple mapper closes fail with non-busy errors, the
    //   first error is returned and all mappers were attempted.
    // Why it exists: ensures error accumulation works end-to-end for the
    //   multi-failure case — the first error wins, but nothing is skipped.
    // Scenario: umount succeeds; both aaa and bbb fail with non-busy errors.
    //   The returned error mentions aaa (first in iteration order).
    #[test]
    fn lock_collects_first_mapper_error() {
        let inner = lock_mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_err_raw("cryptsetup close braid-aaa", 4, "Device is not active."),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_err_raw("cryptsetup close braid-bbb", 1, "permission denied"),
            );
        let runner = LockRecordingRunner::new(inner);
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail with first mapper error");
        let msg = err.to_string();
        assert!(
            msg.contains("braid-aaa"),
            "expected first error (aaa), got: {msg}"
        );
        let calls = runner.close_calls();
        assert!(
            calls.contains(&"braid-aaa".to_string()) && calls.contains(&"braid-bbb".to_string()),
            "expected both mappers attempted, got: {calls:?}"
        );
    }

    /*
     * Intent: `cryptsetup close` that returns "busy" once but succeeds on
     * retry must let `braid lock` finish cleanly, closing the mapper on
     * attempt 2.
     *
     * Why it exists: the btrfs scan registry can keep device references
     * alive for a short window after umount (see commit 1484ff1 and
     * tests/repro/cryptsetup-close-btrfs-held.py). The retry loop in
     * `close_mapper_with_retry` exists to cover that window. Without
     * this test, a regression that misclassifies the busy substring,
     * flips CLOSE_RETRY_ATTEMPTS to 1, or mis-orders the early returns
     * would pass every existing unit test -- only the race-dependent VM
     * repro could surface it.
     *
     * Scenario: pool mounted; umount and btrfs forget succeed; first
     * `cryptsetup close braid-aaa` returns "Device braid-aaa is still
     * in use.", second returns ok; `braid-bbb` closes cleanly on the
     * first try.
     */
    #[test]
    fn lock_retries_busy_close_then_succeeds() {
        let inner = lock_mounted_runner().with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-bbb".into()),
            },
            lock_ok_raw("cryptsetup close braid-bbb"),
        );
        let runner = LockRecordingRunner::new(inner).with_close_sequence(
            "braid-aaa",
            vec![
                lock_err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Device braid-aaa is still in use.",
                ),
                lock_ok_raw("cryptsetup close braid-aaa"),
            ],
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed after retry");

        let calls = runner.close_calls();
        let aaa_calls = calls.iter().filter(|m| m.as_str() == "braid-aaa").count();
        let bbb_calls = calls.iter().filter(|m| m.as_str() == "braid-bbb").count();
        assert_eq!(
            aaa_calls, 2,
            "expected exactly 2 close attempts for braid-aaa, got: {calls:?}"
        );
        assert_eq!(
            bbb_calls, 1,
            "expected exactly 1 close for braid-bbb, got: {calls:?}"
        );
    }

    // Intent: cryptsetup close with exit status 5 goes through the retry
    //   loop and surfaces as LockError::DeviceBusy, regardless of the
    //   specific English phrase in stderr.
    // Why it exists: the classifier in mapper_close.rs close_mapper_with_retry
    //   is what distinguishes "kernel-async release race, retry wins" from
    //   "every close hard-fails on first attempt". An earlier stderr-substring
    //   classifier ("busy" || "in use") would hard-fail on wording drift
    //   like "still active and cannot be removed". This test uses that
    //   non-canonical wording at exit 5 so a regression back to
    //   stderr-based matching fails here.
    // Scenario: umount succeeds; braid-aaa close returns exit 5 on every
    //   attempt with non-canonical busy wording; braid-bbb closes cleanly.
    //   Lock must retry braid-aaa CLOSE_RETRY_ATTEMPTS times, then return
    //   LockError::DeviceBusy.
    #[test]
    fn lock_mapper_close_exit5_is_busy_regardless_of_wording() {
        let inner = lock_mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Target braid-aaa is still active and cannot be removed.",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let runner = LockRecordingRunner::new(inner);
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("busy close should bubble up after retries exhaust");
        assert!(
            matches!(err, LockError::DeviceBusy(_)),
            "expected LockError::DeviceBusy, got: {err:?}"
        );
        let aaa_attempts = runner
            .close_calls()
            .iter()
            .filter(|m| m.as_str() == "braid-aaa")
            .count();
        assert_eq!(
            aaa_attempts, CLOSE_RETRY_ATTEMPTS as usize,
            "expected {} retry attempts, got {}",
            CLOSE_RETRY_ATTEMPTS, aaa_attempts
        );
    }

    // Intent: when `cryptsetup close` returns busy on every retry and umount
    //   succeeded (no suppression), cmd_lock surfaces a LockError::DeviceBusy
    //   whose rendered message preserves the mapper name, the raw exit code,
    //   and the ORIGINAL-CASED, TRIMMED stderr from cryptsetup exactly.
    // Why it exists: locks the full DeviceBusy message contract so refactors
    //   in close_mapper_with_retry (dedup, formatting tweaks) can't silently
    //   drop .trim(), change the shape, or drift the text. The sibling test
    //   `lock_mapper_close_exit5_is_busy_regardless_of_wording` pins variant
    //   + retry count but not message content; this test pins the exact
    //   bytes the user sees.
    // Scenario: pool mounted; umount/forget succeed; every close attempt
    //   for braid-aaa returns exit 5 with a mixed-case stderr padded with
    //   leading whitespace and a trailing newline; braid-bbb closes cleanly.
    #[test]
    fn lock_busy_close_exhausts_retries_preserves_stderr_contract() {
        let busy_stderr = "  Device braid-aaa IS STILL IN USE.\n";
        let runner = lock_mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_err_raw("cryptsetup close braid-aaa", 5, busy_stderr),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail: busy retries exhausted");
        // Variant check is orthogonal to text check: guards against a rename
        // that also updates the #[error(...)] attribute and still renders
        // the same bytes.
        assert!(
            matches!(err, LockError::DeviceBusy(_)),
            "expected LockError::DeviceBusy, got: {err:?}"
        );
        // Full rendered-message lock: pins the thiserror prefix
        // ("device busy: "), the cryptsetup phrasing, the raw exit code,
        // and the ORIGINAL-CASED TRIMMED stderr all in one assertion. Any
        // drift -- shape change, dropped .trim(), missing exit code --
        // flips this.
        assert_eq!(
            err.to_string(),
            "device busy: cryptsetup close braid-aaa failed (exit 5): \
             Device braid-aaa IS STILL IN USE."
        );
    }

    // Intent: systemd-stop lock proceeds while a balance is running.
    // Why it exists: shutdown must reach the ordered umount path so the
    //   explicit pause can persist the balance before LUKS close.
    // Scenario: UPS low-battery shutdown interrupts remove-missing during
    //   its post-commit balance and ExecStop performs lock cleanup.
    #[test]
    fn systemd_stop_proceeds_on_running_balance() {
        let runner = mounted_systemd_stop_runner();
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_excl_op("balance");
        let config = lock_test_config();
        let membership = lock_test_membership();

        cmd_lock_systemd_stop(&runner, &fs, &config, &membership)
            .expect("systemd-stop lock should proceed during balance");

        assert_eq!(
            balance_pause_request_count(&runner),
            1,
            "expected running balance to be paused before umount"
        );
        assert_eq!(umount_request_count(&runner), 1, "expected one umount");
        assert_eq!(
            forget_requests(&runner),
            vec![vec![
                "/dev/mapper/braid-aaa".to_owned(),
                "/dev/mapper/braid-bbb".to_owned(),
            ]],
            "expected scoped forget after umount"
        );
        assert_eq!(
            cryptsetup_close_request_count(&runner),
            2,
            "expected both member mappers to close"
        );
    }

    // Intent: systemd-stop treats btrfs balance-pause exit 2 as the benign
    //   "balance finished before pause" race regardless of stderr wording.
    // Why it exists: btrfs-progs maps ENOTCONN uniquely to exit 2 on this
    //   subcommand, while its human-readable diagnostic can change across
    //   releases. Requiring both signals can strand shutdown before unmount.
    // Scenario: planning observes a running balance, but it finishes before
    //   execute issues the pause; teardown must warn and continue.
    #[test]
    fn systemd_stop_balance_pause_exit2_continues_regardless_of_wording() {
        let runner = mounted_systemd_stop_runner().with_output(
            CmdRequest::BtrfsBalancePause {
                mount_point: MountPoint::new("/mnt/storage".to_owned()),
            },
            lock_err_raw(
                "btrfs balance pause /mnt/storage",
                2,
                "Balance already completed.",
            ),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_excl_op("balance");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_lock_systemd_stop(&runner, &fs, &config, &membership));
        });
        result
            .expect("captured systemd-stop result")
            .expect("exit 2 should continue teardown");

        assert!(
            captured
                .lines()
                .any(|line| line == "[warn] pool: balance was no longer running -- continuing"),
            "expected benign-race warning, got:\n{captured}"
        );
        assert_eq!(umount_request_count(&runner), 1, "expected one umount");
        assert_eq!(
            cryptsetup_close_request_count(&runner),
            2,
            "expected both member mappers to close"
        );
    }

    // Intent: systemd-stop still fails closed when balance pause returns a
    //   non-ENOTCONN exit code.
    // Why it exists: accepting the benign exit-2 race must not turn every
    //   pause failure into permission to unmount and close pool devices.
    // Scenario: the pause ioctl fails with a generic btrfs error before
    //   teardown starts.
    #[test]
    fn systemd_stop_balance_pause_non_exit2_fails_before_unmount() {
        let runner = mounted_systemd_stop_runner().with_output(
            CmdRequest::BtrfsBalancePause {
                mount_point: MountPoint::new("/mnt/storage".to_owned()),
            },
            lock_err_raw("btrfs balance pause /mnt/storage", 1, "Input/output error"),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_excl_op("balance");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_systemd_stop(&runner, &fs, &config, &membership)
            .expect_err("non-exit-2 pause failure must abort teardown");

        assert!(
            matches!(err, LockError::Failed(ref msg) if msg == "btrfs balance pause /mnt/storage failed (exit 1): Input/output error"),
            "expected exact pause failure, got: {err:?}"
        );
        assert_eq!(
            umount_request_count(&runner),
            0,
            "pause failure must precede umount"
        );
        assert_eq!(
            cryptsetup_close_request_count(&runner),
            0,
            "pause failure must precede mapper close"
        );
    }

    // Intent: systemd-stop lock proceeds while a balance is paused, with the
    //   same paused-balance note as plain lock.
    // Why it exists: a previously-paused balance is still safe for the
    //   shutdown path because umount keeps it paused on disk. Both lock modes
    //   share one paused-balance rule, so the note must not differ by mode.
    // Scenario: ExecStop observes "balance paused" and must still run the
    //   ordered umount, forget, and LUKS close sequence.
    #[test]
    fn systemd_stop_proceeds_on_paused_balance() {
        let runner = mounted_systemd_stop_runner();
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_excl_op("balance paused");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_lock_systemd_stop(&runner, &fs, &config, &membership));
        });
        result
            .expect("captured systemd-stop result")
            .expect("systemd-stop lock should proceed during paused balance");

        assert!(
            captured
                .lines()
                .any(|line| line == format!("[warn] {PAUSED_BALANCE_NOTE}")),
            "expected paused-balance note, got:\n{captured}"
        );

        assert_eq!(
            balance_pause_request_count(&runner),
            0,
            "already-paused balance should not be paused again"
        );
        assert_eq!(umount_request_count(&runner), 1, "expected one umount");
        assert_eq!(
            forget_requests(&runner),
            vec![vec![
                "/dev/mapper/braid-aaa".to_owned(),
                "/dev/mapper/braid-bbb".to_owned(),
            ]],
            "expected scoped forget after umount"
        );
        assert_eq!(
            cryptsetup_close_request_count(&runner),
            2,
            "expected both member mappers to close"
        );
    }

    // Intent: systemd-stop lock retries busy umount beyond user-lock attempts.
    // Why it exists: if shutdown kills the Rust parent before its btrfs
    //   balance subprocess, btrfs-progs can hold the mount fd briefly after
    //   the pool lock is free.
    // Scenario: the first three umount attempts are busy, matching plain
    //   user-lock exhaustion, then the balance subprocess releases the mount
    //   and systemd-stop lock completes cleanup.
    #[test]
    fn systemd_stop_retries_busy_umount_beyond_user_attempts() {
        let busy_then_success = std::iter::repeat_with(umount_busy_output)
            .take(UMOUNT_RETRY_ATTEMPTS as usize)
            .chain(std::iter::once(lock_ok_raw("umount /mnt/storage")))
            .collect();
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::BtrfsBalancePause {
                mount_point: MountPoint::new("/mnt/storage".to_owned()),
            },
            lock_ok_raw("btrfs balance pause /mnt/storage"),
        )
        .with_output_sequence(umount_request(), busy_then_success)
        .with_output(
            CmdRequest::BtrfsDeviceScanForget {
                devices: vec![
                    "/dev/mapper/braid-aaa".into(),
                    "/dev/mapper/braid-bbb".into(),
                ],
            },
            lock_ok_raw("btrfs device scan --forget"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_ok_raw("cryptsetup close braid-aaa"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-bbb".into()),
            },
            lock_ok_raw("cryptsetup close braid-bbb"),
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_excl_op("balance");
        let config = lock_test_config();
        let membership = lock_test_membership();

        cmd_lock_systemd_stop(&runner, &fs, &config, &membership)
            .expect("systemd-stop lock should outwait transient balance holder");

        assert_eq!(
            balance_pause_request_count(&runner),
            1,
            "expected running balance to be paused before retrying umount"
        );
        assert_eq!(
            umount_request_count(&runner),
            UMOUNT_RETRY_ATTEMPTS as usize + 1,
            "systemd-stop should continue retrying after the user-lock budget"
        );
        assert_eq!(
            cryptsetup_close_request_count(&runner),
            2,
            "lock should close both member mappers after delayed umount"
        );
    }

    // Intent: systemd-stop lock rejects non-balance exclusive operations.
    // Why it exists: only balance has a verified safe umount quiesce path;
    //   other exclusive ops must fail before teardown mutates anything.
    // Scenario: ExecStop observes "device remove" in sysfs and refuses
    //   before issuing umount.
    #[test]
    fn systemd_stop_rejects_non_balance_op() {
        let runner = mounted_systemd_stop_runner();
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_excl_op("device remove");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_systemd_stop(&runner, &fs, &config, &membership)
            .expect_err("systemd-stop lock should reject non-balance exclop");
        let msg = err.to_string();
        assert!(
            msg.contains("device remove") && msg.contains("in progress"),
            "expected device-remove refusal, got: {msg}"
        );
        assert_eq!(
            balance_pause_request_count(&runner),
            0,
            "must refuse before pausing balance"
        );
        assert_eq!(
            umount_request_count(&runner),
            0,
            "must refuse before umount"
        );
    }

    #[test]
    // Intent: lock refuses when any exclusive op is active (running balance).
    // Why: unmounting during an exclusive op is unsafe — data corruption risk.
    // Scenario: a RAID1 balance is in progress, operator runs `braid lock`.
    //   Lock must refuse without unmounting or closing any mappers.
    fn lock_refuses_when_exclusive_op_active() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ));
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_excl_op("balance");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should refuse — balance is active");
        let msg = err.to_string();
        assert!(
            msg.contains("balance") && msg.contains("in progress"),
            "expected active-op refusal, got: {msg}"
        );
    }

    // Intent: the `ProbeFailed` fallback arm runs the exclusive-op preflight
    //   and refuses on an active balance before unmounting or closing any
    //   mapper.
    // Why it exists: the FSID preflight is the only guard between fallback
    //   unmount and an in-flight exclusive op; dropping it from this arm would
    //   risk unmount during balance.
    // Scenario: a mounted pool's per-device probe fails while a balance runs;
    //   the operator runs `braid lock`.
    #[test]
    fn lock_probe_failed_refuses_when_exclusive_op_active() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output_sequence(
            CmdRequest::CryptsetupStatus {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            vec![lock_err_raw(
                "cryptsetup status braid-aaa",
                5,
                "transient status failure",
            )],
        );
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_excl_op("balance");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should refuse before fallback cleanup");
        let msg = err.to_string();
        assert!(
            msg.contains("balance") && msg.contains("in progress"),
            "expected active-op refusal, got: {msg}"
        );
        assert_eq!(
            umount_request_count(&runner),
            0,
            "fallback must refuse before umount"
        );
        assert_eq!(
            cryptsetup_close_request_count(&runner),
            0,
            "fallback must refuse before mapper close"
        );
    }

    // Intent: plain `braid lock` proceeds under a paused balance. The dry-run
    //   preview and the real run both warn that the balance stays paused, and
    //   the real run issues no pause, unmounts once, and closes every member.
    // Why it exists: lock used to refuse a paused balance with "Wait for it to
    //   finish first". A paused balance never finishes on its own, so the
    //   operator waited forever. Unmount keeps a paused balance paused on disk
    //   and braid mounts with skip_balance, so the refusal protected nothing.
    // Scenario: braid's shutdown path paused a balance. After the next unlock,
    //   the operator runs `braid lock --dry-run`, then `braid lock`.
    #[test]
    fn lock_proceeds_on_paused_balance() {
        const NOTE: &str = "[warn] btrfs balance is paused -- lock leaves it paused, \
                            and the next braid unlock and braid doctor will report it";
        let runner = || {
            lock_mounted_runner()
                .with_output(
                    CmdRequest::CryptsetupClose {
                        mapper: MapperName::from_basename("braid-aaa".into()),
                    },
                    lock_ok_raw("cryptsetup close braid-aaa"),
                )
                .with_output(
                    CmdRequest::CryptsetupClose {
                        mapper: MapperName::from_basename("braid-bbb".into()),
                    },
                    lock_ok_raw("cryptsetup close braid-bbb"),
                )
        };
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_excl_op("balance paused");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let preview = plan_lock(&runner(), &fs, &config, &membership, LockMode::User)
            .expect("dry-run plan should proceed under a paused balance")
            .preview()
            .render();
        assert!(
            preview.lines().any(|line| line == NOTE),
            "expected paused-balance note in preview, got:\n{preview}"
        );

        let runner = runner();
        let mut result = None;
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            result = Some(cmd_lock_impl(
                &runner,
                &fs,
                &LockNoopSleeper,
                &config,
                &membership,
                false,
            ));
        });
        result
            .expect("captured lock result")
            .expect("lock should proceed under a paused balance");

        assert!(
            captured.lines().any(|line| line == NOTE),
            "expected paused-balance note on stderr, got:\n{captured}"
        );
        assert_eq!(
            balance_pause_request_count(&runner),
            0,
            "an already-paused balance needs no pause request"
        );
        assert_eq!(umount_request_count(&runner), 1, "expected one umount");
        assert_eq!(
            cryptsetup_close_request_count(&runner),
            2,
            "expected both member mappers to close"
        );
    }

    // Intent: cmd_lock preserves probe_pool's NotBtrfs contract through
    //   probe_fsid -- if the mount point is held by a non-btrfs
    //   filesystem, lock fails with a typed message naming the fstype
    //   rather than a generic btrfs-show parse failure.
    // Why: the refactor from probe_pool to probe_fsid dropped per-device
    //   cryptsetup checks; it must NOT also drop the mounted-non-btrfs
    //   check. Without this guard, an ext4-mounted /mnt/storage would
    //   fall through to `btrfs filesystem show`, fail with a confusing
    //   parse error, and mask the real mount-configuration issue.
    // Scenario: MountpointCheck succeeds, mountinfo reports the mount
    //   point's fstype as ext4. cmd_lock must refuse with a
    //   LockError::Failed whose message mentions "not btrfs".
    #[test]
    fn lock_rejects_mounted_but_not_btrfs() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_mountinfo("36 35 0:32 / /mnt/storage rw,noatime shared:1 - ext4 /dev/sda1 rw\n");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should refuse -- mount is not btrfs");
        let msg = err.to_string();
        assert!(
            msg.contains("not btrfs") && msg.contains("ext4"),
            "expected NotBtrfs-style message naming ext4, got: {msg}"
        );
    }

    /// Test-local helper: build a member-owned close entry from a bare
    /// mapper basename and disk name for compile_lock_steps tests.
    fn member_close(mapper: &str, disk_name: &str) -> LockMapperClose {
        LockMapperClose {
            mapper: MapperName::from_basename(mapper.into()),
            kind: LockMapperCloseKind::MemberOwned {
                display_name: DiskName::parse(disk_name).expect("valid test disk name"),
            },
        }
    }

    fn orphan_close(mapper: &str, disk_name: &str) -> LockMapperClose {
        LockMapperClose {
            mapper: MapperName::from_basename(mapper.into()),
            kind: LockMapperCloseKind::Orphan {
                disk_name: disk_name.into(),
            },
        }
    }

    fn test_close_set(
        members: Vec<LockMapperClose>,
        orphans: Vec<LockMapperClose>,
    ) -> LockCloseSet {
        LockCloseSet::from_classified(members, orphans)
    }

    fn member_summaries(close_set: &LockCloseSet) -> Vec<(String, String)> {
        close_set
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                LockMapperCloseKind::MemberOwned { display_name } => Some((
                    entry.mapper.as_str().to_owned(),
                    display_name.as_str().to_owned(),
                )),
                LockMapperCloseKind::Orphan { .. } => None,
            })
            .collect()
    }

    fn orphan_summaries(close_set: &LockCloseSet) -> Vec<(String, String)> {
        close_set
            .entries()
            .iter()
            .filter_map(|entry| match &entry.kind {
                LockMapperCloseKind::MemberOwned { .. } => None,
                LockMapperCloseKind::Orphan { disk_name } => {
                    Some((entry.mapper.as_str().to_owned(), disk_name.clone()))
                }
            })
            .collect()
    }

    // Intent: dry-run for lock shows umount + scan forget + close per open mapper.
    // Why: verifies compile_lock_steps produces correct output. The
    // rendered forget command must include the explicit device paths,
    // not the bare kernel-global form.
    // Scenario: pool mounted, 2 open mappers, no orphans.
    #[test]
    fn dry_run_render_lock_mounted_2_disks() {
        use crate::types::MountPoint;
        let mount_point = MountPoint::new("/mnt/storage".into());
        let close_set = test_close_set(
            vec![
                member_close("braid-disk1", "disk1"),
                member_close("braid-disk2", "disk2"),
            ],
            vec![],
        );
        let steps = compile_lock_steps(true, false, &[], &close_set, &mount_point);
        let output = Step::render_dry_run(&steps);
        assert_eq!(
            steps.len(),
            4,
            "mounted 2-disk lock must emit umount + scan forget + 2 closes; got {:?}",
            steps
        );
        assert_lines_in_order(
            &output,
            &[
                "unmount",
                "$ umount",
                "btrfs device scan --forget",
                "$ btrfs device scan --forget",
                "close LUKS mapper braid-disk1",
                "close LUKS mapper braid-disk2",
            ],
        );
        let forget_line = output
            .lines()
            .nth(line_index(&output, "$ btrfs device scan --forget"))
            .expect("forget command line present");
        assert!(
            forget_line.contains("--forget /dev/mapper/braid-disk1 /dev/mapper/braid-disk2"),
            "rendered forget command must list pool mapper paths, got: {}",
            forget_line
        );
    }

    // Intent: dry-run when not mounted skips umount/scan, shows only close.
    // Why: verifies conditional step omission.
    // Scenario: pool not mounted, 1 mapper still open.
    #[test]
    fn dry_run_lock_not_mounted_1_open() {
        use crate::types::MountPoint;
        let mount_point = MountPoint::new("/mnt/storage".into());
        let close_set = test_close_set(vec![member_close("braid-disk1", "disk1")], vec![]);
        let steps = compile_lock_steps(false, false, &[], &close_set, &mount_point);
        let output = Step::render_dry_run(&steps);
        assert_eq!(steps.len(), 1, "not-mounted lock must emit one close step");
        assert_lines_in_order(&output, &["close LUKS mapper", "$ cryptsetup close"]);
        assert!(!output.contains("unmount"));
    }

    // Intent: dry-run when nothing to do returns empty steps.
    // Why: verifies the "nothing to do" case.
    // Scenario: pool not mounted, all mappers closed, no orphans.
    #[test]
    fn dry_run_lock_nothing_to_do() {
        use crate::types::MountPoint;
        let mount_point = MountPoint::new("/mnt/storage".into());
        let close_set = test_close_set(vec![], vec![]);
        let steps = compile_lock_steps(false, false, &[], &close_set, &mount_point);
        assert!(steps.is_empty());
    }

    // Intent: the compiled dry-run plan's forget step lists the pool's
    // own mapper paths, never the kernel-global no-arg form.
    // Why: the no-arg form (btrfs_forget_devices(NULL) in
    // reference/btrfs-progs/cmds/device.c) invalidates every btrfs scan
    // entry on the host. Pool-scoping matters as soon as a second
    // (non-braid) btrfs filesystem coexists.
    // Scenario: 2-disk pool, no orphans; the forget step must carry
    // exactly the pool's mapper paths in membership order.
    #[test]
    fn dry_run_lock_forget_step_lists_scoped_devices() {
        use crate::types::MountPoint;
        let mount_point = MountPoint::new("/mnt/storage".into());
        let close_set = test_close_set(
            vec![
                member_close("braid-aaa", "aaa"),
                member_close("braid-bbb", "bbb"),
            ],
            vec![],
        );
        let steps = compile_lock_steps(true, false, &[], &close_set, &mount_point);
        assert_eq!(
            lock_forget_step_devices(&steps),
            vec![
                "/dev/mapper/braid-aaa".to_string(),
                "/dev/mapper/braid-bbb".to_string(),
            ],
        );
    }

    // Intent: dry-run's forget step unions membership + orphan mappers
    // -- the exact set compile_lock_steps will also close below it.
    // Why: the kernel forget path is per-device, not per-fsid
    // (reference/linux/fs/btrfs/volumes.c btrfs_free_stale_devices).
    // Forgetting only membership leaves an orphan mapper (from a prior
    // crash between cryptsetup open and pool.json write, per
    // docs/design/principles.md#3-safe-by-construction-operations) with a stale scan entry, reviving the
    // cryptsetup-close-btrfs-held race for the orphan.
    // Scenario: 1 membership mapper, 1 orphan; forget devices = union.
    #[test]
    fn dry_run_lock_forget_step_includes_orphans() {
        use crate::types::MountPoint;
        let mount_point = MountPoint::new("/mnt/storage".into());
        let close_set = test_close_set(
            vec![member_close("braid-aaa", "aaa")],
            vec![orphan_close("braid-orphan", "orphan")],
        );
        let steps = compile_lock_steps(true, false, &[], &close_set, &mount_point);
        assert_eq!(
            lock_forget_step_devices(&steps),
            vec![
                "/dev/mapper/braid-aaa".to_string(),
                "/dev/mapper/braid-orphan".to_string(),
            ],
        );
    }

    // Intent: the forget step is omitted entirely when there are no
    // mappers to close, even if the pool was mounted.
    // Why: a forget call with no arguments is kernel-global. The only
    // safe way to express "forget nothing" is to not issue the command
    // at all.
    // Scenario: pool_was_mounted=true but membership and orphan lists
    // are both empty -- only the umount step remains in the plan.
    #[test]
    fn dry_run_lock_forget_step_omitted_when_no_mappers() {
        use crate::types::MountPoint;
        let mount_point = MountPoint::new("/mnt/storage".into());
        let close_set = test_close_set(vec![], vec![]);
        let steps = compile_lock_steps(true, false, &[], &close_set, &mount_point);
        assert_eq!(
            lock_count_forget_steps(&steps),
            0,
            "no forget step expected"
        );
        assert!(
            steps.iter().any(|s| s
                .commands
                .iter()
                .any(|c| matches!(c, CmdRequest::Umount { .. }))),
            "umount step should still be emitted",
        );
    }

    // Intent: systemd-stop dry-run includes a balance pause before umount
    // when planning observed a running balance, and the unit stops sit
    // between the pause and the umount.
    // Why it exists: preview and execute must share the same ordered mutation
    // path, so the generated plan cannot omit the quiesce request or move the
    // unit stops away from the umount they free.
    // Scenario: UPS shutdown reaches ExecStop during an in-flight balance and
    // lock previews pause -> stop -> umount -> close.
    #[test]
    fn dry_run_lock_systemd_stop_pause_precedes_umount() {
        use crate::types::MountPoint;
        let mount_point = MountPoint::new("/mnt/storage".into());
        let close_set = test_close_set(vec![member_close("braid-aaa", "aaa")], vec![]);
        let unit_stops = [LockUnitStop::BoundConsumer("smbd.service".into())];
        let steps = compile_lock_steps(true, true, &unit_stops, &close_set, &mount_point);

        let position = |is_step: fn(&CmdRequest) -> bool, what: &str| {
            steps
                .iter()
                .position(|step| step.commands.iter().any(is_step))
                .unwrap_or_else(|| panic!("{what} step should be present: {steps:?}"))
        };
        let pause_position = position(
            |cmd| matches!(cmd, CmdRequest::BtrfsBalancePause { .. }),
            "pause",
        );
        let stop_position = position(
            |cmd| matches!(cmd, CmdRequest::SystemctlStop { .. }),
            "stop",
        );
        let umount_position = position(|cmd| matches!(cmd, CmdRequest::Umount { .. }), "umount");

        assert!(
            pause_position < stop_position && stop_position < umount_position,
            "steps must run pause -> stop -> umount: {steps:?}"
        );
    }

    // Intent: `braid lock` scopes the forget request to the pool's own
    // mappers, never the kernel-global no-arg form.
    // Why: the no-arg form invalidates every btrfs scan entry on the
    // host (reference/btrfs-progs/cmds/device.c:btrfs_forget_devices
    // with path=NULL). Pool-scoping prevents `braid lock` from
    // clobbering scan state for an unrelated btrfs filesystem.
    // Scenario: 2-disk pool, no orphans; the recorded forget call
    // carries exactly the pool's mapper paths.
    #[test]
    fn lock_forget_is_pool_scoped() {
        let inner = lock_mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let runner = LockRecordingRunner::new(inner);
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed");

        assert_eq!(
            runner.forget_calls(),
            vec![vec![
                "/dev/mapper/braid-aaa".to_string(),
                "/dev/mapper/braid-bbb".to_string(),
            ]],
            "forget must be pool-scoped (not kernel-global, not membership-only)"
        );
    }

    // Intent: `braid lock` forgets the full close set -- membership AND
    // orphan mappers.
    // Why: the kernel forget path is per-device
    // (reference/linux/fs/btrfs/volumes.c btrfs_free_stale_devices).
    // Membership-only forget leaves crash-created orphan mappers with
    // stale scan entries, reviving the cryptsetup-close-btrfs-held
    // race that BtrfsDeviceScanForget exists to prevent (see
    // tests/repro/cryptsetup-close-btrfs-held.py).
    // Scenario: 2-disk pool + 1 orphan (braid-ccc); the recorded forget
    // call carries all three mapper paths.
    #[test]
    fn lock_forget_includes_orphan_mappers() {
        let inner = lock_mounted_runner()
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec![
                        "/dev/mapper/braid-aaa".into(),
                        "/dev/mapper/braid-bbb".into(),
                        "/dev/mapper/braid-ccc".into(),
                    ],
                },
                lock_ok_raw("btrfs device scan --forget"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: MapperName::from_basename("braid-ccc".into()),
                },
                lock_ok_raw("cryptsetup close braid-ccc"),
            );
        let runner = LockRecordingRunner::new(inner);
        let fs = lock_member_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ])
        .with_luks_dm("braid-ccc", ORPHAN_UUID);
        let config = lock_test_config();
        let membership = lock_test_membership();

        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed and close orphan");

        assert_eq!(
            runner.forget_calls(),
            vec![vec![
                "/dev/mapper/braid-aaa".to_string(),
                "/dev/mapper/braid-bbb".to_string(),
                "/dev/mapper/braid-ccc".to_string(),
            ]],
            "forget must include the orphan mapper in the close set"
        );
    }

    // Intent: umount_with_retry sleeps exactly UMOUNT_RETRY_DELAY between
    //   busy attempts, and the production value remains 500ms.
    // Why it exists: the retry delay covers a kernel-side file-descriptor
    //   release race after SMB/NFS consumers stop. Other tests use
    //   LockNoopSleeper, so they cannot catch a regression that zeroes,
    //   removes, or bypasses the production sleep.
    // Scenario: umount reports "target is busy" for all retry attempts; the
    //   RecordingSleeper captures one 500ms sleep between each attempt pair
    //   and the helper returns the final umount failure.
    #[test]
    fn umount_with_retry_sleeps_prod_delay_between_busy_attempts() {
        struct RecordingSleeper(Mutex<Vec<Duration>>);
        impl Sleeper for RecordingSleeper {
            fn sleep(&self, d: Duration) {
                self.0.lock().unwrap().push(d);
            }
        }

        let sleeper = RecordingSleeper(Mutex::new(Vec::new()));
        let runner = MockRunner::default().with_output_sequence(
            umount_request(),
            vec![
                umount_busy_output(),
                umount_busy_output(),
                umount_busy_output(),
            ],
        );
        let mount_point = MountPoint::new("/mnt/storage".into());

        let err = umount_with_retry(
            &runner,
            &sleeper,
            &mount_point,
            false,
            UMOUNT_RETRY_ATTEMPTS,
        )
        .expect_err("should exhaust retries and return umount failure");
        assert!(
            matches!(err, LockError::Failed(_)),
            "expected LockError::Failed after retry exhaustion, got: {err:?}"
        );

        let recorded = sleeper.0.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            (UMOUNT_RETRY_ATTEMPTS - 1) as usize,
            "expected one sleep between each pair of attempts, got: {recorded:?}"
        );
        for d in &recorded {
            assert_eq!(
                *d, UMOUNT_RETRY_DELAY,
                "each retry must sleep UMOUNT_RETRY_DELAY, got: {recorded:?}"
            );
        }
        assert_eq!(
            UMOUNT_RETRY_DELAY,
            Duration::from_millis(500),
            "prod UMOUNT_RETRY_DELAY must stay 500ms; if you intend to \
             change this, update the kernel-race justification in the \
             commit message"
        );
    }

    /*
     * Intent: close_mapper_with_retry sleeps exactly CLOSE_RETRY_DELAY
     *   between busy attempts, and the prod value of CLOSE_RETRY_DELAY
     *   remains 500ms.
     *
     * Why it exists: the retry delay papers over a kernel-level race
     *   between umount and cryptsetup close on multi-device btrfs (see
     *   commit 1484ff1 and tests/repro/cryptsetup-close-btrfs-held.py).
     *   The repro test is race-dependent and the CLI-level VM test
     *   braid-lock-btrfs-held.py relies on the same race to trigger the
     *   retry path -- neither deterministically catches a regression
     *   that removes, zeroes, or bypasses the sleep. This test locks
     *   the contract at the helper.
     *
     * Scenario: a busy close error repeats for all CLOSE_RETRY_ATTEMPTS
     *   tries; the RecordingSleeper captures (CLOSE_RETRY_ATTEMPTS - 1)
     *   sleep calls, each exactly CLOSE_RETRY_DELAY, and the returned
     *   error is DeviceBusy.
     */
    #[test]
    fn close_mapper_with_retry_sleeps_prod_delay_between_busy_attempts() {
        struct RecordingSleeper(Mutex<Vec<Duration>>);
        impl Sleeper for RecordingSleeper {
            fn sleep(&self, d: Duration) {
                self.0.lock().unwrap().push(d);
            }
        }

        let sleeper = RecordingSleeper(Mutex::new(Vec::new()));
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupClose {
                mapper: MapperName::from_basename("braid-aaa".into()),
            },
            lock_err_raw(
                "cryptsetup close braid-aaa",
                5,
                "Device braid-aaa is still in use.",
            ),
        );

        let err = close_mapper_with_retry(
            &runner,
            &sleeper,
            &MapperName::from_basename("braid-aaa".into()),
            false,
        )
        .expect_err("should exhaust retries and return DeviceBusy");
        assert!(
            matches!(err, CloseMapperError::DeviceBusy(_)),
            "expected DeviceBusy after retry exhaustion, got: {err:?}"
        );

        let recorded = sleeper.0.lock().unwrap().clone();
        assert_eq!(
            recorded.len(),
            (CLOSE_RETRY_ATTEMPTS - 1) as usize,
            "expected one sleep between each pair of attempts, got: {recorded:?}"
        );
        for d in &recorded {
            assert_eq!(
                *d, CLOSE_RETRY_DELAY,
                "each retry must sleep CLOSE_RETRY_DELAY, got: {recorded:?}"
            );
        }
        assert_eq!(
            CLOSE_RETRY_DELAY,
            Duration::from_millis(500),
            "prod CLOSE_RETRY_DELAY must stay 500ms; if you intend to \
             change this, update the kernel-race justification in the \
             commit message"
        );
    }

    // -- Migration-Phase-4 lock tests -----------------------------------

    /// Build a synthetic 2-disk PoolState whose mappers are the given
    /// observed names and whose LUKS UUIDs match
    /// `lock_test_membership` so the Full-arm classifier yields two
    /// member-owned close entries.
    fn synthetic_pool_state(mapper_aaa: &str, mapper_bbb: &str) -> crate::types::PoolState {
        use crate::types::{LuksUuid, MapperName, PoolDevice, PoolState};
        PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName::from_basename(mapper_aaa.into()),
                    luks_uuid: LuksUuid::parse("00000000-0000-0000-0000-0000000002bc").unwrap(),
                    devid: Devid::new(1),
                    underlying: "/dev/disk/by-id/a".into(),
                },
                PoolDevice {
                    mapper: MapperName::from_basename(mapper_bbb.into()),
                    luks_uuid: LuksUuid::parse("00000000-0000-0000-0000-0000000002bd").unwrap(),
                    devid: Devid::new(2),
                    underlying: "/dev/disk/by-id/b".into(),
                },
            ],
            missing_count: 0,
            total_devices: 2,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    fn synthetic_pool_state_with_null_underlying(
        mapper_aaa: &str,
        null_mapper: &str,
        null_uuid: &str,
        null_devid: Devid,
    ) -> crate::types::PoolState {
        use crate::types::{LuksUuid, MapperName, NullUnderlyingDevice, PoolDevice, PoolState};
        PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename(mapper_aaa.into()),
                luks_uuid: LuksUuid::parse("00000000-0000-0000-0000-0000000002bc").unwrap(),
                devid: Devid::new(1),
                underlying: "/dev/disk/by-id/a".into(),
            }],
            missing_count: 1,
            total_devices: 2,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![],
            null_underlying: vec![NullUnderlyingDevice {
                mapper: MapperName::from_basename(null_mapper.into()),
                luks_uuid: LuksUuid::parse(null_uuid).unwrap(),
                devid: null_devid,
            }],
        }
    }

    /// Intent: in `Snapshot::Probed`, a drifted member mapper
    /// (`PoolDevice.mapper = "braid-WRONG"`, luks_uuid matches
    /// member) appears in member_owned via UUID classification,
    /// renders into the close-set with the OBSERVED name, and the
    /// forget set carries the observed name.
    /// Why: pins the "close observed, not reconstructed" doctrine
    /// from plan 3369-3404. A regression that reconstructed the
    /// mapper from membership names would close the wrong dm slot
    /// (or skip the close after fs.exists filtering).
    /// Scenario: seed 700 -- post-migration lock seeing a drifted
    /// member mapper.
    #[test]
    fn full_arm_classifies_drifted_member_by_uuid_into_member_owned() {
        let fs = lock_fs(&["/dev/mapper/braid-WRONG", "/dev/mapper/braid-bbb"]);
        let membership = lock_test_membership();
        let pool = synthetic_pool_state("braid-WRONG", "braid-bbb");
        let mut acc = CloseSetAccumulator::default();
        let close_set = build_close_sets_full(&fs, &pool, &membership, &mut acc);
        // Both members are classified by UUID despite the drift.
        let observed: Vec<String> = member_summaries(&close_set)
            .into_iter()
            .map(|(mapper, _display)| mapper)
            .collect();
        assert_eq!(
            observed,
            vec!["braid-WRONG".to_owned(), "braid-bbb".to_owned()]
        );
        let display: Vec<String> = member_summaries(&close_set)
            .into_iter()
            .map(|(_mapper, display)| display)
            .collect();
        assert_eq!(display, vec!["aaa".to_owned(), "bbb".to_owned()]);
        assert!(orphan_summaries(&close_set).is_empty());
    }

    // Intent: a drifted member mapper classified in Pass 1 is not also
    //   reported as a confidently closed member.
    // Why it exists: the already-closed prelude must consume planner
    //   presence facts, not reconstruct expected mapper names during execute.
    // Scenario: btrfs reports member `aaa` as live under `braid-WRONG`.
    #[test]
    fn full_arm_drifted_member_is_not_known_closed() {
        let fs = lock_fs(&["/dev/mapper/braid-WRONG", "/dev/mapper/braid-bbb"])
            .with_luks_dm("braid-WRONG", AAA_UUID)
            .with_luks_dm("braid-bbb", BBB_UUID);
        let runner = mounted_runner_with_btrfs_show("braid-WRONG", "braid-bbb");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::User)
            .expect("plan should succeed");

        assert!(
            member_summaries(&plan.close_set).contains(&("braid-WRONG".into(), "aaa".into())),
            "drifted mapper must be planned as member-owned: {:?}",
            member_summaries(&plan.close_set)
        );
        assert!(
            plan.members_known_closed.is_empty(),
            "live drifted members must not enter known-closed prelude: {:?}",
            known_closed_names(&plan)
        );
    }

    /// Intent: in `Snapshot::Probed`, the forget_devs set passed to
    /// BtrfsDeviceScanForget uses the OBSERVED member mapper string
    /// (`braid-WRONG`) rather than the reconstructed
    /// `mapper_name(&member.name)` string.
    /// Why: plan 3399-3404. Reconstructed names would be filtered
    /// out by `forget_devs.retain(|p| fs.exists(p))` and the kernel
    /// scan registry would retain the stale dm-uuid entry.
    /// Scenario: seed 701.
    #[test]
    fn full_arm_forget_set_uses_observed_mapper_on_drift() {
        let fs = lock_fs(&["/dev/mapper/braid-WRONG", "/dev/mapper/braid-bbb"]);
        let membership = lock_test_membership();
        let pool = synthetic_pool_state("braid-WRONG", "braid-bbb");
        let mut acc = CloseSetAccumulator::default();
        let close_set = build_close_sets_full(&fs, &pool, &membership, &mut acc);
        let mp = MountPoint::new("/mnt/storage".into());
        let steps = super::compile_lock_steps(true, false, &[], &close_set, &mp);
        assert_eq!(
            lock_forget_step_devices(&steps),
            vec![
                "/dev/mapper/braid-WRONG".to_string(),
                "/dev/mapper/braid-bbb".to_string(),
            ],
            "forget set must use observed mapper, not reconstructed",
        );
    }

    // Intent: a PoolDevice whose LUKS UUID is absent from membership is
    //   classified as orphan and emits the orphan warn.
    // Why it exists: pins the Pass 1 orphan path so it cannot regress
    //   back to a silent demotion while Pass 2 still warns.
    // Scenario: a post-migration lock sees a live mapper whose backing
    //   UUID does not match any pool.json member after an interrupted
    //   replace left a stale mapper behind.
    #[test]
    fn full_arm_pass1_unknown_uuid_classifies_as_orphan_and_warns() {
        use crate::types::{LuksUuid, MapperName, PoolDevice, PoolState};

        let fs = lock_fs(&["/dev/mapper/braid-leftover"]);
        let membership = lock_test_membership();
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-leftover".into()),
                luks_uuid: LuksUuid::parse(ORPHAN_UUID).unwrap(),
                devid: Devid::new(99),
                underlying: "/dev/disk/by-id/leftover".into(),
            }],
            missing_count: 0,
            total_devices: 1,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        };
        let mut acc = CloseSetAccumulator::default();

        let close_set = build_close_sets_full(&fs, &pool, &membership, &mut acc);

        assert!(member_summaries(&close_set).is_empty());
        assert_eq!(
            orphan_summaries(&close_set),
            vec![("braid-leftover".to_owned(), "leftover".to_owned())]
        );
        assert!(
            acc.notes.iter().any(|note| matches!(
                note,
                PreviewNote::Warn(body) if body.contains("orphaned mapper braid-leftover")
            )),
            "orphan warning expected, got: {:?}",
            acc.notes
        );
        assert!(!acc.cleanup_uncertain);
    }

    // Intent: a null_underlying entry whose LUKS UUID is absent from
    //   membership is classified as orphan and emits the orphan warn.
    // Why it exists: a null-backed mapper classifies by the UUID its dm UUID
    //   records, exactly like a present pool device, so a foreign one must
    //   warn rather than demote silently.
    // Scenario: a hot-unplugged device leaves an open mapper whose LUKS UUID
    //   never landed in pool.json because `braid add` was interrupted before
    //   membership was committed.
    #[test]
    fn full_arm_null_underlying_unknown_uuid_classifies_as_orphan_and_warns() {
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-ghost"]);
        let membership = lock_test_membership();
        let pool = synthetic_pool_state_with_null_underlying(
            "braid-aaa",
            "braid-ghost",
            ORPHAN_UUID,
            Devid::new(99),
        );
        let mut acc = CloseSetAccumulator::default();

        let close_set = build_close_sets_full(&fs, &pool, &membership, &mut acc);

        assert!(
            orphan_summaries(&close_set).contains(&("braid-ghost".to_owned(), "ghost".to_owned())),
            "braid-ghost must be an orphan: {:?}",
            orphan_summaries(&close_set)
        );
        assert!(
            acc.notes.iter().any(|note| matches!(
                note,
                PreviewNote::Warn(body) if body.contains("orphaned mapper braid-ghost")
            )),
            "orphan warning expected, got: {:?}",
            acc.notes
        );
        assert!(!acc.cleanup_uncertain);
    }

    // Intent: scan_braid_mapper_candidates keeps named `braid-<name>`
    //   mappers but excludes a bare `braid-` (empty disk name).
    // Why it exists: a bare `braid-` is never braid-created (DiskName is
    //   never empty), so it must not be a cleanup candidate; pins the
    //   candidacy fix that routes the filter through braid_disk_name.
    // Scenario: an operator hand-created a dm device literally named
    //   `braid-` alongside real braid mappers; lock's scan must skip it.
    #[test]
    fn scan_excludes_bare_braid_mapper() {
        let fs = lock_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-",
            "/dev/mapper/braid-bbb",
        ]);
        let candidates = scan_braid_mapper_candidates(&fs, &HashSet::new()).unwrap();
        let names: Vec<&str> = candidates.iter().map(|m| m.as_str()).collect();
        assert_eq!(names, vec!["braid-aaa", "braid-bbb"]);
    }

    // Intent: a Pass 1 PoolDevice with a bare `braid-` mapper and a
    //   non-member LUKS UUID renders its orphan label as the full mapper
    //   basename (`braid-`), never a blank.
    // Why it exists: pins the orphan_disk_label reroute at the Pass 1
    //   site so a degenerate bare-`braid-` mapper can never emit a blank
    //   disk label in the orphan status row.
    // Scenario: a btrfs pool is backed by a dm device literally named
    //   `braid-` whose backing UUID is not a member -- the most
    //   degenerate orphan path that still reaches a label site.
    #[test]
    fn full_arm_pass1_bare_braid_orphan_label_is_not_blank() {
        use crate::types::{LuksUuid, MapperName, PoolDevice, PoolState};

        let fs = lock_fs(&["/dev/mapper/braid-"]);
        let membership = lock_test_membership();
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName::from_basename("braid-".into()),
                luks_uuid: LuksUuid::parse(ORPHAN_UUID).unwrap(),
                devid: Devid::new(99),
                underlying: "/dev/disk/by-id/leftover".into(),
            }],
            missing_count: 0,
            total_devices: 1,
            fsid: Some(Fsid::parse("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee").unwrap()),
            missing_devids: vec![],
            null_underlying: vec![],
        };
        let mut acc = CloseSetAccumulator::default();

        let close_set = build_close_sets_full(&fs, &pool, &membership, &mut acc);

        assert!(member_summaries(&close_set).is_empty());
        assert_eq!(
            orphan_summaries(&close_set),
            vec![("braid-".to_owned(), "braid-".to_owned())]
        );
    }

    // Intent: a null_underlying entry with a bare `braid-` mapper and a
    //   non-member LUKS UUID renders its orphan label as the full mapper
    //   basename (`braid-`), never a blank.
    // Why it exists: pins the orphan_disk_label reroute for null-backed pool
    //   devices so the degenerate bare-`braid-` mapper can never emit a blank
    //   disk label there either.
    // Scenario: a null-underlying mapper literally named `braid-` records a
    //   LUKS UUID that never landed in pool.json.
    #[test]
    fn full_arm_null_underlying_bare_braid_orphan_label_is_not_blank() {
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-"]);
        let membership = lock_test_membership();
        let pool = synthetic_pool_state_with_null_underlying(
            "braid-aaa",
            "braid-",
            ORPHAN_UUID,
            Devid::new(99),
        );
        let mut acc = CloseSetAccumulator::default();

        let close_set = build_close_sets_full(&fs, &pool, &membership, &mut acc);

        assert!(
            orphan_summaries(&close_set).contains(&("braid-".to_owned(), "braid-".to_owned())),
            "bare braid- must be an orphan with a non-blank label: {:?}",
            orphan_summaries(&close_set)
        );
    }

    // Intent: in `Snapshot::Probed`, a pool device whose backing disk is gone
    //   is closed as the member its dm UUID records, and the members it does
    //   not account for stay known-closed.
    // Why it exists: lock once bound a null-backed mapper through its btrfs
    //   devid and pool.json, so a missing or duplicated devid skipped the
    //   close and left the volume key in kernel memory; the dm UUID needs
    //   neither.
    // Scenario: the pool is mounted with `aaa` present and `bbb`
    //   hot-unplugged (`cryptsetup status braid-bbb` reports `device: (null)`);
    //   pool.json holds no devids, and member `ccc` is not open.
    #[test]
    fn full_arm_null_underlying_member_closes_by_dm_uuid() {
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint::new("/mnt/storage".to_owned()).into(),
                },
                lock_ok_raw("mountpoint -q /mnt/storage"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint::new("/mnt/storage".to_owned()),
                },
                btrfs_show_for_lock_mappers("braid-aaa", "braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-aaa".into()),
                },
                cryptsetup_status_active("braid-aaa", "/dev/disk/by-id/a"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName::from_basename("braid-bbb".into()),
                },
                cryptsetup_status_active_null("braid-bbb"),
            );
        let config = lock_test_config();
        let membership = lock_test_membership_with_ccc();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::User)
            .expect("plan should succeed");

        assert_eq!(
            plan.close_set.entries(),
            [
                member_close("braid-aaa", "aaa"),
                member_close("braid-bbb", "bbb")
            ]
        );
        assert!(!plan.cleanup_uncertain, "notes: {:?}", plan.notes);
        assert_eq!(known_closed_names(&plan), vec!["ccc"]);
    }

    /// Intent: the UUID-scanned fallback preserves the close order
    /// (member-owned before orphan) while deriving both classes from
    /// the LUKS UUIDs the mappers' dm UUIDs record, not mapper names.
    /// Why: fallback cleanup must stay useful without regressing to
    /// mapper-name ownership.
    /// Scenario: seed 702.
    #[test]
    fn uuid_scanned_fallback_preserves_member_then_orphan_close_order() {
        let fs = lock_member_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ])
        .with_luks_dm("braid-ccc", ORPHAN_UUID);
        let membership = lock_test_membership();
        let mut acc = CloseSetAccumulator::default();
        let close_set = build_close_sets_uuid_scanned_fallback(&fs, &membership, &mut acc);
        let mp = MountPoint::new("/mnt/storage".into());
        let steps = super::compile_lock_steps(true, false, &[], &close_set, &mp);
        let forget = lock_forget_step_devices(&steps);
        // Members first, orphan last -- mirroring LockCloseSet order.
        assert_eq!(
            forget,
            vec![
                "/dev/mapper/braid-aaa".to_string(),
                "/dev/mapper/braid-bbb".to_string(),
                "/dev/mapper/braid-ccc".to_string(),
            ]
        );
        assert!(
            !acc.cleanup_uncertain,
            "verified candidates are complete cleanup"
        );
    }

    /// Intent: the mounted fallback warning carries the two pinned operator-
    /// relevant substrings independently.
    /// Why: the warning must tell operators both that fallback cleanup is
    /// UUID-scanned and that unverified candidates are skipped.
    /// Scenario: seed 703.
    #[test]
    fn uuid_scanned_fallback_warn_body_contains_pinned_substrings() {
        // Synthesize a ProbeError::Cmd to feed into the warn body.
        let pe = ProbeError::PoolDevice {
            mapper: "/mnt/storage".into(),
            detail: "synthetic".into(),
        };
        let body = super::uuid_scanned_fallback_warn_body(&pe);
        assert!(
            body.contains("falling back to UUID-scanned mapper cleanup."),
            "missing first pinned substring; body was: {body}"
        );
        assert!(
            body.contains("unverified candidates are skipped."),
            "missing second pinned substring; body was: {body}"
        );
    }

    /// Intent: a malformed `braid-<not-a-valid-name>` mapper with a
    /// readable non-member UUID is still closable as an orphan. The
    /// orphan disk_name carries the raw suffix for the warning body.
    /// Why: `braid-*` is the cleanup namespace even when the suffix is
    /// not a valid DiskName, but identity still comes from LUKS UUID.
    /// Scenario: seed 704.
    #[test]
    fn uuid_scanned_fallback_malformed_mapper_with_uuid_is_orphan() {
        // A mapper named "braid-..foo" -- DiskName::parse rejects it.
        let fs = lock_fs(&["/dev/mapper/braid-..foo"]).with_luks_dm("braid-..foo", ORPHAN_UUID);
        let membership = lock_test_membership();
        let mut acc = CloseSetAccumulator::default();
        let close_set = build_close_sets_uuid_scanned_fallback(&fs, &membership, &mut acc);
        let names: Vec<String> = orphan_summaries(&close_set)
            .into_iter()
            .map(|(_mapper, disk_name)| disk_name)
            .collect();
        assert!(
            names.contains(&"..foo".to_owned()),
            "malformed mapper basename must be carried as orphan disk_name, got: {names:?}",
        );
        assert!(
            !acc.cleanup_uncertain,
            "readable UUID should not be skipped"
        );
    }

    // Intent: the UUID-scanned fallback closes a member's mapper whose backing
    //   disk is gone, as that member, from the LUKS UUID the mapper's dm UUID
    //   records.
    // Why it exists: lock proved ownership by rereading the LUKS header through
    //   the backing device, so a dead member's mapper was warned about, skipped,
    //   and left open with its volume key in kernel memory (finding
    //   261021-1103-013).
    // Scenario: the pool is unmounted and disk `bbb` was hot-unplugged: its
    //   mapper stays open with no backing device (`cryptsetup status` reports
    //   `device: (null)`), while sysfs still carries the dm UUID cryptsetup
    //   recorded at activation.
    #[test]
    fn uuid_scanned_fallback_closes_null_backed_member_by_dm_uuid() {
        let fs = lock_fs(&["/dev/mapper/braid-bbb"]).with_luks_dm("braid-bbb", BBB_UUID);
        let membership = lock_test_membership();
        let mut acc = CloseSetAccumulator::default();
        let close_set = build_close_sets_uuid_scanned_fallback(&fs, &membership, &mut acc);

        assert_eq!(close_set.entries(), [member_close("braid-bbb", "bbb")]);
        assert!(
            !acc.cleanup_uncertain,
            "a mapper proven by its dm UUID is complete cleanup, got notes: {:?}",
            acc.notes,
        );
    }

    // Intent: a scanned braid-* candidate with no entry in the device-mapper
    //   table is skipped -- never closed as orphan-by-name.
    // Why it exists: without a dm UUID nothing proves member or orphan
    //   identity; closing by the `braid-*` name would be fail-open.
    // Scenario: seed 708 -- pool unmounted, /dev/mapper/braid-gone is listed
    //   but no /sys/block/dm-* device carries that name (the mapper was torn
    //   down after the scan, or the node is a stray file).
    #[test]
    fn uuid_scanned_fallback_candidate_missing_from_dm_table_is_skipped() {
        let fs = lock_fs(&["/dev/mapper/braid-gone"]);
        let membership = lock_test_membership();
        let mut acc = CloseSetAccumulator::default();
        let close_set = build_close_sets_uuid_scanned_fallback(&fs, &membership, &mut acc);

        assert!(close_set.is_empty());
        assert!(acc.cleanup_uncertain);
        assert_eq!(
            warn_bodies(&acc.notes),
            vec![
                "skipping mapper braid-gone: cannot verify backing LUKS UUID \
                 (no device-mapper device by that name)"
            ]
        );
    }

    // Intent: a scanned braid-* candidate whose dm UUID is not a cryptsetup
    //   LUKS one is skipped -- never closed as orphan-by-name.
    // Why it exists: a plain dm-crypt or hand-made mapping under the braid-*
    //   namespace proves no LUKS identity, so lock must leave it alone.
    // Scenario: pool unmounted; braid-plain is a plain dm-crypt mapping.
    #[test]
    fn uuid_scanned_fallback_non_luks_candidate_is_skipped() {
        let fs = lock_fs(&["/dev/mapper/braid-plain"])
            .with_raw_dm("braid-plain", "CRYPT-PLAIN-braid-plain");
        let membership = lock_test_membership();
        let mut acc = CloseSetAccumulator::default();
        let close_set = build_close_sets_uuid_scanned_fallback(&fs, &membership, &mut acc);

        assert!(close_set.is_empty());
        assert!(acc.cleanup_uncertain);
        assert_eq!(
            warn_bodies(&acc.notes),
            vec![
                "skipping mapper braid-plain: cannot verify backing LUKS UUID \
                 (device-mapper UUID \"CRYPT-PLAIN-braid-plain\" is not a cryptsetup LUKS UUID)"
            ]
        );
    }

    /// Intent: stranded-mapper classification skips a mapper with no
    /// provable LUKS UUID instead of demoting it to orphan by name.
    /// Why: a missing identity cannot prove either member ownership or
    /// orphan status; closing by the `braid-*` name would be unsafe.
    /// Scenario: seed 705 -- a stranded mapper with no entry in the
    /// device-mapper table.
    #[test]
    fn full_arm_stranded_mapper_classify_failure_skips_candidate() {
        let fs = lock_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-stranded",
        ]);
        let membership = lock_test_membership();
        // braid-stranded is NOT in pool.devices, so it gets classified by
        // its dm UUID. The dm table has no entry for it, so the helper
        // skips it with the pinned warning instead of closing by name.
        let pool = synthetic_pool_state("braid-aaa", "braid-bbb");
        let mut acc = CloseSetAccumulator::default();
        let close_set = build_close_sets_full(&fs, &pool, &membership, &mut acc);
        // Member-owned still has the two pool.devices entries.
        assert_eq!(member_summaries(&close_set).len(), 2);
        assert!(orphan_summaries(&close_set).is_empty());
        assert!(acc.cleanup_uncertain);
        assert!(
            acc.notes.iter().any(|note| matches!(
                note,
                PreviewNote::Warn(body)
                    if body.contains("skipping mapper braid-stranded")
            )),
            "skip warning expected, got: {:?}",
            acc.notes
        );
    }

    // Intent: a stranded-mapper classify failure suppresses all known-closed
    //   claims for unaccounted members.
    // Why it exists: an unverified stranded mapper could be a drifted
    //   member, so an execute-side "already closed" line would contradict
    //   the planner's uncertainty.
    // Scenario: `aaa` and `bbb` are mounted, `ccc` is in pool.json, and an
    //   unverified `/dev/mapper/braid-stranded` cannot be classified.
    #[test]
    fn full_arm_stranded_classify_failure_suppresses_known_closed_members() {
        let fs = lock_member_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-stranded",
        ]);
        let runner = mounted_runner_with_btrfs_show("braid-aaa", "braid-bbb");
        let config = lock_test_config();
        let membership = lock_test_membership_with_ccc();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::User)
            .expect("plan should succeed");

        assert!(
            plan.notes.iter().any(|note| matches!(
                note,
                PreviewNote::Warn(body)
                    if body.contains("skipping mapper braid-stranded")
            )),
            "skip warning for braid-stranded expected, got: {:?}",
            plan.notes
        );
        assert!(plan.cleanup_uncertain);
        assert!(
            plan.members_known_closed.is_empty(),
            "unclassified stranded mapper must suppress known-closed rows, got: {:?}",
            known_closed_names(&plan)
        );
    }

    // Intent: full-arm `/dev/mapper` scan failure suppresses known-closed
    //   claims for unaccounted members and marks cleanup uncertain.
    // Why it exists: without a mapper listing, a live drifted mapper for an
    //   unaccounted member cannot be ruled out.
    // Scenario: `aaa` and `bbb` are mounted, `ccc` is in pool.json, and
    //   `/dev/mapper` cannot be listed.
    #[test]
    fn full_arm_scan_failure_suppresses_known_closed_members() {
        let fs = lock_member_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_dev_mapper_error();
        let runner = mounted_runner_with_btrfs_show("braid-aaa", "braid-bbb");
        let config = lock_test_config();
        let membership = lock_test_membership_with_ccc();

        let plan = plan_lock(&runner, &fs, &config, &membership, LockMode::User)
            .expect("plan should succeed");

        assert!(plan.cleanup_uncertain);
        assert!(
            plan.members_known_closed.is_empty(),
            "unscannable mapper namespace must suppress known-closed rows, got: {:?}",
            known_closed_names(&plan)
        );
    }

    /// Intent: a stranded mapper whose dm UUID records a membership LUKS
    /// UUID is classified as member-owned.
    /// Why: the dm UUID is the mapper's own record of its LUKS container,
    /// so it decides identity without reading the backing disk or the
    /// decrypted `/dev/mapper/<name>` payload.
    /// Scenario: seed 705b -- pool.devices reports the normal members,
    /// and a third stranded mapper's dm UUID records membership's `aaa`
    /// UUID.
    #[test]
    fn full_arm_stranded_mapper_classifies_member_by_dm_uuid() {
        let fs = lock_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-stranded",
        ])
        .with_luks_dm("braid-stranded", AAA_UUID);
        let membership = lock_test_membership();
        let pool = synthetic_pool_state("braid-aaa", "braid-bbb");

        let mut acc = CloseSetAccumulator::default();
        let close_set = build_close_sets_full(&fs, &pool, &membership, &mut acc);

        let member_owned = member_summaries(&close_set);
        assert!(
            member_owned.contains(&("braid-stranded".to_owned(), "aaa".to_owned())),
            "stranded mapper should be member-owned via its dm UUID, got: {member_owned:?}",
        );
        assert!(
            orphan_summaries(&close_set)
                .iter()
                .all(|(mapper, _disk_name)| mapper != "braid-stranded"),
            "stranded member must not be demoted to orphan: {:?}",
            orphan_summaries(&close_set),
        );
    }

    /// Intent: probe_pool's NotBtrfs error variant is NOT routed
    /// through fallback cleanup -- it aborts the lock with the
    /// preserved mounted-non-btrfs message.
    /// Why: plan 3320 -- only per-device variants are catchable by
    /// fallback cleanup so a real configuration error
    /// (NotBtrfs) cannot be silently masked.
    /// Scenario: seed 706. NB: an existing test
    /// `lock_rejects_mounted_but_not_btrfs` proves this same
    /// behavior end-to-end; this test exists separately to pin the
    /// match-arm shape so a future refactor cannot collapse NotBtrfs
    /// into the catch-all without surfacing in CI.
    #[test]
    fn full_arm_notbtrfs_aborts_does_not_fall_back() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint::new("/mnt/storage".to_owned()).into(),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_mountinfo("36 35 0:32 / /mnt/storage rw,noatime shared:1 - ext4 /dev/sda1 rw\n");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let result = plan_lock(&runner, &fs, &config, &membership, LockMode::User);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("NotBtrfs must surface as an abort, not fallback cleanup"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("not btrfs") && msg.contains("ext4"),
            "expected NotBtrfs-style message naming ext4, got: {msg}"
        );
    }
}
