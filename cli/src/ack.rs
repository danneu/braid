use std::collections::{BTreeMap, BTreeSet};

use crate::alert::{self, save_acked_stats, snapshot_current};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_btrfs_device_stats;
use crate::probe::{probe_pool, ProbeError};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;

pub fn cmd_ack<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> Result<(), AckError> {
    // 1. Read latch for count (authoritative alert state)
    let latch = alert::load_alert_latch(paths);
    let latch_count = latch.as_ref().map_or(0, |s| s.causes.len());

    // 2. Check if pool is mounted
    let pool = match probe_pool(runner, mount_point) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return ack_offline(latch_count, paths);
        }
        Err(e) => return Err(AckError::Probe(e)),
    };

    if !pool.mounted {
        return ack_offline(latch_count, paths);
    }

    // 3. Run btrfs device stats
    let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStatsJson {
        mount_point: mount_point.clone(),
    })?;
    let device_stats = parse_btrfs_device_stats(&stats_raw)?;

    // 4. Compute alert-local missing devids: btrfs MISSING ∪ null-underlying
    let alert_missing_devids: Vec<u64> = pool
        .missing_devids
        .iter()
        .copied()
        .chain(pool.null_underlying.iter().map(|d| d.devid))
        .collect::<BTreeSet<u64>>()
        .into_iter()
        .collect();

    // 5. Build devid map from pool devices + null-underlying devices
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

    // 6. Snapshot current state
    let new_acked = snapshot_current(&device_stats, &alert_missing_devids, &path_to_devid)?;
    save_acked_stats(&new_acked, paths)?;

    // 7. Remove smartd alert flag + alert latch
    alert::remove_smartd_alert_flag(paths)?;
    alert::remove_alert_latch(paths)?;

    // 8. Stop beeper (best-effort)
    stop_beeper();

    // 9. Print confirmation using latch count
    if latch_count > 0 {
        println!("acknowledged {latch_count} alert(s)");
    } else {
        println!("no active alerts");
    }

    Ok(())
}

fn ack_offline(latch_count: usize, paths: &StatePaths) -> Result<(), AckError> {
    let smartd_active = alert::smartd_alert_active(paths);

    let has_alert = latch_count > 0 || smartd_active;
    if !has_alert {
        return Err(AckError::PoolNotMounted);
    }

    alert::remove_alert_latch(paths)?;
    alert::remove_smartd_alert_flag(paths)?;
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
    #[error("pool is not mounted -- nothing to acknowledge")]
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
