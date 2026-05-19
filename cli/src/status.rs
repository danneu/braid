use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use crate::alert::{self, AlertCause, AlertState};
use crate::cmd::{CmdError, CmdRequest, CommandRunner, LsblkFieldKind};
use crate::config::{Config, mapper_name};
use crate::confirm::get_lsblk_field;
use crate::luks::{self, BackingPathResolver};
use crate::membership::{self, PoolMembership};
use crate::parse::types::BalanceState;
use crate::parse::types::BtrfsDfOutput;
use crate::parse::{
    BtrfsDeviceStatsOutput, ParseError, ScrubState, parse_btrfs_balance_status,
    parse_btrfs_device_stats, parse_btrfs_device_usage, parse_btrfs_df_json,
    parse_btrfs_filesystem_usage, parse_btrfs_scrub_status,
};
use crate::probe::{Filesystem, ProbeError, probe_config_disk, probe_pool};
use crate::progress::pct_from_bytes;
use crate::state_paths::StatePaths;
use crate::types::*;

// ---------------------------------------------------------------------------
// Public types (JSON schema)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusCode {
    Intact,
    Degraded,
    NotMounted,
}

impl StatusCode {
    pub fn display_human(self, missing_count: u64) -> String {
        match self {
            StatusCode::Intact => "intact".to_owned(),
            StatusCode::Degraded if missing_count == 1 => "DEGRADED (1 missing device)".to_owned(),
            StatusCode::Degraded => format!("DEGRADED ({missing_count} missing devices)"),
            StatusCode::NotMounted => "not mounted".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub mount_point: MountPoint,
    pub status: StatusCode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_devices: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub present_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub missing_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub capacity: Option<CapacityReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_scrub: Option<ScrubReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub balance: Option<BalanceReport>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allocation: Option<Vec<AllocationEntry>>,
    pub disks: Vec<DiskReport>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub advisories: Vec<String>,
    #[serde(default)]
    pub alert_active: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub alert_causes: Vec<AlertCause>,
    /// Every devid that contributes to `missing_count`. This is the union of
    /// btrfs's authoritative `MISSING` set and null-underlying devids (LUKS
    /// mapper open, backing block device gone), matching the set used for
    /// `MissingDevice` alert causes. Destructive commands such as
    /// `remove-missing` and `replace --missing-id` use the btrfs-only subset
    /// and can reject a null-underlying devid reported here.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub missing_devids: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AllocationEntry {
    pub bg_type: String,
    pub profile: String,
    pub used_bytes: u64,
    pub allocated_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityReport {
    pub total_bytes: Option<u64>,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

/// Estimate usable pool capacity for RAID1 given raw device sizes.
///
/// For RAID1, every chunk is mirrored, so total usable = sum/2 — except when
/// drives are different sizes: the oversized portion of the largest drive can
/// never be paired, so usable = min(sum/2, sum - max).
///
/// Single-disk pools have no mirroring, so the full size is usable.
pub fn estimate_pool_capacity(device_sizes: &[u64]) -> u64 {
    let total: u64 = device_sizes.iter().sum();
    if device_sizes.len() < 2 {
        return total;
    }
    let max: u64 = device_sizes.iter().copied().max().unwrap();
    (total / 2).min(total - max)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum BalanceReport {
    /// Probe failed or parse error — balance state is indeterminate.
    Unknown,
    /// No balance operation is running.
    Idle,
    Running {
        done_chunks: u64,
        estimated_total_chunks: u64,
        considered_chunks: u64,
        pct_left: u8,
    },
    Paused {
        done_chunks: u64,
        estimated_total_chunks: u64,
        considered_chunks: u64,
        pct_left: u8,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum ScrubReport {
    Never,
    Running {
        #[serde(skip_serializing_if = "Option::is_none")]
        pct: Option<u8>,
    },
    Finished {
        started_at: String,
        error_count: u64,
    },
    Aborted {
        started_at: String,
        error_count: u64,
    },
    Interrupted {
        started_at: String,
        error_count: u64,
    },
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DiskStatus {
    Present,
    Missing,
    LuksHeaderUnreadable,
    LuksHeaderDamaged,
    Unknown,
}

impl std::fmt::Display for DiskStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Present => f.write_str("present"),
            Self::Missing => f.write_str("missing"),
            Self::LuksHeaderUnreadable => f.write_str("luks-header-unreadable"),
            Self::LuksHeaderDamaged => f.write_str("luks-header-damaged"),
            Self::Unknown => f.write_str("unknown"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskReport {
    pub name: String,
    pub mapper: String,
    pub by_id: String,
    pub luks_uuid: String,
    pub devid: Option<String>,
    pub underlying: Option<String>,
    pub status: DiskStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub errors: Option<DiskErrors>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskErrors {
    pub read: u64,
    pub write: u64,
    pub flush: u64,
    pub corruption: u64,
    pub generation: u64,
}

impl DiskErrors {
    pub fn total(&self) -> u64 {
        self.read + self.write + self.flush + self.corruption + self.generation
    }
}

// ---------------------------------------------------------------------------
// Compact drive (always-on summary)
// ---------------------------------------------------------------------------

struct CompactDrive {
    name: String,
    device_short: String,
    devid: Option<u64>,
    status: DiskStatus,
}

fn build_compact_drives(pool: &PoolState, membership: &PoolMembership) -> Vec<CompactDrive> {
    let mut drives = Vec::new();

    // Present pool devices
    let pool_luks_uuids: HashSet<&LuksUuid> = pool.devices.iter().map(|d| &d.luks_uuid).collect();
    for pd in &pool.devices {
        let name = membership
            .by_uuid(&pd.luks_uuid)
            .map(|member| member.name.as_str())
            .unwrap_or(&pd.mapper.0)
            .to_owned();
        let device_short = pd
            .underlying
            .strip_prefix("/dev/")
            .unwrap_or(&pd.underlying)
            .to_owned();
        drives.push(CompactDrive {
            name,
            device_short,
            devid: Some(pd.devid),
            status: DiskStatus::Present,
        });
    }

    // Unpooled membership disks
    let alert_devids: HashSet<u64> = pool.alert_missing_devids().into_iter().collect();
    for (uuid, member) in membership.iter_by_name() {
        if pool_luks_uuids.contains(uuid) {
            continue;
        }
        let name = member.name.as_str();
        let devid = member.devid.filter(|d| alert_devids.contains(d));
        drives.push(CompactDrive {
            name: name.to_owned(),
            device_short: "-".to_owned(),
            devid,
            status: DiskStatus::Missing,
        });
    }

    drives
}

/// Resolve every btrfs-surfaced devid to the display name status should show.
/// Present devices mirror `build_disk_reports`'s UUID-first name rule; missing
/// and null-underlying entries use persisted devid only as the no-live-UUID
/// fallback authorized for display joins.
/// The TUI has parallel input-specific logic that can collapse into this once
/// both paths expose the same membership-shaped inputs.
fn build_devid_names(
    pool: &PoolState,
    membership: &PoolMembership,
) -> Result<HashMap<u64, String>, membership::MembershipError> {
    let mut names = HashMap::new();

    for pd in &pool.devices {
        let name = membership
            .by_uuid(&pd.luks_uuid)
            .map(|member| member.name.as_str().to_owned())
            .unwrap_or_else(|| pd.mapper.0.clone());
        names.insert(pd.devid, name);
    }

    for nu in &pool.null_underlying {
        if let Some((_, member)) = membership.by_devid(nu.devid)? {
            names
                .entry(nu.devid)
                .or_insert_with(|| member.name.as_str().to_owned());
        }
    }

    for devid in &pool.missing_devids {
        if let Some((_, member)) = membership.by_devid(*devid)? {
            names
                .entry(*devid)
                .or_insert_with(|| member.name.as_str().to_owned());
        }
    }

    Ok(names)
}

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum StatusError {
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("command error: {0}")]
    Cmd(#[from] CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] ParseError),
    #[error("json serialization failed: {0}")]
    Json(#[from] serde_json::Error),
    #[error("{0}")]
    Membership(#[from] membership::MembershipError),
}

// ---------------------------------------------------------------------------
// Unpooled disk classification
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Internal types (verbose context)
// ---------------------------------------------------------------------------

struct VerboseContext {
    disks: Vec<DiskReport>,
    human_details: Vec<HumanDisk>,
}

struct HumanDisk {
    name: String,
    member_name: Option<DiskName>,
    by_id: String,
    luks_uuid: String,
    devid: Option<String>,
    status: DiskStatus,
    model: Option<String>,
    serial: Option<String>,
    errors: Option<DiskErrors>,
}

// ---------------------------------------------------------------------------
// Status assembly
// ---------------------------------------------------------------------------

struct BuiltStatus {
    report: StatusReport,
    mounted_extras: Option<MountedExtras>,
}

struct MountedExtras {
    compact_drives: Vec<CompactDrive>,
    human_details: Vec<HumanDisk>,
    devid_names: HashMap<u64, String>,
}

fn not_mounted_status(config: &Config, paths: &StatePaths, advisories: Vec<String>) -> BuiltStatus {
    let alert_state = resolve_alert_state(paths);
    BuiltStatus {
        report: StatusReport {
            mount_point: config.mount_point().clone(),
            status: StatusCode::NotMounted,
            total_devices: None,
            present_count: None,
            missing_count: None,
            profile: None,
            capacity: None,
            last_scrub: None,
            balance: None,
            allocation: None,
            disks: vec![],
            advisories,
            alert_active: alert_state.active(),
            alert_causes: alert_state.causes,
            missing_devids: vec![],
        },
        mounted_extras: None,
    }
}

fn build_status<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    config: &Config,
    paths: &StatePaths,
    backing_path_resolver: &dyn BackingPathResolver,
) -> Result<BuiltStatus, StatusError> {
    let mut advisories = luks::header_backup_advisories(paths);

    let pool = match probe_pool(runner, fs, config.mount_point()) {
        Ok(p) => p,
        Err(e @ ProbeError::NotBtrfs { .. }) => {
            advisories.push(e.to_string());
            return Ok(not_mounted_status(config, paths, advisories));
        }
        Err(e) => return Err(e.into()),
    };

    if !pool.mounted {
        return Ok(not_mounted_status(config, paths, advisories));
    }

    let membership = match membership::load_membership(paths) {
        Ok(m) => m,
        Err(membership::MembershipError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::NotFound =>
        {
            PoolMembership::empty()
        }
        Err(e) => return Err(e.into()),
    };

    let df = fetch_df(runner, config.mount_point())?;
    let df_summary = summarize_df(&df);
    let capacity = get_capacity(runner, config.mount_point(), pool.missing_count, &df)?;
    let last_scrub = get_scrub_report(runner, config.mount_point());
    let balance = get_balance_report(runner, config.mount_point());

    let code = if pool.missing_count == 0 {
        StatusCode::Intact
    } else {
        StatusCode::Degraded
    };

    let compact_drives = build_compact_drives(&pool, &membership);
    let devid_names = build_devid_names(&pool, &membership)?;

    let members: Vec<_> = membership
        .iter_by_name()
        .into_iter()
        .map(|(_, member)| member)
        .collect();
    let config_disks: Vec<ConfigDisk> = members
        .into_iter()
        .map(|member| {
            probe_config_disk(
                runner,
                fs,
                &member.name,
                &member.by_id,
                backing_path_resolver,
            )
        })
        .collect::<Result<Vec<_>, _>>()?;
    let device_stats = get_device_stats(runner, config.mount_point())?;
    let verbose_ctx = build_disk_reports(runner, &membership, &config_disks, &pool, &device_stats);

    let alert_state = resolve_alert_state(paths);

    let present_count = pool.total_devices.saturating_sub(pool.missing_count);
    let report = StatusReport {
        mount_point: config.mount_point().clone(),
        status: code,
        total_devices: Some(pool.total_devices),
        present_count: Some(present_count),
        missing_count: Some(pool.missing_count),
        profile: Some(df_summary.profile),
        capacity: Some(capacity),
        last_scrub: Some(last_scrub),
        balance: Some(balance),
        allocation: Some(df_summary.allocation),
        disks: verbose_ctx.disks,
        advisories,
        alert_active: alert_state.active(),
        alert_causes: alert_state.causes,
        missing_devids: pool.alert_missing_devids(),
    };

    Ok(BuiltStatus {
        report,
        mounted_extras: Some(MountedExtras {
            compact_drives,
            human_details: verbose_ctx.human_details,
            devid_names,
        }),
    })
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn cmd_status<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    config: &Config,
    json: bool,
    paths: &StatePaths,
    backing_path_resolver: &dyn BackingPathResolver,
) -> Result<(), StatusError> {
    let built = build_status(runner, fs, config, paths, backing_path_resolver)?;

    if json {
        println!("{}", serde_json::to_string_pretty(&built.report)?);
    } else {
        let extras = built.mounted_extras.as_ref();
        print!(
            "{}",
            format_status_human(
                &built.report,
                extras.map(|e| e.compact_drives.as_slice()),
                extras.map(|e| e.human_details.as_slice()),
                extras.map(|e| &e.devid_names),
            )
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Alert state (latch-based)
// ---------------------------------------------------------------------------

/// Read alert state from the latch file + smartd flag. Status reads the latch
/// instead of recomputing live alert state -- the latch is the single source of
/// truth. Recomputing would cause the alert to disappear when a condition
/// resolves, contradicting the "latched until ack" model. The smartd flag is
/// checked as a bridge for between-cycle fires. The cleanup-pending sentinel is
/// also surfaced so interrupted ack cleanup remains visible to status and TUI.
pub(crate) fn resolve_alert_state(paths: &StatePaths) -> AlertState {
    let smartd_active = alert::smartd_alert_active(paths);
    let cleanup_pending = alert::alert_cleanup_pending(paths);

    let latch = match alert::load_alert_latch(paths) {
        Ok(opt) => opt,
        Err(e) => {
            // Fail loud: don't pretend "no alert" when we can't read the
            // latch. Status is read-only -- never quarantine here; that is
            // monitor's job.
            let mut causes = vec![AlertCause::ComputationError {
                detail: format!("alert latch unreadable -- {e}"),
            }];
            if cleanup_pending {
                causes.push(AlertCause::ComputationError {
                    detail: "ack cleanup pending -- re-run `braid ack` to resume".to_owned(),
                });
            }
            if smartd_active {
                causes.push(AlertCause::SmartdAlert);
            }
            return AlertState { causes };
        }
    };

    let mut state = latch.unwrap_or_default();
    if smartd_active
        && !state
            .causes
            .iter()
            .any(|c| matches!(c, AlertCause::SmartdAlert))
    {
        state.causes.push(AlertCause::SmartdAlert);
    }
    if cleanup_pending {
        state.causes.push(AlertCause::ComputationError {
            detail: "ack cleanup pending -- re-run `braid ack` to resume".to_owned(),
        });
    }
    state
}

// ---------------------------------------------------------------------------
// Private helpers — strict (return Result)
// ---------------------------------------------------------------------------

struct DfSummary {
    profile: String,
    allocation: Vec<AllocationEntry>,
}

fn fetch_df<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<BtrfsDfOutput, StatusError> {
    let raw = runner.run(&CmdRequest::BtrfsFilesystemDfJson {
        mount_point: mount_point.clone(),
    })?;
    Ok(parse_btrfs_df_json(&raw)?)
}

fn summarize_df(df: &BtrfsDfOutput) -> DfSummary {
    let profiles = df.profiles_for(crate::parse::types::BtrfsBgType::Data);
    let profile = if profiles.is_empty() {
        "unknown".to_owned()
    } else {
        profiles
            .iter()
            .map(|p| p.to_string())
            .collect::<Vec<_>>()
            .join(", ")
    };

    let mut entries: Vec<_> = df
        .entries
        .iter()
        .filter(|e| e.bg_type != crate::parse::types::BtrfsBgType::GlobalReserve)
        .collect();
    entries.sort();

    let allocation = entries
        .iter()
        .map(|e| AllocationEntry {
            bg_type: e.bg_type.to_string(),
            profile: e.bg_profile.to_string(),
            used_bytes: e.bg_used,
            allocated_bytes: e.bg_total,
        })
        .collect();

    DfSummary {
        profile,
        allocation,
    }
}

fn get_capacity<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    missing_count: u64,
    df: &BtrfsDfOutput,
) -> Result<CapacityReport, StatusError> {
    let raw = runner.run(&CmdRequest::BtrfsFilesystemUsageRaw {
        mount_point: mount_point.clone(),
    })?;
    let usage = parse_btrfs_filesystem_usage(&raw)?;

    let total_bytes = if missing_count == 0 {
        let dev_raw = runner.run(&CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: mount_point.clone(),
        })?;
        let dev_usage = parse_btrfs_device_usage(&dev_raw)?;
        let sizes: Vec<u64> = dev_usage.devices.iter().map(|d| d.device_size).collect();
        Some(estimate_pool_capacity(&sizes))
    } else {
        None
    };

    Ok(CapacityReport {
        total_bytes,
        used_bytes: df.logical_used_bytes(),
        free_bytes: usage.free_estimated_bytes,
    })
}

fn get_device_stats<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<BtrfsDeviceStatsOutput, StatusError> {
    let raw = runner.run(&CmdRequest::BtrfsDeviceStatsJson {
        mount_point: mount_point.clone(),
    })?;
    let stats = parse_btrfs_device_stats(&raw)?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Private helpers — tolerant (never fail)
// ---------------------------------------------------------------------------

fn get_scrub_report<R: CommandRunner>(runner: &R, mount_point: &MountPoint) -> ScrubReport {
    let raw = match runner.run(&CmdRequest::BtrfsScrubStatus {
        mount_point: mount_point.clone(),
    }) {
        Ok(r) => r,
        Err(_) => return ScrubReport::Unknown,
    };

    match parse_btrfs_scrub_status(&raw) {
        Ok(out) => match out.state {
            ScrubState::Never => ScrubReport::Never,
            ScrubState::Running {
                bytes_scrubbed,
                total_bytes,
                ..
            } => {
                let pct = match (bytes_scrubbed, total_bytes) {
                    (Some(scrubbed), Some(total)) => pct_from_bytes(scrubbed, total),
                    _ => None,
                };
                ScrubReport::Running { pct }
            }
            ScrubState::Finished {
                started_at,
                error_count,
                ..
            } => ScrubReport::Finished {
                started_at: format_scrub_timestamp(&started_at),
                error_count,
            },
            ScrubState::Aborted {
                started_at,
                error_count,
                ..
            } => ScrubReport::Aborted {
                started_at: format_scrub_timestamp(&started_at),
                error_count,
            },
            ScrubState::Interrupted {
                started_at,
                error_count,
                ..
            } => ScrubReport::Interrupted {
                started_at: format_scrub_timestamp(&started_at),
                error_count,
            },
            ScrubState::Unknown => ScrubReport::Unknown,
        },
        Err(_) => ScrubReport::Unknown,
    }
}

fn format_scrub_timestamp(ts: &crate::parse::types::ScrubTimestamp) -> String {
    use time::macros::format_description;
    let fmt = format_description!(
        "[weekday repr:short] [month repr:short] [day padding:space] [hour]:[minute]:[second] [year]"
    );
    ts.0.format(&fmt).unwrap_or_else(|_| "unknown".to_owned())
}

pub(crate) fn get_balance_report<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> BalanceReport {
    let raw = match runner.run(&CmdRequest::BtrfsBalanceStatus {
        mount_point: mount_point.clone(),
    }) {
        Ok(r) => r,
        Err(_) => return BalanceReport::Unknown,
    };
    match parse_btrfs_balance_status(&raw) {
        Ok(parsed) => match parsed.state {
            BalanceState::None => BalanceReport::Idle,
            BalanceState::Running {
                done_chunks,
                estimated_total_chunks,
                considered_chunks,
                pct_left,
            } => BalanceReport::Running {
                done_chunks,
                estimated_total_chunks,
                considered_chunks,
                pct_left,
            },
            BalanceState::Paused {
                done_chunks,
                estimated_total_chunks,
                considered_chunks,
                pct_left,
            } => BalanceReport::Paused {
                done_chunks,
                estimated_total_chunks,
                considered_chunks,
                pct_left,
            },
        },
        Err(_) => BalanceReport::Unknown,
    }
}

/// Check for a paused balance and emit a warning to `out` if found.
/// Returns true if a warning was emitted. Best-effort: command or parse
/// failures emit nothing and return false.
pub fn emit_paused_balance_warning<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    out: &mut dyn std::io::Write,
) -> bool {
    if matches!(
        get_balance_report(runner, mount_point),
        BalanceReport::Paused { .. }
    ) {
        writeln!(out).ok();
        writeln!(out, "  paused balance detected -- will not auto-resume").ok();
        writeln!(out, "    resume:  btrfs balance resume {mount_point}").ok();
        writeln!(out, "    cancel:  btrfs balance cancel {mount_point}").ok();
        true
    } else {
        false
    }
}

// ---------------------------------------------------------------------------
// build_disk_reports
// ---------------------------------------------------------------------------

fn build_disk_reports<R: CommandRunner>(
    runner: &R,
    membership: &PoolMembership,
    config_disks: &[ConfigDisk],
    pool: &PoolState,
    device_stats: &BtrfsDeviceStatsOutput,
) -> VerboseContext {
    let pool_uuid_set: HashSet<&LuksUuid> = pool.devices.iter().map(|d| &d.luks_uuid).collect();

    let mut disk_reports = Vec::new();
    let mut human_details = Vec::new();

    // Present pool devices
    for pd in &pool.devices {
        let matched_member = membership.by_uuid(&pd.luks_uuid);
        let matched_config =
            matched_member.and_then(|member| config_disks.iter().find(|cd| cd.name == member.name));

        // `build_status` derives `config_disks` from membership today, but
        // keep the member-name fallback so this helper stays correct for
        // partial probes or future callers.
        let disk_name = matched_config
            .map(|cd| cd.name.as_str().to_owned())
            .or_else(|| matched_member.map(|member| member.name.as_str().to_owned()))
            .unwrap_or_else(|| {
                // Display-only fallback for foreign live devices. Keep the
                // raw mapper basename instead of deriving a member name from
                // `braid-*`; UUID-keyed membership remains the identity join.
                pd.mapper.0.clone()
            });

        let by_id = matched_config
            .map(|cd| cd.by_id_path.as_str().to_owned())
            .or_else(|| matched_member.map(|member| member.by_id.as_str().to_owned()))
            .unwrap_or_else(|| format!("/dev/mapper/{}", pd.mapper.0));

        let mapper = pd.mapper.0.clone();

        // Model/serial via lsblk (tolerant)
        let model = get_lsblk_field(runner, &by_id, LsblkFieldKind::Model);
        let serial = get_lsblk_field(runner, &by_id, LsblkFieldKind::Serial);

        // Error stats. Pair by the btrfs-native devid row key -- the stats
        // row's path can differ from the mapper path without changing which
        // live btrfs device it describes.
        let errors = device_stats
            .devices
            .iter()
            .find(|d| d.devid == pd.devid)
            .map(|d| DiskErrors {
                read: d.read_io_errs,
                write: d.write_io_errs,
                flush: d.flush_io_errs,
                corruption: d.corruption_errs,
                generation: d.generation_errs,
            });

        disk_reports.push(DiskReport {
            name: disk_name.clone(),
            mapper: mapper.clone(),
            by_id: by_id.clone(),
            luks_uuid: pd.luks_uuid.as_str().to_owned(),
            devid: Some(pd.devid.to_string()),
            underlying: Some(pd.underlying.clone()),
            status: DiskStatus::Present,
            errors: errors.clone(),
        });

        human_details.push(HumanDisk {
            name: disk_name,
            member_name: matched_member.map(|m| m.name.clone()),
            by_id: by_id.clone(),
            luks_uuid: pd.luks_uuid.as_str().to_owned(),
            devid: Some(pd.devid.to_string()),
            status: DiskStatus::Present,
            model,
            serial,
            errors,
        });
    }

    // Unpooled config disks (not matched to pool)
    for cd in config_disks {
        let membership_uuid_live = membership
            .by_name(&cd.name)
            .is_some_and(|(uuid, _)| pool_uuid_set.contains(uuid));
        if membership_uuid_live {
            continue;
        }

        let status = match &cd.state {
            ConfigDiskState::Absent => DiskStatus::Missing,
            ConfigDiskState::PresentLuks { .. } => DiskStatus::Unknown,
            ConfigDiskState::PresentNotLuks => {
                // luksUuid failed during the initial probe. Refine here for
                // diagnostic reporting only -- do NOT propagate this back into
                // ConfigDiskState (mutating commands like add/replace must keep
                // seeing the coarse PresentNotLuks state to preserve their
                // destructive-format guards).
                match luks::probe_luks_header(runner, cd.by_id_path.as_str()) {
                    luks::LuksHeaderState::Unreadable => DiskStatus::LuksHeaderUnreadable,
                    luks::LuksHeaderState::Damaged => DiskStatus::LuksHeaderDamaged,
                    // luksUuid failed but isLuks + luksDump succeeded -- the
                    // re-probe contradicts the original failure, so the most
                    // likely cause is a transient blip (udev settling,
                    // momentary I/O error). Surface Unknown rather than
                    // overclaiming Damaged: nothing in the re-probe
                    // demonstrates damage, and braid doctor would classify
                    // the same header as healthy via the same probe path.
                    luks::LuksHeaderState::Ok => DiskStatus::Unknown,
                    // Probe itself could not run; collapse to the generic
                    // Unknown bucket rather than guessing at Unreadable vs
                    // Damaged.
                    luks::LuksHeaderState::ProbeFailed(_) => DiskStatus::Unknown,
                }
            }
        };
        let mapper = mapper_name(&cd.name).0;

        disk_reports.push(DiskReport {
            name: cd.name.as_str().to_owned(),
            mapper: mapper.clone(),
            by_id: cd.by_id_path.as_str().to_owned(),
            luks_uuid: String::new(),
            devid: None,
            underlying: None,
            status,
            errors: None,
        });

        human_details.push(HumanDisk {
            name: cd.name.as_str().to_owned(),
            member_name: Some(cd.name.clone()),
            by_id: cd.by_id_path.as_str().to_owned(),
            luks_uuid: String::new(),
            devid: None,
            status,
            model: None,
            serial: None,
            errors: None,
        });
    }

    VerboseContext {
        disks: disk_reports,
        human_details,
    }
}

// ---------------------------------------------------------------------------
// Human output formatting
// ---------------------------------------------------------------------------

fn devid_to_name(devid_names: Option<&HashMap<u64, String>>, devid: u64) -> String {
    devid_names
        .and_then(|names| names.get(&devid))
        .map(|name| format!("{name} (devid {devid})"))
        .unwrap_or_else(|| format!("devid {devid}"))
}

fn format_status_human(
    report: &StatusReport,
    compact_drives: Option<&[CompactDrive]>,
    human_disks: Option<&[HumanDisk]>,
    devid_names: Option<&HashMap<u64, String>>,
) -> String {
    let mut out = String::new();

    // Alert banner (before everything else)
    if report.alert_active {
        out.push_str(
            "ALERT -- disk health issue detected. Run 'braid ack' to acknowledge and silence.\n",
        );
        for cause in &report.alert_causes {
            match cause {
                AlertCause::BtrfsDeviceErrors { devid } => {
                    let name = devid_to_name(devid_names, *devid);
                    out.push_str(&format!("  - btrfs device errors on {name}\n"));
                }
                AlertCause::MissingDevice { devid } => {
                    let name = devid_to_name(devid_names, *devid);
                    out.push_str(&format!("  - missing device: {name}\n"));
                }
                AlertCause::SmartdAlert => {
                    out.push_str("  - SMART health warning\n");
                }
                AlertCause::ComputationError { detail } => {
                    out.push_str(&format!("  - alert computation error: {detail}\n"));
                }
            }
        }
        out.push('\n');
    }

    for advisory in &report.advisories {
        out.push_str(&format!("warning: {advisory}\n"));
    }

    out.push_str(&format!("Pool:     {}\n", report.mount_point));
    out.push_str(&format!(
        "Status:   {}\n",
        report
            .status
            .display_human(report.missing_count.unwrap_or(0))
    ));

    if report.status == StatusCode::NotMounted {
        return out;
    }

    if let Some(ref alloc) = report.allocation
        && !alloc.is_empty()
    {
        out.push_str("Allocation:\n");
        out.push_str("  Type       Profile  Used        Allocated\n");
        for a in alloc {
            out.push_str(&format!(
                "  {:<9}  {:<7}  {:<10}  {}\n",
                a.bg_type,
                a.profile,
                format_bytes(a.used_bytes),
                format_bytes(a.allocated_bytes),
            ));
        }
    }

    if let Some(ref balance) = report.balance {
        match balance {
            BalanceReport::Running {
                done_chunks,
                estimated_total_chunks,
                pct_left,
                ..
            } => {
                out.push_str(&format!(
                    "Balance:  running, {done_chunks}/{estimated_total_chunks} chunks ({}% complete)\n",
                    100u8.saturating_sub(*pct_left),
                ));
            }
            BalanceReport::Paused {
                done_chunks,
                estimated_total_chunks,
                pct_left,
                ..
            } => {
                out.push_str(&format!(
                    "Balance:  paused, {done_chunks}/{estimated_total_chunks} chunks ({}% complete)\n",
                    100u8.saturating_sub(*pct_left),
                ));
            }
            BalanceReport::Unknown => {
                out.push_str("Balance:  unknown\n");
            }
            BalanceReport::Idle => {}
        }
    }

    // Compact drive listing
    if let Some(drives) = compact_drives {
        out.push_str("Drives:\n");
        for d in drives {
            let devid_str = d
                .devid
                .map(|id| format!("devid={id}"))
                .unwrap_or_else(|| "-".to_owned());
            out.push_str(&format!(
                "  {:<12} {:<4} {:<8} {}\n",
                d.name, d.device_short, devid_str, d.status
            ));
        }
    }

    if let Some(ref cap) = report.capacity {
        out.push('\n');
        out.push_str("Capacity:\n");
        if let Some(total) = cap.total_bytes {
            out.push_str(&format!("  Total:  {} (Estimated)\n", format_bytes(total)));
        }
        out.push_str(&format!("  Used:   {}\n", format_bytes(cap.used_bytes)));
        out.push_str(&format!("  Free:   {}\n", format_bytes(cap.free_bytes)));
    }

    if let Some(ref scrub) = report.last_scrub {
        let line = match scrub {
            ScrubReport::Never => "never".to_owned(),
            ScrubReport::Running { pct } => match pct {
                Some(p) => format!("running ({p}%)"),
                None => "running".to_owned(),
            },
            ScrubReport::Finished {
                started_at,
                error_count,
            } => {
                if *error_count == 0 {
                    format!("{started_at} (no errors)")
                } else {
                    format!("{started_at} ({error_count} errors)")
                }
            }
            ScrubReport::Aborted { started_at, .. } => {
                format!("{started_at} cancelled (will resume)")
            }
            ScrubReport::Interrupted { started_at, .. } => {
                format!("{started_at} interrupted")
            }
            ScrubReport::Unknown => "unknown".to_owned(),
        };
        out.push_str(&format!("\nLast scrub: {line}\n"));
    }

    // Verbose: per-disk section
    if let Some(disks) = human_disks {
        out.push_str("\nDisks:\n");
        for d in disks {
            out.push('\n');
            // show disk name
            match d.status {
                DiskStatus::Missing => {
                    out.push_str(&format!("  {:<18}MISSING\n", d.name));
                }
                DiskStatus::Unknown => {
                    out.push_str(&format!("  {:<18}UNKNOWN\n", d.name));
                }
                DiskStatus::LuksHeaderUnreadable => {
                    out.push_str(&format!("  {:<18}LUKS HEADER UNREADABLE\n", d.name));
                }
                DiskStatus::LuksHeaderDamaged => {
                    out.push_str(&format!("  {:<18}LUKS HEADER DAMAGED\n", d.name));
                }
                DiskStatus::Present => {
                    let devid_str = d
                        .devid
                        .as_deref()
                        .map(|id| format!("devid {id}"))
                        .unwrap_or_default();
                    out.push_str(&format!("  {:<18}{:<10}{}\n", d.name, devid_str, d.status));
                }
            }

            // Device path
            if d.status == DiskStatus::Missing {
                out.push_str(&format!("    Device:  {}  (not found)\n", d.by_id));
            } else {
                out.push_str(&format!("    Device:  {}\n", d.by_id));
            }

            // Model/Serial (present only)
            if d.status == DiskStatus::Present {
                let model_str = d.model.as_deref().unwrap_or("(unknown)");
                let serial_str = d.serial.as_deref().unwrap_or("(unknown)");
                out.push_str(&format!("    Model:   {model_str}\n"));
                out.push_str(&format!("    Serial:  {serial_str}\n"));
            }

            // LUKS UUID
            if !d.luks_uuid.is_empty() {
                out.push_str(&format!("    LUKS:    {}\n", d.luks_uuid));
            }

            // Errors
            let has_errors = match &d.errors {
                Some(e) => {
                    out.push_str(&format!(
                        "    Errors:  read {} / write {} / flush {} / corruption {} / generation {}\n",
                        e.read, e.write, e.flush, e.corruption, e.generation
                    ));
                    e.read + e.write + e.flush + e.corruption + e.generation > 0
                }
                None if d.status == DiskStatus::Missing => {
                    out.push_str("    Errors:  unknown (device absent)\n");
                    false
                }
                None if d.status == DiskStatus::LuksHeaderUnreadable => {
                    out.push_str("    Errors:  unknown (LUKS header unreadable)\n");
                    false
                }
                None if d.status == DiskStatus::LuksHeaderDamaged => {
                    out.push_str("    Errors:  unknown (LUKS header damaged)\n");
                    false
                }
                None if d.status == DiskStatus::Unknown => {
                    out.push_str("    Errors:  unknown (metadata unavailable)\n");
                    false
                }
                None => false,
            };

            // Action guidance
            let needs_doctor = matches!(
                d.status,
                DiskStatus::LuksHeaderUnreadable | DiskStatus::LuksHeaderDamaged
            );
            if has_errors || d.status == DiskStatus::Missing {
                match &d.member_name {
                    Some(name) => out.push_str(&format!(
                        "    Action:  add replacement disk to config, then: braid replace --old {name} --new <new-name>\n",
                    )),
                    None => out.push_str(
                        "    Action:  foreign mapper detected -- run 'braid doctor' to investigate\n",
                    ),
                }
            } else if needs_doctor {
                out.push_str("    Action:  run 'braid doctor' for recovery guidance\n");
            }
        }
    }

    out
}

pub use crate::confirm::format_bytes;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::membership::{DiskMember, PoolMembership};
    // Keep the err_raw alias to document that status reuses mount's raw
    // error factory through the test fixture facade.
    use crate::test_fixtures::{disk_member_with, test_uuid};
    use crate::test_fixtures::{
        err_raw as status_err_raw, isolated_paths, mock_ok, status_btrfs_device_stats_3disk,
        status_btrfs_device_usage_raw_1disk, status_btrfs_df_raid1, status_btrfs_df_single,
        status_btrfs_scrub_aborted, status_btrfs_scrub_finished,
        status_btrfs_scrub_finished_with_errors, status_btrfs_scrub_interrupted,
        status_btrfs_scrub_never, status_btrfs_show_1disk, status_btrfs_show_3disk_1missing,
        status_btrfs_show_3disk_1null_underlying_1missing, status_btrfs_show_3disk_missing_devid3,
        status_btrfs_usage_raw, status_cfg_present_not_luks, status_config,
        status_cryptsetup_status_active, status_cryptsetup_uuid_ok, status_disk_report_missing,
        status_disk_report_named, status_fs_ext4, status_fs_mounted, status_fs_not_mounted,
        status_fs_one_disk, status_fs_three_disk, status_is_luks_raw, status_luks_dump_text_raw,
        status_membership_1disk, status_mp, status_pool_empty, status_report_with_alerts,
        status_report_with_scrub, status_runner_healthy_3disk_base,
        status_runner_healthy_3disk_verbose,
    };

    fn membership_from(entries: Vec<(LuksUuid, DiskMember)>) -> PoolMembership {
        let mut membership = PoolMembership::empty();
        for (uuid, member) in entries {
            membership.insert(uuid, member).expect("insert test member");
        }
        membership
    }
    // =======================================================================
    // Schema envelope tests
    // =======================================================================

    #[test]
    fn status_json_not_mounted() {
        let runner = MockRunner::default();
        let fs = status_fs_not_mounted(&[]);
        let config = status_config();

        let code = StatusCode::NotMounted;
        let report = StatusReport {
            mount_point: config.mount_point().clone(),
            status: code,
            total_devices: None,
            present_count: None,
            missing_count: None,
            profile: None,
            capacity: None,
            last_scrub: None,
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };

        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let obj = v.as_object().unwrap();

        // Must exist
        assert_eq!(obj["mount_point"], "/mnt/storage");
        assert_eq!(obj["status"], "not_mounted");
        assert_eq!(obj["disks"], serde_json::json!([]));
        assert_eq!(obj["alert_active"], false);

        // Must NOT exist
        assert!(!obj.contains_key("total_devices"));
        assert!(!obj.contains_key("present_count"));
        assert!(!obj.contains_key("missing_count"));
        assert!(!obj.contains_key("profile"));
        assert!(!obj.contains_key("capacity"));
        assert!(!obj.contains_key("last_scrub"));
        assert!(!obj.contains_key("allocation"));

        // Lock envelope: exactly 4 keys
        assert_eq!(
            obj.len(),
            4,
            "envelope should have exactly 4 keys, got: {obj:?}"
        );

        // Also verify cmd_status doesn't error
        let (_tmp, paths) = isolated_paths();
        let _ = cmd_status(
            &runner,
            &fs,
            &config,
            false,
            &paths,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        );
    }

    /*
     * Intent: not_mounted_status produces the canonical minimal JSON envelope
     *   for offline `braid status` -- exactly four keys, no leakage of
     *   mounted-only fields. Any drift (e.g. accidentally setting an Option
     *   to Some(default), or adding a field that should be skipped) trips
     *   this test.
     * Why it exists: status_json_not_mounted hand-builds a StatusReport and
     *   does not exercise the helper; the helper is the production source
     *   of truth for the offline envelope and needs its own pin.
     * Scenario: pool offline (no btrfs at the mount point), no advisories.
     *   `braid status --json` must serialize to { mount_point, status,
     *   disks, alert_active } and nothing else.
     */
    #[test]
    fn not_mounted_status_envelope_is_minimal() {
        let (_tmp, paths) = isolated_paths();
        let config = status_config();

        let built = not_mounted_status(&config, &paths, vec![]);
        let v: serde_json::Value =
            serde_json::from_str(&serde_json::to_string(&built.report).unwrap()).unwrap();
        let obj = v.as_object().unwrap();

        assert_eq!(
            obj.len(),
            4,
            "envelope should have exactly 4 keys, got: {obj:?}"
        );
        assert_eq!(obj["mount_point"], "/mnt/storage");
        assert_eq!(obj["status"], "not_mounted");
        assert_eq!(obj["disks"], serde_json::json!([]));
        assert_eq!(obj["alert_active"], false);
        assert!(built.mounted_extras.is_none());
    }

    #[test]
    fn status_json_healthy() {
        let runner = status_runner_healthy_3disk_base();
        let config = status_config();

        let df = fetch_df(&runner, &status_mp()).unwrap();
        let df_summary = summarize_df(&df);
        let capacity = get_capacity(&runner, &status_mp(), 0, &df).unwrap();
        let last_scrub = get_scrub_report(&runner, &status_mp());

        let code = StatusCode::Intact;
        let report = StatusReport {
            mount_point: config.mount_point().clone(),
            status: code,
            total_devices: Some(3),
            present_count: Some(3),
            missing_count: Some(0),
            profile: Some(df_summary.profile),
            capacity: Some(capacity),
            last_scrub: Some(last_scrub),
            balance: None,
            allocation: Some(df_summary.allocation),
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };

        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let obj = v.as_object().unwrap();

        assert_eq!(obj["status"], "intact");
        assert_eq!(obj["total_devices"], 3);
        assert_eq!(obj["present_count"], 3);
        assert_eq!(obj["missing_count"], 0);
        assert_eq!(obj["profile"], "RAID1");
        assert!(obj.contains_key("capacity"));
        assert!(obj.contains_key("last_scrub"));
        assert_eq!(obj["disks"], serde_json::json!([]));

        // allocation array: Data, Metadata, System (sorted by BtrfsBgType ord, GlobalReserve filtered)
        let alloc = obj["allocation"].as_array().unwrap();
        assert_eq!(alloc.len(), 3);
        assert_eq!(alloc[0]["bg_type"], "Data");
        assert_eq!(alloc[0]["profile"], "RAID1");
        assert_eq!(alloc[0]["used_bytes"], 16777216);
        assert_eq!(alloc[0]["allocated_bytes"], 67108864);
        assert_eq!(alloc[1]["bg_type"], "Metadata");
        assert_eq!(alloc[2]["bg_type"], "System");

        // missing_devids omitted when empty (skip_serializing_if)
        assert!(
            !obj.contains_key("missing_devids"),
            "missing_devids should be omitted when empty"
        );
    }

    #[test]
    fn status_json_degraded() {
        let code = StatusCode::Degraded;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(3),
            present_count: Some(2),
            missing_count: Some(1),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: None,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![3],
        };

        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let obj = v.as_object().unwrap();

        assert_eq!(obj["status"], "degraded");
        assert_eq!(obj["missing_devids"], serde_json::json!([3]));
    }

    #[test]
    fn status_json_verbose_disks() {
        let present = DiskReport {
            name: "disk1".to_owned(),
            mapper: "disk1".to_owned(),
            by_id: "/dev/disk/by-id/disk1".to_owned(),
            luks_uuid: "11111111-1111-1111-1111-111111111111".to_owned(),
            devid: Some("1".to_owned()),
            underlying: Some("/dev/vda".to_owned()),
            status: DiskStatus::Present,
            errors: Some(DiskErrors {
                read: 0,
                write: 0,
                flush: 0,
                corruption: 0,
                generation: 0,
            }),
        };
        let missing = DiskReport {
            name: "disk3".to_owned(),
            mapper: "disk3".to_owned(),
            by_id: "/dev/disk/by-id/disk3".to_owned(),
            luks_uuid: String::new(),
            devid: None,
            underlying: None,
            status: DiskStatus::Missing,
            errors: None,
        };
        let unreadable = DiskReport {
            name: "disk4".to_owned(),
            mapper: "disk4".to_owned(),
            by_id: "/dev/disk/by-id/disk4".to_owned(),
            luks_uuid: String::new(),
            devid: None,
            underlying: None,
            status: DiskStatus::LuksHeaderUnreadable,
            errors: None,
        };
        let damaged = DiskReport {
            name: "disk5".to_owned(),
            mapper: "disk5".to_owned(),
            by_id: "/dev/disk/by-id/disk5".to_owned(),
            luks_uuid: String::new(),
            devid: None,
            underlying: None,
            status: DiskStatus::LuksHeaderDamaged,
            errors: None,
        };

        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: StatusCode::Degraded,
            total_devices: Some(2),
            present_count: Some(1),
            missing_count: Some(1),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: None,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: vec![present, missing, unreadable, damaged],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };

        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let disks = v["disks"].as_array().unwrap();
        assert_eq!(disks.len(), 4);

        // Present disk
        let d0 = &disks[0];
        assert_eq!(d0["mapper"], "disk1");
        assert_eq!(d0["by_id"], "/dev/disk/by-id/disk1");
        assert_eq!(d0["luks_uuid"], "11111111-1111-1111-1111-111111111111");
        assert_eq!(d0["devid"], "1");
        assert_eq!(d0["status"], "present");
        assert!(d0["errors"].is_object());
        assert_eq!(d0["errors"]["read"], 0);
        assert_eq!(d0["errors"]["write"], 0);
        assert_eq!(d0["errors"]["corruption"], 0);

        // Missing disk
        let d1 = &disks[1];
        assert_eq!(d1["mapper"], "disk3");
        assert_eq!(d1["status"], "missing");
        assert!(d1["errors"].is_null());
        assert!(d1["devid"].is_null());

        // LUKS header unreadable disk
        let d2 = &disks[2];
        assert_eq!(d2["mapper"], "disk4");
        assert_eq!(d2["status"], "luks-header-unreadable");
        assert!(d2["errors"].is_null());

        // LUKS header damaged disk
        let d3 = &disks[3];
        assert_eq!(d3["mapper"], "disk5");
        assert_eq!(d3["status"], "luks-header-damaged");
        assert!(d3["errors"].is_null());
    }

    // =======================================================================
    // disks always-array tests
    // =======================================================================

    #[test]
    fn status_json_disks_always_array_not_mounted() {
        let code = StatusCode::NotMounted;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: None,
            present_count: None,
            missing_count: None,
            profile: None,
            capacity: None,
            last_scrub: None,
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(v["disks"].is_array());
        assert_eq!(v["disks"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn status_json_disks_always_array_empty() {
        let code = StatusCode::Intact;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(3),
            present_count: Some(3),
            missing_count: Some(0),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: Some(1040187392),
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(v["disks"].is_array());
        assert_eq!(v["disks"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn status_json_disks_always_array_verbose() {
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: StatusCode::Intact,
            total_devices: Some(1),
            present_count: Some(1),
            missing_count: Some(0),
            profile: Some("single".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: Some(1073741824),
                used_bytes: 536870912,
                free_bytes: 536870912,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: vec![DiskReport {
                name: "disk1".to_owned(),
                mapper: "disk1".to_owned(),
                by_id: "/dev/disk/by-id/disk1".to_owned(),
                luks_uuid: "11111111-1111-1111-1111-111111111111".to_owned(),
                devid: Some("1".to_owned()),
                underlying: Some("/dev/vda".to_owned()),
                status: DiskStatus::Present,
                errors: Some(DiskErrors {
                    read: 0,
                    write: 0,
                    flush: 0,
                    corruption: 0,
                    generation: 0,
                }),
            }],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(v["disks"].is_array());
        assert!(!v["disks"].as_array().unwrap().is_empty());
    }

    // =======================================================================
    // Human output tests
    // =======================================================================

    #[test]
    fn status_human_not_mounted() {
        let code = StatusCode::NotMounted;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: None,
            present_count: None,
            missing_count: None,
            profile: None,
            capacity: None,
            last_scrub: None,
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let human = format_status_human(&report, None, None, None);
        assert!(human.contains("not mounted"), "got:\n{human}");
        assert!(!human.contains("Capacity"), "got:\n{human}");
        assert!(!human.contains("Allocation:"), "got:\n{human}");
    }

    #[test]
    fn status_human_healthy_single() {
        let code = StatusCode::Intact;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(1),
            present_count: Some(1),
            missing_count: Some(0),
            profile: Some("single".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: Some(1073741824),
                used_bytes: 536870912,
                free_bytes: 536870912,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: Some(vec![
                AllocationEntry {
                    bg_type: "Data".to_owned(),
                    profile: "single".to_owned(),
                    used_bytes: 536870912,
                    allocated_bytes: 1073741824,
                },
                AllocationEntry {
                    bg_type: "Metadata".to_owned(),
                    profile: "single".to_owned(),
                    used_bytes: 65536,
                    allocated_bytes: 268435456,
                },
                AllocationEntry {
                    bg_type: "System".to_owned(),
                    profile: "single".to_owned(),
                    used_bytes: 16384,
                    allocated_bytes: 4194304,
                },
            ]),
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let compact = vec![CompactDrive {
            name: "disk1".to_owned(),
            device_short: "vda".to_owned(),
            devid: Some(1),
            status: DiskStatus::Present,
        }];
        let human = format_status_human(&report, Some(&compact), None, None);
        assert!(human.contains("intact"), "got:\n{human}");
        assert!(human.contains("Drives:"), "got:\n{human}");
        assert!(human.contains("disk1"), "got:\n{human}");
        assert!(human.contains("vda"), "got:\n{human}");
        assert!(human.contains("present"), "got:\n{human}");
        assert!(human.contains("Allocation:"), "got:\n{human}");
        assert!(human.contains("single"), "got:\n{human}");
        assert!(human.contains("Total:"), "got:\n{human}");
        assert!(human.contains("Used:"), "got:\n{human}");
        assert!(human.contains("Free:"), "got:\n{human}");
        assert!(!human.contains("RAID1"), "got:\n{human}");
        assert!(!human.contains("missing"), "got:\n{human}");
        assert!(!human.contains("Profile:"), "got:\n{human}");
    }

    #[test]
    fn status_human_healthy_raid1() {
        let code = StatusCode::Intact;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(3),
            present_count: Some(3),
            missing_count: Some(0),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: Some(1040187392),
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: Some(vec![
                AllocationEntry {
                    bg_type: "Data".to_owned(),
                    profile: "RAID1".to_owned(),
                    used_bytes: 16777216,
                    allocated_bytes: 67108864,
                },
                AllocationEntry {
                    bg_type: "Metadata".to_owned(),
                    profile: "RAID1".to_owned(),
                    used_bytes: 65536,
                    allocated_bytes: 33554432,
                },
                AllocationEntry {
                    bg_type: "System".to_owned(),
                    profile: "RAID1".to_owned(),
                    used_bytes: 16384,
                    allocated_bytes: 4194304,
                },
            ]),
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let compact = vec![
            CompactDrive {
                name: "disk1".into(),
                device_short: "vda".into(),
                devid: Some(1),
                status: DiskStatus::Present,
            },
            CompactDrive {
                name: "disk2".into(),
                device_short: "vdb".into(),
                devid: Some(2),
                status: DiskStatus::Present,
            },
            CompactDrive {
                name: "disk3".into(),
                device_short: "vdc".into(),
                devid: Some(3),
                status: DiskStatus::Present,
            },
        ];
        let human = format_status_human(&report, Some(&compact), None, None);
        assert!(human.contains("intact"), "got:\n{human}");
        assert!(human.contains("Drives:"), "got:\n{human}");
        assert!(human.contains("disk1"), "got:\n{human}");
        assert!(human.contains("Allocation:"), "got:\n{human}");
        assert!(human.contains("RAID1"), "got:\n{human}");
        assert!(human.contains("Total:"), "got:\n{human}");
        assert!(human.contains("scrub"), "got:\n{human}");
        assert!(!human.contains("missing"), "got:\n{human}");
    }

    #[test]
    fn status_human_degraded() {
        let code = StatusCode::Degraded;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(3),
            present_count: Some(2),
            missing_count: Some(1),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: None,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let compact = vec![
            CompactDrive {
                name: "disk1".into(),
                device_short: "vda".into(),
                devid: Some(1),
                status: DiskStatus::Present,
            },
            CompactDrive {
                name: "disk2".into(),
                device_short: "vdb".into(),
                devid: Some(2),
                status: DiskStatus::Present,
            },
            CompactDrive {
                name: "disk3".into(),
                device_short: "-".into(),
                devid: None,
                status: DiskStatus::Missing,
            },
        ];
        let human = format_status_human(&report, Some(&compact), None, None);
        assert!(
            human.contains("DEGRADED (1 missing device)"),
            "got:\n{human}"
        );
        assert!(human.contains("missing"), "got:\n{human}");
        assert!(human.contains("disk3"), "got:\n{human}");
    }

    #[test]
    fn status_human_degraded_plural() {
        let code = StatusCode::Degraded;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(4),
            present_count: Some(2),
            missing_count: Some(2),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: None,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let human = format_status_human(&report, None, None, None);
        assert!(
            human.contains("DEGRADED (2 missing devices)"),
            "got:\n{human}"
        );
    }

    // =======================================================================
    // Verbose human tests
    // =======================================================================

    #[test]
    fn status_verbose_present_disks() {
        let human_disks = vec![HumanDisk {
            name: "disk1".to_owned(),
            member_name: Some(DiskName::parse("disk1").unwrap()),
            by_id: "/dev/disk/by-id/disk1".to_owned(),
            luks_uuid: "11111111-1111-1111-1111-111111111111".to_owned(),
            devid: Some("1".to_owned()),
            status: DiskStatus::Present,
            model: Some("VBOX HARDDISK".to_owned()),
            serial: Some("disk1".to_owned()),
            errors: Some(DiskErrors {
                read: 0,
                write: 0,
                flush: 0,
                corruption: 0,
                generation: 0,
            }),
        }];

        let code = StatusCode::Intact;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(1),
            present_count: Some(1),
            missing_count: Some(0),
            profile: Some("single".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: Some(1073741824),
                used_bytes: 536870912,
                free_bytes: 536870912,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };

        let human = format_status_human(&report, None, Some(&human_disks), None);
        assert!(human.contains("present"), "got:\n{human}");
        assert!(human.contains("devid 1"), "got:\n{human}");
        assert!(human.contains("LUKS:"), "got:\n{human}");
        assert!(human.contains("Errors:"), "got:\n{human}");
        assert!(human.contains("Model:"), "got:\n{human}");
        assert!(human.contains("Serial:"), "got:\n{human}");
    }

    #[test]
    fn status_verbose_missing_disk() {
        let human_disks = vec![HumanDisk {
            name: "disk3".to_owned(),
            member_name: Some(DiskName::parse("disk3").unwrap()),
            by_id: "/dev/disk/by-id/disk3".to_owned(),
            luks_uuid: String::new(),
            devid: None,
            status: DiskStatus::Missing,
            model: None,
            serial: None,
            errors: None,
        }];

        let code = StatusCode::Degraded;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(2),
            present_count: Some(1),
            missing_count: Some(1),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: None,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };

        let human = format_status_human(&report, None, Some(&human_disks), None);
        assert!(human.contains("MISSING"), "got:\n{human}");
        assert!(human.contains("not found"), "got:\n{human}");
        assert!(human.contains("device absent"), "got:\n{human}");
    }

    /// Intent: a disk classified as `LuksHeaderUnreadable` in the verbose
    /// human output must show the dedicated label, the LUKS-header-specific
    /// errors line, and the doctor action guidance.
    ///
    /// Why: previously every unpooled non-LUKS disk collapsed into the
    /// generic `unknown (metadata unavailable)` bucket, so users had no
    /// signal that recovery from a header backup might be possible.
    ///
    /// Scenario: declared pool member whose `cryptsetup isLuks` fails
    /// (e.g. fully zeroed header). status.rs refines `PresentNotLuks` into
    /// `LuksHeaderUnreadable` and the human output renders the new label.
    #[test]
    fn status_verbose_luks_header_unreadable_disk() {
        let human_disks = vec![HumanDisk {
            name: "disk4".to_owned(),
            member_name: Some(DiskName::parse("disk4").unwrap()),
            by_id: "/dev/disk/by-id/disk4".to_owned(),
            luks_uuid: String::new(),
            devid: None,
            status: DiskStatus::LuksHeaderUnreadable,
            model: None,
            serial: None,
            errors: None,
        }];

        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: StatusCode::Degraded,
            total_devices: Some(2),
            present_count: Some(1),
            missing_count: Some(1),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: None,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };

        let human = format_status_human(&report, None, Some(&human_disks), None);
        assert!(human.contains("LUKS HEADER UNREADABLE"), "got:\n{human}");
        assert!(
            human.contains("unknown (LUKS header unreadable)"),
            "got:\n{human}"
        );
        assert!(human.contains("braid doctor"), "got:\n{human}");
        // Must NOT use the destructive `replace` action — Damaged or
        // Unreadable headers may be recoverable.
        assert!(
            !human.contains("braid replace"),
            "header-state disks must not surface a replace action; got:\n{human}"
        );
    }

    /// Intent: a disk classified as `LuksHeaderDamaged` in the verbose human
    /// output must show its dedicated label, the damaged-specific errors
    /// line, and the doctor action guidance.
    ///
    /// Why: damaged metadata is potentially repairable via
    /// `cryptsetup repair`, which has a different recovery story than
    /// header restoration. Status output must signal the distinction so
    /// users do not skip straight to a destructive replace.
    ///
    /// Scenario: declared pool member whose `isLuks` succeeds but
    /// `luksDump` fails. status.rs refines `PresentNotLuks` into
    /// `LuksHeaderDamaged` and the human output renders the new label.
    #[test]
    fn status_verbose_luks_header_damaged_disk() {
        let human_disks = vec![HumanDisk {
            name: "disk5".to_owned(),
            member_name: Some(DiskName::parse("disk5").unwrap()),
            by_id: "/dev/disk/by-id/disk5".to_owned(),
            luks_uuid: String::new(),
            devid: None,
            status: DiskStatus::LuksHeaderDamaged,
            model: None,
            serial: None,
            errors: None,
        }];

        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: StatusCode::Degraded,
            total_devices: Some(2),
            present_count: Some(1),
            missing_count: Some(1),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: None,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };

        let human = format_status_human(&report, None, Some(&human_disks), None);
        assert!(human.contains("LUKS HEADER DAMAGED"), "got:\n{human}");
        assert!(
            human.contains("unknown (LUKS header damaged)"),
            "got:\n{human}"
        );
        assert!(human.contains("braid doctor"), "got:\n{human}");
        assert!(
            !human.contains("braid replace"),
            "header-state disks must not surface a replace action; got:\n{human}"
        );
    }

    #[test]
    fn status_verbose_lsblk_failure() {
        let human_disks = vec![HumanDisk {
            name: "disk1".to_owned(),
            member_name: Some(DiskName::parse("disk1").unwrap()),
            by_id: "/dev/disk/by-id/disk1".to_owned(),
            luks_uuid: "11111111-1111-1111-1111-111111111111".to_owned(),
            devid: Some("1".to_owned()),
            status: DiskStatus::Present,
            model: None,
            serial: None,
            errors: Some(DiskErrors {
                read: 0,
                write: 0,
                flush: 0,
                corruption: 0,
                generation: 0,
            }),
        }];

        let code = StatusCode::Intact;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(1),
            present_count: Some(1),
            missing_count: Some(0),
            profile: Some("single".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: Some(1073741824),
                used_bytes: 536870912,
                free_bytes: 536870912,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };

        let human = format_status_human(&report, None, Some(&human_disks), None);
        assert!(human.contains("(unknown)"), "got:\n{human}");
    }

    // =======================================================================
    // Error policy tests
    // =======================================================================

    #[test]
    fn status_scrub_finished() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            status_btrfs_scrub_finished(),
        );
        let result = get_scrub_report(&runner, &status_mp());
        match result {
            ScrubReport::Finished {
                started_at,
                error_count,
            } => {
                assert!(started_at.contains("Mon Feb 23"), "got: {started_at}");
                assert_eq!(error_count, 0);
            }
            other => panic!("expected Finished, got: {other:?}"),
        }
    }

    #[test]
    fn status_scrub_finished_with_errors() {
        // Intent: verify that scrub error counts survive the get_scrub_report path.
        // Why it exists: get_scrub_string previously discarded error_count via `..`,
        // making a scrub with 50 errors look identical to a clean scrub.
        // Scenario: btrfs scrub finishes with csum=50 — the report must carry that count.
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            status_btrfs_scrub_finished_with_errors(),
        );
        let result = get_scrub_report(&runner, &status_mp());
        match result {
            ScrubReport::Finished {
                started_at,
                error_count,
            } => {
                assert!(started_at.contains("Mon Feb 23"), "got: {started_at}");
                assert_eq!(error_count, 50);
            }
            other => panic!("expected Finished, got: {other:?}"),
        }
    }

    #[test]
    fn status_scrub_aborted() {
        // Intent: verify cancelled scrub status is reported as resumable, not finished.
        // Why it exists: cancelled btrfs scrub state is normal after lock/shutdown.
        // Scenario: braid status runs after a scrub was cancelled during lock.
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            status_btrfs_scrub_aborted(),
        );
        let result = get_scrub_report(&runner, &status_mp());
        match result {
            ScrubReport::Aborted {
                started_at,
                error_count,
            } => {
                assert!(started_at.contains("Mon Feb 23"), "got: {started_at}");
                assert_eq!(error_count, 0);
            }
            other => panic!("expected Aborted, got: {other:?}"),
        }
    }

    #[test]
    fn status_scrub_interrupted() {
        // Intent: verify interrupted scrub status is reported distinctly.
        // Why it exists: process death is different from clean completion.
        // Scenario: braid status runs after a scrub userspace process was killed.
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            status_btrfs_scrub_interrupted(),
        );
        let result = get_scrub_report(&runner, &status_mp());
        match result {
            ScrubReport::Interrupted {
                started_at,
                error_count,
            } => {
                assert!(started_at.contains("Mon Feb 23"), "got: {started_at}");
                assert_eq!(error_count, 0);
            }
            other => panic!("expected Interrupted, got: {other:?}"),
        }
    }

    #[test]
    fn status_scrub_failure_tolerant() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            status_err_raw("btrfs scrub status", 1, "some error"),
        );
        let result = get_scrub_report(&runner, &status_mp());
        assert_eq!(result, ScrubReport::Unknown);
    }

    #[test]
    fn scrub_report_json_finished() {
        // Intent: verify the JSON shape of ScrubReport::Finished.
        // Why it exists: the old last_scrub was a flat string — we need to ensure
        // the new tagged enum serializes to the expected object shape.
        // Scenario: JSON consumers (scripts, monitoring) parse the last_scrub field.
        let report = ScrubReport::Finished {
            started_at: "Mon Feb 23 10:00:00 2026".to_owned(),
            error_count: 3,
        };
        let json: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(json["state"], "finished");
        assert_eq!(json["started_at"], "Mon Feb 23 10:00:00 2026");
        assert_eq!(json["error_count"], 3);
    }

    #[test]
    fn scrub_report_json_aborted() {
        // Intent: verify JSON shape of ScrubReport::Aborted.
        // Why it exists: JSON consumers must be able to distinguish resumable
        // cancellation from clean completion.
        // Scenario: monitoring reads last_scrub after lock cancelled a scrub.
        let report = ScrubReport::Aborted {
            started_at: "Mon Feb 23 10:00:00 2026".to_owned(),
            error_count: 0,
        };
        let json: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(json["state"], "aborted");
        assert_eq!(json["started_at"], "Mon Feb 23 10:00:00 2026");
        assert_eq!(json["error_count"], 0);
    }

    #[test]
    fn scrub_report_json_interrupted() {
        // Intent: verify JSON shape of ScrubReport::Interrupted.
        // Why it exists: JSON consumers must not mistake interrupted for finished.
        // Scenario: monitoring reads last_scrub after a power loss mid-scrub.
        let report = ScrubReport::Interrupted {
            started_at: "Mon Feb 23 10:00:00 2026".to_owned(),
            error_count: 0,
        };
        let json: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(json["state"], "interrupted");
        assert_eq!(json["started_at"], "Mon Feb 23 10:00:00 2026");
        assert_eq!(json["error_count"], 0);
    }

    #[test]
    fn scrub_report_json_never() {
        // Intent: verify JSON shape of ScrubReport::Never.
        // Why it exists: last_scrub changed from a flat string to a tagged object;
        // each variant's serialization must be covered.
        // Scenario: pool has never been scrubbed, JSON consumers see {"state":"never"}.
        let json: serde_json::Value = serde_json::to_value(&ScrubReport::Never).unwrap();
        assert_eq!(json["state"], "never");
    }

    #[test]
    fn scrub_report_json_running_with_pct() {
        // Intent: verify JSON shape of ScrubReport::Running with progress.
        // Why it exists: last_scrub changed from a flat string to a tagged object;
        // each variant's serialization must be covered.
        // Scenario: scrub is in progress at 42%, JSON consumers see pct field.
        let report = ScrubReport::Running { pct: Some(42) };
        let json: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(json["state"], "running");
        assert_eq!(json["pct"], 42);
    }

    #[test]
    fn scrub_report_json_running_no_pct() {
        // Intent: verify pct is omitted (not null) when unavailable.
        // Why it exists: serde skip_serializing_if must actually omit the key,
        // not emit null, to match our JSON contract.
        // Scenario: scrub just started, no progress yet — pct absent from JSON.
        let report = ScrubReport::Running { pct: None };
        let json: serde_json::Value = serde_json::to_value(&report).unwrap();
        assert_eq!(json["state"], "running");
        assert!(json.get("pct").is_none(), "pct should be omitted when None");
    }

    #[test]
    fn human_scrub_shows_no_errors() {
        // Intent: verify human output includes "(no errors)" for clean scrub.
        // Why it exists: the old code showed only the timestamp with no error info.
        // Scenario: user runs `braid status` after a clean scrub.
        let report = status_report_with_scrub(ScrubReport::Finished {
            started_at: "Mon Feb 23 10:00:00 2026".to_owned(),
            error_count: 0,
        });
        let human = format_status_human(&report, None, None, None);
        assert!(
            human.contains("\nLast scrub: Mon Feb 23 10:00:00 2026 (no errors)\n"),
            "expected exact last-scrub line, got:\n{human}"
        );
    }

    #[test]
    fn human_scrub_shows_error_count() {
        // Intent: verify human output includes error count for failed scrub.
        // Why it exists: the old code showed only the timestamp — errors were invisible.
        // Scenario: user runs `braid status` after a scrub found 3 errors.
        let report = status_report_with_scrub(ScrubReport::Finished {
            started_at: "Mon Feb 23 10:00:00 2026".to_owned(),
            error_count: 3,
        });
        let human = format_status_human(&report, None, None, None);
        assert!(
            human.contains("\nLast scrub: Mon Feb 23 10:00:00 2026 (3 errors)\n"),
            "expected exact last-scrub line, got:\n{human}"
        );
    }

    #[test]
    fn human_scrub_shows_aborted() {
        // Intent: verify human output marks cancelled scrub as resumable.
        // Why it exists: the status renderer must not show cancelled as clean.
        // Scenario: user runs `braid status` after lock cancelled a scrub.
        let report = status_report_with_scrub(ScrubReport::Aborted {
            started_at: "Mon Feb 23 10:00:00 2026".to_owned(),
            error_count: 0,
        });
        let human = format_status_human(&report, None, None, None);
        assert!(
            human.contains("\nLast scrub: Mon Feb 23 10:00:00 2026 cancelled (will resume)\n"),
            "expected exact cancelled last-scrub line, got:\n{human}"
        );
    }

    #[test]
    fn human_scrub_shows_interrupted() {
        // Intent: verify human output marks interrupted scrub distinctly.
        // Why it exists: interrupted scrub status must not render as clean.
        // Scenario: user runs `braid status` after shutdown interrupted a scrub.
        let report = status_report_with_scrub(ScrubReport::Interrupted {
            started_at: "Mon Feb 23 10:00:00 2026".to_owned(),
            error_count: 0,
        });
        let human = format_status_human(&report, None, None, None);
        assert!(
            human.contains("\nLast scrub: Mon Feb 23 10:00:00 2026 interrupted\n"),
            "expected exact interrupted last-scrub line, got:\n{human}"
        );
    }

    // =======================================================================
    // Balance report tests
    // =======================================================================

    #[test]
    fn balance_report_idle() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            mock_ok(
                "btrfs balance status",
                "No balance found on '/mnt/storage'\n",
            ),
        );
        assert_eq!(
            get_balance_report(&runner, &status_mp()),
            BalanceReport::Idle
        );
    }

    #[test]
    fn balance_report_running() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs balance status".into(),
                stdout: "Balance on '/mnt/storage' is running\n\
                         3 out of about 10 chunks balanced (7 considered), 70% left\n"
                    .into(),
                stderr: String::new(),
                exit_status: 1,
            },
        );
        assert_eq!(
            get_balance_report(&runner, &status_mp()),
            BalanceReport::Running {
                done_chunks: 3,
                estimated_total_chunks: 10,
                considered_chunks: 7,
                pct_left: 70,
            }
        );
    }

    #[test]
    fn balance_report_paused() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs balance status".into(),
                stdout: "Balance on '/mnt/storage' is paused\n\
                         5 out of about 12 chunks balanced (8 considered), 58% left\n"
                    .into(),
                stderr: String::new(),
                exit_status: 1,
            },
        );
        assert_eq!(
            get_balance_report(&runner, &status_mp()),
            BalanceReport::Paused {
                done_chunks: 5,
                estimated_total_chunks: 12,
                considered_chunks: 8,
                pct_left: 58,
            }
        );
    }

    #[test]
    fn balance_report_unknown_on_cmd_error() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            status_err_raw("btrfs balance status", 2, "ERROR: not a btrfs filesystem"),
        );
        assert_eq!(
            get_balance_report(&runner, &status_mp()),
            BalanceReport::Unknown
        );
    }

    #[test]
    fn emit_paused_balance_warning_writes_to_buffer() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: "btrfs balance status".into(),
                stdout: "Balance on '/mnt/storage' is paused\n\
                         3 out of about 10 chunks balanced (7 considered), 70% left\n"
                    .into(),
                stderr: String::new(),
                exit_status: 1,
            },
        );
        let mut buf = Vec::new();
        let warned = emit_paused_balance_warning(&runner, &status_mp(), &mut buf);
        assert!(warned, "should return true for paused balance");
        let output = String::from_utf8(buf).unwrap();
        let expected = concat!(
            "\n",
            "  paused balance detected -- will not auto-resume\n",
            "    resume:  btrfs balance resume /mnt/storage\n",
            "    cancel:  btrfs balance cancel /mnt/storage\n",
        );
        assert_eq!(output, expected);
    }

    #[test]
    fn emit_paused_balance_warning_silent_when_idle() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            mock_ok(
                "btrfs balance status",
                "No balance found on '/mnt/storage'\n",
            ),
        );
        let mut buf = Vec::new();
        let warned = emit_paused_balance_warning(&runner, &status_mp(), &mut buf);
        assert!(!warned, "should return false when no balance is paused");
        assert!(buf.is_empty(), "should write nothing when idle");
    }

    #[test]
    fn balance_human_running() {
        let code = StatusCode::Intact;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(2),
            present_count: Some(2),
            missing_count: Some(0),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: Some(1040187392),
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: Some(BalanceReport::Running {
                done_chunks: 108,
                estimated_total_chunks: 160,
                considered_chunks: 120,
                pct_left: 32,
            }),
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let human = format_status_human(&report, None, None, None);
        assert!(
            human.contains("Balance:  running, 108/160 chunks (68% complete)"),
            "got:\n{human}"
        );
    }

    #[test]
    fn balance_human_unknown() {
        let code = StatusCode::Intact;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(1),
            present_count: Some(1),
            missing_count: Some(0),
            profile: Some("single".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: Some(1040187392),
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: Some(BalanceReport::Unknown),
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let human = format_status_human(&report, None, None, None);
        assert!(human.contains("Balance:  unknown"), "got:\n{human}");
    }

    #[test]
    fn balance_human_idle_no_line() {
        let code = StatusCode::Intact;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(1),
            present_count: Some(1),
            missing_count: Some(0),
            profile: Some("single".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: Some(1040187392),
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: Some(BalanceReport::Idle),
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let human = format_status_human(&report, None, None, None);
        assert!(
            !human.contains("Balance:"),
            "Idle balance should not show Balance line, got:\n{human}"
        );
    }

    #[test]
    fn status_df_failure_fatal() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemDfJson {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            status_err_raw("btrfs filesystem df", 1, "not a btrfs filesystem"),
        );
        let result = fetch_df(&runner, &status_mp());
        assert!(result.is_err());
    }

    #[test]
    fn status_usage_failure_fatal() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemUsageRaw {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            status_err_raw("btrfs filesystem usage", 1, "error"),
        );
        let df = BtrfsDfOutput { entries: vec![] };
        let result = get_capacity(&runner, &status_mp(), 0, &df);
        assert!(result.is_err());
    }

    /// Intent: CapacityReport.used_bytes must be logical (df-derived)
    /// and never exceed total_bytes.
    ///
    /// Why it exists: regression guard for the same unit mismatch as
    /// the TUI "112% pool usage" bug. `braid status` prints Used and
    /// Total on separate lines (no percent), so a raw vs logical
    /// mismatch would show Used > Total -- nonsense data, but harder
    /// to notice than a >100% percent.
    ///
    /// Scenario: 2-disk RAID1 pool. btrfs filesystem usage --raw
    /// reports aggregate raw Used = 570458112 that exceeds the
    /// estimated logical capacity. btrfs filesystem df --json reports
    /// logical used per block group; the df-sum (GlobalReserve
    /// excluded) is 285229056.
    #[test]
    fn get_capacity_raid1_used_is_logical() {
        use crate::parse::types::{BtrfsBgType, BtrfsDfEntry, BtrfsProfile};

        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw {
                    mount_point: status_mp(),
                },
                mock_ok(
                    "btrfs filesystem usage",
                    "Overall:\n\
                     \tDevice size:\t\t\t1073741824\n\
                     \tDevice allocated:\t\t620756992\n\
                     \tDevice unallocated:\t\t452984832\n\
                     \tUsed:\t\t\t\t570458112\n\
                     \tFree (estimated):\t\t251641856\t(min: 251641856)\n\
                     \tData ratio:\t\t\t2.00\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsDeviceUsageRaw {
                    mount_point: status_mp(),
                },
                mock_ok(
                    "btrfs device usage",
                    "/dev/dm-0, ID: 1\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:              0\n\
                     \x20  Unallocated:          226492416\n\
                     \n\
                     /dev/dm-1, ID: 2\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:              0\n\
                     \x20  Unallocated:          226492416\n",
                ),
            );

        let df = BtrfsDfOutput {
            entries: vec![
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::Data,
                    bg_profile: BtrfsProfile::Raid1,
                    bg_used: 268_435_456,
                    bg_total: 268_435_456,
                },
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::Metadata,
                    bg_profile: BtrfsProfile::Dup,
                    bg_used: 16_777_216,
                    bg_total: 33_554_432,
                },
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::System,
                    bg_profile: BtrfsProfile::Dup,
                    bg_used: 16_384,
                    bg_total: 8_388_608,
                },
                BtrfsDfEntry {
                    bg_type: BtrfsBgType::GlobalReserve,
                    bg_profile: BtrfsProfile::Single,
                    bg_used: 3_670_016,
                    bg_total: 3_670_016,
                },
            ],
        };

        let report = get_capacity(&runner, &status_mp(), 0, &df).unwrap();

        assert_eq!(report.total_bytes, Some(536_870_912));
        assert!(
            report.used_bytes <= report.total_bytes.unwrap(),
            "used ({}) must not exceed total ({}) -- unit mismatch?",
            report.used_bytes,
            report.total_bytes.unwrap(),
        );
        assert_eq!(report.used_bytes, 285_229_056);
    }

    #[test]
    fn status_device_stats_failure_fatal() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsDeviceStatsJson {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            status_err_raw("btrfs device stats", 1, "error"),
        );
        let result = get_device_stats(&runner, &status_mp());
        assert!(result.is_err());
    }

    // Intent: status keeps reporting `NotMounted` for a foreign-fstype
    // mount, and surfaces the actual fstype through the existing
    // `advisories` channel using `ProbeError::NotBtrfs`'s Display text.
    // Why it exists: prior behavior dropped `ProbeError::NotBtrfs`'s
    // `fstype` field, leaving operators with contradictory status and
    // unlock messages and no direct diagnosis of the foreign mount.
    // Scenario: operator left an `ext4` partition mounted at `/mnt/storage`;
    // `braid status` must still report not mounted but also show the exact
    // warning text naming the foreign filesystem.
    #[test]
    fn build_status_not_btrfs_surfaces_fstype_advisory() {
        let runner = MockRunner::default();
        let fs = status_fs_ext4(&[]);
        let config = status_config();
        let (_tmp, paths) = isolated_paths();

        let built = build_status(
            &runner,
            &fs,
            &config,
            &paths,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        )
        .expect("build_status should succeed for foreign-fstype mount");

        assert_eq!(built.report.status, StatusCode::NotMounted);
        assert_eq!(
            built.report.advisories,
            vec!["/mnt/storage is mounted but fstype is ext4, not btrfs"],
        );
        assert!(built.mounted_extras.is_none());
    }

    // =======================================================================
    // format_bytes tests
    // =======================================================================

    // =======================================================================
    // Integration-style tests (cmd_status end-to-end with mocks)
    // =======================================================================

    #[test]
    fn cmd_status_not_mounted_ok() {
        let runner = MockRunner::default();
        let fs = status_fs_not_mounted(&[]);
        let config = status_config();

        let (_tmp, paths) = isolated_paths();
        let result = cmd_status(
            &runner,
            &fs,
            &config,
            false,
            &paths,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        );
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    #[test]
    fn cmd_status_healthy_ok() {
        let runner = status_runner_healthy_3disk_verbose(status_runner_healthy_3disk_base());
        let fs = status_fs_three_disk();
        let config = status_config();

        let (_tmp, paths) = isolated_paths();
        let result = cmd_status(
            &runner,
            &fs,
            &config,
            false,
            &paths,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_healthy_json_ok() {
        let runner = status_runner_healthy_3disk_verbose(status_runner_healthy_3disk_base());
        let fs = status_fs_three_disk();
        let config = status_config();

        let (_tmp, paths) = isolated_paths();
        let result = cmd_status(
            &runner,
            &fs,
            &config,
            true,
            &paths,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        );
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_degraded_ok() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_show_3disk_1missing(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName("disk1".into()),
                },
                status_cryptsetup_status_active("disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                status_cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName("disk2".into()),
                },
                status_cryptsetup_status_active("disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                status_cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_df_raid1(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_usage_raw(),
            )
            .with_output(
                CmdRequest::BtrfsScrubStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_scrub_never(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceStatsJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_device_stats_3disk(),
            )
            // probe_config_disk for each config disk (by-id path)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk1".into(),
                },
                status_cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk2".into(),
                },
                status_cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk3".into(),
                },
                status_cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk3",
                    "33333333-3333-3333-3333-333333333333",
                ),
            );
        let fs = status_fs_three_disk();
        let config = status_config();

        let (_tmp, paths) = isolated_paths();
        let result = cmd_status(
            &runner,
            &fs,
            &config,
            false,
            &paths,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        );
        assert!(result.is_ok());
    }

    /*
     * Intent: StatusReport.missing_devids enumerates every devid that
     * contributes to missing_count -- the union of btrfs's authoritative
     * MISSING set and null-underlying devids whose LUKS mapper is open but
     * whose backing block device is gone.
     *
     * Why it exists: missing_count counts null-underlying drives (probe.rs
     * derives it from total - devices.len(), and the null-underlying loop
     * branch skips pushing into devices). Without the union, JSON consumers can
     * see mutually inconsistent fields where missing_count includes a
     * hot-unplugged drive but missing_devids does not. Per
     * docs/tool-behavior/device-disappearance.md, null-underlying is the
     * empirical first state after a SATA hot-unplug, so this is the common
     * case, not a corner case.
     *
     * The mixed-scenario assertion is also load-bearing: a test that covered
     * only null-underlying could pass against an implementation that replaced
     * missing_devids with null_underlying rather than unioning them. Including
     * both contributors at once pins the union contract.
     *
     * Scenario: 3-device pool.
     *   - Devid 1: healthy mapper.
     *   - Devid 2: null-underlying -- mapper still appears in btrfs filesystem
     *     show, but cryptsetup status reports device: (null).
     *   - Devid 3: btrfs MISSING placeholder line in btrfs filesystem show
     *     (path MISSING); cryptsetup is never queried for it.
     * The StatusReport produced via the production assembly path must report
     * missing_count == 2 and missing_devids containing both 2 and 3, deduped
     * and sorted by alert_missing_devids.
     */
    #[test]
    fn build_status_missing_devids_unions_btrfs_missing_and_null_underlying() {
        let runner = status_runner_healthy_3disk_base()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_show_3disk_1null_underlying_1missing(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName("disk2".into()),
                },
                status_cryptsetup_status_active("disk2", "(null)"),
            );
        let fs = status_fs_three_disk();
        let config = status_config();

        let (_tmp, paths) = isolated_paths();
        membership::save_membership(&PoolMembership::empty(), &paths).unwrap();

        let built = build_status(
            &runner,
            &fs,
            &config,
            &paths,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        )
        .unwrap();

        assert_eq!(built.report.missing_count, Some(2));
        assert_eq!(built.report.missing_devids, vec![2, 3]);
        assert_eq!(
            built.report.missing_devids.len(),
            built.report.missing_count.unwrap() as usize
        );
    }

    #[test]
    fn build_status_missing_device_banner_and_compact_row_name_member_end_to_end() {
        // Intent: exercise the full mounted-status assembly and human
        // formatter path for a btrfs-MISSING member whose only name join is
        // persisted membership devid.
        // Why it exists: the banner and compact `Drives:` list are plumbed
        // separately, so builder-only tests could pass while the user-facing
        // output still showed bare `devid 3` or `-`.
        // Scenario: disk3 is absent, btrfs reports devid 3 as MISSING, and the
        // alert latch contains `MissingDevice { devid: 3 }`.
        let (_, member1) = disk_member_with(1, "toshiba1", "/dev/disk/by-id/disk1", Some(1), None);
        let (_, member2) = disk_member_with(2, "toshiba2", "/dev/disk/by-id/disk2", Some(2), None);
        let (_, member3) = disk_member_with(3, "toshiba3", "/dev/disk/by-id/disk3", Some(3), None);
        let membership = membership_from(vec![
            (
                LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                member1,
            ),
            (
                LuksUuid::parse("22222222-2222-2222-2222-222222222222").unwrap(),
                member2,
            ),
            (
                LuksUuid::parse("33333333-3333-3333-3333-333333333333").unwrap(),
                member3,
            ),
        ]);
        let runner = status_runner_healthy_3disk_base()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: status_mp(),
                },
                status_btrfs_show_3disk_missing_devid3(),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk1".into(),
                },
                status_cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk2".into(),
                },
                status_cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_luks_dump_text_luks2_for(&["/dev/disk/by-id/disk1", "/dev/disk/by-id/disk2"])
            .with_mappers_closed(&["braid-toshiba1", "braid-toshiba2"]);
        let fs = status_fs_mounted(&["/dev/disk/by-id/disk1", "/dev/disk/by-id/disk2"]);
        let config = status_config();
        let (_tmp, paths) = isolated_paths();
        membership::save_membership(&membership, &paths).unwrap();
        alert::save_alert_latch(
            &AlertState {
                causes: vec![AlertCause::MissingDevice { devid: 3 }],
            },
            &paths,
        )
        .unwrap();

        let built = build_status(
            &runner,
            &fs,
            &config,
            &paths,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        )
        .unwrap();
        let extras = built.mounted_extras.as_ref().unwrap();
        let human = format_status_human(
            &built.report,
            Some(&extras.compact_drives),
            Some(&extras.human_details),
            Some(&extras.devid_names),
        );

        assert!(
            human.contains("missing device: toshiba3 (devid 3)"),
            "expected missing-device alert to name toshiba3, got:\n{human}"
        );
        let toshiba3_row = human
            .lines()
            .find(|line| line.trim_start().starts_with("toshiba3") && line.contains("missing"))
            .expect("missing compact row for toshiba3");
        assert!(
            toshiba3_row.contains("devid=3"),
            "missing compact row must show live-confirmed devid, got:\n{human}"
        );
    }

    #[test]
    fn cmd_status_single_disk_ok() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_show_1disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName("disk1".into()),
                },
                status_cryptsetup_status_active("disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                status_cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_df_single(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_usage_raw(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_device_usage_raw_1disk(),
            )
            .with_output(
                CmdRequest::BtrfsScrubStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_scrub_never(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceStatsJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                mock_ok(
                    "btrfs device stats",
                    r#"{"device-stats": [
                        {"device": "/dev/mapper/disk1", "devid": 1, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
                    ]}"#,
                ),
            )
            // probe_config_disk for disk1 (by-id path)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk1".into(),
                },
                status_cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            );
        let fs = status_fs_one_disk();
        let config = status_config();

        let (_tmp, paths) = isolated_paths();
        let result = cmd_status(
            &runner,
            &fs,
            &config,
            false,
            &paths,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        );
        assert!(result.is_ok());
    }

    // =======================================================================
    // build_disk_reports: PresentNotLuks classification
    // =======================================================================

    /// Intent: when probe_luks_header itself cannot run (no mock outputs),
    /// build_disk_reports must collapse the unpooled PresentNotLuks disk to
    /// the generic Unknown bucket rather than guess at Unreadable/Damaged.
    ///
    /// Why: if we cannot prove which header state caused the luksUuid
    /// failure, surfacing a confident "unreadable" or "damaged" label would
    /// mislead users.
    ///
    /// Scenario: a declared pool member with PresentNotLuks state, no
    /// CryptsetupIsLuks/LuksDumpText outputs configured on the runner.
    #[test]
    fn build_disk_reports_present_not_luks_probe_failed_falls_back_to_unknown() {
        let config_disks = status_cfg_present_not_luks("disk1", "/dev/disk/by-id/disk1");
        let runner = MockRunner::default();
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(
            &runner,
            &PoolMembership::empty(),
            &config_disks,
            &status_pool_empty(),
            &stats,
        );
        assert_eq!(ctx.disks.len(), 1);
        assert_eq!(ctx.disks[0].status, DiskStatus::Unknown);
    }

    /// Intent: when probe_luks_header reports Unreadable on the
    /// PresentNotLuks branch, build_disk_reports must surface
    /// DiskStatus::LuksHeaderUnreadable.
    ///
    /// Why: status reporting is the user-facing surface for the
    /// Unreadable/Damaged distinction; without this mapping the diagnostic
    /// would stay collapsed in the generic Unknown bucket.
    ///
    /// Scenario: PresentNotLuks config disk where cryptsetup isLuks exits
    /// non-zero (LUKS magic absent or corrupted).
    #[test]
    fn build_disk_reports_present_not_luks_unreadable_maps_to_luks_header_unreadable() {
        let config_disks = status_cfg_present_not_luks("disk1", "/dev/disk/by-id/disk1");
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupIsLuks {
                device: "/dev/disk/by-id/disk1".to_owned(),
            },
            status_is_luks_raw(
                "/dev/disk/by-id/disk1",
                1,
                "Device /dev/disk/by-id/disk1 is not a valid LUKS device.\n",
            ),
        );
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(
            &runner,
            &PoolMembership::empty(),
            &config_disks,
            &status_pool_empty(),
            &stats,
        );
        assert_eq!(ctx.disks.len(), 1);
        assert_eq!(ctx.disks[0].status, DiskStatus::LuksHeaderUnreadable);
    }

    /// Intent: when probe_luks_header reports Damaged (isLuks ok, luksDump
    /// fails), build_disk_reports must surface DiskStatus::LuksHeaderDamaged.
    ///
    /// Why: damaged metadata has a different recovery story
    /// (`cryptsetup repair`) than unreadable headers; status output must
    /// preserve the distinction.
    ///
    /// Scenario: PresentNotLuks config disk where isLuks succeeds but
    /// luksDump fails to parse the header metadata blocks.
    #[test]
    fn build_disk_reports_present_not_luks_damaged_maps_to_luks_header_damaged() {
        let config_disks = status_cfg_present_not_luks("disk1", "/dev/disk/by-id/disk1");
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/disk1".to_owned(),
                },
                status_is_luks_raw("/dev/disk/by-id/disk1", 0, ""),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk1".to_owned(),
                },
                status_luks_dump_text_raw(
                    "/dev/disk/by-id/disk1",
                    1,
                    "",
                    "Cannot read LUKS header metadata.\n",
                ),
            );
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(
            &runner,
            &PoolMembership::empty(),
            &config_disks,
            &status_pool_empty(),
            &stats,
        );
        assert_eq!(ctx.disks.len(), 1);
        assert_eq!(ctx.disks[0].status, DiskStatus::LuksHeaderDamaged);
    }

    /*
     * Intent: when probe_luks_header reports Ok after probe_config_disk saw
     * PresentNotLuks, build_disk_reports must classify the disk as Unknown
     * rather than LuksHeaderDamaged.
     *
     * Why it exists: a clean isLuks + luksDump re-probe contradicts the
     * original luksUuid failure; the most likely cause is a transient blip, not
     * a damaged header. braid doctor shares the same probe and would classify
     * the disk as healthy, so labelling it Damaged produces a
     * self-contradicting recovery flow and overclaims damage that the tools
     * have not demonstrated.
     *
     * Scenario: PresentNotLuks config disk where isLuks succeeds, luksDump also
     * succeeds, and the original luksUuid exit-non-zero was a transient failure.
     */
    #[test]
    fn build_disk_reports_present_not_luks_inconsistent_falls_back_to_unknown() {
        let config_disks = status_cfg_present_not_luks("disk1", "/dev/disk/by-id/disk1");
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/disk1".to_owned(),
                },
                status_is_luks_raw("/dev/disk/by-id/disk1", 0, ""),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk1".to_owned(),
                },
                status_luks_dump_text_raw(
                    "/dev/disk/by-id/disk1",
                    0,
                    "LUKS header information\nVersion: 2\n",
                    "",
                ),
            );
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(
            &runner,
            &PoolMembership::empty(),
            &config_disks,
            &status_pool_empty(),
            &stats,
        );
        assert_eq!(ctx.disks.len(), 1);
        assert_eq!(ctx.disks[0].status, DiskStatus::Unknown);
    }

    /*
     * Intent: when a config disk's by_id LUKS header probe failed
     * (PresentNotLuks) but its membership UUID is already live in the pool,
     * build_disk_reports must emit exactly one Present row for that disk in
     * both the JSON disks array and the verbose human output, not a duplicate
     * Unknown/LuksHeader* row from the unpooled fall-through.
     *
     * Why it exists: the unpooled loop must join through UUID-keyed
     * membership rather than the expected mapper name. A live mapper named
     * `braid-disk1` is not proof that it is the disk1 member; the live LUKS
     * UUID is.
     *
     * Scenario: pool device "braid-disk1" / uuid U1 / devid 1; config disk
     * "disk1" with state PresentNotLuks. No CryptsetupIsLuks/LuksDumpText
     * mocks exist, so probe_luks_header would return ProbeFailed if it ran.
     */
    #[test]
    fn build_disk_reports_skips_unpooled_row_when_membership_uuid_live_for_present_not_luks() {
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk1".to_owned()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: 1,
                underlying: "/dev/vda".to_owned(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };
        let config_disks = status_cfg_present_not_luks("disk1", "/dev/disk/by-id/disk1");
        let membership = status_membership_1disk();
        let runner = MockRunner::default();
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(&runner, &membership, &config_disks, &pool, &stats);

        assert_eq!(ctx.disks.len(), 1, "disks: {:?}", ctx.disks);
        assert_eq!(ctx.disks[0].status, DiskStatus::Present);
        assert_eq!(ctx.disks[0].name, "disk1");
        assert_eq!(ctx.human_details.len(), 1);
        assert_eq!(ctx.human_details[0].name, "disk1");

        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: StatusCode::Intact,
            total_devices: Some(1),
            present_count: Some(1),
            missing_count: Some(0),
            profile: Some("single".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: Some(1073741824),
                used_bytes: 0,
                free_bytes: 1073741824,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: ctx.disks.clone(),
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let human = format_status_human(&report, None, Some(&ctx.human_details), None);

        assert!(human.contains("disk1"), "got:\n{human}");
        assert!(
            !human.contains("UNKNOWN"),
            "duplicate Unknown row leaked; got:\n{human}"
        );
        assert!(!human.contains("LUKS HEADER UNREADABLE"), "got:\n{human}");
        assert!(!human.contains("LUKS HEADER DAMAGED"), "got:\n{human}");
    }

    /*
     * Intent: a foreign live mapper whose name looks like a member's mapper
     * does not suppress the UUID-keyed member's unpooled row in verbose
     * status.
     *
     * Why it exists: mapper names are runtime handles, not membership
     * identity. A stale implementation can parse `braid-disk1` as proof that
     * disk1 is present and hide the fact that UUID U1 is missing.
     *
     * Scenario: membership expects disk1 at UUID U1. The mounted pool reports
     * mapper `braid-disk1` with UUID U9, and the configured by-id path cannot
     * currently report a LUKS UUID. Status must show the foreign runtime
     * handle and a separate disk1 diagnostic row.
     */
    #[test]
    fn build_disk_reports_foreign_mapper_name_does_not_hide_missing_member() {
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk1".to_owned()),
                luks_uuid: LuksUuid::parse("99999999-9999-9999-9999-999999999999").unwrap(),
                devid: 1,
                underlying: "/dev/vdz".to_owned(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };
        let config_disks = status_cfg_present_not_luks("disk1", "/dev/disk/by-id/disk1");
        let membership = status_membership_1disk();
        let runner = MockRunner::default();
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(&runner, &membership, &config_disks, &pool, &stats);

        assert_eq!(ctx.disks.len(), 2, "disks: {:?}", ctx.disks);
        assert_eq!(ctx.disks[0].name, "braid-disk1");
        assert_eq!(ctx.disks[0].status, DiskStatus::Present);
        assert_eq!(
            ctx.disks[0].luks_uuid,
            "99999999-9999-9999-9999-999999999999"
        );
        assert_eq!(ctx.disks[1].name, "disk1");
        assert_eq!(ctx.disks[1].status, DiskStatus::Unknown);
    }

    /*
     * Intent: a config disk whose current by-id target reports the foreign
     * live UUID still does not satisfy the UUID-keyed member.
     *
     * Why it exists: a swapped/reformatted disk can make the member's by-id
     * path point at the live foreign UUID. Verbose status must not attach the
     * member's name to that live UUID or suppress the member diagnostic row.
     *
     * Scenario: membership expects disk1 at UUID U1, but disk1's by-id path
     * probes as PresentLuks UUID U9 and the mounted pool also reports U9.
     */
    #[test]
    fn build_disk_reports_foreign_config_uuid_does_not_hide_missing_member() {
        let foreign_uuid = LuksUuid::parse("99999999-9999-9999-9999-999999999999").unwrap();
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk1".to_owned()),
                luks_uuid: foreign_uuid.clone(),
                devid: 1,
                underlying: "/dev/vdz".to_owned(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };
        let config_disks = vec![ConfigDisk {
            name: DiskName::parse("disk1").unwrap(),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/disk1").unwrap(),
            state: ConfigDiskState::PresentLuks {
                uuid: foreign_uuid,
                label: Some("braid-disk1".to_owned()),
                mapper_open: true,
            },
        }];
        let membership = status_membership_1disk();
        let runner = MockRunner::default();
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(&runner, &membership, &config_disks, &pool, &stats);

        assert_eq!(ctx.disks.len(), 2, "disks: {:?}", ctx.disks);
        assert_eq!(ctx.disks[0].name, "braid-disk1");
        assert_eq!(ctx.disks[0].status, DiskStatus::Present);
        assert_eq!(
            ctx.disks[0].luks_uuid,
            "99999999-9999-9999-9999-999999999999"
        );
        assert_eq!(ctx.disks[1].name, "disk1");
        assert_eq!(ctx.disks[1].status, DiskStatus::Unknown);
    }

    // Intent: foreign live mappers with btrfs errors must not receive a
    // member-scoped `braid replace --old` action in verbose status.
    // Why it exists: replacement targets are member names joined by LUKS UUID;
    // a runtime mapper basename can look like a member while identifying a
    // different encrypted device.
    // Scenario: one pool row is a foreign `braid-disk1` mapper with errors
    // and another pool row is the real disk1 member with errors.
    #[test]
    fn build_disk_reports_routes_foreign_mapper_errors_to_doctor() {
        use crate::parse::types::{DeviceErrorStats, DeviceStatsTarget};

        let member_uuid = LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap();
        let foreign_uuid = LuksUuid::parse("99999999-9999-9999-9999-999999999999").unwrap();
        let pool = PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    mapper: MapperName("braid-disk1".to_owned()),
                    luks_uuid: foreign_uuid,
                    devid: 1,
                    underlying: "/dev/vdz".to_owned(),
                },
                PoolDevice {
                    mapper: MapperName("braid-member".to_owned()),
                    luks_uuid: member_uuid.clone(),
                    devid: 2,
                    underlying: "/dev/vda".to_owned(),
                },
            ],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 2,
            fsid: None,
            null_underlying: vec![],
        };
        let config_disks = vec![ConfigDisk {
            name: DiskName::parse("disk1").unwrap(),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/disk1").unwrap(),
            state: ConfigDiskState::PresentLuks {
                uuid: member_uuid,
                label: None,
                mapper_open: true,
            },
        }];
        let stats = BtrfsDeviceStatsOutput {
            devices: vec![
                DeviceErrorStats {
                    devid: 1,
                    target: DeviceStatsTarget::Path("/dev/mapper/braid-disk1".to_owned()),
                    read_io_errs: 5,
                    write_io_errs: 0,
                    flush_io_errs: 0,
                    corruption_errs: 0,
                    generation_errs: 0,
                },
                DeviceErrorStats {
                    devid: 2,
                    target: DeviceStatsTarget::Path("/dev/mapper/braid-member".to_owned()),
                    read_io_errs: 7,
                    write_io_errs: 0,
                    flush_io_errs: 0,
                    corruption_errs: 0,
                    generation_errs: 0,
                },
            ],
        };
        let runner = MockRunner::default();
        let membership = status_membership_1disk();

        let ctx = build_disk_reports(&runner, &membership, &config_disks, &pool, &stats);

        let foreign = ctx
            .human_details
            .iter()
            .find(|d| d.name == "braid-disk1")
            .expect("foreign mapper row");
        assert!(foreign.member_name.is_none());
        let member = ctx
            .human_details
            .iter()
            .find(|d| d.name == "disk1")
            .expect("member row");
        assert_eq!(
            member.member_name.as_ref().map(DiskName::as_str),
            Some("disk1")
        );

        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: StatusCode::Degraded,
            total_devices: Some(2),
            present_count: Some(2),
            missing_count: Some(0),
            profile: Some("RAID1".to_owned()),
            capacity: None,
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: ctx.disks.clone(),
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let human = format_status_human(&report, None, Some(&ctx.human_details), None);

        assert!(
            !human.contains("braid replace --old braid-"),
            "foreign mapper must not be rendered as a replace target; got:\n{human}"
        );
        assert!(human.contains("foreign mapper detected"), "got:\n{human}");
        assert!(human.contains("run 'braid doctor'"), "got:\n{human}");
        assert!(
            human.contains("braid replace --old disk1 --new <new-name>"),
            "member row must keep replacement guidance; got:\n{human}"
        );
    }

    // Intent: missing member rows built from config disks must retain their
    // typed member name for verbose replacement guidance.
    // Why it exists: the same formatter branch handles missing disks and
    // erroring present disks; only real members may render `braid replace`.
    // Scenario: disk1 is declared in membership and config, but the mounted
    // pool has no live device for it.
    #[test]
    fn build_disk_reports_missing_member_keeps_replace_action_target() {
        let config_disks = vec![ConfigDisk {
            name: DiskName::parse("disk1").unwrap(),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/disk1").unwrap(),
            state: ConfigDiskState::Absent,
        }];
        let runner = MockRunner::default();
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };
        let membership = status_membership_1disk();

        let ctx = build_disk_reports(
            &runner,
            &membership,
            &config_disks,
            &status_pool_empty(),
            &stats,
        );

        assert_eq!(ctx.human_details.len(), 1);
        assert_eq!(ctx.human_details[0].status, DiskStatus::Missing);
        assert_eq!(
            ctx.human_details[0]
                .member_name
                .as_ref()
                .map(DiskName::as_str),
            Some("disk1")
        );

        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: StatusCode::Degraded,
            total_devices: Some(1),
            present_count: Some(0),
            missing_count: Some(1),
            profile: Some("RAID1".to_owned()),
            capacity: None,
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: ctx.disks.clone(),
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };
        let human = format_status_human(&report, None, Some(&ctx.human_details), None);

        assert!(
            human.contains("braid replace --old disk1 --new <new-name>"),
            "missing member must keep replacement guidance; got:\n{human}"
        );
    }

    // =======================================================================
    // build_devid_names tests
    // =======================================================================

    #[test]
    fn build_devid_names_covers_present_null_underlying_and_missing() {
        // Intent: the alert-name map covers every live btrfs devid source:
        // present devices, null-underlying devices, and btrfs-MISSING rows.
        // Why it exists: missing and null-underlying rows have no live
        // DiskReport.devid join, so they must resolve through persisted
        // membership devids instead.
        // Scenario: a three-disk pool has one present disk, one hot-unplugged
        // null-underlying mapper, and one btrfs-MISSING placeholder.
        let (uuid1, member1) =
            disk_member_with(11, "toshiba1", "/dev/disk/by-id/disk1", Some(1), None);
        let (uuid2, member2) =
            disk_member_with(12, "toshiba2", "/dev/disk/by-id/disk2", Some(2), None);
        let (uuid3, member3) =
            disk_member_with(13, "toshiba3", "/dev/disk/by-id/disk3", Some(3), None);
        let membership = membership_from(vec![
            (uuid1.clone(), member1),
            (uuid2, member2),
            (uuid3, member3),
        ]);
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-toshiba1".to_owned()),
                luks_uuid: uuid1,
                devid: 1,
                underlying: "/dev/vda".to_owned(),
            }],
            missing_count: 2,
            missing_devids: vec![3],
            total_devices: 3,
            fsid: None,
            null_underlying: vec![NullUnderlyingDevice {
                mapper: MapperName("braid-toshiba2".to_owned()),
                devid: 2,
            }],
        };

        let names = build_devid_names(&pool, &membership).unwrap();

        assert_eq!(names.get(&1).map(String::as_str), Some("toshiba1"));
        assert_eq!(names.get(&2).map(String::as_str), Some("toshiba2"));
        assert_eq!(names.get(&3).map(String::as_str), Some("toshiba3"));
    }

    #[test]
    fn build_devid_names_present_foreign_live_uses_mapper_basename() {
        // Intent: present live devices without a membership UUID match still
        // get a display name in the devid alert map.
        // Why it exists: btrfs device-error alerts are keyed by devid; a
        // foreign live mapper must not degrade to a bare `devid N` banner.
        // Scenario: btrfs reports a live mapper whose LUKS UUID is not in
        // pool.json, so status falls back to the observed mapper basename.
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("foreign-live".to_owned()),
                luks_uuid: test_uuid(910),
                devid: 7,
                underlying: "/dev/vdz".to_owned(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };

        let names = build_devid_names(&pool, &PoolMembership::empty()).unwrap();

        assert_eq!(names.get(&7).map(String::as_str), Some("foreign-live"));
    }

    #[test]
    fn build_devid_names_propagates_duplicate_devid() {
        // Intent: corrupt membership with duplicate persisted devids fails
        // closed during missing-device name resolution.
        // Why it exists: silently picking one member would attach the wrong
        // operator-facing disk name to a missing-device alert.
        // Scenario: two pool.json entries both claim devid 7 and btrfs
        // reports devid 7 as MISSING.
        let (uuid1, member1) =
            disk_member_with(921, "toshiba1", "/dev/disk/by-id/disk1", Some(7), None);
        let (uuid2, member2) =
            disk_member_with(922, "toshiba2", "/dev/disk/by-id/disk2", Some(7), None);
        let membership =
            PoolMembership::for_corruption_tests(vec![(uuid1, member1), (uuid2, member2)]);
        let pool = PoolState {
            mounted: true,
            devices: vec![],
            missing_count: 1,
            missing_devids: vec![7],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };

        let err = build_devid_names(&pool, &membership).unwrap_err();

        assert!(
            matches!(
                err,
                membership::MembershipError::DuplicateDevid { devid: 7, .. }
            ),
            "expected DuplicateDevid, got: {err:?}"
        );
    }

    // =======================================================================
    // Compact drive tests
    // =======================================================================

    #[test]
    fn status_compact_missing_disk() {
        let pool = PoolState {
            mounted: true,
            devices: vec![],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 0,
            fsid: None,
            null_underlying: vec![],
        };
        let membership = status_membership_1disk();
        let drives = build_compact_drives(&pool, &membership);
        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].status, DiskStatus::Missing);
    }

    #[test]
    fn status_compact_names_present_disk_from_membership_uuid() {
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-drifted".to_owned()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: 1,
                underlying: "/dev/vda".to_owned(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };
        let membership = status_membership_1disk();

        let drives = build_compact_drives(&pool, &membership);

        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].name, "disk1");
        assert_eq!(drives[0].status, DiskStatus::Present);
    }

    /*
     * Intent: a live pool device with a foreign LUKS UUID does not satisfy a
     * missing member just because its mapper name has the expected
     * `braid-<name>` shape.
     *
     * Why it exists: LUKS UUID is the membership join. The status compact
     * summary must not parse mapper names to decide that a UUID-keyed member
     * is present, or it can hide a swapped/reformatted disk behind the old
     * human name.
     *
     * Scenario: membership expects disk1 at UUID U1, but the mounted pool has
     * a live mapper `braid-disk1` with UUID U9. The compact list shows the
     * foreign runtime handle as present and disk1 as missing.
     */
    #[test]
    fn status_compact_foreign_mapper_name_does_not_hide_missing_member() {
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk1".to_owned()),
                luks_uuid: LuksUuid::parse("99999999-9999-9999-9999-999999999999").unwrap(),
                devid: 1,
                underlying: "/dev/vdz".to_owned(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };
        let membership = status_membership_1disk();

        let drives = build_compact_drives(&pool, &membership);

        assert_eq!(drives.len(), 2);
        assert_eq!(drives[0].name, "braid-disk1");
        assert_eq!(drives[0].status, DiskStatus::Present);
        assert_eq!(drives[1].name, "disk1");
        assert_eq!(drives[1].status, DiskStatus::Missing);
    }

    #[test]
    fn build_compact_drives_missing_member_shows_devid_when_live_confirmed() {
        // Intent: a missing compact row shows a persisted devid only when live
        // btrfs confirms that devid is currently missing.
        // Why it exists: this is the compact `Drives:` half of the missing
        // device UX; the alert banner fix alone would still leave `-`.
        // Scenario: pool.json remembers disk1 as devid 3 and btrfs reports
        // devid 3 in the alert-local missing set.
        let (_, member) = disk_member_with(931, "toshiba3", "/dev/disk/by-id/disk3", Some(3), None);
        let membership = membership_from(vec![(test_uuid(931), member)]);
        let pool = PoolState {
            mounted: true,
            devices: vec![],
            missing_count: 1,
            missing_devids: vec![3],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };

        let drives = build_compact_drives(&pool, &membership);

        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].name, "toshiba3");
        assert_eq!(drives[0].devid, Some(3));
        assert_eq!(drives[0].status, DiskStatus::Missing);
    }

    #[test]
    fn build_compact_drives_missing_member_hides_stale_persisted_devid() {
        // Intent: a missing compact row hides a persisted devid when live
        // btrfs does not currently report that devid as missing.
        // Why it exists: persisted membership is only a fallback join key; it
        // must not make stale devids look btrfs-authoritative in display.
        // Scenario: pool.json remembers disk1 as devid 3, but live btrfs has
        // no MISSING or null-underlying record for devid 3.
        let (_, member) = disk_member_with(941, "toshiba3", "/dev/disk/by-id/disk3", Some(3), None);
        let membership = membership_from(vec![(test_uuid(941), member)]);
        let pool = PoolState {
            mounted: true,
            devices: vec![],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 0,
            fsid: None,
            null_underlying: vec![],
        };

        let drives = build_compact_drives(&pool, &membership);

        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].name, "toshiba3");
        assert_eq!(drives[0].devid, None);
        assert_eq!(drives[0].status, DiskStatus::Missing);
    }

    // =======================================================================
    // Verbose unknown tests
    // =======================================================================

    /*
     * Intent: Unknown remains a non-alarming, non-prescriptive bucket in the
     * verbose human output.
     *
     * Why it exists: callers rely on Unknown meaning braid could not reconcile
     * available metadata, not that recovery should assume header damage.
     *
     * Scenario: verbose status renders a config disk with no resolved metadata
     * as UNKNOWN, with metadata unavailable, and without damage or doctor
     * guidance.
     */
    #[test]
    fn status_verbose_unknown_disk() {
        let human_disks = vec![HumanDisk {
            name: "disk2".to_owned(),
            member_name: Some(DiskName::parse("disk2").unwrap()),
            by_id: "/dev/disk/by-id/disk2".to_owned(),
            luks_uuid: String::new(),
            devid: None,
            status: DiskStatus::Unknown,
            model: None,
            serial: None,
            errors: None,
        }];

        let code = StatusCode::Intact;
        let report = StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: code,
            total_devices: Some(1),
            present_count: Some(1),
            missing_count: Some(0),
            profile: Some("single".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: Some(1073741824),
                used_bytes: 536870912,
                free_bytes: 536870912,
            }),
            last_scrub: Some(ScrubReport::Never),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        };

        let human = format_status_human(&report, None, Some(&human_disks), None);
        assert!(human.contains("UNKNOWN"), "got:\n{human}");
        assert!(human.contains("metadata unavailable"), "got:\n{human}");
        assert!(
            !human.contains("LUKS HEADER DAMAGED"),
            "Unknown must not surface a damaged-header label; got:\n{human}"
        );
        assert!(
            !human.contains("LUKS HEADER UNREADABLE"),
            "Unknown must not surface an unreadable-header label; got:\n{human}"
        );
        assert!(
            !human.contains("braid doctor"),
            "Unknown must not push users toward doctor recovery; got:\n{human}"
        );
    }

    // =======================================================================
    // alert display tests
    // =======================================================================

    #[test]
    fn alert_missing_device_uses_devid_names_map() {
        // Intent: a missing-device alert names the member through the explicit
        // devid map even when the missing DiskReport row has no devid field.
        // Why it exists: this is the regression where the alert banner dropped
        // the disk name for btrfs-MISSING/null-underlying members.
        // Scenario: report.disks contains the unpooled missing-row shape and
        // the human formatter receives `{3: "toshiba3"}` from build_status.
        let disks = vec![status_disk_report_missing("toshiba3")];
        let report = status_report_with_alerts(disks, vec![AlertCause::MissingDevice { devid: 3 }]);
        let devid_names = std::collections::HashMap::from([(3, "toshiba3".to_owned())]);
        let human = format_status_human(&report, None, None, Some(&devid_names));
        assert!(
            human.contains("missing device: toshiba3 (devid 3)"),
            "expected device name in alert, got:\n{human}"
        );
    }

    #[test]
    fn alert_btrfs_errors_shows_name() {
        // Intent: a btrfs device-error alert names the matching present disk.
        // Why it exists: alert causes are keyed by devid, but operators need
        // the display name in the banner.
        // Scenario: devid 1 has an entry in the status-built devid name map.
        let disks = vec![
            status_disk_report_named("aaa", 1),
            status_disk_report_named("bbb", 2),
        ];
        let report =
            status_report_with_alerts(disks, vec![AlertCause::BtrfsDeviceErrors { devid: 1 }]);
        let devid_names =
            std::collections::HashMap::from([(1, "aaa".to_owned()), (2, "bbb".to_owned())]);
        let human = format_status_human(&report, None, None, Some(&devid_names));
        assert!(
            human.contains("btrfs device errors on aaa (devid 1)"),
            "expected device name in alert, got:\n{human}"
        );
    }

    #[test]
    fn alert_missing_device_falls_back_when_map_missing_entry() {
        // Intent: a missing-device alert falls back to the raw devid when the
        // explicit name map lacks that devid.
        // Why it exists: report.disks is no longer the fallback join; missing
        // rows with stale or unrelated devids must not influence the banner.
        // Scenario: the report has a DiskReport with devid 99, but the status
        // path did not authorize a display-name binding for devid 99.
        let disks = vec![status_disk_report_named("aaa", 99)];
        let report =
            status_report_with_alerts(disks, vec![AlertCause::MissingDevice { devid: 99 }]);
        let devid_names = std::collections::HashMap::new();
        let human = format_status_human(&report, None, None, Some(&devid_names));
        assert!(
            human.contains("missing device: devid 99"),
            "unknown devid should fall back to raw id, got:\n{human}"
        );
    }

    #[test]
    fn alert_btrfs_errors_foreign_live_mapper_keeps_basename() {
        // Intent: a foreign live mapper still names btrfs device errors by its
        // observed mapper basename.
        // Why it exists: present devices without membership matches have a
        // live display fallback and should not regress to bare `devid N`.
        // Scenario: btrfs reports devid 1 on a mapper whose LUKS UUID is not
        // present in pool.json, and an alert fires for that devid.
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("foreign-live".to_owned()),
                luks_uuid: test_uuid(950),
                devid: 1,
                underlying: "/dev/vdz".to_owned(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };
        let devid_names = build_devid_names(&pool, &PoolMembership::empty()).unwrap();
        let report = status_report_with_alerts(
            vec![status_disk_report_named("foreign-live", 1)],
            vec![AlertCause::BtrfsDeviceErrors { devid: 1 }],
        );

        let human = format_status_human(&report, None, None, Some(&devid_names));

        assert!(
            human.contains("btrfs device errors on foreign-live (devid 1)"),
            "expected foreign mapper basename in alert, got:\n{human}"
        );
    }

    // =======================================================================
    // estimate_pool_capacity tests
    // =======================================================================

    #[test]
    fn estimate_pool_capacity_0_disks() {
        assert_eq!(estimate_pool_capacity(&[]), 0);
    }

    #[test]
    fn estimate_pool_capacity_1_disk() {
        assert_eq!(
            estimate_pool_capacity(&[8_000_000_000_000]),
            8_000_000_000_000
        );
    }

    #[test]
    fn estimate_pool_capacity_2_equal() {
        // 2×4 TB = 8 TB total, RAID1 = 4 TB usable
        assert_eq!(
            estimate_pool_capacity(&[4_000_000_000_000, 4_000_000_000_000]),
            4_000_000_000_000,
        );
    }

    #[test]
    fn estimate_pool_capacity_3_equal() {
        // 3×4 TB = 12 TB total, RAID1 = 6 TB usable
        let sizes = &[4_000_000_000_000, 4_000_000_000_000, 4_000_000_000_000];
        assert_eq!(estimate_pool_capacity(sizes), 6_000_000_000_000);
    }

    #[test]
    fn estimate_pool_capacity_mixed_with_waste() {
        // 3+3+8 = 14 TB total. sum/2 = 7 TB, sum-max = 6 TB. Usable = 6 TB.
        let sizes = &[3_000_000_000_000, 3_000_000_000_000, 8_000_000_000_000];
        assert_eq!(estimate_pool_capacity(sizes), 6_000_000_000_000);
    }

    #[test]
    fn estimate_pool_capacity_mixed_no_waste() {
        // 4+4+6 = 14 TB total. sum/2 = 7 TB, sum-max = 8 TB. Usable = 7 TB.
        let sizes = &[4_000_000_000_000, 4_000_000_000_000, 6_000_000_000_000];
        assert_eq!(estimate_pool_capacity(sizes), 7_000_000_000_000);
    }

    // =======================================================================
    // Corrupt pool.json regression test
    // =======================================================================

    #[test]
    fn cmd_status_corrupt_membership_returns_error() {
        // Intent: a corrupt pool.json must surface as an error, not be silently
        // treated as an empty pool.
        //
        // Why it exists: the original code used Err(_) => PoolMembership::empty(),
        // which collapsed NotFound (expected) and Corrupt (data loss) into the
        // same fallback, hiding corruption from the user.
        //
        // Scenario: pool is mounted and healthy, but pool.json contains garbage.
        // braid status should return StatusError::Membership(Corrupt(..)).
        let (_tmp, paths) = isolated_paths();
        std::fs::write(paths.pool_json(), "not valid json {{{").unwrap();

        let runner = status_runner_healthy_3disk_base();
        let fs = status_fs_three_disk();
        let config = status_config();

        let result = cmd_status(
            &runner,
            &fs,
            &config,
            false,
            &paths,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        );
        assert!(result.is_err(), "expected error for corrupt pool.json");
        assert!(
            matches!(
                result.unwrap_err(),
                StatusError::Membership(membership::MembershipError::Corrupt { .. })
            ),
            "expected StatusError::Membership(Corrupt(..))"
        );
    }

    /*
     * Intent: offline `braid status` must not read pool.json.
     * Why it exists: this refactor centralizes membership loading inside
     *   build_status; without this test, a future edit could accidentally load
     *   membership before the not-mounted check, turning an offline
     *   `braid status` with a corrupt pool.json into a hard error.
     * Scenario: pool is offline (LUKS not unlocked, no btrfs at the mount
     *   point) and pool.json contains garbage. `braid status` must return Ok
     *   with a NotMounted report rather than StatusError::Membership.
     */
    #[test]
    fn cmd_status_unmounted_corrupt_membership_returns_ok() {
        let (_tmp, paths) = isolated_paths();
        std::fs::write(paths.pool_json(), "not valid json {{{").unwrap();

        let runner = MockRunner::default();
        let fs = status_fs_not_mounted(&[]);
        let config = status_config();

        let result = cmd_status(
            &runner,
            &fs,
            &config,
            false,
            &paths,
            crate::test_fixtures::mock_virtio_backing_path_resolver(),
        );
        assert!(
            result.is_ok(),
            "expected offline status to ignore pool.json corruption"
        );
    }

    /*
     * Intent: a ProbeError::MapperConflict raised by probe_config_disk while
     *   building the per-disk status report must surface through
     *   build_status as a StatusError::Probe(MapperConflict), not be
     *   swallowed or remapped.
     * Why it exists: a future regression in the status-path error handling
     *   could narrow or drop the MapperConflict variant (e.g. a .or_else that
     *   filters probe errors), hiding the probe-layer safety fix from the
     *   non-mutating command boundary. The probe-level tests in probe.rs lock
     *   the gateway behavior; this test locks the propagation contract so
     *   both halves stay honest.
     * Scenario: braid status run on a host where the LUKS mapper
     *   /dev/mapper/braid-disk1 was externally aliased to a different LUKS
     *   container (a distinct UUID) before braid was invoked.
     */
    #[test]
    fn status_surfaces_mapper_conflict() {
        use crate::probe::ProbeError;
        use crate::types::LuksUuid;

        let (_tmp, paths) = isolated_paths();

        let mut membership = PoolMembership::empty();
        membership
            .insert(
                LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                DiskMember::new(
                    DiskName::parse("disk1").unwrap(),
                    ByIdPath::parse("/dev/disk/by-id/disk1").unwrap(),
                ),
            )
            .expect("insert disk1 fixture member");

        // Healthy 1-disk mounted pool for probe_pool + data-gathering; the
        // pool-side mapper "disk1" is distinct from the config-side mapper
        // "braid-disk1" and does not collide.
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_show_1disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName("disk1".into()),
                },
                status_cryptsetup_status_active("disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                status_cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_df_single(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_usage_raw(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_device_usage_raw_1disk(),
            )
            .with_output(
                CmdRequest::BtrfsScrubStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                status_btrfs_scrub_never(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceStatsJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                mock_ok(
                    "btrfs device stats",
                    r#"{"device-stats": [
                        {"device": "/dev/mapper/disk1", "devid": 1, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
                    ]}"#,
                ),
            )
            // probe_config_disk for "disk1": by-id UUID = 11111111, mapper
            // backing reports 99999999 → MapperConflict.
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk1".into(),
                },
                status_cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk1".into(),
                },
                mock_ok(
                    "cryptsetup luksDump",
                    "LUKS header information\nVersion:       \t2\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: MapperName("braid-disk1".into()),
                },
                status_cryptsetup_status_active("braid-disk1", "/dev/vdz"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdz".into(),
                },
                status_cryptsetup_uuid_ok("/dev/vdz", "99999999-9999-9999-9999-999999999999"),
            );

        let fs = status_fs_one_disk();
        let config = status_config();

        membership::save_membership(&membership, &paths).unwrap();

        let backing_path_resolver = crate::test_fixtures::MockBackingPathResolver::default()
            .with_path("/dev/disk/by-id/disk1", "/dev/vdz");
        let result = build_status(&runner, &fs, &config, &paths, &backing_path_resolver);
        match result {
            Err(StatusError::Probe(ProbeError::MapperConflict {
                name,
                expected,
                found,
            })) => {
                assert_eq!(name, "disk1");
                assert_eq!(
                    expected,
                    LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap()
                );
                assert_eq!(
                    found,
                    Some(LuksUuid::parse("99999999-9999-9999-9999-999999999999").unwrap())
                );
            }
            Err(other) => panic!("expected StatusError::Probe(MapperConflict), got: {other:?}"),
            Ok(_) => panic!("expected StatusError::Probe(MapperConflict), got Ok"),
        }
    }

    /*
     * Intent: build_disk_reports pairs btrfs device-stats rows to DiskReport
     * by devid, not by mapper-path string. A stats row whose path differs
     * from the expected /dev/mapper/braid-X but whose devid matches a pool
     * member must still populate DiskReport.errors.
     *
     * Why it exists: the previous `target.as_path() == Some(dev_path)`
     * comparison silently dropped error stats whenever btrfs reported a
     * row by an alternate path spelling (e.g. /dev/dm-N) -- the same
     * path-match blind spot that the alert pipeline used to suffer via
     * UnmappedDeviceError. This test pins the devid-based pairing so a
     * future revert to path matching cannot land silently.
     *
     * Scenario: pool device with mapper "braid-disk1" / devid 1; stats row
     * for devid 1 carries path "/dev/dm-0" (not "/dev/mapper/braid-disk1")
     * and read_io_errs = 5. The disk1 DiskReport must surface those 5
     * errors despite the path mismatch.
     */
    #[test]
    fn disk_report_pairs_stats_by_devid_when_path_differs() {
        use crate::parse::types::{BtrfsDeviceStatsOutput, DeviceErrorStats, DeviceStatsTarget};
        use crate::types::{DiskName, LuksUuid, MapperName, PoolDevice};

        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk1".to_owned()),
                luks_uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                devid: 1,
                underlying: "/dev/vda".to_owned(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };
        let config_disks = vec![ConfigDisk {
            name: DiskName::parse("disk1").unwrap(),
            by_id_path: ByIdPath::parse("/dev/disk/by-id/disk1").unwrap(),
            state: ConfigDiskState::PresentLuks {
                uuid: LuksUuid::parse("11111111-1111-1111-1111-111111111111").unwrap(),
                label: None,
                mapper_open: true,
            },
        }];
        // Stats row for devid 1 reports "/dev/dm-0", NOT "/dev/mapper/braid-disk1".
        // Old path-match code would have dropped this row.
        let stats = BtrfsDeviceStatsOutput {
            devices: vec![DeviceErrorStats {
                devid: 1,
                target: DeviceStatsTarget::Path("/dev/dm-0".to_owned()),
                read_io_errs: 5,
                write_io_errs: 0,
                flush_io_errs: 0,
                corruption_errs: 0,
                generation_errs: 0,
            }],
        };
        let runner = MockRunner::default();
        let membership = status_membership_1disk();

        let ctx = build_disk_reports(&runner, &membership, &config_disks, &pool, &stats);

        assert_eq!(ctx.disks.len(), 1);
        let errors = ctx.disks[0]
            .errors
            .as_ref()
            .expect("disk1 errors must be present despite mismatched stats path");
        assert_eq!(
            errors.read, 5,
            "stats row paired by devid must surface its read_io_errs"
        );
    }

    /*
     * Intent: resolve_alert_state surfaces a corrupt alert latch as an
     * active AlertState containing a single ComputationError cause, rather
     * than silently reporting "no alert".
     *
     * Why it exists: when the latch on disk is unparseable, the prior
     * implementation returned None and resolve_alert_state degraded to
     * "no alert", hiding the latched-until-ack invariant violation from
     * the user. status is the read-only surface for the latch -- it must
     * fail loud here even though it deliberately does NOT quarantine
     * (that's monitor's job). Asserting on the typed AlertCause variant
     * (not message substrings) follows the project's typed-error
     * convention.
     *
     * Scenario: external tampering or filesystem damage corrupts
     * /var/lib/braid/alert-latch.json between monitor cycles. Operator
     * runs `braid status`; they must see a clear corruption signal, not
     * an "all clear".
     */
    #[test]
    fn resolve_alert_state_surfaces_corrupt_latch_as_computation_error() {
        let (_tmp, paths) = isolated_paths();
        std::fs::write(paths.alert_latch_json(), b"not json").unwrap();

        let state = resolve_alert_state(&paths);

        assert!(state.active(), "corrupt latch must surface as active alert");
        assert!(
            matches!(
                state.causes.as_slice(),
                [AlertCause::ComputationError { .. }]
            ),
            "expected exactly one ComputationError cause, got {:?}",
            state.causes
        );
    }

    // Intent: resolve_alert_state surfaces a cleanup-pending sentinel as an
    //   active ComputationError even when no latch or smartd flag exists.
    // Why it exists: ack cleanup can fail after removing alert-latch.json, so
    //   status must still show that `braid ack` has cleanup work to resume.
    // Scenario: `braid ack` marked cleanup-pending, removed the latch, then hit
    //   an I/O error before clearing the marker. Operator runs `braid status`.
    #[test]
    fn resolve_alert_state_surfaces_cleanup_pending_as_computation_error() {
        let (_tmp, paths) = isolated_paths();
        std::fs::write(paths.alert_cleanup_pending(), b"").unwrap();

        let state = resolve_alert_state(&paths);

        assert!(
            state.active(),
            "cleanup-pending must surface as active alert"
        );
        let [AlertCause::ComputationError { detail }] = state.causes.as_slice() else {
            panic!(
                "expected exactly one ComputationError cause, got {:?}",
                state.causes
            );
        };
        assert!(
            detail.contains("ack cleanup pending"),
            "detail must name cleanup-pending state, got: {detail}"
        );
        assert!(
            detail.contains("braid ack"),
            "detail must point at ack recovery, got: {detail}"
        );
    }
}
