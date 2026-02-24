use std::collections::HashSet;

use serde::{Deserialize, Serialize};

use crate::cmd::{CmdError, CmdRequest, CommandRunner, LsblkFieldKind};
use crate::config::Config;
use crate::parse::{
    parse_btrfs_device_stats, parse_btrfs_df_json, parse_btrfs_filesystem_usage,
    parse_btrfs_scrub_status, parse_lsblk_field, BtrfsDeviceStatsOutput, ParseError, ScrubState,
};
use crate::plan::mapper_name_for_by_id;
use crate::probe::{probe_config_disk, probe_pool, Filesystem, ProbeError};
use crate::types::*;

// ---------------------------------------------------------------------------
// Public types (JSON schema)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StatusCode {
    Healthy,
    Degraded,
    NotMounted,
}

impl StatusCode {
    pub fn display_status(self, missing_count: u64) -> String {
        match self {
            StatusCode::Healthy => "healthy".to_owned(),
            StatusCode::Degraded if missing_count == 1 => {
                "DEGRADED (1 missing device)".to_owned()
            }
            StatusCode::Degraded => format!("DEGRADED ({missing_count} missing devices)"),
            StatusCode::NotMounted => "not mounted".to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusReport {
    pub schema_version: u32,
    pub mount_point: String,
    pub status_code: StatusCode,
    pub status: String,
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
    pub disks: Vec<DiskReport>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapacityReport {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiskReport {
    pub mapper: String,
    pub by_id: String,
    pub luks_uuid: String,
    pub devid: Option<String>,
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
}

// ---------------------------------------------------------------------------
// Internal types (verbose context)
// ---------------------------------------------------------------------------

struct VerboseContext {
    disks: Vec<DiskReport>,
    human_details: Vec<HumanDisk>,
}

struct HumanDisk {
    mapper: String,
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
) -> Result<StatusReport, StatusError> {
    let pool = match probe_pool(runner, &config.mount_point) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            let code = StatusCode::NotMounted;
            return Ok(StatusReport {
                schema_version: 1,
                mount_point: config.mount_point.clone(),
                status_code: code,
                status: code.display_status(0),
                total_devices: None,
                present_count: None,
                missing_count: None,
                profile: None,
                capacity: None,
                last_scrub: None,
                disks: vec![],
            });
        }
        Err(e) => return Err(e.into()),
    };

    if !pool.mounted {
        let code = StatusCode::NotMounted;
        return Ok(StatusReport {
            schema_version: 1,
            mount_point: config.mount_point.clone(),
            status_code: code,
            status: code.display_status(0),
            total_devices: None,
            present_count: None,
            missing_count: None,
            profile: None,
            capacity: None,
            last_scrub: None,
            disks: vec![],
        });
    }

    let profile = get_profile(runner, &config.mount_point)?;
    let capacity = get_capacity(runner, &config.mount_point)?;
    let last_scrub = get_scrub_string(runner, &config.mount_point);

    let code = if pool.missing_count == 0 {
        StatusCode::Healthy
    } else {
        StatusCode::Degraded
    };

    let config_disks: Vec<ConfigDisk> = config
        .disks
        .iter()
        .map(|d| probe_config_disk(runner, fs, d))
        .collect::<Result<Vec<_>, _>>()?;
    let device_stats = get_device_stats(runner, &config.mount_point)?;
    let verbose_ctx = build_disk_reports(runner, &config_disks, &pool, &device_stats);

    let present_count = pool.total_devices.saturating_sub(pool.missing_count);
    Ok(StatusReport {
        schema_version: 1,
        mount_point: config.mount_point.clone(),
        status_code: code,
        status: code.display_status(pool.missing_count),
        total_devices: Some(pool.total_devices),
        present_count: Some(present_count),
        missing_count: Some(pool.missing_count),
        profile: Some(profile),
        capacity: Some(capacity),
        last_scrub: Some(last_scrub),
        disks: verbose_ctx.disks,
    })
}

pub fn cmd_status<R: CommandRunner, F: Filesystem>(
    runner: &R,
    fs: &F,
    config: &Config,
    verbose: bool,
    json: bool,
) -> Result<(), StatusError> {
    // 1. Probe pool, mapping NotBtrfs to not-mounted
    let pool = match probe_pool(runner, &config.mount_point) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => PoolState {
            mounted: false,
            devices: vec![],
            missing_count: 0,
            total_devices: 0,
        },
        Err(e) => return Err(e.into()),
    };

    // 2. Not mounted → minimal report
    if !pool.mounted {
        let code = StatusCode::NotMounted;
        let report = StatusReport {
            schema_version: 1,
            mount_point: config.mount_point.clone(),
            status_code: code,
            status: code.display_status(0),
            total_devices: None,
            present_count: None,
            missing_count: None,
            profile: None,
            capacity: None,
            last_scrub: None,
            disks: vec![],
        };
        if json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", format_status_human(&report, None));
        }
        return Ok(());
    }

    // 3. Strict data gathering
    let profile = get_profile(runner, &config.mount_point)?;
    let capacity = get_capacity(runner, &config.mount_point)?;
    let last_scrub = get_scrub_string(runner, &config.mount_point);

    // 4. Compute status code
    let code = if pool.missing_count == 0 {
        StatusCode::Healthy
    } else {
        StatusCode::Degraded
    };
    let status = code.display_status(pool.missing_count);

    // 5. Verbose context
    let verbose_ctx = if verbose {
        let config_disks: Vec<ConfigDisk> = config
            .disks
            .iter()
            .map(|d| probe_config_disk(runner, fs, d))
            .collect::<Result<Vec<_>, _>>()?;
        let device_stats = get_device_stats(runner, &config.mount_point)?;
        Some(build_disk_reports(
            runner,
            &config_disks,
            &pool,
            &device_stats,
        ))
    } else {
        None
    };

    // 6. Assemble report
    let present_count = pool.total_devices.saturating_sub(pool.missing_count);
    let report = StatusReport {
        schema_version: 1,
        mount_point: config.mount_point.clone(),
        status_code: code,
        status,
        total_devices: Some(pool.total_devices),
        present_count: Some(present_count),
        missing_count: Some(pool.missing_count),
        profile: Some(profile),
        capacity: Some(capacity),
        last_scrub: Some(last_scrub),
        disks: verbose_ctx
            .as_ref()
            .map(|v| v.disks.clone())
            .unwrap_or_default(),
    };

    // 7. Output
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print!(
            "{}",
            format_status_human(
                &report,
                verbose_ctx
                    .as_ref()
                    .map(|v| v.human_details.as_slice()),
            )
        );
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Private helpers — strict (return Result)
// ---------------------------------------------------------------------------

fn get_profile<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
) -> Result<String, StatusError> {
    let raw = runner.run(&CmdRequest::BtrfsFilesystemDfJson {
        mount_point: mount_point.to_owned(),
    })?;
    let df = parse_btrfs_df_json(&raw)?;

    // Use the Data profile; fall back to first entry's profile
    let profile = df
        .entries
        .iter()
        .find(|e| e.bg_type == "Data")
        .or_else(|| df.entries.first())
        .map(|e| e.bg_profile.clone())
        .unwrap_or_else(|| "unknown".to_owned());

    Ok(profile)
}

fn get_capacity<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
) -> Result<CapacityReport, StatusError> {
    let raw = runner.run(&CmdRequest::BtrfsFilesystemUsageRaw {
        mount_point: mount_point.to_owned(),
    })?;
    let usage = parse_btrfs_filesystem_usage(&raw)?;

    Ok(CapacityReport {
        total_bytes: usage.device_size_bytes,
        used_bytes: usage.used_bytes,
        free_bytes: usage.free_estimated_bytes,
    })
}

fn get_device_stats<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
) -> Result<BtrfsDeviceStatsOutput, StatusError> {
    let raw = runner.run(&CmdRequest::BtrfsDeviceStats {
        mount_point: mount_point.to_owned(),
    })?;
    let stats = parse_btrfs_device_stats(&raw)?;
    Ok(stats)
}

// ---------------------------------------------------------------------------
// Private helpers — tolerant (never fail)
// ---------------------------------------------------------------------------

fn get_scrub_string<R: CommandRunner>(runner: &R, mount_point: &str) -> String {
    let raw = match runner.run(&CmdRequest::BtrfsScrubStatus {
        mount_point: mount_point.to_owned(),
    }) {
        Ok(r) => r,
        Err(_) => return "unknown".to_owned(),
    };

    match parse_btrfs_scrub_status(&raw) {
        Ok(out) => match out.state {
            ScrubState::Never => "never".to_owned(),
            ScrubState::Completed { started_at } => started_at.0,
            ScrubState::Unknown => "unknown".to_owned(),
        },
        Err(_) => "unknown".to_owned(),
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
    let pool_uuid_set: HashSet<&LuksUuid> =
        pool.devices.iter().map(|d| &d.luks_uuid).collect();

    let mut disk_reports = Vec::new();
    let mut human_details = Vec::new();

    // Present pool devices
    for pd in &pool.devices {
        // Find matching config disk
        let matched_config = config_disks.iter().find(|cd| {
            matches!(&cd.state, ConfigDiskState::PresentLuks { uuid, .. } if uuid == &pd.luks_uuid)
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
            .find(|d| d.device_path == dev_path)
            .map(|d| DiskErrors {
                read: d.read_io_errs,
                write: d.write_io_errs,
                flush: d.flush_io_errs,
                corruption: d.corruption_errs,
                generation: d.generation_errs,
            });

        disk_reports.push(DiskReport {
            mapper: mapper.clone(),
            by_id: by_id.clone(),
            luks_uuid: pd.luks_uuid.0.clone(),
            devid: Some(pd.devid.to_string()),
            status: "present".to_owned(),
            errors: errors.clone(),
        });

        human_details.push(HumanDisk {
            mapper: mapper.clone(),
            by_id: by_id.clone(),
            luks_uuid: pd.luks_uuid.0.clone(),
            devid: Some(pd.devid.to_string()),
            status: "present".to_owned(),
            model,
            serial,
            errors,
        });
    }

    // Missing config disks (not matched to pool)
    for cd in config_disks {
        let is_missing = match &cd.state {
            ConfigDiskState::Absent => true,
            ConfigDiskState::PresentLuks { uuid, .. } => !pool_uuid_set.contains(uuid),
            ConfigDiskState::PresentNotLuks => false,
        };

        if !is_missing {
            continue;
        }

        let mapper = mapper_name_for_by_id(&cd.by_id_path)
            .map(|m| m.0)
            .unwrap_or_else(|| cd.by_id_path.0.clone());

        disk_reports.push(DiskReport {
            mapper: mapper.clone(),
            by_id: cd.by_id_path.0.clone(),
            luks_uuid: String::new(),
            devid: None,
            status: "missing".to_owned(),
            errors: None,
        });

        human_details.push(HumanDisk {
            mapper: mapper.clone(),
            by_id: cd.by_id_path.0.clone(),
            luks_uuid: String::new(),
            devid: None,
            status: "missing".to_owned(),
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

fn format_status_human(report: &StatusReport, human_disks: Option<&[HumanDisk]>) -> String {
    let mut out = String::new();

    out.push_str(&format!("Pool:     {}\n", report.mount_point));
    out.push_str(&format!("Status:   {}\n", report.status));

    if report.status_code == StatusCode::NotMounted {
        return out;
    }

    // Drive count line
    if let (Some(total), Some(missing)) = (report.total_devices, report.missing_count) {
        if missing > 0 {
            let present = total.saturating_sub(missing);
            out.push_str(&format!(
                "Drives:   {present} present, {missing} missing\n"
            ));
        } else {
            out.push_str(&format!("Drives:   {total}\n"));
        }
    }

    if let Some(ref profile) = report.profile {
        out.push_str(&format!("Profile:  {profile}\n"));
    }

    if let Some(ref cap) = report.capacity {
        out.push('\n');
        out.push_str("Capacity:\n");
        out.push_str(&format!("  Total:  {}\n", format_bytes(cap.total_bytes)));
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
            // Header line
            if d.status == "missing" {
                out.push_str(&format!("  {:<18}MISSING\n", d.mapper));
            } else {
                let devid_str = d
                    .devid
                    .as_deref()
                    .map(|id| format!("devid {id}"))
                    .unwrap_or_default();
                out.push_str(&format!(
                    "  {:<18}{:<10}{}\n",
                    d.mapper, devid_str, d.status
                ));
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
            match &d.errors {
                Some(e) => {
                    out.push_str(&format!(
                        "    Errors:  read {} / write {} / flush {} / corruption {} / generation {}\n",
                        e.read, e.write, e.flush, e.corruption, e.generation
                    ));
                }
                None if d.status == "missing" => {
                    out.push_str("    Errors:  unknown (device absent)\n");
                }
                None => {}
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
             \tFree (estimated):\t\t442957824\t(min: 442957824)\n",
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
        Config {
            disks: vec![
                ByIdPath("/dev/disk/by-id/disk1".to_owned()),
                ByIdPath("/dev/disk/by-id/disk2".to_owned()),
                ByIdPath("/dev/disk/by-id/disk3".to_owned()),
            ],
            mount_point: "/mnt/storage".to_owned(),
        }
    }

    fn config_1disk() -> Config {
        Config {
            disks: vec![ByIdPath("/dev/disk/by-id/disk1".to_owned())],
            mount_point: "/mnt/storage".to_owned(),
        }
    }

    /// Build a MockRunner for a 3-disk mounted healthy pool (no verbose).
    fn runner_healthy_3disk_base() -> MockRunner {
        MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson { mount_point: "/mnt/storage".into() },
                findmnt_btrfs(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow { mount_point: "/mnt/storage".into() },
                btrfs_show_3disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus { mapper: "disk1".into() },
                cryptsetup_status_active("disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/vda".into() },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus { mapper: "disk2".into() },
                cryptsetup_status_active("disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/vdb".into() },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus { mapper: "disk3".into() },
                cryptsetup_status_active("disk3", "/dev/vdc"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/vdc".into() },
                cryptsetup_uuid_ok("/dev/vdc", "33333333-3333-3333-3333-333333333333"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson { mount_point: "/mnt/storage".into() },
                btrfs_df_raid1(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw { mount_point: "/mnt/storage".into() },
                btrfs_usage_raw(),
            )
            .with_output(
                CmdRequest::BtrfsScrubStatus { mount_point: "/mnt/storage".into() },
                btrfs_scrub_never(),
            )
    }

    /// Extend a base runner with verbose probe outputs for 3-disk config.
    fn runner_healthy_3disk_verbose(runner: MockRunner) -> MockRunner {
        runner
            // probe_config_disk for each disk
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/disk/by-id/disk1".into() },
                cryptsetup_uuid_ok("/dev/disk/by-id/disk1", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/disk/by-id/disk2".into() },
                cryptsetup_uuid_ok("/dev/disk/by-id/disk2", "22222222-2222-2222-2222-222222222222"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/disk/by-id/disk3".into() },
                cryptsetup_uuid_ok("/dev/disk/by-id/disk3", "33333333-3333-3333-3333-333333333333"),
            )
            // device stats
            .with_output(
                CmdRequest::BtrfsDeviceStats { mount_point: "/mnt/storage".into() },
                btrfs_device_stats_3disk(),
            )
            // lsblk model/serial for each disk
            .with_output(
                CmdRequest::LsblkField { device: "/dev/disk/by-id/disk1".into(), field: LsblkFieldKind::Model },
                lsblk_field_ok("lsblk", "VBOX HARDDISK"),
            )
            .with_output(
                CmdRequest::LsblkField { device: "/dev/disk/by-id/disk1".into(), field: LsblkFieldKind::Serial },
                lsblk_field_ok("lsblk", "disk1"),
            )
            .with_output(
                CmdRequest::LsblkField { device: "/dev/disk/by-id/disk2".into(), field: LsblkFieldKind::Model },
                lsblk_field_ok("lsblk", "VBOX HARDDISK"),
            )
            .with_output(
                CmdRequest::LsblkField { device: "/dev/disk/by-id/disk2".into(), field: LsblkFieldKind::Serial },
                lsblk_field_ok("lsblk", "disk2"),
            )
            .with_output(
                CmdRequest::LsblkField { device: "/dev/disk/by-id/disk3".into(), field: LsblkFieldKind::Model },
                lsblk_field_ok("lsblk", "VBOX HARDDISK"),
            )
            .with_output(
                CmdRequest::LsblkField { device: "/dev/disk/by-id/disk3".into(), field: LsblkFieldKind::Serial },
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
        MockFs::new(&[
            "/dev/disk/by-id/disk1",
            "/dev/mapper/disk1",
        ])
    }

    // =======================================================================
    // Schema envelope tests
    // =======================================================================

    #[test]
    fn status_json_not_mounted() {
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson { mount_point: "/mnt/storage".into() },
            findmnt_not_mounted(),
        );
        let fs = MockFs::new(&[]);
        let config = config_3disk();

        let code = StatusCode::NotMounted;
        let report = StatusReport {
            schema_version: 1,
            mount_point: config.mount_point.clone(),
            status_code: code,
            status: code.display_status(0),
            total_devices: None,
            present_count: None,
            missing_count: None,
            profile: None,
            capacity: None,
            last_scrub: None,
            disks: vec![],
        };

        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let obj = v.as_object().unwrap();

        // Must exist
        assert_eq!(obj["schema_version"], 1);
        assert_eq!(obj["mount_point"], "/mnt/storage");
        assert_eq!(obj["status_code"], "not_mounted");
        assert_eq!(obj["status"], "not mounted");
        assert_eq!(obj["disks"], serde_json::json!([]));

        // Must NOT exist
        assert!(!obj.contains_key("total_devices"));
        assert!(!obj.contains_key("present_count"));
        assert!(!obj.contains_key("missing_count"));
        assert!(!obj.contains_key("profile"));
        assert!(!obj.contains_key("capacity"));
        assert!(!obj.contains_key("last_scrub"));

        // Lock envelope: exactly 5 keys
        assert_eq!(obj.len(), 5, "envelope should have exactly 5 keys, got: {obj:?}");

        // Also verify cmd_status doesn't error
        let _ = cmd_status(&runner, &fs, &config, false, false);
    }

    #[test]
    fn status_json_healthy() {
        let runner = runner_healthy_3disk_base();
        let config = config_3disk();

        let profile = get_profile(&runner, "/mnt/storage").unwrap();
        let capacity = get_capacity(&runner, "/mnt/storage").unwrap();
        let last_scrub = get_scrub_string(&runner, "/mnt/storage");

        let code = StatusCode::Healthy;
        let report = StatusReport {
            schema_version: 1,
            mount_point: config.mount_point.clone(),
            status_code: code,
            status: code.display_status(0),
            total_devices: Some(3),
            present_count: Some(3),
            missing_count: Some(0),
            profile: Some(profile),
            capacity: Some(capacity),
            last_scrub: Some(last_scrub),
            disks: vec![],
        };

        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let obj = v.as_object().unwrap();

        assert_eq!(obj["status_code"], "healthy");
        assert_eq!(obj["status"], "healthy");
        assert_eq!(obj["total_devices"], 3);
        assert_eq!(obj["present_count"], 3);
        assert_eq!(obj["missing_count"], 0);
        assert_eq!(obj["profile"], "RAID1");
        assert!(obj.contains_key("capacity"));
        assert!(obj.contains_key("last_scrub"));
        assert_eq!(obj["disks"], serde_json::json!([]));
    }

    #[test]
    fn status_json_degraded() {
        let code = StatusCode::Degraded;
        let report = StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: code,
            status: code.display_status(1),
            total_devices: Some(3),
            present_count: Some(2),
            missing_count: Some(1),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: 1040187392,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some("never".to_owned()),
            disks: vec![],
        };

        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        let obj = v.as_object().unwrap();

        assert_eq!(obj["status_code"], "degraded");
        assert!(obj["status"].as_str().unwrap().contains("DEGRADED"));
    }

    #[test]
    fn status_json_verbose_disks() {
        let present = DiskReport {
            mapper: "disk1".to_owned(),
            by_id: "/dev/disk/by-id/disk1".to_owned(),
            luks_uuid: "11111111-1111-1111-1111-111111111111".to_owned(),
            devid: Some("1".to_owned()),
            status: "present".to_owned(),
            errors: Some(DiskErrors { read: 0, write: 0, flush: 0, corruption: 0, generation: 0 }),
        };
        let missing = DiskReport {
            mapper: "disk3".to_owned(),
            by_id: "/dev/disk/by-id/disk3".to_owned(),
            luks_uuid: String::new(),
            devid: None,
            status: "missing".to_owned(),
            errors: None,
        };

        let report = StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: StatusCode::Degraded,
            status: "DEGRADED (1 missing device)".to_owned(),
            total_devices: Some(2),
            present_count: Some(1),
            missing_count: Some(1),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: 1040187392,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some("never".to_owned()),
            disks: vec![present, missing],
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
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: code,
            status: code.display_status(0),
            total_devices: None,
            present_count: None,
            missing_count: None,
            profile: None,
            capacity: None,
            last_scrub: None,
            disks: vec![],
        };
        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(v["disks"].is_array());
        assert_eq!(v["disks"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn status_json_disks_always_array_non_verbose() {
        let code = StatusCode::Healthy;
        let report = StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: code,
            status: code.display_status(0),
            total_devices: Some(3),
            present_count: Some(3),
            missing_count: Some(0),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: 1040187392,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some("never".to_owned()),
            disks: vec![],
        };
        let json_str = serde_json::to_string_pretty(&report).unwrap();
        let v: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert!(v["disks"].is_array());
        assert_eq!(v["disks"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn status_json_disks_always_array_verbose() {
        let report = StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: StatusCode::Healthy,
            status: "healthy".to_owned(),
            total_devices: Some(1),
            present_count: Some(1),
            missing_count: Some(0),
            profile: Some("single".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: 1073741824,
                used_bytes: 536870912,
                free_bytes: 536870912,
            }),
            last_scrub: Some("never".to_owned()),
            disks: vec![DiskReport {
                mapper: "disk1".to_owned(),
                by_id: "/dev/disk/by-id/disk1".to_owned(),
                luks_uuid: "11111111-1111-1111-1111-111111111111".to_owned(),
                devid: Some("1".to_owned()),
                status: "present".to_owned(),
                errors: Some(DiskErrors { read: 0, write: 0, flush: 0, corruption: 0, generation: 0 }),
            }],
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
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: code,
            status: code.display_status(0),
            total_devices: None,
            present_count: None,
            missing_count: None,
            profile: None,
            capacity: None,
            last_scrub: None,
            disks: vec![],
        };
        let human = format_status_human(&report, None);
        assert!(human.contains("not mounted"), "got:\n{human}");
        assert!(!human.contains("Capacity"), "got:\n{human}");
        assert!(!human.contains("Profile"), "got:\n{human}");
    }

    #[test]
    fn status_human_healthy_single() {
        let code = StatusCode::Healthy;
        let report = StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: code,
            status: code.display_status(0),
            total_devices: Some(1),
            present_count: Some(1),
            missing_count: Some(0),
            profile: Some("single".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: 1073741824,
                used_bytes: 536870912,
                free_bytes: 536870912,
            }),
            last_scrub: Some("never".to_owned()),
            disks: vec![],
        };
        let human = format_status_human(&report, None);
        assert!(human.contains("healthy"), "got:\n{human}");
        assert!(human.contains("Drives:   1"), "got:\n{human}");
        assert!(human.contains("single"), "got:\n{human}");
        assert!(human.contains("Total:"), "got:\n{human}");
        assert!(human.contains("Used:"), "got:\n{human}");
        assert!(human.contains("Free:"), "got:\n{human}");
        assert!(!human.contains("RAID1"), "got:\n{human}");
        assert!(!human.contains("missing"), "got:\n{human}");
    }

    #[test]
    fn status_human_healthy_raid1() {
        let code = StatusCode::Healthy;
        let report = StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: code,
            status: code.display_status(0),
            total_devices: Some(3),
            present_count: Some(3),
            missing_count: Some(0),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: 1040187392,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some("never".to_owned()),
            disks: vec![],
        };
        let human = format_status_human(&report, None);
        assert!(human.contains("healthy"), "got:\n{human}");
        assert!(human.contains("Drives:   3"), "got:\n{human}");
        assert!(human.contains("RAID1"), "got:\n{human}");
        assert!(human.contains("Total:"), "got:\n{human}");
        assert!(human.contains("scrub"), "got:\n{human}");
        assert!(!human.contains("missing"), "got:\n{human}");
    }

    #[test]
    fn status_human_degraded() {
        let code = StatusCode::Degraded;
        let report = StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: code,
            status: code.display_status(1),
            total_devices: Some(3),
            present_count: Some(2),
            missing_count: Some(1),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: 1040187392,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some("never".to_owned()),
            disks: vec![],
        };
        let human = format_status_human(&report, None);
        assert!(human.contains("DEGRADED (1 missing device)"), "got:\n{human}");
        assert!(human.contains("2 present, 1 missing"), "got:\n{human}");
    }

    #[test]
    fn status_human_degraded_plural() {
        let code = StatusCode::Degraded;
        let report = StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: code,
            status: code.display_status(2),
            total_devices: Some(4),
            present_count: Some(2),
            missing_count: Some(2),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: 1040187392,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some("never".to_owned()),
            disks: vec![],
        };
        let human = format_status_human(&report, None);
        assert!(human.contains("DEGRADED (2 missing devices)"), "got:\n{human}");
    }

    // =======================================================================
    // Verbose human tests
    // =======================================================================

    #[test]
    fn status_verbose_present_disks() {
        let human_disks = vec![
            HumanDisk {
                mapper: "disk1".to_owned(),
                by_id: "/dev/disk/by-id/disk1".to_owned(),
                luks_uuid: "11111111-1111-1111-1111-111111111111".to_owned(),
                devid: Some("1".to_owned()),
                status: "present".to_owned(),
                model: Some("VBOX HARDDISK".to_owned()),
                serial: Some("disk1".to_owned()),
                errors: Some(DiskErrors { read: 0, write: 0, flush: 0, corruption: 0, generation: 0 }),
            },
        ];

        let code = StatusCode::Healthy;
        let report = StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: code,
            status: code.display_status(0),
            total_devices: Some(1),
            present_count: Some(1),
            missing_count: Some(0),
            profile: Some("single".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: 1073741824,
                used_bytes: 536870912,
                free_bytes: 536870912,
            }),
            last_scrub: Some("never".to_owned()),
            disks: vec![],
        };

        let human = format_status_human(&report, Some(&human_disks));
        assert!(human.contains("present"), "got:\n{human}");
        assert!(human.contains("devid 1"), "got:\n{human}");
        assert!(human.contains("LUKS:"), "got:\n{human}");
        assert!(human.contains("Errors:"), "got:\n{human}");
        assert!(human.contains("Model:"), "got:\n{human}");
        assert!(human.contains("Serial:"), "got:\n{human}");
    }

    #[test]
    fn status_verbose_missing_disk() {
        let human_disks = vec![
            HumanDisk {
                mapper: "disk3".to_owned(),
                by_id: "/dev/disk/by-id/disk3".to_owned(),
                luks_uuid: String::new(),
                devid: None,
                status: "missing".to_owned(),
                model: None,
                serial: None,
                errors: None,
            },
        ];

        let code = StatusCode::Degraded;
        let report = StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: code,
            status: code.display_status(1),
            total_devices: Some(2),
            present_count: Some(1),
            missing_count: Some(1),
            profile: Some("RAID1".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: 1040187392,
                used_bytes: 33914880,
                free_bytes: 442957824,
            }),
            last_scrub: Some("never".to_owned()),
            disks: vec![],
        };

        let human = format_status_human(&report, Some(&human_disks));
        assert!(human.contains("MISSING"), "got:\n{human}");
        assert!(human.contains("not found"), "got:\n{human}");
        assert!(human.contains("device absent"), "got:\n{human}");
    }

    #[test]
    fn status_verbose_lsblk_failure() {
        let human_disks = vec![
            HumanDisk {
                mapper: "disk1".to_owned(),
                by_id: "/dev/disk/by-id/disk1".to_owned(),
                luks_uuid: "11111111-1111-1111-1111-111111111111".to_owned(),
                devid: Some("1".to_owned()),
                status: "present".to_owned(),
                model: None,
                serial: None,
                errors: Some(DiskErrors { read: 0, write: 0, flush: 0, corruption: 0, generation: 0 }),
            },
        ];

        let code = StatusCode::Healthy;
        let report = StatusReport {
            schema_version: 1,
            mount_point: "/mnt/storage".to_owned(),
            status_code: code,
            status: code.display_status(0),
            total_devices: Some(1),
            present_count: Some(1),
            missing_count: Some(0),
            profile: Some("single".to_owned()),
            capacity: Some(CapacityReport {
                total_bytes: 1073741824,
                used_bytes: 536870912,
                free_bytes: 536870912,
            }),
            last_scrub: Some("never".to_owned()),
            disks: vec![],
        };

        let human = format_status_human(&report, Some(&human_disks));
        assert!(human.contains("(unknown)"), "got:\n{human}");
    }

    // =======================================================================
    // Error policy tests
    // =======================================================================

    #[test]
    fn status_scrub_completed() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus { mount_point: "/mnt/storage".into() },
            btrfs_scrub_completed(),
        );
        let result = get_scrub_string(&runner, "/mnt/storage");
        assert!(result.contains("Mon Feb 23"), "got: {result}");
    }

    #[test]
    fn status_scrub_failure_tolerant() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsScrubStatus { mount_point: "/mnt/storage".into() },
            err_raw("btrfs scrub status", 1, "some error"),
        );
        let result = get_scrub_string(&runner, "/mnt/storage");
        assert_eq!(result, "unknown");
    }

    #[test]
    fn status_df_failure_fatal() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemDfJson { mount_point: "/mnt/storage".into() },
            err_raw("btrfs filesystem df", 1, "not a btrfs filesystem"),
        );
        let result = get_profile(&runner, "/mnt/storage");
        assert!(result.is_err());
    }

    #[test]
    fn status_usage_failure_fatal() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsFilesystemUsageRaw { mount_point: "/mnt/storage".into() },
            err_raw("btrfs filesystem usage", 1, "error"),
        );
        let result = get_capacity(&runner, "/mnt/storage");
        assert!(result.is_err());
    }

    #[test]
    fn status_device_stats_failure_fatal() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsDeviceStats { mount_point: "/mnt/storage".into() },
            err_raw("btrfs device stats", 1, "error"),
        );
        let result = get_device_stats(&runner, "/mnt/storage");
        assert!(result.is_err());
    }

    #[test]
    fn status_not_btrfs_maps_to_not_mounted() {
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson { mount_point: "/mnt/storage".into() },
            findmnt_ext4(),
        );
        let fs = MockFs::new(&[]);
        let config = config_3disk();

        // cmd_status should succeed (not error), treating it as not-mounted
        let result = cmd_status(&runner, &fs, &config, false, false);
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
            CmdRequest::FindmntJson { mount_point: "/mnt/storage".into() },
            findmnt_not_mounted(),
        );
        let fs = MockFs::new(&[]);
        let config = config_3disk();

        let result = cmd_status(&runner, &fs, &config, false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_healthy_non_verbose_ok() {
        let runner = runner_healthy_3disk_base();
        let fs = fs_3disk();
        let config = config_3disk();

        let result = cmd_status(&runner, &fs, &config, false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_healthy_verbose_ok() {
        let runner = runner_healthy_3disk_verbose(runner_healthy_3disk_base());
        let fs = fs_3disk();
        let config = config_3disk();

        let result = cmd_status(&runner, &fs, &config, true, false);
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_healthy_json_ok() {
        let runner = runner_healthy_3disk_base();
        let fs = fs_3disk();
        let config = config_3disk();

        let result = cmd_status(&runner, &fs, &config, false, true);
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_degraded_non_verbose_ok() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson { mount_point: "/mnt/storage".into() },
                findmnt_btrfs(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow { mount_point: "/mnt/storage".into() },
                btrfs_show_3disk_1missing(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus { mapper: "disk1".into() },
                cryptsetup_status_active("disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/vda".into() },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus { mapper: "disk2".into() },
                cryptsetup_status_active("disk2", "/dev/vdb"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/vdb".into() },
                cryptsetup_uuid_ok("/dev/vdb", "22222222-2222-2222-2222-222222222222"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson { mount_point: "/mnt/storage".into() },
                btrfs_df_raid1(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw { mount_point: "/mnt/storage".into() },
                btrfs_usage_raw(),
            )
            .with_output(
                CmdRequest::BtrfsScrubStatus { mount_point: "/mnt/storage".into() },
                btrfs_scrub_never(),
            );
        let fs = fs_3disk();
        let config = config_3disk();

        let result = cmd_status(&runner, &fs, &config, false, false);
        assert!(result.is_ok());
    }

    #[test]
    fn cmd_status_single_disk_ok() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson { mount_point: "/mnt/storage".into() },
                findmnt_btrfs(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow { mount_point: "/mnt/storage".into() },
                btrfs_show_1disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus { mapper: "disk1".into() },
                cryptsetup_status_active("disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/vda".into() },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson { mount_point: "/mnt/storage".into() },
                btrfs_df_single(),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw { mount_point: "/mnt/storage".into() },
                btrfs_usage_raw(),
            )
            .with_output(
                CmdRequest::BtrfsScrubStatus { mount_point: "/mnt/storage".into() },
                btrfs_scrub_never(),
            );
        let fs = fs_1disk();
        let config = config_1disk();

        let result = cmd_status(&runner, &fs, &config, false, false);
        assert!(result.is_ok());
    }
}
