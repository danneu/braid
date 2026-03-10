use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::probe::probe_pool;
use crate::progress::{run_replace_with_progress, run_with_progress, ProgressOutput};

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Failed(String),
}

/// Add a device to an existing btrfs pool.
pub fn pool_add_device<R: CommandRunner + Sync>(
    runner: &R,
    device: &str,
    mount_point: &str,
) -> Result<(), PoolError> {
    let result = runner.run(&CmdRequest::BtrfsDeviceAdd {
        device: device.to_owned(),
        mount_point: mount_point.to_owned(),
    })?;
    if result.exit_status != 0 {
        return Err(PoolError::Failed(format!(
            "btrfs device add failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim()
        )));
    }
    Ok(())
}

/// Balance pool to RAID1 with progress display.
pub fn pool_balance_raid1<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &str,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    let result = run_with_progress(
        runner,
        &CmdRequest::BtrfsBalanceRaid1 {
            mount_point: mount_point.to_owned(),
        },
        mount_point,
        progress,
    )?;
    if result.exit_status != 0 {
        return Err(PoolError::Failed(format!(
            "btrfs balance to RAID1 failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim()
        )));
    }
    Ok(())
}

/// Balance pool to single profile (pre-remove conversion) with progress.
pub fn pool_balance_single<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &str,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    let result = run_with_progress(
        runner,
        &CmdRequest::BtrfsBalanceSingle {
            mount_point: mount_point.to_owned(),
        },
        mount_point,
        progress,
    )?;
    if result.exit_status != 0 {
        return Err(PoolError::Failed(format!(
            "btrfs balance to single failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim()
        )));
    }
    Ok(())
}

/// Gracefully remove a specific device from the pool with progress.
pub fn pool_remove_device<R: CommandRunner + Sync>(
    runner: &R,
    device: &str,
    mount_point: &str,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    let result = run_with_progress(
        runner,
        &CmdRequest::BtrfsDeviceRemove {
            device: device.to_owned(),
            mount_point: mount_point.to_owned(),
        },
        mount_point,
        progress,
    )?;
    if result.exit_status != 0 {
        return Err(PoolError::Failed(format!(
            "btrfs device remove failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim()
        )));
    }
    Ok(())
}

/// Remove all missing devices from the pool.
pub fn pool_remove_missing<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &str,
) -> Result<(), PoolError> {
    let result = runner.run(&CmdRequest::BtrfsDeviceRemoveMissing {
        mount_point: mount_point.to_owned(),
    })?;
    if result.exit_status != 0 {
        return Err(PoolError::Failed(format!(
            "btrfs device remove missing failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim()
        )));
    }
    Ok(())
}

/// Remove a specific device by devid from the pool.
pub fn pool_remove_devid<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &str,
    devid: u64,
) -> Result<(), PoolError> {
    let result = runner.run(&CmdRequest::BtrfsDeviceRemove {
        device: devid.to_string(),
        mount_point: mount_point.to_owned(),
    })?;
    if result.exit_status != 0 {
        return Err(PoolError::Failed(format!(
            "btrfs device remove devid {devid} failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim()
        )));
    }
    Ok(())
}

/// Replace a device in the pool using `btrfs replace start` with progress display.
pub fn pool_replace_device<R: CommandRunner + Sync>(
    runner: &R,
    devid: u64,
    target_device: &str,
    mount_point: &str,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    let result = run_replace_with_progress(
        runner,
        &CmdRequest::BtrfsReplaceStart {
            devid,
            target_device: target_device.to_owned(),
            mount_point: mount_point.to_owned(),
        },
        mount_point,
        progress,
    )?;
    if result.exit_status != 0 {
        return Err(PoolError::Failed(format!(
            "btrfs replace failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim()
        )));
    }
    Ok(())
}

/// Resize a device in the pool to its maximum capacity.
pub fn pool_resize_device<R: CommandRunner + Sync>(
    runner: &R,
    devid: u64,
    mount_point: &str,
) -> Result<(), PoolError> {
    let result = runner.run(&CmdRequest::BtrfsFilesystemResize {
        devid,
        mount_point: mount_point.to_owned(),
    })?;
    if result.exit_status != 0 {
        return Err(PoolError::Failed(format!(
            "btrfs filesystem resize failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim()
        )));
    }
    Ok(())
}

/// Result of evicting a live device from the pool.
#[derive(Debug, PartialEq, Eq)]
pub enum EvictResult {
    /// Device was removed from pool and LUKS mapper closed.
    Removed,
    /// Device mapper was already absent — nothing to do.
    AlreadyAbsent,
}

/// Shared helper: evict a live (present) device from the pool.
///
/// 1. Probes the current pool to decide if RAID1→single conversion is needed.
/// 2. If only one device would remain, balances to single first.
///    Note: if a device is faulty, we do not want it to participate in the balance.
///          Rather, it should just be removed. But we don't have that info.
/// 3. Removes the target device from the pool.
/// 4. Closes the LUKS mapper (best-effort; warns on failure).
///
/// Idempotent: if target mapper is already absent from the pool, returns
/// `EvictResult::AlreadyAbsent`.
pub fn evict_present_device<R: CommandRunner + Sync>(
    runner: &R,
    mapper: &str,
    mount_point: &str,
    progress: ProgressOutput,
) -> Result<EvictResult, PoolError> {
    let pool = probe_pool(runner, mount_point).map_err(|e| PoolError::Failed(e.to_string()))?;

    let device_path = format!("/dev/mapper/{mapper}");
    let in_pool = pool.devices.iter().any(|d| d.mapper.0 == mapper);

    if !in_pool {
        return Ok(EvictResult::AlreadyAbsent);
    }

    let remaining = pool.devices.len() - 1;
    if remaining == 1 {
        eprintln!("Converting pool from RAID1 to single profile...");
        pool_balance_single(runner, mount_point, progress)?;
    }

    eprintln!("Removing {} from pool (data will migrate)...", mapper);
    pool_remove_device(runner, &device_path, mount_point, progress)?;

    // Best-effort LUKS close — warn on failure, don't fail the command.
    let result = runner.run(&CmdRequest::CryptsetupClose {
        mapper: mapper.to_owned(),
    });
    match result {
        Ok(r) if r.exit_status != 0 => {
            eprintln!(
                "Warning: failed to close LUKS mapper {} (exit {})",
                mapper, r.exit_status
            );
        }
        Err(e) => {
            eprintln!("Warning: failed to close LUKS mapper {}: {}", mapper, e);
        }
        _ => {}
    }

    Ok(EvictResult::Removed)
}

/// Bootstrap the pool: mkfs.btrfs (only if no superblock) then mount.
pub fn pool_bootstrap_mount<R: CommandRunner + Sync>(
    runner: &R,
    device: &str,
    mount_point: &str,
) -> Result<(), PoolError> {
    // Check for existing btrfs superblock
    let scan = runner.run(&CmdRequest::BtrfsDeviceScan {
        device: device.to_owned(),
    })?;
    if scan.exit_status != 0 {
        // No superblock — create filesystem
        let mkfs = runner.run(&CmdRequest::MkfsBtrfs {
            device: device.to_owned(),
        })?;
        if mkfs.exit_status != 0 {
            return Err(PoolError::Failed(format!(
                "mkfs.btrfs failed (exit {}): {}",
                mkfs.exit_status,
                mkfs.stderr.trim()
            )));
        }
    }

    // Create mount point directory if needed
    let _ = std::fs::create_dir_all(mount_point);

    let mount = runner.run(&CmdRequest::Mount {
        device: device.to_owned(),
        mount_point: mount_point.to_owned(),
    })?;
    if mount.exit_status != 0 {
        return Err(PoolError::Failed(format!(
            "mount failed (exit {}): {}",
            mount.exit_status,
            mount.stderr.trim()
        )));
    }
    Ok(())
}

/// Bootstrap the pool with multiple devices in RAID1: mkfs.btrfs -d raid1 -m raid1, then mount.
/// Callers must verify all devices are fresh (no btrfs superblock) before calling this.
pub fn pool_bootstrap_mount_raid1<R: CommandRunner + Sync>(
    runner: &R,
    devices: &[String],
    mount_point: &str,
) -> Result<(), PoolError> {
    let mkfs = runner.run(&CmdRequest::MkfsBtrfsRaid1 {
        devices: devices.to_vec(),
    })?;
    if mkfs.exit_status != 0 {
        return Err(PoolError::Failed(format!(
            "mkfs.btrfs RAID1 failed (exit {}): {}",
            mkfs.exit_status,
            mkfs.stderr.trim()
        )));
    }

    let _ = std::fs::create_dir_all(mount_point);

    let mount = runner.run(&CmdRequest::Mount {
        device: devices[0].clone(),
        mount_point: mount_point.to_owned(),
    })?;
    if mount.exit_status != 0 {
        return Err(PoolError::Failed(format!(
            "mount failed (exit {}): {}",
            mount.exit_status,
            mount.stderr.trim()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::progress::ProgressOutput;

    fn ok_raw() -> RawCommandOutput {
        RawCommandOutput {
            cmd: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    #[test]
    // Intent: pool_replace_device must issue BtrfsReplaceStart with the correct
    // devid, target device, and mount point.
    // Why: the pool layer is the boundary between business logic and shell
    // commands. If the CmdRequest contract breaks, the wrong btrfs command runs.
    // The actual -r flag is enforced in CmdRequest::to_argv() (tested in
    // cmd::tests); this test locks in the CmdRequest plumbing.
    // Scenario: live replace of devid 2 with a new encrypted mapper device.
    fn pool_replace_device_issues_correct_replace_start() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsReplaceStart {
                devid: 2,
                target_device: "/dev/mapper/braid-new".to_owned(),
                mount_point: "/mnt/storage".to_owned(),
            },
            ok_raw(),
        );

        let result = pool_replace_device(
            &runner,
            2,
            "/dev/mapper/braid-new",
            "/mnt/storage",
            ProgressOutput::Off,
        );
        assert!(
            result.is_ok(),
            "pool_replace_device should succeed when mock matches: {result:?}"
        );
    }

    #[test]
    // Intent: pool_replace_device must propagate non-zero exit status as an error.
    // Why: if btrfs replace fails (e.g. ENOSPC, device busy), the error must
    // bubble up so callers can handle it.
    // Scenario: replacement fails because the target device is too small.
    fn pool_replace_device_propagates_failure() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsReplaceStart {
                devid: 2,
                target_device: "/dev/mapper/braid-new".to_owned(),
                mount_point: "/mnt/storage".to_owned(),
            },
            RawCommandOutput {
                cmd: String::new(),
                stdout: String::new(),
                stderr: "target device is too small".to_owned(),
                exit_status: 1,
            },
        );

        let result = pool_replace_device(
            &runner,
            2,
            "/dev/mapper/braid-new",
            "/mnt/storage",
            ProgressOutput::Off,
        );
        assert!(result.is_err(), "should propagate btrfs replace failure");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("target device is too small"),
            "error should include stderr: {err}"
        );
    }
}
