use std::collections::BTreeMap;

use crate::alert::{self, save_acked_stats, snapshot_current};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_btrfs_device_stats;
use crate::probe::{probe_pool, ProbeError};
use crate::types::MountPoint;

pub fn cmd_ack<R: CommandRunner>(runner: &R, mount_point: &str) -> Result<(), AckError> {
    // 1. Check if pool is mounted
    let pool = match probe_pool(runner, mount_point) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return ack_offline();
        }
        Err(e) => return Err(AckError::Probe(e)),
    };

    if !pool.mounted {
        return ack_offline();
    }

    // 2. Run btrfs device stats
    let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStats {
        mount_point: MountPoint(mount_point.to_owned()),
    })?;
    let device_stats = parse_btrfs_device_stats(&stats_raw)?;

    // 3. Get missing devids
    let missing_devids = &pool.missing_devids;

    // 4. Build devid map
    let path_to_devid: BTreeMap<String, u64> = pool
        .devices
        .iter()
        .map(|d| (format!("/dev/mapper/{}", d.mapper.0), d.devid))
        .collect();

    // Count current alerts before acking (for user message)
    let smartd_active = alert::smartd_alert_active();
    let acked = alert::load_acked_stats();
    let current_alert = alert::compute_alert_state_with_devid_map(
        &device_stats,
        &acked,
        missing_devids,
        smartd_active,
        &path_to_devid,
    )?;

    // 5. Snapshot current state
    let new_acked = snapshot_current(&device_stats, missing_devids, &path_to_devid)?;
    save_acked_stats(&new_acked)?;

    // 6. Remove smartd alert flag + alert latch
    alert::remove_smartd_alert_flag()?;
    alert::remove_alert_latch()?;

    // 7. Stop beeper (best-effort)
    stop_beeper();

    // 8. Print confirmation
    let count = current_alert.causes.len();
    if count > 0 {
        println!("acknowledged {count} alert(s)");
    } else {
        println!("no active alerts");
    }

    Ok(())
}

fn ack_offline() -> Result<(), AckError> {
    let latch = alert::load_alert_latch();
    let smartd_active = alert::smartd_alert_active();

    let has_alert = latch.as_ref().map_or(false, |s| s.active) || smartd_active;
    if !has_alert {
        return Err(AckError::PoolNotMounted);
    }

    alert::remove_alert_latch()?;
    alert::remove_smartd_alert_flag()?;
    stop_beeper();
    println!("acknowledged current alerts");
    Ok(())
}

fn stop_beeper() {
    let result = std::process::Command::new("systemctl")
        .args(["stop", "braid-alert.service"])
        .output();
    if let Err(e) = result {
        eprintln!("Warning: could not stop braid-alert.service: {e}");
    }
}

#[derive(Debug, thiserror::Error)]
pub enum AckError {
    #[error("pool is not mounted — nothing to acknowledge")]
    PoolNotMounted,
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] crate::parse::ParseError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("unmapped device: {0}")]
    UnmappedDevice(#[from] crate::alert::UnmappedDeviceError),
}
