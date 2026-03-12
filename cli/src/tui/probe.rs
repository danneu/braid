use std::collections::HashMap;
use std::time::Instant;

use crate::cmd::{CmdRequest, CommandRunner};
use crate::hdparm::check_power_mode;
use crate::parse::types::{ScrubState, SmartHealth};
use crate::parse::{
    parse_btrfs_device_usage, parse_btrfs_filesystem_usage, parse_btrfs_scrub_status,
    parse_cryptsetup_luks_dump, parse_lsblk_json, parse_smartctl_health,
};
use crate::probe::probe_pool;
use crate::tui::model::{DiskLuksInfo, DiskUsage, PoolState};
use crate::types::MountPoint;

pub fn probe_pool_for_tui<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
    disk_by_id: &HashMap<String, String>,
) -> Result<Option<PoolState>, String> {
    let domain = probe_pool(runner, mount_point).map_err(|e| e.to_string())?;

    if !domain.mounted {
        return Ok(None);
    }

    let usage_raw = runner
        .run(&CmdRequest::BtrfsFilesystemUsageRaw {
            mount_point: MountPoint(mount_point.to_owned()),
        })
        .map_err(|e| e.to_string())?;
    let usage = parse_btrfs_filesystem_usage(&usage_raw).map_err(|e| e.to_string())?;

    let profile = if usage.data_ratio == 2 {
        "RAID1"
    } else {
        "single"
    };

    let dev_usage_raw = runner
        .run(&CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: MountPoint(mount_point.to_owned()),
        })
        .map_err(|e| e.to_string())?;
    let dev_usage = parse_btrfs_device_usage(&dev_usage_raw).map_err(|e| e.to_string())?;

    // Map devid → disk name via probe_pool's devices (from btrfs filesystem show,
    // which reports stable /dev/mapper/braid-* paths). btrfs device usage may
    // report raw /dev/dm-N paths that don't match config disk names.
    let devid_to_name: HashMap<u64, &str> = domain
        .devices
        .iter()
        .filter_map(|d| {
            d.mapper
                .0
                .strip_prefix("braid-")
                .map(|name| (d.devid, name))
        })
        .collect();

    let mut disk_usage = HashMap::new();
    for entry in &dev_usage.devices {
        let disk_name = match devid_to_name.get(&entry.devid) {
            Some(name) => *name,
            None => continue,
        };
        let data: u64 = entry
            .allocations
            .iter()
            .filter(|a| a.alloc_type == "Data")
            .map(|a| a.bytes)
            .sum();
        let metadata: u64 = entry
            .allocations
            .iter()
            .filter(|a| a.alloc_type == "Metadata")
            .map(|a| a.bytes)
            .sum();
        disk_usage.insert(
            disk_name.to_owned(),
            DiskUsage {
                size: entry.device_size,
                data,
                metadata,
            },
        );
    }

    let scrub = runner
        .run(&CmdRequest::BtrfsScrubStatus {
            mount_point: MountPoint(mount_point.to_owned()),
        })
        .ok()
        .and_then(|raw| parse_btrfs_scrub_status(&raw).ok())
        .map(|out| out.state)
        .unwrap_or(ScrubState::Unknown);

    let mut smart_health = HashMap::new();
    let mut power_state = HashMap::new();
    let mut luks_info = HashMap::new();
    for (disk_name, by_id_path) in disk_by_id {
        let health = runner
            .run(&CmdRequest::SmartctlHealthJson {
                device: by_id_path.clone(),
            })
            .map(|raw| parse_smartctl_health(&raw))
            .unwrap_or(SmartHealth::Unknown);
        smart_health.insert(disk_name.clone(), health);

        if let Ok(state) = check_power_mode(by_id_path) {
            power_state.insert(disk_name.clone(), state);
        }

        if let Ok(raw) = runner.run(&CmdRequest::CryptsetupLuksDump {
            device: by_id_path.clone(),
        }) {
            if let Ok(dump) = parse_cryptsetup_luks_dump(&raw) {
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
    }

    // Extract transport type (sata, nvme, usb, etc.) from lsblk tree.
    // Walk parent devices: for each child named "braid-{name}", take the
    // parent's TRAN value. TRAN is only set on physical devices, not dm-crypt.
    let mut disk_transport = HashMap::new();
    if let Ok(lsblk_raw) = runner.run(&CmdRequest::LsblkJson) {
        if let Ok(lsblk) = parse_lsblk_json(&lsblk_raw) {
            for dev in &lsblk.blockdevices {
                if let Some(tran) = &dev.tran {
                    for child in &dev.children {
                        if let Some(name) = child.name.strip_prefix("braid-") {
                            disk_transport.insert(name.to_owned(), tran.clone());
                        }
                    }
                }
            }
        }
    }

    Ok(Some(PoolState {
        mount_point: MountPoint(mount_point.to_owned()),
        profile: profile.to_owned(),
        used: usage.used_bytes,
        total: usage.free_estimated_bytes + usage.used_bytes,
        disk_usage,
        disk_transport,
        smart_health,
        power_state,
        luks_info,
        scrub,
        probed_at: Instant::now(),
    }))
}
