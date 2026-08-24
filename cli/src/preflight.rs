use std::fmt;
use std::path::Path;

use crate::btrfs_ioctl::BtrfsDevInfo;
use crate::capacity;
use crate::cmd::CmdRequest;
use crate::cmd::CommandRunner;
use crate::confirm;
use crate::journal;
use crate::luks::LUKS2_DEFAULT_HDR_SIZE;
use crate::membership::PoolMembership;
use crate::mount_check::{self, mount_entry_at_via_fs};
use crate::parse::parse_cryptsetup_luks_dump;
use crate::parse::types::{
    BtrfsBgType, BtrfsDeviceUsageEntry, BtrfsDfOutput, Luks2SegmentSize, UpsSeverity,
};
use crate::preview::PreviewNote;
use crate::probe::Filesystem;
use crate::repair_hint;
use crate::state_paths::StatePaths;
use crate::status::format_bytes;
use crate::types::{Devid, Fsid, MountPoint, PoolState};
use crate::ups::{UpsQueryError, query_ups};
use crate::util::detail_suffix;

/// Refuse if pool.json lists members but the pool is not mounted (locked).
/// Catches the silent-bootstrap case where `braid add` against a locked pool
/// would otherwise overwrite pool.json with a one-disk pool, orphaning the
/// existing locked members.
pub fn check_pool_unlocked_if_membership_exists(
    membership: &PoolMembership,
    pool: &PoolState,
) -> Result<(), String> {
    if pool.mounted || membership.is_empty() {
        return Ok(());
    }
    let n = membership.len();
    let mut names: Vec<&str> = membership.names().map(|n| n.as_str()).collect();
    names.sort();
    Err(format!(
        "pool exists but is not unlocked -- pool.json lists {n} member{}: {}.\n\
         Run `braid unlock` first, then re-run `braid add`.\n\
         If pool.json is stale (members no longer plugged in or you intend \
         to start over), reconcile with `braid discover` / `braid remove-missing`, \
         or remove /var/lib/braid/pool.json manually.",
        if n == 1 { "" } else { "s" },
        names.join(", ")
    ))
}

/// Refuse if a pending-operation journal exists.
/// This gate is called by the membership/mount/key-enrollment commands
/// (`add`, `remove`, `remove-missing`, `replace`, `unlock`, `enroll`, and
/// `discover --write`). `recover` is the only journal-clearing path; read-only
/// diagnostics and cleanup surfaces (`status`, `doctor`, `lock`, bare
/// `discover`) stay available.
pub fn check_no_pending_operation(paths: &StatePaths) -> Result<(), String> {
    match journal::load_journal(paths) {
        Ok(Some(j)) => Err(format!(
            "interrupted operation detected (pending-op.json exists, started {}).\n\
             Pool membership may be inconsistent. Run 'braid recover' to reconcile \
             from live pool state, or 'braid status' to inspect.",
            j.started_at
        )),
        Ok(None) => Ok(()),
        Err(e) => Err(format!(
            "cannot read pending-op.json: {e}. Remove it manually or run 'braid recover'."
        )),
    }
}

// ---------------------------------------------------------------------------
// Exclusive operation check (sysfs-based)
// ---------------------------------------------------------------------------

/// Recognized btrfs exclusive busy operations, as reported by
/// `/sys/fs/btrfs/{fsid}/exclusive_operation`. Does not include the
/// kernel's `"none"` sentinel -- absence of a busy op is modeled as
/// `Ok(None)` from [`ExclusiveOp::parse`], so consumers cannot
/// accidentally treat idle as a member of this enum.
///
/// The kernel emits these strings from `btrfs_exclusive_operation_show`
/// (`reference/linux/fs/btrfs/sysfs.c`) -- that switch is the authority for
/// what this file can contain. btrfs-progs is a fellow parser of the same
/// file (`btrfs-progs v6.19.1, reference/btrfs-progs/common/utils.c
/// (get_fs_exclop, exclop_def[])`), not the source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusiveOp {
    Balance,
    BalancePaused,
    DeviceAdd,
    /// The kernel writes "device remove" -- not "device delete" as
    /// btrfs-man5.rst sometimes says. The string is the
    /// `BTRFS_EXCLOP_DEV_REMOVE` arm of `btrfs_exclusive_operation_show`
    /// (`reference/linux/fs/btrfs/sysfs.c`).
    DeviceRemove,
    DeviceReplace,
    Resize,
    SwapActivate,
}

impl ExclusiveOp {
    /// Parse a single value from `/sys/fs/btrfs/{fsid}/exclusive_operation`.
    /// Expects caller-trimmed input; `"none"` means idle, recognized busy
    /// values return `Ok(Some(op))`, and unknown values return the input.
    pub fn parse(s: &str) -> Result<Option<Self>, String> {
        match s {
            "none" => Ok(None),
            "balance" => Ok(Some(Self::Balance)),
            "balance paused" => Ok(Some(Self::BalancePaused)),
            "device add" => Ok(Some(Self::DeviceAdd)),
            "device remove" => Ok(Some(Self::DeviceRemove)),
            "device replace" => Ok(Some(Self::DeviceReplace)),
            "resize" => Ok(Some(Self::Resize)),
            "swap activate" => Ok(Some(Self::SwapActivate)),
            other => Err(other.to_owned()),
        }
    }
}

impl fmt::Display for ExclusiveOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Balance => write!(f, "balance"),
            // Parenthesized so `braid lock`'s `RejectAnyBusy` template
            // (`"cannot lock: {op} is in progress..."`) reads naturally.
            // `braid idle` has its own standalone-label surface
            // (`"balance paused"`) via `BusyReason::Display` in
            // `cli/src/idle.rs`.
            Self::BalancePaused => write!(f, "balance (paused)"),
            Self::DeviceAdd => write!(f, "device add"),
            Self::DeviceRemove => write!(f, "device remove"),
            Self::DeviceReplace => write!(f, "device replace"),
            Self::Resize => write!(f, "resize"),
            Self::SwapActivate => write!(f, "swap activate"),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ExclusiveOpError {
    #[error("an exclusive operation is already running: {0}")]
    Busy(ExclusiveOp),
    #[error("cannot read exclusive operation status: {0}")]
    Read(std::io::Error),
    #[error("unrecognized exclusive operation: {0:?}")]
    Unrecognized(String),
}

/// Shared sysfs reader so policy preflight and host-wide scans classify the
/// kernel state through the same path, trim, and parser contract. Takes a raw
/// `&str` entry, not an `&Fsid`: the scoped callers pass a validated
/// `fsid.as_str()`, but `check_any_btrfs_exclusive_op` passes an unvalidated
/// `/sys/fs/btrfs` directory name that must be *read* (fail-closed) even when
/// it is not a parseable UUID -- so the leaf cannot demand a typed FSID.
fn read_exclop_for_sysfs_entry<F: Filesystem + ?Sized>(
    fs: &F,
    entry: &str,
) -> Result<Option<ExclusiveOp>, ExclusiveOpError> {
    let path = format!("/sys/fs/btrfs/{entry}/exclusive_operation");
    let contents = fs.read_to_string(&path).map_err(ExclusiveOpError::Read)?;
    ExclusiveOp::parse(contents.trim()).map_err(ExclusiveOpError::Unrecognized)
}

/// How to handle `/sys/fs/btrfs/<fsid>/exclusive_operation` when it is not
/// `none`.
enum ExclusiveOpPolicy {
    /// `braid lock` behavior:
    /// hard-fail on any active exclusive op.
    ///
    /// Why: lock is teardown (unmount + close). It must not proceed while btrfs
    /// is mid balance/device-add/device-remove/device-replace/resize.
    RejectAnyBusy,

    /// Mutating command behavior (`add`, `remove`, `remove-missing`, `replace`):
    /// - `balance paused` => hard error (operator must resume/cancel)
    /// - any other busy state => `Ok(Some(op))` for the caller to surface
    ///   as a `PreviewNote::Info`
    ///
    /// Why: these commands invoke btrfs with `--enqueue`, so kernel serialization
    /// is the correctness mechanism and avoids TOCTOU-style preflight busy failures.
    /// Paused balance is the exception because it can block indefinitely.
    RejectPausedBalanceElseEnqueue,

    /// `braid lock --systemd-stop` behavior:
    /// a running balance is safe to pause before unmount, and a paused
    /// balance is safe to unmount; every other exclusive op is still unsafe
    /// to unmount under.
    AllowBalanceElseReject,
}

/// Apply `policy` to the current exclusive-op state read from sysfs.
///
/// `Ok(None)` means the pool is idle. `Ok(Some(op))` is reachable only under
/// `RejectPausedBalanceElseEnqueue` when a non-paused exclusive op is in
/// flight, or under `AllowBalanceElseReject` for a running balance that the
/// systemd-stop executor must pause before unmount. `Err(msg)` means the
/// policy rejected the state (paused balance, any busy under `RejectAnyBusy`,
/// unrecognized value, or sysfs read failure).
fn check_exclusive_op_with_policy<F: Filesystem + ?Sized>(
    fs: &F,
    fsid: &Fsid,
    policy: ExclusiveOpPolicy,
) -> Result<Option<ExclusiveOp>, String> {
    let op = match read_exclop_for_sysfs_entry(fs, fsid.as_str()).map_err(|e| e.to_string())? {
        None => return Ok(None),
        Some(op) => op,
    };
    match policy {
        ExclusiveOpPolicy::RejectAnyBusy => Err(format!(
            "cannot lock: {op} is in progress. Wait for it to finish first."
        )),
        ExclusiveOpPolicy::RejectPausedBalanceElseEnqueue => match op {
            ExclusiveOp::BalancePaused => {
                Err("a btrfs balance is paused. Resume or cancel it before proceeding.".into())
            }
            _ => Ok(Some(op)),
        },
        ExclusiveOpPolicy::AllowBalanceElseReject => match op {
            ExclusiveOp::Balance => Ok(Some(op)),
            ExclusiveOp::BalancePaused => Ok(None),
            _ => Err(format!(
                "cannot lock: {op} is in progress. Wait for it to finish first."
            )),
        },
    }
}

/// Names of `/sys/fs/btrfs/` entries that are not per-filesystem fsid dirs
/// and therefore do not expose `exclusive_operation`. Source: the sysfs
/// path table in `reference/linux/fs/btrfs/sysfs.c`, which lists `features`
/// and `debug` as the only non-`<uuid>` entries.
///
/// Allowlist (rather than "skip any NotFound") so a real fsid dir whose
/// `exclusive_operation` disappears mid-scan -- e.g. concurrent unmount --
/// surfaces as `ExclusiveOpError::Read` instead of being silently treated
/// as a pseudo-dir. The fail-closed contract requires that every listed
/// fsid is actually checked.
const BTRFS_SYSFS_NON_FSID_ENTRIES: &[&str] = &["features", "debug"];

/// Check `/sys/fs/btrfs/*/exclusive_operation` across every btrfs filesystem
/// the kernel exposes, returning busy as soon as any one reports a non-`none`
/// state.
///
/// Used by `braid idle` to avoid an extra `findmnt` + `btrfs filesystem show`
/// round trip just to discover the pool's UUID. Semantics: any in-flight
/// exclusive op on any btrfs fs on the host counts as busy. On a typical
/// braid host (one btrfs filesystem, the pool) this is identical to a
/// fsid-scoped check; on a host with btrfs root alongside the pool the
/// reported `BusyReason` may name an op on the non-pool fs, but the suspend
/// decision is still correct -- autosuspend's job is to err conservative.
///
/// Skips known non-fsid entries (`features`, `debug`) by name before any
/// read; see `BTRFS_SYSFS_NON_FSID_ENTRIES`. Every other listed entry is
/// treated as a fsid dir, and any read failure on it -- including
/// `NotFound` from a concurrent unmount race -- is `ExclusiveOpError::Read`.
///
/// Fail-closed: list_dir IO errors, any read error on a fsid dir,
/// unrecognized parser values, and an empty `/sys/fs/btrfs/` after the
/// caller has already confirmed a btrfs mount all return `Err`.
pub(crate) fn check_any_btrfs_exclusive_op<F: Filesystem + ?Sized>(
    fs: &F,
) -> Result<(), ExclusiveOpError> {
    let entries = fs
        .list_dir("/sys/fs/btrfs")
        .map_err(ExclusiveOpError::Read)?;
    let mut found_fsid_dir = false;
    for entry in entries {
        if BTRFS_SYSFS_NON_FSID_ENTRIES.contains(&entry.as_str()) {
            continue;
        }
        found_fsid_dir = true;
        if let Some(op) = read_exclop_for_sysfs_entry(fs, &entry)? {
            return Err(ExclusiveOpError::Busy(op));
        }
    }
    if !found_fsid_dir {
        return Err(ExclusiveOpError::Read(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "no btrfs filesystem found in /sys/fs/btrfs",
        )));
    }
    Ok(())
}

/// Refuse if the pool is mounted read-only.
/// Reads mountinfo directly so the check sees both VFS mount flags and
/// filesystem/superblock flags without spawning a subprocess.
///
/// `Ok(None)` = writable mount. `Ok(Some(body))` = the probe itself
/// failed; the caller wraps `body` in a `PreviewNote::Warn` so operators
/// know the ro guard did not run.
/// `Err(msg)` = pool is mounted read-only.
fn check_not_read_only<F: Filesystem + ?Sized>(
    fs: &F,
    mount_point: &MountPoint,
) -> Result<Option<String>, String> {
    let entry = match mount_entry_at_via_fs(fs, mount_point.as_str()) {
        Ok(Some(entry)) => entry,
        Ok(None) => {
            return Ok(Some(
                "mount point not present in /proc/self/mountinfo".to_string(),
            ));
        }
        Err(e) => return Ok(Some(format!("read /proc/self/mountinfo: {e}"))),
    };

    if mount_check::entry_is_read_only(&entry) {
        return Err(format!(
            "pool is mounted read-only. Remount read-write first:\n  \
                 mount -o remount,rw {mount_point}"
        ));
    }
    Ok(None)
}

/// Refuse if the pool has missing devices.
pub fn check_no_missing_devices(missing_count: u64, action: &str) -> Result<(), String> {
    if missing_count > 0 {
        let repair_command = repair_hint::missing_replace_command(None);
        let status_hint = repair_hint::see_missing_names_and_devids_in_status(missing_count);
        Err(format!(
            "pool has {missing_count} missing device{}. \
             Resolve the missing device{} first -- repair with \
             `{repair_command}`, or forget the entry with \
             `braid remove-missing` -- then {action}. \
             {status_hint}",
            if missing_count == 1 { "" } else { "s" },
            if missing_count == 1 { "" } else { "s" },
        ))
    } else {
        Ok(())
    }
}

/// Check that remaining devices have enough RAID1-aware space to absorb the
/// allocations from the target device(s) being removed or relocated.
///
/// Checks per allocation type (Data, Metadata, System) independently, because
/// the kernel allocates chunks per type and cannot use Data space for Metadata.
///
/// For RAID1, two constraints must hold:
///   1. At least 2 remaining devices must have unallocated space (RAID1 requires
///      two devices with capacity to write a new chunk).
///   2. Effective RAID1 capacity = min(largest, rest) where largest is the
///      biggest device's unallocated space and rest is the sum of all others.
///      Each RAID1 chunk needs space on 2 devices, so a device with more
///      free space than all others combined is bottlenecked by what those
///      others can provide.
pub fn check_raid1_relocation_space(
    target_devs: &[&BtrfsDeviceUsageEntry],
    remaining_devs: &[&BtrfsDeviceUsageEntry],
) -> Result<(), String> {
    for alloc_type in &["Data", "Metadata", "System"] {
        let bytes_on_target: u64 = target_devs
            .iter()
            .map(|d| d.allocated_by_type(alloc_type))
            .sum();

        if bytes_on_target == 0 {
            continue;
        }

        let mut remaining_unalloc: Vec<u64> =
            remaining_devs.iter().map(|d| d.unallocated).collect();
        remaining_unalloc.sort_unstable_by(|a, b| b.cmp(a));

        let devices_with_space = remaining_unalloc.iter().filter(|&&s| s > 0).count();
        if devices_with_space < 2 {
            return Err(format!(
                "cannot relocate {} chunks: fewer than 2 remaining devices \
                 have unallocated space (need space on 2 devices for RAID1)",
                alloc_type
            ));
        }

        let raid1_capacity = capacity::raid1_chunk_pair_capacity(&remaining_unalloc);

        if raid1_capacity < bytes_on_target {
            return Err(format!(
                "not enough space to relocate {} chunks.\n\n  \
                 {} allocated on device{} being removed: {}\n  \
                 RAID1 capacity on remaining devices: {}\n\n\
                 Each RAID1 chunk requires space on 2 devices simultaneously.",
                alloc_type,
                alloc_type,
                if target_devs.len() == 1 { "" } else { "s" },
                format_bytes(bytes_on_target),
                format_bytes(raid1_capacity),
            ));
        }
    }
    Ok(())
}

/// Check that the surviving device can hold all live data after a 2->1
/// eviction (RAID1 data -> single, RAID1 metadata/system -> DUP).
///
/// Uses logical usage from `btrfs filesystem df` rather than per-device
/// allocations, so it is correct regardless of current profile mix
/// (RAID1, single, DUP, or leftover chunks from an interrupted balance).
///
/// Post-balance + post-remove demand on the survivor:
///   Data (single):     Data.used
///   Metadata (DUP):    2 * Metadata.used
///   System (DUP):      2 * System.used
/// Usable survivor capacity = device_size - device_slack.
///
/// GlobalReserve is excluded -- it is an internal emergency reservation
/// carved out of Metadata, not additional on-disk data.
pub fn check_single_survivor_capacity(
    df: &BtrfsDfOutput,
    survivor: &BtrfsDeviceUsageEntry,
) -> Result<(), String> {
    let sum_bg = |t: BtrfsBgType| -> u64 {
        df.entries
            .iter()
            .filter(|e| e.bg_type == t)
            .map(|e| e.bg_used)
            .sum()
    };
    let data = sum_bg(BtrfsBgType::Data);
    let metadata = sum_bg(BtrfsBgType::Metadata);
    let system = sum_bg(BtrfsBgType::System);
    let needed = data + 2 * metadata + 2 * system;
    let usable = survivor.device_size.saturating_sub(survivor.device_slack);
    if needed > usable {
        return Err(format!(
            "not enough space on surviving device after RAID1 -> single conversion.\n  \
             data + 2 * metadata + 2 * system: {}\n  \
             surviving device usable capacity:  {}\n\n\
             Free up space by deleting files first, or `braid add` a larger disk.",
            format_bytes(needed),
            format_bytes(usable),
        ));
    }
    Ok(())
}

/// Minimal source identity for replace-size preflight; devid is the authority
/// btrfs itself uses for `BTRFS_IOC_DEV_INFO`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReplaceSourceProbe {
    pub devid: Devid,
}

/// Candidate replacement device's raw byte size (lsblk `-b`). Distinct from
/// `Luks2SegmentOffset` so `mapper_capacity_from_dynamic_segment` cannot
/// transpose size and offset: a swap inverts the capacity guard and would
/// format or accept an undersized disk before `btrfs replace`'s own check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawDeviceSize(u64);

/// LUKS2 data-segment offset in bytes (real luksDump offset for existing
/// targets, the default header size for fresh ones). Subtracted from
/// `RawDeviceSize` to model mapper capacity; typed apart from it so the
/// subtraction operands cannot be reversed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Luks2SegmentOffset(u64);

/// Target state needed to compute the mapper capacity btrfs will compare
/// against the source device during `btrfs replace start`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplaceTargetProbe<'a> {
    PresentLuks { by_id: &'a str },
    PresentNotLuks { by_id: &'a str },
}

/// Refuse `braid replace` before journal write or LUKS format when the target
/// mapper capacity is smaller than btrfs's source `total_bytes`.
pub fn check_replace_target_capacity<R, D>(
    runner: &R,
    dev_info: &D,
    mount: &Path,
    source: ReplaceSourceProbe,
    target: ReplaceTargetProbe<'_>,
) -> Result<(), String>
where
    R: CommandRunner,
    D: BtrfsDevInfo + ?Sized,
{
    let source_total_bytes = dev_info.total_bytes(mount, source.devid).map_err(|e| {
        format!(
            "failed to read btrfs total_bytes for devid {}: {e}",
            source.devid
        )
    })?;
    if source_total_bytes == 0 {
        return Err(format!(
            "btrfs reports total_bytes 0 for source devid {} -- cannot verify the new disk is large enough",
            source.devid
        ));
    }

    let (target_by_id, target_capacity) = match target {
        ReplaceTargetProbe::PresentLuks { by_id } => {
            let raw_target = target_raw_size(runner, by_id)?;
            let raw = runner
                .run(&CmdRequest::CryptsetupLuksDump {
                    device: by_id.to_owned(),
                })
                .map_err(|e| {
                    format!(
                        "failed to run cryptsetup luksDump --dump-json-metadata for target {by_id}: {e}"
                    )
                })?;
            let parsed = parse_cryptsetup_luks_dump(&raw).map_err(|e| {
                format!("failed to parse LUKS2 segment metadata for target {by_id}: {e}")
            })?;
            let capacity = match parsed.segment_size {
                Luks2SegmentSize::Dynamic => mapper_capacity_from_dynamic_segment(
                    raw_target,
                    Luks2SegmentOffset(parsed.segment_offset_bytes),
                    by_id,
                )?,
                Luks2SegmentSize::Fixed(0) => {
                    return Err(
                        "LUKS2 segment 0 has fixed size 0 -- header is malformed".to_owned()
                    );
                }
                Luks2SegmentSize::Fixed(n) => n,
            };
            (by_id, capacity)
        }
        ReplaceTargetProbe::PresentNotLuks { by_id } => {
            let raw_target = target_raw_size(runner, by_id)?;
            let capacity = mapper_capacity_from_dynamic_segment(
                raw_target,
                Luks2SegmentOffset(LUKS2_DEFAULT_HDR_SIZE),
                by_id,
            )?;
            (by_id, capacity)
        }
    };

    if target_capacity < source_total_bytes {
        return Err(format!(
            "new disk is smaller than the disk being replaced -- refusing to luksFormat / proceed. \
             source devid {} btrfs size {} ({}), target {} mapper capacity {} ({}). \
             Use a target at least as large as the source.",
            source.devid,
            source_total_bytes,
            format_bytes(source_total_bytes),
            target_by_id,
            target_capacity,
            format_bytes(target_capacity),
        ));
    }

    Ok(())
}

fn target_raw_size<R: CommandRunner>(runner: &R, by_id: &str) -> Result<RawDeviceSize, String> {
    confirm::query_disk_hw_info(runner, by_id)
        .size
        .map(RawDeviceSize)
        .ok_or_else(|| {
            format!(
                "failed to read raw size for target {by_id} with lsblk -- cannot verify the new disk is large enough"
            )
        })
}

/// Mapper capacity btrfs compares against the source `total_bytes`, computed
/// as `raw - offset` with no sector_size rounding: cryptsetup sizes the
/// dm-crypt device that way in 512B sectors exactly (`device_block_adjust`),
/// and dm-crypt rejects -- never rounds -- a mapper whose length is not a
/// sector_size multiple (`crypt_ctr`), so an existing container is exact at
/// any sector_size. The offset is the caller's: existing LUKS targets pass
/// their real luksDump segment offset; fresh targets pass the default 16 MiB
/// offset, which holds because braid rejects offset/sector-size format flags.
fn mapper_capacity_from_dynamic_segment(
    raw_target: RawDeviceSize,
    offset: Luks2SegmentOffset,
    by_id: &str,
) -> Result<u64, String> {
    if raw_target.0 <= offset.0 {
        return Err(format!(
            "target raw size {} ({}) is not larger than LUKS2 segment offset {} ({}) for {} -- header may be corrupt",
            raw_target.0,
            format_bytes(raw_target.0),
            offset.0,
            format_bytes(offset.0),
            by_id,
        ));
    }
    Ok(raw_target.0 - offset.0)
}

/// Refuse unless the configured UPS explicitly reports utility power
/// (`OL`); also refuse on battery, in any critical state, or when the
/// UPS is unreachable.
///
/// Fail-closed: query failure and an empty `ups.status` both refuse the
/// mutation. The refusal wording always points operators at `braid ups
/// status` because the safety decision needs explicit utility-power
/// proof (`OL`), not merely the absence of a known blocker. Caller
/// passes `None` when no UPS is configured, which makes this a no-op.
///
/// Severity classification is shared with the TUI and the human UPS status
/// render via `UpsSeverity`, so every surface uses the same ordering: critical
/// tokens block first, `OB` blocks next, `OL` passes, and everything else
/// remains untrusted.
///
/// Wire into `add`, `remove`, `remove-missing`, `replace` before journal
/// write. See docs/design/decisions/020-ups-integration.md for the safety
/// rationale.
pub fn check_ups_not_on_battery<R: CommandRunner>(
    runner: &R,
    ups_name: Option<&str>,
    op: &str,
) -> Result<(), String> {
    let Some(name) = ups_name else {
        return Ok(());
    };
    let refuse = |context: &str| {
        Err(format!(
            "cannot verify UPS is on utility power ({context}) -- refusing to start {op}. \
             Check 'braid ups status', restore utility power, then retry."
        ))
    };
    let parsed = match query_ups(runner, name) {
        Ok(q) => q.parsed,
        Err(UpsQueryError::InvocationFailed(_)) => {
            return refuse("upsc invocation failed");
        }
        Err(UpsQueryError::QueryFailed { stderr, .. }) => {
            return refuse(&format!("upsc query failed{}", detail_suffix(&stderr)));
        }
    };
    if parsed.status_flags.is_empty() {
        return refuse("ups.status is empty or missing");
    }
    match parsed.severity() {
        UpsSeverity::Online => Ok(()),
        UpsSeverity::Critical => {
            refuse("UPS reports a critical state (LB / TESTFAIL / COMMBAD / FSD)")
        }
        UpsSeverity::OnBattery => refuse("UPS reports on-battery"),
        UpsSeverity::Indeterminate => refuse("UPS does not report utility power (OL missing)"),
    }
}

/// Guard for mutating pool commands (add, remove, remove-missing, replace).
///
/// Returns accumulated soft-success notes the caller surfaces as
/// `PreviewNote` entries (dry-run stdout via `Preview::render`, real-run
/// stderr via `preview::render_notes_for_stderr`, failure-path stderr
/// via `cmd_*`'s `report.notes` rendering). Never writes to stderr
/// itself. Hard failures (paused balance, mounted read-only) return
/// `Err(String)` suitable for wrapping in a command's `Validation`
/// error variant.
///
/// `Ok(notes)`: the vec may be empty (clean preflight) or carry one
/// `Info` (busy-op enqueued) and/or one `Warn` (read-only probe
/// degraded), in that insertion order.
pub fn require_mutation_preflight<F: Filesystem + ?Sized>(
    fs: &F,
    fsid: &Fsid,
    mount_point: &MountPoint,
) -> Result<Vec<PreviewNote>, String> {
    let mut notes: Vec<PreviewNote> = Vec::new();
    if let Some(op) =
        check_exclusive_op_with_policy(fs, fsid, ExclusiveOpPolicy::RejectPausedBalanceElseEnqueue)?
    {
        notes.push(PreviewNote::Info(format!(
            "waiting for in-flight {op} to finish..."
        )));
    }
    if let Some(probe_err) = check_not_read_only(fs, mount_point)? {
        notes.push(PreviewNote::Warn(format!(
            "read-only pre-flight failed: {probe_err}; proceeding anyway"
        )));
    }
    Ok(notes)
}

/// Guard for `braid lock` (teardown: unmount + close LUKS).
///
/// Hard-fails on any active exclusive op. Lock must not proceed while btrfs
/// is mid balance/device-add/device-remove/device-replace/resize.
///
/// Returns `Err(String)` suitable for wrapping in `LockError::Failed`.
pub fn require_lock_preflight<F: Filesystem + ?Sized>(fs: &F, fsid: &Fsid) -> Result<(), String> {
    check_exclusive_op_with_policy(fs, fsid, ExclusiveOpPolicy::RejectAnyBusy).map(|_| ())
}

/// Tell the systemd-stop executor whether a running balance needs an
/// explicit pause request before unmount.
///
/// This is the sole systemd-stop exclusive-op preflight gate; the pause bool
/// is a side product of the single sysfs read, so there is no parallel
/// `require_*` guard to keep in sync.
///
/// Returns false for idle and already-paused balance states; rejects every
/// non-balance exclusive operation with the same message as lock preflight.
pub fn systemd_stop_lock_requires_balance_pause<F: Filesystem + ?Sized>(
    fs: &F,
    fsid: &Fsid,
) -> Result<bool, String> {
    check_exclusive_op_with_policy(fs, fsid, ExclusiveOpPolicy::AllowBalanceElseReject)
        .map(|op| matches!(op, Some(ExclusiveOp::Balance)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::btrfs_ioctl::tests_support::MockBtrfsDevInfo;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};
    use crate::probe::Filesystem;

    struct MockFs {
        files: std::collections::HashMap<String, String>,
        mountinfo: Option<Result<String, std::io::ErrorKind>>,
    }

    impl MockFs {
        fn with_sysfs(fsid: &str, content: &str) -> Self {
            Self::empty().with_sysfs_entry(fsid, content)
        }

        fn with_sysfs_entry(mut self, fsid: &str, content: &str) -> Self {
            self.files.insert(
                format!("/sys/fs/btrfs/{fsid}/exclusive_operation"),
                content.to_owned(),
            );
            self
        }

        fn empty() -> Self {
            Self {
                files: std::collections::HashMap::new(),
                mountinfo: None,
            }
        }

        fn with_mountinfo(mut self, body: &str) -> Self {
            self.mountinfo = Some(Ok(body.to_owned()));
            self
        }

        fn with_mountinfo_error(mut self, kind: std::io::ErrorKind) -> Self {
            self.mountinfo = Some(Err(kind));
            self
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, _path: &str) -> bool {
            false
        }
        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path == "/proc/self/mountinfo" {
                return match &self.mountinfo {
                    Some(Ok(body)) => Ok(body.clone()),
                    Some(Err(kind)) => Err(std::io::Error::new(*kind, "mock mountinfo error")),
                    None => Err(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "mock mountinfo not seeded",
                    )),
                };
            }
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }
        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
        fn create_dir_all(&self, _path: &str) -> Result<(), std::io::Error> {
            unreachable!(
                "preflight::MockFs: read-only fixture; create_dir_all must never be called"
            )
        }
    }

    const FSID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";

    /// Typed `FSID` for the scoped preflight guards, which now take `&Fsid`.
    /// The `&str` const stays for `with_sysfs`, which keys the mock sysfs path
    /// by raw string (the leaf reader's contract), so both spellings coexist.
    fn fsid() -> Fsid {
        Fsid::parse(FSID).unwrap()
    }
    const TARGET: &str = "/dev/disk/by-id/virtio-disk3";
    const SOURCE_TOTAL: u64 = 520_093_696;
    const TARGET_RAW_512_MIB: u64 = 536_870_912;

    fn dev_info_with_total(total_bytes: u64) -> MockBtrfsDevInfo {
        MockBtrfsDevInfo::default().with_total_bytes("/mnt/storage", Devid::new(2), total_bytes)
    }

    fn runner_with_target_size(size: u64) -> MockRunner {
        MockRunner::default().with_output(
            CmdRequest::LsblkDeviceJson {
                device: TARGET.into(),
            },
            crate::test_fixtures::lsblk_device_json_output(None, None, Some(size)),
        )
    }

    fn runner_with_target_size_and_luks_dump(
        size: u64,
        offset: u64,
        segment_size: &str,
        sector_size: u64,
    ) -> MockRunner {
        runner_with_target_size(size).with_output(
            CmdRequest::CryptsetupLuksDump {
                device: TARGET.into(),
            },
            RawCommandOutput {
                cmd: "cryptsetup luksDump --dump-json-metadata".into(),
                stdout: luks_dump_json(offset, segment_size, sector_size),
                stderr: String::new(),
                exit_status: 0,
            },
        )
    }

    fn luks_dump_json(offset: u64, segment_size: &str, sector_size: u64) -> String {
        format!(
            r#"{{
  "keyslots": {{}},
  "tokens": {{}},
  "segments": {{
    "0": {{
      "type": "crypt",
      "offset": "{offset}",
      "size": "{segment_size}",
      "iv_tweak": "0",
      "encryption": "aes-xts-plain64",
      "sector_size": {sector_size}
    }}
  }},
  "digests": {{}},
  "config": {{}}
}}"#
        )
    }

    fn source_probe() -> ReplaceSourceProbe {
        ReplaceSourceProbe {
            devid: Devid::new(2),
        }
    }

    #[test]
    // Intent: fresh replacement targets are refused when the default LUKS2
    //   data offset leaves less mapper capacity than the source device.
    // Why it exists: protects against formatting an undersized disk before
    //   btrfs's later replace-time size check rejects it.
    // Scenario: 512 MiB source, 256 MiB raw replacement.
    fn check_replace_target_capacity_fresh_refuses_when_target_smaller() {
        let runner = runner_with_target_size(268_435_456);
        let dev_info = dev_info_with_total(SOURCE_TOTAL);
        let err = check_replace_target_capacity(
            &runner,
            &dev_info,
            Path::new("/mnt/storage"),
            source_probe(),
            ReplaceTargetProbe::PresentNotLuks { by_id: TARGET },
        )
        .unwrap_err();
        assert!(
            err.contains("smaller than the disk being replaced"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Intent: fresh replacement targets pass when modeled mapper capacity is
    //   equal to or larger than the source device.
    // Why it exists: confirms the check mirrors btrfs's strict
    //   `source > target` refusal instead of requiring extra headroom.
    // Scenario: 512 MiB raw target after the 16 MiB LUKS2 default offset
    //   equals the source btrfs size, and a larger raw target also passes.
    fn check_replace_target_capacity_fresh_accepts_equal_and_larger() {
        for raw_size in [TARGET_RAW_512_MIB, TARGET_RAW_512_MIB + 1] {
            let runner = runner_with_target_size(raw_size);
            let dev_info = dev_info_with_total(SOURCE_TOTAL);
            check_replace_target_capacity(
                &runner,
                &dev_info,
                Path::new("/mnt/storage"),
                source_probe(),
                ReplaceTargetProbe::PresentNotLuks { by_id: TARGET },
            )
            .expect("fresh target with sufficient modeled capacity should pass");
        }
    }

    #[test]
    // Intent: existing LUKS targets with dynamic segment size derive capacity
    //   as raw device size minus segment offset.
    // Why it exists: the preflight must match the mapper size btrfs will see
    //   after opening the LUKS container.
    // Scenario: target has the default dynamic segment at 16 MiB offset.
    fn check_replace_target_capacity_existing_dynamic_segment() {
        let runner =
            runner_with_target_size_and_luks_dump(TARGET_RAW_512_MIB, 16_777_216, "dynamic", 512);
        let dev_info = dev_info_with_total(SOURCE_TOTAL);
        check_replace_target_capacity(
            &runner,
            &dev_info,
            Path::new("/mnt/storage"),
            source_probe(),
            ReplaceTargetProbe::PresentLuks { by_id: TARGET },
        )
        .expect("dynamic segment with sufficient capacity should pass");
    }

    #[test]
    // Intent: existing dynamic LUKS target capacity is not rounded down to
    //   the reported segment sector_size.
    // Why it exists: guards the no-rounding invariant for externally
    //   formatted 4Kn targets; whole-MiB fixtures cannot catch a round-down
    //   regression because they are already 4096-aligned.
    // Scenario: the target's raw size leaves exactly 4608 bytes after the
    //   16 MiB segment offset, which is enough for btrfs but would be refused
    //   if rounded down to the reported 4096-byte sector_size.
    fn check_replace_target_capacity_existing_dynamic_segment_does_not_round_sector_size() {
        let offset = 16_777_216;
        let source_total = 4_608;
        let runner =
            runner_with_target_size_and_luks_dump(offset + source_total, offset, "dynamic", 4096);
        let dev_info = dev_info_with_total(source_total);

        check_replace_target_capacity(
            &runner,
            &dev_info,
            Path::new("/mnt/storage"),
            source_probe(),
            ReplaceTargetProbe::PresentLuks { by_id: TARGET },
        )
        .expect("dynamic segment capacity should not be rounded down to sector_size");
    }

    #[test]
    // Intent: existing LUKS targets with fixed segment size use that fixed
    //   size directly as mapper capacity.
    // Why it exists: LUKS2 reencrypt states can report fixed segment sizes,
    //   where `raw - offset` is not the mapper size btrfs will compare.
    // Scenario: synthetic LUKS2 metadata reports a fixed 520093696-byte
    //   segment on the replacement disk.
    fn check_replace_target_capacity_existing_fixed_segment() {
        let runner =
            runner_with_target_size_and_luks_dump(TARGET_RAW_512_MIB, 16_777_216, "520093696", 512);
        let dev_info = dev_info_with_total(SOURCE_TOTAL);
        check_replace_target_capacity(
            &runner,
            &dev_info,
            Path::new("/mnt/storage"),
            source_probe(),
            ReplaceTargetProbe::PresentLuks { by_id: TARGET },
        )
        .expect("fixed segment with sufficient capacity should pass");
    }

    #[test]
    // Intent: missing or failing btrfs device-info lookup is a hard refusal.
    // Why it exists: replace cannot safely proceed when the source-size
    //   authority btrfs uses is unavailable.
    // Scenario: mock btrfs device-info has no row for the source devid.
    fn check_replace_target_capacity_refuses_when_dev_info_errors() {
        let runner = MockRunner::default();
        let dev_info = MockBtrfsDevInfo::default();
        let err = check_replace_target_capacity(
            &runner,
            &dev_info,
            Path::new("/mnt/storage"),
            source_probe(),
            ReplaceTargetProbe::PresentNotLuks { by_id: TARGET },
        )
        .unwrap_err();
        assert!(
            err.contains("failed to read btrfs total_bytes"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Intent: btrfs device-info returning `total_bytes == 0` is refused.
    // Why it exists: zero cannot prove a replacement is large enough and
    //   usually means the source-size authority is not usable.
    // Scenario: ioctl boundary returns a zero size for the source devid.
    fn check_replace_target_capacity_refuses_when_total_bytes_zero() {
        let runner = MockRunner::default();
        let dev_info = dev_info_with_total(0);
        let err = check_replace_target_capacity(
            &runner,
            &dev_info,
            Path::new("/mnt/storage"),
            source_probe(),
            ReplaceTargetProbe::PresentNotLuks { by_id: TARGET },
        )
        .unwrap_err();
        assert!(err.contains("total_bytes 0"), "unexpected error: {err}");
    }

    #[test]
    // Intent: an existing LUKS target whose JSON dump command fails is refused.
    // Why it exists: braid cannot model mapper capacity without trustworthy
    //   LUKS2 segment metadata.
    // Scenario: cryptsetup reports a non-zero exit for the target header.
    fn check_replace_target_capacity_refuses_when_luks_dump_fails() {
        let runner = runner_with_target_size(TARGET_RAW_512_MIB).with_output(
            CmdRequest::CryptsetupLuksDump {
                device: TARGET.into(),
            },
            RawCommandOutput {
                cmd: "cryptsetup luksDump --dump-json-metadata".into(),
                stdout: String::new(),
                stderr: "metadata read failed".into(),
                exit_status: 5,
            },
        );
        let dev_info = dev_info_with_total(SOURCE_TOTAL);
        let err = check_replace_target_capacity(
            &runner,
            &dev_info,
            Path::new("/mnt/storage"),
            source_probe(),
            ReplaceTargetProbe::PresentLuks { by_id: TARGET },
        )
        .unwrap_err();
        assert!(
            err.contains("failed to parse LUKS2 segment metadata"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Intent: missing structured lsblk size for the target is refused.
    // Why it exists: target capacity cannot be checked if raw disk size is
    //   unknown, and this branch precedes destructive format.
    // Scenario: the runner has no size output for the replacement by-id path.
    fn check_replace_target_capacity_refuses_when_lsblk_none() {
        let runner = MockRunner::default();
        let dev_info = dev_info_with_total(SOURCE_TOTAL);
        let err = check_replace_target_capacity(
            &runner,
            &dev_info,
            Path::new("/mnt/storage"),
            source_probe(),
            ReplaceTargetProbe::PresentNotLuks { by_id: TARGET },
        )
        .unwrap_err();
        assert!(
            err.contains("failed to read raw size"),
            "unexpected error: {err}"
        );
    }

    // Intent: an explicit nullable SIZE cannot satisfy replacement preflight.
    // Why it exists: structured lsblk distinguishes a present-but-null column
    //   from command failure, but neither proves the target capacity.
    // Scenario: lsblk returns a valid device row whose SIZE value is null.
    #[test]
    fn check_replace_target_capacity_refuses_when_lsblk_size_is_null() {
        let runner = MockRunner::default().with_output(
            CmdRequest::LsblkDeviceJson {
                device: TARGET.into(),
            },
            crate::test_fixtures::lsblk_device_json_output(None, None, None),
        );
        let dev_info = dev_info_with_total(SOURCE_TOTAL);

        let err = check_replace_target_capacity(
            &runner,
            &dev_info,
            Path::new("/mnt/storage"),
            source_probe(),
            ReplaceTargetProbe::PresentNotLuks { by_id: TARGET },
        )
        .unwrap_err();

        assert!(
            err.contains("failed to read raw size"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Intent: dynamic-segment capacity refuses when raw size is not larger
    //   than the segment offset.
    // Why it exists: subtracting the offset would underflow or yield no
    //   usable mapper capacity.
    // Scenario: target LUKS metadata reports a 100-byte offset on a 100-byte disk.
    fn check_replace_target_capacity_refuses_when_raw_below_offset() {
        let runner = runner_with_target_size_and_luks_dump(100, 100, "dynamic", 512);
        let dev_info = dev_info_with_total(SOURCE_TOTAL);
        let err = check_replace_target_capacity(
            &runner,
            &dev_info,
            Path::new("/mnt/storage"),
            source_probe(),
            ReplaceTargetProbe::PresentLuks { by_id: TARGET },
        )
        .unwrap_err();
        assert!(
            err.contains("not larger than LUKS2 segment offset"),
            "unexpected error: {err}"
        );
    }

    #[test]
    // Intent: fixed-size LUKS2 segment metadata with size 0 is refused.
    // Why it exists: a zero-capacity fixed segment is malformed and cannot
    //   prove the target is large enough.
    // Scenario: synthetic segment metadata reports `"size":"0"`.
    fn check_replace_target_capacity_refuses_when_fixed_size_zero() {
        let runner =
            runner_with_target_size_and_luks_dump(TARGET_RAW_512_MIB, 16_777_216, "0", 512);
        let dev_info = dev_info_with_total(SOURCE_TOTAL);
        let err = check_replace_target_capacity(
            &runner,
            &dev_info,
            Path::new("/mnt/storage"),
            source_probe(),
            ReplaceTargetProbe::PresentLuks { by_id: TARGET },
        )
        .unwrap_err();
        assert!(err.contains("fixed size 0"), "unexpected error: {err}");
    }

    // --- ExclusiveOp::parse tests ---

    #[test]
    // Intent: ExclusiveOp::parse maps every sysfs string from the kernel
    //   exclop set to the right outcome -- `"none"` -> Ok(None) (idle), each busy
    //   string -> Ok(Some(variant)).
    // Why: Pins the type-level split between idle and busy. If a kernel
    //   string is added or renamed, this catches it before the busy paths
    //   silently misclassify.
    // Scenario: Kernel writes each possible exclusive_operation value.
    fn exclusive_op_parse_all_variants() {
        assert_eq!(ExclusiveOp::parse("none"), Ok(None));
        assert_eq!(
            ExclusiveOp::parse("balance"),
            Ok(Some(ExclusiveOp::Balance))
        );
        assert_eq!(
            ExclusiveOp::parse("balance paused"),
            Ok(Some(ExclusiveOp::BalancePaused))
        );
        assert_eq!(
            ExclusiveOp::parse("device add"),
            Ok(Some(ExclusiveOp::DeviceAdd))
        );
        assert_eq!(
            ExclusiveOp::parse("device remove"),
            Ok(Some(ExclusiveOp::DeviceRemove))
        );
        assert_eq!(
            ExclusiveOp::parse("device replace"),
            Ok(Some(ExclusiveOp::DeviceReplace))
        );
        assert_eq!(ExclusiveOp::parse("resize"), Ok(Some(ExclusiveOp::Resize)));
        assert_eq!(
            ExclusiveOp::parse("swap activate"),
            Ok(Some(ExclusiveOp::SwapActivate))
        );
    }

    #[test]
    // Intent: ExclusiveOp::parse returns Err(s) carrying the unrecognized
    //   input for any value outside the kernel exclop set.
    // Why: Future kernel versions may add new op types; fail-closed is
    //   safer. Carrying the offending string lets callers surface it via
    //   `ExclusiveOpError::Unrecognized`.
    // Scenario: Kernel writes a value `btrfs_exclusive_operation_show` would not emit.
    fn exclusive_op_parse_unrecognized() {
        assert_eq!(
            ExclusiveOp::parse("something new"),
            Err("something new".to_string())
        );
        assert_eq!(ExclusiveOp::parse(""), Err(String::new()));
    }

    #[test]
    // Intent: ExclusiveOp Display produces human-readable strings.
    // Why: These strings appear in user-facing "waiting for..." messages.
    // Scenario: Each op variant is formatted for display.
    fn exclusive_op_display() {
        assert_eq!(format!("{}", ExclusiveOp::Balance), "balance");
        assert_eq!(
            format!("{}", ExclusiveOp::BalancePaused),
            "balance (paused)"
        );
        assert_eq!(format!("{}", ExclusiveOp::DeviceRemove), "device remove");
    }

    fn mountinfo_for_target(vfs_options: &str, fs_options: &str) -> String {
        format!(
            "36 35 0:32 / /mnt/storage {vfs_options} shared:1 - btrfs /dev/mapper/braid-vdb {fs_options}\n"
        )
    }

    fn mountinfo_rw() -> String {
        mountinfo_for_target("rw,relatime", "rw,space_cache=v2")
    }

    fn mountinfo_without_target() -> String {
        "26 25 0:23 / / rw,noatime shared:1 - ext4 /dev/sda1 rw\n".to_string()
    }

    fn malformed_mountinfo() -> String {
        "36 35 0:32 / /mnt/storage rw,relatime shared:1 no_dash_separator\n".to_string()
    }

    fn duplicate_mountinfo() -> String {
        format!("{}{}", mountinfo_rw(), mountinfo_rw())
    }

    #[test]
    // Intent: check_not_read_only returns Ok(None) when both mountinfo option
    //   fields are rw.
    // Why: Confirms writable mounts are not falsely rejected.
    // Scenario: Normal pool mount with rw VFS and rw filesystem state.
    fn read_only_passes_when_both_vfs_and_fs_options_are_rw() {
        let fs = MockFs::empty().with_mountinfo(&mountinfo_rw());
        let out = check_not_read_only(&fs, &mp()).unwrap();
        assert!(out.is_none(), "expected Ok(None) on rw mount, got {out:?}");
    }

    #[test]
    // Intent: check_not_read_only refuses when the VFS mount options contain ro.
    // Why: operator-issued `mount -o remount,ro` lands in mountinfo field 6.
    // Scenario: pool is remounted read-only at the VFS layer.
    fn read_only_refuses_when_vfs_options_ro() {
        let fs = MockFs::empty()
            .with_mountinfo(&mountinfo_for_target("ro,relatime", "rw,space_cache=v2"));
        let err = check_not_read_only(&fs, &mp()).unwrap_err();
        assert!(err.contains("read-only"), "expected 'read-only' in: {err}");
        assert!(
            err.contains("remount"),
            "expected remount guidance in: {err}"
        );
    }

    #[test]
    // Intent: check_not_read_only refuses when filesystem options contain ro.
    // Why: btrfs can auto-remount the superblock read-only after I/O errors;
    //   that state lands in mountinfo field 11, not necessarily field 6.
    // Scenario: pool degraded to filesystem-level read-only after errors.
    fn read_only_refuses_when_fs_options_ro() {
        let fs = MockFs::empty()
            .with_mountinfo(&mountinfo_for_target("rw,relatime", "ro,space_cache=v2"));
        let err = check_not_read_only(&fs, &mp()).unwrap_err();
        assert!(err.contains("read-only"), "expected 'read-only' in: {err}");
        assert!(
            err.contains("remount"),
            "expected remount guidance in: {err}"
        );
    }

    #[test]
    // Intent: check_not_read_only refuses when both option fields contain ro.
    // Why: the two-field check should be symmetric.
    // Scenario: both VFS and filesystem state are read-only.
    fn read_only_refuses_when_both_fields_ro() {
        let fs = MockFs::empty()
            .with_mountinfo(&mountinfo_for_target("ro,relatime", "ro,space_cache=v2"));
        let err = check_not_read_only(&fs, &mp()).unwrap_err();
        assert!(err.contains("read-only"), "expected 'read-only' in: {err}");
    }

    #[test]
    // Intent: check_not_read_only surfaces mountinfo IO failures via Ok(Some(_)).
    // Why: caller must emit a Warn note when the best-effort ro probe cannot run.
    // Scenario: /proc/self/mountinfo cannot be read.
    fn read_only_returns_probe_error_body_on_io_failure() {
        let fs = MockFs::empty().with_mountinfo_error(std::io::ErrorKind::PermissionDenied);
        let body = check_not_read_only(&fs, &mp())
            .unwrap()
            .expect("expected Ok(Some(_)) with probe-failure body");
        assert!(!body.is_empty(), "probe-failure body must not be empty");
    }

    #[test]
    // Intent: check_not_read_only surfaces malformed mountinfo via Ok(Some(_)).
    // Why: malformed mountinfo means the ro guard could not make a safe call.
    // Scenario: target line is missing the dash separator.
    fn read_only_returns_probe_error_body_on_malformed_line() {
        let fs = MockFs::empty().with_mountinfo(&malformed_mountinfo());
        let body = check_not_read_only(&fs, &mp())
            .unwrap()
            .expect("expected Ok(Some(_)) with probe-failure body");
        assert!(!body.is_empty(), "probe-failure body must not be empty");
    }

    #[test]
    // Intent: check_not_read_only surfaces duplicate target entries via Ok(Some(_)).
    // Why: overmount ambiguity must not be silently treated as writable.
    // Scenario: two mountinfo entries report the configured mount point.
    fn read_only_returns_probe_error_body_on_duplicate_target() {
        let fs = MockFs::empty().with_mountinfo(&duplicate_mountinfo());
        let body = check_not_read_only(&fs, &mp())
            .unwrap()
            .expect("expected Ok(Some(_)) with probe-failure body");
        assert!(!body.is_empty(), "probe-failure body must not be empty");
    }

    #[test]
    // Intent: check_not_read_only surfaces an absent target via Ok(Some(_)).
    // Why: after mounted-pool preflight passes, a missing mountinfo entry is
    //   a race or anomaly worth warning about, not proof of writability.
    // Scenario: mountinfo is well-formed but lacks /mnt/storage.
    fn read_only_returns_probe_error_body_when_target_absent() {
        let fs = MockFs::empty().with_mountinfo(&mountinfo_without_target());
        let body = check_not_read_only(&fs, &mp())
            .unwrap()
            .expect("expected Ok(Some(_)) with probe-failure body");
        assert!(
            body.contains("not present"),
            "expected missing-target body, got: {body}"
        );
    }

    #[test]
    // Intent: a field containing exactly the single ro token is refused.
    // Why: exact-token matching must handle one-token option strings too.
    // Scenario: mountinfo field is simply "ro".
    fn read_only_options_with_only_ro_token_is_refused() {
        let fs = MockFs::empty().with_mountinfo(&mountinfo_for_target("rw,relatime", "ro"));
        let err = check_not_read_only(&fs, &mp()).unwrap_err();
        assert!(err.contains("read-only"), "expected 'read-only' in: {err}");
    }

    #[test]
    // Intent: ro substrings inside other option values do not match.
    // Why: exact-token matching avoids false positives like errors=remount-ro.
    // Scenario: VFS options mention remount-ro but include no standalone ro.
    fn read_only_options_containing_ro_substring_does_not_match() {
        let fs = MockFs::empty().with_mountinfo(&mountinfo_for_target(
            "errors=remount-ro,rw",
            "rw,space_cache=v2",
        ));
        let out = check_not_read_only(&fs, &mp()).unwrap();
        assert!(out.is_none(), "expected Ok(None), got {out:?}");
    }

    #[test]
    // Intent: check_no_missing_devices passes when no devices are missing.
    // Why: Confirms healthy pools are not rejected.
    // Scenario: Normal 3-disk pool, all present.
    fn missing_devices_passes_when_none() {
        assert!(check_no_missing_devices(0, "remove a disk").is_ok());
    }

    #[test]
    // Intent: check_no_missing_devices refuses when devices are missing.
    // Why: Removing a live disk from a degraded pool is dangerous.
    // Scenario: One disk has died, operator tries to remove a different live disk.
    fn missing_devices_refuses_when_degraded() {
        let err = check_no_missing_devices(2, "remove a disk").unwrap_err();
        assert!(
            err.contains("2 missing devices"),
            "expected count in: {err}"
        );
        assert!(
            err.contains(
                "braid replace --old <missing-name> --new <new-name>=/dev/disk/by-id/<...>"
            ),
            "expected repair guidance in: {err}"
        );
        assert!(
            !err.contains("replace --missing-id"),
            "repair guidance must not request --missing-id: {err}"
        );
        assert!(
            err.contains("remove-missing"),
            "expected cleanup guidance in: {err}"
        );
    }

    #[test]
    // Intent: check_no_missing_devices uses singular for 1 device.
    // Why: Grammar correctness in user-facing messages.
    // Scenario: Pool has exactly 1 missing device.
    fn missing_devices_singular_grammar() {
        let err = check_no_missing_devices(1, "remove a disk").unwrap_err();
        assert!(
            err.contains("1 missing device."),
            "expected singular in: {err}"
        );
    }

    // --- check_raid1_relocation_space tests ---

    use crate::parse::types::DeviceAllocation;

    fn make_dev(devid: Devid, unallocated: u64, allocs: &[(&str, u64)]) -> BtrfsDeviceUsageEntry {
        BtrfsDeviceUsageEntry {
            path: format!("/dev/mapper/braid-disk{}", devid.get()),
            devid,
            device_size: 1_000_000_000,
            device_slack: 0,
            allocations: allocs
                .iter()
                .map(|(t, b)| DeviceAllocation {
                    alloc_type: t.to_string(),
                    profile: "RAID1".to_string(),
                    bytes: *b,
                })
                .collect(),
            unallocated,
        }
    }

    #[test]
    // Intent: check_raid1_relocation_space passes when 3 remaining devices have
    //   enough space for target's Data and Metadata allocations.
    // Why: Confirms valid operations are not blocked.
    // Scenario: 4-disk pool removing one disk; remaining three each have 200MB
    //   unallocated; target has 100MB Data + 50MB Metadata.
    fn raid1_space_passes_sufficient_space() {
        let target = make_dev(
            Devid::new(1),
            0,
            &[("Data", 100_000_000), ("Metadata", 50_000_000)],
        );
        let rem1 = make_dev(Devid::new(2), 200_000_000, &[]);
        let rem2 = make_dev(Devid::new(3), 200_000_000, &[]);
        let rem3 = make_dev(Devid::new(4), 200_000_000, &[]);
        let result = check_raid1_relocation_space(&[&target], &[&rem1, &rem2, &rem3]);
        assert!(result.is_ok(), "should pass: {result:?}");
    }

    #[test]
    // Intent: check_raid1_relocation_space fails when RAID1 chunk-level capacity
    //   is insufficient despite large total unallocated.
    // Why: The naive sum/2 can be misleading when one device dominates —
    //   each RAID1 chunk needs 2 devices, so the dominant device is
    //   bottlenecked by what others can provide.
    // Scenario: 3 remaining devices with [200MB, 10MB, 10MB] unallocated.
    //   RAID1 capacity = rest = 20MB (not 110MB). Target has 500MB Data.
    fn raid1_space_fails_chunk_capacity_constraint() {
        let target = make_dev(Devid::new(1), 0, &[("Data", 500_000_000)]);
        let rem1 = make_dev(Devid::new(2), 200_000_000, &[]);
        let rem2 = make_dev(Devid::new(3), 10_000_000, &[]);
        let rem3 = make_dev(Devid::new(4), 10_000_000, &[]);
        let result = check_raid1_relocation_space(&[&target], &[&rem1, &rem2, &rem3]);
        let err = result.expect_err("should fail: chunk capacity constraint");
        assert!(err.contains("Data"), "expected 'Data' in error: {err}");
    }

    #[test]
    // Intent: check_raid1_relocation_space fails when fewer than 2 remaining
    //   devices have unallocated space.
    // Why: RAID1 requires 2 devices with capacity; 1 device cannot form a RAID1 chunk.
    // Scenario: Target has 100MB Data; remaining has 200MB + 0MB unallocated.
    fn raid1_space_fails_fewer_than_two_devices_with_space() {
        let target = make_dev(Devid::new(1), 0, &[("Data", 100_000_000)]);
        let rem1 = make_dev(Devid::new(2), 200_000_000, &[]);
        let rem2 = make_dev(Devid::new(3), 0, &[]);
        let result = check_raid1_relocation_space(&[&target], &[&rem1, &rem2]);
        let err = result.expect_err("should fail: fewer than 2 devices with space");
        assert!(
            err.contains("fewer than 2"),
            "expected 'fewer than 2' in error: {err}"
        );
    }

    #[test]
    // Intent: check_raid1_relocation_space skips types with zero allocations on target.
    // Why: Types not present on target don't need relocation; checking them would
    //   cause false negatives against an empty remaining device list.
    // Scenario: Target has 0 Data but 40MB Metadata; remaining have 50MB each.
    //   Data is skipped (0 allocated). Metadata RAID1 capacity = 50MB > 40MB → pass.
    fn raid1_space_skips_zero_allocation_type() {
        let target = make_dev(Devid::new(1), 0, &[("Data", 0), ("Metadata", 40_000_000)]);
        let rem1 = make_dev(Devid::new(2), 50_000_000, &[]);
        let rem2 = make_dev(Devid::new(3), 50_000_000, &[]);
        let result = check_raid1_relocation_space(&[&target], &[&rem1, &rem2]);
        assert!(
            result.is_ok(),
            "should pass (Data skipped, Metadata fits): {result:?}"
        );
    }

    #[test]
    // Intent: check_raid1_relocation_space fails on the per-type that is tight,
    //   even when other types have plenty of space.
    // Why: DATA and METADATA are independent allocation pools in the kernel.
    //   Surplus Data space cannot cover Metadata relocation.
    // Scenario: Target has 0 Data but 100MB Metadata; remaining have 40MB each.
    //   Metadata RAID1 capacity = 40MB < 100MB → fail.
    fn raid1_space_fails_tight_metadata_despite_data_ok() {
        let target = make_dev(Devid::new(1), 0, &[("Metadata", 100_000_000)]);
        let rem1 = make_dev(Devid::new(2), 40_000_000, &[]);
        let rem2 = make_dev(Devid::new(3), 40_000_000, &[]);
        let result = check_raid1_relocation_space(&[&target], &[&rem1, &rem2]);
        let err = result.expect_err("should fail: Metadata tight");
        assert!(
            err.contains("Metadata"),
            "expected 'Metadata' in error: {err}"
        );
    }

    #[test]
    // Intent: check_raid1_relocation_space handles 4 remaining devices with
    //   RAID1 capacity correctly using total/2 (no dominant device).
    // Why: When no single device dominates, capacity = total/2 is the correct formula.
    // Scenario: 5-disk pool, target has 1GB Data; remaining [500MB, 400MB, 300MB] unallocated.
    //   total=1200MB, largest=500MB, rest=700MB → 500 <= 700 → capacity=600MB < 1000MB → fail.
    fn raid1_space_fails_4devs_insufficient_total() {
        let target = make_dev(Devid::new(1), 0, &[("Data", 1_000_000_000)]);
        let rem1 = make_dev(Devid::new(2), 500_000_000, &[]);
        let rem2 = make_dev(Devid::new(3), 400_000_000, &[]);
        let rem3 = make_dev(Devid::new(4), 300_000_000, &[]);
        let result = check_raid1_relocation_space(&[&target], &[&rem1, &rem2, &rem3]);
        let err = result.expect_err("should fail: total/2 < bytes_on_target");
        assert!(err.contains("Data"), "expected 'Data' in error: {err}");
    }

    // --- check_single_survivor_capacity tests ---

    use crate::parse::types::{BtrfsDfEntry, BtrfsProfile};

    fn make_df(entries: &[(BtrfsBgType, u64)]) -> BtrfsDfOutput {
        BtrfsDfOutput {
            entries: entries
                .iter()
                .map(|(t, used)| BtrfsDfEntry {
                    bg_type: *t,
                    bg_profile: BtrfsProfile::Raid1,
                    bg_used: *used,
                    bg_total: *used,
                })
                .collect(),
        }
    }

    fn make_survivor(device_size: u64, device_slack: u64) -> BtrfsDeviceUsageEntry {
        BtrfsDeviceUsageEntry {
            path: "/dev/mapper/braid-disk2".to_string(),
            devid: Devid::new(2),
            device_size,
            device_slack,
            allocations: vec![],
            unallocated: 0,
        }
    }

    #[test]
    // Intent: check_single_survivor_capacity passes when data + 2*meta + 2*sys
    //   fits comfortably within the survivor's device_size - device_slack.
    // Why: Common healthy pool: a 1 GiB survivor can absorb a lightly-used pool.
    // Scenario: 1 GiB survivor (no slack); 200 MiB Data, 10 MiB Metadata,
    //   4 KiB System. needed = 200 + 20 + ~0 = 220 MiB << 1024 MiB.
    fn survivor_fits_passes() {
        let df = make_df(&[
            (BtrfsBgType::Data, 200 * 1024 * 1024),
            (BtrfsBgType::Metadata, 10 * 1024 * 1024),
            (BtrfsBgType::System, 4 * 1024),
        ]);
        let survivor = make_survivor(1024 * 1024 * 1024, 0);
        assert!(check_single_survivor_capacity(&df, &survivor).is_ok());
    }

    #[test]
    // Intent: check_single_survivor_capacity fails when the data alone already
    //   exceeds the survivor's usable capacity.
    // Why: This is the obvious sad path — the balance would ENOSPC on Data.
    // Scenario: 512 MiB survivor; Data.used = 600 MiB.
    fn survivor_undersized_fails() {
        let df = make_df(&[(BtrfsBgType::Data, 600 * 1024 * 1024)]);
        let survivor = make_survivor(512 * 1024 * 1024, 0);
        let err = check_single_survivor_capacity(&df, &survivor)
            .expect_err("should fail: data > survivor");
        assert!(
            err.contains("not enough space on surviving device"),
            "wrong error: {err}"
        );
    }

    #[test]
    // Intent: check_single_survivor_capacity fails when Data alone fits but
    //   2 * Metadata tips the demand past usable.
    // Why: This is the exact bug the 2->1 preflight exists to catch —
    //   post-balance metadata is DUP (2x physical) even when pre-balance
    //   RAID1 hid the overhead.
    // Scenario: 1000 MiB survivor; Data = 700 MiB, Metadata = 200 MiB.
    //   Data alone fits. needed = 700 + 400 = 1100 MiB > 1000 MiB.
    fn metadata_doubling_tips_over() {
        let df = make_df(&[
            (BtrfsBgType::Data, 700 * 1024 * 1024),
            (BtrfsBgType::Metadata, 200 * 1024 * 1024),
        ]);
        let survivor = make_survivor(1000 * 1024 * 1024, 0);
        let err = check_single_survivor_capacity(&df, &survivor)
            .expect_err("should fail: 2 * meta tips over");
        assert!(err.contains("data + 2 * metadata"), "wrong error: {err}");
    }

    #[test]
    // Intent: check_single_survivor_capacity passes on an empty pool.
    // Why: No entries must not crash or false-fail; the helper is called on
    //   every 2->1 remove including against a pool mounted for the first time.
    // Scenario: Empty df, 1 GiB survivor. needed = 0.
    fn empty_pool_passes() {
        let df = make_df(&[]);
        let survivor = make_survivor(1024 * 1024 * 1024, 0);
        assert!(check_single_survivor_capacity(&df, &survivor).is_ok());
    }

    #[test]
    // Intent: check_single_survivor_capacity passes when only metadata/system
    //   is present and 2x fits.
    // Why: Exercises the boundary where Data.used == 0 but Metadata/System
    //   still incur the 2x multiplier -- confirms metadata/system are
    //   counted correctly when data is absent.
    // Scenario: 1 GiB survivor; 200 MiB Metadata, 16 MiB System, 0 Data.
    //   needed = 2 * 200 + 2 * 16 = 432 MiB << 1024 MiB.
    fn metadata_only_passes() {
        let df = make_df(&[
            (BtrfsBgType::Metadata, 200 * 1024 * 1024),
            (BtrfsBgType::System, 16 * 1024 * 1024),
        ]);
        let survivor = make_survivor(1024 * 1024 * 1024, 0);
        assert!(check_single_survivor_capacity(&df, &survivor).is_ok());
    }

    #[test]
    // Intent: check_single_survivor_capacity excludes GlobalReserve from the
    //   demand calculation.
    // Why: GlobalReserve is an internal emergency reservation carved out of
    //   Metadata, not on-disk data that needs to migrate; counting it would
    //   false-fail healthy pools.
    // Scenario: 100 MiB survivor; real Data = 30 MiB, real Metadata = 5 MiB,
    //   GlobalReserve.used = 999 MiB (impossibly big, a forgotten filter
    //   would double it into needed and refuse). Expected: pass.
    fn global_reserve_excluded() {
        let df = make_df(&[
            (BtrfsBgType::Data, 30 * 1024 * 1024),
            (BtrfsBgType::Metadata, 5 * 1024 * 1024),
            (BtrfsBgType::GlobalReserve, 999 * 1024 * 1024),
        ]);
        let survivor = make_survivor(100 * 1024 * 1024, 0);
        assert!(check_single_survivor_capacity(&df, &survivor).is_ok());
    }

    #[test]
    // Intent: check_single_survivor_capacity subtracts device_slack from
    //   device_size when computing usable capacity.
    // Why: device_slack is space the kernel cannot address (alignment
    //   gaps, reserved boundary regions); ignoring it would false-pass on
    //   a pool whose real usable capacity is smaller than device_size.
    // Scenario: 1 GiB device_size + 100 MiB device_slack = 924 MiB usable;
    //   demand = 950 MiB. Expected: fail (950 > 924).
    fn device_slack_reduces_usable() {
        let df = make_df(&[(BtrfsBgType::Data, 950 * 1024 * 1024)]);
        let survivor = make_survivor(1024 * 1024 * 1024, 100 * 1024 * 1024);
        assert!(check_single_survivor_capacity(&df, &survivor).is_err());
    }

    #[test]
    // Intent: check_no_pending_operation passes when no journal exists.
    // Why: Normal operations should not be blocked when there's no interrupted op.
    // Scenario: Fresh state dir, no pending-op.json.
    fn pending_op_passes_when_absent() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        assert!(check_no_pending_operation(&paths).is_ok());
    }

    #[test]
    // Intent: check_no_pending_operation refuses when a journal exists.
    // Why: Operations on suspect membership risk mounting the wrong disks.
    // Scenario: An add was interrupted; pending-op.json exists.
    fn pending_op_refuses_when_present() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let journal = crate::journal::build_journal(
            crate::membership::PoolMembership::empty(),
            crate::membership::PoolMembership::empty(),
            crate::journal::OpKind::Add {
                phase: crate::journal::AddPhase::PoolMutation,
                targets: crate::membership::LuksUuidMap::new(),
            },
        );
        crate::journal::write_journal(&paths, &journal).unwrap();
        let err = check_no_pending_operation(&paths).unwrap_err();
        assert!(
            err.contains("interrupted operation"),
            "expected 'interrupted operation' in: {err}"
        );
    }

    #[test]
    // Intent: check_no_pending_operation refuses on corrupt journal (fail-closed).
    // Why: A corrupt journal is ambiguous — safer to block than proceed.
    // Scenario: pending-op.json exists but contains garbage.
    fn pending_op_refuses_on_corrupt_journal() {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        std::fs::write(paths.pending_op_json(), "not json").unwrap();
        let err = check_no_pending_operation(&paths).unwrap_err();
        assert!(
            err.contains("cannot read"),
            "expected 'cannot read' in: {err}"
        );
    }

    // --- require_lock_preflight tests ---

    #[test]
    // Intent: require_lock_preflight passes when sysfs says "none".
    // Why: Lock teardown should proceed when nothing is running.
    // Scenario: No active exclusive op.
    fn lock_preflight_passes_when_none() {
        let fs = MockFs::with_sysfs(FSID, "none\n");
        assert!(require_lock_preflight(&fs, &fsid()).is_ok());
    }

    #[test]
    // Intent: require_lock_preflight rejects on any busy op, including non-paused ones.
    // Why: Lock is teardown — must not proceed while btrfs is mid-operation.
    // Scenario: sysfs says "device add".
    fn lock_preflight_rejects_busy_op() {
        let fs = MockFs::with_sysfs(FSID, "device add\n");
        let err = require_lock_preflight(&fs, &fsid()).unwrap_err();
        assert!(
            err.contains("device add") && err.contains("in progress"),
            "expected 'device add' + 'in progress' in: {err}"
        );
    }

    #[test]
    // Intent: require_lock_preflight also rejects paused balance (not just running ops).
    // Why: A paused balance is still an active exclusive-op holder.
    // Scenario: sysfs says "balance paused".
    fn lock_preflight_rejects_balance_paused() {
        let fs = MockFs::with_sysfs(FSID, "balance paused\n");
        let err = require_lock_preflight(&fs, &fsid()).unwrap_err();
        assert!(
            err.contains("balance (paused)") && err.contains("in progress"),
            "expected 'balance (paused)' + 'in progress' in: {err}"
        );
    }

    #[test]
    // Intent: require_lock_preflight rejects when sysfs is unreadable.
    // Why: Fail-closed -- if we cannot determine kernel state, lock teardown
    //   must not proceed and risk unmounting mid exclusive-op.
    // Scenario: /sys/fs/btrfs/{fsid}/exclusive_operation cannot be read
    //   (for example, namespace/sandbox without sysfs or permission denied).
    fn lock_preflight_rejects_on_sysfs_read_failure() {
        let fs = MockFs::empty();
        let err = require_lock_preflight(&fs, &fsid()).unwrap_err();
        assert!(
            err.contains("cannot read exclusive operation status"),
            "expected read-failure error, got: {err}"
        );
    }

    #[test]
    // Intent: require_lock_preflight rejects when sysfs reports a value the
    //   parser does not recognize.
    // Why: Fail-closed -- a future kernel that adds a new exclop name must not
    //   silently allow lock teardown. Pins the parser-error to caller-facing
    //   string wiring at the boundary that actually matters for callers.
    // Scenario: New btrfs version writes a value `btrfs_exclusive_operation_show` would not emit.
    fn lock_preflight_rejects_on_unrecognized_value() {
        let fs = MockFs::with_sysfs(FSID, "brand new op\n");
        let err = require_lock_preflight(&fs, &fsid()).unwrap_err();
        assert!(
            err.contains("unrecognized exclusive operation"),
            "expected unrecognized-value error, got: {err}"
        );
    }

    #[test]
    // Intent: require_lock_preflight reads the exclusive_operation file for the
    //   exact fsid it is given, not a fixed or sibling fsid's file.
    // Why it exists: the path is /sys/fs/btrfs/{fsid}/exclusive_operation; a
    //   regression that stopped tracking the fsid argument (hardcoded, cached,
    //   or captured-outer fsid) would read the wrong filesystem's busy state.
    //   Lock teardown is fail-closed precisely to avoid unmounting mid
    //   balance/replace, so the per-fsid derivation is a real safety gate. The
    //   lock fixtures (test_fixtures/shared.rs) match any path ending in
    //   /exclusive_operation and cannot prove this -- assert it here in the
    //   fsid-keyed unit lane.
    // Scenario: two btrfs filesystems present -- one mid-balance, one idle.
    //   Locking the idle pool must pass; locking the balancing pool must refuse.
    fn lock_preflight_keys_off_given_fsid() {
        const OTHER_FSID: &str = "11111111-2222-3333-4444-555555555555";
        let fs = MockFs::with_sysfs(FSID, "balance\n").with_sysfs_entry(OTHER_FSID, "none\n");

        assert!(
            require_lock_preflight(&fs, &Fsid::parse(OTHER_FSID).unwrap()).is_ok(),
            "expected idle fsid to pass preflight"
        );

        let err = require_lock_preflight(&fs, &fsid()).unwrap_err();
        assert!(
            err.contains("in progress"),
            "expected busy refusal for the balancing fsid, got: {err}"
        );
    }

    // Intent: systemd-stop preflight reports only running balance as needing pause.
    // Why it exists: ExecStop must request pause before unmount for a running
    //   balance, but idle and already-paused states should not issue a pause ioctl.
    // Scenario: sysfs reports none, balance, and balance paused during shutdown.
    #[test]
    fn systemd_stop_lock_preflight_reports_pause_requirement() {
        for (body, expected) in [
            ("none\n", false),
            ("balance\n", true),
            ("balance paused\n", false),
        ] {
            let fs = MockFs::with_sysfs(FSID, body);
            assert_eq!(
                systemd_stop_lock_requires_balance_pause(&fs, &fsid()),
                Ok(expected),
                "unexpected pause requirement for {body:?}"
            );
        }
    }

    // Intent: systemd-stop lock preflight rejects non-balance exclusive ops.
    // Why it exists: only btrfs balance has a verified safe quiesce path
    //   through umount; other exclusive ops remain unsafe for shutdown lock.
    //   This is the only direct preflight-boundary test of the full non-balance
    //   op matrix and the exact `cannot lock ... in progress` wording; the
    //   command-level `cli/src/lock.rs#systemd_stop_rejects_non_balance_op` test
    //   drives only the `device remove` case, so do not delete this as redundant.
    // Scenario: ExecStop observes a device add/remove/replace, resize, or
    //   swap activation in progress and must fail before unmounting.
    #[test]
    fn systemd_stop_lock_preflight_rejects_non_balance_ops() {
        for op in [
            "device add",
            "device remove",
            "device replace",
            "resize",
            "swap activate",
        ] {
            let fs = MockFs::with_sysfs(FSID, &format!("{op}\n"));
            let err = systemd_stop_lock_requires_balance_pause(&fs, &fsid()).unwrap_err();
            assert!(
                err.contains(op) && err.contains("cannot lock") && err.contains("in progress"),
                "expected systemd-stop refusal naming {op:?}, got: {err}"
            );
        }
    }

    // --- require_mutation_preflight tests ---

    fn mp() -> MountPoint {
        MountPoint::new("/mnt/storage".into())
    }

    #[test]
    // Intent: require_mutation_preflight returns an empty notes vec on the
    //   clean path (no busy op, rw probe).
    // Why: Baseline happy path -- mutating commands should proceed on a healthy
    //   pool without emitting any PreviewNote.
    // Scenario: sysfs says "none", mountinfo reports rw.
    fn mutation_preflight_passes_when_none() {
        let fs = MockFs::with_sysfs(FSID, "none\n").with_mountinfo(&mountinfo_rw());
        let notes = require_mutation_preflight(&fs, &fsid(), &mp()).unwrap();
        assert!(notes.is_empty(), "expected empty notes, got {notes:?}");
    }

    #[test]
    // Intent: require_mutation_preflight rejects when a balance is paused.
    // Why: A paused balance holds the exclusive-op lock indefinitely; proceeding
    //   would deadlock.
    // Scenario: sysfs says "balance paused".
    fn mutation_preflight_rejects_balance_paused() {
        let fs = MockFs::with_sysfs(FSID, "balance paused\n");
        let err = require_mutation_preflight(&fs, &fsid(), &mp()).unwrap_err();
        assert!(
            err.contains("balance is paused"),
            "expected 'balance is paused' in: {err}"
        );
    }

    #[test]
    // Intent: require_mutation_preflight surfaces a busy exclusive op as a
    //   single Info note.
    // Why: The kernel serializes exclusive ops, so waiting is safe; the
    //   operator still needs to know the mutation is about to enqueue behind
    //   the in-flight op.
    // Scenario: sysfs says "device add", mountinfo reports rw.
    fn mutation_preflight_busy_op_returns_info_note() {
        let fs = MockFs::with_sysfs(FSID, "device add\n").with_mountinfo(&mountinfo_rw());
        let notes = require_mutation_preflight(&fs, &fsid(), &mp()).unwrap();
        assert_eq!(notes.len(), 1, "expected one Info note, got {notes:?}");
        match &notes[0] {
            PreviewNote::Info(body) => {
                assert!(body.contains("waiting for in-flight"), "body={body:?}");
                assert!(body.contains("device add"), "body={body:?}");
            }
            other => panic!("expected Info, got {other:?}"),
        }
    }

    #[test]
    // Intent: require_mutation_preflight surfaces a mountinfo probe failure as
    //   a single Warn note.
    // Why: The read-only guard is a best-effort safety net; if the probe
    //   itself fails, the caller must not silently proceed -- operators
    //   should know the ro mount guard did not run.
    // Scenario: sysfs says "none"; mountinfo read fails.
    fn mutation_preflight_readonly_probe_failure_returns_warn_note() {
        let fs = MockFs::with_sysfs(FSID, "none\n")
            .with_mountinfo_error(std::io::ErrorKind::PermissionDenied);
        let notes = require_mutation_preflight(&fs, &fsid(), &mp()).unwrap();
        assert_eq!(notes.len(), 1, "expected one Warn note, got {notes:?}");
        match &notes[0] {
            PreviewNote::Warn(body) => {
                assert!(
                    body.starts_with("read-only pre-flight failed:"),
                    "body={body:?}"
                );
                assert!(body.ends_with("; proceeding anyway"), "body={body:?}");
            }
            other => panic!("expected Warn, got {other:?}"),
        }
    }

    #[test]
    // Intent: require_mutation_preflight stacks [Info, Warn] when both an
    //   in-flight exclusive op AND a probe failure happen.
    // Why: insertion order is load-bearing for the renderer (busy-op Info
    //   before probe-failure Warn) so dry-run stdout and failure-path stderr
    //   agree on how the two diagnostics present.
    // Scenario: sysfs says "device add"; mountinfo read fails.
    fn mutation_preflight_busy_and_probe_failure_returns_info_then_warn() {
        let fs = MockFs::with_sysfs(FSID, "device add\n")
            .with_mountinfo_error(std::io::ErrorKind::PermissionDenied);
        let notes = require_mutation_preflight(&fs, &fsid(), &mp()).unwrap();
        assert_eq!(notes.len(), 2, "expected two notes, got {notes:?}");
        assert!(
            matches!(
                &notes[0],
                PreviewNote::Info(b) if b.contains("waiting for in-flight") && b.contains("device add")
            ),
            "notes[0]={:?}",
            notes[0]
        );
        assert!(
            matches!(
                &notes[1],
                PreviewNote::Warn(b) if b.starts_with("read-only pre-flight failed:")
                    && b.ends_with("; proceeding anyway")
            ),
            "notes[1]={:?}",
            notes[1]
        );
    }

    // --- check_ups_not_on_battery tests ---

    fn upsc_mock(name: &str, stdout: &str, exit: i32) -> MockRunner {
        MockRunner::default().with_output(
            CmdRequest::UpscQuery {
                name: name.to_owned(),
            },
            RawCommandOutput {
                cmd: format!("upsc {name}"),
                stdout: stdout.to_owned(),
                stderr: if exit == 0 {
                    ""
                } else {
                    "Error: Connection failure: Connection refused"
                }
                .to_owned(),
                exit_status: exit,
            },
        )
    }

    #[test]
    // Intent: check_ups_not_on_battery passes when ups_name is None.
    // Why: users who have not enabled braid.ups should not see a preflight
    // change at all. The no-op guard is load-bearing for config compat.
    // Scenario: braid.ups.enable = false (default), operator runs `braid add`.
    fn ups_no_config_is_noop() {
        let runner = MockRunner::default();
        assert!(check_ups_not_on_battery(&runner, None, "add").is_ok());
    }

    #[test]
    // Intent: check_ups_not_on_battery passes when ups.status = OL.
    // Why: preflight must not refuse the healthy case; doing so would make
    // `braid.ups.enable = true` refuse every mutation and silently regress.
    // Scenario: operator runs `braid add` against a UPS on utility power.
    fn ups_online_passes() {
        let runner = upsc_mock("ups", "ups.status: OL\n", 0);
        assert!(check_ups_not_on_battery(&runner, Some("ups"), "add").is_ok());
    }

    #[test]
    // Intent: OB in the status set triggers refusal.
    // Why: primary safety feature -- narrow the mid-mutation recovery surface
    // by rejecting avoidable starts on battery.
    // Scenario: operator runs `braid remove` while the UPS is on battery.
    fn ups_on_battery_refuses() {
        let runner = upsc_mock("ups", "ups.status: OB\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "remove").unwrap_err();
        assert!(err.contains("on-battery"), "got: {err}");
        assert!(err.contains("remove"), "op name should appear in: {err}");
    }

    #[test]
    // Intent: OB wins over OL when both flags are present.
    // Why: contradictory UPS status must not pass as online merely because
    // affirmative utility power is also reported.
    // Scenario: a driver emits the contradictory pair `OL OB` during a power
    // transition while an operator tries to start a mutation.
    fn ups_on_battery_with_ol_refuses() {
        let runner = upsc_mock("ups", "ups.status: OL OB\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(err.contains("on-battery"), "got: {err}");
    }

    #[test]
    // Intent: LB alone (without OB) still triggers refusal.
    // Why: upsmon's critical-state check requires OB+LB together, but a
    // battery self-test can transiently show LB+OL. braid refuses either
    // way because starting a long mutation while LB is reported is risky.
    // Scenario: UPS reports LB during a self-test or flaky USB HID state.
    fn ups_low_battery_refuses() {
        let runner = upsc_mock("ups", "ups.status: OL LB\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(err.contains("critical"), "got: {err}");
        assert!(!err.contains("on-battery"), "got: {err}");
    }

    #[test]
    // Intent: TESTFAIL in ups.status triggers refusal, even when OL is
    // also present.
    // Why: the TUI shows TESTFAIL in red as a critical state; preflight
    // must agree. A driver that surfaces TESTFAIL while OL is lit must
    // not be a "green light" for mutation starts. Shares the predicate
    // with the UI so the two surfaces cannot drift.
    // Scenario: some drivers append TESTFAIL to ups.status on a
    // failed self-test.
    fn ups_test_fail_refuses() {
        let runner = upsc_mock("ups", "ups.status: OL TESTFAIL\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(err.contains("critical"), "got: {err}");
    }

    #[test]
    // Intent: COMMBAD triggers refusal.
    // Why: comms loss is fail-closed by definition -- we cannot trust
    // what the UPS reports next. The TUI paints this red; preflight
    // refuses.
    // Scenario: USB cable unplugged mid-session; driver reports
    // COMMBAD in ups.status before declaring the UPS lost.
    fn ups_comm_bad_refuses() {
        let runner = upsc_mock("ups", "ups.status: OL COMMBAD\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(err.contains("critical"), "got: {err}");
    }

    #[test]
    // Intent: FSD triggers refusal.
    // Why: Forced-Shutdown-Delay means the UPS has decided shutdown is
    // imminent. Starting a mutation here is always wrong.
    // Scenario: network UPS has been issued a scheduled shutdown.
    fn ups_fsd_refuses() {
        let runner = upsc_mock("ups", "ups.status: OL FSD\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(err.contains("critical"), "got: {err}");
    }

    #[test]
    // Intent: query failure (non-zero upsc exit) refuses the mutation.
    // Why: fail-closed -- if braid cannot determine UPS state, it must not
    // start work it can't guarantee a clean shutdown from.
    // Scenario: upsd.service has crashed or hasn't started yet.
    fn ups_query_failed_refuses() {
        let runner = upsc_mock("ups", "", 1);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "replace").unwrap_err();
        assert!(err.contains("utility power"), "got: {err}");
        assert!(err.contains("upsc query failed"), "got: {err}");
        assert!(err.contains("Connection failure"), "got: {err}");
    }

    // Intent: query failure with empty stderr refuses without a dangling
    // suffix inside the parenthesized context.
    // Why: the preflight refusal wraps the query detail in a larger safety
    // message, so a contentless `: ` tail would be visible to operators
    // before every refused mutation.
    // Scenario: upsc exits non-zero but writes nothing to stderr while a
    // mutation preflight tries to prove utility power.
    #[test]
    fn ups_query_failed_with_empty_stderr_refuses_without_detail_tail() {
        let runner = MockRunner::default().with_output(
            CmdRequest::UpscQuery { name: "ups".into() },
            RawCommandOutput {
                cmd: "upsc ups".into(),
                stdout: String::new(),
                stderr: String::new(),
                exit_status: 1,
            },
        );
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(err.contains("utility power"), "got: {err}");
        assert!(err.contains("(upsc query failed)"), "got: {err}");
    }

    #[test]
    // Intent: empty status set (no ups.status line) refuses.
    // Why: an absent ups.status is indistinguishable from a stuck driver;
    // treating empty as OL would undermine the whole preflight contract.
    // Scenario: dummy-ups driver hasn't filled in ups.status yet.
    fn ups_empty_status_refuses() {
        let runner = upsc_mock("ups", "battery.charge: 100\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "remove-missing").unwrap_err();
        assert!(err.contains("empty or missing"), "got: {err}");
    }

    #[test]
    // Intent: missing mock output is treated as invocation failure (fail-closed).
    // Why: MockRunner::default() produces MissingMock, which mirrors a
    // subprocess spawn failure at runtime; both must refuse.
    // Scenario: a future refactor forgets to wire the upsc mock in a test.
    fn ups_invocation_failed_refuses() {
        let runner = MockRunner::default();
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(err.contains("utility power"), "got: {err}");
        assert!(err.contains("upsc invocation failed"), "got: {err}");
    }

    #[test]
    // Intent: a known non-critical advisory flag alongside OL still passes.
    // Why: the gate proves utility power, not full battery health. `RB`
    // (replace-battery advisory) is not evidence that input power is absent,
    // so requiring exactly {OL} would lock out mutations on a healthy-but-
    // aging UPS that is plainly on line power.
    // Scenario: a UPS on utility power that has also raised a battery-
    // replacement advisory.
    fn ups_online_with_advisory_passes() {
        let runner = upsc_mock("ups", "ups.status: OL RB\n", 0);
        assert!(check_ups_not_on_battery(&runner, Some("ups"), "add").is_ok());
    }

    #[test]
    // Intent: a non-empty status set with no OL refuses, even with no known
    // blocker present.
    // Why: preflight requires affirmative utility-power evidence (`OL`), not
    // merely the absence of `OB`. The old final-`Ok(())` blocklist would let
    // this pass; the OL gate is what closes that hole.
    // Scenario: a driver reports only `RB` and drops the `OL` token while the
    // line-power state is unproven.
    fn ups_status_without_ol_refuses() {
        let runner = upsc_mock("ups", "ups.status: RB\n", 0);
        let err = check_ups_not_on_battery(&runner, Some("ups"), "add").unwrap_err();
        assert!(err.contains("OL missing"), "got: {err}");
    }

    #[test]
    // Intent: an unknown status token alongside OL still passes.
    // Why: NUT permits clients to ignore unidentified tokens; failing closed
    // on every novel advisory would create avoidable maintenance lockouts on
    // routine NUT/device changes. `OL` present + no known blocker = safe to
    // start.
    // Scenario: a firmware/driver update surfaces a new ups.status token braid
    // does not classify yet, while the UPS is on utility power.
    fn ups_online_with_unknown_token_passes() {
        let runner = upsc_mock("ups", "ups.status: OL NEWFLAG\n", 0);
        assert!(check_ups_not_on_battery(&runner, Some("ups"), "add").is_ok());
    }

    #[test]
    // Intent: require_mutation_preflight rejects when the pool is mounted read-only.
    // Why: Mutating commands will fail at the filesystem layer; better to fail
    //   early with a clear message.
    // Scenario: sysfs says "none", mountinfo reports ro.
    fn mutation_preflight_rejects_read_only() {
        let fs = MockFs::with_sysfs(FSID, "none\n")
            .with_mountinfo(&mountinfo_for_target("ro,relatime", "rw,space_cache=v2"));
        let err = require_mutation_preflight(&fs, &fsid(), &mp()).unwrap_err();
        assert!(err.contains("read-only"), "expected 'read-only' in: {err}");
    }

    // --- check_pool_unlocked_if_membership_exists ---

    fn pool_mounted_for_test() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 0,
            fsid: Some(Fsid::parse(FSID).unwrap()),
            null_underlying: vec![],
        }
    }

    fn pool_unmounted_for_test() -> PoolState {
        PoolState {
            mounted: false,
            devices: vec![],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 0,
            fsid: None,
            null_underlying: vec![],
        }
    }

    fn membership_with(names: &[&str]) -> PoolMembership {
        use crate::membership::DiskMember;
        use crate::types::{ByIdPath, DiskName, LuksUuid};
        let mut m = PoolMembership::empty();
        for (idx, n) in names.iter().enumerate() {
            let member = DiskMember::new(
                DiskName::parse(n).expect("valid fixture disk name"),
                ByIdPath::parse(&format!("/dev/disk/by-id/virtio-{n}"))
                    .expect("valid fixture by-id"),
            );
            m.insert(
                LuksUuid::parse(&format!("00000000-0000-0000-0000-{:012x}", idx + 1))
                    .expect("valid fixture UUID"),
                member,
            )
            .expect("insert fixture member");
        }
        m
    }

    /* Intent: check_pool_unlocked_if_membership_exists is a no-op when
     * pool.json has no members, regardless of mount state.
     * Why it exists: bootstrap of a fresh system has empty pool.json and
     * an unmounted pool; the check must not block legitimate first-add.
     * Scenario: brand-new system, operator runs `braid add disk1=...`.
     */
    #[test]
    fn pool_unlocked_check_passes_when_membership_empty() {
        let m = PoolMembership::empty();
        assert!(check_pool_unlocked_if_membership_exists(&m, &pool_unmounted_for_test()).is_ok());
        assert!(check_pool_unlocked_if_membership_exists(&m, &pool_mounted_for_test()).is_ok());
    }

    /* Intent: check_pool_unlocked_if_membership_exists is a no-op when the
     * pool is mounted, regardless of membership.
     * Why it exists: an unlocked, mounted pool is the steady state in
     * which `braid add` legitimately mutates membership.
     * Scenario: operator unlocked the pool, now adds a disk.
     */
    #[test]
    fn pool_unlocked_check_passes_when_pool_mounted() {
        let m = membership_with(&["disk1", "disk2"]);
        assert!(check_pool_unlocked_if_membership_exists(&m, &pool_mounted_for_test()).is_ok());
    }

    /* Intent: check_pool_unlocked_if_membership_exists rejects when
     * pool.json lists members but the pool is not mounted, naming the
     * locked members so the operator can verify which pool they have.
     * Why it exists: this is the core regression for the silent-bootstrap
     * bug. Without this check, `braid add <fresh-disk>` against a locked
     * 2-member pool overwrites pool.json and orphans the existing members.
     * Scenario: 2-disk pool, both LUKS-locked, operator forgets `braid
     * unlock` and runs `braid add disk3=...`.
     */
    #[test]
    fn pool_unlocked_check_rejects_when_pool_locked_with_members() {
        let m = membership_with(&["disk1", "disk2"]);
        let err =
            check_pool_unlocked_if_membership_exists(&m, &pool_unmounted_for_test()).unwrap_err();
        assert!(err.contains("not unlocked"), "got: {err}");
        assert!(err.contains("disk1"), "expected disk1 named, got: {err}");
        assert!(err.contains("disk2"), "expected disk2 named, got: {err}");
        assert!(
            err.contains("braid unlock"),
            "expected unlock remediation, got: {err}"
        );
    }
}
