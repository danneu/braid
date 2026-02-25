use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_btrfs_filesystem_usage;
use crate::probe::probe_pool;
use crate::tui::app::PoolState;

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

    Ok(Some(PoolState {
        mount_point: mount_point.to_owned(),
        profile: profile.to_owned(),
        health: "healthy".to_owned(),
        used: usage.used_bytes,
        total: usage.free_estimated_bytes + usage.used_bytes,
    }))
}
