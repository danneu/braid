use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::alert::{self, AlertCause, AlertState};
use crate::cmd::{CmdError, CmdRequest, CommandRunner, LsblkFieldKind};
use crate::config::{self, mapper_name, Config};
use crate::luks;
use crate::membership::{self, PoolMembership};
use crate::parse::types::BalanceState;
use crate::parse::{
    parse_btrfs_balance_status, parse_btrfs_device_stats, parse_btrfs_device_usage,
    parse_btrfs_df_json, parse_btrfs_filesystem_usage, parse_btrfs_scrub_status, parse_lsblk_field,
    BtrfsDeviceStatsOutput, ParseError, ScrubState,
};
use crate::probe::{probe_config_disk, probe_pool, Filesystem, ProbeError};
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
    pub last_scrub: Option<String>,
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
pub struct DiskReport {
    pub name: String,
    pub mapper: String,
    pub by_id: String,
    pub luks_uuid: String,
    pub devid: Option<String>,
    pub underlying: Option<String>,
    pub status: String,
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

// ---------------------------------------------------------------------------
// Compact drive (always-on summary)
// ---------------------------------------------------------------------------

struct CompactDrive {
    name: String,
    device_short: String,
    devid: Option<u64>,
    status: &'static str,
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
            status: "present",
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
                status: "missing",
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
    status: String,
    model: Option<String>,
    serial: Option<String>,
    errors: Option<DiskErrors>,
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

pub fn build_status_report<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    config: &Config,
    membership: &PoolMembership,
    paths: &StatePaths,
) -> Result<StatusReport, StatusError> {
    let advisories = luks::header_backup_advisories(paths);

    let pool = match probe_pool(runner, config.mount_point().as_str()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            let code = StatusCode::NotMounted;
            let alert_state = resolve_alert_state(paths);
            return Ok(StatusReport {
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
                advisories: advisories.clone(),
                alert_active: alert_state.active,
                alert_causes: alert_state.causes,
            });
        }
        Err(e) => return Err(e.into()),
    };

    if !pool.mounted {
        let code = StatusCode::NotMounted;
        let alert_state = resolve_alert_state(paths);
        return Ok(StatusReport {
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
            advisories: advisories.clone(),
            alert_active: alert_state.active,
            alert_causes: alert_state.causes,
        });
    }

    let df_summary = summarize_df(runner, config.mount_point().as_str())?;
    let capacity = get_capacity(runner, config.mount_point().as_str(), pool.missing_count)?;
    let last_scrub = get_scrub_string(runner, config.mount_point().as_str());
    let balance = get_balance_report(runner, config.mount_point().as_str());

    let code = if pool.missing_count == 0 {
        StatusCode::Intact
    } else {
        StatusCode::Degraded
    };

    let config_disks: Vec<ConfigDisk> = membership
        .disks
        .iter()
        .map(|(name, member)| probe_config_disk(runner, fs, name, &member.by_id))
        .collect::<Result<Vec<_>, _>>()?;
    let device_stats = get_device_stats(runner, config.mount_point().as_str())?;
    let verbose_ctx = build_disk_reports(runner, &config_disks, &pool, &device_stats);

    let alert_state = resolve_alert_state(paths);

    let present_count = pool.total_devices.saturating_sub(pool.missing_count);
    Ok(StatusReport {
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
        alert_active: alert_state.active,
        alert_causes: alert_state.causes,
    })
}

pub fn cmd_status<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    config: &Config,
    json: bool,
    paths: &StatePaths,
) -> Result<(), StatusError> {
    let advisories = luks::header_backup_advisories(paths);

    // 1. Probe pool, mapping NotBtrfs to not-mounted
    let pool = match probe_pool(runner, config.mount_point().as_str()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => PoolState {
            mounted: false,
            devices: vec![],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 0,
            fsid: None,
        },
        Err(e) => return Err(e.into()),
    };

    // 2. Not mounted → minimal report
    if !pool.mounted {
        let code = StatusCode::NotMounted;
        let alert_state = resolve_alert_state(paths);
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
            advisories: advisories.clone(),
            alert_active: alert_state.active,
            alert_causes: alert_state.causes,
        };
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", format_status_human(&report, None, None));
        }
        return Ok(());
    }

    // 3. Membership load (try_load pattern)
    let membership_result = membership::load_membership(paths);
    let membership = match &membership_result {
        Ok(m) => m.clone(),
        Err(_) => PoolMembership::empty(),
    };

    // 4. Strict data gathering
    let df_summary = summarize_df(runner, config.mount_point().as_str())?;
    let capacity = get_capacity(runner, config.mount_point().as_str(), pool.missing_count)?;
    let last_scrub = get_scrub_string(runner, config.mount_point().as_str());
    let balance = get_balance_report(runner, config.mount_point().as_str());

    // 5. Compute status code
    let code = if pool.missing_count == 0 {
        StatusCode::Intact
    } else {
        StatusCode::Degraded
    };

    // 6. Compact drives (always built when mounted)
    let compact_drives = build_compact_drives(&pool, &membership);

    // 7. Alert state (latch-based)
    let alert_state = resolve_alert_state(paths);

    // 8. Per-disk detail
    let verbose_ctx = {
        let device_stats = get_device_stats(runner, config.mount_point().as_str())?;
        let config_disks: Vec<ConfigDisk> = membership
            .disks
            .iter()
            .map(|(name, member)| probe_config_disk(runner, fs, name, &member.by_id))
            .collect::<Result<Vec<_>, _>>()?;
        build_disk_reports(runner, &config_disks, &pool, &device_stats)
    };

    // 9. Assemble report
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
        disks: verbose_ctx.disks.clone(),
        advisories,
        alert_active: alert_state.active,
        alert_causes: alert_state.causes,
    };

    // 9. Output
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!(
            "{}",
            format_status_human(
                &report,
                Some(&compact_drives),
                Some(&verbose_ctx.human_details),
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
    let latch = alert::load_alert_latch(paths);
    let smartd_active = alert::smartd_alert_active(paths);

    match latch {
        Some(mut state) if state.active => {
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
        _ if smartd_active => AlertState {
            active: true,
            causes: vec![AlertCause::SmartdAlert],
        },
        _ => AlertState {
            active: false,
            causes: vec![],
        },
    }
}

// ---------------------------------------------------------------------------
// Private helpers — strict (return Result)
// ---------------------------------------------------------------------------

struct DfSummary {
    profile: String,
    allocation: Vec<AllocationEntry>,
}

fn summarize_df<R: CommandRunner>(runner: &R, mount_point: &str) -> Result<DfSummary, StatusError> {
    let raw = runner.run(&CmdRequest::BtrfsFilesystemDfJson {
        mount_point: MountPoint(mount_point.to_owned()),
    })?;
    let df = parse_btrfs_df_json(&raw)?;

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

    Ok(DfSummary {
        profile,
        allocation,
    })
}

fn get_capacity<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
    missing_count: u64,
) -> Result<CapacityReport, StatusError> {
    let raw = runner.run(&CmdRequest::BtrfsFilesystemUsageRaw {
        mount_point: MountPoint(mount_point.to_owned()),
    })?;
    let usage = parse_btrfs_filesystem_usage(&raw)?;

    let total_bytes = if missing_count == 0 {
        let dev_raw = runner.run(&CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: MountPoint(mount_point.to_owned()),
        })?;
        let dev_usage = parse_btrfs_device_usage(&dev_raw)?;
        let sizes: Vec<u64> = dev_usage.devices.iter().map(|d| d.device_size).collect();
        Some(estimate_pool_capacity(&sizes))
    } else {
        None
    };

    Ok(CapacityReport {
        total_bytes,
        used_bytes: usage.used_bytes,
        free_bytes: usage.free_estimated_bytes,
    })
}

fn get_device_stats<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
) -> Result<BtrfsDeviceStatsOutput, StatusError> {
    let raw = runner.run(&CmdRequest::BtrfsDeviceStats {
        mount_point: MountPoint(mount_point.to_owned()),
    })?;
    let stats = parse_btrfs_device_stats(&raw)?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Private helpers — tolerant (never fail)
// ---------------------------------------------------------------------------

fn get_scrub_string<R: CommandRunner>(runner: &R, mount_point: &str) -> String {
    let raw = match runner.run(&CmdRequest::BtrfsScrubStatus {
        mount_point: MountPoint(mount_point.to_owned()),
    }) {
        Ok(r) => r,
        Err(_) => return "unknown".to_owned(),
    };

    match parse_btrfs_scrub_status(&raw) {
        Ok(out) => match out.state {
            ScrubState::Never => "never".to_owned(),
            ScrubState::Running { .. } => "running".to_owned(),
            ScrubState::Completed { started_at, .. } => {
                use time::macros::format_description;
                let fmt = format_description!(
                    "[weekday repr:short] [month repr:short] [day padding:space] [hour]:[minute]:[second] [year]"
                );
                started_at
                    .0
                    .format(&fmt)
                    .unwrap_or_else(|_| "unknown".to_owned())
            }
            ScrubState::Unknown => "unknown".to_owned(),
        },
        Err(_) => "unknown".to_owned(),
    }
}

pub(crate) fn get_balance_report<R: CommandRunner>(runner: &R, mount_point: &str) -> BalanceReport {
    let raw = match runner.run(&CmdRequest::BtrfsBalanceStatus {
        mount_point: MountPoint(mount_point.to_owned()),
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

fn get_lsblk_field<R: CommandRunner>(
    runner: &R,
    device: &str,
    field: LsblkFieldKind,
) -> Option<String> {
    let raw = runner
        .run(&CmdRequest::LsblkField {
            device: device.to_owned(),
            field,
        })
        .ok()?;
    parse_lsblk_field(&raw).ok()?.value
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

        // Error stats
        let dev_path = format!("/dev/mapper/{}", pd.mapper.0);
        let errors = device_stats
            .devices
            .iter()
            .find(|d| d.target.as_path() == Some(dev_path.as_str()))
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
            status: "present".to_owned(),
            errors: errors.clone(),
        });

        human_details.push(HumanDisk {
            name: disk_name,
            by_id: by_id.clone(),
            luks_uuid: pd.luks_uuid.0.clone(),
            devid: Some(pd.devid.to_string()),
            status: "present".to_owned(),
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
            ConfigDiskState::PresentNotLuks => true,
        };

        if !is_unpooled {
            continue;
        }

        let status = "missing".to_owned();
        let mapper = mapper_name(&cd.name).0;

        disk_reports.push(DiskReport {
            name: cd.name.clone(),
            mapper: mapper.clone(),
            by_id: cd.by_id_path.0.clone(),
            luks_uuid: String::new(),
            devid: None,
            underlying: None,
            status: status.clone(),
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
                    out.push_str(&format!("  - btrfs device errors on devid {devid}\n"));
                }
                AlertCause::MissingDevice { devid } => {
                    out.push_str(&format!("  - missing device (devid {devid})\n"));
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

    if let Some(ref alloc) = report.allocation {
        if !alloc.is_empty() {
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
        out.push_str(&format!("\nLast scrub: {scrub}\n"));
    }

    // Verbose: per-disk section
    if let Some(disks) = human_disks {
        out.push_str("\nDisks:\n");
        for d in disks {
            out.push('\n');
            // show disk name
            if d.status == "missing" {
                out.push_str(&format!("  {:<18}MISSING\n", d.name));
            } else if d.status == "new" {
                out.push_str(&format!("  {:<18}NEW\n", d.name));
            } else if d.status == "unknown" {
                out.push_str(&format!("  {:<18}UNKNOWN\n", d.name));
            } else {
                let devid_str = d
                    .devid
                    .as_deref()
                    .map(|id| format!("devid {id}"))
                    .unwrap_or_default();
                out.push_str(&format!("  {:<18}{:<10}{}\n", d.name, devid_str, d.status));
            }

            // Device path
            if d.status == "missing" {
                out.push_str(&format!("    Device:  {}  (not found)\n", d.by_id));
            } else {
                out.push_str(&format!("    Device:  {}\n", d.by_id));
            }

            // Model/Serial (present only)
            if d.status == "present" {
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
                None if d.status == "missing" => {
                    out.push_str("    Errors:  unknown (device absent)\n");
                    false
                }
                None if d.status == "unknown" => {
                    out.push_str("    Errors:  unknown (disk-map unavailable)\n");
                    false
                }
                None => false,
            };

            // Action guidance
            if has_errors {
                out.push_str(&format!(
                    "    Action:  add replacement disk to config, then: braid replace --old {} --new <new-name>\n",
                    d.name
                ));
            } else if d.status == "missing" {
                out.push_str(&format!(
                    "    Action:  add replacement disk to config, then: braid replace --old {} --new <new-name>\n",
                    d.name
                ));
            }
        }
    }

    out
}

pub fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    const TIB: u64 = 1024 * GIB;

    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

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
    }

    impl MockFs {
        fn new(paths: &[&str]) -> Self {
            Self {
                paths: paths.iter().map(|s| s.to_string()).collect(),
                block_devices: vec![],
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

    // --- Mock data builders ---

    fn findmnt_not_mounted() -> RawCommandOutput {
        err_raw("findmnt", 1, "")
    }

    fn findmnt_btrfs() -> RawCommandOutput {
        ok_raw(
            "findmnt",
            r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/disk1","fstype":"btrfs"}]}"#,
        )
    }

    fn findmnt_ext4() -> RawCommandOutput {
        ok_raw(
            "findmnt",
            r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/sda1","fstype":"ext4"}]}"#,
        )
    }

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
            "btrfs scrub status",
            "UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\nScrub started:    no stats available\n",
        )
    }

    fn btrfs_scrub_completed() -> RawCommandOutput {
        ok_raw(
            "btrfs scrub status",
            "UUID:             aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             Scrub started:    Mon Feb 23 10:00:00 2026\n\
             Status:           finished\n\
             Duration:         0:00:01\n\
             Total to scrub:   1.00GiB\n\
             Rate:             1.00GiB/s\n\
             Error summary:    no errors found\n",
        )
    }

    fn btrfs_device_stats_3disk() -> RawCommandOutput {
        ok_raw(
            "btrfs device stats",
            "[/dev/mapper/disk1].write_io_errs    0\n\
             [/dev/mapper/disk1].read_io_errs     0\n\
             [/dev/mapper/disk1].flush_io_errs    0\n\
             [/dev/mapper/disk1].corruption_errs  0\n\
             [/dev/mapper/disk1].generation_errs  0\n\
             [/dev/mapper/disk2].write_io_errs    0\n\
             [/dev/mapper/disk2].read_io_errs     0\n\
             [/dev/mapper/disk2].flush_io_errs    0\n\
             [/dev/mapper/disk2].corruption_errs  0\n\
             [/dev/mapper/disk2].generation_errs  0\n\
             [/dev/mapper/disk3].write_io_errs    0\n\
             [/dev/mapper/disk3].read_io_errs     0\n\
             [/dev/mapper/disk3].flush_io_errs    0\n\
             [/dev/mapper/disk3].corruption_errs  0\n\
             [/dev/mapper/disk3].generation_errs  0\n",
        )
    }

    fn lsblk_field_ok(cmd: &str, value: &str) -> RawCommandOutput {
        ok_raw(cmd, &format!("{value}\n"))
    }

    fn config_3disk() -> Config {
        Config::new(MountPoint("/mnt/storage".to_owned())).unwrap()
    }

    fn membership_3disk() -> PoolMembership {
        let mut disks = BTreeMap::new();
        disks.insert(
            "disk1".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/disk1".to_owned())),
        );
        disks.insert(
            "disk2".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/disk2".to_owned())),
        );
        disks.insert(
            "disk3".to_owned(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/disk3".to_owned())),
        );
        PoolMembership { disks }
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
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                findmnt_btrfs(),
            )
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
                CmdRequest::BtrfsDeviceStats {
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
                CmdRequest::BtrfsDeviceStats {
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
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            findmnt_not_mounted(),
        );
        let fs = MockFs::new(&[]);
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
        let _ = cmd_status(&runner, &fs, &config, false, &StatePaths::production());
    }

    #[test]
    fn status_json_healthy() {
        let runner = runner_healthy_3disk_base();
        let config = config_3disk();

        let df_summary = summarize_df(&runner, "/mnt/storage").unwrap();
        let capacity = get_capacity(&runner, "/mnt/storage", 0).unwrap();
        let last_scrub = get_scrub_string(&runner, "/mnt/storage");

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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
        };

        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let obj = v.as_object().unwrap();

        assert_eq!(obj["status"], "degraded");
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
            status: "present".to_owned(),
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
            status: "missing".to_owned(),
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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![present, missing],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
        };

        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let disks = v["disks"].as_array().unwrap();
        assert_eq!(disks.len(), 2);

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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![DiskReport {
                name: "disk1".to_owned(),
                mapper: "disk1".to_owned(),
                by_id: "/dev/disk/by-id/disk1".to_owned(),
                luks_uuid: "11111111-1111-1111-1111-111111111111".to_owned(),
                devid: Some("1".to_owned()),
                underlying: Some("/dev/vda".to_owned()),
                status: "present".to_owned(),
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
        };
        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(v["disks"].is_array());
        assert!(v["disks"].as_array().unwrap().len() > 0);
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
            last_scrub: Some("never".to_owned()),
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
        };
        let compact = vec![CompactDrive {
            name: "disk1".to_owned(),
            device_short: "vda".to_owned(),
            devid: Some(1),
            status: "present",
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
            last_scrub: Some("never".to_owned()),
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
        };
        let compact = vec![
            CompactDrive {
                name: "disk1".into(),
                device_short: "vda".into(),
                devid: Some(1),
                status: "present",
            },
            CompactDrive {
                name: "disk2".into(),
                device_short: "vdb".into(),
                devid: Some(2),
                status: "present",
            },
            CompactDrive {
                name: "disk3".into(),
                device_short: "vdc".into(),
                devid: Some(3),
                status: "present",
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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
        };
        let compact = vec![
            CompactDrive {
                name: "disk1".into(),
                device_short: "vda".into(),
                devid: Some(1),
                status: "present",
            },
            CompactDrive {
                name: "disk2".into(),
                device_short: "vdb".into(),
                devid: Some(2),
                status: "present",
            },
            CompactDrive {
                name: "disk3".into(),
                device_short: "-".into(),
                devid: None,
                status: "missing",
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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
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
            status: "present".to_owned(),
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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
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
            status: "missing".to_owned(),
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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
        };

        let human = format_status_human(&report, None, Some(&human_disks));
        assert!(human.contains("MISSING"), "got:\n{human}");
        assert!(human.contains("not found"), "got:\n{human}");
        assert!(human.contains("device absent"), "got:\n{human}");
    }

    #[test]
    fn status_verbose_lsblk_failure() {
        let human_disks = vec![HumanDisk {
            name: "disk1".to_owned(),
            by_id: "/dev/disk/by-id/disk1".to_owned(),
            luks_uuid: "11111111-1111-1111-1111-111111111111".to_owned(),
            devid: Some("1".to_owned()),
            status: "present".to_owned(),
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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
        };

        let human = format_status_human(&report, None, Some(&human_disks));
        assert!(human.contains("(unknown)"), "got:\n{human}");
    }

    // =======================================================================
    // Error policy tests
    // =======================================================================

    #[test]
    fn status_scrub_completed() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            btrfs_scrub_completed(),
        );
        let result = get_scrub_string(&runner, "/mnt/storage");
        assert!(result.contains("Mon Feb 23"), "got: {result}");
    }

    #[test]
    fn status_scrub_failure_tolerant() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("btrfs scrub status", 1, "some error"),
        );
        let result = get_scrub_string(&runner, "/mnt/storage");
        assert_eq!(result, "unknown");
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
        assert_eq!(
            get_balance_report(&runner, "/mnt/storage"),
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
            get_balance_report(&runner, "/mnt/storage"),
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
            get_balance_report(&runner, "/mnt/storage"),
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
        assert_eq!(
            get_balance_report(&runner, "/mnt/storage"),
            BalanceReport::Unknown
        );
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
            last_scrub: Some("never".to_owned()),
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
            last_scrub: Some("never".to_owned()),
            balance: Some(BalanceReport::Unknown),
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
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
            last_scrub: Some("never".to_owned()),
            balance: Some(BalanceReport::Idle),
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
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
        let result = summarize_df(&runner, "/mnt/storage");
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
        let result = get_capacity(&runner, "/mnt/storage", 0);
        assert!(result.is_err());
    }

    #[test]
    fn status_device_stats_failure_fatal() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsDeviceStats {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            err_raw("btrfs device stats", 1, "error"),
        );
        let result = get_device_stats(&runner, "/mnt/storage");
        assert!(result.is_err());
    }

    #[test]
    fn status_not_btrfs_maps_to_not_mounted() {
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            findmnt_ext4(),
        );
        let fs = MockFs::new(&[]);
        let config = config_3disk();

        // cmd_status should succeed (not error), treating it as not-mounted
        let result = cmd_status(&runner, &fs, &config, false, &StatePaths::production());
        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    // =======================================================================
    // format_bytes tests
    // =======================================================================

    #[test]
    fn format_bytes_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1), "1 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1048576), "1.00 MiB");
        assert_eq!(format_bytes(1073741824), "1.00 GiB");
        assert_eq!(format_bytes(1099511627776), "1.00 TiB");
    }

    // =======================================================================
    // Integration-style tests (cmd_status end-to-end with mocks)
    // =======================================================================

    #[test]
    fn cmd_status_not_mounted_ok() {
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson {
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            findmnt_not_mounted(),
        );
        let fs = MockFs::new(&[]);
        let config = config_3disk();

        let result = cmd_status(&runner, &fs, &config, false, &StatePaths::production());
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_healthy_ok() {
        let runner = runner_healthy_3disk_verbose(runner_healthy_3disk_base());
        let fs = fs_3disk();
        let config = config_3disk();

        let result = cmd_status(&runner, &fs, &config, false, &StatePaths::production());
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_healthy_json_ok() {
        let runner = runner_healthy_3disk_verbose(runner_healthy_3disk_base());
        let fs = fs_3disk();
        let config = config_3disk();

        let result = cmd_status(&runner, &fs, &config, true, &StatePaths::production());
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_degraded_ok() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                findmnt_btrfs(),
            )
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
                CmdRequest::BtrfsDeviceStats {
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

        let result = cmd_status(&runner, &fs, &config, false, &StatePaths::production());
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_single_disk_ok() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                findmnt_btrfs(),
            )
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
                CmdRequest::BtrfsDeviceStats {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs device stats",
                    "[/dev/mapper/disk1].write_io_errs    0\n\
                     [/dev/mapper/disk1].read_io_errs     0\n\
                     [/dev/mapper/disk1].flush_io_errs    0\n\
                     [/dev/mapper/disk1].corruption_errs  0\n\
                     [/dev/mapper/disk1].generation_errs  0\n",
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

        let result = cmd_status(&runner, &fs, &config, false, &StatePaths::production());
        assert!(result.is_ok());
    }

    // =======================================================================
    // build_disk_reports: PresentNotLuks classification
    // =======================================================================

    #[test]
    fn build_disk_reports_present_not_luks_missing() {
        let config_disks = vec![ConfigDisk {
            name: "disk1".to_owned(),
            by_id_path: ByIdPath("/dev/disk/by-id/disk1".to_owned()),
            state: ConfigDiskState::PresentNotLuks,
        }];
        let pool = PoolState {
            mounted: true,
            devices: vec![],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 0,
            fsid: None,
        };
        let runner = MockRunner::default();
        let stats = BtrfsDeviceStatsOutput { devices: vec![] };

        let ctx = build_disk_reports(&runner, &config_disks, &pool, &stats);
        assert_eq!(ctx.disks.len(), 1);
        assert_eq!(ctx.disks[0].status, "missing");
    }

    // =======================================================================
    // Compact drive tests
    // =======================================================================

    #[test]
    fn status_compact_new_disk() {
        let compact = vec![CompactDrive {
            name: "disk2".to_owned(),
            device_short: "-".to_owned(),
            devid: None,
            status: "new",
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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
        };
        let human = format_status_human(&report, Some(&compact), None);
        assert!(human.contains("new"), "got:\n{human}");
        assert!(!human.contains("missing"), "got:\n{human}");
    }

    #[test]
    fn status_compact_missing_disk() {
        let pool = PoolState {
            mounted: true,
            devices: vec![],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 0,
            fsid: None,
        };
        let membership = membership_1disk();
        let drives = build_compact_drives(&pool, &membership);
        assert_eq!(drives.len(), 1);
        assert_eq!(drives[0].status, "missing");
    }

    // =======================================================================
    // Verbose new/unknown tests
    // =======================================================================

    #[test]
    fn status_verbose_new_disk() {
        let human_disks = vec![HumanDisk {
            name: "disk2".to_owned(),
            by_id: "/dev/disk/by-id/disk2".to_owned(),
            luks_uuid: String::new(),
            devid: None,
            status: "new".to_owned(),
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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
        };

        let human = format_status_human(&report, None, Some(&human_disks));
        assert!(human.contains("NEW"), "got:\n{human}");
        assert!(!human.contains("(not found)"), "got:\n{human}");
        assert!(!human.contains("Errors:"), "got:\n{human}");
    }

    #[test]
    fn status_verbose_unknown_disk() {
        let human_disks = vec![HumanDisk {
            name: "disk2".to_owned(),
            by_id: "/dev/disk/by-id/disk2".to_owned(),
            luks_uuid: String::new(),
            devid: None,
            status: "unknown".to_owned(),
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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
        };

        let human = format_status_human(&report, None, Some(&human_disks));
        assert!(human.contains("UNKNOWN"), "got:\n{human}");
        assert!(human.contains("disk-map unavailable"), "got:\n{human}");
    }

    // =======================================================================
    // Healthy tests assert no "new"
    // =======================================================================

    #[test]
    fn status_human_healthy_no_new() {
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
            last_scrub: Some("never".to_owned()),
            balance: None,
            allocation: None,
            disks: vec![],
            advisories: vec![],
            alert_active: false,
            alert_causes: vec![],
        };
        let compact = vec![
            CompactDrive {
                name: "disk1".into(),
                device_short: "vda".into(),
                devid: Some(1),
                status: "present",
            },
            CompactDrive {
                name: "disk2".into(),
                device_short: "vdb".into(),
                devid: Some(2),
                status: "present",
            },
            CompactDrive {
                name: "disk3".into(),
                device_short: "vdc".into(),
                devid: Some(3),
                status: "present",
            },
        ];
        let human = format_status_human(&report, Some(&compact), None);
        assert!(!human.contains("new"), "got:\n{human}");
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
}
