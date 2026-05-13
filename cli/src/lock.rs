use crate::cmd::{CmdError, CmdRequest, CommandRunner, Step};
use crate::config::{Config, mapper_name, name_from_mapper};
use crate::mapper_close::{CloseMapperError, close_mapper_with_retry};
use crate::membership::PoolMembership;
use crate::parse::{parse_cryptsetup_luks_uuid, parse_cryptsetup_status};
use crate::preflight;
use crate::preview::{Preview, PreviewCompleteness, PreviewNote};
use crate::probe::{Filesystem, ProbeError, probe_fsid, probe_pool};
use crate::progress::{RealSleeper, Sleeper};
use crate::status_tag::{StatusTag, color_enabled_for_stderr, status_line};
use crate::types::{DiskName, MapperName, MountPoint, PoolState};

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

/// Snapshot of the pool's live state at lock-planning time. `Full`
/// carries the UUID-classified `PoolState` from `probe_pool`;
/// `FsidOnly` is the per-device-probe-failed fallback that keeps
/// `require_lock_preflight` running while drift detection is
/// disabled. The two arms produce identical close orders on identical
/// inputs by construction (see plan 3215-3226).
#[allow(dead_code)] // fsid/probe_error are reserved for the FsidOnly arm warning.
pub enum LockSnapshot {
    Full(PoolState),
    FsidOnly {
        fsid: String,
        probe_error: ProbeError,
    },
}

/// A member-owned mapper to close at lock execution. `mapper` is the
/// observed `MapperName` from `PoolDevice.mapper` (or the
/// classification helper for stranded mappers); `display_name` is the
/// canonical disk name for status output. Classified once at plan
/// time so `execute` never has to redo the identity decision.
// TODO(post-migration): consider unifying MemberOwnedClose and
// OrphanMapper into LockMapperClose { kind } once this migration
// lands -- see plans/impl/2026-05-12-luks-uuid-as-identity/plan.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberOwnedClose {
    pub mapper: MapperName,
    pub display_name: DiskName,
}

/// Combined close set produced by lock planning. Two parallel
/// vectors so the close path can iterate member-owned then orphan in
/// the order today's `close_set_paths` already uses.
// TODO(post-migration): consider unifying MemberOwnedClose and
// OrphanMapper into LockMapperClose { kind } once this migration
// lands -- see plans/impl/2026-05-12-luks-uuid-as-identity/plan.md.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockCloseSets {
    pub member_owned: Vec<MemberOwnedClose>,
    pub orphan_mappers: Vec<OrphanMapper>,
}

/// Internal scanned-orphan representation. Constructed by
/// `scan_orphan_mappers_by_name` (FsidOnly fallback) or by the
/// stranded-mapper classification helper (Full path): a
/// `/dev/mapper/braid-*` entry observed at plan time that is not part
/// of pool membership. The `mapper` field is typed `MapperName`
/// post-migration so every observed-mapper field shares one type;
/// `disk_name` stays `String` to carry the raw basename text from
/// `name_from_mapper` -- including text `DiskName::parse` rejects
/// (malformed-mapper fall-through to orphan classification).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrphanMapper {
    pub mapper: MapperName,
    pub disk_name: String,
}

/// Classification result for a stranded `/dev/mapper/braid-*` slot
/// that did not appear in the planning-time `pool.devices` scan.
/// `MemberOwned` carries the canonical disk name so the close set's
/// status line can render `disk <name>: locking...`; `Orphan` rolls
/// straight into the orphan-warn path.
enum StrandedClass {
    MemberOwned { display_name: DiskName },
    Orphan,
}

/// Issue exactly two `CmdRequest` calls per stranded mapper -- a
/// `CryptsetupStatus` to confirm the mapper is a cryptsetup-managed
/// dm slot, then a `CryptsetupLuksUuid` against the dm path to read
/// the live LUKS UUID. The parsed UUID is matched against membership
/// keys to decide MemberOwned vs Orphan. Failure on either call ends
/// the helper with `Err(CmdError::...)`; the caller's per-mapper
/// degrade path turns it into a logged-warning Orphan and continues
/// scanning so one cryptsetup hiccup cannot tank the whole lock.
fn classify_stranded_mapper<R: CommandRunner>(
    runner: &R,
    mapper: &MapperName,
    membership: &PoolMembership,
) -> Result<StrandedClass, CmdError> {
    let status_raw = runner.run(&CmdRequest::CryptsetupStatus {
        mapper: mapper.0.clone(),
    })?;
    parse_cryptsetup_status(&status_raw)
        .map_err(|e| CmdError::Failed(format!("cryptsetup status {}: {e}", mapper.0)))?;
    let uuid_raw = runner.run(&CmdRequest::CryptsetupLuksUuid {
        device: format!("/dev/mapper/{}", mapper.0),
    })?;
    let parsed = parse_cryptsetup_luks_uuid(&uuid_raw)
        .map_err(|e| CmdError::Failed(format!("cryptsetup luksUUID {}: {e}", mapper.0)))?;
    match membership.by_uuid(&parsed.uuid) {
        Some(member) => Ok(StrandedClass::MemberOwned {
            display_name: member.name.clone(),
        }),
        None => Ok(StrandedClass::Orphan),
    }
}

/// Enumerate `/dev/mapper/braid-*` entries that are NOT in pool
/// membership by name (the FsidOnly-arm fallback). Drift-blind by
/// design -- per-device drift detection requires the per-device
/// probe data that only `LockSnapshot::Full` provides. Renamed from
/// `scan_orphan_mappers` to flag the legacy classifier explicitly:
/// in the Full arm the close-set builder routes through
/// `classify_stranded_mapper` instead.
fn scan_orphan_mappers_by_name<F: Filesystem + ?Sized>(
    fs: &F,
    membership: &PoolMembership,
) -> Result<Vec<OrphanMapper>, std::io::Error> {
    let entries = fs.list_dir("/dev/mapper")?;
    let mut orphans = Vec::new();
    for entry in entries {
        let Some(disk_name_raw) = name_from_mapper(&entry) else {
            continue;
        };
        // Malformed `braid-<not-a-valid-disk-name>` falls through to
        // orphan classification rather than skipping silently. The
        // OrphanMapper.disk_name carries the raw basename for the
        // warning body, which by construction admits text
        // DiskName::parse rejects.
        let parsed = DiskName::parse(disk_name_raw);
        if let Ok(name) = &parsed
            && membership.by_name(name).is_some()
        {
            continue;
        }
        if !fs.exists(&format!("/dev/mapper/{entry}")) {
            continue;
        }
        orphans.push(OrphanMapper {
            mapper: MapperName(entry.clone()),
            disk_name: disk_name_raw.to_owned(),
        });
    }
    Ok(orphans)
}

/// Message body (no `[warn]` prefix) for a failed /dev/mapper scan.
/// Shared between the dry-run preview and the real-run stderr warn so
/// both branches use identical wording.
fn orphan_scan_warn_body(e: &std::io::Error) -> String {
    format!("could not scan /dev/mapper for orphans: {e} (skipping)")
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

/// Message body (no `[warn]` prefix) for the FsidOnly-arm warning.
/// Pinned by plan 3228-3239. Two operator-relevant substrings are
/// pinned independently by the lock test suite:
/// `Mapper drift detection is disabled for this run.` AND
/// `an unrelated disk opened under that name will be torn down.`
fn fsid_only_warn_body(probe_error: &ProbeError) -> String {
    format!(
        "warning: per-device probe failed ({probe_error}); falling back to FSID-only lock preflight. \
         Mapper drift detection is disabled for this run. \
         In this mode, mappers under the names braid-<member-name> are closed without verifying their LUKS UUID; \
         an unrelated disk opened under that name will be torn down."
    )
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

/// Compose the lock close set as fully qualified `/dev/mapper/...`
/// paths: every member-owned mapper observed open at plan time,
/// followed by orphaned braid-* mappers, in that order. Caller is
/// responsible for any TOCTOU re-filter -- this helper does not
/// touch the filesystem. Both slices render through
/// `MapperName::Display` so the wire format stays byte-identical to
/// the pre-migration `String` path.
fn close_set_paths(
    member_owned: &[MemberOwnedClose],
    orphan_mappers: &[OrphanMapper],
) -> Vec<String> {
    member_owned
        .iter()
        .map(|m| format!("/dev/mapper/{}", m.mapper))
        .chain(
            orphan_mappers
                .iter()
                .map(|o| format!("/dev/mapper/{}", o.mapper)),
        )
        .collect()
}

/// Shared close-and-aggregate state for the membership and orphan
/// loops in `LockPlan::execute`. Bundles the loop-invariant inputs
/// (runner, sleeper, color, umount-busy suppression flag) with the
/// `&mut first_mapper_error` accumulator so status formatting and
/// error-aggregation cannot drift between the two callers.
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
    fn close_one(&mut self, mapper: &str, disk_label: &str, is_orphan: bool) {
        let color_enabled = self.color_enabled;
        let line = |t, body: &str| status_line(t, color_enabled, body);
        let paren = if is_orphan { " (orphan)" } else { "" };

        eprint!(
            "{}",
            line(
                StatusTag::Wait,
                &format!("disk {disk_label}: locking{paren}..."),
            )
        );
        match close_mapper_with_retry(self.runner, self.sleeper, mapper, color_enabled) {
            Ok(()) => {
                eprint!(
                    "{}",
                    line(StatusTag::Ok, &format!("disk {disk_label}: locked{paren}"))
                );
            }
            Err(CloseMapperError::DeviceBusy(msg)) if self.umount_error.is_some() => {
                let phrase = if is_orphan {
                    "orphan close failed"
                } else {
                    "close failed"
                };
                eprint!(
                    "{}",
                    line(
                        StatusTag::Warn,
                        &format!("disk {disk_label}: {phrase} (umount was stuck): {msg}")
                    )
                );
            }
            Err(e) => {
                let err = LockError::from(e);
                let prefix = if is_orphan { "orphan: " } else { "" };
                eprint!(
                    "{}",
                    line(
                        StatusTag::Fail,
                        &format!("disk {disk_label}: {prefix}{err}")
                    )
                );
                if self.first_mapper_error.is_none() {
                    *self.first_mapper_error = Some(err);
                }
            }
        }
    }
}

/// Compile dry-run steps for lock. The close-set's two slices are
/// driven by the same observed mapper strings as `execute` (see
/// `LockPlan::execute`); this keeps the forget set, dry-run steps, and
/// real-run close calls byte-identical on identical inputs.
fn compile_lock_steps(
    pool_was_mounted: bool,
    member_owned: &[MemberOwnedClose],
    orphan_mappers: &[OrphanMapper],
    mount_point: &MountPoint,
) -> Vec<Step> {
    let mut steps = Vec::new();

    if pool_was_mounted {
        steps.push(Step {
            risk: "safe",
            description: format!("unmount {}", mount_point),
            commands: vec![CmdRequest::Umount {
                mount_point: mount_point.clone(),
            }],
        });
        let forget_devs = close_set_paths(member_owned, orphan_mappers);
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

    for entry in member_owned {
        steps.push(Step {
            risk: "safe",
            description: format!("close LUKS mapper {}", entry.mapper),
            commands: vec![CmdRequest::CryptsetupClose {
                // TODO(post-migration): retype mapper: String to
                // MapperName once this migration lands.
                mapper: entry.mapper.0.clone(),
            }],
        });
    }

    for orphan in orphan_mappers {
        steps.push(Step {
            risk: "safe",
            description: format!("close LUKS mapper {} (orphan)", orphan.mapper),
            commands: vec![CmdRequest::CryptsetupClose {
                // TODO(post-migration): retype mapper: String to
                // MapperName once this migration lands.
                mapper: orphan.mapper.0.clone(),
            }],
        });
    }

    steps
}

/// The dry-run preview source of truth for `braid lock` and the sets
/// pre-computed during planning. The membership close decision and
/// forget set are driven by `member_owned`; `fs.exists` during execute
/// is only a disappearance guard before mutating an already-planned
/// mapper.
pub struct LockPlan {
    pub notes: Vec<PreviewNote>,
    pub steps: Vec<Step>,
    pub pool_was_mounted: bool,
    /// Member-owned mappers observed open during planning (the
    /// successor to `open_mappers`). For `LockSnapshot::Full` each
    /// entry's `mapper` is cloned from the live `PoolDevice.mapper`
    /// (or the stranded-mapper classifier); for `LockSnapshot::FsidOnly`
    /// it is reconstructed via `mapper_name(member.name)` because no
    /// per-device live data is available.
    pub member_owned: Vec<MemberOwnedClose>,
    orphan_mappers: Vec<OrphanMapper>,
    pub mount_point: MountPoint,
}

impl LockPlan {
    pub fn preview(&self) -> Preview {
        Preview {
            completeness: PreviewCompleteness::Complete,
            notes: self.notes.clone(),
            steps: self.steps.clone(),
        }
    }

    pub(crate) fn execute<R, F, S>(
        self,
        runner: &R,
        fs: &F,
        sleeper: &S,
        membership: &PoolMembership,
    ) -> Result<(), LockError>
    where
        R: CommandRunner,
        F: Filesystem + ?Sized,
        S: Sleeper,
    {
        let color_enabled = color_enabled_for_stderr();
        let line = |t, body: &str| status_line(t, color_enabled, body);

        // Emit accumulated Warn notes to stderr before any mutation.
        // The plan carries the orphan-scan-failure warn and one warn
        // per detected orphan mapper as PreviewNote::Warn; this loop
        // is the single emit point for both.
        for note in &self.notes {
            if let PreviewNote::Warn(body) = note {
                eprint!("{}", line(StatusTag::Warn, body));
            }
        }

        let mount_point = &self.mount_point;
        let orphan_mappers = &self.orphan_mappers;

        // 2. If mounted → unmount
        let mut umount_error: Option<LockError> = None;
        let mut first_mapper_error: Option<LockError> = None;
        if self.pool_was_mounted {
            eprint!(
                "{}",
                line(
                    StatusTag::Wait,
                    &format!("pool: unmounting {mount_point}..."),
                )
            );
            let umount_result = runner.run(&CmdRequest::Umount {
                mount_point: mount_point.clone(),
            })?;
            if umount_result.exit_status != 0 {
                let stderr = umount_result.stderr.trim();
                let mut msg = format!(
                    "umount {mount_point} failed (exit {}): {stderr}",
                    umount_result.exit_status,
                );
                if umount_stderr_is_busy(stderr) {
                    msg.push_str(&format!(
                        "\nhint: a process may be using files on the mount. \
                         Run 'lsof {mount_point}' or 'fuser -vm {mount_point}' to identify it."
                    ));
                }
                let err = LockError::Failed(msg);
                eprint!("{}", line(StatusTag::Fail, &format!("{err}")));
                eprint!(
                    "{}",
                    line(
                        StatusTag::Warn,
                        "attempting to close LUKS mappers despite umount failure..."
                    )
                );
                umount_error = Some(err);
            } else {
                eprint!(
                    "{}",
                    line(StatusTag::Ok, &format!("pool: unmounted {mount_point}"))
                );

                // Clear btrfs kernel scan registry so that cryptsetup close
                // doesn't race against stale device references on multi-device
                // pools. Scope to the close set (membership + orphan mappers)
                // -- the no-arg form is kernel-global and would invalidate
                // scan entries for unrelated btrfs filesystems on the host.
                let mut forget_devs = close_set_paths(&self.member_owned, orphan_mappers);
                forget_devs.retain(|p| fs.exists(p));
                if !forget_devs.is_empty() {
                    let forget_result = runner.run(&CmdRequest::BtrfsDeviceScanForget {
                        devices: forget_devs,
                    });
                    match forget_result {
                        Ok(r) if r.exit_status == 0 => {}
                        Ok(r) => {
                            eprint!(
                                "{}",
                                line(
                                    StatusTag::Warn,
                                    &format!(
                                        "btrfs device scan --forget failed (exit {}): {} (continuing)",
                                        r.exit_status,
                                        r.stderr.trim()
                                    )
                                )
                            );
                        }
                        Err(e) => {
                            eprint!(
                                "{}",
                                line(
                                    StatusTag::Warn,
                                    &format!("btrfs device scan --forget failed: {e} (continuing)")
                                )
                            );
                        }
                    }
                }
            }
        }

        // 3. Close each member-owned mapper. For every membership
        // entry NOT in member_owned (`planned`-set), surface
        // "already closed". The planned set comes from observed
        // mappers in the Full arm, and from reconstructed names in
        // the FsidOnly arm; either way the close path treats
        // anything outside that set as already-closed.
        let planned_mappers: std::collections::HashSet<&str> = self
            .member_owned
            .iter()
            .map(|e| e.mapper.0.as_str())
            .collect();
        let mut all_already_closed = true;
        {
            let mut close_ctx = CloseMapperCtx {
                runner,
                sleeper,
                color_enabled,
                umount_error: &umount_error,
                first_mapper_error: &mut first_mapper_error,
            };
            // Membership-side "already closed" prelude: for every
            // member whose expected mapper is not in the planned
            // close set, emit the already-closed status line. The
            // expected mapper is reconstructed from name purely for
            // display here; identity decisions for the close itself
            // were made at plan time.
            for (_uuid, member) in membership.iter() {
                let mn = mapper_name(member.name.as_str());
                if !planned_mappers.contains(mn.0.as_str()) {
                    eprint!(
                        "{}",
                        line(
                            StatusTag::Ok,
                            &format!("disk {}: already closed", member.name)
                        )
                    );
                }
            }
            // Member-owned closes, observed-mapper-first
            // (matches today's close_set_paths ordering).
            for entry in &self.member_owned {
                let mapper_path = format!("/dev/mapper/{}", entry.mapper);
                if !fs.exists(&mapper_path) {
                    eprint!(
                        "{}",
                        line(
                            StatusTag::Ok,
                            &format!("disk {}: already closed", entry.display_name)
                        )
                    );
                    continue;
                }
                // Accepted risk: in-process member-owned close
                // double-drift -- see plan section "Accepted risk:
                // in-process member-owned close double-drift" for
                // rationale.
                close_ctx.close_one(&entry.mapper.0, entry.display_name.as_str(), false);
                all_already_closed = false;
            }
        }

        // 3b. Close orphaned braid-* mappers (precomputed during
        // planning so the forget call shared the same close-set). An
        // orphan is detected iff fs.exists was true at plan time;
        // re-check to cover the narrow window where it disappeared on
        // its own.
        {
            let mut close_ctx = CloseMapperCtx {
                runner,
                sleeper,
                color_enabled,
                umount_error: &umount_error,
                first_mapper_error: &mut first_mapper_error,
            };
            for orphan in orphan_mappers {
                if !fs.exists(&format!("/dev/mapper/{}", orphan.mapper)) {
                    continue;
                }
                close_ctx.close_one(&orphan.mapper.0, &orphan.disk_name, true);
                all_already_closed = false;
            }
        }

        // 4. Return first fatal mapper error if any, otherwise deferred umount error
        if let Some(e) = first_mapper_error {
            return Err(e);
        }
        if let Some(e) = umount_error {
            return Err(e);
        }

        // 5. If nothing was done → short message
        if !self.pool_was_mounted && all_already_closed {
            eprintln!("pool already locked");
        }

        Ok(())
    }
}

/// Plan a `braid lock` run. Owns the mountpoint probe, preflight,
/// per-device probe (for UUID classification), close-set assembly,
/// step compilation, and any `PreviewNote::Warn` notes: one per
/// detected orphan mapper, the FsidOnly fallback warning when
/// per-device probe failed, or a single warn from a failed orphan
/// scan. The returned `LockPlan` is the single source of truth for
/// both `--dry-run` preview and real execution.
pub fn plan_lock<R, F>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
) -> Result<LockPlan, LockError>
where
    R: CommandRunner,
    F: Filesystem + ?Sized,
{
    let mount_point = config.mount_point().clone();

    // 1. Check if pool is mounted
    let mp_result = runner.run(&CmdRequest::MountpointCheck {
        path: mount_point.clone(),
    })?;
    let pool_was_mounted = mp_result.exit_status == 0;

    // 2. Try per-device probe; on success take the Full path so
    // close-set classification routes through observed UUIDs.
    // Per-device failures fall back to FSID-only preflight; NotBtrfs
    // aborts to preserve today's mounted-non-btrfs refusal.
    let snapshot = if pool_was_mounted {
        match probe_pool(runner, fs, &mount_point) {
            Ok(pool) => LockSnapshot::Full(pool),
            // Explicit per-variant routing. NotBtrfs aborts; every
            // other variant falls back to probe_fsid + FsidOnly.
            // No catch-all -- if a future ProbeError variant lands,
            // it must opt in explicitly here so a real configuration
            // error cannot be silently masked by the FsidOnly path.
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
                | ProbeError::MapperConflict { .. }
                | ProbeError::MountInfo(_)),
            ) => {
                let fsid = probe_fsid(runner, fs, &mount_point)
                    .map_err(|e| LockError::Failed(format!("cannot probe pool: {e}")))?;
                LockSnapshot::FsidOnly { fsid, probe_error }
            }
        }
    } else {
        // Pool unmounted: skip per-device probing entirely and use
        // the FsidOnly-shape branch -- preflight does not run when
        // the pool is unmounted (preflight is the mounted-pool gate
        // for exclusive operations).
        LockSnapshot::FsidOnly {
            fsid: String::new(),
            probe_error: ProbeError::PoolDevice {
                mapper: mount_point.0.clone(),
                detail: "unmounted (skip preflight)".into(),
            },
        }
    };

    let mut notes: Vec<PreviewNote> = Vec::new();
    let close_sets = match &snapshot {
        LockSnapshot::Full(pool) => {
            if pool_was_mounted && let Some(fsid) = &pool.fsid {
                preflight::require_lock_preflight(fs, fsid).map_err(LockError::Failed)?;
            }
            build_close_sets_full(runner, fs, pool, membership, &mut notes)
        }
        LockSnapshot::FsidOnly { fsid, probe_error } => {
            if pool_was_mounted {
                notes.push(PreviewNote::Warn(fsid_only_warn_body(probe_error)));
                preflight::require_lock_preflight(fs, fsid).map_err(LockError::Failed)?;
            }
            build_close_sets_fsid_only(fs, membership, &mut notes)
        }
    };

    let steps = compile_lock_steps(
        pool_was_mounted,
        &close_sets.member_owned,
        &close_sets.orphan_mappers,
        &mount_point,
    );

    Ok(LockPlan {
        notes,
        steps,
        pool_was_mounted,
        member_owned: close_sets.member_owned,
        orphan_mappers: close_sets.orphan_mappers,
        mount_point,
    })
}

/// Close-set construction for the `LockSnapshot::Full` arm. Drives the
/// member-owned classification through observed `PoolDevice.mapper`
/// strings so the close + forget + dry-run preview all share one
/// observed-mapper source-of-truth. Stranded `/dev/mapper/braid-*`
/// slots that did not appear in pool.devices are re-classified via
/// `classify_stranded_mapper`; per-mapper failures degrade to logged
/// Orphan rather than failing the whole lock.
fn build_close_sets_full<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    pool: &PoolState,
    membership: &PoolMembership,
    notes: &mut Vec<PreviewNote>,
) -> LockCloseSets {
    let mut member_owned: Vec<MemberOwnedClose> = Vec::new();
    let mut orphan_mappers: Vec<OrphanMapper> = Vec::new();

    // Pass 1: pool.devices, classified by observed LUKS UUID.
    for dev in &pool.devices {
        if let Some(member) = membership.by_uuid(&dev.luks_uuid) {
            member_owned.push(MemberOwnedClose {
                mapper: dev.mapper.clone(),
                display_name: member.name.clone(),
            });
        } else {
            // Live mapper carries a UUID not in membership -- treat
            // as orphan (the close-observed-not-reconstructed
            // doctrine). disk_name carries the basename for the
            // warning body.
            let disk_name = name_from_mapper(&dev.mapper.0)
                .unwrap_or(dev.mapper.0.as_str())
                .to_owned();
            orphan_mappers.push(OrphanMapper {
                mapper: dev.mapper.clone(),
                disk_name,
            });
        }
    }

    // Pass 2: pool.null_underlying, classified by persisted devid.
    for nu in &pool.null_underlying {
        match membership.by_devid(nu.devid) {
            Ok(Some((_uuid, member))) => member_owned.push(MemberOwnedClose {
                mapper: nu.mapper.clone(),
                display_name: member.name.clone(),
            }),
            _ => {
                let disk_name = name_from_mapper(&nu.mapper.0)
                    .unwrap_or(nu.mapper.0.as_str())
                    .to_owned();
                orphan_mappers.push(OrphanMapper {
                    mapper: nu.mapper.clone(),
                    disk_name,
                });
            }
        }
    }

    // Pass 3: stranded `braid-*` slots in /dev/mapper that did NOT
    // appear in pool.devices or pool.null_underlying. Each one is
    // probed via classify_stranded_mapper to decide MemberOwned vs
    // Orphan. Per-mapper failures degrade to a logged warning + Orphan.
    let already_observed: std::collections::HashSet<&str> = member_owned
        .iter()
        .map(|m| m.mapper.0.as_str())
        .chain(orphan_mappers.iter().map(|o| o.mapper.0.as_str()))
        .collect();

    let dev_mapper_entries = match fs.list_dir("/dev/mapper") {
        Ok(entries) => entries,
        Err(e) => {
            notes.push(PreviewNote::Warn(orphan_scan_warn_body(&e)));
            // Preserve best-effort semantics: with no /dev/mapper
            // listing, return what we have. Pass-1/2 member_owned
            // is still valid.
            return LockCloseSets {
                member_owned,
                orphan_mappers,
            };
        }
    };

    let mut stranded: Vec<MapperName> = Vec::new();
    for entry in dev_mapper_entries {
        if name_from_mapper(&entry).is_none() {
            continue;
        }
        if already_observed.contains(entry.as_str()) {
            continue;
        }
        if !fs.exists(&format!("/dev/mapper/{entry}")) {
            continue;
        }
        stranded.push(MapperName(entry));
    }

    for mapper in stranded {
        match classify_stranded_mapper(runner, &mapper, membership) {
            Ok(StrandedClass::MemberOwned { display_name }) => {
                member_owned.push(MemberOwnedClose {
                    mapper,
                    display_name,
                });
            }
            Ok(StrandedClass::Orphan) => {
                let disk_name = name_from_mapper(&mapper.0)
                    .unwrap_or(mapper.0.as_str())
                    .to_owned();
                notes.push(PreviewNote::Warn(orphan_mapper_warn_body(&mapper)));
                orphan_mappers.push(OrphanMapper { mapper, disk_name });
            }
            Err(cmd_err) => {
                eprintln!(
                    "Warning: failed to classify stranded mapper {mapper}: {cmd_err}; treating as orphan",
                );
                let disk_name = name_from_mapper(&mapper.0)
                    .unwrap_or(mapper.0.as_str())
                    .to_owned();
                notes.push(PreviewNote::Warn(orphan_mapper_warn_body(&mapper)));
                orphan_mappers.push(OrphanMapper { mapper, disk_name });
            }
        }
    }

    LockCloseSets {
        member_owned,
        orphan_mappers,
    }
}

/// Close-set construction for the `LockSnapshot::FsidOnly` arm.
/// Drift-blind by design -- per-device drift detection requires
/// per-device probe data that only `LockSnapshot::Full` provides.
/// Mirrors today's `scan_orphan_mappers` flow: reconstruct
/// member-owned mappers from membership names, scan /dev/mapper for
/// stranded slots, classify by name alone.
fn build_close_sets_fsid_only<F: Filesystem + ?Sized>(
    fs: &F,
    membership: &PoolMembership,
    notes: &mut Vec<PreviewNote>,
) -> LockCloseSets {
    let mut member_owned: Vec<MemberOwnedClose> = Vec::new();
    for (_uuid, member) in membership.iter() {
        let mn = mapper_name(member.name.as_str());
        if fs.exists(&format!("/dev/mapper/{}", mn.0)) {
            member_owned.push(MemberOwnedClose {
                mapper: mn,
                display_name: member.name.clone(),
            });
        }
    }

    let orphan_mappers = match scan_orphan_mappers_by_name(fs, membership) {
        Ok(v) => {
            for om in &v {
                notes.push(PreviewNote::Warn(orphan_mapper_warn_body(&om.mapper)));
            }
            v
        }
        Err(e) => {
            notes.push(PreviewNote::Warn(orphan_scan_warn_body(&e)));
            Vec::new()
        }
    };

    LockCloseSets {
        member_owned,
        orphan_mappers,
    }
}

pub fn cmd_lock<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    dry_run: bool,
) -> Result<(), LockError> {
    cmd_lock_impl(runner, fs, &RealSleeper, config, membership, dry_run)
}

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
    let plan = plan_lock(runner, fs, config, membership)?;
    if dry_run {
        plan.preview().print_colored();
        return Ok(());
    }
    plan.execute(runner, fs, sleeper, membership)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::MockRunner;
    use crate::mapper_close::{CLOSE_RETRY_ATTEMPTS, CLOSE_RETRY_DELAY};
    use crate::test_fixtures::{
        LockNoopSleeper, LockRecordingRunner, lock_count_forget_steps, lock_err_raw,
        lock_forget_step_devices, lock_fs, lock_mounted_runner, lock_ok_raw, lock_test_config,
        lock_test_membership, lock_umount_failed_runner, lock_with_fsid_probe_mocks,
    };
    use std::sync::Mutex;
    use std::time::Duration;

    #[test]
    fn lock_happy_path_unmounts_and_closes() {
        let runner = lock_mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                path: MountPoint("/mnt/storage".into()),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let plan_fs = lock_fs(&[]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let plan =
            plan_lock(&runner, &plan_fs, &config, &membership).expect("plan_lock should succeed");
        assert!(
            plan.member_owned.is_empty(),
            "precondition: plan should record no membership opens"
        );

        let execute_fs = lock_fs(&["/dev/mapper/braid-aaa"]);
        let recording = LockRecordingRunner::new(runner);
        plan.execute(&recording, &execute_fs, &LockNoopSleeper, &membership)
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

    #[test]
    fn lock_already_locked() {
        let runner = MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = lock_fs(&[]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed (already locked)");
    }

    #[test]
    fn lock_partial_state() {
        // Pool not mounted, only aaa mapper open
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MountpointCheck {
                    path: MountPoint("/mnt/storage".to_owned()),
                },
                lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            );
        let fs = lock_fs(&["/dev/mapper/braid-aaa"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed (partial)");
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
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            lock_err_raw("umount /mnt/storage", 32, "target is busy"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-aaa".into(),
            },
            lock_err_raw(
                "cryptsetup close braid-aaa",
                5,
                "Device braid-aaa is still in use.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-bbb".into(),
            },
            lock_err_raw(
                "cryptsetup close braid-bbb",
                5,
                "Device braid-bbb is still in use.",
            ),
        );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should fail on busy");
        assert!(err.to_string().contains("target is busy"));
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
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            lock_err_raw("umount /mnt/storage", 32, "target is busy"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-aaa".into(),
            },
            lock_err_raw(
                "cryptsetup close braid-aaa",
                5,
                "Device braid-aaa is still in use.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-bbb".into(),
            },
            lock_err_raw(
                "cryptsetup close braid-bbb",
                5,
                "Device braid-bbb is still in use.",
            ),
        );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            lock_err_raw(
                "umount /mnt/storage",
                32,
                "umount: /mnt/storage: can't write superblock.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-aaa".into(),
            },
            lock_ok_raw("cryptsetup close braid-aaa"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-bbb".into(),
            },
            lock_ok_raw("cryptsetup close braid-bbb"),
        );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            lock_err_raw(
                "umount /mnt/storage",
                32,
                "umount: /mnt/has target is busy here/storage: can't write superblock.",
            ),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-aaa".into(),
            },
            lock_ok_raw("cryptsetup close braid-aaa"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-bbb".into(),
            },
            lock_ok_raw("cryptsetup close braid-bbb"),
        );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        // If BtrfsDeviceScanForget were not called, MockRunner would return
        // MissingMock and the test would fail.
        cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect("lock should succeed with forget");
    }

    #[test]
    fn lock_forget_failure_is_nonfatal() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ))
        .with_output(
            CmdRequest::Umount {
                mount_point: MountPoint("/mnt/storage".to_owned()),
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
                mapper: "braid-aaa".into(),
            },
            lock_ok_raw("cryptsetup close braid-aaa"),
        )
        .with_output(
            CmdRequest::CryptsetupClose {
                mapper: "braid-bbb".into(),
            },
            lock_ok_raw("cryptsetup close braid-bbb"),
        );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-ccc".into(),
                },
                lock_ok_raw("cryptsetup close braid-ccc"),
            );
        // ccc is not in membership but exists as a mapper → orphan
        let fs = lock_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs =
            lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]).with_dev_mapper_error();
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
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs =
            lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]).with_dev_mapper_error();
        let config = lock_test_config();
        let plan = plan_lock(&runner, &fs, &config, &lock_test_membership())
            .expect("plan_lock should succeed with list_dir failure");
        let output = plan.preview().render();

        assert!(
            output.starts_with(
                "[warn] could not scan /dev/mapper for orphans: permission denied (skipping)\n"
            ),
            "preview must start with the exact [warn] line, got:\n{output}"
        );
        assert!(
            output.contains("close LUKS mapper braid-aaa"),
            "preview must still render membership close steps, got:\n{output}"
        );
        assert!(
            output.contains("close LUKS mapper braid-bbb"),
            "preview must still render membership close steps, got:\n{output}"
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
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ));
        let fs = lock_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
        let config = lock_test_config();
        let plan = plan_lock(&runner, &fs, &config, &lock_test_membership())
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
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ));
        let fs = lock_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
        let config = lock_test_config();
        let plan = plan_lock(&runner, &fs, &config, &lock_test_membership())
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
            .find("[safe       ]")
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
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_err_raw("mountpoint -q /mnt/storage", 1, ""),
        );
        let fs = lock_fs(&[]);
        let config = lock_test_config();
        let plan = plan_lock(&runner, &fs, &config, &lock_test_membership())
            .expect("plan_lock should succeed on already-locked pool");
        let output = plan.preview().render();

        assert_eq!(output, "nothing to do.\n", "unexpected preview: {output:?}");
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
                    mapper: "braid-aaa".into(),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Device braid-aaa is still in use.",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_err_raw(
                    "cryptsetup close braid-bbb",
                    5,
                    "Device braid-bbb is still in use.",
                ),
            );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_err_raw("cryptsetup close braid-aaa", 4, "Device is not active."),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_err_raw("cryptsetup close braid-aaa", 4, "Device is not active."),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-ccc".into(),
                },
                lock_err_raw(
                    "cryptsetup close braid-ccc",
                    5,
                    "Device braid-ccc is still in use.",
                ),
            );
        let fs = lock_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-ccc".into(),
                },
                lock_err_raw("cryptsetup close braid-ccc", 4, "Device is not active."),
            );
        let fs = lock_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-orphan".into(),
                },
                lock_err_raw("cryptsetup close braid-orphan", 4, "Device is not active."),
            );
        let fs = lock_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-orphan",
        ]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_err_raw("cryptsetup close braid-aaa", 4, "Device is not active."),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let runner = LockRecordingRunner::new(inner);
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_err_raw("cryptsetup close braid-aaa", 4, "Device is not active."),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_err_raw("cryptsetup close braid-bbb", 1, "permission denied"),
            );
        let runner = LockRecordingRunner::new(inner);
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                mapper: "braid-bbb".into(),
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
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Target braid-aaa is still active and cannot be removed.",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let runner = LockRecordingRunner::new(inner);
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_err_raw("cryptsetup close braid-aaa", 5, busy_stderr),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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

    #[test]
    // Intent: lock refuses when any exclusive op is active (running balance).
    // Why: unmounting during an exclusive op is unsafe — data corruption risk.
    // Scenario: a RAID1 balance is in progress, operator runs `braid lock`.
    //   Lock must refuse without unmounting or closing any mappers.
    fn lock_refuses_when_exclusive_op_active() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ));
        let fs =
            lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]).with_excl_op("balance");
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

    #[test]
    // Intent: lock refuses when a balance is paused.
    // Why: a paused balance still holds the exclusive lock — unmounting is unsafe.
    // Scenario: operator paused a balance and forgot, then runs `braid lock`.
    fn lock_refuses_when_balance_paused() {
        let runner = lock_with_fsid_probe_mocks(MockRunner::default().with_output(
            CmdRequest::MountpointCheck {
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        ));
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_excl_op("balance paused");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let err = cmd_lock_impl(&runner, &fs, &LockNoopSleeper, &config, &membership, false)
            .expect_err("should refuse — balance is paused");
        let msg = err.to_string();
        assert!(
            msg.contains("in progress"),
            "expected paused-balance refusal, got: {msg}"
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
                path: MountPoint("/mnt/storage".to_owned()),
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

    /// Test-local helper: build a MemberOwnedClose entry from a bare
    /// mapper basename and disk name for compile_lock_steps tests.
    fn mo(mapper: &str, disk_name: &str) -> MemberOwnedClose {
        MemberOwnedClose {
            mapper: MapperName(mapper.into()),
            display_name: DiskName::parse(disk_name).expect("valid test disk name"),
        }
    }

    // Intent: dry-run for lock shows umount + scan forget + close per open mapper.
    // Why: verifies compile_lock_steps produces correct output. The
    // rendered forget command must include the explicit device paths,
    // not the bare kernel-global form.
    // Scenario: pool mounted, 2 open mappers, no orphans.
    #[test]
    fn dry_run_render_lock_mounted_2_disks() {
        use crate::types::MountPoint;
        let mount_point = MountPoint("/mnt/storage".into());
        let member_owned = vec![mo("braid-disk1", "disk1"), mo("braid-disk2", "disk2")];
        let steps = compile_lock_steps(true, &member_owned, &[], &mount_point);
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // 4 steps (umount + scan forget + 2× close), each with 1 command = 8 lines
        assert_eq!(lines.len(), 8, "expected 8 lines, got:\n{output}");
        assert!(lines[0].contains("unmount"));
        assert!(lines[1].contains("$ umount"));
        assert!(lines[2].contains("btrfs device scan --forget"));
        assert!(
            lines[3].contains("--forget /dev/mapper/braid-disk1 /dev/mapper/braid-disk2"),
            "rendered forget command must list pool mapper paths, got: {}",
            lines[3]
        );
        assert!(lines[4].contains("close LUKS mapper braid-disk1"));
        assert!(lines[6].contains("close LUKS mapper braid-disk2"));
    }

    // Intent: dry-run when not mounted skips umount/scan, shows only close.
    // Why: verifies conditional step omission.
    // Scenario: pool not mounted, 1 mapper still open.
    #[test]
    fn dry_run_lock_not_mounted_1_open() {
        use crate::types::MountPoint;
        let mount_point = MountPoint("/mnt/storage".into());
        let member_owned = vec![mo("braid-disk1", "disk1")];
        let steps = compile_lock_steps(false, &member_owned, &[], &mount_point);
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // 1 step (close), 2 lines
        assert_eq!(lines.len(), 2, "expected 2 lines, got:\n{output}");
        assert!(lines[0].contains("close LUKS mapper"));
        assert!(!output.contains("unmount"));
    }

    // Intent: dry-run when nothing to do returns empty steps.
    // Why: verifies the "nothing to do" case.
    // Scenario: pool not mounted, all mappers closed, no orphans.
    #[test]
    fn dry_run_lock_nothing_to_do() {
        use crate::types::MountPoint;
        let mount_point = MountPoint("/mnt/storage".into());
        let steps = compile_lock_steps(false, &[], &[], &mount_point);
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
        let mount_point = MountPoint("/mnt/storage".into());
        let member_owned = vec![mo("braid-aaa", "aaa"), mo("braid-bbb", "bbb")];
        let steps = compile_lock_steps(true, &member_owned, &[], &mount_point);
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
    // docs/principles.md:18) with a stale scan entry, reviving the
    // cryptsetup-close-btrfs-held race for the orphan.
    // Scenario: 1 membership mapper, 1 orphan; forget devices = union.
    #[test]
    fn dry_run_lock_forget_step_includes_orphans() {
        use crate::types::MountPoint;
        let mount_point = MountPoint("/mnt/storage".into());
        let member_owned = vec![mo("braid-aaa", "aaa")];
        let orphan_mappers = vec![OrphanMapper {
            mapper: MapperName("braid-orphan".into()),
            disk_name: "orphan".into(),
        }];
        let steps = compile_lock_steps(true, &member_owned, &orphan_mappers, &mount_point);
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
        let mount_point = MountPoint("/mnt/storage".into());
        let steps = compile_lock_steps(true, &[], &[], &mount_point);
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
                    mapper: "braid-aaa".into(),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            );
        let runner = LockRecordingRunner::new(inner);
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
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
                    mapper: "braid-aaa".into(),
                },
                lock_ok_raw("cryptsetup close braid-aaa"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_ok_raw("cryptsetup close braid-bbb"),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-ccc".into(),
                },
                lock_ok_raw("cryptsetup close braid-ccc"),
            );
        let runner = LockRecordingRunner::new(inner);
        let fs = lock_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
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
                mapper: "braid-aaa".into(),
            },
            lock_err_raw(
                "cryptsetup close braid-aaa",
                5,
                "Device braid-aaa is still in use.",
            ),
        );

        let err = close_mapper_with_retry(&runner, &sleeper, "braid-aaa", false)
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

    /*
     * Intent: public cmd_lock wires a real sleeper. An always-busy
     *   mapper makes the wrapper pay measurable wall-clock sleep time
     *   before returning DeviceBusy, proving &RealSleeper (not
     *   &LockNoopSleeper) is on the hot path.
     *
     * Why it exists: the helper-level RecordingSleeper test proves
     *   close_mapper_with_retry uses CLOSE_RETRY_DELAY, but does not
     *   prove the public wrapper hands in &RealSleeper. A regression
     *   that shipped &LockNoopSleeper (or dropped the sleeper entirely) in
     *   production would leave lock reliability race-dependent and
     *   pass every helper-level unit test -- including
     *   braid-lock-btrfs-held.py, which only asserts success and does
     *   not deterministically force the retry path.
     *
     * Scenario: umount succeeds, then every mapper close returns
     *   "is still in use" so the retry loop runs to exhaustion. Because
     *   umount did not set umount_error, DeviceBusy is NOT suppressed:
     *   it becomes first_mapper_error and is the returned value. Wall
     *   time is bounded below by (CLOSE_RETRY_ATTEMPTS - 1) *
     *   CLOSE_RETRY_DELAY for a single mapper; we assert a tolerant
     *   lower bound of that amount to stay robust on slow CI while
     *   still failing loudly if no real sleep happened.
     */
    #[test]
    fn cmd_lock_wrapper_uses_real_sleeper() {
        use std::time::Instant;

        let runner = lock_mounted_runner()
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-aaa".into(),
                },
                lock_err_raw(
                    "cryptsetup close braid-aaa",
                    5,
                    "Device braid-aaa is still in use.",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupClose {
                    mapper: "braid-bbb".into(),
                },
                lock_err_raw(
                    "cryptsetup close braid-bbb",
                    5,
                    "Device braid-bbb is still in use.",
                ),
            );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"]);
        let config = lock_test_config();
        let membership = lock_test_membership();

        let start = Instant::now();
        let err = cmd_lock(&runner, &fs, &config, &membership, false)
            .expect_err("should fail with DeviceBusy after retry exhaustion");
        let elapsed = start.elapsed();

        assert!(
            matches!(err, LockError::DeviceBusy(_)),
            "expected DeviceBusy from public wrapper, got: {err:?}"
        );

        // Both mappers hit the full retry loop: expected total real
        // sleep is 2 * (CLOSE_RETRY_ATTEMPTS - 1) * CLOSE_RETRY_DELAY =
        // 2s. We assert a tolerant lower bound of one mapper's worth
        // (~900ms) so scheduler jitter on slow CI does not cause flake,
        // while still catching a LockNoopSleeper regression (which would
        // complete in microseconds).
        let min_expected =
            CLOSE_RETRY_DELAY * (CLOSE_RETRY_ATTEMPTS - 1) - Duration::from_millis(100);
        assert!(
            elapsed >= min_expected,
            "wrapper must use RealSleeper -- elapsed {:?} < min {:?}",
            elapsed,
            min_expected,
        );
    }

    // -- Migration-Phase-4 lock tests -----------------------------------

    /// Build a synthetic 2-disk PoolState whose mappers are the given
    /// observed names and whose LUKS UUIDs match
    /// `lock_test_membership` so the Full-arm classifier yields two
    /// MemberOwnedClose entries.
    fn synthetic_pool_state(mapper_aaa: &str, mapper_bbb: &str) -> crate::types::PoolState {
        use crate::types::{LuksUuid, MapperName, PoolDevice, PoolState};
        PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName(mapper_aaa.into()),
                    luks_uuid: LuksUuid::parse("00000000-0000-0000-0000-0000000002bc").unwrap(),
                    devid: 1,
                    underlying: "/dev/disk/by-id/a".into(),
                },
                PoolDevice {
                    mapper: MapperName(mapper_bbb.into()),
                    luks_uuid: LuksUuid::parse("00000000-0000-0000-0000-0000000002bd").unwrap(),
                    devid: 2,
                    underlying: "/dev/disk/by-id/b".into(),
                },
            ],
            missing_count: 0,
            total_devices: 2,
            fsid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
            missing_devids: vec![],
            null_underlying: vec![],
        }
    }

    /// Intent: in LockSnapshot::Full, a drifted member mapper
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
        let mut notes = Vec::new();
        let pool = synthetic_pool_state("braid-WRONG", "braid-bbb");
        let runner = MockRunner::default();
        let close_sets = super::build_close_sets_full(&runner, &fs, &pool, &membership, &mut notes);
        // Both members are classified by UUID despite the drift.
        let observed: Vec<String> = close_sets
            .member_owned
            .iter()
            .map(|m| m.mapper.0.clone())
            .collect();
        assert_eq!(
            observed,
            vec!["braid-WRONG".to_owned(), "braid-bbb".to_owned()]
        );
        let display: Vec<String> = close_sets
            .member_owned
            .iter()
            .map(|m| m.display_name.as_str().to_owned())
            .collect();
        assert_eq!(display, vec!["aaa".to_owned(), "bbb".to_owned()]);
        assert!(close_sets.orphan_mappers.is_empty());
    }

    /// Intent: in LockSnapshot::Full, the forget_devs set passed to
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
        let mut notes = Vec::new();
        let pool = synthetic_pool_state("braid-WRONG", "braid-bbb");
        let runner = MockRunner::default();
        let close_sets = super::build_close_sets_full(&runner, &fs, &pool, &membership, &mut notes);
        let mp = MountPoint("/mnt/storage".into());
        let steps = super::compile_lock_steps(
            true,
            &close_sets.member_owned,
            &close_sets.orphan_mappers,
            &mp,
        );
        assert_eq!(
            lock_forget_step_devices(&steps),
            vec![
                "/dev/mapper/braid-WRONG".to_string(),
                "/dev/mapper/braid-bbb".to_string(),
            ],
            "forget set must use observed mapper, not reconstructed",
        );
    }

    /// Intent: LockSnapshot::FsidOnly preserves today's close order
    /// (member-owned before orphan) so the two arms produce
    /// identical orders on identical inputs.
    /// Why: plan 3215-3226.
    /// Scenario: seed 702.
    #[test]
    fn fsid_only_arm_preserves_member_then_orphan_close_order() {
        let fs = lock_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-ccc",
        ]);
        let membership = lock_test_membership();
        let mut notes = Vec::new();
        let close_sets = super::build_close_sets_fsid_only(&fs, &membership, &mut notes);
        let mp = MountPoint("/mnt/storage".into());
        let steps = super::compile_lock_steps(
            true,
            &close_sets.member_owned,
            &close_sets.orphan_mappers,
            &mp,
        );
        let forget = lock_forget_step_devices(&steps);
        // Members first, orphan last -- mirroring close_set_paths order.
        assert_eq!(
            forget,
            vec![
                "/dev/mapper/braid-aaa".to_string(),
                "/dev/mapper/braid-bbb".to_string(),
                "/dev/mapper/braid-ccc".to_string(),
            ]
        );
    }

    /// Intent: the FsidOnly warning carries the two pinned operator-
    /// relevant substrings independently.
    /// Why: plan 3228-3239. The full text is pinned with two
    /// `assert!.contains(...)` calls so a future edit that drops
    /// either signal surfaces in CI.
    /// Scenario: seed 703.
    #[test]
    fn fsid_only_warn_body_contains_pinned_substrings() {
        // Synthesize a ProbeError::Cmd to feed into the warn body.
        let pe = ProbeError::PoolDevice {
            mapper: "/mnt/storage".into(),
            detail: "synthetic".into(),
        };
        let body = super::fsid_only_warn_body(&pe);
        assert!(
            body.contains("Mapper drift detection is disabled for this run."),
            "missing first pinned substring; body was: {body}"
        );
        assert!(
            body.contains("an unrelated disk opened under that name will be torn down."),
            "missing second pinned substring; body was: {body}"
        );
    }

    /// Intent: scan_orphan_mappers_by_name falls through a malformed
    /// `braid-<not-a-valid-name>` mapper as an orphan rather than
    /// silently skipping. The OrphanMapper.disk_name carries the raw
    /// text for the warning body.
    /// Why: plan 3257-3286.
    /// Scenario: seed 704.
    #[test]
    fn fsid_only_malformed_mapper_falls_through_to_orphan() {
        // A mapper named "braid-..foo" -- DiskName::parse rejects it.
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-..foo"]);
        let membership = lock_test_membership();
        let orphans = super::scan_orphan_mappers_by_name(&fs, &membership).unwrap();
        let names: Vec<&str> = orphans.iter().map(|o| o.disk_name.as_str()).collect();
        assert!(
            names.contains(&"..foo"),
            "malformed mapper basename must be carried as orphan disk_name, got: {names:?}",
        );
    }

    /// Intent: classify_stranded_mapper demotes per-mapper failures
    /// to a logged-warning Orphan rather than aborting the lock.
    /// Why: plan 3134-3196. Per-mapper degrade keeps one cryptsetup
    /// hiccup from tanking the whole lock.
    /// Scenario: seed 705 -- a stranded mapper whose cryptsetup
    /// status call returns a CmdError (not Ok).
    #[test]
    fn full_arm_stranded_mapper_classify_failure_demotes_to_orphan() {
        let fs = lock_fs(&[
            "/dev/mapper/braid-aaa",
            "/dev/mapper/braid-bbb",
            "/dev/mapper/braid-stranded",
        ]);
        let membership = lock_test_membership();
        // braid-stranded is NOT in pool.devices, so it gets routed
        // through classify_stranded_mapper. The MockRunner has no
        // CryptsetupStatus mock for that mapper -- it returns
        // MissingMock (a CmdError), and the helper demotes to
        // Orphan + emits the pinned warning.
        let pool = synthetic_pool_state("braid-aaa", "braid-bbb");
        let runner = MockRunner::default();
        let mut notes = Vec::new();
        let close_sets = super::build_close_sets_full(&runner, &fs, &pool, &membership, &mut notes);
        // Member-owned still has the two pool.devices entries.
        assert_eq!(close_sets.member_owned.len(), 2);
        // Stranded mapper became an orphan.
        let orphan_mappers: Vec<&str> = close_sets
            .orphan_mappers
            .iter()
            .map(|o| o.mapper.0.as_str())
            .collect();
        assert_eq!(orphan_mappers, vec!["braid-stranded"]);
    }

    /// Intent: probe_pool's NotBtrfs error variant is NOT routed
    /// through the FsidOnly fallback -- it aborts the lock with the
    /// preserved mounted-non-btrfs message.
    /// Why: plan 3320 -- only per-device variants are catchable by
    /// the FsidOnly fallback so a real configuration error
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
                path: MountPoint("/mnt/storage".to_owned()),
            },
            lock_ok_raw("mountpoint -q /mnt/storage"),
        );
        let fs = lock_fs(&["/dev/mapper/braid-aaa", "/dev/mapper/braid-bbb"])
            .with_mountinfo("36 35 0:32 / /mnt/storage rw,noatime shared:1 - ext4 /dev/sda1 rw\n");
        let config = lock_test_config();
        let membership = lock_test_membership();

        let result = plan_lock(&runner, &fs, &config, &membership);
        let err = match result {
            Err(e) => e,
            Ok(_) => panic!("NotBtrfs must surface as an abort, not FsidOnly fallback"),
        };
        let msg = err.to_string();
        assert!(
            msg.contains("not btrfs") && msg.contains("ext4"),
            "expected NotBtrfs-style message naming ext4, got: {msg}"
        );
    }
}
