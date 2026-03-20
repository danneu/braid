use std::collections::BTreeMap;
use std::path::Path;

use crate::alert::{
    self, compute_alert_state_with_devid_map, load_acked_stats, merge_into_latch, save_acked_stats,
    AlertCause, AlertState,
};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::journal;
use crate::parse::parse_btrfs_device_stats;
use crate::probe::{probe_pool, ProbeError};
use crate::types::MountPoint;

#[derive(Debug, PartialEq, Eq)]
pub enum MonitorResult {
    PoolOffline,
    PoolOfflineJournalAlert(AlertState),
    Ok,
    Alert(AlertState),
}

pub fn cmd_monitor<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
) -> Result<MonitorResult, MonitorError> {
    let cursor_path = Path::new(journal::CURSOR_FILE);

    // 1. Scan kernel journal (BEFORE pool check)
    let journal_result = journal::check_journal(cursor_path);

    // 2. Check if pool is mounted
    let pool = match probe_pool(runner, mount_point) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return handle_pool_offline(&journal_result, cursor_path);
        }
        Err(e) => return Err(MonitorError::Probe(e)),
    };

    if !pool.mounted {
        return handle_pool_offline(&journal_result, cursor_path);
    }

    // 3. Run btrfs device stats
    let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStats {
        mount_point: MountPoint(mount_point.to_owned()),
    })?;
    let device_stats = parse_btrfs_device_stats(&stats_raw)?;

    // 4. Load acked stats
    let mut acked = load_acked_stats();

    // 5. Get missing devids from pool probe
    let missing_devids = &pool.missing_devids;

    // 6. Check smartd alert flag
    let smartd_active = alert::smartd_alert_active();

    // 7. Build devid map from pool devices
    let path_to_devid: BTreeMap<String, u64> = pool
        .devices
        .iter()
        .map(|d| (format!("/dev/mapper/{}", d.mapper.0), d.devid))
        .collect();

    // 8. Self-heal stale ack state: if a devid was missing_acked but is now
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

    // 9. Compute live alert state
    let live_causes = match compute_alert_state_with_devid_map(
        &device_stats,
        &acked,
        missing_devids,
        smartd_active,
        &path_to_devid,
    ) {
        Ok(state) => state.causes,
        Err(e) => {
            eprintln!("error: {e}");
            // Fail closed: merge ComputationError + journal causes into latch
            let error_causes = vec![AlertCause::ComputationError {
                detail: e.to_string(),
            }];
            let existing_latch = alert::load_alert_latch();
            let merged = merge_into_latch(
                existing_latch.as_ref(),
                &error_causes,
                &journal_result.causes,
            );
            if let Err(write_err) = alert::save_alert_latch(&merged) {
                eprintln!("Warning: failed to write alert latch: {write_err}");
            }
            // Save cursor after latch write succeeds
            if let Some(ref cursor) = journal_result.new_cursor {
                if let Err(ce) = journal::save_cursor(cursor_path, cursor) {
                    eprintln!("Warning: failed to save journal cursor: {ce}");
                }
            }
            return Err(MonitorError::UnmappedDevice(e));
        }
    };

    // 10. Load existing latch
    let existing_latch = alert::load_alert_latch();

    // 11. Merge: existing latch + live causes + journal causes
    let merged = merge_into_latch(
        existing_latch.as_ref(),
        &live_causes,
        &journal_result.causes,
    );

    // 12. If merged state active → write latch
    if merged.active {
        if let Err(e) = alert::save_alert_latch(&merged) {
            eprintln!("Warning: failed to write alert latch: {e}");
        }
    }

    // 13. Save journal cursor (ONLY after latch write succeeds)
    if let Some(ref cursor) = journal_result.new_cursor {
        if let Err(e) = journal::save_cursor(cursor_path, cursor) {
            eprintln!("Warning: failed to save journal cursor: {e}");
        }
    }

    // 14. Return result based on merged state
    if merged.active {
        Ok(MonitorResult::Alert(merged))
    } else {
        Ok(MonitorResult::Ok)
    }
}

/// Handle pool-offline case: merge journal causes into the existing latch.
fn handle_pool_offline(
    journal_result: &journal::JournalCheckResult,
    cursor_path: &Path,
) -> Result<MonitorResult, MonitorError> {
    if journal_result.causes.is_empty() {
        // No journal causes — just save cursor if any and return PoolOffline
        if let Some(ref cursor) = journal_result.new_cursor {
            if let Err(e) = journal::save_cursor(cursor_path, cursor) {
                eprintln!("Warning: failed to save journal cursor: {e}");
            }
        }
        return Ok(MonitorResult::PoolOffline);
    }

    let existing_latch = alert::load_alert_latch();
    let merged = merge_into_latch(existing_latch.as_ref(), &[], &journal_result.causes);

    if merged.active {
        if let Err(e) = alert::save_alert_latch(&merged) {
            eprintln!("Warning: failed to write alert latch: {e}");
        }
        // Save cursor after latch write succeeds
        if let Some(ref cursor) = journal_result.new_cursor {
            if let Err(e) = journal::save_cursor(cursor_path, cursor) {
                eprintln!("Warning: failed to save journal cursor: {e}");
            }
        }
        Ok(MonitorResult::PoolOfflineJournalAlert(merged))
    } else {
        if let Some(ref cursor) = journal_result.new_cursor {
            if let Err(e) = journal::save_cursor(cursor_path, cursor) {
                eprintln!("Warning: failed to save journal cursor: {e}");
            }
        }
        Ok(MonitorResult::PoolOffline)
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
