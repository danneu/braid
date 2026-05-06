use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::alert::{self, AlertCause, AlertState};
use crate::cmd::{CmdError, CmdRequest, CommandRunner, LsblkFieldKind};
use crate::config::{self, Config, mapper_name};
use crate::confirm::get_lsblk_field;
use crate::luks;
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
    let pool_mappers: HashSet<&str> = pool.devices.iter().map(|d| d.mapper.0.as_str()).collect();
    for pd in &pool.devices {
        let name = config::name_from_mapper(&pd.mapper.0)
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
    for name in membership.disks.keys() {
        let expected_mapper = format!("braid-{name}");
        if !pool_mappers.contains(expected_mapper.as_str()) {
            drives.push(CompactDrive {
                name: name.clone(),
                device_short: "-".to_owned(),
                devid: None,
                status: DiskStatus::Missing,
            });
        }
    }

    drives
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
    #[error("validation error: {0}")]
    Validation(String),
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
) -> Result<BuiltStatus, StatusError> {
    let advisories = luks::header_backup_advisories(paths);

    let pool = match probe_pool(runner, fs, config.mount_point()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return Ok(not_mounted_status(config, paths, advisories));
        }
        Err(e) => return Err(e.into()),
    };

    if !pool.mounted {
        return Ok(not_mounted_status(config, paths, advisories));
    }

    let membership = match membership::load_membership(paths) {
        Ok(m) => m,
        Err(membership::MembershipError::NotFound(_)) => PoolMembership::empty(),
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

    let config_disks: Vec<ConfigDisk> = membership
        .disks
        .iter()
        .map(|(name, member)| probe_config_disk(runner, fs, name, &member.by_id))
        .collect::<Result<Vec<_>, _>>()?;
    let device_stats = get_device_stats(runner, config.mount_point())?;
    let verbose_ctx = build_disk_reports(runner, &config_disks, &pool, &device_stats);

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
) -> Result<(), StatusError> {
    let built = build_status(runner, fs, config, paths)?;

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
            )
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Alert state (latch-based)
// ---------------------------------------------------------------------------

/// Read alert state from the latch file + smartd flag. Status reads the latch
/// instead of recomputing live alert state — the latch is the single source of
/// truth. Recomputing would cause the alert to disappear when a condition
/// resolves, contradicting the "latched until ack" model. The smartd flag is
/// checked as a bridge for between-cycle fires.
pub(crate) fn resolve_alert_state(paths: &StatePaths) -> AlertState {
    let smartd_active = alert::smartd_alert_active(paths);

    let latch = match alert::load_alert_latch(paths) {
        Ok(opt) => opt,
        Err(e) => {
            // Fail loud: don't pretend "no alert" when we can't read the
            // latch. Status is read-only -- never quarantine here; that is
            // monitor's job.
            let mut causes = vec![AlertCause::ComputationError {
                detail: format!("alert latch unreadable -- {e}"),
            }];
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
    config_disks: &[ConfigDisk],
    pool: &PoolState,
    device_stats: &BtrfsDeviceStatsOutput,
) -> VerboseContext {
    let pool_uuid_set: HashSet<&LuksUuid> = pool.devices.iter().map(|d| &d.luks_uuid).collect();
    let pool_mapper_set: HashSet<&str> = pool.devices.iter().map(|d| d.mapper.0.as_str()).collect();

    let mut disk_reports = Vec::new();
    let mut human_details = Vec::new();

    // Present pool devices
    for pd in &pool.devices {
        // Find matching config disk by LUKS UUID
        let matched_config = config_disks.iter().find(|cd| {
            matches!(&cd.state, ConfigDiskState::PresentLuks { uuid, .. } if uuid == &pd.luks_uuid)
        });

        let disk_name = matched_config.map(|cd| cd.name.clone()).unwrap_or_else(|| {
            // Derive name from mapper (strip braid- prefix)
            config::name_from_mapper(&pd.mapper.0)
                .unwrap_or(&pd.mapper.0)
                .to_owned()
        });

        let by_id = matched_config
            .map(|cd| cd.by_id_path.0.clone())
            .unwrap_or_else(|| format!("/dev/mapper/{}", pd.mapper.0));

        let mapper = pd.mapper.0.clone();

        // Model/serial via lsblk (tolerant)
        let model = get_lsblk_field(runner, &by_id, LsblkFieldKind::Model);
        let serial = get_lsblk_field(runner, &by_id, LsblkFieldKind::Serial);

        // Error stats. Pair by devid (canonical identity) -- the stats row's
        // path can differ from the canonical mapper path without changing
        // which physical device it describes.
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
            luks_uuid: pd.luks_uuid.0.clone(),
            devid: Some(pd.devid.to_string()),
            underlying: Some(pd.underlying.clone()),
            status: DiskStatus::Present,
            errors: errors.clone(),
        });

        human_details.push(HumanDisk {
            name: disk_name,
            by_id: by_id.clone(),
            luks_uuid: pd.luks_uuid.0.clone(),
            devid: Some(pd.devid.to_string()),
            status: DiskStatus::Present,
            model,
            serial,
            errors,
        });
    }

    // Unpooled config disks (not matched to pool)
    for cd in config_disks {
        let is_unpooled = match &cd.state {
            ConfigDiskState::Absent => true,
            ConfigDiskState::PresentLuks { uuid, .. } => !pool_uuid_set.contains(uuid),
            ConfigDiskState::PresentNotLuks => {
                !pool_mapper_set.contains(mapper_name(&cd.name).0.as_str())
            }
        };

        if !is_unpooled {
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
                match luks::probe_luks_header(runner, &cd.by_id_path.0) {
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
            name: cd.name.clone(),
            mapper: mapper.clone(),
            by_id: cd.by_id_path.0.clone(),
            luks_uuid: String::new(),
            devid: None,
            underlying: None,
            status,
            errors: None,
        });

        human_details.push(HumanDisk {
            name: cd.name.clone(),
            by_id: cd.by_id_path.0.clone(),
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

fn devid_to_name(report: &StatusReport, devid: u64) -> String {
    let key = devid.to_string();
    report
        .disks
        .iter()
        .find(|d| d.devid.as_deref() == Some(&key))
        .map(|d| format!("{} (devid {devid})", d.name))
        .unwrap_or_else(|| format!("devid {devid}"))
}

fn format_status_human(
    report: &StatusReport,
    compact_drives: Option<&[CompactDrive]>,
    human_disks: Option<&[HumanDisk]>,
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
                    let name = devid_to_name(report, *devid);
                    out.push_str(&format!("  - btrfs device errors on {name}\n"));
                }
                AlertCause::MissingDevice { devid } => {
                    let name = devid_to_name(report, *devid);
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
                out.push_str(&format!(
                    "    Action:  add replacement disk to config, then: braid replace --old {} --new <new-name>\n",
                    d.name
                ));
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
    use crate::state_paths::StatePaths;
    use std::collections::BTreeMap;

    struct MockFs {
        paths: Vec<String>,
        block_devices: Vec<String>,
        mountinfo: String,
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                block_devices: vec![],
                mountinfo: "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/disk1 rw\n"
                    .to_string(),
            }
        }

        fn not_mounted(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                block_devices: vec![],
                mountinfo: "26 25 0:23 / / rw shared:1 - ext4 /dev/sda1 rw\n".to_string(),
            }
        }

        fn ext4(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                block_devices: vec![],
                mountinfo: "36 35 0:32 / /mnt/storage rw shared:1 - ext4 /dev/sda1 rw\n"
                    .to_string(),
            }
        }
    }

    impl Filesystem for MockFs {
        fn exists(&self, path: &str) -> bool {
            self.paths.contains(&path.to_string())
        }

        fn is_block_device(&self, path: &str) -> bool {
            self.block_devices.contains(&path.to_string())
        }

        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path == "/proc/self/mountinfo" {
                return Ok(self.mountinfo.clone());
            }
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    fn ok_raw(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
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

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
    }

    fn report_with_scrub(last_scrub: ScrubReport) -> StatusReport {
        StatusReport {
            mount_point: MountPoint("/mnt/storage".to_owned()),
            status: StatusCode::Intact,
            total_devices: Some(3),
            present_count: Some(3),
            missing_count: Some(0),
            profile: Some("RAID1".to_owned()),
            capacity: None,
            last_scrub: Some(last_scrub),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
            missing_devids: vec![],
        }
    }

    // --- Mock data builders ---

    fn btrfs_show_1disk() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 1 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/disk1\n",
        )
    }

    fn btrfs_show_3disk() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/disk2\n\
             \tdevid    3 size 10.00GiB used 2.00GiB path /dev/mapper/disk3\n",
        )
    }

    fn btrfs_show_3disk_1missing() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/disk2\n\
             \t*** Some devices missing\n",
        )
    }

    fn btrfs_show_3disk_1null_underlying_1missing() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 3 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/disk1\n\
             \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/disk2\n\
             \tdevid    3 size 0 used 0 path MISSING\n\
             \t*** Some devices missing\n",
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

    fn btrfs_df_single() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem df",
            r#"{
  "filesystem-df": [
    { "bg-type": "Data", "bg-profile": "single", "total": 1073741824, "used": 536870912 },
    { "bg-type": "Metadata", "bg-profile": "single", "total": 268435456, "used": 65536 },
    { "bg-type": "System", "bg-profile": "single", "total": 4194304, "used": 16384 }
  ]
}"#,
        )
    }

    fn btrfs_df_raid1() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem df",
            r#"{
  "filesystem-df": [
    { "bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216 },
    { "bg-type": "System", "bg-profile": "RAID1", "total": 4194304, "used": 16384 },
    { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 65536 },
    { "bg-type": "GlobalReserve", "bg-profile": "single", "total": 3670016, "used": 0 }
  ]
}"#,
        )
    }

    fn btrfs_usage_raw() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem usage",
            "Overall:\n\
             \tDevice size:\t\t\t1040187392\n\
             \tDevice allocated:\t\t503316480\n\
             \tDevice unallocated:\t\t536870912\n\
             \tUsed:\t\t\t\t33914880\n\
             \tFree (estimated):\t\t442957824\t(min: 442957824)\n\
             \tData ratio:\t\t\t2.00\n",
        )
    }

    fn btrfs_device_usage_raw_3disk() -> RawCommandOutput {
        ok_raw(
            "btrfs device usage",
            "/dev/mapper/disk1, ID: 1\n\
             \x20  Device size:          346729130\n\
             \x20  Device slack:              0\n\
             \x20  Data,RAID1:           67108864\n\
             \x20  Metadata,RAID1:       33554432\n\
             \x20  System,RAID1:          4194304\n\
             \x20  Unallocated:         241871530\n\
             \n\
             /dev/mapper/disk2, ID: 2\n\
             \x20  Device size:          346729130\n\
             \x20  Device slack:              0\n\
             \x20  Data,RAID1:           67108864\n\
             \x20  Metadata,RAID1:       33554432\n\
             \x20  System,RAID1:          4194304\n\
             \x20  Unallocated:         241871530\n\
             \n\
             /dev/mapper/disk3, ID: 3\n\
             \x20  Device size:          346729130\n\
             \x20  Device slack:              0\n\
             \x20  Data,RAID1:           67108864\n\
             \x20  Metadata,RAID1:       33554432\n\
             \x20  System,RAID1:          4194304\n\
             \x20  Unallocated:         241871530\n",
        )
    }

    fn btrfs_device_usage_raw_1disk() -> RawCommandOutput {
        ok_raw(
            "btrfs device usage",
            "/dev/mapper/disk1, ID: 1\n\
             \x20  Device size:         1040187392\n\
             \x20  Device slack:              0\n\
             \x20  Data,single:         1073741824\n\
             \x20  Metadata,single:      268435456\n\
             \x20  System,single:          4194304\n\
             \x20  Unallocated:                 0\n",
        )
    }

    fn btrfs_scrub_never() -> RawCommandOutput {
        ok_raw(
            "btrfs scrub status --raw",
            "UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\nScrub started:    no stats available\n",
        )
    }

    fn btrfs_scrub_finished() -> RawCommandOutput {
        ok_raw(
            "btrfs scrub status --raw",
            "UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             Scrub started:    Mon Feb 23 10:00:00 2026\n\
             Status:           finished\n\
             Duration:         0:00:01\n\
             Total to scrub:   1073741824\n\
             Rate:             1073741824/s\n\
             Error summary:    no errors found\n",
        )
    }

    fn btrfs_scrub_finished_with_errors() -> RawCommandOutput {
        ok_raw(
            "btrfs scrub status --raw",
            "UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             Scrub started:    Mon Feb 23 10:00:00 2026\n\
             Status:           finished\n\
             Duration:         0:00:01\n\
             Total to scrub:   1073741824\n\
             Rate:             1073741824/s\n\
             Error summary:    csum=50\n",
        )
    }

    fn btrfs_scrub_aborted() -> RawCommandOutput {
        ok_raw(
            "btrfs scrub status --raw",
            "UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             Scrub started:    Mon Feb 23 10:00:00 2026\n\
             Status:           aborted\n\
             Duration:         0:00:01\n\
             Total to scrub:   1073741824\n\
             Rate:             1073741824/s\n\
             Error summary:    no errors found\n",
        )
    }

    fn btrfs_scrub_interrupted() -> RawCommandOutput {
        ok_raw(
            "btrfs scrub status --raw",
            "UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             Scrub started:    Mon Feb 23 10:00:00 2026\n\
             Status:           interrupted\n\
             Duration:         0:00:01\n\
             Total to scrub:   1073741824\n\
             Rate:             1073741824/s\n\
             Error summary:    no errors found\n",
        )
    }

    fn btrfs_device_stats_3disk() -> RawCommandOutput {
        ok_raw(
            "btrfs device stats",
            r#"{"device-stats": [
                {"device": "/dev/mapper/disk1", "devid": 1, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
                {"device": "/dev/mapper/disk2", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
                {"device": "/dev/mapper/disk3", "devid": 3, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
            ]}"#,
        )
    }

    fn lsblk_field_ok(cmd: &str, value: &str) -> RawCommandOutput {
        ok_raw(cmd, &format!("{value}\n"))
    }

    fn test_paths() -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        (tmp, paths)
    }

    fn config_3disk() -> Config {
        Config::new(MountPoint("/mnt/storage".to_owned())).unwrap()
    }

    fn config_1disk() -> Config {
        Config::new(MountPoint("/mnt/storage".to_owned())).unwrap()
    }

    fn membership_1disk() -> PoolMembership {
        let mut disks = BTreeMap::new();
        disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/disk1".to_owned())),
        );
        PoolMembership { disks }
    }

    /// Build a MockRunner for a 3-disk mounted healthy pool (base probes, no per-disk detail).
    fn runner_healthy_3disk_base() -> MockRunner {
        MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_show_3disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "disk1".into(),
                },
                cryptsetup_status_active("disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "disk2".into(),
                },
                cryptsetup_status_active("disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "disk3".into(),
                },
                cryptsetup_status_active("disk3", "/dev/vdc"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into(),
                },
                cryptsetup_uuid_ok("/dev/vdc", "33333333-3333-3333-3333-333333333333"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_df_raid1(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_usage_raw(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_device_usage_raw_3disk(),
            )
            .with_output(
                CmdRequest::BtrfsScrubStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_scrub_never(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceStatsJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_device_stats_3disk(),
            )
    }

    /// Extend a base runner with verbose probe outputs for 3-disk config.
    fn runner_healthy_3disk_verbose(runner: MockRunner) -> MockRunner {
        runner
            // probe_config_disk for each disk
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk3".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk3",
                    "33333333-3333-3333-3333-333333333333",
                ),
            )
            // device stats
            .with_output(
                CmdRequest::BtrfsDeviceStatsJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_device_stats_3disk(),
            )
            // lsblk model/serial for each disk
            .with_output(
                CmdRequest::LsblkField {
                    device: "/dev/disk/by-id/disk1".into(),
                    field: LsblkFieldKind::Model,
                },
                lsblk_field_ok("lsblk", "VBOX HARDDISK"),
            )
            .with_output(
                CmdRequest::LsblkField {
                    device: "/dev/disk/by-id/disk1".into(),
                    field: LsblkFieldKind::Serial,
                },
                lsblk_field_ok("lsblk", "disk1"),
            )
            .with_output(
                CmdRequest::LsblkField {
                    device: "/dev/disk/by-id/disk2".into(),
                    field: LsblkFieldKind::Model,
                },
                lsblk_field_ok("lsblk", "VBOX HARDDISK"),
            )
            .with_output(
                CmdRequest::LsblkField {
                    device: "/dev/disk/by-id/disk2".into(),
                    field: LsblkFieldKind::Serial,
                },
                lsblk_field_ok("lsblk", "disk2"),
            )
            .with_output(
                CmdRequest::LsblkField {
                    device: "/dev/disk/by-id/disk3".into(),
                    field: LsblkFieldKind::Model,
                },
                lsblk_field_ok("lsblk", "VBOX HARDDISK"),
            )
            .with_output(
                CmdRequest::LsblkField {
                    device: "/dev/disk/by-id/disk3".into(),
                    field: LsblkFieldKind::Serial,
                },
                lsblk_field_ok("lsblk", "disk3"),
            )
    }

    fn fs_3disk() -> MockFs {
        MockFs::new(&[
            "/dev/disk/by-id/disk1",
            "/dev/disk/by-id/disk2",
            "/dev/disk/by-id/disk3",
            "/dev/mapper/disk1",
            "/dev/mapper/disk2",
            "/dev/mapper/disk3",
        ])
    }

    fn fs_1disk() -> MockFs {
        MockFs::new(&["/dev/disk/by-id/disk1", "/dev/mapper/disk1"])
    }

    // =======================================================================
    // Schema envelope tests
    // =======================================================================

    #[test]
    fn status_json_not_mounted() {
        let runner = MockRunner::default();
        let fs = MockFs::not_mounted(&[]);
        let config = config_3disk();

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
        let (_tmp, paths) = test_paths();
        let _ = cmd_status(&runner, &fs, &config, false, &paths);
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
        let tmpdir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(tmpdir.path().to_path_buf());
        let config = config_3disk();

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
        let runner = runner_healthy_3disk_base();
        let config = config_3disk();

        let df = fetch_df(&runner, &mp()).unwrap();
        let df_summary = summarize_df(&df);
        let capacity = get_capacity(&runner, &mp(), 0, &df).unwrap();
        let last_scrub = get_scrub_report(&runner, &mp());

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
        let human = format_status_human(&report, None, None);
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
        let human = format_status_human(&report, Some(&compact), None);
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
        let human = format_status_human(&report, Some(&compact), None);
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
        let human = format_status_human(&report, Some(&compact), None);
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
        let human = format_status_human(&report, None, None);
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

        let human = format_status_human(&report, None, Some(&human_disks));
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

        let human = format_status_human(&report, None, Some(&human_disks));
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

        let human = format_status_human(&report, None, Some(&human_disks));
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

        let human = format_status_human(&report, None, Some(&human_disks));
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

        let human = format_status_human(&report, None, Some(&human_disks));
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
            btrfs_scrub_finished(),
        );
        let result = get_scrub_report(&runner, &mp());
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
            btrfs_scrub_finished_with_errors(),
        );
        let result = get_scrub_report(&runner, &mp());
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
            btrfs_scrub_aborted(),
        );
        let result = get_scrub_report(&runner, &mp());
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
            btrfs_scrub_interrupted(),
        );
        let result = get_scrub_report(&runner, &mp());
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
            err_raw("btrfs scrub status", 1, "some error"),
        );
        let result = get_scrub_report(&runner, &mp());
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
        let report = report_with_scrub(ScrubReport::Finished {
            started_at: "Mon Feb 23 10:00:00 2026".to_owned(),
            error_count: 0,
        });
        let human = format_status_human(&report, None, None);
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
        let report = report_with_scrub(ScrubReport::Finished {
            started_at: "Mon Feb 23 10:00:00 2026".to_owned(),
            error_count: 3,
        });
        let human = format_status_human(&report, None, None);
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
        let report = report_with_scrub(ScrubReport::Aborted {
            started_at: "Mon Feb 23 10:00:00 2026".to_owned(),
            error_count: 0,
        });
        let human = format_status_human(&report, None, None);
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
        let report = report_with_scrub(ScrubReport::Interrupted {
            started_at: "Mon Feb 23 10:00:00 2026".to_owned(),
            error_count: 0,
        });
        let human = format_status_human(&report, None, None);
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
            ok_raw(
                "btrfs balance status",
                "No balance found on '/mnt/storage'\n",
            ),
        );
        assert_eq!(get_balance_report(&runner, &mp()), BalanceReport::Idle);
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
            get_balance_report(&runner, &mp()),
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
            get_balance_report(&runner, &mp()),
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
            err_raw("btrfs balance status", 2, "ERROR: not a btrfs filesystem"),
        );
        assert_eq!(get_balance_report(&runner, &mp()), BalanceReport::Unknown);
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
        let warned = emit_paused_balance_warning(&runner, &mp(), &mut buf);
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
            ok_raw(
                "btrfs balance status",
                "No balance found on '/mnt/storage'\n",
            ),
        );
        let mut buf = Vec::new();
        let warned = emit_paused_balance_warning(&runner, &mp(), &mut buf);
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
        let human = format_status_human(&report, None, None);
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
        let human = format_status_human(&report, None, None);
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
        let human = format_status_human(&report, None, None);
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
            err_raw("btrfs filesystem df", 1, "not a btrfs filesystem"),
        );
        let result = fetch_df(&runner, &mp());
        assert!(result.is_err());
    }

    #[test]
    fn status_usage_failure_fatal() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemUsageRaw {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("btrfs filesystem usage", 1, "error"),
        );
        let df = BtrfsDfOutput { entries: vec![] };
        let result = get_capacity(&runner, &mp(), 0, &df);
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
                CmdRequest::BtrfsFilesystemUsageRaw { mount_point: mp() },
                ok_raw(
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
                CmdRequest::BtrfsDeviceUsageRaw { mount_point: mp() },
                ok_raw(
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

        let report = get_capacity(&runner, &mp(), 0, &df).unwrap();

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
            err_raw("btrfs device stats", 1, "error"),
        );
        let result = get_device_stats(&runner, &mp());
        assert!(result.is_err());
    }

    #[test]
    fn status_not_btrfs_maps_to_not_mounted() {
        let runner = MockRunner::default();
        let fs = MockFs::ext4(&[]);
        let config = config_3disk();

        // cmd_status should succeed (not error), treating it as not-mounted
        let (_tmp, paths) = test_paths();
        let result = cmd_status(&runner, &fs, &config, false, &paths);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
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
        let fs = MockFs::not_mounted(&[]);
        let config = config_3disk();

        let (_tmp, paths) = test_paths();
        let result = cmd_status(&runner, &fs, &config, false, &paths);
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    #[test]
    fn cmd_status_healthy_ok() {
        let runner = runner_healthy_3disk_verbose(runner_healthy_3disk_base());
        let fs = fs_3disk();
        let config = config_3disk();

        let (_tmp, paths) = test_paths();
        let result = cmd_status(&runner, &fs, &config, false, &paths);
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_healthy_json_ok() {
        let runner = runner_healthy_3disk_verbose(runner_healthy_3disk_base());
        let fs = fs_3disk();
        let config = config_3disk();

        let (_tmp, paths) = test_paths();
        let result = cmd_status(&runner, &fs, &config, true, &paths);
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_degraded_ok() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_show_3disk_1missing(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "disk1".into(),
                },
                cryptsetup_status_active("disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "disk2".into(),
                },
                cryptsetup_status_active("disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdb".into(),
                },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_df_raid1(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_usage_raw(),
            )
            .with_output(
                CmdRequest::BtrfsScrubStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_scrub_never(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceStatsJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_device_stats_3disk(),
            )
            // probe_config_disk for each config disk (by-id path)
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk1".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk2".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk2",
                    "22222222-2222-2222-2222-222222222222",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/disk3".into(),
                },
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk3",
                    "33333333-3333-3333-3333-333333333333",
                ),
            );
        let fs = fs_3disk();
        let config = config_3disk();

        let (_tmp, paths) = test_paths();
        let result = cmd_status(&runner, &fs, &config, false, &paths);
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
        let runner = runner_healthy_3disk_base()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_show_3disk_1null_underlying_1missing(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "disk2".into(),
                },
                cryptsetup_status_active("disk2", "(null)"),
            );
        let fs = fs_3disk();
        let config = config_3disk();

        let (_tmp, paths) = test_paths();
        membership::save_membership(&PoolMembership::empty(), &paths).unwrap();

        let built = build_status(&runner, &fs, &config, &paths).unwrap();

        assert_eq!(built.report.missing_count, Some(2));
        assert_eq!(built.report.missing_devids, vec![2, 3]);
        assert_eq!(
            built.report.missing_devids.len(),
            built.report.missing_count.unwrap() as usize
        );
    }

    #[test]
    fn cmd_status_single_disk_ok() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_show_1disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "disk1".into(),
                },
                cryptsetup_status_active("disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_df_single(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_usage_raw(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_device_usage_raw_1disk(),
            )
            .with_output(
                CmdRequest::BtrfsScrubStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_scrub_never(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceStatsJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw(
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
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            );
        let fs = fs_1disk();
        let config = config_1disk();

        let (_tmp, paths) = test_paths();
        let result = cmd_status(&runner, &fs, &config, false, &paths);
        assert!(result.is_ok());
    }

    // =======================================================================
    // build_disk_reports: PresentNotLuks classification
    // =======================================================================

    fn pool_empty() -> PoolState {
        PoolState {
            mounted: true,
            devices: vec![],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 0,
            fsid: None,
            null_underlying: vec![],
        }
    }

    fn cfg_present_not_luks(name: &str, by_id: &str) -> Vec<ConfigDisk> {
        vec![ConfigDisk {
            name: name.to_owned(),
            by_id_path: ByIdPath(by_id.to_owned()),
            state: ConfigDiskState::PresentNotLuks,
        }]
    }

    fn is_luks_raw(device: &str, exit: i32, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup isLuks {device}"),
            stdout: String::new(),
            stderr: stderr.to_owned(),
            exit_status: exit,
        }
    }

    fn luks_dump_text_raw(device: &str, exit: i32, stdout: &str, stderr: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: format!("cryptsetup luksDump {device}"),
            stdout: stdout.to_owned(),
            stderr: stderr.to_owned(),
            exit_status: exit,
        }
    }

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
        let config_disks = cfg_present_not_luks("disk1", "/dev/disk/by-id/disk1");
        let runner = MockRunner::default();
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(&runner, &config_disks, &pool_empty(), &stats);
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
        let config_disks = cfg_present_not_luks("disk1", "/dev/disk/by-id/disk1");
        let runner = MockRunner::default().with_output(
            CmdRequest::CryptsetupIsLuks {
                device: "/dev/disk/by-id/disk1".to_owned(),
            },
            is_luks_raw(
                "/dev/disk/by-id/disk1",
                1,
                "Device /dev/disk/by-id/disk1 is not a valid LUKS device.\n",
            ),
        );
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(&runner, &config_disks, &pool_empty(), &stats);
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
        let config_disks = cfg_present_not_luks("disk1", "/dev/disk/by-id/disk1");
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/disk1".to_owned(),
                },
                is_luks_raw("/dev/disk/by-id/disk1", 0, ""),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk1".to_owned(),
                },
                luks_dump_text_raw(
                    "/dev/disk/by-id/disk1",
                    1,
                    "",
                    "Cannot read LUKS header metadata.\n",
                ),
            );
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(&runner, &config_disks, &pool_empty(), &stats);
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
        let config_disks = cfg_present_not_luks("disk1", "/dev/disk/by-id/disk1");
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/disk1".to_owned(),
                },
                is_luks_raw("/dev/disk/by-id/disk1", 0, ""),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk1".to_owned(),
                },
                luks_dump_text_raw(
                    "/dev/disk/by-id/disk1",
                    0,
                    "LUKS header information\nVersion: 2\n",
                    "",
                ),
            );
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(&runner, &config_disks, &pool_empty(), &stats);
        assert_eq!(ctx.disks.len(), 1);
        assert_eq!(ctx.disks[0].status, DiskStatus::Unknown);
    }

    /*
     * Intent: when a config disk's by_id LUKS header probe failed
     * (PresentNotLuks) but the mapper is already open and in the pool,
     * build_disk_reports must emit exactly one Present row for that disk in
     * both the JSON disks array and the verbose human output, not a duplicate
     * Unknown/LuksHeader* row from the unpooled fall-through.
     *
     * Why it exists: the unpooled loop keys on LUKS UUID, but a
     * PresentNotLuks config disk has no UUID to match, so the dedup misses
     * without an explicit mapper-name guard. Pinning both JSON and human
     * output prevents drift between disks and human_details.
     *
     * Scenario: pool device "braid-disk1" / uuid U1 / devid 1; config disk
     * "disk1" with state PresentNotLuks. No CryptsetupIsLuks/LuksDumpText
     * mocks exist, so probe_luks_header would return ProbeFailed if it ran.
     */
    #[test]
    fn build_disk_reports_skips_unpooled_row_when_mapper_in_pool_for_present_not_luks() {
        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk1".to_owned()),
                luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".to_owned()),
                devid: 1,
                underlying: "/dev/vda".to_owned(),
            }],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 1,
            fsid: None,
            null_underlying: vec![],
        };
        let config_disks = cfg_present_not_luks("disk1", "/dev/disk/by-id/disk1");
        let runner = MockRunner::default();
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(&runner, &config_disks, &pool, &stats);

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
        let human = format_status_human(&report, None, Some(&ctx.human_details));

        assert!(human.contains("disk1"), "got:\n{human}");
        assert!(
            !human.contains("UNKNOWN"),
            "duplicate Unknown row leaked; got:\n{human}"
        );
        assert!(!human.contains("LUKS HEADER UNREADABLE"), "got:\n{human}");
        assert!(!human.contains("LUKS HEADER DAMAGED"), "got:\n{human}");
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
        let membership = membership_1disk();
        let drives = build_compact_drives(&pool, &membership);
        assert_eq!(drives.len(), 1);
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

        let human = format_status_human(&report, None, Some(&human_disks));
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

    fn report_with_alerts(disks: Vec<DiskReport>, causes: Vec<AlertCause>) -> StatusReport {
        StatusReport {
            mount_point: MountPoint("/mnt/storage".into()),
            status: StatusCode::Degraded,
            total_devices: Some(3),
            present_count: Some(2),
            missing_count: Some(1),
            profile: None,
            capacity: None,
            last_scrub: None,
            balance: None,
            allocation: None,
            disks,
            advisories: vec![],
            alert_active: true,
            alert_causes: causes,
            missing_devids: vec![],
        }
    }

    fn disk_report_named(name: &str, devid: u64) -> DiskReport {
        DiskReport {
            name: name.into(),
            mapper: format!("braid-{name}"),
            by_id: format!("/dev/disk/by-id/{name}"),
            luks_uuid: "00000000-0000-0000-0000-000000000000".into(),
            devid: Some(devid.to_string()),
            underlying: None,
            status: DiskStatus::Present,
            errors: None,
        }
    }

    #[test]
    fn alert_missing_device_shows_name() {
        let disks = vec![
            disk_report_named("aaa", 1),
            disk_report_named("bbb", 2),
            disk_report_named("ccc", 3),
        ];
        let report = report_with_alerts(disks, vec![AlertCause::MissingDevice { devid: 3 }]);
        let human = format_status_human(&report, None, None);
        assert!(
            human.contains("missing device: ccc (devid 3)"),
            "expected device name in alert, got:\n{human}"
        );
    }

    #[test]
    fn alert_btrfs_errors_shows_name() {
        let disks = vec![disk_report_named("aaa", 1), disk_report_named("bbb", 2)];
        let report = report_with_alerts(disks, vec![AlertCause::BtrfsDeviceErrors { devid: 1 }]);
        let human = format_status_human(&report, None, None);
        assert!(
            human.contains("btrfs device errors on aaa (devid 1)"),
            "expected device name in alert, got:\n{human}"
        );
    }

    #[test]
    fn alert_unknown_devid_falls_back() {
        let disks = vec![disk_report_named("aaa", 1)];
        let report = report_with_alerts(disks, vec![AlertCause::MissingDevice { devid: 99 }]);
        let human = format_status_human(&report, None, None);
        assert!(
            human.contains("missing device: devid 99"),
            "unknown devid should fall back to raw id, got:\n{human}"
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
        let tmpdir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(tmpdir.path().to_path_buf());
        std::fs::write(paths.pool_json(), "not valid json {{{").unwrap();

        let runner = runner_healthy_3disk_base();
        let fs = fs_3disk();
        let config = config_3disk();

        let result = cmd_status(&runner, &fs, &config, false, &paths);
        assert!(result.is_err(), "expected error for corrupt pool.json");
        assert!(
            matches!(
                result.unwrap_err(),
                StatusError::Membership(membership::MembershipError::Corrupt(_, _))
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
        let tmpdir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(tmpdir.path().to_path_buf());
        std::fs::write(paths.pool_json(), "not valid json {{{").unwrap();

        let runner = MockRunner::default();
        let fs = MockFs::not_mounted(&[]);
        let config = config_3disk();

        let result = cmd_status(&runner, &fs, &config, false, &paths);
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

        let tmpdir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(tmpdir.path().to_path_buf());

        let mut disks = BTreeMap::new();
        disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/disk1".to_owned())),
        );
        let membership = PoolMembership { disks };

        // Healthy 1-disk mounted pool for probe_pool + data-gathering; the
        // pool-side mapper "disk1" is distinct from the config-side mapper
        // "braid-disk1" and does not collide.
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_show_1disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "disk1".into(),
                },
                cryptsetup_status_active("disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_df_single(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_usage_raw(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_device_usage_raw_1disk(),
            )
            .with_output(
                CmdRequest::BtrfsScrubStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                btrfs_scrub_never(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceStatsJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw(
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
                cryptsetup_uuid_ok(
                    "/dev/disk/by-id/disk1",
                    "11111111-1111-1111-1111-111111111111",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/disk1".into(),
                },
                ok_raw(
                    "cryptsetup luksDump",
                    "LUKS header information\nVersion:       \t2\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vdz"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdz".into(),
                },
                cryptsetup_uuid_ok("/dev/vdz", "99999999-9999-9999-9999-999999999999"),
            );

        let fs = fs_1disk();
        let config = config_1disk();

        membership::save_membership(&membership, &paths).unwrap();

        let result = build_status(&runner, &fs, &config, &paths);
        match result {
            Err(StatusError::Probe(ProbeError::MapperConflict {
                name,
                expected,
                found,
            })) => {
                assert_eq!(name, "disk1");
                assert_eq!(
                    expected,
                    LuksUuid("11111111-1111-1111-1111-111111111111".into())
                );
                assert_eq!(
                    found,
                    Some(LuksUuid("99999999-9999-9999-9999-999999999999".into()))
                );
            }
            Err(other) => panic!("expected StatusError::Probe(MapperConflict), got: {other:?}"),
            Ok(_) => panic!("expected StatusError::Probe(MapperConflict), got Ok"),
        }
    }

    /*
     * Intent: build_disk_reports pairs btrfs device-stats rows to DiskReport
     * by devid, not by mapper-path string. A stats row whose path differs
     * from the canonical /dev/mapper/braid-X but whose devid matches a pool
     * member must still populate DiskReport.errors.
     *
     * Why it exists: the previous `target.as_path() == Some(dev_path)`
     * comparison silently dropped error stats whenever btrfs reported a
     * row by an alternate path spelling (e.g. /dev/dm-N) -- the same
     * identity weakness that the alert pipeline used to suffer via
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
        use crate::types::{LuksUuid, MapperName, PoolDevice};

        let pool = PoolState {
            mounted: true,
            devices: vec![PoolDevice {
                mapper: MapperName("braid-disk1".to_owned()),
                luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".to_owned()),
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
            name: "disk1".to_owned(),
            by_id_path: ByIdPath("/dev/disk/by-id/disk1".to_owned()),
            state: ConfigDiskState::PresentLuks {
                uuid: LuksUuid("11111111-1111-1111-1111-111111111111".to_owned()),
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

        let ctx = build_disk_reports(&runner, &config_disks, &pool, &stats);

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
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().to_path_buf());
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
}
