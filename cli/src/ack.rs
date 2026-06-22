use std::time::SystemTime;

use crate::alert::{
    self, AlertCause, EnospcAck, live_pool_key, load_acked_stats_fallible, save_acked_stats,
    save_enospc_ack, snapshot_current,
};
use crate::capacity::evaluate_enospc_risk;
use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::{parse_btrfs_device_stats, parse_btrfs_device_usage};
use crate::probe::{AlertPoolState, Filesystem, ProbeError, probe_pool_alerts};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;
use crate::util::detail_suffix;

/// Shared no-count ack confirmation for paths that complete real cleanup but
/// have no meaningful mounted latch count to report.
///
/// Offline ack intentionally uses this line even when the latch contains
/// causes: only mounted ack re-baselines counters, so only mounted ack reports
/// `causes.len()`.
const ACK_NO_COUNT_LINE: &str = "acknowledged current alerts";

/// Production entry point that wires ack cleanup to the real beeper stop hook.
///
/// Tests call `cmd_ack_impl` directly with explicit hooks so they never shell
/// out to host systemd while still exercising the same ack logic.
pub fn cmd_ack<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> Result<(), AckError> {
    cmd_ack_impl(runner, fs, mount_point, paths, &stop_beeper)
}

/// Injectable-hook variant used by tests -- in this module and the monitor's
/// re-ack integration test -- to exercise the real ack path without shelling out
/// to systemd.
///
/// Production goes through `cmd_ack`, which supplies the real systemd hook.
pub(crate) fn cmd_ack_impl<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
) -> Result<(), AckError> {
    // Snapshot the gating inputs (alert latch + smartd flag + cleanup-pending
    // sentinel) before probing the pool. They feed the "is there an alert?"
    // decision, the cleanup-only retry branch, and the snapshot-scoped cleanup
    // decision. probe_pool_alerts still has per-disk shell-outs, so the
    // asynchronous smartd hook can fire during it; reading smartd after the
    // probe would let a hook firing during the probe either flip an
    // empty-latch gate or get swallowed by cleanup. An unreadable latch counts
    // as active for gating so the user can clear a corrupt file even with the
    // pool offline.
    let (causes, latch_corrupt) = match alert::load_alert_latch(paths) {
        Ok(Some(s)) => (s.causes, false),
        Ok(None) => (Vec::new(), false),
        Err(e) => {
            eprintln!("warning: alert latch unreadable -- treating as active for ack gating: {e}");
            (Vec::new(), true)
        }
    };
    let smartd_active = alert::smartd_alert_active(paths);
    let scrub_failed_active = alert::scrub_failed_active(paths);
    let cleanup_pending = alert::alert_cleanup_pending(paths);
    let latch_had_smartd = causes.iter().any(|c| matches!(c, AlertCause::SmartdAlert));
    let remove_smartd = smartd_active || latch_had_smartd;
    let latch_had_scrub_failed = causes.iter().any(|c| matches!(c, AlertCause::ScrubFailed));
    let remove_scrub_failed = scrub_failed_active || latch_had_scrub_failed;

    if cleanup_pending
        && causes.is_empty()
        && !smartd_active
        && !scrub_failed_active
        && !latch_corrupt
    {
        if let Err(e) = cleanup_alert_files_and_beeper(paths, stop_beeper, false, false) {
            return Err(AckError::CleanupFailed(e));
        }
        println!("{ACK_NO_COUNT_LINE}");
        return Ok(());
    }

    // 2. Check if pool is mounted
    let pool = match probe_pool_alerts(runner, fs, mount_point) {
        Ok(p) => p,
        Err(e) => return Err(AckError::Probe(e)),
    };

    if !pool.mounted {
        return ack_offline(
            &causes,
            latch_corrupt,
            smartd_active,
            scrub_failed_active,
            remove_smartd,
            remove_scrub_failed,
            paths,
            stop_beeper,
        );
    }

    if causes.is_empty() && !smartd_active && !scrub_failed_active && !latch_corrupt {
        println!("no active alerts");
        return Ok(());
    }

    // 3. Run btrfs device stats
    let stats_raw = runner.run(&CmdRequest::BtrfsDeviceStatsJson {
        mount_point: mount_point.clone(),
    })?;
    let device_stats = parse_btrfs_device_stats(&stats_raw)?;

    // 4. Compute alert-local membership views.
    let devids = pool.alert_devids();

    // 5. Snapshot current state. Identity is the devid carried on each
    //    stats row by btrfs -- no path-to-devid map needed.
    let new_acked = snapshot_current(&device_stats, &devids);
    save_acked_stats(&new_acked, paths).map_err(AckError::Io)?;

    // 6. ENOSPC snooze: if the latch carries an EnospcRisk cause, snooze the
    //    reminder from one fresh usage probe -- when the pool is still at risk,
    //    write the live pool key + a reminder deadline one interval out. Ack
    //    snoozes the reminder, it does not resolve the risk. Best-effort: a
    //    probe/parse failure, an absent fs_uuid, or a pool that recovered by ack
    //    time clears the latch but writes no marker (a later recurrence fires armed).
    if causes
        .iter()
        .any(|c| matches!(c, AlertCause::EnospcRisk { .. }))
    {
        let missing_count = devids.missing.len() as u64;
        let now = SystemTime::now();
        write_enospc_baseline(runner, mount_point, &pool, missing_count, paths, now);
    }

    if let Err(e) =
        cleanup_alert_files_and_beeper(paths, stop_beeper, remove_smartd, remove_scrub_failed)
    {
        return Err(AckError::CleanupFailed(e));
    }

    // 8. Print a count for latched causes. Smartd-only and corrupt-latch
    // gated acknowledgments still completed real cleanup, but have no
    // meaningful latch count to report.
    println!("{}", format_ack_confirmation(causes.len()));

    Ok(())
}

/// Mounted ack confirmation builder so the user-facing count remains strictly
/// tied to latched causes, not synthesized status-only alert signals.
fn format_ack_confirmation(latched_count: usize) -> String {
    if latched_count == 0 {
        ACK_NO_COUNT_LINE.to_owned()
    } else {
        format!(
            "acknowledged {latched_count} alert{}",
            if latched_count == 1 { "" } else { "s" }
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn ack_offline(
    causes: &[AlertCause],
    latch_corrupt: bool,
    smartd_active: bool,
    scrub_failed_active: bool,
    remove_smartd: bool,
    remove_scrub_failed: bool,
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
) -> Result<(), AckError> {
    let has_alert = !causes.is_empty() || smartd_active || scrub_failed_active || latch_corrupt;
    if !has_alert {
        return Err(AckError::PoolNotMounted);
    }

    // Refuse if the latch contains any BtrfsDeviceErrors cause: the counter
    // baseline that suppresses re-firing requires live `btrfs device stats`
    // output, which we cannot produce with the pool offline. Refusing the
    // *whole* ack (rather than partial-acking other causes) avoids leaving
    // the user in an ambiguous "I acked but it still says ALERT" state.
    // ScrubFailed falls through this refusal and the MissingDevice filter
    // unchanged (no new arm), exactly as SmartdAlert does -- offline ack just
    // removes the flag and writes no acked-stats.
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
    let missing_devids: Vec<_> = causes
        .iter()
        .filter_map(|c| match c {
            AlertCause::MissingDevice { devid } => Some(*devid),
            _ => None,
        })
        .collect();

    if !missing_devids.is_empty() {
        let mut acked = load_acked_stats_fallible(paths).map_err(AckError::Io)?;
        for devid in missing_devids {
            acked.0.entry(devid.to_string()).or_default().missing_acked = true;
        }
        save_acked_stats(&acked, paths).map_err(AckError::Io)?;
    }

    if let Err(e) =
        cleanup_alert_files_and_beeper(paths, stop_beeper, remove_smartd, remove_scrub_failed)
    {
        return Err(AckError::CleanupFailed(e));
    }
    println!("{ACK_NO_COUNT_LINE}");
    Ok(())
}

/// Cleanup of all alert-side files plus the beeper unit, used by both the
/// mounted and offline branches of `cmd_ack_impl`.
///
/// Each `remove_*` call is NotFound-tolerant, so a missing file is not an
/// error. `stop_beeper` runs first so a later file-removal failure cannot
/// prevent the beeper-stop hook from being reached. A real I/O error on any
/// `remove_*` then short-circuits the remaining removals via `?` and
/// propagates the error.
///
/// The beeper stop is best-effort: production issues `systemctl stop
/// braid-alert.service`, logs a warning when spawning `systemctl` fails or it
/// exits non-zero, and returns no error to cleanup. The ordering guarantees the
/// hook is invoked on every cleanup call, not that the audible alert was
/// silenced.
///
/// `cmd_ack_impl` derives `remove_smartd` / `remove_scrub_failed` once from
/// inputs snapshotted at entry. Cleanup deletes each flag only when the snapshot
/// already represented an active source for it: the flag was present at entry,
/// or the latch carried the matching `SmartdAlert` / `ScrubFailed` cause. A flag
/// that arrives after a snapshot with neither condition is left for the next
/// monitor cycle.
///
/// Cleanup marks `alert-cleanup-pending` after `stop_beeper` and before any
/// destructive removal. The removals then run in smartd-flag, scrub-failed-flag,
/// latch, corrupt-sidecar order so ADR 014's forensic sidecar is the last
/// destructive step. The marker is cleared only after every removal succeeds. If
/// marker creation itself fails, no destructive removal has run and the original
/// entry signals still drive retry; if a later step fails, the marker remains to
/// drive the cleanup-only retry branch in `cmd_ack_impl`.
///
/// The `stop_beeper` parameter is the injected `&dyn Fn()` from
/// `cmd_ack_impl`; callers must forward their own hook so tests can record
/// beeper invocations.
fn cleanup_alert_files_and_beeper(
    paths: &StatePaths,
    stop_beeper: &dyn Fn(),
    remove_smartd: bool,
    remove_scrub_failed: bool,
) -> Result<(), std::io::Error> {
    stop_beeper();
    alert::mark_alert_cleanup_pending(paths)?;
    if remove_smartd {
        alert::remove_smartd_alert_flag(paths)?;
    }
    if remove_scrub_failed {
        alert::remove_scrub_failed_flag(paths)?;
    }
    alert::remove_alert_latch(paths)?;
    alert::remove_alert_latch_corrupt(paths)?;
    alert::clear_alert_cleanup_pending(paths)?;
    Ok(())
}

/// Snooze ENOSPC reminders from one fresh `btrfs device usage --raw` probe: when
/// the probe still sees the pool at risk, write a snooze marker (live `PoolKey`
/// plus a reminder deadline one `ENOSPC_REMINDER_INTERVAL` past `now`).
///
/// Writes no marker when the fresh probe is NOT at risk: the pool recovered
/// between fire and ack, and a snooze stamped on a not-at-risk pool would wrongly
/// suppress a recurrence inside the window (the dead-band monitor branch keeps the
/// marker). No marker -> a later recurrence fires armed.
///
/// Best-effort by contract. A usage-probe failure, a parse failure, or an absent
/// `fs_uuid` (no usable `PoolKey`) logs and writes no marker -- the same end-state
/// as an offline ack (one quiet re-fire next cycle at the non-beeping Warning
/// level, then a mounted ack snoozes it). It never fails the ack: the latch is
/// already cleared by the time this runs.
fn write_enospc_baseline<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    pool: &AlertPoolState,
    missing_count: u64,
    paths: &StatePaths,
    now: SystemTime,
) {
    let raw = match runner.run(&CmdRequest::BtrfsDeviceUsageRaw {
        mount_point: mount_point.clone(),
    }) {
        Ok(raw) => raw,
        Err(e) => {
            eprintln!(
                "warning: could not probe usage to baseline ENOSPC risk -- {e}; ack cleared the alert but wrote no baseline"
            );
            return;
        }
    };
    let entries = match parse_btrfs_device_usage(&raw) {
        Ok(parsed) => parsed.devices,
        Err(e) => {
            eprintln!(
                "warning: could not parse usage to baseline ENOSPC risk -- {e}; ack cleared the alert but wrote no baseline"
            );
            return;
        }
    };
    let Some(pool_key) = live_pool_key(pool.fs_uuid.as_deref(), &entries) else {
        eprintln!(
            "warning: no FS UUID to key the ENOSPC baseline; ack cleared the alert but wrote no baseline"
        );
        return;
    };
    let assessment = evaluate_enospc_risk(&entries, missing_count);
    // Snooze only a live risk. If the pool recovered (dead-band or past re-arm) by
    // ack time, write no marker: a snooze stamped on a not-at-risk pool would
    // wrongly suppress a recurrence inside the window, since the dead-band monitor
    // branch keeps the marker. No marker -> a later recurrence fires armed.
    if !assessment.at_risk() {
        return;
    }
    if let Err(e) = save_enospc_ack(&EnospcAck::snooze(pool_key, now), paths) {
        eprintln!(
            "warning: failed to persist ENOSPC snooze marker -- {e}; ack cleared the alert but wrote no marker"
        );
    }
}

/// Shells out directly rather than via `OnlineStateOps::systemctl_stop`
/// because the beeper stop also runs on the offline cleanup path
/// (`ack_offline`), which issues no `CommandRunner` requests.
///
/// Stops both alert units: the Critical beeper (`braid-alert.service`) and the
/// non-beeping Warning advisory (`braid-alert-advisory.service`), so one ack
/// silences whichever tier the last monitor cycle started.
fn stop_beeper() {
    stop_unit("braid-alert.service");
    stop_unit("braid-alert-advisory.service");
}

fn stop_unit(unit: &str) {
    let result = std::process::Command::new("systemctl")
        .args(["stop", unit])
        .output();
    match result {
        Err(e) => {
            eprintln!("warning: could not stop {unit}: {e}");
        }
        Ok(output) => {
            if let Some(msg) = format_systemctl_stop_failure(unit, &output) {
                eprintln!("{msg}");
            }
        }
    }
}

fn format_systemctl_stop_failure(unit: &str, output: &std::process::Output) -> Option<String> {
    if output.status.success() {
        return None;
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    Some(format!(
        "warning: systemctl stop {unit}: {}{}",
        output.status,
        detail_suffix(stderr)
    ))
}

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
    /// Pre-cleanup state-load/save I/O failure. `#[from]` is deliberately
    /// omitted so `AckError`-returning code must use `map_err(AckError::Io)`
    /// or wrap as `CleanupFailed`; a new `?` propagating an `io::Error` into
    /// `AckError` cannot silently bypass the partial-state recovery message.
    #[error("I/O error: {0}")]
    Io(#[source] std::io::Error),
    /// Cleanup of latch + smartd-alert + corrupt-latch files failed after the
    /// best-effort beeper stop hook had already been attempted and ack had
    /// already started persisting state: after `save_acked_stats` in the
    /// mounted path, after offline missing-device ack state was persisted, or
    /// after one cleanup file was already removed. Retry still has a signal:
    /// if `mark_alert_cleanup_pending` failed, no destructive removal has run
    /// and the original entry signals drive the regular ack path; if it
    /// succeeded and a later step failed, the `alert-cleanup-pending` sentinel
    /// drives the hoisted cleanup-only retry before probing or re-baselining.
    /// Re-running `braid ack` after fixing the I/O issue is idempotent because
    /// every cleanup removal is NotFound-tolerant.
    #[error(
        "alert state cleanup failed -- some files may be in a partial state; \
         fix the I/O error and re-run `braid ack`: {0}"
    )]
    CleanupFailed(#[source] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{
        AckedDeviceCounters, AckedDisk, AckedStats, ENOSPC_REMINDER_INTERVAL, load_acked_stats,
        load_enospc_ack,
    };
    use crate::monitor::{MonitorResult, cmd_monitor};
    use crate::test_fixtures::{
        ACK_DEVICE_SIZE, ACK_FS_UUID, AckPanicFilesystem, AckPanicRunner, MonitorTestRunner,
        ack_fs_btrfs, ack_fs_ext4, ack_fs_not_mounted, ack_mounted_fs_that_touches_smartd,
        ack_mounted_probe_runner, ack_mounted_probe_runner_no_uuid_with_enospc_usage,
        ack_mounted_probe_runner_with_device_stats, ack_mounted_probe_runner_with_enospc_usage,
        ack_mounted_probe_runner_with_healthy_enospc_usage,
        ack_mounted_probe_runner_with_stale_devid_stats, ack_mp, ack_noop_beeper,
        ack_offline_fs_that_touches_scrub_failed, ack_offline_fs_that_touches_smartd,
        ack_write_latch, isolated_paths, monitor_fs_btrfs, monitor_mp,
    };
    use crate::types::Devid;
    use std::collections::BTreeMap;
    #[cfg(unix)]
    use std::os::unix::process::ExitStatusExt;
    #[cfg(unix)]
    use std::process::{ExitStatus, Output};
    use std::time::UNIX_EPOCH;

    // Intent: The mounted ack confirmation formatter preserves the no-count
    //   fallback and the singular/plural counted forms.
    // Why it exists: Ack counts only latched causes; regressions that print
    //   "acknowledged 0 alerts" or drop pluralization would make the CLI
    //   contradict the documented confirmation contract.
    // Scenario: A mounted operator ack covers zero, one, or several latched
    //   causes while synthesized smartd and cleanup-pending signals remain
    //   outside the reported count.
    #[test]
    fn format_ack_confirmation_pins_count_and_pluralization() {
        assert_eq!(format_ack_confirmation(0), "acknowledged current alerts");
        assert_eq!(format_ack_confirmation(1), "acknowledged 1 alert");
        assert_eq!(format_ack_confirmation(2), "acknowledged 2 alerts");
        assert_eq!(format_ack_confirmation(3), "acknowledged 3 alerts");
    }

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

        let result = cmd_ack_impl(
            &runner,
            &ack_fs_btrfs(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );

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

    // Intent: cmd_ack must not persist an acked entry for a btrfs device-stats
    //   row whose devid is outside the pool's recognized set, even when that
    //   row carries non-zero counters. After ack, acked-stats.json contains
    //   only the recognized devids' baselines.
    // Why it exists: snapshot_current used to walk every stats row, so an
    //   unrecognized devid 99 would land in acked-stats.json. The very next
    //   monitor cycle would prune devid 99 via reconcile_acked_stats and
    //   compute_alert_state would re-latch BtrfsDeviceErrors { devid: 99 } --
    //   the loop the operator could never escape. Filtering snapshot_current
    //   by recognized_devids closes that half of the loop; this test pins it
    //   directly so an implementation that only filters compute_alert_state
    //   cannot pass.
    // Scenario: a MissingDevice alert is already latched. btrfs filesystem
    //   show reports devids 1 and 3 as the pool. btrfs device stats reports
    //   rows for devids 1, 3, and a stale /dev/mapper/braid-stale at devid 99
    //   with non-zero counters. The operator runs braid ack. ack must succeed
    //   and acked-stats.json must contain keys "1" and "3" but not "99".
    #[test]
    fn cmd_ack_does_not_persist_unrecognized_devid_in_acked_stats() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(7),
            }],
        );
        let runner = ack_mounted_probe_runner_with_stale_devid_stats();
        let beeper = || {};

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);
        assert!(result.is_ok(), "ack must succeed, got {result:?}");

        let acked = load_acked_stats(&paths);
        let keys: Vec<&str> = acked.0.keys().map(String::as_str).collect();
        assert!(
            keys.contains(&"1") && keys.contains(&"3"),
            "recognized devid baselines must be persisted, got {keys:?}"
        );
        assert!(
            !keys.contains(&"99"),
            "unrecognized devid must not be persisted, got {keys:?}"
        );
    }

    // Intent: the documented ack contract -- acking BtrfsDeviceErrors "sets the
    //   current device error counts as the new baseline so the same condition
    //   won't re-trigger" -- holds across the real cmd_monitor -> cmd_ack ->
    //   cmd_monitor wiring: monitor latches BtrfsDeviceErrors, ack snapshots the
    //   live counters as the baseline keyed by devid, the next monitor pass at
    //   the same counters stays Ok, and counters above the baseline re-fire.
    // Why it exists: snapshot -> persist -> load -> compare -> key was only
    //   covered as isolated halves (snapshot_current writes counters;
    //   compute_alert_state suppresses below a hand-built baseline). A regression
    //   where ack persists a wrong/empty baseline, keys it differently than
    //   monitor reads (devid.to_string()), or where the recognized-devid filter
    //   drops the acked row would re-fire the same disk-error alert forever, and
    //   no test would catch it. The MissingDevice round-trip
    //   (monitor-lifecycle.py) and the negative unrecognized-devid case
    //   (stale_mapper_row_with_errors_does_not_latch_or_loop) leave this positive
    //   counter-baseline path unexercised end-to-end.
    // Scenario: devid 1 reports read/corruption errors on a mounted, recognized
    //   2-disk pool; monitor latches BtrfsDeviceErrors{devid:1}. The operator
    //   runs braid ack, which baselines the live counts. The next monitor cycle
    //   at the same counts must stay silent; a later cycle with higher counts on
    //   devid 1 must alert again -- the baseline is a floor, not a permanent mute.
    #[test]
    fn ack_baseline_suppresses_then_refires_btrfs_device_errors() {
        // devid 1 has errors; devid 2 clean. Both are recognized (present in show).
        const STATS_DEVID1_ERRORS: &str = r#"{
    "__header": {"version": "1"},
    "device-stats": [
        {"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": 3, "flush_io_errs": 0, "corruption_errs": 1, "generation_errs": 0},
        {"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
    ]
}"#;
        // devid 1 strictly above the acked baseline (read_io_errs 5 > 3) for the
        // re-fire phase.
        const STATS_DEVID1_ERRORS_HIGHER: &str = r#"{
    "__header": {"version": "1"},
    "device-stats": [
        {"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": 5, "flush_io_errs": 0, "corruption_errs": 1, "generation_errs": 0},
        {"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
    ]
}"#;

        let (_dir, paths) = isolated_paths();
        let fs = monitor_fs_btrfs();
        let mp = monitor_mp();
        let runner = MonitorTestRunner::with_stats_payload(STATS_DEVID1_ERRORS);

        // Phase 1: monitor latches BtrfsDeviceErrors{devid:1}.
        let first = cmd_monitor(&runner, &fs, &mp, &paths);
        match first {
            MonitorResult::Alert(s) => assert_eq!(
                s.causes,
                vec![AlertCause::BtrfsDeviceErrors {
                    devid: Devid::new(1)
                }],
                "monitor must latch exactly the devid-1 btrfs error"
            ),
            other => panic!("expected Alert, got {other:?}"),
        }
        assert!(
            paths.alert_latch_json().exists(),
            "phase 1 must write the latch"
        );

        // Phase 2: ack snapshots the live counters as the baseline, keyed by
        // devid, for every recognized devid, and removes the latch. The value +
        // key assertions directly witness the three named regressions, so a
        // break pinpoints the layer rather than only flipping phase 3 red.
        cmd_ack_impl(&runner, &fs, &mp, &paths, &ack_noop_beeper).expect("ack ok");
        let acked = load_acked_stats(&paths);
        let d1 = acked
            .0
            .get("1")
            .expect("recognized devid 1 baseline persisted");
        assert_eq!(
            d1.device_stats.read_io_errs, 3,
            "right baseline + right key"
        );
        assert_eq!(d1.device_stats.corruption_errs, 1);
        assert!(
            acked.0.contains_key("2"),
            "ack snapshots all recognized devids"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "ack must remove the latch"
        );

        // Phase 3: monitor with the SAME counters must NOT re-fire.
        let second = cmd_monitor(&runner, &fs, &mp, &paths);
        assert_eq!(
            second,
            MonitorResult::Ok,
            "counters at the acked baseline must stay suppressed"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "no latch on the suppressed cycle"
        );

        // Phase 4 (re-fire): counters above the baseline alert again -- the
        // baseline is a floor, not a permanent mute for the devid.
        let runner_higher = MonitorTestRunner::with_stats_payload(STATS_DEVID1_ERRORS_HIGHER);
        let third = cmd_monitor(&runner_higher, &fs, &mp, &paths);
        match third {
            MonitorResult::Alert(s) => assert!(
                s.causes.contains(&AlertCause::BtrfsDeviceErrors {
                    devid: Devid::new(1)
                }),
                "errors above the baseline must re-fire, got {:?}",
                s.causes
            ),
            other => panic!("expected re-fire Alert, got {other:?}"),
        }
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

    /*
     * Intent: Offline ack with a bare smartd-alert flag present at entry and
     * no latch clears the flag and exits Ok, not PoolNotMounted.
     * Why it exists: ack_offline's gate is
     * `has_alert = !causes.is_empty() || smartd_active || latch_corrupt`. Every
     * other offline smartd test either carries a SmartdAlert *cause* in the
     * latch (gate satisfied by `causes`) or has the flag arrive mid-probe
     * (asserting PoolNotMounted), so a regression dropping the `smartd_active`
     * term would slip through. This pins that term directly.
     * Scenario: smartd wrote /var/lib/braid/smartd-alert; before monitor
     * latched it, the user locked the pool and runs `braid ack`.
     */
    #[test]
    fn ack_offline_smartd_flag_no_latch_clears_flag_not_pool_not_mounted() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.smartd_alert(), b"").unwrap();

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );

        assert!(
            result.is_ok(),
            "offline smartd-flag ack must succeed, got {result:?}"
        );
        assert!(
            !paths.smartd_alert().exists(),
            "smartd flag must be removed"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "smartd-only offline ack must not write acked-stats"
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

        let result = cmd_ack_impl(&AckPanicRunner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

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

    // Intent: Mounted ack with only the scrub-failed flag (no latch) runs the
    //   full ack path -- queries btrfs device stats, removes the flag, writes a
    //   fresh baseline, and fires the beeper-stop hook once.
    // Why it exists: braid-scrub-failed.service can fire between monitor cycles.
    //   The mounted no-alert gate now reads `!scrub_failed_active`; a regression
    //   dropping that term would no-op this ack and leave the flag + beeper.
    //   Mirrors the smartd-flag-no-latch mounted test.
    // Scenario: onFailure wrote /var/lib/braid/scrub-failed; before monitor
    //   latched it, the operator runs `braid ack` on a mounted pool.
    #[test]
    fn cmd_ack_with_mounted_pool_and_scrub_failed_flag_no_latch_runs_full_ack_path() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.scrub_failed(), b"").unwrap();
        let runner = ack_mounted_probe_runner_with_device_stats();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);

        assert!(
            result.is_ok(),
            "scrub-failed-only ack should succeed, got {result:?}"
        );
        assert!(
            runner
                .requests()
                .iter()
                .any(|r| matches!(r, CmdRequest::BtrfsDeviceStatsJson { .. })),
            "scrub-failed-only ack must run the full ack path"
        );
        assert!(
            !paths.scrub_failed().exists(),
            "scrub-failed flag must be removed"
        );
        assert!(
            paths.acked_stats_json().exists(),
            "snapshot must have been saved"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire once on mounted scrub-failed-only ack"
        );
    }

    // Intent: Offline ack of a bare scrub-failed flag (no latch) clears the flag
    //   and exits Ok, not PoolNotMounted, and writes no acked-stats.
    // Why it exists: ack_offline's gate now includes `scrub_failed_active`; a
    //   regression dropping that term would refuse a bare-flag offline ack with
    //   PoolNotMounted. Mirrors the smartd bare-flag offline test, and pins that
    //   ScrubFailed falls through the MissingDevice filter (no acked-stats).
    // Scenario: onFailure wrote scrub-failed; before monitor latched it, the
    //   operator locked the pool and runs `braid ack`.
    #[test]
    fn ack_offline_scrub_failed_flag_no_latch_clears_flag_not_pool_not_mounted() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.scrub_failed(), b"").unwrap();

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );

        assert!(
            result.is_ok(),
            "offline scrub-failed-flag ack must succeed, got {result:?}"
        );
        assert!(
            !paths.scrub_failed().exists(),
            "scrub-failed flag must be removed"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "scrub-failed-only offline ack must not write acked-stats"
        );
    }

    // Intent: Offline ack does not let a scrub-failed flag written during probing
    //   turn an empty entry snapshot into an acknowledged alert.
    // Why it exists: onFailure is not under the pool lock, so it can fire while
    //   probe_pool_alerts reads mountinfo. ack snapshots scrub_failed_active at
    //   entry (before the probe); a post-probe read would consume the late flag
    //   and hide it from the next monitor cycle. Mirrors the smartd snapshot-race.
    // Scenario: pool is offline with no alerts at ack entry, but onFailure writes
    //   the flag while ack is probing the mount point.
    #[test]
    fn ack_offline_does_not_consume_scrub_failed_flag_arriving_during_probe() {
        let (_dir, paths) = isolated_paths();
        let fs = ack_offline_fs_that_touches_scrub_failed(&paths);

        let result = cmd_ack_impl(&AckPanicRunner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

        assert!(
            matches!(result, Err(AckError::PoolNotMounted)),
            "empty offline snapshot must refuse, got {result:?}"
        );
        assert!(
            paths.scrub_failed().exists(),
            "late scrub-failed flag must remain for the next monitor cycle"
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

    // Intent: Offline cleanup removes a scrub-failed flag written during probing
    //   when the entry snapshot already had a latched ScrubFailed cause.
    // Why it exists: the crash-recovery arm treats a latched ScrubFailed as an
    //   active source even if the flag was absent at entry, so `remove_scrub_failed`
    //   must be driven by `latch_had_scrub_failed`, not by `scrub_failed_active`
    //   alone. Mirrors the smartd second-arm exception.
    // Scenario: a prior monitor cycle latched ScrubFailed, the flag is absent at
    //   ack entry, and onFailure writes it again during the offline probe.
    #[test]
    fn ack_offline_with_scrub_failed_latch_cleans_mid_probe_flag() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::ScrubFailed]);
        let fs = ack_offline_fs_that_touches_scrub_failed(&paths);

        let result = cmd_ack_impl(&AckPanicRunner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

        assert!(
            result.is_ok(),
            "offline scrub-failed-latch ack should succeed, got {result:?}"
        );
        assert!(!paths.alert_latch_json().exists(), "latch must be removed");
        assert!(
            !paths.scrub_failed().exists(),
            "latched ScrubFailed cleanup must remove the mid-probe flag"
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

        let result = cmd_ack_impl(&runner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

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
        ack_write_latch(
            &paths,
            vec![AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1),
            }],
        );
        let fs = ack_mounted_fs_that_touches_smartd(&paths);
        let runner = ack_mounted_probe_runner_with_device_stats();

        let result = cmd_ack_impl(&runner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

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
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(2),
            }],
        );
        let fs = ack_offline_fs_that_touches_smartd(&paths);

        let result = cmd_ack_impl(&AckPanicRunner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

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

        let result = cmd_ack_impl(&AckPanicRunner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

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

        let result = cmd_ack_impl(&runner, &fs, &ack_mp(), &paths, &ack_noop_beeper);

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
     * Intent: When mounted ack succeeds at save_acked_stats but the third
     * cleanup file removal fails, the user-visible error names the partial
     * state and points at the recovery path. The new baseline is durable,
     * the latch is removed, the corrupt sidecar remains, and the beeper-stop
     * hook has already fired.
     * Why it exists: Without the dedicated variant, a cleanup-phase I/O
     * error surfaces as "I/O error: <kind>" with no hint that re-running ack
     * will eventually clear the latch. Without the ordering pin, a failure
     * after latch removal can leave a retry behind the no-op gate with an
     * unreached beeper-stop hook; the cleanup-pending sentinel now preserves
     * the retry signal while the beeper-stop hook remains first.
     * Scenario: a directory sits at the corrupt-latch sidecar path (manual
     * tampering, leftover from a previous bug, or permission drift), so
     * remove_file fails with EISDIR/EPERM. The latch carried
     * BtrfsDeviceErrors. Mounted pool, healthy device stats. cmd_ack must
     * save the new baseline, invoke the beeper hook, fail cleanup, and
     * return the dedicated variant.
     */
    #[test]
    fn cmd_ack_returns_cleanup_failed_when_corrupt_latch_cleanup_errors_after_baseline_saved() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1),
            }],
        );
        // remove_file on a directory returns EISDIR (Linux) / EPERM (macOS)
        // -- a platform-portable non-NotFound io::Error from
        // remove_alert_latch_corrupt.
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

        let runner = ack_mounted_probe_runner_with_device_stats();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);
        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper)
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
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "cleanup-pending sentinel must remain so retry re-enters cleanup"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire even when a later cleanup remove_* fails"
        );
    }

    /*
     * Intent: Mounted cleanup invokes the beeper-stop hook before the first
     * cleanup file removal can fail.
     * Why it exists: The corrupt-sidecar cleanup failure tests exercise the
     * third removal. This test exercises the first removal so the union pins
     * the stronger invariant: stop_beeper runs before every remove_*, not
     * merely sometime during cleanup.
     * Scenario: monitor latched SmartdAlert, but the smartd flag path is a
     * poison directory. Mounted pool, healthy device stats. cmd_ack must save
     * the new baseline, invoke the beeper hook, fail on the smartd flag
     * removal, and leave later cleanup files untouched.
     */
    #[test]
    fn cmd_ack_stops_beeper_before_mounted_smartd_flag_cleanup_error() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(&paths, vec![AlertCause::SmartdAlert]);
        // smartd_alert_active ignores directories, but the latched
        // SmartdAlert still opts cleanup into removing the flag path.
        std::fs::create_dir(paths.smartd_alert()).unwrap();

        let runner = ack_mounted_probe_runner_with_device_stats();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);
        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper)
            .expect_err("smartd cleanup failure must propagate");

        assert!(
            matches!(err, AckError::CleanupFailed(_)),
            "expected AckError::CleanupFailed, got: {err:?}"
        );
        assert!(
            paths.acked_stats_json().exists(),
            "mounted ack must persist a fresh baseline before cleanup"
        );
        assert!(
            paths.alert_latch_json().exists(),
            "latch must remain because cleanup failed on the first removal"
        );
        assert!(
            paths.smartd_alert().exists(),
            "cleanup poison directory must remain and prove where cleanup failed"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "cleanup-pending sentinel must remain after mark succeeds"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire before the first cleanup remove_* fails"
        );
    }

    // Intent: After cleanup_alert_files_and_beeper fails at
    //   remove_alert_latch_corrupt, the alert-cleanup-pending sentinel remains
    //   on disk, so the retry re-enters cleanup, re-invokes the stop hook, and
    //   completes cleanup.
    // Why it exists: c889f9c made stop_beeper run first but did not change the
    //   removal order. The retry on a poisoned corrupt sidecar still hit the
    //   mounted no-op gate because the latch JSON had already been removed.
    //   The sentinel preserves the retry signal without moving the corrupt
    //   sidecar's destructive step, which would break ADR 014's forensic
    //   guarantee.
    // Scenario: monitor latched BtrfsDeviceErrors{devid:1}. A directory sits
    //   at alert-latch.json.corrupt. `braid ack` returns CleanupFailed.
    //   Operator removes the directory and re-runs `braid ack`.
    #[test]
    fn cmd_ack_mounted_retry_after_cleanup_failed_completes_recovery() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1),
            }],
        );
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

        let runner = ack_mounted_probe_runner_with_device_stats();
        let beeper_calls_first = std::cell::Cell::new(0u32);
        let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_first)
            .expect_err("first call must fail on the poisoned corrupt sidecar");
        assert!(matches!(err, AckError::CleanupFailed(_)));
        assert_eq!(
            beeper_calls_first.get(),
            1,
            "stop hook must be invoked on the failing first call"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "sentinel must remain on disk so retry re-enters cleanup"
        );
        assert!(paths.alert_latch_corrupt().exists(), "poison still wedged");

        std::fs::remove_dir(paths.alert_latch_corrupt()).unwrap();

        let beeper_calls_retry = std::cell::Cell::new(0u32);
        let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_retry);
        assert!(
            result.is_ok(),
            "retry must succeed after operator clears poison"
        );
        assert_eq!(
            beeper_calls_retry.get(),
            1,
            "retry must re-invoke the stop hook"
        );
        assert!(
            !paths.alert_cleanup_pending().exists(),
            "retry must clear the sentinel"
        );
        assert!(!paths.alert_latch_corrupt().exists());
    }

    // Intent: After offline ack fails at the corrupt-sidecar removal, the
    //   alert-cleanup-pending sentinel remains on disk and the latch has been
    //   removed. The retry takes the hoisted cleanup-only branch before
    //   probe_pool_alerts, re-invokes the stop hook, and completes cleanup.
    // Why it exists: without the hoisted branch, this retry would land in
    //   ack_offline's has_alert == false arm and return PoolNotMounted right
    //   after the user followed the CleanupFailed recovery instruction.
    // Scenario: pool offline, monitor latched MissingDevice{devid:1}. A
    //   directory sits at alert-latch.json.corrupt. Operator runs `braid ack`,
    //   gets CleanupFailed, removes the poison, and re-runs `braid ack`.
    #[test]
    fn ack_offline_retry_after_cleanup_failed_completes_recovery() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(1),
            }],
        );
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

        let beeper_calls_first = std::cell::Cell::new(0u32);
        let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

        let err = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper_first,
        )
        .expect_err("first call must fail");
        assert!(matches!(err, AckError::CleanupFailed(_)));
        assert_eq!(
            beeper_calls_first.get(),
            1,
            "stop hook must fire on the failing first call"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "sentinel preserved on cleanup failure"
        );
        assert!(paths.alert_latch_corrupt().exists());

        std::fs::remove_dir(paths.alert_latch_corrupt()).unwrap();

        let beeper_calls_retry = std::cell::Cell::new(0u32);
        let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper_retry,
        );
        assert!(
            result.is_ok(),
            "offline retry must succeed after operator clears poison"
        );
        assert_eq!(
            beeper_calls_retry.get(),
            1,
            "retry must re-invoke the stop hook"
        );
        assert!(
            !paths.alert_cleanup_pending().exists(),
            "retry must clear the sentinel"
        );
        assert!(!paths.alert_latch_corrupt().exists());
        let acked = load_acked_stats(&paths);
        assert!(acked.0.get("1").unwrap().missing_acked);
    }

    // Intent: When the entry alert signal is a smartd flag and no latch JSON
    //   exists, a cleanup failure at remove_alert_latch_corrupt leaves the
    //   sentinel on disk, so the retry re-enters cleanup even though smartd
    //   was already removed during the first attempt.
    // Why it exists: latch-backed retry tests still pass if a future refactor
    //   narrows the sentinel to latch-present cases. This pins the smartd-only
    //   path where neither a latch nor an active smartd flag survives the
    //   first call's cleanup.
    // Scenario: smartd hook fired but monitor has not run yet, so there is no
    //   latch JSON. A directory sits at alert-latch.json.corrupt. Operator
    //   clears that directory and re-runs `braid ack`.
    #[test]
    fn cmd_ack_mounted_smartd_only_retry_after_cleanup_failed_completes_recovery() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.smartd_alert(), b"").unwrap();
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

        let runner = ack_mounted_probe_runner_with_device_stats();
        let beeper_calls_first = std::cell::Cell::new(0u32);
        let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_first)
            .expect_err("first call must fail on the poisoned corrupt sidecar");
        assert!(matches!(err, AckError::CleanupFailed(_)));
        assert_eq!(
            beeper_calls_first.get(),
            1,
            "stop hook must be invoked on the failing first call"
        );
        assert!(
            !paths.smartd_alert().exists(),
            "smartd flag was removed before the corrupt-sidecar step failed"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "sentinel preserved on cleanup failure"
        );
        assert!(paths.alert_latch_corrupt().exists(), "poison still wedged");

        std::fs::remove_dir(paths.alert_latch_corrupt()).unwrap();

        let beeper_calls_retry = std::cell::Cell::new(0u32);
        let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);

        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_retry);
        assert!(
            result.is_ok(),
            "retry must succeed after operator clears poison"
        );
        assert_eq!(
            beeper_calls_retry.get(),
            1,
            "retry must re-invoke the stop hook"
        );
        assert!(
            !paths.alert_cleanup_pending().exists(),
            "retry must clear the sentinel"
        );
        assert!(!paths.alert_latch_corrupt().exists());
    }

    // Intent: When the retry's only entry signal is the cleanup-pending
    //   sentinel, cmd_ack_impl takes the hoisted sentinel-only branch before
    //   probe_pool_alerts. The retry issues zero runner requests and does not
    //   rewrite acked-stats.json.
    // Why it exists: a sentinel-aware gate below probe_pool_alerts would
    //   re-wedge cleanup on probe failure and could re-baseline new counters
    //   that arrived after the failed first ack. The retry is finishing the
    //   previous ack, not starting a new one.
    // Scenario: a mounted ack saves a baseline, then cleanup fails at the
    //   corrupt sidecar. Operator removes the directory; retry must complete
    //   cleanup without re-querying btrfs.
    #[test]
    fn cmd_ack_mounted_sentinel_only_retry_does_not_query_btrfs_or_rewrite_baseline() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1),
            }],
        );
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();

        let runner = ack_mounted_probe_runner_with_device_stats();
        let beeper_calls_first = std::cell::Cell::new(0u32);
        let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_first)
            .expect_err("first call must fail on the poisoned corrupt sidecar");
        assert!(matches!(err, AckError::CleanupFailed(_)));
        let baseline_after_first = std::fs::read(paths.acked_stats_json()).unwrap();
        let requests_after_first = runner.requests().len();

        std::fs::remove_dir(paths.alert_latch_corrupt()).unwrap();

        let beeper_calls_retry = std::cell::Cell::new(0u32);
        let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);

        let result = cmd_ack_impl(
            &runner,
            &AckPanicFilesystem,
            &ack_mp(),
            &paths,
            &beeper_retry,
        );
        assert!(result.is_ok(), "retry must succeed without probing");

        let retry_requests: Vec<_> = runner
            .requests()
            .into_iter()
            .skip(requests_after_first)
            .collect();
        assert!(
            retry_requests.is_empty(),
            "sentinel-only retry must issue zero runner requests; got {retry_requests:?}"
        );
        let baseline_after_retry = std::fs::read(paths.acked_stats_json()).unwrap();
        assert_eq!(
            baseline_after_first, baseline_after_retry,
            "sentinel-only retry must not rewrite acked-stats.json"
        );
        assert!(
            !paths.alert_cleanup_pending().exists(),
            "retry must clear the sentinel"
        );
        assert!(!paths.alert_latch_corrupt().exists());
    }

    // Intent: When mark_alert_cleanup_pending itself fails, cleanup
    //   short-circuits before any destructive removal runs. CleanupFailed is
    //   returned, every entry alert signal is byte-identical to entry, and the
    //   retry observes the original entry snapshot to re-enter cleanup.
    // Why it exists: the cleanup-pending sentinel is itself a file ack writes,
    //   so it can be poisoned like other alert-state paths. Any destructive
    //   removal before marker creation would destroy alert state before retry
    //   had a chance to record cleanup-pending.
    // Scenario: a directory sits at alert-cleanup-pending. Latch carries
    //   BtrfsDeviceErrors{devid:1}. After the operator removes the poison
    //   directory, retry completes cleanly.
    #[test]
    fn cmd_ack_mounted_retry_after_poisoned_sentinel_completes_recovery() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1),
            }],
        );
        std::fs::create_dir(paths.alert_cleanup_pending()).unwrap();
        let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();

        let runner = ack_mounted_probe_runner_with_device_stats();
        let beeper_calls_first = std::cell::Cell::new(0u32);
        let beeper_first = || beeper_calls_first.set(beeper_calls_first.get() + 1);

        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_first)
            .expect_err("marker creation must fail on the poisoned sentinel path");
        assert!(matches!(err, AckError::CleanupFailed(_)));
        assert_eq!(
            beeper_calls_first.get(),
            1,
            "stop hook must fire before marker creation"
        );
        assert_eq!(
            std::fs::read(paths.alert_latch_json()).unwrap(),
            original_latch_bytes,
            "latch JSON must be preserved because no destructive removal ran"
        );
        assert!(
            paths.alert_cleanup_pending().exists(),
            "poison sentinel directory still wedged"
        );
        assert!(
            !alert::alert_cleanup_pending(&paths),
            "directory-form sentinel must not be treated as cleanup-pending"
        );
        assert!(
            paths.acked_stats_json().exists(),
            "save_acked_stats ran before cleanup"
        );

        std::fs::remove_dir(paths.alert_cleanup_pending()).unwrap();

        let beeper_calls_retry = std::cell::Cell::new(0u32);
        let beeper_retry = || beeper_calls_retry.set(beeper_calls_retry.get() + 1);
        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper_retry);
        assert!(
            result.is_ok(),
            "retry must succeed after operator clears the sentinel poison"
        );
        assert_eq!(
            beeper_calls_retry.get(),
            1,
            "retry must re-invoke the stop hook"
        );
        assert!(!paths.alert_cleanup_pending().exists());
        assert!(!paths.alert_latch_json().exists());
    }

    // Intent: ack does not quarantine a corrupt alert-latch.json. The
    //   no-quarantine invariant is the monitor/ack asymmetry: monitor moves
    //   corrupt bytes to alert-latch.json.corrupt because it rewrites the
    //   latch, while ack deletes the latch outright in cleanup.
    // Why it exists: a symmetry edit could swap load_alert_latch for
    //   load_alert_latch_or_quarantine in cmd_ack_impl. On the success path,
    //   the sidecar would be created by quarantine and then deleted by
    //   remove_alert_latch_corrupt during cleanup, so a sidecar-absence
    //   assertion at the end of a successful ack proves nothing. Forcing
    //   cleanup to fail at mark_alert_cleanup_pending stops execution before
    //   destructive removal, so any sidecar created by quarantine persists.
    // Scenario: corrupt latch on disk; the alert-cleanup-pending path is a
    //   directory. ack must return CleanupFailed, preserve the original corrupt
    //   latch bytes verbatim, and not create a .corrupt sidecar.
    #[test]
    fn cmd_ack_mounted_corrupt_latch_does_not_quarantine_when_cleanup_fails() {
        let (_dir, paths) = isolated_paths();
        let original_bytes: &[u8] = b"not json";
        std::fs::write(paths.alert_latch_json(), original_bytes).unwrap();
        std::fs::create_dir(paths.alert_cleanup_pending()).unwrap();

        let runner = ack_mounted_probe_runner_with_device_stats();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper)
            .expect_err("marker creation must fail on the poisoned sentinel path");
        assert!(
            matches!(err, AckError::CleanupFailed(_)),
            "expected AckError::CleanupFailed, got {err:?}"
        );

        assert_eq!(
            std::fs::read(paths.alert_latch_json()).unwrap(),
            original_bytes,
            "corrupt latch bytes must remain untouched because no destructive removal ran"
        );
        assert!(
            !paths.alert_latch_corrupt().exists(),
            "ack must not quarantine -- monitor is the only path that creates the sidecar"
        );
    }

    // Intent: When ack cleanup fails before reaching remove_alert_latch_corrupt,
    //   the corrupt sidecar's bytes are unchanged. Pins ADR 014's forensic
    //   guarantee at the unit level for the cleanup-failure path.
    // Why it exists: the cleanup-pending sentinel design keeps
    //   alert-latch.json.corrupt as the last destructive step so its bytes
    //   survive partial cleanup.
    // Scenario: a prior monitor cycle quarantined a corrupt latch. The current
    //   cycle latched SmartdAlert, but the smartd flag path is a poison
    //   directory. ack fails at remove_smartd_alert_flag and must leave the
    //   corrupt sidecar untouched.
    #[test]
    fn cmd_ack_preserves_corrupt_sidecar_bytes_through_cleanup_failure() {
        let (_dir, paths) = isolated_paths();
        let forensic_bytes: &[u8] = b"first corruption forensic data";
        std::fs::write(paths.alert_latch_corrupt(), forensic_bytes).unwrap();
        ack_write_latch(&paths, vec![AlertCause::SmartdAlert]);
        std::fs::create_dir(paths.smartd_alert()).unwrap();

        let runner = ack_mounted_probe_runner_with_device_stats();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let err = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper)
            .expect_err("smartd cleanup failure must propagate");
        assert!(matches!(err, AckError::CleanupFailed(_)));
        assert_eq!(beeper_calls.get(), 1);

        let preserved = std::fs::read(paths.alert_latch_corrupt()).unwrap();
        assert_eq!(
            preserved, forensic_bytes,
            "corrupt sidecar bytes must survive cleanup failure"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "sentinel must be on disk after mark succeeds"
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
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(2),
            }],
        );
        let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();
        std::fs::write(paths.smartd_alert(), b"").unwrap();

        let err = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_ext4(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        )
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

        let err = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_ext4(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        )
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

        let err = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_ext4(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        )
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
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(2),
            }],
        );
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
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(2),
            }],
        );
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
     * missing-device ack update to acked-stats.json before cleanup. When the
     * third cleanup file removal then fails, the user-visible error names the
     * partial state, the latch is removed, the corrupt sidecar remains, and
     * the beeper-stop hook has already fired.
     * Why it exists: cmd_ack_impl and ack_offline have separate cleanup call
     * sites. A regression that reverts only the offline wrapping would
     * silently fall back to AckError::Io and the mounted test would still
     * pass. The beeper assertion independently pins that the offline path's
     * cleanup reaches stop_beeper before a later remove_* can short-circuit.
     * The cleanup-pending sentinel is the retry signal after the latch is
     * already removed.
     * Scenario: pool offline, latch contains MissingDevice{devid:1}, and a
     * directory sits at the corrupt-latch sidecar path so remove_file fails
     * with EISDIR/EPERM.
     */
    #[test]
    fn ack_offline_cleanup_failure_after_missing_acked_returns_cleanup_failed() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(1),
            }],
        );
        // remove_file on a directory returns EISDIR (Linux) / EPERM (macOS)
        // -- a platform-portable non-NotFound io::Error from
        // remove_alert_latch_corrupt.
        std::fs::create_dir(paths.alert_latch_corrupt()).unwrap();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let err = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper,
        )
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
        // missing-device ack state was persisted (acked-stats exists), latch
        // was removed, and cleanup stopped at the corrupt sidecar.
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
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "cleanup-pending sentinel must remain so offline retry can resume"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire even when a later cleanup remove_* fails"
        );
    }

    // Intent: Offline ack treats an unreadable alert latch as active even
    //   when no causes can be parsed from it.
    // Why it exists: ADR 014's recovery contract depends on `cmd_ack_impl`
    //   mapping both Read and Parse latch load failures to `latch_corrupt =
    //   true`. If a future refactor narrows that gate to Parse only, the
    //   offline branch returns PoolNotMounted before cleanup and leaves the
    //   user unable to clear the broken latch while the pool is locked.
    // Scenario: pool is offline, and filesystem damage or external tampering
    //   leaves a directory where alert-latch.json should be.
    #[test]
    fn ack_offline_read_error_latch_reaches_cleanup_failed() {
        let (_dir, paths) = isolated_paths();
        std::fs::create_dir(paths.alert_latch_json()).unwrap();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let err = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &beeper,
        )
        .expect_err("unreadable latch on an offline pool must reach cleanup");

        assert!(
            matches!(err, AckError::CleanupFailed(_)),
            "Read-error latch must gate as active and reach cleanup, got {err:?}"
        );
        assert_eq!(
            beeper_calls.get(),
            1,
            "stop_beeper must fire before the failed latch removal"
        );
        assert!(
            paths.alert_latch_json().exists(),
            "latch directory cannot be removed by remove_file"
        );
        assert!(
            paths.alert_cleanup_pending().is_file(),
            "sentinel must be marked before the failed latch removal"
        );
        assert!(
            !paths.acked_stats_json().exists(),
            "no MissingDevice cause means no acked-stats write"
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
                AlertCause::BtrfsDeviceErrors {
                    devid: Devid::new(1),
                },
                AlertCause::MissingDevice {
                    devid: Devid::new(2),
                },
            ],
        );
        let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();
        assert!(!paths.acked_stats_json().exists());

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );
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

        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(1),
            }],
        );

        cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        )
        .unwrap();

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

        ack_write_latch(
            &paths,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(1),
            }],
        );

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );
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

        cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        )
        .unwrap();

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

        cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        )
        .unwrap();

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

        let msg = format_systemctl_stop_failure("braid-alert.service", &output)
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

    // Intent: A non-zero `systemctl stop braid-alert.service` exit with empty
    //   stderr still warns, rendering the process status with no trailing
    //   diagnostic suffix.
    // Why it exists: The empty-stderr arm formats a distinct message
    //   ("...{status}" with no ": {stderr}" tail). A swapped empty/non-empty
    //   arm, a dropped status, or a lost prefix would ship a malformed or
    //   useless beeper-stop warning; only an empty-stderr input exercises that
    //   arm.
    // Scenario: systemctl exits non-zero but prints nothing to stderr (the
    //   stop is rejected with only an exit code), so braid must still surface
    //   the status.
    #[cfg(unix)]
    #[test]
    fn format_systemctl_stop_failure_warns_on_nonzero_exit_without_stderr() {
        let output = Output {
            status: ExitStatus::from_raw(5 << 8),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };

        assert_eq!(
            format_systemctl_stop_failure("braid-alert.service", &output),
            Some("warning: systemctl stop braid-alert.service: exit status: 5".to_string()),
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

        assert_eq!(
            format_systemctl_stop_failure("braid-alert.service", &output),
            None
        );
    }

    // --- ENOSPC baseline ack (Step 5) ---

    // Intent: a mounted ack of an EnospcRisk latch whose fresh probe is still at
    //   risk writes enospc-ack.json keyed on the live (devid, device_size) pairs,
    //   with snoozed_until one reminder interval past ack time; it clears the
    //   latch, leaves the marker in place, and fires the cleanup hook once.
    // Why it exists: ack snoozes the reminder (it does not resolve), and the
    //   monitor's suppression relies on a fresh, correctly-keyed deadline. The
    //   re-probe (live key from the ack-time usage probe, not the latched fire-time
    //   cause) and the persist-past-ack are that contract.
    // Scenario: monitor latched EnospcRisk on a mounted 2-disk pool; the operator
    //   runs braid ack while the pool is still at risk.
    #[test]
    fn cmd_ack_mounted_enospc_risk_writes_reprobed_keyed_baseline() {
        let (_dir, paths) = isolated_paths();
        // A fire-time cause in the latch; the snooze marker must be keyed off the
        // fresh ack-time probe, not this latched value.
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );
        let runner = ack_mounted_probe_runner_with_enospc_usage();
        let beeper_calls = std::cell::Cell::new(0u32);
        let beeper = || beeper_calls.set(beeper_calls.get() + 1);

        let before = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let result = cmd_ack_impl(&runner, &ack_fs_btrfs(), &ack_mp(), &paths, &beeper);
        let after = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(result.is_ok(), "ack must succeed, got {result:?}");

        let ack = load_enospc_ack(&paths)
            .unwrap()
            .expect("mounted ack of a still-at-risk pool must write a snooze marker");
        assert_eq!(
            ack.pool_key.fs_uuid, ACK_FS_UUID,
            "keyed on the live FS UUID"
        );
        assert_eq!(
            ack.pool_key.devices,
            vec![
                (Devid::new(1), ACK_DEVICE_SIZE),
                (Devid::new(3), ACK_DEVICE_SIZE),
            ],
            "pool_key carries (devid, device_size) from the ack-time usage probe"
        );
        let interval = ENOSPC_REMINDER_INTERVAL.as_secs();
        assert!(
            (before + interval..=after + interval).contains(&ack.snoozed_until),
            "snoozed_until {} must be one reminder interval past ack time [{}, {}]",
            ack.snoozed_until,
            before + interval,
            after + interval
        );
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
        assert!(
            paths.enospc_ack_json().exists(),
            "the marker persists past ack (post-ack snooze memory)"
        );
        assert_eq!(beeper_calls.get(), 1, "cleanup hook fires once");
    }

    // Intent: a mounted ack of an EnospcRisk latch whose fresh probe is HEALTHY
    //   (dead-band, 0 <= margin < REARM) clears the latch but writes no snooze
    //   marker.
    // Why it exists (F1): the at-risk gate. Acking a recovered pool must not stamp
    //   a snooze onto a not-at-risk pool -- the dead-band monitor branch keeps a
    //   marker, so a snooze written here would wrongly suppress a recurrence inside
    //   the window. No marker -> a later recurrence fires armed (pairs with
    //   cmd_monitor_at_risk_no_marker_fires_armed).
    // Scenario: monitor latched EnospcRisk, the pool recovered into the dead-band
    //   between fire and ack, and the operator runs braid ack.
    #[test]
    fn cmd_ack_mounted_enospc_healthy_at_ack_writes_no_snooze() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );
        let runner = ack_mounted_probe_runner_with_healthy_enospc_usage();

        let result = cmd_ack_impl(
            &runner,
            &ack_fs_btrfs(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );
        assert!(result.is_ok(), "ack must succeed, got {result:?}");
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
        assert!(
            !paths.enospc_ack_json().exists(),
            "a not-at-risk ack-time probe writes no snooze marker"
        );
    }

    // Intent: a mounted ack of an EnospcRisk latch whose usage probe is unstubbed
    //   (MissingMock) clears the latch but writes no baseline.
    // Why it exists: the baseline probe is best-effort -- a probe failure must not
    //   fail the ack or fabricate a baseline. One quiet re-fire next cycle, then a
    //   clean ack establishes it.
    // Scenario: the usage probe fails during a mounted ack of an ENOSPC alert.
    #[test]
    fn cmd_ack_mounted_enospc_risk_unstubbed_usage_writes_no_baseline() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );
        // device-stats stubbed, usage NOT -> MissingMock on the baseline probe.
        let runner = ack_mounted_probe_runner_with_device_stats();

        let result = cmd_ack_impl(
            &runner,
            &ack_fs_btrfs(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );
        assert!(result.is_ok(), "best-effort baseline must not fail the ack");
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
        assert!(
            !paths.enospc_ack_json().exists(),
            "a failed usage probe writes no baseline"
        );
    }

    // Intent: a mounted ack whose live pool has no FS UUID clears the latch but
    //   writes no baseline (no usable PoolKey).
    // Why it exists: a keyless baseline would be invalidated immediately anyway;
    //   the absent strongest-identity field must not produce a weak baseline.
    // Scenario: a transient mounted ack reads a btrfs show with no uuid line.
    #[test]
    fn cmd_ack_mounted_enospc_risk_no_fs_uuid_writes_no_baseline() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );
        let runner = ack_mounted_probe_runner_no_uuid_with_enospc_usage();

        let result = cmd_ack_impl(
            &runner,
            &ack_fs_btrfs(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );
        assert!(result.is_ok(), "ack must succeed, got {result:?}");
        assert!(!paths.alert_latch_json().exists(), "ack clears the latch");
        assert!(
            !paths.enospc_ack_json().exists(),
            "no FS UUID -> no keyed baseline"
        );
    }

    // Intent: an offline ack of an EnospcRisk-only latch is allowed, clears the
    //   latch, and writes no baseline.
    // Why it exists: EnospcRisk carries no monotonic counter, so unlike
    //   BtrfsDeviceErrors it is safe to ack offline -- but offline ack cannot
    //   probe the live pool_key, so it intentionally writes no baseline (one quiet
    //   re-fire on remount, then a mounted ack establishes it).
    // Scenario: the pool is locked/offline and the operator acks a latched ENOSPC
    //   warning.
    #[test]
    fn ack_offline_enospc_risk_clears_latch_writes_no_baseline() {
        let (_dir, paths) = isolated_paths();
        ack_write_latch(
            &paths,
            vec![AlertCause::EnospcRisk {
                margin: -5,
                count_below: 1,
                device_count: 2,
            }],
        );

        let result = cmd_ack_impl(
            &AckPanicRunner,
            &ack_fs_not_mounted(),
            &ack_mp(),
            &paths,
            &ack_noop_beeper,
        );
        assert!(
            result.is_ok(),
            "offline ack of an EnospcRisk-only latch must be allowed, got {result:?}"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "offline ack clears the latch"
        );
        assert!(
            !paths.enospc_ack_json().exists(),
            "offline ack cannot key a baseline, so it writes none"
        );
    }
}
