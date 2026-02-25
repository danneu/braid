use crate::cmd::{CmdError, CmdRequest, CommandRunner};
use crate::progress::{run_with_progress, ProgressOutput};

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
