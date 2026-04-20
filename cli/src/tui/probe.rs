use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::Instant;

use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::FanControl;
use crate::luks;
use crate::parse::types::{ScrubState, SmartHealth, SmartProbe};
use crate::parse::{
    parse_btrfs_device_stats, parse_btrfs_device_usage, parse_btrfs_filesystem_usage,
    parse_btrfs_scrub_status, parse_cryptsetup_luks_dump, parse_lsblk_json, parse_smartctl,
};
use crate::probe::{probe_config_disk, probe_pool, Filesystem, ProbeError};
use crate::state_paths::StatePaths;
use crate::status::resolve_alert_state;
use crate::status::{estimate_pool_capacity, get_balance_report, DiskErrors};
use crate::tui::model::{
    DaemonStatus, DiskLuksInfo, DiskUsage, DrivingDrive, FanReading, FanSnapshot, PoolState,
    TemperatureDiskId, TemperatureReading, UnpooledDiskRender,
};
use crate::types::{ByIdPath, ConfigDiskState, LuksUuid, MountPoint};

pub fn probe_pool_for_tui<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    disk_by_id: &HashMap<String, String>,
    paths: &StatePaths,
) -> Result<Option<PoolState>, String> {
    let domain = probe_pool(runner, mount_point).map_err(|e| e.to_string())?;

    if !domain.mounted {
        return Ok(None);
    }

    let df_raw = runner
        .run(&CmdRequest::BtrfsFilesystemDfJson {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| e.to_string())?;
    let df = crate::parse::parse_btrfs_df_json(&df_raw).map_err(|e| e.to_string())?;

    let dev_usage_raw = runner
        .run(&CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| e.to_string())?;
    let dev_usage = parse_btrfs_device_usage(&dev_usage_raw).map_err(|e| e.to_string())?;

    // Map devid → disk name via probe_pool's devices (from btrfs filesystem show,
    // which reports stable /dev/mapper/braid-* paths). btrfs device usage may
    // report raw /dev/dm-N paths that don't match config disk names.
    let devid_to_name: HashMap<u64, &str> = domain
        .devices
        .iter()
        .filter_map(|d| crate::config::name_from_mapper(&d.mapper.0).map(|name| (d.devid, name)))
        .collect();

    // Map disk name -> LuksUuid for physical-identity keying of the TUI's
    // session temperature watermarks. Same pattern as devid_to_name --
    // mapper-name parse keys both onto the config disk name.
    let name_to_luks_uuid: HashMap<&str, LuksUuid> = domain
        .devices
        .iter()
        .filter_map(|d| {
            crate::config::name_from_mapper(&d.mapper.0).map(|name| (name, d.luks_uuid.clone()))
        })
        .collect();

    let mut disk_usage = HashMap::new();
    for entry in &dev_usage.devices {
        let disk_name = match devid_to_name.get(&entry.devid) {
            Some(name) => *name,
            None => continue,
        };
        disk_usage.insert(
            disk_name.to_owned(),
            DiskUsage {
                size: entry.device_size,
                allocations: entry.allocations.clone(),
                unallocated: entry.unallocated,
            },
        );
    }

    let scrub = runner
        .run(&CmdRequest::BtrfsScrubStatus {
            mount_point: mount_point.clone(),
        })
        .ok()
        .and_then(|raw| parse_btrfs_scrub_status(&raw).ok())
        .map(|out| out.state)
        .unwrap_or(ScrubState::Unknown);

    let balance = get_balance_report(runner, mount_point);

    let mut smart_health = HashMap::new();
    let mut disk_temperature_readings = HashMap::new();
    let mut luks_info = HashMap::new();
    for (disk_name, by_id_path) in disk_by_id {
        let probe = runner
            .run(&CmdRequest::SmartctlHealthJson {
                device: by_id_path.clone(),
            })
            .map(|raw| parse_smartctl(&raw))
            .unwrap_or(SmartProbe {
                health: SmartHealth::Unknown,
                celsius: None,
            });
        smart_health.insert(disk_name.clone(), probe.health);

        if let Some(celsius) = probe.celsius {
            let id = match name_to_luks_uuid.get(disk_name.as_str()) {
                Some(uuid) => TemperatureDiskId::LuksUuid(uuid.clone()),
                None => TemperatureDiskId::ByIdPath(ByIdPath(by_id_path.clone())),
            };
            disk_temperature_readings
                .insert(disk_name.clone(), TemperatureReading { id, celsius });
        }

        if let Ok(raw) = runner.run(&CmdRequest::CryptsetupLuksDump {
            device: by_id_path.clone(),
        })
            && let Ok(dump) = parse_cryptsetup_luks_dump(&raw) {
                luks_info.insert(
                    disk_name.clone(),
                    DiskLuksInfo {
                        cipher: dump.cipher,
                        key_size_bits: dump.key_size_bits,
                        keyslot_count: dump.keyslot_count,
                    },
                );
            }
    }

    // Extract transport type (sata, nvme, usb, etc.) from lsblk tree.
    // Walk parent devices: for each child named "braid-{name}", take the
    // parent's TRAN value. TRAN is only set on physical devices, not dm-crypt.
    let mut disk_transport = HashMap::new();
    if let Ok(lsblk_raw) = runner.run(&CmdRequest::LsblkJson)
        && let Ok(lsblk) = parse_lsblk_json(&lsblk_raw) {
            for dev in &lsblk.blockdevices {
                if let Some(tran) = &dev.tran {
                    for child in &dev.children {
                        if let Some(name) = crate::config::name_from_mapper(&child.name) {
                            disk_transport.insert(name.to_owned(), tran.clone());
                        }
                    }
                }
            }
        }

    let fs_usage_raw = runner
        .run(&CmdRequest::BtrfsFilesystemUsageRaw {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| e.to_string())?;
    let fs_usage = parse_btrfs_filesystem_usage(&fs_usage_raw).map_err(|e| e.to_string())?;

    // Device error stats
    let mut device_errors = HashMap::new();
    let device_stats_raw = runner
        .run(&CmdRequest::BtrfsDeviceStatsJson {
            mount_point: mount_point.clone(),
        })
        .ok();
    let device_stats = device_stats_raw
        .as_ref()
        .and_then(|raw| parse_btrfs_device_stats(raw).ok());
    if let Some(ref stats) = device_stats {
        for dev in &stats.devices {
            if let Some(name) = dev
                .target
                .as_path()
                .and_then(|p| p.strip_prefix("/dev/mapper/braid-"))
            {
                device_errors.insert(
                    name.to_owned(),
                    DiskErrors {
                        read: dev.read_io_errs,
                        write: dev.write_io_errs,
                        flush: dev.flush_io_errs,
                        corruption: dev.corruption_errs,
                        generation: dev.generation_errs,
                    },
                );
            }
        }
    }

    let alert_state = resolve_alert_state(paths);

    let capacity_total_bytes = if domain.missing_count == 0 {
        let sizes: Vec<u64> = dev_usage.devices.iter().map(|d| d.device_size).collect();
        Some(estimate_pool_capacity(&sizes))
    } else {
        None
    };

    // Classify any declared disk that is NOT in the live pool's
    // disk_usage so the disk table can render Unreadable / Damaged /
    // UnknownLuks / Missing distinctly. The live-pool UUID set is built
    // from `domain.devices` (the authoritative live source); luks_info
    // here only carries cipher/keyslot metadata, not UUIDs, so it cannot
    // answer the "valid LUKS but not in this pool" question.
    let live_pool_uuids: HashSet<LuksUuid> =
        domain.devices.iter().map(|d| d.luks_uuid.clone()).collect();
    let mut unpooled_disks: HashMap<String, UnpooledDiskRender> = HashMap::new();
    for (disk_name, by_id_path) in disk_by_id {
        if disk_usage.contains_key(disk_name) {
            continue;
        }
        let by_id = ByIdPath(by_id_path.clone());
        let probed = match probe_config_disk(runner, fs, disk_name, &by_id) {
            Ok(p) => p,
            Err(ProbeError::UnsupportedLuksVersion { version, .. }) => {
                // Surface the wrong-version disk explicitly instead of
                // silently skipping it. The TUI is the only diagnostic
                // path that doesn't bail on the gateway error.
                unpooled_disks.insert(
                    disk_name.clone(),
                    UnpooledDiskRender::WrongLuksVersion(version),
                );
                continue;
            }
            Err(_) => continue, // degrade gracefully — skip this disk
        };
        let render = match probed.state {
            ConfigDiskState::Absent => UnpooledDiskRender::Missing,
            ConfigDiskState::PresentLuks { uuid, .. } => {
                if live_pool_uuids.contains(&uuid) {
                    // The disk is part of the live pool by UUID but is
                    // somehow absent from disk_usage — treat as Missing
                    // defensively rather than lying about state.
                    UnpooledDiskRender::Missing
                } else {
                    UnpooledDiskRender::UnknownLuks
                }
            }
            ConfigDiskState::PresentNotLuks => {
                // Refine PresentNotLuks (luksUuid failed) into Unreadable
                // vs Damaged for diagnostic rendering only — do NOT
                // propagate the refinement back into ConfigDiskState.
                // Mutating commands (add/replace) keep the coarse state.
                match luks::probe_luks_header(runner, by_id_path) {
                    luks::LuksHeaderState::Damaged => UnpooledDiskRender::LuksHeaderDamaged,
                    // Unreadable, the inconsistent Ok-but-luksUuid-failed
                    // case, and ProbeFailed all collapse to Unreadable
                    // (consistent with mount.rs::plan_open_pool).
                    _ => UnpooledDiskRender::LuksHeaderUnreadable,
                }
            }
        };
        unpooled_disks.insert(disk_name.clone(), render);
    }

    Ok(Some(PoolState {
        mount_point: mount_point.clone(),
        df_entries: df.entries,
        disk_usage,
        disk_transport,
        smart_health,
        disk_temperature_readings,
        luks_info,
        device_errors,
        unpooled_disks,
        alert_state,
        scrub,
        balance,
        capacity_total_bytes,
        capacity_used_bytes: fs_usage.used_bytes,
        probed_at: Instant::now(),
    }))
}

/// Unit that backs `braid.fanControl` on the host. Single source of truth
/// for `probe_daemon_status`; tests can shadow this.
const FAN_DAEMON_UNIT: &str = "hddfancontrol-braid.service";

/// Snapshot of the chassis-fan subsystem for the Data tab.
///
/// Three independent sub-reads (fan hardware, hottest ATA drivetemp,
/// daemon liveness), each best-effort. Failures in one do not cascade
/// into the others. Daemon liveness via `systemctl is-active` is the
/// source of truth for whether hddfancontrol is actually driving the
/// fan — sensor values can look healthy while the control loop is down.
///
/// Paths are injected (`sysfs_root`, `dev_root`) so the sysfs traversal
/// and `/dev/disk/by-id/ata-*` selector can run under tempdirs in tests.
/// Production passes `Path::new("/sys")` / `Path::new("/dev")`.
pub fn probe_fan_for_tui<R: CommandRunner>(
    runner: &R,
    sysfs_root: &Path,
    dev_root: &Path,
    disk_by_id: &HashMap<String, String>,
    fan_control: &FanControl,
) -> FanSnapshot {
    let fan = resolve_pwm_dir(sysfs_root, fan_control)
        .and_then(|dir| read_fan_hardware(&dir, fan_control.pwm.number));

    let ata_drives = enumerate_ata_drives(dev_root);
    let mut temps: Vec<(String, i16)> = Vec::new();
    for sd in &ata_drives {
        if let Some(c) = read_drivetemp(sysfs_root, sd) {
            temps.push((sd.clone(), c));
        }
    }
    let sd_to_friendly = map_disk_by_id_to_sd(dev_root, disk_by_id);
    let driving = pick_driving(&temps, &sd_to_friendly);

    let daemon = probe_daemon_status(runner, FAN_DAEMON_UNIT);

    FanSnapshot {
        fan,
        driving,
        daemon,
        probed_at: Instant::now(),
    }
}

/// Resolve `<sysfs_root>/devices/platform/<dev>/hwmon/hwmon*/{device/,}pwmN`
/// to the single directory containing `pwmN` and `fanN_input`. Mirrors the
/// resolution logic in `modules/braid/fan-control.nix` (lines 166-187):
/// exactly one match across both `hwmon*/device/pwmN` and `hwmon*/pwmN`
/// layouts. Zero or more than one → `None` so the UI renders "-/-"
/// rather than picking arbitrarily.
fn resolve_pwm_dir(sysfs_root: &Path, fc: &FanControl) -> Option<PathBuf> {
    let base = sysfs_root
        .join("devices/platform")
        .join(&fc.pwm.platform_device)
        .join("hwmon");
    let pwm_file = format!("pwm{}", fc.pwm.number);
    let entries = std::fs::read_dir(&base).ok()?;
    let mut matches: Vec<PathBuf> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with("hwmon") {
            continue;
        }
        let hwmon_path = entry.path();
        if hwmon_path.join(&pwm_file).is_file() {
            matches.push(hwmon_path.clone());
        }
        let device_layout = hwmon_path.join("device");
        if device_layout.join(&pwm_file).is_file() {
            matches.push(device_layout);
        }
    }
    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

/// Read `pwmN` (0-255) and sibling `fanN_input` (RPM). Either file failing
/// to open/parse collapses both to `None` — they live in the same sysfs
/// directory, so their failure modes are correlated.
fn read_fan_hardware(pwm_dir: &Path, n: u8) -> Option<FanReading> {
    let pwm_path = pwm_dir.join(format!("pwm{n}"));
    let fan_path = pwm_dir.join(format!("fan{n}_input"));
    let pwm_raw = std::fs::read_to_string(&pwm_path)
        .ok()?
        .trim()
        .parse::<u8>()
        .ok()?;
    let rpm = std::fs::read_to_string(&fan_path)
        .ok()?
        .trim()
        .parse::<u32>()
        .ok()?;
    Some(FanReading { pwm_raw, rpm })
}

/// Mirror of hddfancontrol's `-d ata` selector
/// (`reference/hddfancontrol/src/cl.rs:117-135`): enumerate
/// `<dev_root>/disk/by-id/` entries whose name starts with `ata-` and
/// does NOT end in `-partN`, canonicalize the symlink, keep only
/// targets under `dev_root` with `sdX`-shaped file names. Broken
/// symlinks and anything that canonicalizes outside `dev_root` are
/// silently skipped — this is a display-layer approximation of what
/// the daemon sees, not parity.
fn enumerate_ata_drives(dev_root: &Path) -> Vec<String> {
    let by_id = dev_root.join("disk/by-id");
    let Ok(entries) = std::fs::read_dir(&by_id) else {
        return vec![];
    };
    let Ok(canon_dev) = std::fs::canonicalize(dev_root) else {
        return vec![];
    };
    let mut drives: Vec<String> = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name_str) = name.to_str() else {
            continue;
        };
        if !name_str.starts_with("ata-") {
            continue;
        }
        // Partition exclusion: strip trailing digits and check for `-part`.
        // Matches `reference/hddfancontrol/src/cl.rs:128`.
        if name_str
            .trim_end_matches(|c: char| c.is_ascii_digit())
            .ends_with("-part")
        {
            continue;
        }
        let Ok(target) = std::fs::canonicalize(entry.path()) else {
            continue;
        };
        if !target.starts_with(&canon_dev) {
            continue;
        }
        let Some(sd) = target.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        if !is_sd_shaped(sd) {
            continue;
        }
        drives.push(sd.to_owned());
    }
    drives.sort();
    drives.dedup();
    drives
}

fn is_sd_shaped(name: &str) -> bool {
    let Some(suffix) = name.strip_prefix("sd") else {
        return false;
    };
    !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_alphabetic())
}

/// Mirror `reference/hddfancontrol/src/probe/drivetemp.rs:20-46`: walk
/// `<sysfs_root>/block/<sdX>/../../hwmon/` looking for a subdir whose
/// `name` file equals `drivetemp`, then read sibling `temp1_input`
/// (millidegrees). The traversal via `../../` resolves through the
/// `/sys/block/sdX` symlink into the drive-specific device subtree,
/// so each drive lands in its own hwmon dir.
fn read_drivetemp(sysfs_root: &Path, sd_name: &str) -> Option<i16> {
    let hwmon_dir = sysfs_root.join("block").join(sd_name).join("../../hwmon");
    if !hwmon_dir.is_dir() {
        return None;
    }
    for entry in std::fs::read_dir(&hwmon_dir).ok()?.flatten() {
        let subdir = entry.path();
        if !subdir.is_dir() {
            continue;
        }
        let name = match std::fs::read_to_string(subdir.join("name")) {
            Ok(s) => s.trim_end().to_owned(),
            Err(_) => continue,
        };
        if name != "drivetemp" {
            continue;
        }
        let raw = std::fs::read_to_string(subdir.join("temp1_input")).ok()?;
        let millicelsius = raw.trim().parse::<i32>().ok()?;
        return Some((millicelsius / 1000) as i16);
    }
    None
}

/// For each pool member (friendly name → /dev/disk/by-id/... path),
/// canonicalize through `dev_root` and produce a `sdX → friendly`
/// reverse map. Entries whose canonicalization lands outside `dev_root`
/// are silently dropped so an unrelated USB-attached disk in the pool
/// can't collide with a real ATA sdX.
fn map_disk_by_id_to_sd(
    dev_root: &Path,
    disk_by_id: &HashMap<String, String>,
) -> HashMap<String, String> {
    let Ok(canon_dev) = std::fs::canonicalize(dev_root) else {
        return HashMap::new();
    };
    let mut out = HashMap::new();
    for (friendly, by_id_path) in disk_by_id {
        let Ok(target) = std::fs::canonicalize(Path::new(by_id_path)) else {
            continue;
        };
        if !target.starts_with(&canon_dev) {
            continue;
        }
        let Some(sd) = target.file_name().and_then(|f| f.to_str()) else {
            continue;
        };
        out.insert(sd.to_owned(), friendly.clone());
    }
    out
}

/// Pick the hottest drive; ties broken alphabetically by sdX so snapshot
/// tests are deterministic. Friendly label lookup falls back to the raw
/// sdX when the drive isn't a known pool member (drivetemp covers all
/// SATA disks, not just pool members).
fn pick_driving(
    temps: &[(String, i16)],
    sd_to_friendly: &HashMap<String, String>,
) -> Option<DrivingDrive> {
    let mut best: Option<&(String, i16)> = None;
    for entry in temps {
        best = Some(match best {
            None => entry,
            Some(b) => {
                if entry.1 > b.1 || (entry.1 == b.1 && entry.0 < b.0) {
                    entry
                } else {
                    b
                }
            }
        });
    }
    let (sd, celsius) = best?;
    let label = sd_to_friendly.get(sd).cloned().unwrap_or_else(|| sd.clone());
    Some(DrivingDrive {
        label,
        celsius: *celsius,
    })
}

/// Parse `systemctl is-active <unit>`. Exits non-zero (3) for
/// `inactive`/`failed` but still prints the status word — inspect
/// stdout regardless of exit code. Anything unrecognised, or a spawn
/// error, becomes `Unknown`.
fn probe_daemon_status<R: CommandRunner>(runner: &R, unit: &str) -> DaemonStatus {
    let req = CmdRequest::SystemctlIsActive {
        unit: unit.to_owned(),
    };
    let raw = match runner.run(&req) {
        Ok(r) => r,
        Err(_) => return DaemonStatus::Unknown,
    };
    match raw.stdout.trim() {
        "active" => DaemonStatus::Active,
        "activating" | "reloading" | "deactivating" => DaemonStatus::Transitioning,
        "inactive" => DaemonStatus::Inactive,
        "failed" => DaemonStatus::Failed,
        _ => DaemonStatus::Unknown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::parse::types::DeviceAllocation;

    fn test_paths() -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        (tmp, paths)
    }

    /// Filesystem stub for TUI probe tests. The unpooled-disk loop calls
    /// `probe_config_disk` which checks `fs.exists(by_id_path)` first; an
    /// empty default returns false (treated as Absent).
    struct StubFs {
        present_paths: Vec<String>,
    }

    impl StubFs {
        fn empty() -> Self {
            Self {
                present_paths: vec![],
            }
        }

        fn with_paths(paths: &[&str]) -> Self {
            Self {
                present_paths: paths.iter().map(|s| (*s).to_owned()).collect(),
            }
        }
    }

    impl Filesystem for StubFs {
        fn exists(&self, path: &str) -> bool {
            self.present_paths.iter().any(|p| p == path)
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }

        fn read_to_string(&self, _path: &str) -> Result<String, std::io::Error> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "stub"))
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

    /// Intent: probe_pool_for_tui passes through per-device allocations and
    /// unallocated bytes from btrfs device usage into DiskUsage, rather than
    /// collapsing them into aggregate data/metadata sums.
    ///
    /// Why: the old code discarded per-allocation detail (type + profile),
    /// making it impossible to show a breakdown in the disk detail panel.
    ///
    /// Scenario: 2-disk RAID1 pool. btrfs device usage reports Data, Metadata,
    /// and System allocations per device. The TUI probe must preserve all three
    /// allocation rows and the unallocated value for each disk.
    #[test]
    fn allocations_passed_through() {
        let mp = MountPoint("/mnt/storage".to_owned());

        let runner = MockRunner::default()
            // probe_pool: findmnt
            .with_output(
                CmdRequest::FindmntJson { mount_point: mp.clone() },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-toshiba","fstype":"btrfs"}]}"#,
                ),
            )
            // probe_pool: btrfs filesystem show
            .with_output(
                CmdRequest::BtrfsFilesystemShow { mount_point: mp.clone() },
                ok_raw(
                    "btrfs filesystem show",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 2 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n\
                     \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-ironwolf\n",
                ),
            )
            // probe_pool: cryptsetup status for each device
            .with_output(
                CmdRequest::CryptsetupStatus { mapper: "braid-toshiba".into() },
                ok_raw(
                    "cryptsetup status",
                    "/dev/mapper/braid-toshiba is active.\n\tdevice:  /dev/vda\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/vda".into() },
                ok_raw("cryptsetup luksUUID", "11111111-1111-1111-1111-111111111111\n"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus { mapper: "braid-ironwolf".into() },
                ok_raw(
                    "cryptsetup status",
                    "/dev/mapper/braid-ironwolf is active.\n\tdevice:  /dev/vdb\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/vdb".into() },
                ok_raw("cryptsetup luksUUID", "22222222-2222-2222-2222-222222222222\n"),
            )
            // btrfs filesystem df --json
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson { mount_point: mp.clone() },
                ok_raw(
                    "btrfs filesystem df",
                    r#"{"filesystem-df": [
                        {"bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216},
                        {"bg-type": "System", "bg-profile": "RAID1", "total": 4194304, "used": 16384},
                        {"bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 65536},
                        {"bg-type": "GlobalReserve", "bg-profile": "single", "total": 3670016, "used": 0}
                    ]}"#,
                ),
            )
            // btrfs device usage --raw (the key part we're testing)
            .with_output(
                CmdRequest::BtrfsDeviceUsageRaw { mount_point: mp.clone() },
                ok_raw(
                    "btrfs device usage",
                    "/dev/dm-0, ID: 1\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:              0\n\
                     \x20  Data,RAID1:           67108864\n\
                     \x20  Metadata,DUP:         51970048\n\
                     \x20  System,DUP:            8388608\n\
                     \x20  Unallocated:          409403392\n\
                     \n\
                     /dev/dm-1, ID: 2\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:              0\n\
                     \x20  Data,RAID1:           67108864\n\
                     \x20  Metadata,DUP:         51970048\n\
                     \x20  System,DUP:            8388608\n\
                     \x20  Unallocated:          409403392\n",
                ),
            );

        let runner = runner
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs filesystem usage",
                    "Overall:\n\
                     \tDevice size:\t\t\t1073741824\n\
                     \tDevice allocated:\t\t503316480\n\
                     \tDevice unallocated:\t\t570425344\n\
                     \tUsed:\t\t\t\t33914880\n\
                     \tFree (estimated):\t\t442957824\t(min: 442957824)\n\
                     \tData ratio:\t\t\t2.00\n",
                ),
            );

        let result = probe_pool_for_tui(
            &runner,
            &StubFs::empty(),
            &MountPoint("/mnt/storage".into()),
            &HashMap::new(),
            &test_paths().1,
        )
        .unwrap();
        let pool = result.expect("pool should be Some");

        // Verify balance is idle
        assert_eq!(pool.balance, crate::status::BalanceReport::Idle);

        // Verify toshiba (devid 1) allocations
        let toshiba = pool
            .disk_usage
            .get("toshiba")
            .expect("toshiba should be present");
        assert_eq!(toshiba.size, 536870912);
        assert_eq!(toshiba.unallocated, 409403392);
        assert_eq!(toshiba.allocations.len(), 3);
        assert_eq!(
            toshiba.allocations[0],
            DeviceAllocation {
                alloc_type: "Data".into(),
                profile: "RAID1".into(),
                bytes: 67108864
            },
        );
        assert_eq!(
            toshiba.allocations[1],
            DeviceAllocation {
                alloc_type: "Metadata".into(),
                profile: "DUP".into(),
                bytes: 51970048
            },
        );
        assert_eq!(
            toshiba.allocations[2],
            DeviceAllocation {
                alloc_type: "System".into(),
                profile: "DUP".into(),
                bytes: 8388608
            },
        );
        assert_eq!(toshiba.allocated(), 67108864 + 51970048 + 8388608);

        // Verify ironwolf (devid 2) has same structure
        let ironwolf = pool
            .disk_usage
            .get("ironwolf")
            .expect("ironwolf should be present");
        assert_eq!(ironwolf.allocations.len(), 3);
        assert_eq!(ironwolf.unallocated, 409403392);

        // Verify capacity: 2 equal disks of 536870912 → estimated total = 536870912
        assert_eq!(pool.capacity_total_bytes, Some(536870912));
        assert_eq!(pool.capacity_used_bytes, 33914880);
    }

    /// Helper: build the minimum mock-runner mocks for a single-disk
    /// mounted-pool probe so the unpooled-disk classification tests can
    /// reuse them. Returns a runner with everything set up except for
    /// any per-test cryptsetup mocks the caller wants to add for a
    /// declared but unpooled disk.
    fn one_disk_mounted_pool_runner() -> MockRunner {
        let mp = MountPoint("/mnt/storage".to_owned());
        MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: mp.clone(),
                },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-toshiba","fstype":"btrfs"}]}"#,
                ),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: mp.clone(),
                },
                ok_raw(
                    "btrfs filesystem show",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 1 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-toshiba".into(),
                },
                ok_raw(
                    "cryptsetup status",
                    "/dev/mapper/braid-toshiba is active.\n\tdevice:  /dev/vda\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson {
                    mount_point: mp.clone(),
                },
                ok_raw(
                    "btrfs filesystem df",
                    r#"{"filesystem-df": [
                        {"bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216}
                    ]}"#,
                ),
            )
            .with_output(
                CmdRequest::BtrfsDeviceUsageRaw {
                    mount_point: mp.clone(),
                },
                ok_raw(
                    "btrfs device usage",
                    "/dev/dm-0, ID: 1\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:              0\n\
                     \x20  Data,RAID1:           67108864\n\
                     \x20  Unallocated:          409403392\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: mp.clone(),
                },
                ok_raw("btrfs balance status", "No balance found on '/mnt/storage'\n"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw {
                    mount_point: mp.clone(),
                },
                ok_raw(
                    "btrfs filesystem usage",
                    "Overall:\n\
                     \tDevice size:\t\t\t1073741824\n\
                     \tDevice allocated:\t\t503316480\n\
                     \tDevice unallocated:\t\t570425344\n\
                     \tUsed:\t\t\t\t33914880\n\
                     \tFree (estimated):\t\t442957824\t(min: 442957824)\n\
                     \tData ratio:\t\t\t2.00\n",
                ),
            )
    }

    /// Intent: probe_pool_for_tui must classify a declared disk that is
    /// absent from the host as `UnpooledDiskRender::Missing`.
    ///
    /// Why: this is the existing "device unplugged" baseline; ensuring it
    /// produces a record (not just a no-entry hole) means the disk table
    /// can render a per-row label even for the simple unplugged case.
    ///
    /// Scenario: 1-disk live pool plus a second declared disk whose
    /// /dev/disk/by-id path does not exist on the host.
    #[test]
    fn unpooled_disk_absent_classified_as_missing() {
        let runner = one_disk_mounted_pool_runner();
        let fs = StubFs::with_paths(&["/dev/disk/by-id/braid-toshiba"]);

        let disk_by_id = HashMap::from([
            ("toshiba".to_owned(), "/dev/disk/by-id/braid-toshiba".to_owned()),
            ("ironwolf".to_owned(), "/dev/disk/by-id/braid-ironwolf".to_owned()),
        ]);

        let pool = probe_pool_for_tui(
            &runner,
            &fs,
            &MountPoint("/mnt/storage".into()),
            &disk_by_id,
            &test_paths().1,
        )
            .unwrap()
            .expect("pool should be Some");

        assert_eq!(
            pool.unpooled_disks.get("ironwolf"),
            Some(&UnpooledDiskRender::Missing)
        );
        // toshiba is in the live pool — must NOT be in unpooled_disks.
        assert!(
            !pool.unpooled_disks.contains_key("toshiba"),
            "live disks must not appear in unpooled_disks"
        );
    }

    /// Intent: probe_pool_for_tui must classify a declared disk that has a
    /// valid LUKS header whose UUID does NOT belong to the live pool as
    /// `UnpooledDiskRender::UnknownLuks` — distinct from "missing".
    ///
    /// Why: a stale-LUKS disk left over from a previous pool, or a disk
    /// belonging to a different braid instance, should be visibly
    /// different from a hot-unplugged cable so the operator does not
    /// confuse them.
    ///
    /// Scenario: 1-disk live pool with UUID `11111111...`. Second declared
    /// disk has a valid LUKS header reporting UUID `99999999...`.
    #[test]
    fn unpooled_disk_present_luks_unknown_uuid_classified_as_unknown_luks() {
        let runner = one_disk_mounted_pool_runner()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID",
                    "99999999-9999-9999-9999-999999999999\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                ok_raw(
                    "cryptsetup luksDump",
                    "LUKS header information\nVersion:       \t2\n",
                ),
            );
        let fs = StubFs::with_paths(&[
            "/dev/disk/by-id/braid-toshiba",
            "/dev/disk/by-id/braid-ironwolf",
        ]);

        let disk_by_id = HashMap::from([
            ("toshiba".to_owned(), "/dev/disk/by-id/braid-toshiba".to_owned()),
            ("ironwolf".to_owned(), "/dev/disk/by-id/braid-ironwolf".to_owned()),
        ]);

        let pool = probe_pool_for_tui(
            &runner,
            &fs,
            &MountPoint("/mnt/storage".into()),
            &disk_by_id,
            &test_paths().1,
        )
            .unwrap()
            .expect("pool should be Some");

        assert_eq!(
            pool.unpooled_disks.get("ironwolf"),
            Some(&UnpooledDiskRender::UnknownLuks)
        );
    }

    /// Intent: probe_pool_for_tui must refine PresentNotLuks → Unreadable
    /// when probe_luks_header reports the LUKS magic is gone.
    ///
    /// Why: the previous TUI rendered every unrepresented disk as plain
    /// "missing"; users could not see whether a header restore was the
    /// right next step. Surfacing Unreadable as a distinct state is the
    /// trigger that points the user at off-system header backups.
    ///
    /// Scenario: 1-disk live pool. Second declared disk: `luksUuid` exits
    /// non-zero, `isLuks` exits non-zero (LUKS magic missing).
    #[test]
    fn unpooled_disk_present_not_luks_unreadable_classified_correctly() {
        let runner = one_disk_mounted_pool_runner()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksUUID".into(),
                    stdout: String::new(),
                    stderr: "Device is not a valid LUKS device.\n".into(),
                    exit_status: 1,
                },
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup isLuks".into(),
                    stdout: String::new(),
                    stderr: "Device is not a valid LUKS device.\n".into(),
                    exit_status: 1,
                },
            );
        let fs = StubFs::with_paths(&[
            "/dev/disk/by-id/braid-toshiba",
            "/dev/disk/by-id/braid-ironwolf",
        ]);

        let disk_by_id = HashMap::from([
            ("toshiba".to_owned(), "/dev/disk/by-id/braid-toshiba".to_owned()),
            ("ironwolf".to_owned(), "/dev/disk/by-id/braid-ironwolf".to_owned()),
        ]);

        let pool = probe_pool_for_tui(
            &runner,
            &fs,
            &MountPoint("/mnt/storage".into()),
            &disk_by_id,
            &test_paths().1,
        )
            .unwrap()
            .expect("pool should be Some");

        assert_eq!(
            pool.unpooled_disks.get("ironwolf"),
            Some(&UnpooledDiskRender::LuksHeaderUnreadable)
        );
    }

    /// Intent: probe_pool_for_tui must refine PresentNotLuks → Damaged
    /// when isLuks succeeds but luksDump fails — the metadata-corruption
    /// case that has a distinct `cryptsetup repair` recovery story.
    ///
    /// Why: metadata damage is potentially repairable in place; collapsing
    /// it into the same "missing" or even Unreadable bucket would steer
    /// the user away from a less-destructive recovery option.
    ///
    /// Scenario: 1-disk live pool. Second declared disk: `luksUuid` fails,
    /// `isLuks` succeeds, `luksDump` fails.
    #[test]
    fn unpooled_disk_present_not_luks_damaged_classified_correctly() {
        let runner = one_disk_mounted_pool_runner()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksUUID".into(),
                    stdout: String::new(),
                    stderr: "Cannot read LUKS header metadata.\n".into(),
                    exit_status: 1,
                },
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup isLuks".into(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksDump".into(),
                    stdout: String::new(),
                    stderr: "Cannot read LUKS header metadata.\n".into(),
                    exit_status: 1,
                },
            );
        let fs = StubFs::with_paths(&[
            "/dev/disk/by-id/braid-toshiba",
            "/dev/disk/by-id/braid-ironwolf",
        ]);

        let disk_by_id = HashMap::from([
            ("toshiba".to_owned(), "/dev/disk/by-id/braid-toshiba".to_owned()),
            ("ironwolf".to_owned(), "/dev/disk/by-id/braid-ironwolf".to_owned()),
        ]);

        let pool = probe_pool_for_tui(
            &runner,
            &fs,
            &MountPoint("/mnt/storage".into()),
            &disk_by_id,
            &test_paths().1,
        )
            .unwrap()
            .expect("pool should be Some");

        assert_eq!(
            pool.unpooled_disks.get("ironwolf"),
            Some(&UnpooledDiskRender::LuksHeaderDamaged)
        );
    }

    /// Intent: probe_pool_for_tui must surface a wrong-LUKS-version disk
    /// as `UnpooledDiskRender::WrongLuksVersion(version)` rather than
    /// silently skipping it (which is what the catch-all `Err(_) => continue`
    /// would otherwise do).
    ///
    /// Why: the gateway probe `probe_config_disk` returns a hard error for
    /// non-LUKS2 disks. The CLI command paths bail loudly with that error,
    /// but the TUI degrades gracefully — it must still tell the user the
    /// disk exists and explain why it's unusable, otherwise the disk would
    /// disappear from the table without explanation.
    ///
    /// Scenario: 1-disk live pool. Second declared disk is on-disk LUKS1
    /// (luksUuid succeeds, luksDump reports `Version: 1`).
    #[test]
    fn unpooled_disk_wrong_luks_version_classified_correctly() {
        let runner = one_disk_mounted_pool_runner()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID",
                    "22222222-2222-2222-2222-222222222222\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                ok_raw(
                    "cryptsetup luksDump",
                    "LUKS header information\n\
                     Version:       \t1\n\
                     Cipher name:   \taes\n",
                ),
            );
        let fs = StubFs::with_paths(&[
            "/dev/disk/by-id/braid-toshiba",
            "/dev/disk/by-id/braid-ironwolf",
        ]);

        let disk_by_id = HashMap::from([
            ("toshiba".to_owned(), "/dev/disk/by-id/braid-toshiba".to_owned()),
            ("ironwolf".to_owned(), "/dev/disk/by-id/braid-ironwolf".to_owned()),
        ]);

        let pool = probe_pool_for_tui(
            &runner,
            &fs,
            &MountPoint("/mnt/storage".into()),
            &disk_by_id,
            &test_paths().1,
        )
            .unwrap()
            .expect("pool should be Some");

        assert_eq!(
            pool.unpooled_disks.get("ironwolf"),
            Some(&UnpooledDiskRender::WrongLuksVersion(1))
        );
    }

    // ----- Fan probe tests -----

    use crate::config::{FanControl as FanControlCfg, Pwm};
    use std::os::unix::fs::symlink;

    fn sample_fan_control() -> FanControlCfg {
        FanControlCfg {
            pwm: Pwm {
                platform_device: "f71882fg.656".to_owned(),
                number: 2,
                min_start: 70,
                max_stop: 60,
            },
            min_temp: 30,
            max_temp: 40,
            min_fan_speed_percent: 20,
        }
    }

    /// Create a sysfs+dev fixture with the given drives (sd_name, temp in
    /// millicelsius). Each drive gets its OWN hwmon subdir via symlink
    /// traversal from /sys/block/sdX so the `../../hwmon` relative walk
    /// lands in a per-drive directory -- catches bugs where a flat fixture
    /// would let all drives share a single hwmon.
    fn build_sysfs_fixture(tmp: &Path, drives: &[(&str, i32)]) {
        let dev = tmp.join("dev");
        let by_id = dev.join("disk/by-id");
        let sys = tmp.join("sys");
        let sys_block = sys.join("block");
        std::fs::create_dir_all(&by_id).unwrap();
        std::fs::create_dir_all(&sys_block).unwrap();
        for (i, (sd, millicelsius)) in drives.iter().enumerate() {
            // Placeholder dev file -- enumerate_ata_drives canonicalizes
            // the ata-* symlink to this path.
            std::fs::write(dev.join(sd), b"").unwrap();
            // /dev/disk/by-id/ata-<UPPER> -> ../../sdX
            let ata_name = format!("ata-{}", sd.to_ascii_uppercase());
            let target = PathBuf::from("../..").join(sd);
            symlink(&target, by_id.join(&ata_name)).unwrap();
            // /sys/devices/pci/ataN/host0/target0/block/sdX/
            let ata_dir = sys.join(format!("devices/pci/ata{}/host0/target0", i + 1));
            let ata_block = ata_dir.join("block").join(sd);
            std::fs::create_dir_all(&ata_block).unwrap();
            // /sys/devices/pci/ataN/host0/target0/hwmon/hwmonN/{name,temp1_input}
            let hwmon_dir = ata_dir.join(format!("hwmon/hwmon{i}"));
            std::fs::create_dir_all(&hwmon_dir).unwrap();
            std::fs::write(hwmon_dir.join("name"), "drivetemp\n").unwrap();
            std::fs::write(hwmon_dir.join("temp1_input"), format!("{millicelsius}\n")).unwrap();
            // /sys/block/sdX -> ../devices/pci/ataN/host0/target0/block/sdX
            let rel_block = PathBuf::from("..")
                .join(format!("devices/pci/ata{}/host0/target0/block", i + 1))
                .join(sd);
            symlink(&rel_block, sys_block.join(sd)).unwrap();
        }
    }

    // Intent: resolve_pwm_dir accepts both the `hwmon*/device/pwmN` layout
    // (f71882fg, nct6775) and the `hwmon*/pwmN` fallback (some other drivers).
    // Why: hwmon layout varies across Super I/O drivers; requiring one layout
    // would silently leave half of supported chips unreadable in the TUI.
    // Scenario: fixture with exactly one matching pwm path.
    #[test]
    fn resolve_pwm_dir_accepts_device_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp
            .path()
            .join("sys/devices/platform/f71882fg.656/hwmon/hwmon4/device");
        std::fs::create_dir_all(&hwmon).unwrap();
        std::fs::write(hwmon.join("pwm2"), "128\n").unwrap();
        let fc = sample_fan_control();
        let resolved = resolve_pwm_dir(&tmp.path().join("sys"), &fc).unwrap();
        assert_eq!(resolved, hwmon);
    }

    #[test]
    fn resolve_pwm_dir_accepts_pwm_only_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let hwmon = tmp
            .path()
            .join("sys/devices/platform/f71882fg.656/hwmon/hwmon4");
        std::fs::create_dir_all(&hwmon).unwrap();
        std::fs::write(hwmon.join("pwm2"), "128\n").unwrap();
        let fc = sample_fan_control();
        let resolved = resolve_pwm_dir(&tmp.path().join("sys"), &fc).unwrap();
        assert_eq!(resolved, hwmon);
    }

    // Intent: 0 matches or >1 matches resolve to None.
    // Why: ambiguity is a config/hardware bug -- rendering fan values from
    // an arbitrary pick would be misleading. None surfaces as "-/-" in the
    // UI, which is what the user should see.
    // Scenario (0): platform dir exists but the PWM number isn't there.
    // Scenario (>1): two hwmon subdirs both have pwm2 (driver glitch).
    #[test]
    fn resolve_pwm_dir_returns_none_for_zero_or_many_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("sys/devices/platform/f71882fg.656/hwmon");
        std::fs::create_dir_all(&base).unwrap();
        let fc = sample_fan_control();
        assert!(resolve_pwm_dir(&tmp.path().join("sys"), &fc).is_none());

        let h0 = base.join("hwmon4");
        let h1 = base.join("hwmon5");
        std::fs::create_dir_all(&h0).unwrap();
        std::fs::create_dir_all(&h1).unwrap();
        std::fs::write(h0.join("pwm2"), "128\n").unwrap();
        std::fs::write(h1.join("pwm2"), "200\n").unwrap();
        assert!(resolve_pwm_dir(&tmp.path().join("sys"), &fc).is_none());
    }

    // Intent: read_fan_hardware returns the pair (pwm, rpm) on happy path
    // and None on any file error or parse error.
    // Why: a half-read (pwm but no fan, or vice versa) would let the UI
    // show one value with a dash next to it -- cluttered and confusing.
    // Treating them atomically keeps the "-/-" fallback crisp.
    // Scenario: happy path + missing files + non-numeric content.
    #[test]
    fn read_fan_hardware_happy_and_failure_cases() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join("pwm2"), "215\n").unwrap();
        std::fs::write(dir.join("fan2_input"), "1240\n").unwrap();
        let r = read_fan_hardware(dir, 2).unwrap();
        assert_eq!(r.pwm_raw, 215);
        assert_eq!(r.rpm, 1240);

        // Missing fan_input -> None.
        std::fs::remove_file(dir.join("fan2_input")).unwrap();
        assert!(read_fan_hardware(dir, 2).is_none());

        // Non-numeric pwm -> None.
        std::fs::write(dir.join("pwm2"), "garbage\n").unwrap();
        std::fs::write(dir.join("fan2_input"), "1240\n").unwrap();
        assert!(read_fan_hardware(dir, 2).is_none());
    }

    // Intent: enumerate_ata_drives keeps `ata-*` entries whose canonical
    // target is a real `sdX` under dev_root, and silently drops
    // partitions, non-ata entries, broken symlinks, and targets outside
    // dev_root.
    // Why: this is the TUI's approximation of hddfancontrol's `-d ata`
    // selector. Over-including partitions would double-count drives;
    // over-including USB-attached devices would misrepresent which drive
    // is driving the fan. Daemon health is still the source of truth --
    // this just powers the "Driving" column.
    // Scenario: fixture with a mix of ata-*, ata-*-part1, usb-*, nvme-*,
    // broken symlinks, and external-target symlinks.
    #[test]
    fn enumerate_ata_drives_filters_partitions_and_non_ata() {
        let tmp = tempfile::tempdir().unwrap();
        build_sysfs_fixture(tmp.path(), &[("sda", 38000), ("sdb", 45000)]);
        let by_id = tmp.path().join("dev/disk/by-id");

        // Partition entry pointing at sda.
        symlink(PathBuf::from("../..").join("sda"), by_id.join("ata-FOO-part1")).unwrap();
        // Non-ata buses that should be excluded.
        symlink(PathBuf::from("../..").join("sda"), by_id.join("usb-USBSTICK")).unwrap();
        symlink(PathBuf::from("../..").join("sda"), by_id.join("nvme-Samsung980")).unwrap();
        // Broken symlink.
        symlink(
            PathBuf::from("../..").join("nonexistent"),
            by_id.join("ata-BROKEN"),
        )
        .unwrap();
        // Target outside dev_root: create an unrelated file and point at it.
        let outside = tmp.path().join("outside-target");
        std::fs::write(&outside, b"").unwrap();
        symlink(&outside, by_id.join("ata-OUTSIDE")).unwrap();

        let drives = enumerate_ata_drives(&tmp.path().join("dev"));
        assert_eq!(drives, vec!["sda".to_owned(), "sdb".to_owned()]);
    }

    // Intent: enumerate_ata_drives skips targets that exist but whose
    // file_name is not sdX-shaped (e.g. ata-* symlink pointing at a
    // weird mdadm device).
    // Why: the Driving column assumes the label is usable with
    // read_drivetemp's sysfs walk, which only makes sense for sdX.
    // Scenario: rare but possible -- some storage controller exposes a
    // /dev/{something-else} under disk/by-id.
    #[test]
    fn enumerate_ata_drives_skips_non_sd_shaped_targets() {
        let tmp = tempfile::tempdir().unwrap();
        let dev = tmp.path().join("dev");
        let by_id = dev.join("disk/by-id");
        std::fs::create_dir_all(&by_id).unwrap();
        std::fs::write(dev.join("md0"), b"").unwrap();
        symlink(PathBuf::from("../..").join("md0"), by_id.join("ata-MDRAID")).unwrap();
        let drives = enumerate_ata_drives(&dev);
        assert!(drives.is_empty(), "got {drives:?}");
    }

    // Intent: read_drivetemp converts millicelsius to celsius, picks the
    // subdir whose `name` file equals "drivetemp", and returns None when
    // `temp1_input` is missing or unparseable.
    // Why: other hwmon subdirs in the same directory (coretemp, etc.)
    // report unrelated temps; picking by `name == "drivetemp"` is the
    // contract with the kernel module.
    // Scenario: hwmon dir with both drivetemp and an unrelated sibling.
    #[test]
    fn read_drivetemp_selects_drivetemp_by_name_file() {
        let tmp = tempfile::tempdir().unwrap();
        build_sysfs_fixture(tmp.path(), &[("sda", 38500)]);
        // Add a red-herring hwmon subdir under the same parent.
        let parent = tmp.path().join("sys/devices/pci/ata1/host0/target0/hwmon");
        let redherring = parent.join("hwmon9");
        std::fs::create_dir_all(&redherring).unwrap();
        std::fs::write(redherring.join("name"), "coretemp\n").unwrap();
        std::fs::write(redherring.join("temp1_input"), "99000\n").unwrap();

        let c = read_drivetemp(&tmp.path().join("sys"), "sda").unwrap();
        assert_eq!(c, 38);
    }

    #[test]
    fn read_drivetemp_missing_temp_file_returns_none() {
        let tmp = tempfile::tempdir().unwrap();
        build_sysfs_fixture(tmp.path(), &[("sda", 38500)]);
        let temp_file = tmp
            .path()
            .join("sys/devices/pci/ata1/host0/target0/hwmon/hwmon0/temp1_input");
        std::fs::remove_file(&temp_file).unwrap();
        assert!(read_drivetemp(&tmp.path().join("sys"), "sda").is_none());
    }

    // Intent: read_drivetemp traverses per-drive through the
    // /sys/block/sdX symlink so two drives with different temps resolve
    // to different hwmon dirs.
    // Why: a flat-dir fixture (or a wrong `../..` relative walk) would
    // collapse all drives to one hwmon -- both reads would return the
    // same temp, and the bug would only surface in production where
    // each drive DOES have a unique device subtree. This test makes
    // that regression impossible to merge.
    // Scenario: fixture with sda @ 38 C and sdb @ 45 C in independent
    // device subtrees; verify each drive reads its own value.
    #[test]
    fn read_drivetemp_per_drive_isolation() {
        let tmp = tempfile::tempdir().unwrap();
        build_sysfs_fixture(tmp.path(), &[("sda", 38000), ("sdb", 45000)]);
        let sysfs = tmp.path().join("sys");
        assert_eq!(read_drivetemp(&sysfs, "sda"), Some(38));
        assert_eq!(read_drivetemp(&sysfs, "sdb"), Some(45));
    }

    // Intent: map_disk_by_id_to_sd resolves pool members whose by-id
    // symlinks point inside dev_root, and drops those that don't.
    // Why: a pool member whose by-id path points outside the scanned
    // dev root (e.g. a stale absolute path on a recovered host) must
    // not collide with an unrelated real sdX.
    // Scenario: two pool members, one resolving inside dev_root and
    // one outside.
    #[test]
    fn map_disk_by_id_to_sd_filters_outside_targets() {
        let tmp = tempfile::tempdir().unwrap();
        build_sysfs_fixture(tmp.path(), &[("sda", 38000), ("sdb", 45000)]);
        let dev = tmp.path().join("dev");
        let outside = tmp.path().join("outside-file");
        std::fs::write(&outside, b"").unwrap();
        // Friendly -> by-id path inputs (the TUI's disk_by_id map).
        let disk_by_id = HashMap::from([
            (
                "toshiba".to_owned(),
                dev.join("disk/by-id/ata-SDA").display().to_string(),
            ),
            ("other".to_owned(), outside.display().to_string()),
        ]);
        let map = map_disk_by_id_to_sd(&dev, &disk_by_id);
        assert_eq!(map.get("sda"), Some(&"toshiba".to_owned()));
        assert!(!map.values().any(|v| v == "other"));
    }

    // Intent: pick_driving returns None on empty input, picks max
    // celsius, breaks ties alphabetically, and falls back to the raw
    // sdX label when not a known pool member.
    // Why: ties must be deterministic or snapshot tests will flake.
    // The friendly-name fallback is important because hddfancontrol
    // monitors all SATA drives, not just pool members.
    // Scenario: empty, single, tie, and unmapped-label cases.
    #[test]
    fn pick_driving_tie_break_and_label_fallback() {
        let map = HashMap::from([("sda".to_owned(), "toshiba".to_owned())]);
        assert!(pick_driving(&[], &map).is_none());

        let d = pick_driving(&[("sda".to_owned(), 38)], &map).unwrap();
        assert_eq!(d.label, "toshiba");
        assert_eq!(d.celsius, 38);

        // Tie at 42 between sdc and sdb -- alphabetical winner is sdb.
        let d = pick_driving(
            &[
                ("sdc".to_owned(), 42),
                ("sdb".to_owned(), 42),
                ("sda".to_owned(), 30),
            ],
            &map,
        )
        .unwrap();
        assert_eq!(d.label, "sdb"); // not in map, falls back to raw label
        assert_eq!(d.celsius, 42);
    }

    // Intent: probe_daemon_status parses every documented systemctl
    // is-active status word, including the inactive/failed cases where
    // systemctl exits 3, and defaults to Unknown on garbage or spawn
    // errors.
    // Why: systemctl is-active exits non-zero for inactive/failed but
    // still prints the status word -- a naive "bail on non-zero exit"
    // would collapse everything into Unknown and silently break the
    // Failed-state rendering.
    // Scenario: mock responses for each documented state + edge cases.
    #[test]
    fn probe_daemon_status_parses_all_states() {
        fn raw(stdout: &str, exit: i32) -> RawCommandOutput {
            RawCommandOutput {
                cmd: "systemctl is-active".into(),
                stdout: stdout.into(),
                stderr: String::new(),
                exit_status: exit,
            }
        }
        let req = CmdRequest::SystemctlIsActive {
            unit: "hddfancontrol-braid.service".to_owned(),
        };
        let cases: &[(&str, i32, DaemonStatus)] = &[
            ("active\n", 0, DaemonStatus::Active),
            ("activating\n", 0, DaemonStatus::Transitioning),
            ("reloading\n", 0, DaemonStatus::Transitioning),
            ("deactivating\n", 0, DaemonStatus::Transitioning),
            ("inactive\n", 3, DaemonStatus::Inactive),
            ("failed\n", 3, DaemonStatus::Failed),
            ("unknown\n", 4, DaemonStatus::Unknown),
            ("", 1, DaemonStatus::Unknown),
            ("\x00garbage\n", 1, DaemonStatus::Unknown),
        ];
        for (stdout, exit, expected) in cases {
            let mock =
                MockRunner::default().with_output(req.clone(), raw(stdout, *exit));
            let got = probe_daemon_status(&mock, "hddfancontrol-braid.service");
            assert_eq!(&got, expected, "stdout={stdout:?} exit={exit}");
        }

        // Spawn error path: MockRunner with no output returns
        // CmdError::MissingMock -> Unknown.
        let empty_mock = MockRunner::default();
        assert_eq!(
            probe_daemon_status(&empty_mock, "hddfancontrol-braid.service"),
            DaemonStatus::Unknown
        );
    }
}
