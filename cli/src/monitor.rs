use std::collections::{BTreeMap, BTreeSet};

use crate::alert::{
    self, compute_alert_state_with_devid_map, load_acked_stats, merge_into_latch, save_acked_stats,
    AlertCause,
};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_btrfs_device_stats;
use crate::probe::{probe_pool, ProbeError};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;

#[derive(Debug, PartialEq, Eq)]
pub enum MonitorResult {
    PoolOffline,
    Ok,
    Alert(alert::AlertState),
}

pub fn cmd_monitor<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> Result<MonitorResult, MonitorError> {
    // 1. Check if pool is mounted
    let pool = match probe_pool(runner, mount_point) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return Ok(MonitorResult::PoolOffline);
        }
        Err(e) => return Err(MonitorError::Probe(e)),
    };

    if !pool.mounted {
        return Ok(MonitorResult::PoolOffline);
    }

    // 2. Run btrfs device stats
    let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStatsJson {
        mount_point: mount_point.clone(),
    })?;
    let device_stats = parse_btrfs_device_stats(&stats_raw)?;

    // 3. Load acked stats
    let mut acked = load_acked_stats(paths);

    // 4. Compute alert-local missing devids: btrfs MISSING ∪ null-underlying
    let alert_missing_devids: Vec<u64> = pool
        .missing_devids
        .iter()
        .copied()
        .chain(pool.null_underlying.iter().map(|d| d.devid))
        .collect::<BTreeSet<u64>>()
        .into_iter()
        .collect();

    // 5. Check smartd alert flag
    let smartd_active = alert::smartd_alert_active(paths);

    // 6. Build devid map from pool devices + null-underlying devices
    let path_to_devid: BTreeMap<String, u64> = pool
        .devices
        .iter()
        .map(|d| (format!("/dev/mapper/{}", d.mapper.0), d.devid))
        .chain(
            pool.null_underlying
                .iter()
                .map(|d| (format!("/dev/mapper/{}", d.mapper.0), d.devid)),
        )
        .collect();

    // 7. Self-heal stale ack state: if a devid was missing_acked but is now
    //    present, reset missing_acked to false
    let mut ack_changed = false;
    let present_devids: Vec<u64> = pool.devices.iter().map(|d| d.devid).collect();
    for (key, disk) in acked.0.iter_mut() {
        if disk.missing_acked
            && let Ok(devid) = key.parse::<u64>()
                && present_devids.contains(&devid) {
                    disk.missing_acked = false;
                    ack_changed = true;
                }
    }
    if ack_changed
        && let Err(e) = save_acked_stats(&acked, paths) {
            eprintln!("Warning: failed to update acked stats: {e}");
        }

    // 8. Compute live alert state
    let live_causes = match compute_alert_state_with_devid_map(
        &device_stats,
        &acked,
        &alert_missing_devids,
        smartd_active,
        &path_to_devid,
    ) {
        Ok(state) => state.causes,
        Err(e) => {
            eprintln!("error: {e}");
            // Fail closed: merge ComputationError into latch
            let error_causes = vec![AlertCause::ComputationError {
                detail: e.to_string(),
            }];
            let existing_latch = alert::load_alert_latch(paths);
            let merged = merge_into_latch(existing_latch.as_ref(), &error_causes);
            if let Err(write_err) = alert::save_alert_latch(&merged, paths) {
                eprintln!("Warning: failed to write alert latch: {write_err}");
            }
            return Err(MonitorError::UnmappedDevice(e));
        }
    };

    // 9. Load existing latch
    let existing_latch = alert::load_alert_latch(paths);

    // 10. Merge: existing latch + live causes
    let merged = merge_into_latch(existing_latch.as_ref(), &live_causes);

    // 11. If merged state active → write latch
    if merged.active
        && let Err(e) = alert::save_alert_latch(&merged, paths) {
            eprintln!("Warning: failed to write alert latch: {e}");
        }

    // 12. Return result based on merged state
    if merged.active {
        Ok(MonitorResult::Alert(merged))
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
    #[error("unmapped device: {0}")]
    UnmappedDevice(#[from] crate::alert::UnmappedDeviceError),
}
