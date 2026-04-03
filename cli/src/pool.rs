use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
use crate::probe::probe_pool;
use crate::progress::{run_replace_with_progress, run_with_progress, ProgressOutput};
use crate::types::MountPoint;

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
        mount_point: MountPoint(mount_point.to_owned()),
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

/// Build a `PoolError::Failed` for a balance command, detecting ENOSPC and
/// appending a recovery hint when present.
fn balance_error(label: &str, mount_point: &str, result: &RawCommandOutput) -> PoolError {
    let stderr = result.stderr.to_lowercase();
    if stderr.contains("no space left") {
        PoolError::Failed(format!(
            "{label} failed (exit {}): {}\nhint: run `btrfs balance start -dusage=0 {mount_point}` to free empty block groups, then retry",
            result.exit_status,
            result.stderr.trim(),
        ))
    } else {
        PoolError::Failed(format!(
            "{label} failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim(),
        ))
    }
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
            mount_point: MountPoint(mount_point.to_owned()),
        },
        mount_point,
        progress,
    )?;
    if result.exit_status != 0 {
        return Err(balance_error(
            "btrfs balance to RAID1",
            mount_point,
            &result,
        ));
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
            mount_point: MountPoint(mount_point.to_owned()),
        },
        mount_point,
        progress,
    )?;
    if result.exit_status != 0 {
        return Err(balance_error(
            "btrfs balance to single",
            mount_point,
            &result,
        ));
    }
    Ok(())
}

/// Balance pool to RAID1 (soft: only non-RAID1 chunks) with progress display.
/// Used after degraded operations to restore redundancy for single-profile chunks.
pub fn pool_balance_raid1_soft<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &str,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    let result = run_with_progress(
        runner,
        &CmdRequest::BtrfsBalanceRaid1Soft {
            mount_point: MountPoint(mount_point.to_owned()),
        },
        mount_point,
        progress,
    )?;
    if result.exit_status != 0 {
        return Err(balance_error(
            "btrfs soft balance to RAID1",
            mount_point,
            &result,
        ));
    }
    Ok(())
}

/// Run a soft RAID1 rebalance if the operation just transitioned the pool from
/// degraded to non-degraded with ≥2 present devices. This restores redundancy
/// for single-profile chunks created during degraded operation (known btrfs bug).
///
/// Callers: `remove-missing` and `replace` (missing path), after their primary
/// operation and pool.json update have completed.
pub fn maybe_restore_raid1<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &str,
    pre_op_missing_count: u64,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    if pre_op_missing_count == 0 {
        return Ok(()); // Pool wasn't degraded — nothing to restore
    }
    let pool_after = probe_pool(runner, mount_point)
        .map_err(|e| PoolError::Failed(format!("post-operation pool probe failed: {e}")))?;
    if pool_after.missing_count == 0 && pool_after.devices.len() >= 2 {
        eprintln!("Restoring RAID1 redundancy (soft balance)...");
        pool_balance_raid1_soft(runner, mount_point, progress)?;
        eprintln!("Soft balance complete.");
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
            mount_point: MountPoint(mount_point.to_owned()),
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

/// Remove a specific device by devid from the pool.
pub fn pool_remove_devid<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &str,
    devid: u64,
) -> Result<(), PoolError> {
    let result = runner.run(&CmdRequest::BtrfsDeviceRemove {
        device: devid.to_string(),
        mount_point: MountPoint(mount_point.to_owned()),
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
            mount_point: MountPoint(mount_point.to_owned()),
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
        mount_point: MountPoint(mount_point.to_owned()),
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
#[allow(clippy::doc_overindented_list_items)]
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
        mount_point: MountPoint(mount_point.to_owned()),
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
        mount_point: MountPoint(mount_point.to_owned()),
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

    use crate::cmd::CmdError;
    use std::sync::{Arc, Mutex};

    /// Recording runner that models a post-operation pool state for
    /// maybe_restore_raid1 tests.
    #[derive(Clone)]
    struct RestoreRunner {
        log: Arc<Mutex<Vec<CmdRequest>>>,
        /// Number of missing devices in the post-op pool probe
        post_missing_count: u64,
        /// Number of present devices in the post-op pool probe
        post_present_count: usize,
        /// If true, make the post-op probe fail
        probe_fails: bool,
    }

    impl RestoreRunner {
        fn new(post_missing: u64, post_present: usize) -> Self {
            Self {
                log: Arc::new(Mutex::new(Vec::new())),
                post_missing_count: post_missing,
                post_present_count: post_present,
                probe_fails: false,
            }
        }

        fn failing_probe() -> Self {
            Self {
                log: Arc::new(Mutex::new(Vec::new())),
                post_missing_count: 0,
                post_present_count: 0,
                probe_fails: true,
            }
        }

        fn calls(&self) -> Vec<CmdRequest> {
            self.log.lock().unwrap().clone()
        }
    }

    impl CommandRunner for RestoreRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());

            match request {
                CmdRequest::FindmntJson { mount_point } => {
                    if self.probe_fails {
                        return Ok(RawCommandOutput {
                            cmd: String::new(),
                            stdout: "{}".to_owned(),
                            stderr: "not found".to_owned(),
                            exit_status: 1,
                        });
                    }
                    Ok(RawCommandOutput {
                        cmd: String::new(),
                        stdout: format!(
                            r#"{{"filesystems":[{{"target":"{mount_point}","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}}]}}"#
                        ),
                        stderr: String::new(),
                        exit_status: 0,
                    })
                }
                CmdRequest::BtrfsFilesystemShow { .. } => {
                    let mut lines =
                        String::from("Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n");
                    let total = self.post_present_count as u64 + self.post_missing_count;
                    lines.push_str(&format!("\tTotal devices {total} FS bytes used 16.17MiB\n"));
                    for i in 0..self.post_present_count {
                        lines.push_str(&format!(
                            "\tdevid    {} size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk{}\n",
                            i + 1, i + 1
                        ));
                    }
                    if self.post_missing_count > 0 {
                        lines.push_str("\t*** Some devices missing\n");
                    }
                    Ok(RawCommandOutput {
                        cmd: String::new(),
                        stdout: lines,
                        stderr: String::new(),
                        exit_status: 0,
                    })
                }
                CmdRequest::CryptsetupStatus { mapper } => Ok(RawCommandOutput {
                    cmd: String::new(),
                    stdout: format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                    ),
                    stderr: String::new(),
                    exit_status: 0,
                }),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(RawCommandOutput {
                    cmd: String::new(),
                    stdout: "11111111-1111-1111-1111-111111111111\n".to_owned(),
                    stderr: String::new(),
                    exit_status: 0,
                }),
                CmdRequest::BtrfsBalanceRaid1Soft { .. } => Ok(ok_raw()),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(RawCommandOutput {
                    cmd: String::new(),
                    stdout: "No balance found on '/mnt/storage'\n".to_owned(),
                    stderr: String::new(),
                    exit_status: 0,
                }),
                _ => Err(CmdError::MissingMock),
            }
        }

        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            self.run(request)
        }
    }

    #[test]
    // Intent: maybe_restore_raid1 is a no-op when pre_op_missing_count == 0.
    // Why: if the pool wasn't degraded before the operation, there are no
    // single-profile chunks to fix.
    // Scenario: operator runs remove-missing on a pool that wasn't degraded
    // (e.g., already cleaned up).
    fn maybe_restore_raid1_noop_when_not_degraded() {
        let runner = RestoreRunner::failing_probe(); // should never be called
        let result = maybe_restore_raid1(&runner, "/mnt/storage", 0, ProgressOutput::Off);
        assert!(result.is_ok(), "should be no-op: {result:?}");
        assert!(
            runner.calls().is_empty(),
            "should not call any commands when pre_op_missing_count == 0"
        );
    }

    #[test]
    // Intent: maybe_restore_raid1 runs soft balance when post-op is healthy
    // with ≥2 devices.
    // Why: this is the core case — clearing the last missing device should
    // restore redundancy for single-profile chunks.
    // Scenario: 3-disk pool had 1 missing, remove-missing clears it, 2 remain.
    fn maybe_restore_raid1_runs_soft_balance() {
        let runner = RestoreRunner::new(0, 2); // post-op: 0 missing, 2 present
        let result = maybe_restore_raid1(&runner, "/mnt/storage", 1, ProgressOutput::Off);
        assert!(result.is_ok(), "should succeed: {result:?}");
        assert!(
            runner
                .calls()
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "should call BtrfsBalanceRaid1Soft; calls: {:?}",
            runner.calls()
        );
    }

    #[test]
    // Intent: maybe_restore_raid1 does NOT run balance when post-op still has
    // missing devices.
    // Why: running a balance while still degraded would be pointless and could
    // make things worse.
    // Scenario: 4-disk pool had 2 missing, operator removes 1, 1 still missing.
    fn maybe_restore_raid1_skips_when_still_degraded() {
        let runner = RestoreRunner::new(1, 2); // post-op: 1 still missing
        let result = maybe_restore_raid1(&runner, "/mnt/storage", 2, ProgressOutput::Off);
        assert!(result.is_ok(), "should succeed: {result:?}");
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "should NOT call BtrfsBalanceRaid1Soft when still degraded"
        );
    }

    #[test]
    // Intent: maybe_restore_raid1 does NOT run balance when only 1 device remains.
    // Why: can't have RAID1 with 1 device.
    // Scenario: 2-disk pool, 1 missing, remove-missing leaves 1 survivor.
    fn maybe_restore_raid1_skips_single_device() {
        let runner = RestoreRunner::new(0, 1); // post-op: 0 missing, 1 present
        let result = maybe_restore_raid1(&runner, "/mnt/storage", 1, ProgressOutput::Off);
        assert!(result.is_ok(), "should succeed: {result:?}");
        assert!(
            !runner
                .calls()
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "should NOT call BtrfsBalanceRaid1Soft with only 1 device"
        );
    }

    #[test]
    // Intent: maybe_restore_raid1 propagates probe failure as an error.
    // Why: if we can't determine pool state, we should fail rather than silently
    // skip — the caller can decide how to handle it.
    // Scenario: btrfs filesystem show fails after remove-missing.
    fn maybe_restore_raid1_propagates_probe_failure() {
        let runner = RestoreRunner::failing_probe();
        let result = maybe_restore_raid1(&runner, "/mnt/storage", 1, ProgressOutput::Off);
        assert!(result.is_err(), "should propagate probe failure");
        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("post-operation pool probe failed"),
            "error should mention probe: {err}"
        );
    }

    #[test]
    // Intent: balance_error appends a recovery hint when stderr contains ENOSPC.
    // Why: ENOSPC is a common balance failure with a well-known recovery
    //   (dusage=0). Without a hint, users must search for the fix.
    // Scenario: pool is near-full, balance fails with "No space left on device".
    fn balance_error_detects_enospc() {
        let result = RawCommandOutput {
            cmd: String::new(),
            stdout: String::new(),
            stderr: "ERROR: error during balancing '/mnt/storage': No space left on device"
                .to_owned(),
            exit_status: 1,
        };
        let err = balance_error("btrfs balance to RAID1", "/mnt/storage", &result);
        let msg = err.to_string();
        assert!(msg.contains("hint:"), "should contain recovery hint: {msg}");
        assert!(
            msg.contains("dusage=0"),
            "should suggest dusage=0 filter: {msg}"
        );
        assert!(
            msg.contains("/mnt/storage"),
            "should include concrete mount point: {msg}"
        );
    }

    #[test]
    // Intent: balance_error returns a plain error (no hint) for non-ENOSPC failures.
    // Why: the hint should only appear for ENOSPC, not for unrelated errors.
    // Scenario: balance fails because the filesystem is read-only.
    fn balance_error_no_hint_for_other_failures() {
        let result = RawCommandOutput {
            cmd: String::new(),
            stdout: String::new(),
            stderr: "ERROR: error during balancing '/mnt/storage': Read-only file system"
                .to_owned(),
            exit_status: 1,
        };
        let err = balance_error("btrfs balance to RAID1", "/mnt/storage", &result);
        let msg = err.to_string();
        assert!(
            !msg.contains("hint:"),
            "should NOT contain recovery hint for non-ENOSPC: {msg}"
        );
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
                mount_point: MountPoint("/mnt/storage".to_owned()),
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
                mount_point: MountPoint("/mnt/storage".to_owned()),
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
