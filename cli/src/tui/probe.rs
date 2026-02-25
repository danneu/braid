use std::collections::HashMap;

use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::{parse_btrfs_device_usage, parse_btrfs_filesystem_usage};
use crate::probe::probe_pool;
use crate::tui::model::{DiskUsage, PoolState};

const BRAID_MAPPER_PREFIX: &str = "/dev/mapper/braid-";

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

    let mut disk_usage = HashMap::new();
    for entry in &dev_usage.devices {
        let disk_key = entry
            .path
            .strip_prefix(BRAID_MAPPER_PREFIX)
            .unwrap_or(&entry.path);
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

    Ok(Some(PoolState {
        mount_point: mount_point.to_owned(),
        profile: profile.to_owned(),
        health: "healthy".to_owned(),
        used: usage.used_bytes,
        total: usage.free_estimated_bytes + usage.used_bytes,
        disk_usage,
    }))
}
