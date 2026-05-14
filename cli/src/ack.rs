use crate::alert::{
    self, AlertCause, load_acked_stats_fallible, save_acked_stats, snapshot_current,
};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_btrfs_device_stats;
use crate::probe::{Filesystem, ProbeError, probe_pool_alerts};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;

pub fn cmd_ack<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> Result<(), AckError> {
    cmd_ack_impl(runner, fs, mount_point, paths, &stop_beeper)
}

fn cmd_ack_impl<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
) -> Result<(), AckError> {
    // Snapshot the gating inputs (alert latch + smartd flag) before probing
    // the pool. Both feed the "is there an alert?" decision and the
    // snapshot-scoped cleanup decision. probe_pool_alerts still has per-disk
    // shell-outs, so the asynchronous smartd hook can fire during it; reading
    // smartd after the probe would let a hook firing during the probe either
    // flip an empty-latch gate or get swallowed by cleanup. An
    // unreadable latch counts as active for gating so the user can clear a
    // corrupt file even with the pool offline.
    let (latch_state, latch_corrupt) = match alert::load_alert_latch(paths) {
        Ok(Some(s)) => (Some(s), false),
        Ok(None) => (None, false),
        Err(e) => {
            eprintln!("warning: alert latch unreadable -- treating as active for ack gating: {e}");
            (None, true)
        }
    };
    let causes: &[AlertCause] = latch_state
        .as_ref()
        .map(|s| s.causes.as_slice())
        .unwrap_or(&[]);
    let smartd_active = alert::smartd_alert_active(paths);
    let latch_had_smartd = causes.iter().any(|c| matches!(c, AlertCause::SmartdAlert));
    let remove_smartd = smartd_active || latch_had_smartd;

    // 2. Check if pool is mounted
    let pool = match probe_pool_alerts(runner, fs, mount_point) {
        Ok(p) => p,
        Err(e) => return Err(AckError::Probe(e)),
    };

    if !pool.mounted {
        return ack_offline(
            causes,
            latch_corrupt,
            smartd_active,
            remove_smartd,
            paths,
            stop_beeper,
        );
    }

    if causes.is_empty() && !smartd_active && !latch_corrupt {
        println!("no active alerts");
        return Ok(());
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

    if let Err(e) = cleanup_alert_files_and_beeper(paths, stop_beeper, remove_smartd) {
        return Err(AckError::CleanupFailed(e));
    }

    // 8. Print a count for latched causes. Smartd-only and corrupt-latch
    // gated acknowledgments still completed real cleanup, but have no
    // meaningful latch count to report.
    if !causes.is_empty() {
        println!(
            "acknowledged {} alert{}",
            causes.len(),
            if causes.len() == 1 { "" } else { "s" }
        );
    } else {
        println!("acknowledged current alerts");
    }

    Ok(())
}

fn ack_offline(
    causes: &[AlertCause],
    latch_corrupt: bool,
    smartd_active: bool,
    remove_smartd: bool,
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
) -> Result<(), AckError> {
    let has_alert = !causes.is_empty() || smartd_active || latch_corrupt;
    if !has_alert {
        return Err(AckError::PoolNotMounted);
    }

    // Refuse if the latch contains any BtrfsDeviceErrors cause: the counter
    // baseline that suppresses re-firing requires live `btrfs device stats`
    // output, which we cannot produce with the pool offline. Refusing the
    // *whole* ack (rather than partial-acking other causes) avoids leaving
    // the user in an ambiguous "I acked but it still says ALERT" state.
    if causes
        .iter()
        .any(|c| matches!(c, AlertCause::BtrfsDeviceErrors { .. }))
    {
        return Err(AckError::OfflineBtrfsErrorsRefused);
    }

    // Apply latched MissingDevice causes to acked-stats. Only touch the file
    // when at least one MissingDevice cause exists -- a parseable latch with
    // only SmartdAlert / ComputationError causes does not need ack-state
    // updates, and coupling them would let an unrelated corrupt acked-stats
    // file fail an otherwise-fine offline ack.
    let missing_devids: Vec<u64> = causes
        .iter()
        .filter_map(|c| match c {
            AlertCause::MissingDevice { devid } => Some(*devid),
            _ => None,
        })
        .collect();

    if !missing_devids.is_empty() {
        let mut acked = load_acked_stats_fallible(paths)?;
        for devid in missing_devids {
            acked.0.entry(devid.to_string()).or_default().missing_acked = true;
        }
        save_acked_stats(&acked, paths)?;
    }

    if let Err(e) = cleanup_alert_files_and_beeper(paths, stop_beeper, remove_smartd) {
        return Err(AckError::CleanupFailed(e));
    }
    println!("acknowledged current alerts");
    Ok(())
}

/// Cleanup of all alert-side files plus the beeper unit, used by both the
/// mounted and offline branches of `cmd_ack_impl`.
///
/// Each `remove_*` call is NotFound-tolerant, so a missing file is not an
/// error. A real I/O error on any `remove_*` short-circuits via `?`:
/// subsequent removals and the `stop_beeper` invocation are skipped, and the
/// error is propagated.
///
/// `cmd_ack_impl` derives `remove_smartd` once from inputs snapshotted at
/// entry. Cleanup deletes the smartd flag only when the snapshot already
/// represented an active smartd source: the flag was present at entry, or the
/// latch carried a `SmartdAlert` cause. A flag that arrives after a snapshot
/// with neither condition is left for the next monitor cycle.
///
/// The `stop_beeper` parameter is the injected `&dyn Fn()` from
/// `cmd_ack_impl`; callers must forward their own hook so tests can record
/// beeper invocations.
fn cleanup_alert_files_and_beeper(
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
    remove_smartd: bool,
) -> Result<(), std::io::Error> {
    if remove_smartd {
        alert::remove_smartd_alert_flag(paths)?;
    }
    alert::remove_alert_latch(paths)?;
    alert::remove_alert_latch_corrupt(paths)?;
    stop_beeper();
    Ok(())
}

#[cfg(not(test))]
fn stop_beeper() {
    let result = std::process::Command::new("systemctl")
        .args(["stop", "braid-alert.service"])
        .output();
    match result {
        Err(e) => {
            eprintln!("warning: could not stop braid-alert.service: {e}");
        }
        Ok(output) => {
            if let Some(msg) = format_systemctl_stop_failure(&output) {
                eprintln!("{msg}");
            }
        }
    }
}

fn format_systemctl_stop_failure(output: &std::process::Output) -> Option<String> {
    if output.status.success() {
        return None;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        Some(format!(
            "warning: systemctl stop braid-alert.service: {}",
            output.status
        ))
    } else {
        Some(format!(
            "warning: systemctl stop braid-alert.service: {}: {stderr}",
            output.status
        ))
    }
}

// Unit tests exercise cmd_ack end-to-end; without this gate they would shell
// out to a real `systemctl` on the host running `cargo test`. Keep the
// production behavior behind cfg(not(test)) and make the test build a no-op.
#[cfg(test)]
fn stop_beeper() {}

#[derive(Debug, thiserror::Error)]
pub enum AckError {
    #[error("pool is not mounted -- nothing to acknowledge")]
    PoolNotMounted,
    #[error("cannot ack btrfs device errors while pool is offline -- unlock the pool first")]
    OfflineBtrfsErrorsRefused,
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
    #[error("parse error: {0}")]
    Parse(#[from] crate::parse::ParseError),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Cleanup of latch + smartd-alert + corrupt-latch and the beeper hook
    /// failed after ack had already started persisting state: after
    /// `save_acked_stats` in the mounted path, after offline missing-device
    /// ack state was persisted, or after one cleanup file was already
    /// removed. Re-running `braid ack` after fixing the I/O issue is
    /// idempotent.
    #[error(
        "alert state cleanup failed -- some files may be in a partial state; \
         fix the I/O error and re-run `braid ack`: {0}"
    )]
    CleanupFailed(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{AckedDeviceCounters, AckedDisk, AckedStats, load_acked_stats};
    use crate::test_fixtures::{
        AckPanicRunner, ack_fs_btrfs, ack_fs_ext4, ack_fs_not_mounted,
        ack_mounted_fs_that_touches_smartd, ack_mounted_probe_runner,
        ack_mounted_probe_runner_with_device_stats, ack_mp, ack_offline_fs_that_touches_smartd,
        ack_write_latch, isolated_paths,
    };
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(unix)]
    use std::process::{ExitStatus, Output};

    /*
     * Intent: Mounted ack with no latched alert, no smartd flag, and no
     * corrupt latch is a true no-op: it does not query btrfs device stats
     * or rewrite acked-stats.json.
     * Why it exists: A proactive ack on a healthy pool used to snapshot
     * current btrfs counters as the new baseline, which could bury errors
     * that appeared after the last monitor cycle but before ack.
     * Scenario: the pool is mounted and healthy; the user runs `braid ack`
     * "for good measure" before monitor has latched any alert.
     */
    #[test]
    fn cmd_ack_noop_when_no_alerts_does_not_query_btrfs_or_write_acked_stats() {
        let (_dir, paths) = isolated_paths();
        let runner = ack_mounted_probe_runner();

        let result = cmd_ack(&runner, &ack_fs_btrfs(), &ack_mp(), &paths);

        assert!(result.is_ok(), "no-op ack should succeed, got {result:?}");
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceStatsJson { .. })),
            "no-op ack must not query btrfs device stats"
        );
        assert!(!paths.acked_stats_json().exists());
    }

    /*
     * Intent: Mounted ack still runs the full ack path when
     * alert-latch.json is corrupt, even though the parsed latch count is
     * zero.
     * Why it exists: A guard that checks only latch_count and smartd state
     * would return early on corrupt latch bytes, leaving the corrupt file
     * uncleared and violating the corrupt-latch recovery contract.
     * Scenario: external tampering or filesystem damage leaves a corrupt
     * latch while the pool is mounted; `braid ack` must clear it.
     */
    #[test]
    fn cmd_ack_with_mounted_pool_and_corrupt_latch_runs_full_ack_path() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.alert_latch_json(), b"not json").unwrap();
        let runner = ack_mounted_probe_runner_with_device_stats();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);

        assert!(
            result.is_ok(),
            "corrupt-latch ack should succeed, got {result:?}"
        );
        assert!(
            runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceStatsJson { .. })),
            "corrupt-latch ack must run the full ack path"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "corrupt latch must be removed"
        );
        assert!(
            paths.acked_stats_json().exists(),
            "snapshot must have been saved"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire once on mounted corrupt-latch ack"
        );
    }

    /*
     * Intent: Mounted ack still runs the full ack path when only the
     * smartd alert flag exists and monitor has not latched it yet.
     * Why it exists: The smartd hook can fire between monitor cycles. A
     * guard that ignores the flag would return early, leaving the flag in
     * place and failing to silence the alert source.
     * Scenario: smartd writes /var/lib/braid/smartd-alert; before monitor
     * runs, the user runs `braid ack` on a mounted pool.
     */
    #[test]
    fn cmd_ack_with_mounted_pool_and_smartd_flag_no_latch_runs_full_ack_path() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.smartd_alert(), b"").unwrap();
        let runner = ack_mounted_probe_runner_with_device_stats();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);

        assert!(
            result.is_ok(),
            "smartd-only ack should succeed, got {result:?}"
        );
        assert!(
            runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceStatsJson { .. })),
            "smartd-only ack must run the full ack path"
        );
        assert!(
            !paths.smartd_alert().exists(),
            "smartd flag must be removed"
        );
        assert!(
            paths.acked_stats_json().exists(),
            "snapshot must have been saved"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire once on mounted smartd-only ack"
        );
    }

    // Intent: Offline ack does not let a smartd flag written during probing
    // turn an empty entry snapshot into an acknowledged alert.
    // Why it exists: The smartd hook is not under the pool lock, so it can
    // fire while probe_pool_alerts is reading mountinfo. A post-probe gate read
    // would consume that new flag and hide it from the next monitor cycle.
    // Scenario: pool is offline and there are no alerts at ack entry, but
    // smartd writes the flag while ack is probing the mount point.
    #[test]
    fn ack_offline_does_not_consume_smartd_flag_arriving_during_probe() {
        let (_dir, paths) = isolated_paths();
        let fs = ack_offline_fs_that_touches_smartd(&paths);

        let result = cmd_ack(&AckPanicRunner, &fs, &ack_mp(), &paths);

        assert!(
            matches!(result, Err(AckError::PoolNotMounted)),
            "empty offline snapshot must refuse, got {result:?}"
        );
        assert!(
            paths.smartd_alert().exists(),
            "late smartd flag must remain for the next monitor cycle"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "ack must not create a latch"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "empty offline ack must not create acked-stats"
        );
    }

    // Intent: Mounted no-op ack does not let a smartd flag written during
    // probing turn an empty entry snapshot into a full ack path.
    // Why it exists: Reading smartd after probe_pool_alerts would make the
    // no-alert gate observe the late flag, query btrfs device stats, and then
    // delete the flag before monitor could latch it.
    // Scenario: pool is mounted and healthy; there are no alerts at ack
    // entry, but smartd writes the flag while ack is probing the pool.
    #[test]
    fn cmd_ack_mounted_does_not_consume_smartd_flag_arriving_during_probe() {
        let (_dir, paths) = isolated_paths();
        let fs = ack_mounted_fs_that_touches_smartd(&paths);
        let runner = ack_mounted_probe_runner();

        let result = cmd_ack(&runner, &fs, &ack_mp(), &paths);

        assert!(
            result.is_ok(),
            "no-op mounted ack should succeed, got {result:?}"
        );
        assert!(
            paths.smartd_alert().exists(),
            "late smartd flag must remain for the next monitor cycle"
        );
        assert!(
            !runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceStatsJson { .. })),
            "empty entry snapshot must not run the full ack path"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "empty entry snapshot must not write acked-stats"
        );
    }

    // Intent: Mounted cleanup preserves a smartd flag written during probing
    // when the entry snapshot only had a non-smartd latched cause.
    // Why it exists: Cleanup used to remove smartd-alert unconditionally,
    // which swallowed a late smartd alert even though this ack was only
    // acknowledging a btrfs device error.
    // Scenario: monitor latched BtrfsDeviceErrors, smartd has not fired at
    // ack entry, and then smartd writes the flag during the mounted probe.
    #[test]
    fn cmd_ack_mounted_with_btrfs_errors_preserves_mid_probe_smartd_flag() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::BtrfsDeviceErrors { devid: 1 }]);
        let fs = ack_mounted_fs_that_touches_smartd(&paths);
        let runner = ack_mounted_probe_runner_with_device_stats();

        let result = cmd_ack(&runner, &fs, &ack_mp(), &paths);

        assert!(
            result.is_ok(),
            "btrfs-error ack should succeed, got {result:?}"
        );
        assert!(!paths.alert_latch_json().exists(), "latch must be removed");
        assert!(
            paths.acked_stats_json().exists(),
            "mounted ack must persist a fresh baseline"
        );
        assert!(
            paths.smartd_alert().exists(),
            "late smartd flag must remain for the next monitor cycle"
        );
    }

    // Intent: Offline cleanup preserves a smartd flag written during probing
    // when the entry snapshot only had a non-smartd latched cause.
    // Why it exists: Offline ack has a separate cleanup call site from the
    // mounted path; both must honor the same snapshot-scoped smartd rule.
    // Scenario: monitor latched MissingDevice, smartd has not fired at ack
    // entry, and then smartd writes the flag while ack confirms the pool is
    // offline.
    #[test]
    fn ack_offline_with_missing_device_preserves_mid_probe_smartd_flag() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::MissingDevice { devid: 2 }]);
        let fs = ack_offline_fs_that_touches_smartd(&paths);

        let result = cmd_ack(&AckPanicRunner, &fs, &ack_mp(), &paths);

        assert!(
            result.is_ok(),
            "offline missing-device ack should succeed, got {result:?}"
        );
        assert!(!paths.alert_latch_json().exists(), "latch must be removed");
        let acked = load_acked_stats(&paths);
        let entry = acked.0.get("2").expect("devid 2 entry must be present");
        assert!(entry.missing_acked);
        assert!(
            paths.smartd_alert().exists(),
            "late smartd flag must remain for the next monitor cycle"
        );
    }

    // Intent: Offline cleanup removes a smartd flag written during probing
    // when the entry snapshot already had a latched SmartdAlert.
    // Why it exists: The crash-recovery exception treats a latched
    // SmartdAlert as an active smartd source even if the flag was absent at
    // entry, so this branch must not regress to `remove_smartd = smartd_active`.
    // Scenario: a prior monitor cycle latched SmartdAlert, the flag is absent
    // at ack entry, and the smartd hook writes it again during the offline
    // probe.
    #[test]
    fn ack_offline_with_smartd_latch_cleans_mid_probe_smartd_flag() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::SmartdAlert]);
        let fs = ack_offline_fs_that_touches_smartd(&paths);

        let result = cmd_ack(&AckPanicRunner, &fs, &ack_mp(), &paths);

        assert!(
            result.is_ok(),
            "offline smartd-latch ack should succeed, got {result:?}"
        );
        assert!(!paths.alert_latch_json().exists(), "latch must be removed");
        assert!(
            !paths.smartd_alert().exists(),
            "latched SmartdAlert cleanup must remove the mid-probe flag"
        );
    }

    // Intent: Mounted cleanup removes a smartd flag written during probing
    // when the entry snapshot already had a latched SmartdAlert.
    // Why it exists: The mounted branch must apply the shared entry-snapshotted
    // SmartdAlert cleanup decision instead of regressing to `remove_smartd =
    // smartd_active`.
    // Scenario: a prior monitor cycle latched SmartdAlert, the flag is absent
    // at ack entry, and the smartd hook writes it again during the mounted
    // probe.
    #[test]
    fn cmd_ack_mounted_with_smartd_latch_cleans_mid_probe_smartd_flag() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::SmartdAlert]);
        let fs = ack_mounted_fs_that_touches_smartd(&paths);
        let runner = ack_mounted_probe_runner_with_device_stats();

        let result = cmd_ack(&runner, &fs, &ack_mp(), &paths);

        assert!(
            result.is_ok(),
            "mounted smartd-latch ack should succeed, got {result:?}"
        );
        assert!(!paths.alert_latch_json().exists(), "latch must be removed");
        assert!(
            paths.acked_stats_json().exists(),
            "mounted ack must persist a fresh baseline"
        );
        assert!(
            !paths.smartd_alert().exists(),
            "latched SmartdAlert cleanup must remove the mid-probe flag"
        );
    }

    // Intent: Mounted ack with a parseable latch whose only cause is
    // ComputationError runs the full ack path -- btrfs device stats are
    // queried, the latch is removed, a fresh acked-stats baseline is written,
    // and the beeper hook fires exactly once.
    // Why it exists: The mounted gate at cmd_ack_impl falls through on any
    // non-empty `causes`. A future refactor that narrowed the gate to
    // "actionable" causes, or that special-cased SmartdAlert without doing the
    // same for ComputationError, would silently no-op this mounted ack and
    // leave the latch on disk with the beeper still running. The offline
    // equivalent does not catch this because the offline branch has a different
    // gate. The beeper-call assertion additionally pins the cleanup hook on the
    // mounted success path, which was previously only covered for NotBtrfs and
    // offline success.
    // Scenario: monitor latched a ComputationError on a prior cycle, such as a
    // transient probe failure. The pool is now mounted and healthy; the
    // operator runs `braid ack`. The latch must be cleared, a fresh baseline
    // persisted, and the beeper silenced.
    #[test]
    fn cmd_ack_with_mounted_pool_and_computation_error_only_latch_runs_full_ack_path() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::ComputationError {
                detail: "test".to_owned(),
            }],
        );
        let runner = ack_mounted_probe_runner_with_device_stats();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);

        assert!(
            result.is_ok(),
            "computation-error-only ack should succeed, got {result:?}"
        );
        assert!(
            runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceStatsJson { .. })),
            "computation-error-only ack must run the full ack path"
        );
        assert!(!paths.alert_latch_json().exists(), "latch must be removed");
        assert!(
            paths.acked_stats_json().exists(),
            "mounted ack must persist a fresh baseline"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire once on mounted-ack success"
        );
    }

    /*
     * Intent: When cmd_ack succeeds at save_acked_stats but
     * cleanup_alert_files_and_beeper fails, the user-visible error names
     * the partial state and points at the recovery path. The new baseline
     * is durable on disk and the corrupt sidecar remains -- the witnesses
     * that distinguish CleanupFailed from a generic AckError::Io.
     * Why it exists: Without the dedicated variant, a cleanup-phase I/O
     * error surfaces as "I/O error: <kind>" with no hint that re-running
     * ack will eventually clear the latch. The user observes "alert
     * latched but no live cause" on the next monitor cycle and has no
     * signpost to recovery.
     * Scenario: a directory sits at the corrupt-latch sidecar path (manual
     * tampering, leftover from a previous bug, or permission drift), so
     * remove_file fails with EISDIR/EPERM. The latch carried
     * BtrfsDeviceErrors. Mounted pool, healthy device stats. cmd_ack must
     * save the new baseline, fail cleanup, and return the dedicated
     * variant.
     */
    #[test]
    fn cmd_ack_returns_cleanup_failed_when_corrupt_latch_cleanup_errors_after_baseline_saved() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::BtrfsDeviceErrors { devid: 1 }]);
        // remove_file on a directory returns EISDIR (Linux) / EPERM (macOS)
        // -- a platform-portable non-NotFound io::Error from
        // remove_alert_latch_corrupt.
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

        let runner = ack_mounted_probe_runner_with_device_stats();
        let err = cmd_ack(&runner, &ack_fs_btrfs(), &ack_mp(), &paths)
            .expect_err("cleanup failure must propagate");

        assert!(
            matches!(err, AckError::CleanupFailed(_)),
            "expected AckError::CleanupFailed, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("partial state") && msg.contains("re-run `braid ack`"),
            "message must name partial state and recovery path, got: {msg}"
        );

        // Witnesses for why CleanupFailed is distinct from AckError::Io.
        // Starting from acked-stats absent makes the partial apply visible:
        // the file appears before cleanup fails.
        assert!(
            paths.acked_stats_json().exists(),
            "save_acked_stats runs before cleanup -- baseline must be durable"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "cleanup must remove the latch before failing on the corrupt sidecar"
        );
        assert!(
            paths.alert_latch_corrupt().exists(),
            "cleanup poison directory must remain and prove where cleanup failed"
        );
    }

    /*
     * Intent: cmd_ack must surface ProbeError::NotBtrfs to the caller and
     * leave latched alert state intact when an alert is already on disk.
     * Why it exists: Prior behavior silently deleted the latch + smartd flag,
     * mutated acked-stats for any latched MissingDevice cause, and printed
     * "acknowledged current alerts" for a state that is not actually offline.
     * Pins the regression guard for the with-alerts case.
     * Scenario: operator left an ext4 partition mounted at /mnt/storage. A
     * pre-existing alert latch and smartd flag are on disk. Running
     * `braid ack` must error out without touching alert-latch.json,
     * smartd-alert, or acked-stats.json.
     */
    #[test]
    fn cmd_ack_with_foreign_fstype_and_alerts_returns_probe_error_and_preserves_state() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::MissingDevice { devid: 2 }]);
        let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();
        std::fs::write(paths.smartd_alert(), b"").unwrap();

        let err = cmd_ack(&AckPanicRunner, &ack_fs_ext4(), &ack_mp(), &paths)
            .expect_err("must refuse -- mount is not btrfs");

        match &err {
            AckError::Probe(ProbeError::NotBtrfs { fstype, .. }) => {
                assert_eq!(fstype.as_str(), "ext4");
            }
            other => panic!("expected AckError::Probe(NotBtrfs), got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("not btrfs") && msg.contains("ext4"),
            "user-visible message must name fstype, got: {msg}"
        );

        assert_eq!(
            std::fs::read(paths.alert_latch_json()).unwrap(),
            original_latch_bytes,
            "latch bytes must be preserved"
        );
        assert!(
            paths.smartd_alert().exists(),
            "smartd flag must be preserved"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "acked-stats must not be created from a NotBtrfs path"
        );
    }

    /*
     * Intent: With no pre-existing alerts, NotBtrfs must surface the real
     * condition rather than AckError::PoolNotMounted.
     * Why it exists: Prior behavior returned "pool is not mounted -- nothing
     * to acknowledge", a lie. Pins the no-alert branch so it cannot regress
     * independently of the with-alerts branch.
     * Scenario: clean state directory, but the mount target holds ext4.
     * `braid ack` must report the fstype, not claim the pool is unmounted,
     * and must not create any alert files.
     */
    #[test]
    fn cmd_ack_with_foreign_fstype_and_no_alerts_returns_probe_error() {
        let (_dir, paths) = isolated_paths();

        let err = cmd_ack(&AckPanicRunner, &ack_fs_ext4(), &ack_mp(), &paths)
            .expect_err("must refuse -- mount is not btrfs");

        match &err {
            AckError::Probe(ProbeError::NotBtrfs { fstype, .. }) => {
                assert_eq!(fstype.as_str(), "ext4");
            }
            other => panic!("expected AckError::Probe(NotBtrfs), got: {other:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("not btrfs") && msg.contains("ext4"),
            "user-visible message must name fstype, got: {msg}"
        );

        assert!(!paths.alert_latch_json().exists(), "no latch should appear");
        assert!(
            !paths.smartd_alert().exists(),
            "no smartd flag should appear"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "no acked-stats should appear"
        );
    }

    /*
     * Intent: A corrupt alert-latch.json plus a foreign fstype still surfaces
     * ProbeError::NotBtrfs and preserves the unreadable latch bytes.
     * Why it exists: cmd_ack reads the latch before probing the pool. The
     * corrupt latch must count as active for gating, but a non-btrfs mount
     * target is not a genuine offline pool, so ack must not clean up the
     * corrupt latch on this path.
     * Scenario: alert-latch.json contains garbage bytes, and an ext4
     * filesystem is mounted at /mnt/storage. `braid ack` must report the
     * fstype mismatch and leave the corrupt file available for later ack
     * after the mount is fixed.
     */
    #[test]
    fn cmd_ack_with_foreign_fstype_and_corrupt_latch_preserves_latch_bytes() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.alert_latch_json(), b"not json").unwrap();
        let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();

        let err = cmd_ack(&AckPanicRunner, &ack_fs_ext4(), &ack_mp(), &paths)
            .expect_err("must refuse -- mount is not btrfs");

        assert!(
            matches!(err, AckError::Probe(ProbeError::NotBtrfs { .. })),
            "expected AckError::Probe(NotBtrfs), got: {err:?}"
        );
        assert_eq!(
            std::fs::read(paths.alert_latch_json()).unwrap(),
            original_latch_bytes,
            "corrupt latch bytes must be preserved on NotBtrfs"
        );
        assert!(
            !paths.alert_latch_corrupt().exists(),
            "NotBtrfs must not quarantine or clean up the latch"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "no acked-stats should appear"
        );
    }

    /*
     * Intent: The NotBtrfs error path must not invoke the beeper hook.
     * Why it exists: Prior behavior routed NotBtrfs through ack_offline,
     * whose success path stops the beeper. The public cmd_ack tests above
     * pin the user-visible error and state preservation; this hook-only test
     * pins the side-effect boundary.
     * Scenario: mount target holds ext4 and a latch exists. The
     * implementation returns Probe(NotBtrfs) before reaching any ack_offline
     * cleanup.
     */
    #[test]
    fn cmd_ack_impl_with_foreign_fstype_does_not_invoke_beeper() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::MissingDevice { devid: 2 }]);
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let err = cmd_ack_impl(&AckPanicRunner, &ack_fs_ext4(), &ack_mp(), &paths, &beeper)
            .expect_err("must refuse -- mount is not btrfs");

        assert!(
            matches!(err, AckError::Probe(ProbeError::NotBtrfs { .. })),
            "expected AckError::Probe(NotBtrfs), got: {err:?}"
        );
        assert_eq!(
            beeper_calls.get(),
            0,
            "stop_beeper must not be called on NotBtrfs"
        );
    }

    /*
     * Intent: Offline ack of a latched MissingDevice cause writes
     * missing_acked=true for that devid into acked-stats.json and removes
     * the latch.
     *
     * Why it exists: This is the regression gate for the bug where offline
     * ack only deleted the latch and never updated acked-stats, so the next
     * monitor cycle re-fired the same MissingDevice cause against an
     * unchanged baseline. Without this assertion, the bug returns silently.
     *
     * Scenario: pool was degraded with devid 2 missing; monitor latched
     * MissingDevice{devid:2}; user locked the pool and ran `braid ack`.
     * The next mounted monitor cycle must see missing_acked=true and not
     * re-fire.
     */
    #[test]
    fn ack_offline_with_missing_device_cause_marks_missing_acked() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::MissingDevice { devid: 2 }]);
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper,
        )
        .unwrap();
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire once on offline-ack success"
        );

        let acked = load_acked_stats(&paths);
        let entry = acked.0.get("2").expect("devid 2 entry must be present");
        assert!(entry.missing_acked);
        assert!(!paths.alert_latch_json().exists());
    }

    /*
     * Intent: Offline ack with a latched MissingDevice cause persists the
     * missing-device ack update to acked-stats.json before invoking
     * cleanup_alert_files_and_beeper. When cleanup then fails, the user-
     * visible error names the partial state and points at the recovery
     * path -- same contract as the mounted branch, pinned independently.
     * Why it exists: cmd_ack_impl and ack_offline have separate cleanup
     * call sites. A regression that reverts only the offline wrapping would
     * silently fall back to AckError::Io and the mounted test would still
     * pass. Pinning both branches forces both to keep returning
     * CleanupFailed.
     * Scenario: pool offline, latch contains MissingDevice{devid:1}, and a
     * directory sits at the corrupt-latch sidecar path so remove_file fails
     * with EISDIR/EPERM.
     */
    #[test]
    fn ack_offline_cleanup_failure_after_missing_acked_returns_cleanup_failed() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::MissingDevice { devid: 1 }]);
        // remove_file on a directory returns EISDIR (Linux) / EPERM (macOS)
        // -- a platform-portable non-NotFound io::Error from
        // remove_alert_latch_corrupt.
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

        let err = cmd_ack(&AckPanicRunner, &ack_fs_not_mounted(), &ack_mp(), &paths)
            .expect_err("offline cleanup failure must propagate");

        assert!(
            matches!(err, AckError::CleanupFailed(_)),
            "expected AckError::CleanupFailed, got: {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("partial state") && msg.contains("re-run `braid ack`"),
            "message must name partial state and recovery path, got: {msg}"
        );

        // Witnesses for the partial-apply state in the offline branch:
        // missing-device ack state was persisted (acked-stats exists), latch was
        // not removed (cleanup short-circuited on remove_smartd_alert_flag).
        assert!(
            paths.acked_stats_json().exists(),
            "save_acked_stats runs before cleanup -- baseline must be durable"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "cleanup must remove the latch before failing on the corrupt sidecar"
        );
        assert!(
            paths.alert_latch_corrupt().exists(),
            "cleanup poison directory must remain and prove where cleanup failed"
        );
    }

    /*
     * Intent: Offline ack refuses with OfflineBtrfsErrorsRefused when the
     * latch contains any BtrfsDeviceErrors cause, even if it also contains
     * a MissingDevice cause that would otherwise be acceptable. The latch
     * and acked-stats.json are both untouched -- all-or-nothing atomicity.
     *
     * Why it exists: Pins the all-or-nothing contract. A buggy implementation
     * that applied the MissingDevice ack update before checking for
     * BtrfsDeviceErrors would partially apply the ack, then refuse, leaving
     * the user with an inconsistent state. Starting from acked-stats.json
     * absent makes that partial-apply visible (the file would appear).
     *
     * Scenario: pool had btrfs device errors AND a missing device. User
     * locked the pool and ran `braid ack`. The user must be told to unlock
     * first; nothing should be partially applied.
     */
    #[test]
    fn ack_offline_refuses_when_btrfs_errors_mixed_with_missing() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![
                AlertCause::BtrfsDeviceErrors { devid: 1 },
                AlertCause::MissingDevice { devid: 2 },
            ],
        );
        let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();
        assert!(!paths.acked_stats_json().exists());

        let result = cmd_ack(&AckPanicRunner, &ack_fs_not_mounted(), &ack_mp(), &paths);
        assert!(
            matches!(result, Err(AckError::OfflineBtrfsErrorsRefused)),
            "expected OfflineBtrfsErrorsRefused, got {result:?}"
        );

        let after_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();
        assert_eq!(after_latch_bytes, original_latch_bytes);
        assert!(
            !paths.acked_stats_json().exists(),
            "partial apply must not create acked-stats.json"
        );
    }

    /*
     * Intent: Applying a latched MissingDevice cause to an existing
     * acked-stats entry sets missing_acked=true while preserving the
     * existing device_stats baseline.
     *
     * Why it exists: Pins the offline ack insert-or-update behavior. A
     * regression that overwrote the entry with a default device_stats would
     * silently zero out a previously-acked counter baseline, making the
     * next online monitor cycle re-alert on the same counters.
     *
     * Scenario: a previous online ack baselined devid 1 at read_io_errs=7.
     * The disk later went missing; monitor latched MissingDevice{devid:1};
     * user offline-acked. The device_stats baseline must survive.
     */
    #[test]
    fn ack_offline_preserves_existing_device_stats_baseline() {
        let (_dir, paths) = isolated_paths();
        let mut map = BTreeMap::new();
        map.insert(
            "1".to_owned(),
            AckedDisk {
                missing_acked: false,
                device_stats: AckedDeviceCounters {
                    read_io_errs: 7,
                    ..Default::default()
                },
            },
        );
        save_acked_stats(&AckedStats(map), &paths).unwrap();

        ack_write_latch(&paths, vec![AlertCause::MissingDevice { devid: 1 }]);

        cmd_ack(&AckPanicRunner, &ack_fs_not_mounted(), &ack_mp(), &paths).unwrap();

        let acked = load_acked_stats(&paths);
        let entry = acked.0.get("1").unwrap();
        assert!(entry.missing_acked);
        assert_eq!(
            entry.device_stats.read_io_errs, 7,
            "existing baseline must be preserved"
        );
    }

    /*
     * Intent: A corrupt alert-latch.json is still acked offline -- the
     * latch file is removed and ack returns Ok, so the user can recover
     * from a manually-tampered or filesystem-damaged latch even with the
     * pool locked.
     *
     * Why it exists: Regression gate for the corrupt-latch recovery path.
     * If a future edit gates ack_offline on latch parseability, the
     * operator would have no way to clear a bad file when the pool is
     * offline. acked-stats must remain absent because no causes can be
     * extracted from the corrupt file.
     *
     * Scenario: alert-latch.json contains garbage bytes (manual edit,
     * filesystem damage). Pool is offline. `braid ack` must succeed.
     */
    #[test]
    fn ack_offline_corrupt_latch_still_clears_files() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.alert_latch_json(), b"not json").unwrap();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper,
        )
        .unwrap();

        assert!(!paths.alert_latch_json().exists());
        assert!(!paths.alert_latch_corrupt().exists());
        assert!(
            !paths.acked_stats_json().exists(),
            "no MissingDevice cause means no acked-stats write"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire once on offline corrupt-latch ack"
        );
    }

    /*
     * Intent: A corrupt acked-stats.json combined with a latched
     * MissingDevice cause causes offline ack to fail with AckError::Io,
     * leaving the corrupt file byte-identical. Mutation paths must
     * fail-closed, not silently overwrite.
     *
     * Why it exists: Pins the use of load_acked_stats_fallible (not the
     * lossy load_acked_stats) on the offline-ack mutation path. The lossy
     * detector loader would treat the corrupt file as empty and silently
     * overwrite it on save -- destroying forensic evidence of the
     * corruption and possibly losing valid acks.
     *
     * Scenario: acked-stats.json was hand-edited (or filesystem-corrupted)
     * to invalid JSON. Latch contains MissingDevice{devid:1}. User runs
     * `braid ack` offline. Must fail loud.
     */
    #[test]
    fn ack_offline_corrupt_acked_stats_propagates_io_error_when_missing_cause() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.acked_stats_json(), b"not json").unwrap();
        let original_bytes = std::fs::read(paths.acked_stats_json()).unwrap();

        ack_write_latch(&paths, vec![AlertCause::MissingDevice { devid: 1 }]);

        let result = cmd_ack(&AckPanicRunner, &ack_fs_not_mounted(), &ack_mp(), &paths);
        assert!(
            matches!(result, Err(AckError::Io(_))),
            "expected AckError::Io, got {result:?}"
        );

        let after_bytes = std::fs::read(paths.acked_stats_json()).unwrap();
        assert_eq!(
            after_bytes, original_bytes,
            "corrupt acked-stats must not be silently overwritten"
        );
    }

    /*
     * Intent: A parseable latch with only SmartdAlert (no MissingDevice,
     * no BtrfsDeviceErrors) does not load acked-stats.json at all, so a
     * corrupt acked-stats file does not block offline ack of an unrelated
     * SmartdAlert.
     *
     * Why it exists: SmartdAlert is silenced by removing the smartd-alert
     * flag file -- it has no acked-stats baseline. Coupling SmartdAlert
     * acks to the acked-stats loader would let an unrelated corrupt file
     * fail an otherwise-correct ack. Pins the gate that skips the loader
     * when no MissingDevice cause is present.
     *
     * Scenario: smartd raised a SMART warning. Pool is locked. acked-stats
     * happens to be unrelated-corrupt. `braid ack` must clear the smartd
     * flag and the latch without touching acked-stats.
     */
    #[test]
    fn ack_offline_smartd_only_latch_does_not_load_acked_stats() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.acked_stats_json(), b"not json").unwrap();
        let original_bytes = std::fs::read(paths.acked_stats_json()).unwrap();

        ack_write_latch(&paths, vec![AlertCause::SmartdAlert]);
        std::fs::write(paths.smartd_alert(), b"").unwrap();

        cmd_ack(&AckPanicRunner, &ack_fs_not_mounted(), &ack_mp(), &paths).unwrap();

        assert!(!paths.smartd_alert().exists());
        assert!(!paths.alert_latch_json().exists());
        let after_bytes = std::fs::read(paths.acked_stats_json()).unwrap();
        assert_eq!(
            after_bytes, original_bytes,
            "acked-stats must be untouched when no MissingDevice cause is latched"
        );
    }

    /*
     * Intent: A parseable latch with only ComputationError (no MissingDevice,
     * no BtrfsDeviceErrors) does not load acked-stats.json -- same gate as
     * SmartdAlert, pinned independently.
     *
     * Why it exists: Both non-Missing cause types must skip the acked-stats
     * load. A regression that gated only on SmartdAlert (or only on
     * ComputationError) would slip through one cause type's coverage.
     *
     * Scenario: monitor latched a ComputationError on a prior cycle (e.g.
     * a transient probe failure). Pool is locked. acked-stats happens to
     * be unrelated-corrupt. `braid ack` must clear the latch without
     * touching acked-stats.
     */
    #[test]
    fn ack_offline_computation_error_only_latch_does_not_load_acked_stats() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.acked_stats_json(), b"not json").unwrap();
        let original_bytes = std::fs::read(paths.acked_stats_json()).unwrap();

        ack_write_latch(
            &paths,
            vec![AlertCause::ComputationError {
                detail: "test".to_owned(),
            }],
        );

        cmd_ack(&AckPanicRunner, &ack_fs_not_mounted(), &ack_mp(), &paths).unwrap();

        assert!(!paths.alert_latch_json().exists());
        let after_bytes = std::fs::read(paths.acked_stats_json()).unwrap();
        assert_eq!(after_bytes, original_bytes);
    }

    /*
     * Intent: Non-zero `systemctl stop braid-alert.service` exits produce a
     * stderr warning that includes both the process status and systemctl's
     * own diagnostic.
     * Why it exists: `Command::output()` returns Ok(Output) for non-zero
     * exits. Without explicitly checking status, `braid ack` can silently
     * leave the beeper running after a cleanup failure.
     * Scenario: braid-alert.service is not loaded in the VM, so systemctl
     * exits 5 and explains that the unit could not be stopped.
     */
    #[cfg(unix)]
    #[test]
    fn format_systemctl_stop_failure_warns_on_nonzero_exit_with_stderr() {
        let output = Output {
            status: ExitStatus::from_raw(5 << 8),
            stdout: Vec::new(),
            stderr: b"Failed to stop braid-alert.service: Unit not loaded.\n".to_vec(),
        };

        let msg = format_systemctl_stop_failure(&output)
            .expect("non-zero systemctl exit must produce a warning");

        assert!(
            msg.contains("exit status: 5"),
            "warning must include status display, got: {msg}"
        );
        assert!(
            msg.contains("Failed to stop braid-alert.service: Unit not loaded."),
            "warning must include trimmed stderr, got: {msg}"
        );
    }

    /*
     * Intent: Successful `systemctl stop braid-alert.service` exits do not
     * produce a warning.
     * Why it exists: The cleanup is best-effort; a healthy stop should keep
     * `braid ack` output focused on the ack confirmation.
     * Scenario: braid-alert.service exists and systemd accepts the stop
     * request with exit status 0.
     */
    #[cfg(unix)]
    #[test]
    fn format_systemctl_stop_failure_silent_on_zero_exit() {
        let output = Output {
            status: ExitStatus::from_raw(0),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };

        assert_eq!(format_systemctl_stop_failure(&output), None);
    }
}
