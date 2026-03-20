use std::collections::BTreeMap;
use std::path::Path;

use crate::alert::{self, save_acked_stats, snapshot_current};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::journal;
use crate::parse::parse_btrfs_device_stats;
use crate::probe::{probe_pool, ProbeError};
use crate::types::MountPoint;

pub fn cmd_ack<R: CommandRunner>(runner: &R, mount_point: &str) -> Result<(), AckError> {
    // 1. Read latch for count (authoritative alert state)
    let latch = alert::load_alert_latch();
    let latch_count = latch.as_ref().map_or(0, |s| s.causes.len());

    // 2. Check if pool is mounted
    let pool = match probe_pool(runner, mount_point) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return ack_offline(latch_count);
        }
        Err(e) => return Err(AckError::Probe(e)),
    };

    if !pool.mounted {
        return ack_offline(latch_count);
    }

    // 3. Run btrfs device stats
    let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStats {
        mount_point: MountPoint(mount_point.to_owned()),
    })?;
    let device_stats = parse_btrfs_device_stats(&stats_raw)?;

    // 4. Get missing devids
    let missing_devids = &pool.missing_devids;

    // 5. Build devid map
    let path_to_devid: BTreeMap<String, u64> = pool
        .devices
        .iter()
        .map(|d| (format!("/dev/mapper/{}", d.mapper.0), d.devid))
        .collect();

    // 6. Snapshot current state
    let new_acked = snapshot_current(&device_stats, missing_devids, &path_to_devid)?;
    save_acked_stats(&new_acked)?;

    // 7. Advance journal cursor to now
    let cursor_path = Path::new(journal::CURSOR_FILE);
    if let Err(e) = journal::advance_cursor_to_now(cursor_path) {
        eprintln!("Warning: failed to advance journal cursor: {e}");
    }

    // 8. Remove smartd alert flag + alert latch
    alert::remove_smartd_alert_flag()?;
    alert::remove_alert_latch()?;

    // 9. Stop beeper (best-effort)
    stop_beeper();

    // 10. Print confirmation using latch count
    if latch_count > 0 {
        println!("acknowledged {latch_count} alert(s)");
    } else {
        println!("no active alerts");
    }

    Ok(())
}

fn ack_offline(latch_count: usize) -> Result<(), AckError> {
    let smartd_active = alert::smartd_alert_active();

    let has_alert = latch_count > 0 || smartd_active;
    if !has_alert {
        return Err(AckError::PoolNotMounted);
    }

    // Advance journal cursor to now
    let cursor_path = Path::new(journal::CURSOR_FILE);
    if let Err(e) = journal::advance_cursor_to_now(cursor_path) {
        eprintln!("Warning: failed to advance journal cursor: {e}");
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
