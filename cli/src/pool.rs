use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
use crate::probe::{Filesystem, probe_pool};
use crate::progress::{
    self, ProgressOutput, run_device_remove_with_progress, run_replace_with_progress,
    run_with_progress,
};
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
    mount_point: &MountPoint,
) -> Result<(), PoolError> {
    let result = runner.run(&CmdRequest::BtrfsDeviceAdd {
        device: device.to_owned(),
        mount_point: mount_point.clone(),
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
fn balance_error(label: &str, mount_point: &MountPoint, result: &RawCommandOutput) -> PoolError {
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
    mount_point: &MountPoint,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    let result = run_with_progress(
        runner,
        &CmdRequest::BtrfsBalanceRaid1 {
            mount_point: mount_point.clone(),
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
    mount_point: &MountPoint,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    let result = run_with_progress(
        runner,
        &CmdRequest::BtrfsBalanceSingle {
            mount_point: mount_point.clone(),
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
    mount_point: &MountPoint,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    let result = run_with_progress(
        runner,
        &CmdRequest::BtrfsBalanceRaid1Soft {
            mount_point: mount_point.clone(),
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

/// Resume a paused btrfs balance using the convert filters the kernel
/// persisted in the chunk tree's `BALANCE_ITEM`. Used by `cmd_recover` to
/// drain a balance that a forced shutdown left paused (`skip_balance` mount
/// option prevents kernel auto-resume on the post-crash mount, see
/// `cli/src/cmd.rs:271-283`). Caller must verify the balance is paused before
/// calling -- this function does not check.
pub fn pool_balance_resume<R: CommandRunner + Sync>(
    runner: &R,
    mount_point: &MountPoint,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    let result = run_with_progress(
        runner,
        &CmdRequest::BtrfsBalanceResume {
            mount_point: mount_point.clone(),
        },
        mount_point,
        progress,
    )?;
    if result.exit_status != 0 {
        return Err(balance_error("btrfs balance resume", mount_point, &result));
    }
    Ok(())
}

/// Run a soft RAID1 rebalance if the operation just transitioned the pool from
/// degraded to non-degraded with >=2 present devices. This restores redundancy
/// for single-profile chunks created during degraded operation (known btrfs bug).
///
/// Callers: `remove-missing` and `replace` (missing path), after the primary
/// btrfs op completes and before the pool.json membership write + journal clear.
/// `pre_op_missing_count` must be the missing count from the pre-op pool probe.
pub fn maybe_restore_raid1<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    pre_op_missing_count: u64,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    if pre_op_missing_count == 0 {
        return Ok(()); // Pool wasn't degraded — nothing to restore
    }
    let pool_after = probe_pool(runner, fs, mount_point)
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
    mount_point: &MountPoint,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    let result = run_device_remove_with_progress(
        runner,
        &CmdRequest::BtrfsDeviceRemove {
            device: device.to_owned(),
            mount_point: mount_point.clone(),
        },
        progress,
    )?;
    device_remove_result(result)
}

pub(crate) fn pool_remove_device_using<R, S, W>(
    runner: &R,
    device: &str,
    mount_point: &MountPoint,
    progress: ProgressOutput,
    sleeper: &S,
    sink: &W,
) -> Result<(), PoolError>
where
    R: CommandRunner + Sync,
    S: progress::Sleeper + ?Sized,
    W: progress::ProgressSink + ?Sized,
{
    let result = progress::run_device_remove_with_progress_using(
        runner,
        &CmdRequest::BtrfsDeviceRemove {
            device: device.to_owned(),
            mount_point: mount_point.clone(),
        },
        progress,
        sleeper,
        sink,
    )?;
    device_remove_result(result)
}

fn device_remove_result(result: RawCommandOutput) -> Result<(), PoolError> {
    if result.exit_status != 0 {
        return Err(PoolError::Failed(format!(
            "btrfs device remove failed (exit {}): {}",
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
    mount_point: &MountPoint,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    let result = run_replace_with_progress(
        runner,
        &CmdRequest::BtrfsReplaceStart {
            devid,
            target_device: target_device.to_owned(),
            mount_point: mount_point.clone(),
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
    mount_point: &MountPoint,
) -> Result<(), PoolError> {
    let result = runner.run(&CmdRequest::BtrfsFilesystemResize {
        devid,
        mount_point: mount_point.clone(),
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

/// Shared helper: evict a live (present) device from the pool.
///
/// 1. Probes the current pool to decide if RAID1→single conversion is needed.
/// 2. If only one device would remain, balances to single first.
///    Note: if a device is faulty, we do not want it to participate in the balance.
///          Rather, it should just be removed. But we don't have that info.
/// 3. Removes the target device from the pool.
/// 4. Closes the LUKS mapper (best-effort; warns on failure).
///
/// Returns `Ok(())` as a no-op if the target mapper is not present in the pool.
#[allow(clippy::doc_overindented_list_items)]
pub fn evict_present_device<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mapper: &str,
    mount_point: &MountPoint,
    progress: ProgressOutput,
) -> Result<(), PoolError> {
    let pool = probe_pool(runner, fs, mount_point).map_err(|e| PoolError::Failed(e.to_string()))?;

    let device_path = format!("/dev/mapper/{mapper}");
    let in_pool = pool.devices.iter().any(|d| d.mapper.0 == mapper);

    if !in_pool {
        return Ok(());
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

    Ok(())
}

/// Bootstrap the pool: mkfs.btrfs (only if no superblock) then mount.
pub fn pool_bootstrap_mount<R: CommandRunner + Sync>(
    runner: &R,
    device: &str,
    mount_point: &MountPoint,
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
    let _ = std::fs::create_dir_all(mount_point.as_str());

    let mount = runner.run(&CmdRequest::Mount {
        device: device.to_owned(),
        mount_point: mount_point.clone(),
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
    mount_point: &MountPoint,
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

    let _ = std::fs::create_dir_all(mount_point.as_str());

    let mount = runner.run(&CmdRequest::Mount {
        device: devices[0].clone(),
        mount_point: mount_point.clone(),
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
    use crate::progress::{self, ProgressOutput};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::Duration;

    fn ok_raw() -> RawCommandOutput {
        RawCommandOutput {
            cmd: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".into())
    }

    use crate::cmd::CmdError;

    #[derive(Default)]
    struct RemoveGate {
        released: bool,
        done: bool,
    }

    #[derive(Clone)]
    struct BlockingRemoveRunner {
        gate: Arc<(Mutex<RemoveGate>, Condvar)>,
    }

    impl BlockingRemoveRunner {
        fn new() -> Self {
            Self {
                gate: Arc::new((Mutex::new(RemoveGate::default()), Condvar::new())),
            }
        }

        fn gate(&self) -> Arc<(Mutex<RemoveGate>, Condvar)> {
            Arc::clone(&self.gate)
        }
    }

    impl CommandRunner for BlockingRemoveRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsDeviceRemove { .. } => {
                    let (lock, cvar) = &*self.gate;
                    let mut state = lock.lock().unwrap();
                    while !state.released {
                        state = cvar.wait(state).unwrap();
                    }
                    state.done = true;
                    cvar.notify_all();
                    Ok(ok_raw())
                }
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

    #[derive(Default)]
    struct FakeSleeper {
        calls: Mutex<Vec<Duration>>,
    }

    impl FakeSleeper {
        fn calls(&self) -> Vec<Duration> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl progress::Sleeper for FakeSleeper {
        fn sleep(&self, duration: Duration) {
            self.calls.lock().unwrap().push(duration);
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        lines: Mutex<Vec<String>>,
        jsons: Mutex<Vec<String>>,
        clears: AtomicUsize,
        gate: Option<Arc<(Mutex<RemoveGate>, Condvar)>>,
    }

    impl RecordingSink {
        fn with_gate(gate: Arc<(Mutex<RemoveGate>, Condvar)>) -> Self {
            Self {
                gate: Some(gate),
                ..Self::default()
            }
        }

        fn lines(&self) -> Vec<String> {
            self.lines.lock().unwrap().clone()
        }

        fn clears(&self) -> usize {
            self.clears.load(Ordering::SeqCst)
        }

        fn release_worker_and_wait(&self) {
            let Some(gate) = &self.gate else {
                return;
            };
            let (lock, cvar) = &**gate;
            let mut state = lock.lock().unwrap();
            state.released = true;
            cvar.notify_all();
            while !state.done {
                state = cvar.wait(state).unwrap();
            }
        }
    }

    impl progress::ProgressSink for RecordingSink {
        fn write_line(&self, msg: &str) {
            self.lines.lock().unwrap().push(msg.to_owned());
            self.release_worker_and_wait();
        }

        fn write_json(&self, msg: &str) {
            self.jsons.lock().unwrap().push(msg.to_owned());
            self.release_worker_and_wait();
        }

        fn clear(&self) {
            self.clears.fetch_add(1, Ordering::SeqCst);
        }
    }

    /*
     * Intent: pool_remove_device_using routes device removal through the
     * heartbeat progress helper.
     * Why it exists: the pool layer owns the btrfs device-remove call; a
     * direct runner.run here would make slow removals silent again.
     * Scenario: a mocked remove blocks until the first human heartbeat is
     * written, then completes successfully.
     */
    #[test]
    fn pool_remove_device_using_emits_heartbeat() {
        let runner = BlockingRemoveRunner::new();
        let sleeper = FakeSleeper::default();
        let sink = RecordingSink::with_gate(runner.gate());

        let result = pool_remove_device_using(
            &runner,
            "/dev/mapper/braid-disk2",
            &mp(),
            ProgressOutput::Human,
            &sleeper,
            &sink,
        );

        assert!(result.is_ok(), "pool remove should succeed: {result:?}");
        let lines = sink.lines();
        assert!(
            !lines.is_empty(),
            "expected pool remove to emit a heartbeat"
        );
        assert_eq!(
            lines[0],
            progress::format_device_remove_heartbeat(progress::HEARTBEAT_INTERVAL)
        );
        assert_eq!(sink.clears(), 1, "human progress should clear once");
        assert!(
            sleeper
                .calls()
                .iter()
                .any(|d| *d == progress::HEARTBEAT_INTERVAL),
            "pool remove should sleep at the configured heartbeat interval"
        );
    }

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
                CmdRequest::BtrfsFilesystemShow { .. } => {
                    if self.probe_fails {
                        return Err(CmdError::Failed(
                            "btrfs filesystem show: mock failure".to_owned(),
                        ));
                    }
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

    struct RestoreFs;

    impl Filesystem for RestoreFs {
        fn exists(&self, _path: &str) -> bool {
            false
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }

        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path == "/proc/self/mountinfo" {
                return Ok(
                    "36 35 0:32 / /mnt/storage rw shared:1 - btrfs /dev/mapper/braid-disk1 rw\n"
                        .to_string(),
                );
            }
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
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
        let result = maybe_restore_raid1(&runner, &RestoreFs, &mp(), 0, ProgressOutput::Off);
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
        let result = maybe_restore_raid1(&runner, &RestoreFs, &mp(), 1, ProgressOutput::Off);
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
        let result = maybe_restore_raid1(&runner, &RestoreFs, &mp(), 2, ProgressOutput::Off);
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
        let result = maybe_restore_raid1(&runner, &RestoreFs, &mp(), 1, ProgressOutput::Off);
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
        let result = maybe_restore_raid1(&runner, &RestoreFs, &mp(), 1, ProgressOutput::Off);
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
        let err = balance_error("btrfs balance to RAID1", &mp(), &result);
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
        let err = balance_error("btrfs balance to RAID1", &mp(), &result);
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
            &mp(),
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
            &mp(),
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
