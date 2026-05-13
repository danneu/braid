use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
use crate::mapper_close::close_mapper_best_effort;
use crate::probe::{Filesystem, probe_pool};
use crate::progress::{
    self, ProgressOutput, Sleeper, run_device_remove_with_progress, run_replace_with_progress,
    run_with_progress,
};
use crate::status_tag::{StatusTag, color_enabled_for_stderr, status_line};
use crate::types::{LuksUuid, MapperName, MountPoint};
use std::collections::BTreeMap;

#[derive(Debug, thiserror::Error)]
pub enum PoolError {
    #[error("command failed: {0}")]
    Cmd(#[from] CmdError),
    #[error("{0}")]
    Failed(String),
}

/// Identity record per present pool device captured at planning time and
/// again at execution-time validation. Equality fails on any field change,
/// catching same-mapper replacement (cryptsetup close + open on a different
/// LUKS device under the same `braid-<name>` mapper between plan and
/// execute), which a mapper-name-only comparison would miss.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceIdentity {
    pub devid: u64,
    pub luks_uuid: LuksUuid,
}

/// Structured drift summary returned by `validate_pool_topology` on
/// mismatch. Position-neutral: the caller chooses where to invoke the
/// validation (pre-journal clean-failure vs. post-journal recovery
/// handoff) and wraps the returned drift with the appropriate
/// remediation. The struct exposes the derived `target_present` and
/// `target_null_underlying` flags so callers can route the hot-unplug
/// case without re-probing.
#[derive(Debug)]
pub struct TopologyDrift {
    /// `None` when the probe succeeded; `Some(message)` when probing
    /// itself failed.
    pub probe_error: Option<String>,
    pub target_present: bool,
    pub target_null_underlying: bool,
    pub expected_present_identities: BTreeMap<MapperName, DeviceIdentity>,
    pub observed_present_identities: BTreeMap<MapperName, DeviceIdentity>,
    pub observed_missing_count: u64,
}

impl TopologyDrift {
    /// True when the target mapper is gone from the live pool AND its
    /// dm-crypt node still resolves but cryptsetup reports `device: (null)`.
    /// Distinct recovery flow: re-plug does NOT self-heal the mapper.
    pub fn is_target_hot_unplug(&self) -> bool {
        !self.target_present && self.target_null_underlying
    }

    /// Brief drift summary for embedding in error messages. Names the
    /// observed deltas (missing, added, identity-changed) so an operator
    /// can tell whether a disk was added/removed vs. replaced under the
    /// same mapper. Callers append the remediation suffix; this helper
    /// is position-neutral on purpose.
    pub fn detail(&self) -> String {
        if let Some(err) = &self.probe_error {
            return format!("topology validation probe failed: {err}");
        }
        let expected_keys: std::collections::BTreeSet<&MapperName> =
            self.expected_present_identities.keys().collect();
        let observed_keys: std::collections::BTreeSet<&MapperName> =
            self.observed_present_identities.keys().collect();

        let mut parts = Vec::<String>::new();
        let missing: Vec<&MapperName> = expected_keys.difference(&observed_keys).copied().collect();
        let added: Vec<&MapperName> = observed_keys.difference(&expected_keys).copied().collect();
        let mut identity_changed: Vec<String> = Vec::new();
        for key in expected_keys.intersection(&observed_keys) {
            let expected_id = &self.expected_present_identities[*key];
            let observed_id = &self.observed_present_identities[*key];
            if expected_id != observed_id {
                identity_changed.push(format!(
                    "{} (expected devid={} luks_uuid={}, observed devid={} luks_uuid={})",
                    key.0,
                    expected_id.devid,
                    expected_id.luks_uuid,
                    observed_id.devid,
                    observed_id.luks_uuid,
                ));
            }
        }

        if !missing.is_empty() {
            let names: Vec<String> = missing.iter().map(|m| m.0.clone()).collect();
            parts.push(format!("expected but absent: {}", names.join(", ")));
        }
        if !added.is_empty() {
            let names: Vec<String> = added.iter().map(|m| m.0.clone()).collect();
            parts.push(format!("observed but unexpected: {}", names.join(", ")));
        }
        if !identity_changed.is_empty() {
            parts.push(format!("identity changed: {}", identity_changed.join("; ")));
        }
        if self.observed_missing_count > 0 {
            parts.push(format!(
                "{} btrfs-MISSING device(s)",
                self.observed_missing_count
            ));
        }
        if self.is_target_hot_unplug() {
            parts.push(
                "target mapper present in pool table but cryptsetup reports \
                 `device: (null)` (null-underlying / hot-unplug)"
                    .to_owned(),
            );
        }

        if parts.is_empty() {
            "topology changed between plan and execute".to_owned()
        } else {
            format!(
                "topology changed between plan and execute: {}",
                parts.join("; ")
            )
        }
    }
}

/// Execution-time validation that the live pool topology matches the
/// planner's exact identity snapshot. Returns `Err(TopologyDrift)` on
/// probe failure, target absence, missing-device drift, mapper-set
/// mismatch, or per-mapper identity drift (devid or luks_uuid changed
/// for the same mapper name).
///
/// **Position-dependent failure semantics.** The helper itself is
/// position-neutral: the caller chooses where to call it and wraps the
/// returned drift with the appropriate remediation:
/// - Pre-`journal::write_journal` (clean failure path): caller wraps
///   with "re-run `braid remove`". Failure here is a "command never
///   started" exit and must NOT strand `pending-op.json`
///   (principle 3, `docs/principles.md:23`).
/// - Post-`journal::write_journal` (recovery-handoff path): caller
///   wraps with "run `braid recover` to reconcile". Failure here
///   intentionally preserves `pending-op.json` so the recovery flow
///   can replay/reconcile.
pub fn validate_pool_topology<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    target_mapper: &str,
    expected_present_identities: &BTreeMap<MapperName, DeviceIdentity>,
) -> Result<(), TopologyDrift> {
    let pool = match probe_pool(runner, fs, mount_point) {
        Ok(p) => p,
        Err(e) => {
            return Err(TopologyDrift {
                probe_error: Some(e.to_string()),
                target_present: false,
                target_null_underlying: false,
                expected_present_identities: expected_present_identities.clone(),
                observed_present_identities: BTreeMap::new(),
                observed_missing_count: 0,
            });
        }
    };
    let observed_present_identities: BTreeMap<MapperName, DeviceIdentity> = pool
        .devices
        .iter()
        .map(|d| {
            (
                d.mapper.clone(),
                DeviceIdentity {
                    devid: d.devid,
                    luks_uuid: d.luks_uuid.clone(),
                },
            )
        })
        .collect();
    let target_present = observed_present_identities
        .keys()
        .any(|m| m.0 == target_mapper);
    let target_null_underlying = pool
        .null_underlying
        .iter()
        .any(|n| n.mapper.0 == target_mapper);

    let identities_match = observed_present_identities == *expected_present_identities;
    let no_missing = pool.missing_count == 0;
    if identities_match && no_missing && target_present {
        return Ok(());
    }
    Err(TopologyDrift {
        probe_error: None,
        target_present,
        target_null_underlying,
        expected_present_identities: expected_present_identities.clone(),
        observed_present_identities,
        observed_missing_count: pool.missing_count,
    })
}

/// Add a device to an existing btrfs pool.
pub fn pool_add_device<R: CommandRunner + Sync>(
    runner: &R,
    device: &str,
    mount_point: &MountPoint,
    force: bool,
) -> Result<(), PoolError> {
    if force {
        let scan_result = runner.run(&CmdRequest::BtrfsDeviceScanForget {
            devices: vec![device.to_owned()],
        })?;
        if scan_result.exit_status != 0 {
            return Err(PoolError::Failed(format!(
                "btrfs device scan --forget failed (exit {}): {}",
                scan_result.exit_status,
                scan_result.stderr.trim()
            )));
        }

        let wipe_result = runner.run(&CmdRequest::WipefsBtrfs {
            device: device.to_owned(),
        })?;
        if wipe_result.exit_status != 0 {
            return Err(PoolError::Failed(format!(
                "wipefs --types btrfs failed (exit {}): {}",
                wipe_result.exit_status,
                wipe_result.stderr.trim()
            )));
        }
    }

    let result = runner.run(&CmdRequest::BtrfsDeviceAdd {
        device: device.to_owned(),
        mount_point: mount_point.clone(),
        force,
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

/// Build a `PoolError::Failed` for `btrfs replace start`, detecting the
/// scrub-collision rejection and appending a recovery hint. The kernel's
/// `--enqueue` wait does not cover scrub (scrub is not in the `exclusive_op`
/// set), so the only way out is for the operator to wait or cancel scrub.
fn replace_error(mount_point: &MountPoint, result: &RawCommandOutput) -> PoolError {
    let stderr = result.stderr.to_lowercase();
    if stderr.contains("scrub is in progress") {
        PoolError::Failed(format!(
            "btrfs replace failed (exit {}): {}\nhint: a scrub is currently running -- check progress with `braid status`, or run `btrfs scrub cancel {mount_point}` to abort it before retrying",
            result.exit_status,
            result.stderr.trim(),
        ))
    } else {
        PoolError::Failed(format!(
            "btrfs replace failed (exit {}): {}",
            result.exit_status,
            result.stderr.trim(),
        ))
    }
}

/// Recovery context for a failed `btrfs device remove`, because present-disk
/// removal and missing-device cleanup require different operator followups.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RemoveContext {
    Live,
    Missing,
}

/// Build a `PoolError::Failed` for `btrfs device remove`, decoding the
/// kernel's min-devices rejection and appending a recovery hint that accounts
/// for braid's already-written pending operation journal.
fn device_remove_error(
    ctx: RemoveContext,
    mount_point: &MountPoint,
    result: &RawCommandOutput,
) -> PoolError {
    let stderr = result.stderr.to_lowercase();
    if stderr.contains("unable to go below") {
        let hint = match ctx {
            RemoveContext::Live => format!(
                "a non-RAID1 chunk likely requires more devices than will remain. \
                 Inspect with `btrfs filesystem usage {mount_point}`, then \
                 `btrfs balance start -dconvert=raid1 -mconvert=raid1 -f {mount_point}` \
                 to convert it back to RAID1, then `braid recover` to clear the \
                 pending operation, then retry `braid remove`."
            ),
            RemoveContext::Missing => "a non-RAID1 chunk requires more devices than will remain. \
                 While a device is missing, do not lower redundancy -- \
                 repair the missing device instead. Run `braid recover` to \
                 clear the pending operation, then `braid replace --missing-id <devid>` \
                 to rebuild data onto a replacement disk."
                .to_owned(),
        };
        PoolError::Failed(format!(
            "btrfs device remove failed (exit {}): {}\nhint: {hint}",
            result.exit_status,
            result.stderr.trim(),
        ))
    } else {
        PoolError::Failed(format!(
            "btrfs device remove failed (exit {}): {}",
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
        let color_enabled = color_enabled_for_stderr();
        eprint!(
            "{}",
            status_line(
                StatusTag::Wait,
                color_enabled,
                "pool: restoring RAID1 redundancy...",
            )
        );
        pool_balance_raid1_soft(runner, mount_point, progress)?;
        eprint!(
            "{}",
            status_line(
                StatusTag::Ok,
                color_enabled,
                "pool: RAID1 redundancy restored",
            )
        );
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
    device_remove_result(RemoveContext::Live, mount_point, &result)
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
    device_remove_result(RemoveContext::Missing, mount_point, &result)
}

fn device_remove_result(
    ctx: RemoveContext,
    mount_point: &MountPoint,
    result: &RawCommandOutput,
) -> Result<(), PoolError> {
    if result.exit_status != 0 {
        return Err(device_remove_error(ctx, mount_point, result));
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
        return Err(replace_error(mount_point, &result));
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
/// 1. If `needs_balance` is true, balances RAID1 -> single first
///    (only safe when the planner promised a 2->1 transition).
/// 2. Removes the target device from the pool.
/// 3. Closes the LUKS mapper (best-effort; warns on failure).
///
/// Caller is responsible for upstream topology validation (see
/// `validate_pool_topology`, which the caller invokes pre-journal AND
/// post-journal). This helper assumes the typed work plan is consistent
/// with the live pool and executes it. Drift in the residual microsecond
/// window between the post-journal validation and the actual btrfs
/// command surfaces as a btrfs command failure (e.g.
/// `pool_balance_single` or `pool_remove_device` returning
/// `PoolError::Failed`) and preserves the journal for `braid recover`
/// to reconcile.
#[allow(clippy::doc_overindented_list_items)]
pub fn evict_present_device<R, S>(
    runner: &R,
    sleeper: &S,
    mapper: &str,
    mount_point: &MountPoint,
    needs_balance: bool,
    progress: ProgressOutput,
) -> Result<(), PoolError>
where
    R: CommandRunner + Sync,
    S: Sleeper + ?Sized,
{
    let device_path = format!("/dev/mapper/{mapper}");
    let color_enabled = color_enabled_for_stderr();
    if needs_balance {
        eprint!(
            "{}",
            status_line(
                StatusTag::Wait,
                color_enabled,
                "pool: balancing RAID1 to single profile...",
            )
        );
        pool_balance_single(runner, mount_point, progress)?;
        eprint!(
            "{}",
            status_line(
                StatusTag::Ok,
                color_enabled,
                "pool: balanced to single profile",
            )
        );
    }

    eprint!(
        "{}",
        status_line(
            StatusTag::Wait,
            color_enabled,
            &format!("pool: removing {mapper}..."),
        )
    );
    pool_remove_device(runner, &device_path, mount_point, progress)?;
    eprint!(
        "{}",
        status_line(
            StatusTag::Ok,
            color_enabled,
            &format!("pool: {mapper} removed"),
        )
    );

    let close_label = mapper.strip_prefix("braid-").unwrap_or(mapper);
    close_mapper_best_effort(runner, sleeper, mapper, close_label, color_enabled);

    Ok(())
}

/// Bootstrap the pool: mkfs.btrfs, then mount.
pub fn pool_bootstrap_mount<R: CommandRunner + Sync>(
    runner: &R,
    device: &str,
    mount_point: &MountPoint,
) -> Result<(), PoolError> {
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

/// Bootstrap the pool with multiple devices in RAID1: mkfs.btrfs -d raid1
/// -m raid1, then mount.
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
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
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

    #[test]
    // Intent: single-device bootstrap formats and mounts without a btrfs
    // probe round-trip.
    // Why it exists: mapper ownership is enforced before this helper, so the
    // helper should be a narrow mkfs + mount wrapper.
    // Scenario: first disk in a new pool was just LUKS-formatted and opened;
    // the mapper has no btrfs filesystem yet.
    fn pool_bootstrap_mount_runs_mkfs_when_fresh() {
        let device = "/dev/mapper/braid-disk1";
        let mount_point = mp();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MkfsBtrfs {
                    device: device.into(),
                },
                ok_raw(),
            )
            .with_output(
                CmdRequest::Mount {
                    device: device.into(),
                    mount_point: mount_point.clone(),
                },
                ok_raw(),
            );

        pool_bootstrap_mount(&runner, device, &mount_point).unwrap();

        assert_eq!(
            runner.requests(),
            vec![
                CmdRequest::MkfsBtrfs {
                    device: device.into(),
                },
                CmdRequest::Mount {
                    device: device.into(),
                    mount_point,
                },
            ]
        );
    }

    #[test]
    // Intent: RAID1 bootstrap formats and mounts without per-device btrfs
    // probe round-trips.
    // Why it exists: mapper ownership is enforced before this helper, so the
    // helper should be a narrow mkfs RAID1 + mount wrapper.
    // Scenario: a new pool is bootstrapped from two freshly opened LUKS
    // mappers.
    fn pool_bootstrap_mount_raid1_runs_mkfs_when_all_fresh() {
        let devices = vec![
            "/dev/mapper/braid-disk1".to_owned(),
            "/dev/mapper/braid-disk2".to_owned(),
        ];
        let mount_point = mp();
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::MkfsBtrfsRaid1 {
                    devices: devices.clone(),
                },
                ok_raw(),
            )
            .with_output(
                CmdRequest::Mount {
                    device: devices[0].clone(),
                    mount_point: mount_point.clone(),
                },
                ok_raw(),
            );

        pool_bootstrap_mount_raid1(&runner, &devices, &mount_point).unwrap();

        assert_eq!(
            runner.requests(),
            vec![
                CmdRequest::MkfsBtrfsRaid1 {
                    devices: devices.clone(),
                },
                CmdRequest::Mount {
                    device: devices[0].clone(),
                    mount_point,
                },
            ]
        );
    }

    #[test]
    fn pool_add_device_without_force_runs_only_device_add() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsDeviceAdd {
                device: "/dev/mapper/braid-new".into(),
                mount_point: mp(),
                force: false,
            },
            ok_raw(),
        );

        pool_add_device(&runner, "/dev/mapper/braid-new", &mp(), false).unwrap();

        assert_eq!(
            runner.requests(),
            vec![CmdRequest::BtrfsDeviceAdd {
                device: "/dev/mapper/braid-new".into(),
                mount_point: mp(),
                force: false,
            }]
        );
    }

    #[test]
    fn pool_add_device_with_force_forgets_wipes_then_adds() {
        let runner = MockRunner::default()
            .with_output(
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec!["/dev/mapper/braid-returned".into()],
                },
                ok_raw(),
            )
            .with_output(
                CmdRequest::WipefsBtrfs {
                    device: "/dev/mapper/braid-returned".into(),
                },
                ok_raw(),
            )
            .with_output(
                CmdRequest::BtrfsDeviceAdd {
                    device: "/dev/mapper/braid-returned".into(),
                    mount_point: mp(),
                    force: true,
                },
                ok_raw(),
            );

        pool_add_device(&runner, "/dev/mapper/braid-returned", &mp(), true).unwrap();

        assert_eq!(
            runner.requests(),
            vec![
                CmdRequest::BtrfsDeviceScanForget {
                    devices: vec!["/dev/mapper/braid-returned".into()],
                },
                CmdRequest::WipefsBtrfs {
                    device: "/dev/mapper/braid-returned".into(),
                },
                CmdRequest::BtrfsDeviceAdd {
                    device: "/dev/mapper/braid-returned".into(),
                    mount_point: mp(),
                    force: true,
                },
            ]
        );
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
            sleeper.calls().contains(&progress::HEARTBEAT_INTERVAL),
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
    // Intent: device_remove_result appends the live-removal recovery hint for
    // the RAID1 min-devices rejection.
    // Why it exists: present-disk remove can hit a kernel fallback that
    // preflight did not predict, and the operator needs the concrete balance
    // and recover sequence rather than raw btrfs stderr.
    // Scenario: `braid remove` asks btrfs to remove a mapper, but the kernel
    // refuses to go below two devices on RAID1.
    fn device_remove_result_live_raid1_min_includes_balance_hint() {
        let result = RawCommandOutput {
            cmd: String::new(),
            stdout: String::new(),
            stderr: "ERROR: error removing device '/dev/mapper/braid-disk2': unable to go below two devices on raid1".to_owned(),
            exit_status: 1,
        };

        let err = device_remove_result(RemoveContext::Live, &mp(), &result)
            .expect_err("min-devices rejection should return an error")
            .to_string();
        assert!(err.contains("hint:"), "error should include hint: {err}");
        assert!(
            err.contains("dconvert=raid1"),
            "hint should suggest converting data chunks to RAID1: {err}"
        );
        assert!(
            err.contains("braid recover"),
            "hint should clear pending operation first: {err}"
        );
        assert!(
            err.contains("braid remove"),
            "hint should tell operator to retry remove: {err}"
        );
        assert!(
            err.contains("/mnt/storage"),
            "hint should include mount point: {err}"
        );
    }

    #[test]
    // Intent: device_remove_result appends the same live-removal recovery hint
    // for non-RAID1 min-devices variants.
    // Why it exists: the kernel has several BTRFS_ERROR_DEV_RAID*_MIN_NOT_MET
    // messages, and braid should classify the whole family with one stable
    // substring instead of matching only RAID1.
    // Scenario: `braid remove` encounters a stray RAID1C3 chunk that requires
    // more devices than would remain.
    fn device_remove_result_live_raid1c3_min_includes_balance_hint() {
        let result = RawCommandOutput {
            cmd: String::new(),
            stdout: String::new(),
            stderr: "ERROR: error removing device '/dev/mapper/braid-disk2': unable to go below three devices on raid1c3".to_owned(),
            exit_status: 1,
        };

        let err = device_remove_result(RemoveContext::Live, &mp(), &result)
            .expect_err("min-devices rejection should return an error")
            .to_string();
        assert!(err.contains("hint:"), "error should include hint: {err}");
        assert!(
            err.contains("dconvert=raid1"),
            "hint should suggest converting data chunks to RAID1: {err}"
        );
        assert!(
            err.contains("braid recover"),
            "hint should clear pending operation first: {err}"
        );
        assert!(
            err.contains("braid remove"),
            "hint should tell operator to retry remove: {err}"
        );
    }

    #[test]
    // Intent: device_remove_result appends the missing-device recovery hint
    // for the CLI-reachable non-RAID1 min-devices rejection.
    // Why it exists: a degraded pool cannot lower redundancy while a device is
    // missing, so the missing path must steer toward recovery and replace, not
    // raw btrfs balance.
    // Scenario: `braid remove-missing` hits a leftover RAID1C3 chunk while
    // clearing a missing device slot from a larger pool.
    fn device_remove_result_missing_raid1c3_min_includes_replace_hint() {
        let result = RawCommandOutput {
            cmd: String::new(),
            stdout: String::new(),
            stderr: "ERROR: error removing device '2': unable to go below three devices on raid1c3"
                .to_owned(),
            exit_status: 1,
        };

        let err = device_remove_result(RemoveContext::Missing, &mp(), &result)
            .expect_err("min-devices rejection should return an error")
            .to_string();
        assert!(err.contains("hint:"), "error should include hint: {err}");
        assert!(
            err.contains("braid replace --missing-id"),
            "hint should point at missing-device replacement: {err}"
        );
        assert!(
            err.contains("braid recover"),
            "hint should clear pending operation first: {err}"
        );
        assert!(
            !err.contains("dconvert=raid1"),
            "missing hint must not suggest RAID1 conversion: {err}"
        );
        assert!(
            !err.contains("btrfs balance"),
            "missing hint must not suggest balance while degraded: {err}"
        );
    }

    #[test]
    // Intent: device_remove_result returns a plain error for unrelated btrfs
    // device remove failures.
    // Why it exists: the min-devices recovery hint should not be attached to
    // every remove failure, because unrelated failures need different
    // operator action.
    // Scenario: btrfs refuses device removal because the device is busy.
    fn device_remove_result_no_hint_for_unrelated_failure() {
        let result = RawCommandOutput {
            cmd: String::new(),
            stdout: String::new(),
            stderr: "ERROR: device is busy".to_owned(),
            exit_status: 1,
        };

        let err = device_remove_result(RemoveContext::Live, &mp(), &result)
            .expect_err("non-zero exit status should return an error")
            .to_string();
        assert!(
            !err.contains("hint:"),
            "unrelated failure must not include min-devices hint: {err}"
        );
    }

    #[test]
    // Intent: device_remove_result leaves successful btrfs device remove output
    // as a no-op.
    // Why it exists: the result router should only classify failures; success
    // must pass through without consulting stderr or context.
    // Scenario: btrfs removes the device successfully after braid's preflight
    // and progress handling have already completed.
    fn device_remove_result_ok_passes_through() {
        let result = RawCommandOutput {
            cmd: String::new(),
            stdout: String::new(),
            stderr: String::new(),
            exit_status: 0,
        };

        let outcome = device_remove_result(RemoveContext::Missing, &mp(), &result);
        assert!(outcome.is_ok(), "success should pass through: {outcome:?}");
    }

    #[test]
    // Intent: pool_remove_device wires present-disk removal to the live
    // recovery hint context.
    // Why it exists: the compiler enforces that a context is passed, but only
    // this wrapper-level test catches a swapped Live/Missing value at the
    // public remove boundary.
    // Scenario: `braid remove` fails with the RAID1 min-devices rejection and
    // should point at balance, recover, then retry remove.
    fn pool_remove_device_failure_emits_live_balance_hint() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsDeviceRemove {
                device: "/dev/mapper/braid-disk2".to_owned(),
                mount_point: mp(),
            },
            RawCommandOutput {
                cmd: String::new(),
                stdout: String::new(),
                stderr: "ERROR: error removing device '/dev/mapper/braid-disk2': unable to go below two devices on raid1".to_owned(),
                exit_status: 1,
            },
        );

        let err = pool_remove_device(
            &runner,
            "/dev/mapper/braid-disk2",
            &mp(),
            ProgressOutput::Off,
        )
        .expect_err("min-devices rejection should return an error")
        .to_string();
        assert!(
            err.contains("dconvert=raid1"),
            "live hint should suggest RAID1 conversion: {err}"
        );
        assert!(
            err.contains("braid recover"),
            "live hint should clear pending operation first: {err}"
        );
        assert!(
            err.contains("braid remove"),
            "live hint should tell operator to retry remove: {err}"
        );
        assert!(
            !err.contains("braid replace --missing-id"),
            "live hint must not point at missing-device replacement: {err}"
        );
    }

    #[test]
    // Intent: pool_remove_device_using wires missing-device cleanup to the
    // missing recovery hint context.
    // Why it exists: the compiler enforces that a context is passed, but only
    // this wrapper-level test catches a swapped Live/Missing value at the
    // remove-missing boundary.
    // Scenario: `braid remove-missing` fails on a stray RAID1C3 chunk and
    // should point at recover, then replace the missing device.
    fn pool_remove_device_using_failure_emits_missing_replace_hint() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsDeviceRemove {
                device: "2".to_owned(),
                mount_point: mp(),
            },
            RawCommandOutput {
                cmd: String::new(),
                stdout: String::new(),
                stderr:
                    "ERROR: error removing device '2': unable to go below three devices on raid1c3"
                        .to_owned(),
                exit_status: 1,
            },
        );
        let sleeper = FakeSleeper::default();
        let sink = RecordingSink::default();

        let err =
            pool_remove_device_using(&runner, "2", &mp(), ProgressOutput::Off, &sleeper, &sink)
                .expect_err("min-devices rejection should return an error")
                .to_string();
        assert!(
            err.contains("braid replace --missing-id"),
            "missing hint should point at replacement: {err}"
        );
        assert!(
            err.contains("braid recover"),
            "missing hint should clear pending operation first: {err}"
        );
        assert!(
            !err.contains("dconvert=raid1"),
            "missing hint must not suggest RAID1 conversion: {err}"
        );
        assert!(
            !err.contains("btrfs balance"),
            "missing hint must not suggest balance while degraded: {err}"
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

    #[test]
    // Intent: when `btrfs replace start` is rejected because a scrub is
    // running on the same pool, the surfaced error includes a recovery hint
    // pointing at `btrfs scrub cancel` and the pool's mount point so the
    // operator knows the exact next command to run.
    // Why it exists: scrub is not in btrfs' `exclusive_operation` set, so the
    // `--enqueue` wait braid passes cannot wait it out -- the kernel emits
    // BTRFS_IOCTL_DEV_REPLACE_RESULT_SCRUB_INPROGRESS and upstream's
    // `replace_dev_result2string` (reference/btrfs-progs/cmds/replace.c:50-64)
    // surfaces "scrub is in progress" in the START-ioctl error. Without the
    // hint the operator sees only the raw upstream stderr.
    // Scenario: a `braid-scrub.service` run (or manual `btrfs scrub start`)
    // is in flight when the operator invokes `braid replace`, and the kernel
    // rejects the START ioctl.
    fn pool_replace_device_scrub_in_progress_includes_hint() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsReplaceStart {
                devid: 2,
                target_device: "/dev/mapper/braid-new".to_owned(),
                mount_point: MountPoint("/mnt/storage".to_owned()),
            },
            RawCommandOutput {
                cmd: String::new(),
                stdout: String::new(),
                stderr: "ERROR: ioctl(DEV_REPLACE_START) failed on \"/mnt/storage\": Operation not permitted, scrub is in progress".to_owned(),
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
        assert!(err.contains("hint:"), "error should include hint: {err}");
        assert!(err.contains("scrub"), "hint should mention scrub: {err}");
        assert!(
            err.contains("/mnt/storage"),
            "hint should include mount point: {err}"
        );
    }

    #[test]
    // Intent: an unrelated `btrfs replace start` failure must not get the
    // scrub-collision recovery hint.
    // Why it exists: the helper classifies on a single substring; this test
    // locks in the negative path so a future broadening of the classifier
    // (or a typo'd substring match) cannot silently misroute every replace
    // failure into the scrub recovery hint.
    // Scenario: target device is too small -- a wholly different rejection
    // path that has nothing to do with scrub.
    fn pool_replace_device_no_hint_for_unrelated_failure() {
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
        assert!(
            !err.contains("hint:"),
            "unrelated failure must not include scrub hint: {err}"
        );
    }

    /// Custom runner for the trimmed `evict_present_device` test. The
    /// helper no longer probes the pool, so we only need to mock the
    /// mutating commands -- BtrfsDeviceRemove and CryptsetupClose.
    /// Mutating commands are recorded so the test can assert sequencing.
    #[derive(Clone)]
    struct EvictRunner {
        close_exit: i32,
        close_busy_then_success: bool,
        close_attempts: Arc<AtomicU32>,
        invocations: Arc<Mutex<Vec<&'static str>>>,
    }

    impl Default for EvictRunner {
        fn default() -> Self {
            Self {
                close_exit: 0,
                close_busy_then_success: false,
                close_attempts: Arc::new(AtomicU32::new(0)),
                invocations: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    impl EvictRunner {
        fn record(&self, tag: &'static str) {
            self.invocations.lock().unwrap().push(tag);
        }
    }

    impl CommandRunner for EvictRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsDeviceRemove { .. } => {
                    self.record("BtrfsDeviceRemove");
                    Ok(ok_raw())
                }
                CmdRequest::CryptsetupClose { .. } => {
                    self.record("CryptsetupClose");
                    let attempt = self.close_attempts.fetch_add(1, Ordering::SeqCst) + 1;
                    let exit_status = if self.close_busy_then_success && attempt == 1 {
                        5
                    } else if self.close_busy_then_success {
                        0
                    } else {
                        self.close_exit
                    };
                    Ok(RawCommandOutput {
                        cmd: String::new(),
                        stdout: String::new(),
                        stderr: match exit_status {
                            0 => String::new(),
                            4 => "device does not exist".into(),
                            _ => "device is busy".into(),
                        },
                        exit_status,
                    })
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

    // Intent: pool::evict_present_device's trailing best-effort LUKS close
    // closes its [wait] row with [warn] when cryptsetup returns non-zero.
    // Why it exists: Principle 13 forbids dangling [wait] rows; a best-effort
    // close that exits the command 0 must still announce the failure on the
    // same subject so the wait window is closed for the operator.
    // Scenario: a 3-disk pool evicts one mapper; pool_remove_device succeeds
    // but cryptsetup close returns ENODEV.
    #[test]
    fn evict_present_device_close_failure_emits_warn_row() {
        let runner = EvictRunner {
            close_exit: 4,
            ..EvictRunner::default()
        };
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            // 3-disk fixture -> remaining == 2 -> no balance needed.
            let result = evict_present_device(
                &runner,
                &progress::NoopSleeper,
                "braid-disk2",
                &mp(),
                false,
                ProgressOutput::Off,
            );
            assert!(result.is_ok(), "evict should still return Ok: {result:?}");
        });
        let wait = "[wait] disk disk2: locking...";
        let warn = "[warn] disk disk2: lock failed (cryptsetup close braid-disk2 failed (exit 4): device does not exist)";
        assert!(captured.contains(wait), "missing wait row: {captured:?}");
        assert!(captured.contains(warn), "missing warn row: {captured:?}");
        assert!(
            captured.find(wait) < captured.find(warn),
            "wait must precede warn, got: {captured:?}"
        );
    }

    // Intent: pool::evict_present_device routes its best-effort LUKS close
    // through the retry helper when cryptsetup reports the mapper busy.
    // Why it exists: a later refactor must not regress the remove path back to
    // a single-shot close that leaks the old mapper on transient EBUSY.
    // Scenario: btrfs removes disk2 from a 3-disk pool, the first close sees a
    // short-lived holder, and the second close succeeds.
    #[test]
    fn evict_present_device_retries_on_busy_then_succeeds() {
        let runner = EvictRunner {
            close_busy_then_success: true,
            ..EvictRunner::default()
        };
        let captured = crate::status_tag::testing::capture_with_color(false, || {
            let result = evict_present_device(
                &runner,
                &progress::NoopSleeper,
                "braid-disk2",
                &mp(),
                false,
                ProgressOutput::Off,
            );
            assert!(
                result.is_ok(),
                "evict should succeed after retry: {result:?}"
            );
        });

        let close_count = runner
            .invocations
            .lock()
            .unwrap()
            .iter()
            .filter(|tag| **tag == "CryptsetupClose")
            .count();
        assert_eq!(close_count, 2);
        assert!(
            captured.contains("[ok]   disk disk2: locked"),
            "missing terminal ok row after retry: {captured:?}"
        );
    }

    // ----- validate_pool_topology helper-level tests --------------------

    /// Build a planner identity snapshot for a healthy 3-disk pool
    /// {braid-disk1, braid-disk2, braid-disk3}.
    fn three_disk_expected_identities() -> BTreeMap<MapperName, DeviceIdentity> {
        let mut m = BTreeMap::new();
        for i in 1..=3u64 {
            m.insert(
                MapperName(format!("braid-disk{i}")),
                DeviceIdentity {
                    devid: i,
                    luks_uuid: LuksUuid::parse(&format!(
                        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                        i as u32, i as u16, i as u16, i as u16, i
                    ))
                    .expect("valid fixture UUID"),
                },
            );
        }
        m
    }

    /// Custom runner that emits a configurable `btrfs filesystem show`
    /// output and the matching `cryptsetup status` / `cryptsetup luksUUID`
    /// answers so probe_pool returns the topology under test.
    #[derive(Clone)]
    struct ValidateProbeRunner {
        show_stdout: String,
        // mapper -> underlying device path (or "(null)" for null-underlying)
        statuses: std::collections::HashMap<String, String>,
        // device path -> luks_uuid
        luks_uuids: std::collections::HashMap<String, String>,
    }

    impl CommandRunner for ValidateProbeRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsFilesystemShow { .. } => Ok(RawCommandOutput {
                    cmd: String::new(),
                    stdout: self.show_stdout.clone(),
                    stderr: String::new(),
                    exit_status: 0,
                }),
                CmdRequest::CryptsetupStatus { mapper } => {
                    let device = self
                        .statuses
                        .get(mapper)
                        .cloned()
                        .unwrap_or_else(|| format!("/dev/sd{mapper}"));
                    Ok(RawCommandOutput {
                        cmd: String::new(),
                        stdout: format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {device}\n  mode:    read/write\n"
                        ),
                        stderr: String::new(),
                        exit_status: 0,
                    })
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = self
                        .luks_uuids
                        .get(device)
                        .cloned()
                        .unwrap_or_else(|| "00000000-0000-0000-0000-000000000000".into());
                    Ok(RawCommandOutput {
                        cmd: String::new(),
                        stdout: format!("{uuid}\n"),
                        stderr: String::new(),
                        exit_status: 0,
                    })
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

    fn make_show(rows: &[(u64, &str)]) -> String {
        let mut s = String::from("Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n");
        s.push_str(&format!(
            "\tTotal devices {} FS bytes used 16.17MiB\n",
            rows.len()
        ));
        for (devid, mapper) in rows {
            s.push_str(&format!(
                "\tdevid    {devid} size 496.00MiB used 121.56MiB path /dev/mapper/{mapper}\n"
            ));
        }
        s
    }

    fn three_disk_runner() -> ValidateProbeRunner {
        let show = make_show(&[(1, "braid-disk1"), (2, "braid-disk2"), (3, "braid-disk3")]);
        let mut statuses = std::collections::HashMap::new();
        let mut luks_uuids = std::collections::HashMap::new();
        for i in 1..=3u64 {
            let mapper = format!("braid-disk{i}");
            let underlying = format!("/dev/sd{i}");
            statuses.insert(mapper.clone(), underlying.clone());
            luks_uuids.insert(
                underlying,
                format!(
                    "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
                    i as u32, i as u16, i as u16, i as u16, i
                ),
            );
        }
        ValidateProbeRunner {
            show_stdout: show,
            statuses,
            luks_uuids,
        }
    }

    /* Intent: validate_pool_topology returns Ok when the live pool exactly
     * matches the planner's identity snapshot.
     * Why: regression guard for the happy path; a defect that flips the
     * equality direction or skips the missing-count check breaks here.
     * Scenario: a stable 3-disk pool between plan and execute.
     */
    #[test]
    fn validate_pool_topology_ok_on_exact_match() {
        let runner = three_disk_runner();
        let expected = three_disk_expected_identities();
        let result = validate_pool_topology(&runner, &RestoreFs, &mp(), "braid-disk2", &expected);
        assert!(result.is_ok(), "exact match must return Ok: {result:?}");
    }

    /* Intent: validate_pool_topology flags target-absent drift (plain).
     * Why: pre-fix, an absent target was caught by the in-helper guard;
     * the new helper must continue catching it before any mutation.
     * Scenario: between plan and execute, the target's row dropped out
     * of `btrfs filesystem show` (not null-underlying, just gone).
     */
    #[test]
    fn validate_pool_topology_target_absent_plain() {
        let show = make_show(&[(1, "braid-disk1"), (3, "braid-disk3")]);
        let mut runner = three_disk_runner();
        runner.show_stdout = show;
        let expected = three_disk_expected_identities();
        let drift = validate_pool_topology(&runner, &RestoreFs, &mp(), "braid-disk2", &expected)
            .expect_err("target absence must surface drift");
        assert!(!drift.target_present);
        assert!(!drift.target_null_underlying);
        assert!(!drift.is_target_hot_unplug());
        assert!(drift.detail().contains("braid-disk2"));
    }

    /* Intent: validate_pool_topology classifies target hot-unplug.
     * Why: replaces the old in-helper classifier so callers can route
     * to the richer remediation without re-probing.
     * Scenario: the target's underlying SCSI device hot-unplugged,
     * leaving cryptsetup reporting `device: (null)`.
     */
    #[test]
    fn validate_pool_topology_target_hot_unplug() {
        // The mapper still appears in `btrfs filesystem show` (the dm-crypt
        // node is open) but cryptsetup reports `device: (null)`. probe_pool
        // sorts it into null_underlying and excludes it from devices.
        let mut runner = three_disk_runner();
        runner
            .statuses
            .insert("braid-disk2".into(), "(null)".into());
        let expected = three_disk_expected_identities();
        let drift = validate_pool_topology(&runner, &RestoreFs, &mp(), "braid-disk2", &expected)
            .expect_err("null-underlying must surface drift");
        assert!(drift.is_target_hot_unplug(), "must classify as hot-unplug");
        let detail = drift.detail();
        assert!(
            detail.contains("null-underlying") || detail.contains("device: (null)"),
            "detail must mention null-underlying / hot-unplug: {detail}"
        );
    }

    /* Intent: validate_pool_topology flags non-target MISSING drift.
     * Why: a non-target disk going MISSING would otherwise pass the old
     * cardinality-only check; the new helper rejects it before mutation.
     * Scenario: a third disk goes MISSING while the target is still
     * present, between plan and execute.
     */
    #[test]
    fn validate_pool_topology_non_target_missing() {
        // Drop disk3 from `btrfs filesystem show` and replace it with a
        // MISSING sentinel row. probe_pool reports it as missing_count > 0.
        let mut s = String::from("Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n");
        s.push_str("\tTotal devices 3 FS bytes used 16.17MiB\n");
        s.push_str("\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n");
        s.push_str("\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n");
        s.push_str("\t*** Some devices missing\n");
        let mut runner = three_disk_runner();
        runner.show_stdout = s;
        let expected = three_disk_expected_identities();
        let drift = validate_pool_topology(&runner, &RestoreFs, &mp(), "braid-disk2", &expected)
            .expect_err("non-target MISSING must surface drift");
        assert!(drift.target_present, "target must still be present");
        assert!(drift.observed_missing_count >= 1);
        let detail = drift.detail();
        assert!(detail.contains("MISSING") || detail.contains("missing"));
    }

    /* Intent: validate_pool_topology flags an extra mapper (4th disk
     * auto-unlocked between plan and execute).
     * Why: BTreeMap equality flips on the extra key; the helper must
     * surface that the observed set is larger than expected.
     * Scenario: a fourth disk auto-unlocked between plan and execute,
     * flipping a 3->2 plan into an unwanted balance against a 4-disk pool.
     */
    #[test]
    fn validate_pool_topology_device_count_grew() {
        let show = make_show(&[
            (1, "braid-disk1"),
            (2, "braid-disk2"),
            (3, "braid-disk3"),
            (4, "braid-disk4"),
        ]);
        let mut runner = three_disk_runner();
        runner.show_stdout = show;
        runner
            .statuses
            .insert("braid-disk4".into(), "/dev/sd4".into());
        runner.luks_uuids.insert(
            "/dev/sd4".into(),
            "44444444-4444-4444-4444-444444444444".into(),
        );
        let expected = three_disk_expected_identities();
        let drift = validate_pool_topology(&runner, &RestoreFs, &mp(), "braid-disk2", &expected)
            .expect_err("extra disk must surface drift");
        assert!(drift.target_present);
        assert_eq!(drift.observed_missing_count, 0);
        assert!(drift.detail().contains("braid-disk4"));
    }

    /* Intent: validate_pool_topology flags same-mapper identity drift
     * (devid or luks_uuid differs for the same mapper name).
     * Why: a BTreeSet<MapperName> comparison would have missed this;
     * BTreeMap<MapperName, DeviceIdentity> catches it on value equality.
     * Scenario: operator ran `cryptsetup close` + `cryptsetup open` on a
     * different LUKS device under the same braid- mapper name between
     * plan and execute, flipping devid and luks_uuid for that mapper.
     */
    #[test]
    fn validate_pool_topology_same_mapper_replacement() {
        // disk3's devid is now 99 with a different luks_uuid.
        let show = make_show(&[(1, "braid-disk1"), (2, "braid-disk2"), (99, "braid-disk3")]);
        let mut runner = three_disk_runner();
        runner.show_stdout = show;
        runner.luks_uuids.insert(
            "/dev/sd3".into(),
            "deadbeef-dead-beef-dead-beefdeadbeef".into(),
        );
        let expected = three_disk_expected_identities();
        let drift = validate_pool_topology(&runner, &RestoreFs, &mp(), "braid-disk2", &expected)
            .expect_err("same-mapper replacement must surface drift");
        assert!(drift.target_present);
        let detail = drift.detail();
        assert!(detail.contains("braid-disk3"), "detail: {detail}");
        assert!(
            detail.contains("identity changed") || detail.contains("devid"),
            "detail must signal identity drift: {detail}"
        );
    }

    /* Intent: validate_pool_topology flags same-count survivor swap
     * (one mapper added, one removed between plan and execute).
     * Why: a cardinality-only check would have missed this; the helper
     * must detect mapper-set drift even when count stays the same.
     * Scenario: disk3 was unplugged AND disk4 plugged in (and auto-
     * unlocked) between plan and execute -- 3 mappers, both not the
     * target, but the planner's identity map is now wrong.
     */
    #[test]
    fn validate_pool_topology_same_count_swap() {
        let show = make_show(&[(1, "braid-disk1"), (2, "braid-disk2"), (4, "braid-disk4")]);
        let mut runner = three_disk_runner();
        runner.show_stdout = show;
        runner
            .statuses
            .insert("braid-disk4".into(), "/dev/sd4".into());
        runner.luks_uuids.insert(
            "/dev/sd4".into(),
            "44444444-4444-4444-4444-444444444444".into(),
        );
        let expected = three_disk_expected_identities();
        let drift = validate_pool_topology(&runner, &RestoreFs, &mp(), "braid-disk2", &expected)
            .expect_err("same-count swap must surface drift");
        let detail = drift.detail();
        assert!(
            detail.contains("braid-disk3"),
            "missing key in detail: {detail}"
        );
        assert!(
            detail.contains("braid-disk4"),
            "added key in detail: {detail}"
        );
    }
}
