use std::collections::BTreeMap;

use crate::alert::{
    self, compute_alert_state_with_devid_map, load_acked_stats, save_acked_stats, AlertState,
};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_btrfs_device_stats;
use crate::probe::{probe_pool, ProbeError};
use crate::types::MountPoint;

#[derive(Debug, PartialEq, Eq)]
pub enum MonitorResult {
    PoolOffline,
    Ok,
    Alert(AlertState),
}

pub fn cmd_monitor<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
) -> Result<MonitorResult, MonitorError> {
    // 1. Check if pool is mounted
    let pool = match probe_pool(runner, mount_point) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => return Ok(MonitorResult::PoolOffline),
        Err(e) => return Err(MonitorError::Probe(e)),
    };

    if !pool.mounted {
        return Ok(MonitorResult::PoolOffline);
    }

    // 2. Run btrfs device stats
    let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStats {
        mount_point: MountPoint(mount_point.to_owned()),
    })?;
    let device_stats = parse_btrfs_device_stats(&stats_raw)?;

    // 3. Load acked stats
    let mut acked = load_acked_stats();

    // 4. Get missing devids from pool probe
    let missing_devids = &pool.missing_devids;

    // 5. Check smartd alert flag
    let smartd_active = alert::smartd_alert_active();

    // 6. Build devid map from pool devices
    let path_to_devid: BTreeMap<String, u64> = pool
        .devices
        .iter()
        .map(|d| (format!("/dev/mapper/{}", d.mapper.0), d.devid))
        .collect();

    // 7. Self-heal stale ack state: if a devid was missing_acked but is now
    //    present, reset missing_acked to false
    let mut ack_changed = false;
    let present_devids: Vec<u64> = pool.devices.iter().map(|d| d.devid).collect();
    for (key, disk) in acked.0.iter_mut() {
        if disk.missing_acked {
            if let Ok(devid) = key.parse::<u64>() {
                if present_devids.contains(&devid) {
                    disk.missing_acked = false;
                    ack_changed = true;
                }
            }
        }
    }
    if ack_changed {
        if let Err(e) = save_acked_stats(&acked) {
            eprintln!("Warning: failed to update acked stats: {e}");
        }
    }

    // 8. Compute alert state
    let alert_state = compute_alert_state_with_devid_map(
        &device_stats,
        &acked,
        missing_devids,
        smartd_active,
        &path_to_devid,
    );

    if alert_state.active {
        Ok(MonitorResult::Alert(alert_state))
    } else {
        Ok(MonitorResult::Ok)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MonitorError {
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] crate::parse::ParseError),
}
