use crate::alert::{self, save_acked_stats, snapshot_current};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_btrfs_device_stats;
use crate::probe::{Filesystem, ProbeError, probe_pool};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;

pub fn cmd_ack<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> Result<(), AckError> {
    // 1. Read latch for count (authoritative alert state). An unreadable
    //    latch counts as an active alert for gating so the user can clear
    //    a corrupt file even with the pool offline.
    let (latch_count, latch_corrupt) = match alert::load_alert_latch(paths) {
        Ok(Some(s)) => (s.causes.len(), false),
        Ok(None) => (0, false),
        Err(e) => {
            eprintln!("warning: alert latch unreadable -- acknowledging anyway: {e}");
            (0, true)
        }
    };

    // 2. Check if pool is mounted
    let pool = match probe_pool(runner, fs, mount_point) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return ack_offline(latch_count, latch_corrupt, paths);
        }
        Err(e) => return Err(AckError::Probe(e)),
    };

    if !pool.mounted {
        return ack_offline(latch_count, latch_corrupt, paths);
    }

    // 3. Run btrfs device stats
    let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStatsJson {
        mount_point: mount_point.clone(),
    })?;
    let device_stats = parse_btrfs_device_stats(&stats_raw)?;

    // 4. Compute alert-local missing devids: btrfs MISSING ∪ null-underlying
    let alert_missing_devids = pool.alert_missing_devids();

    // 5. Snapshot current state. Identity is the devid carried on each
    //    stats row by btrfs -- no path-to-devid map needed.
    let new_acked = snapshot_current(&device_stats, &alert_missing_devids);
    save_acked_stats(&new_acked, paths)?;

    // 6. Remove smartd alert flag + alert latch (+ any corrupt sidecar)
    alert::remove_smartd_alert_flag(paths)?;
    alert::remove_alert_latch(paths)?;
    alert::remove_alert_latch_corrupt(paths)?;

    // 7. Stop beeper (best-effort)
    stop_beeper();

    // 8. Print confirmation using latch count
    if latch_count > 0 {
        println!("acknowledged {latch_count} alert(s)");
    } else {
        println!("no active alerts");
    }

    Ok(())
}

fn ack_offline(
    latch_count: usize,
    latch_corrupt: bool,
    paths: &StatePaths,
) -> Result<(), AckError> {
    let smartd_active = alert::smartd_alert_active(paths);

    let has_alert = latch_count > 0 || smartd_active || latch_corrupt;
    if !has_alert {
        return Err(AckError::PoolNotMounted);
    }

    alert::remove_alert_latch(paths)?;
    alert::remove_alert_latch_corrupt(paths)?;
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
}
