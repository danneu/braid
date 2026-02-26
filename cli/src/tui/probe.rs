use std::collections::HashMap;

use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::types::ScrubState;
use crate::parse::{
    parse_btrfs_device_usage, parse_btrfs_filesystem_usage, parse_btrfs_scrub_status,
};
use crate::probe::probe_pool;
use crate::tui::model::{DiskUsage, PoolState};

pub fn probe_pool_for_tui<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
) -> Result<Option<PoolState>, String> {
    let domain = probe_pool(runner, mount_point).map_err(|e| e.to_string())?;

    if !domain.mounted {
        return Ok(None);
    }

    let usage_raw = runner
        .run(&CmdRequest::BtrfsFilesystemUsageRaw {
            mount_point: mount_point.to_owned(),
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
            mount_point: mount_point.to_owned(),
        })
        .map_err(|e| e.to_string())?;
    let dev_usage = parse_btrfs_device_usage(&dev_usage_raw).map_err(|e| e.to_string())?;

    // Map devid → disk key via probe_pool's devices (from btrfs filesystem show,
    // which reports stable /dev/mapper/braid-* paths). btrfs device usage may
    // report raw /dev/dm-N paths that don't match config disk names.
    let devid_to_key: HashMap<u64, &str> = domain
        .devices
        .iter()
        .filter_map(|d| d.mapper.0.strip_prefix("braid-").map(|key| (d.devid, key)))
        .collect();

    let mut disk_usage = HashMap::new();
    for entry in &dev_usage.devices {
        let disk_key = match devid_to_key.get(&entry.devid) {
            Some(key) => *key,
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
            disk_key.to_owned(),
            DiskUsage {
                size: entry.device_size,
                data,
                metadata,
            },
        );
    }

    let scrub = runner
        .run(&CmdRequest::BtrfsScrubStatus {
            mount_point: mount_point.to_owned(),
        })
        .ok()
        .and_then(|raw| parse_btrfs_scrub_status(&raw).ok())
        .map(|out| out.state)
        .unwrap_or(ScrubState::Unknown);

    Ok(Some(PoolState {
        mount_point: mount_point.to_owned(),
        profile: profile.to_owned(),
        health: if domain.missing_count > 0 { "degraded".to_owned() } else { "healthy".to_owned() },
        used: usage.used_bytes,
        total: usage.free_estimated_bytes + usage.used_bytes,
        disk_usage,
        scrub,
    }))
}
