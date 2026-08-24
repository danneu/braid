#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::time::SystemTime;

use crate::alert::{
    self, AlertCause, compute_alert_state, live_pool_key, load_enospc_ack, merge_into_latch,
    remove_enospc_ack, save_acked_stats,
};
use crate::capacity::{ENOSPC_REARM_MARGIN, evaluate_enospc_risk};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::types::BtrfsDeviceUsageEntry;
use crate::parse::{parse_btrfs_device_stats, parse_btrfs_device_usage};
use crate::probe::{Filesystem, ProbeError, probe_pool_alerts};
use crate::state_paths::StatePaths;
use crate::types::{Fsid, MountPoint};

#[derive(Debug, PartialEq, Eq)]
pub enum MonitorResult {
    PoolOffline,
    Ok,
    Alert(alert::AlertState),
}

/// Centralizes monitor's fail-closed detail folding so each pass contributes
/// at most one `ComputationError` latch slot.
fn folded_computation_error_detail(
    failure_detail: Option<String>,
    latch_corrupt_detail: Option<String>,
) -> Option<String> {
    match (failure_detail, latch_corrupt_detail) {
        (Some(failure), Some(latch_detail)) => Some(format!(
            "{failure}; additionally, previous alert latch was unreadable -- quarantined; {latch_detail}"
        )),
        (Some(failure), None) => Some(failure),
        (None, Some(latch_detail)) => Some(format!(
            "previous alert latch was unreadable -- quarantined; {latch_detail}"
        )),
        (None, None) => None,
    }
}

/// Headless monitor cycle that latches alert state from live pool probes and
/// acked baselines; probe uncertainty becomes an alert except for the ENOSPC
/// risk fail-open carve-out.
pub fn cmd_monitor<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> MonitorResult {
    cmd_monitor_at(runner, fs, mount_point, paths, SystemTime::now())
}

/// `cmd_monitor` with the cycle clock injected (the established `_at` convention,
/// e.g. `membership.rs`). The single `now` feeds both the ENOSPC snooze compare
/// and the latch merge's first-detection stamp, so tests pin both against one
/// fixed instant and a cause first detected this cycle shares that clock.
pub fn cmd_monitor_at<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
    now: SystemTime,
) -> MonitorResult {
    let classified = (|| -> Result<Option<Vec<AlertCause>>, String> {
        // 1. Check if pool is mounted.
        //
        // Exhaustive over every ProbeError variant on purpose: a future variant
        // must produce a "non-exhaustive patterns" compile error here so the
        // reviewer is forced to classify it as either offline or fail-closed
        // alert. monitor is the headless surface, so the wrong default would
        // propagate silently into operator-visible behavior.
        let pool = match probe_pool_alerts(runner, fs, mount_point) {
            Ok(p) => p,
            // Mount target holds a non-btrfs filesystem -- our pool is not here.
            // Treat as offline; no fail-closed beep needed.
            Err(ProbeError::NotBtrfs { .. }) => return Ok(None),
            // All remaining variants describe indeterminate pool state --
            // tooling/probe breakage (Cmd/Parse/MountInfo), pool show
            // internally inconsistent (PoolDevice), or LUKS-side validation
            // failure (the LUKS-side variants are all unreachable from
            // probe_pool_alerts today but listed for the gate). Fail closed
            // per ADR 014: latch ComputationError so the wrapper beeps.
            Err(
                e @ (ProbeError::Cmd(_)
                | ProbeError::Parse(_)
                | ProbeError::PoolDevice { .. }
                | ProbeError::UnsupportedLuksVersion { .. }
                | ProbeError::MapperOwnership(_)
                | ProbeError::MountInfo(_)),
            ) => return Err(e.to_string()),
        };

        if !pool.mounted {
            return Ok(None);
        }

        // 2. Run btrfs device stats
        let stats_raw = runner
            .run(&CmdRequest::BtrfsDeviceStatsJson {
                mount_point: mount_point.clone(),
            })
            .map_err(|e| e.to_string())?;
        let device_stats = parse_btrfs_device_stats(&stats_raw).map_err(|e| e.to_string())?;

        // 3. Load acked stats. Fail closed if the file is unreadable or
        // unparseable: an empty fallback would silently re-fire every acked
        // cause as a BtrfsDeviceErrors / MissingDevice cause against a zero
        // baseline.
        let mut acked = alert::load_acked_stats_fallible(paths)
            .map_err(|e| format!("acked-stats unreadable -- {e}"))?;

        // 4. Compute alert-local membership views.
        let devids = pool.alert_devids();

        // 5. Check smartd + scrub-failed alert flags
        let smartd_active = alert::smartd_alert_active(paths);
        let scrub_failed = alert::scrub_failed_active(paths);

        // 6. Reconcile stale ack state: prune orphan devids and self-heal
        //    missing_acked for devices that are present again.
        let present_devids: BTreeSet<_> = pool.present_devids.iter().copied().collect();
        let ack_changed =
            alert::reconcile_acked_stats(&mut acked, &devids.recognized, &present_devids);
        if ack_changed {
            save_acked_stats(&acked, paths)
                .map_err(|e| format!("acked-stats unwritable -- {e}"))?;
        }

        // 7. Compute live alert state. Identity is the devid carried on each
        //    stats row by btrfs -- no path-to-devid map needed.
        let mut live_causes =
            compute_alert_state(&device_stats, &acked, &devids, smartd_active, scrub_failed);

        // 7b. Best-effort ENOSPC-risk evaluation. This is the single documented
        //     exception to the fail-closed mandate: ADR 014's pure-detector
        //     contract names it (the usage probe skips only EnospcRisk and never
        //     latches ComputationError), and ADR 018 documents the probe
        //     mechanism. The helper owns its errors and never returns Err, so it
        //     stays out of the `?`-propagating ComputationError path below --
        //     device-error / missing-device alerting in this same cycle is
        //     untouched if the usage probe fails.
        let missing_count = devids.missing.len() as u64;
        if let Some(cause) = evaluate_enospc_for_monitor(
            runner,
            mount_point,
            missing_count,
            pool.fsid.as_ref(),
            paths,
            now,
        ) {
            live_causes.push(cause);
        }

        Ok(Some(live_causes))
    })();

    let (mut live_causes, failure_detail) = match classified {
        Ok(None) => return MonitorResult::PoolOffline,
        Ok(Some(causes)) => (causes, None),
        Err(detail) => {
            eprintln!("braid monitor: {detail}");
            (Vec::new(), Some(detail))
        }
    };

    // 8. Load existing latch (quarantine corrupt file if needed)
    let (existing_latch, latch_corrupt_detail) = alert::load_alert_latch_or_quarantine(paths);
    if let Some(detail) = &latch_corrupt_detail {
        eprintln!("braid monitor: warning: alert latch unreadable -- quarantining: {detail}");
    }

    // 9. Surface computation failures and latch corruption as at most one
    // ComputationError cause so status sees it instead of silently rebuilding.
    if let Some(detail) = folded_computation_error_detail(failure_detail, latch_corrupt_detail) {
        live_causes.insert(0, AlertCause::ComputationError { detail });
    }

    // 10. Merge: existing latch + live causes
    let merged = merge_into_latch(existing_latch.as_ref(), &live_causes, now);

    // 11. If merged state active -> write latch
    if merged.active()
        && let Err(e) = alert::save_alert_latch(&merged, paths)
    {
        eprintln!("braid monitor: warning: failed to write alert latch: {e}");
    }

    // 12. Return result based on merged state
    if merged.active() {
        MonitorResult::Alert(merged)
    } else {
        MonitorResult::Ok
    }
}

/// Probe `btrfs device usage --raw` and parse it into per-device entries.
///
/// Returns the parse/probe failure as a `String` detail rather than a typed
/// error so the only consumer (`evaluate_enospc_for_monitor`) can log-and-skip
/// without any path that could re-enter the fail-closed `ComputationError` fold.
fn probe_usage_entries<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
) -> Result<Vec<BtrfsDeviceUsageEntry>, String> {
    let raw = runner
        .run(&CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| e.to_string())?;
    let parsed = parse_btrfs_device_usage(&raw).map_err(|e| e.to_string())?;
    Ok(parsed.devices)
}

/// Evaluate proactive RAID1 chunk-pair ENOSPC risk for one monitor cycle and
/// drive the snooze-marker suppression, returning `Some(EnospcRisk { .. })`
/// to fire or `None` to suppress.
///
/// `now` is the live wall clock (captured by `cmd_monitor`), compared against the
/// marker's snooze deadline; injecting it keeps the window check deterministic in
/// tests.
///
/// Best-effort by contract: every failure mode here (probe, parse, marker
/// load) is logged and folded into a fire/skip decision -- it never returns
/// `Err` and never latches `ComputationError`. This is the scoped fail-open
/// carve-out to the monitor's fail-closed mandate, so a broken usage probe skips
/// only this cause and leaves device-error / missing-device alerting intact.
fn evaluate_enospc_for_monitor<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    missing_count: u64,
    fsid: Option<&Fsid>,
    paths: &StatePaths,
    now: SystemTime,
) -> Option<AlertCause> {
    // Degraded pools alert louder through MissingDevice. `missing_count` is
    // show-probed while the key below is usage-probed; ADR 014's ENOSPC baseline
    // section documents why the accepted skew fires/re-arms against a clean
    // baseline and never suppresses.
    // Skip entirely and leave any baseline untouched -- a transient device
    // absence must not drop a still-at-risk pool's suppression memory (reconnect
    // keeps the same key).
    if missing_count > 0 {
        return None;
    }

    // Cannot determine risk: log and skip. The scoped fail-open exception.
    let entries = match probe_usage_entries(runner, mount_point) {
        Ok(entries) => entries,
        Err(detail) => {
            eprintln!("braid monitor: ENOSPC probe skipped -- {detail}");
            return None;
        }
    };

    let assessment = evaluate_enospc_risk(&entries, missing_count);
    let margin = assessment.margin;

    // Re-arm: a predicate-healthy surplus clears any stored baseline so a future
    // recurrence alerts fresh. Keys off the predicate margin, NOT raw min
    // headroom, so a fault-tolerant pool with one low device still re-arms.
    if margin >= ENOSPC_REARM_MARGIN as i64 {
        if let Err(e) = remove_enospc_ack(paths) {
            eprintln!("braid monitor: warning: failed to clear ENOSPC baseline on re-arm: {e}");
        }
        return None;
    }

    // Hysteresis dead band (0 <= margin < ENOSPC_REARM_MARGIN): neither fire nor
    // re-arm. Leave any baseline in place.
    if !assessment.at_risk() {
        return None;
    }

    let cause = AlertCause::EnospcRisk {
        margin,
        count_below: assessment.count_below as u32,
        device_count: assessment.device_count as u32,
    };

    let live_key = live_pool_key(fsid, &entries);
    let baseline = match load_enospc_ack(paths) {
        Ok(opt) => opt,
        Err(e) => {
            // Risk known, baseline positively invalid (corrupt/unreadable): no
            // usable baseline, clear best-effort, fire armed. Never folds
            // ComputationError.
            eprintln!(
                "braid monitor: ENOSPC baseline unreadable -- firing armed and clearing: {e}"
            );
            let _ = remove_enospc_ack(paths);
            return Some(cause);
        }
    };

    let baseline = match baseline {
        // No baseline at all: armed, fire.
        None => return Some(cause),
        Some(b) => b,
    };

    // ADR 014's ENOSPC baseline section: a stored key carrying btrfs's missing
    // marker is never honored as a legitimate ENOSPC snooze.
    if baseline.pool_key.contains_missing_device() {
        eprintln!(
            "braid monitor: ENOSPC baseline holds a missing (zero-sized) device -- re-arming and firing"
        );
        let _ = remove_enospc_ack(paths);
        return Some(cause);
    }

    match &live_key {
        // Acked with a matching key: suppress while the snooze window is open,
        // otherwise remind. Once the deadline elapses (or a clock anomaly pushes
        // it more than one interval out, which `is_snoozed` treats as elapsed) the
        // monitor re-fires every cycle until a re-ack stamps a fresh deadline.
        Some(live) if baseline.pool_key == *live => {
            if baseline.is_snoozed(now) {
                None
            } else {
                Some(cause)
            }
        }
        // Confirmed key mismatch (bootstrap/membership/geometry change): the
        // stored baseline is stale. Remove it and fire armed.
        Some(_live) => {
            eprintln!(
                "braid monitor: ENOSPC baseline pool key no longer matches the live pool -- re-arming and firing"
            );
            let _ = remove_enospc_ack(paths);
            Some(cause)
        }
        // Identity gap (no FS UUID): we cannot compare keys, but this is not a
        // confirmed different pool. Fire armed yet LEAVE the file so a later
        // cycle with the FS UUID present can compare and re-arm it properly.
        None => {
            eprintln!(
                "braid monitor: ENOSPC baseline cannot be compared (no FS UUID) -- firing armed, baseline left in place"
            );
            Some(cause)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ack::cmd_ack_impl;
    use crate::alert::{
        AlertSeverity, ENOSPC_REMINDER_INTERVAL, EnospcAck, PoolKey, load_enospc_ack,
        save_enospc_ack,
    };
    use crate::cmd::{CmdError, RawCommandOutput};
    use crate::test_fixtures::{
        BTRFS_SHOW_2DISK_1MISSING, BTRFS_SHOW_2DISK_NO_UUID, MONITOR_FSID, MonitorOverride,
        MonitorReconcileRunner, MonitorTestRunner, USAGE_DEVICE_SIZE, ack_noop_beeper,
        assert_monitor_single_computation_error, isolated_paths, missing_pool_key,
        monitor_fs_btrfs, monitor_fs_ext4, monitor_fs_mountinfo_error, monitor_fs_not_mounted,
        monitor_mp, usage_2disk, usage_2disk_one_missing, usage_4disk_one_low,
    };
    use crate::types::Devid;
    use std::time::UNIX_EPOCH;

    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;

    /// Real-clock `now` in Unix seconds, for seeding snooze deadlines relative to
    /// the wall clock `cmd_monitor` reads.
    fn now_secs() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs()
    }

    /// A snooze deadline half a reminder interval in the future, so a matching-key
    /// marker reads as still-snoozed under `cmd_monitor`'s real clock.
    fn open_snooze_deadline() -> u64 {
        now_secs() + ENOSPC_REMINDER_INTERVAL.as_secs() / 2
    }

    /// At-risk two-disk usage: device 1 has 100 MiB unallocated (below the 1 GiB
    /// threshold), device 2 is roomy -- a clearly negative predicate margin.
    fn usage_atrisk() -> String {
        usage_2disk(USAGE_DEVICE_SIZE, 100 * MIB, 50 * GIB)
    }

    fn fsid(raw: &str) -> Fsid {
        Fsid::parse(raw).unwrap()
    }

    /// The `PoolKey` the default monitor probe builds for an at-risk 2-disk pool:
    /// the canonical FS UUID plus both devids at `USAGE_DEVICE_SIZE`.
    fn matching_pool_key() -> PoolKey {
        PoolKey {
            fsid: fsid(MONITOR_FSID),
            devices: vec![
                (Devid::new(1), USAGE_DEVICE_SIZE),
                (Devid::new(2), USAGE_DEVICE_SIZE),
            ],
        }
    }

    fn seed_enospc_baseline(paths: &StatePaths, pool_key: PoolKey, snoozed_until: u64) {
        save_enospc_ack(
            &EnospcAck {
                pool_key,
                snoozed_until,
            },
            paths,
        )
        .unwrap();
    }

    fn has_enospc_cause(result: &MonitorResult) -> bool {
        matches!(result, MonitorResult::Alert(s) if s.causes.iter().any(|c| matches!(c.cause, AlertCause::EnospcRisk { .. })))
    }

    fn has_computation_error(result: &MonitorResult) -> bool {
        matches!(result, MonitorResult::Alert(s) if s.causes.iter().any(|c| matches!(c.cause, AlertCause::ComputationError { .. })))
    }

    fn acked_disk(missing_acked: bool, read_io_errs: u64) -> alert::AckedDisk {
        alert::AckedDisk {
            missing_acked,
            device_stats: alert::AckedDeviceCounters {
                read_io_errs,
                ..Default::default()
            },
        }
    }

    fn alert_state(result: &MonitorResult) -> &alert::AlertState {
        match result {
            MonitorResult::Alert(state) => state,
            other => panic!("expected MonitorResult::Alert, got {other:?}"),
        }
    }

    fn computation_error_details(state: &alert::AlertState) -> Vec<&str> {
        state
            .causes
            .iter()
            .filter_map(|cause| match &cause.cause {
                AlertCause::ComputationError { detail } => Some(detail.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The latched causes stripped of their `detected_at` stamp, so cause-set
    /// assertions stay agnostic to the wall-clock timestamp the merge records.
    fn causes_only(state: &alert::AlertState) -> Vec<AlertCause> {
        state.causes.iter().map(|c| c.cause.clone()).collect()
    }

    /// Wrap a cause as a latched entry with a fixed first-detection stamp, for
    /// seeding an existing latch in tests where the timestamp is incidental.
    fn latch_entry(cause: AlertCause) -> alert::LatchedCause {
        alert::LatchedCause::new(cause, "2023-11-14T22:13:20Z".to_owned())
    }

    /*
     * Intent: cmd_monitor reconciles acked-stats at the command boundary:
     * present devices self-heal missing_acked, still-relevant missing axes are
     * preserved, and true orphan devids are pruned from disk.
     *
     * Why it exists: monitor is the defense-in-depth cleanup layer. Helper
     * tests cannot catch a future edit that stops passing the correct
     * present/null-underlying/MISSING sets from the live pool probe into the
     * reconciler.
     *
     * Scenario: probe sees present devid 1, null-underlying mapper devid 2,
     * btrfs MISSING devid 3, and the ack file also contains orphan devid 99.
     * After monitor, keys are exactly 1, 2, 3; devid 1 is no longer marked
     * missing, while devids 2 and 3 keep their acknowledged missing state.
     */
    #[test]
    fn cmd_monitor_reconciles_acked_stats_across_pool_axes() {
        let (_dir, paths) = isolated_paths();
        let disk1 = acked_disk(true, 1);
        let disk2 = acked_disk(true, 2);
        let disk3 = acked_disk(true, 3);
        let orphan = acked_disk(false, 99);
        save_acked_stats(
            &alert::AckedStats(BTreeMap::from([
                ("1".to_owned(), disk1),
                ("2".to_owned(), disk2.clone()),
                ("3".to_owned(), disk3.clone()),
                ("99".to_owned(), orphan),
            ])),
            &paths,
        )
        .unwrap();

        let result = cmd_monitor(
            &MonitorReconcileRunner,
            &monitor_fs_btrfs(),
            &monitor_mp(),
            &paths,
        );

        assert_eq!(result, MonitorResult::Ok);
        let reloaded = alert::load_acked_stats(&paths);
        let keys: Vec<&str> = reloaded.0.keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["1", "2", "3"]);
        assert_eq!(reloaded.0.get("1"), Some(&acked_disk(false, 1)));
        assert_eq!(
            reloaded.0.get("2"),
            Some(&disk2),
            "null-underlying missing ack must be preserved"
        );
        assert_eq!(
            reloaded.0.get("3"),
            Some(&disk3),
            "btrfs MISSING ack must be preserved"
        );
    }

    // Intent: cmd_monitor surfaces an acked-stats save failure as one
    //   ComputationError after reconcile mutates stale ack state.
    // Why it exists: a failed reconcile write leaves monitor's persisted
    //   ack baseline indeterminate; returning Ok would turn a persistent
    //   state-directory write fault into a journald-only warning.
    // Scenario: monitor prunes an orphan devid from acked-stats.json, but a
    //   poisoned stale temp path prevents the reconciled file from being saved.
    #[cfg(unix)]
    #[test]
    fn save_acked_stats_failure_latches_computation_error() {
        let (_dir, paths) = isolated_paths();
        save_acked_stats(
            &alert::AckedStats(BTreeMap::from([("99".to_owned(), acked_disk(false, 99))])),
            &paths,
        )
        .unwrap();

        let acked_path = paths.acked_stats_json();
        let state_dir = acked_path
            .parent()
            .expect("acked-stats path has state directory")
            .to_path_buf();
        let stale_tmp = state_dir.join(".acked-stats.json.tmp");
        std::fs::create_dir(&stale_tmp).unwrap();

        let result = cmd_monitor(
            &MonitorReconcileRunner,
            &monitor_fs_btrfs(),
            &monitor_mp(),
            &paths,
        );

        let detail = assert_monitor_single_computation_error(&result);
        assert!(
            detail.contains("acked-stats"),
            "detail should name acked-stats failure, got {detail}"
        );
        assert!(
            detail.contains("unwritable"),
            "detail should name unwritable acked-stats, got {detail}"
        );
    }

    // Intent: cmd_monitor durably persists a reconcile self-heal (missing_acked
    //   true -> false) in the same cycle that folds a ComputationError from a
    //   corrupt alert latch.
    // Why it exists: the reconcile save (step 6) runs inside the classified
    //   closure, before the latch load/quarantine and ComputationError fold
    //   (steps 8-9). cmd_monitor_reconciles_acked_stats_across_pool_axes covers
    //   only the clean-Ok path and save_acked_stats_failure_latches_computation_error
    //   covers only the save failing. A refactor that skipped or gated the reconcile
    //   save whenever the cycle also raises a ComputationError would compile and pass
    //   every other monitor test while silently dropping the self-heal -- leaving the
    //   acked baseline stale so the next cycle re-fires or mis-baselines the device.
    // Scenario: present devid 1 was previously acknowledged missing
    //   (missing_acked=true) and is back online, while alert-latch.json is corrupt
    //   this cycle. monitor must self-heal devid 1 to missing_acked=false and persist
    //   it to acked-stats.json, AND return exactly one ComputationError for the
    //   quarantined latch.
    #[test]
    fn reconcile_self_heal_persists_when_cycle_also_folds_computation_error() {
        let (_dir, paths) = isolated_paths();
        // Devid 1 is a present, recognized pool member (BTRFS_SHOW_2DISK) carrying a
        // stale missing ack; reconcile must self-heal it and save acked-stats.json.
        save_acked_stats(
            &alert::AckedStats(BTreeMap::from([("1".to_owned(), acked_disk(true, 1))])),
            &paths,
        )
        .unwrap();
        // Corrupt latch is the sole ComputationError source -- it is loaded AFTER the
        // reconcile save, so a healthy save must already be on disk by then.
        std::fs::write(paths.alert_latch_json(), b"not json").unwrap();

        let result = cmd_monitor(
            &MonitorTestRunner::with_stale_mapper_stats(),
            &monitor_fs_btrfs(),
            &monitor_mp(),
            &paths,
        );

        // Fold half: exactly one ComputationError, and the latch is its SOLE source.
        // The positive check alone is not enough -- folded_computation_error_detail
        // concatenates a failure detail and the latch detail in the (Some, Some) case,
        // so a co-folded "acked-stats unwritable" save failure would still contain the
        // latch substring. The negative check pins that no acked-stats failure was
        // folded, i.e. the reconcile save succeeded.
        let detail = assert_monitor_single_computation_error(&result);
        assert!(
            detail.contains("previous alert latch was unreadable -- quarantined"),
            "ComputationError must name the latch quarantine, got {detail}"
        );
        assert!(
            !detail.contains("acked-stats"),
            "no acked-stats failure may be folded in -- the reconcile save must have succeeded, got {detail}"
        );
        // Self-heal half: the reconcile write persisted despite the folded error.
        let reloaded = alert::load_acked_stats(&paths);
        assert_eq!(
            reloaded.0.get("1"),
            Some(&acked_disk(false, 1)),
            "self-heal must persist to acked-stats.json even when the cycle folds a ComputationError"
        );
    }

    /*
     * Intent: a stats row whose path doesn't match any pool member and
     * whose devid is unknown to the pool no longer trips the fail-closed
     * latch when it carries zero counters. cmd_monitor returns
     * MonitorResult::Ok and writes no alert latch.
     *
     * Why it exists: previously, btrfs reporting a mapper path that wasn't
     * in monitor's path-to-devid map produced an UnmappedDeviceError, which
     * cmd_monitor latched as a ComputationError and surfaced as
     * MonitorResult::Alert. With dev.devid as the btrfs stats row key, an
     * unknown-path row is just a row with no acked baseline -- benign when
     * counters are zero. This test pins the regression: a stale row in
     * stats output must not fire a fail-closed beep.
     *
     * Scenario: btrfs device stats reports a row for /dev/mapper/braid-stale
     * with devid 99 (lingering from a prior configuration), zero counters.
     * Probe succeeds, pool is mounted, no real device errors.
     */
    #[test]
    fn stale_mapper_row_no_longer_latches_computation_error() {
        let (_dir, paths) = isolated_paths();
        let runner = MonitorTestRunner::with_stale_mapper_stats();

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert_eq!(
            result,
            MonitorResult::Ok,
            "stale-mapper row with zero counters must not trigger an alert"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "no alert latch must be written for a benign stale row"
        );
    }

    // Intent: a stats row whose devid is not in the pool's recognized set must
    //   not latch a BtrfsDeviceErrors cause even when it carries non-zero
    //   counters, and a follow-up monitor cycle must remain Ok -- no
    //   ack-induced loop.
    // Why it exists: the prior fix
    //   (stale_mapper_row_no_longer_latches_computation_error) only covered
    //   zero-counter stale rows. A non-zero counter row used to flow into
    //   compute_alert_state, latch BtrfsDeviceErrors { devid: stale }, and --
    //   once the operator ran braid ack -- snapshot_current would write an
    //   acked entry that the very next monitor cycle's reconcile_acked_stats
    //   would prune, re-firing the alert forever. Both passes must agree on
    //   which devids matter.
    // Scenario: btrfs device stats reports two healthy rows for devid 1 and 2
    //   plus a stale /dev/mapper/braid-stale row at devid 99 with non-zero
    //   read_io_errs / corruption_errs. Probe sees only devid 1 and 2 as
    //   present. monitor must return Ok and write no alert latch; a second
    //   monitor cycle on the same state must also return Ok.
    #[test]
    fn stale_mapper_row_with_errors_does_not_latch_or_loop() {
        let (_dir, paths) = isolated_paths();
        let runner = MonitorTestRunner::with_stale_mapper_errors();

        let first = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        assert_eq!(
            first,
            MonitorResult::Ok,
            "non-zero counters on an unrecognized devid must not latch an alert"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "no alert latch must be written for a stale-devid row"
        );

        let runner2 = MonitorTestRunner::with_stale_mapper_errors();
        let second = cmd_monitor(&runner2, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        assert_eq!(
            second,
            MonitorResult::Ok,
            "second cycle must remain Ok -- no reconcile/compute oscillation"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "no alert latch must appear on the second cycle either"
        );
    }

    // Intent: cmd_monitor latches exactly BtrfsDeviceErrors { devid } for a
    //   recognized, present pool member whose btrfs device stats row carries
    //   non-zero error counters, and the saved alert latch reloads to that same
    //   AlertState.
    // Why it exists: this is monitor's most fundamental detection path -- a
    //   real disk logging read/corruption errors -> beep. monitor.rs's own
    //   suite pins only the NEGATIVE device-error cases
    //   (stale_mapper_row_no_longer_latches_computation_error,
    //   stale_mapper_row_with_errors_does_not_latch_or_loop, both asserting
    //   Ok/no-latch) plus the other cause families. The positive path is
    //   exercised only incidentally as Phase-1 setup of ack.rs's
    //   ack_baseline_suppresses_then_refires_btrfs_device_errors; a refactor of
    //   that ack test that seeds its latch directly would erase monitor's only
    //   positive coverage, and a regression that inverted the recognized-devid
    //   filter would pass both monitor negatives. The latch reload-compare also
    //   pins a round-trip the ack test never makes (it only asserts the latch
    //   file exists).
    // Scenario: a mounted, recognized 2-disk pool; btrfs device stats reports
    //   non-zero read_io_errs/corruption_errs on devid 1 (present member
    //   /dev/mapper/braid-vdb) and clean counters on devid 2. monitor must
    //   latch exactly BtrfsDeviceErrors { devid: 1 } and persist it so
    //   alert-latch.json reloads to the returned state.
    #[test]
    fn cmd_monitor_latches_btrfs_device_errors_for_recognized_devid() {
        // Non-zero counters on recognized devid 1 (/dev/mapper/braid-vdb in
        // BTRFS_SHOW_2DISK); devid 2 clean. The healthy/stale fixtures only
        // zero recognized devids, so supply the payload via with_stats_payload.
        const STATS_DEVID1_ERRORS: &str = r#"{
    "__header": {"version": "1"},
    "device-stats": [
        {"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": 3, "flush_io_errs": 0, "corruption_errs": 1, "generation_errs": 0},
        {"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
    ]
}"#;

        let (_dir, paths) = isolated_paths();
        let runner = MonitorTestRunner::with_stats_payload(STATS_DEVID1_ERRORS);

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        // Exactly one cause, the right devid: proves the clean devid-2 row
        // contributed nothing and no spurious ComputationError was folded in.
        let state = alert_state(&result);
        assert_eq!(
            causes_only(state),
            vec![AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1),
            }],
            "recognized devid 1 with non-zero counters must latch exactly its btrfs error"
        );

        // The saved latch must round-trip to the same AlertState -- ack.rs's
        // Phase 1 only asserts the file exists, never reloads it for a
        // BtrfsDeviceErrors cause.
        let saved = alert::load_alert_latch(&paths).unwrap().unwrap();
        assert_eq!(
            &saved, state,
            "saved latch must match returned monitor alert"
        );
    }

    // Intent: cmd_monitor returns MonitorResult::Alert with exactly one
    //   ComputationError cause whose detail names "acked-stats" when
    //   acked-stats.json is unreadable / unparseable, and the corrupt
    //   bytes on disk are preserved byte-identical.
    // Why it exists: pins use of load_acked_stats_fallible (not the
    //   lossy load_acked_stats) on monitor's mutation path. Without it,
    //   cmd_monitor would treat a corrupt acked-stats.json as an empty
    //   baseline and silently return MonitorResult::Ok against an
    //   otherwise-healthy pool -- a fail-open hole in the fail-closed detector
    //   contract pinned by
    //   `docs/design/decisions/014-alerts.md#braid-monitor-is-a-pure-detector`.
    //   The byte-identity assertion also pins that monitor must not silently
    //   rewrite corrupt files (mirrors ack's sentinel-only retry no-rewrite
    //   guard).
    // Scenario: acked-stats.json was hand-edited to invalid JSON; the
    //   pool is mounted and healthy, btrfs device stats reports zero
    //   counters on both members. cmd_monitor must surface the
    //   corruption as a single ComputationError cause and leave the
    //   corrupt file on disk.
    #[test]
    fn cmd_monitor_corrupt_acked_stats_latches_computation_error() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.acked_stats_json(), b"not json").unwrap();
        let original_bytes = std::fs::read(paths.acked_stats_json()).unwrap();
        let runner = MonitorTestRunner::with_stale_mapper_stats();

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        let detail = assert_monitor_single_computation_error(&result);
        assert!(
            detail.contains("acked-stats"),
            "detail should name acked-stats failure, got {detail}"
        );

        assert!(
            paths.alert_latch_json().exists(),
            "alert latch must be written on acked-stats corruption"
        );
        let after_bytes = std::fs::read(paths.acked_stats_json()).unwrap();
        assert_eq!(
            after_bytes, original_bytes,
            "corrupt acked-stats must not be silently overwritten"
        );
    }

    /*
     * Intent: When probe_pool_alerts returns any non-NotBtrfs error, cmd_monitor
     * must latch a ComputationError and return MonitorResult::Alert. This
     * fixes the strictly-worse-than-the-original gap: today these paths
     * exit 2 with NO latch, so braid status shows nothing AND the speaker
     * stays silent.
     *
     * Why it exists: every ProbeError variant (Cmd, Parse, PoolDevice,
     * UnsupportedLuksVersion, mapper ownership) used to flow through
     * MonitorError::Probe -> exit 2 with no fail-closed signal. ADR 014
     * requires fail-closed: indeterminate pool state must beep.
     *
     * Scenario: mountinfo says the pool is mounted, but the btrfs filesystem
     * show command fails to spawn. probe_pool_alerts returns ProbeError::Cmd
     * before any pool device data is available.
     */
    #[test]
    fn probe_error_returns_alert_with_latched_computation_error() {
        let (_dir, paths) = isolated_paths();
        let runner = MonitorTestRunner::with_override(MonitorOverride::BtrfsShowResult(Err(
            CmdError::Failed("btrfs filesystem show: spawn failed".into()),
        )));

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        assert_monitor_single_computation_error(&result);

        let latch_path = paths.alert_latch_json();
        assert!(
            latch_path.exists(),
            "alert latch must be written on ProbeError"
        );
    }

    /*
     * Intent: a mountinfo IO failure latches ComputationError and returns
     * MonitorResult::Alert instead of reporting PoolOffline.
     *
     * Why it exists: this pins the bug fix at the safety-critical callsite.
     * Scenario: `/proc/self/mountinfo` is unreadable.
     */
    #[test]
    fn cmd_monitor_latches_computation_error_on_mountinfo_io_failure() {
        let (_dir, paths) = isolated_paths();
        let runner = MonitorTestRunner::with_stale_mapper_stats();

        let result = cmd_monitor(
            &runner,
            &monitor_fs_mountinfo_error(std::io::ErrorKind::PermissionDenied),
            &monitor_mp(),
            &paths,
        );
        let detail = assert_monitor_single_computation_error(&result);
        assert!(
            detail.contains("mountinfo error"),
            "detail should name mountinfo failure, got {detail}"
        );

        assert!(
            paths.alert_latch_json().exists(),
            "alert latch must be written on mountinfo IO failure"
        );
    }

    /*
     * Intent: BtrfsDeviceStatsJson failures -- both runner-level CmdError
     * and parse failure on malformed JSON -- must latch a ComputationError
     * and return MonitorResult::Alert. Two failure sites, one test.
     *
     * Why it exists: monitor.rs previously used `?` on both the
     * runner.run(BtrfsDeviceStatsJson) call and the
     * parse_btrfs_device_stats call, mapping CmdError/ParseError to
     * MonitorError::Cmd/MonitorError::Parse -> exit 2 with no latch. Same
     * fail-closed gap as the ProbeError case.
     *
     * Scenario: probe succeeds for a healthy 2-disk pool. Then either (a)
     * the btrfs device stats command fails to spawn / returns CmdError, or
     * (b) the command returns malformed JSON that parse_btrfs_device_stats
     * cannot decode. Either case must produce a single ComputationError
     * latched to disk.
     *
     * Note: braid's CommandRunner returns non-zero process exits as
     * Ok(RawCommandOutput) via `output_to_raw`; only spawn/signal
     * failures yield Err(CmdError::Failed). So the cmd-failure case must
     * construct Err(...) directly -- a non-zero exit payload would slip
     * into parse_btrfs_device_stats and accidentally exercise the parse
     * case instead.
     */
    #[test]
    fn stats_path_failures_return_alert_with_latched_computation_error() {
        let cases: Vec<(&str, Result<RawCommandOutput, CmdError>)> = vec![
            (
                "stats-cmd-failure",
                Err(CmdError::Failed("btrfs device stats: spawn failed".into())),
            ),
            (
                "stats-parse-failure",
                Ok(RawCommandOutput {
                    cmd: "test".to_owned(),
                    stdout: "not valid json {{{".to_owned(),
                    stderr: String::new(),
                    exit_status: 0,
                }),
            ),
        ];

        for (label, stats_result) in cases {
            let (_dir, paths) = isolated_paths();
            let runner =
                MonitorTestRunner::with_override(MonitorOverride::StatsResult(stats_result));

            let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
            assert_monitor_single_computation_error(&result);

            assert!(
                paths.alert_latch_json().exists(),
                "{label}: alert latch must be written"
            );
        }
    }

    // Intent: A btrfs stats failure and corrupt alert latch fold into exactly
    //   one ComputationError cause.
    // Why it exists: merge_into_latch keys every ComputationError into one
    //   slot, so emitting separate failure and quarantine causes would lose
    //   one detail and hide part of the incident from status.
    // Scenario: the pool is mounted, btrfs device stats fails, and the prior
    //   alert-latch.json is corrupt. Monitor must preserve the corrupt bytes
    //   while returning one combined ComputationError detail.
    #[test]
    fn stats_failure_with_corrupt_alert_latch_folds_one_computation_error() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.alert_latch_json(), b"not json").unwrap();
        let runner = MonitorTestRunner::with_override(MonitorOverride::StatsResult(Err(
            CmdError::Failed("btrfs device stats: spawn failed".into()),
        )));

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        let detail = assert_monitor_single_computation_error(&result);
        assert!(
            detail.contains("btrfs device stats: spawn failed"),
            "detail should include stats failure, got {detail}"
        );
        assert!(
            detail.contains("previous alert latch was unreadable -- quarantined"),
            "detail should include alert latch quarantine, got {detail}"
        );

        let sidecar = std::fs::read(paths.alert_latch_corrupt()).unwrap();
        assert_eq!(
            sidecar,
            b"not json".to_vec(),
            "corrupt alert latch bytes must be preserved"
        );
    }

    // Intent: A corrupt alert latch alone -- with healthy probe and stats
    //   paths -- latches a ComputationError and returns MonitorResult::Alert,
    //   quarantining the corrupt bytes to the sidecar.
    // Why it exists: pins the (None, Some(latch_detail)) branch of
    //   folded_computation_error_detail. The combined corrupt-latch test only
    //   exercises the (Some, Some) branch, so a regression in the latch-alone
    //   path could silently pass while the manual still promises a
    //   beeper-triggering alert for alert-latch failure alone.
    // Scenario: pool is mounted, probe and btrfs device stats succeed cleanly,
    //   but alert-latch.json is corrupt after a partial write or hand edit.
    //   cmd_monitor must quarantine the corrupt bytes, return
    //   MonitorResult::Alert with one ComputationError whose detail names the
    //   latch quarantine, and write a fresh latch.
    #[test]
    fn cmd_monitor_corrupt_alert_latch_latches_computation_error() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.alert_latch_json(), b"not json").unwrap();
        let runner = MonitorTestRunner::with_stale_mapper_stats();

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        let detail = assert_monitor_single_computation_error(&result);
        assert!(
            detail.contains("previous alert latch was unreadable -- quarantined"),
            "detail should name alert latch quarantine, got {detail}"
        );

        let sidecar = std::fs::read(paths.alert_latch_corrupt()).unwrap();
        assert_eq!(
            sidecar,
            b"not json".to_vec(),
            "corrupt alert latch bytes must be preserved"
        );
        assert!(
            paths.alert_latch_json().exists(),
            "fresh alert latch must be written with ComputationError cause"
        );
    }

    // Intent: A stats failure preserves an already-latched non-ComputationError
    //   cause and adds exactly one ComputationError cause.
    // Why it exists: the refactor must keep the latch merge path shared while
    //   preserving latched operator-visible alerts until ack clears them.
    // Scenario: a prior monitor pass latched MissingDevice, then the next pass
    //   cannot compute fresh btrfs stats. The returned state and saved latch
    //   must both contain MissingDevice plus one fail-closed computation error.
    #[test]
    fn stats_failure_merges_existing_non_computation_latch_once() {
        let (_dir, paths) = isolated_paths();
        let existing = alert::AlertState {
            causes: vec![latch_entry(AlertCause::MissingDevice {
                devid: Devid::new(7),
            })],
        };
        alert::save_alert_latch(&existing, &paths).unwrap();
        let runner = MonitorTestRunner::with_override(MonitorOverride::StatsResult(Err(
            CmdError::Failed("btrfs device stats: spawn failed".into()),
        )));

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        let state = alert_state(&result);
        assert_eq!(
            state
                .causes
                .iter()
                .filter(|cause| matches!(&cause.cause, AlertCause::MissingDevice { devid } if *devid == Devid::new(7)))
                .count(),
            1,
            "original MissingDevice cause must remain latched"
        );
        let computation_details = computation_error_details(state);
        assert_eq!(
            computation_details.len(),
            1,
            "expected exactly one ComputationError, got {:?}",
            state.causes
        );
        assert!(
            computation_details[0].contains("btrfs device stats: spawn failed"),
            "detail should include stats failure, got {}",
            computation_details[0]
        );

        let saved = alert::load_alert_latch(&paths).unwrap().unwrap();
        assert_eq!(
            &saved, state,
            "saved latch must match returned monitor alert"
        );
    }

    // Intent: A fully healthy cycle -- probe ok, stats ok, no smartd flag --
    //   still loads, merges, and re-persists a pre-existing active latch, so
    //   an alert survives until braid ack even after the triggering condition
    //   resolves.
    // Why it exists: ADR 014's sticky-latch invariant is pinned at cmd_monitor
    //   level only via stats_failure_merges_existing_non_computation_latch_once,
    //   which exercises the failure-detail branch.
    //   merge_no_new_causes_carries_forward_latched covers the helper in
    //   isolation. A regression that gated merge_into_latch on
    //   failure_detail.is_some(), passed None for existing_latch on the success
    //   branch, or skipped the latch load entirely would compile and pass every
    //   other monitor unit test while silently violating latched-until-ack on a
    //   recovered pool.
    // Scenario: a prior cycle latched MissingDevice { devid: 7 }, then the next
    //   cycle finds the pool healthy -- btrfs reports the same two members with
    //   zero counters, no smartd flag, no probe failure. monitor must return
    //   MonitorResult::Alert carrying MissingDevice { devid: 7 } and re-persist
    //   it so alert-latch.json reloads to the returned state.
    #[test]
    fn healthy_cycle_carries_forward_existing_non_computation_latch() {
        let (_dir, paths) = isolated_paths();
        let existing = alert::AlertState {
            causes: vec![latch_entry(AlertCause::MissingDevice {
                devid: Devid::new(7),
            })],
        };
        alert::save_alert_latch(&existing, &paths).unwrap();

        let result = cmd_monitor(
            &MonitorTestRunner::with_stale_mapper_stats(),
            &monitor_fs_btrfs(),
            &monitor_mp(),
            &paths,
        );

        let state = alert_state(&result);
        assert_eq!(
            causes_only(state),
            vec![AlertCause::MissingDevice {
                devid: Devid::new(7),
            }],
            "healthy cycle must carry forward the latched cause unchanged"
        );

        let saved = alert::load_alert_latch(&paths).unwrap().unwrap();
        assert_eq!(
            &saved, state,
            "saved latch must match returned monitor alert"
        );
    }

    /*
     * Intent: When mountinfo reports the mount target is held by a non-btrfs
     * filesystem, cmd_monitor must return MonitorResult::PoolOffline and
     * leave no alert latch behind.
     *
     * Why it exists: pins the only non-fail-closed arm of the exhaustive
     * ProbeError match. If a future refactor silently flips NotBtrfs to the
     * fail-closed branch, monitor would start beeping every time a non-btrfs
     * filesystem (e.g. a stale ext4 partition) is mounted at the configured
     * mount point -- a false-alarm regression for the headless surface.
     *
     * Scenario: the operator's mount target /mnt/storage is currently held
     * by an ext4 filesystem (perhaps left over from an OS reinstall, or
     * because someone mounted the wrong thing). probe_pool_alerts returns
     * ProbeError::NotBtrfs.
     */
    #[test]
    fn monitor_classifies_non_btrfs_mount_as_offline() {
        let (_dir, paths) = isolated_paths();
        let runner = MonitorTestRunner::with_stale_mapper_stats();

        let result = cmd_monitor(&runner, &monitor_fs_ext4(), &monitor_mp(), &paths);

        assert_eq!(result, MonitorResult::PoolOffline);
        assert!(
            !paths.alert_latch_json().exists(),
            "PoolOffline must not write an alert latch"
        );
    }

    /*
     * Intent: When `/proc/self/mountinfo` is well-formed but has no entry
     * for the configured mount point, cmd_monitor must return
     * MonitorResult::PoolOffline and leave no alert latch behind.
     *
     * Why it exists: pins the only other non-fail-closed arm besides
     * NotBtrfs. ADR 014 distinguishes this case (legitimate offline,
     * exit 0) from any mountinfo IO/malformed/duplicate failure
     * (ProbeError::MountInfo, fail-closed, exit 1). The probe layer is
     * already pinned by probe_pool_alerts_unmounted, but no integration
     * test pins how cmd_monitor classifies the Ok(p) + pool.mounted=false
     * branch -- an over-eager refactor that drops the `if !pool.mounted`
     * early return or flips the probe's None arm to Err would compile
     * clean and start the beeper on every offline timer cycle.
     *
     * Scenario: the NAS has booted but the encrypted pool has not been
     * unlocked or mounted yet, so mountinfo has no /mnt/storage entry.
     */
    #[test]
    fn monitor_classifies_unmounted_as_offline() {
        let (_dir, paths) = isolated_paths();
        let runner = MonitorTestRunner::with_stale_mapper_stats();

        let result = cmd_monitor(&runner, &monitor_fs_not_mounted(), &monitor_mp(), &paths);

        assert_eq!(result, MonitorResult::PoolOffline);
        assert!(
            !paths.alert_latch_json().exists(),
            "PoolOffline must not write an alert latch"
        );
    }

    // Intent: An already-active alert latch survives a monitor cycle that
    //   concludes PoolOffline; the early Ok(None) return must leave
    //   alert-latch.json byte-for-byte untouched, and it must still reload to
    //   the seeded alert.
    // Why it exists: The PoolOffline early return fires before the latch
    //   load/merge/save path, so ADR 014's sticky-latch invariant here rests on
    //   that path not touching the file. Existing offline tests start with no
    //   latch and only assert none is created. A refactor that moved the latch
    //   load above the early return, or called alert::remove_alert_latch on the
    //   offline path as cmd_ack does for a genuine offline ack, would silently
    //   drop an in-flight alert: the beeper keeps sounding while `braid status`
    //   goes quiet.
    // Scenario: A prior cycle latched MissingDevice { devid: 7 } and the
    //   beeper is sounding; the operator's pool briefly unmounts so the next
    //   cycle sees an empty mountinfo.
    #[test]
    fn unmounted_pool_preserves_existing_alert_latch() {
        let (_dir, paths) = isolated_paths();
        let existing = alert::AlertState {
            causes: vec![latch_entry(AlertCause::MissingDevice {
                devid: Devid::new(7),
            })],
        };
        alert::save_alert_latch(&existing, &paths).unwrap();
        let before = std::fs::read(paths.alert_latch_json()).unwrap();

        let result = cmd_monitor(
            &MonitorTestRunner::with_stale_mapper_stats(),
            &monitor_fs_not_mounted(),
            &monitor_mp(),
            &paths,
        );

        assert_eq!(result, MonitorResult::PoolOffline);
        let after = std::fs::read(paths.alert_latch_json()).unwrap();
        assert_eq!(
            after, before,
            "an offline cycle must leave an active alert latch byte-for-byte untouched"
        );
        assert_eq!(
            alert::load_alert_latch(&paths).unwrap().unwrap(),
            existing,
            "the latched MissingDevice alert must survive the offline cycle"
        );
    }

    /*
     * Intent: When btrfs filesystem show exits non-zero with non-empty
     * stderr, parse_btrfs_filesystem_show maps that to
     * ParseError::CommandFailed,
     * probe_pool_alerts wraps it as ProbeError::Parse, and cmd_monitor must
     * latch a ComputationError and return MonitorResult::Alert.
     *
     * Why it exists: the existing probe_error_returns_alert_... test
     * covers ProbeError::Cmd (spawn/signal failure -- Err(CmdError) at
     * the runner boundary). This test covers the second reachable arm of
     * the catch-all, ProbeError::Parse, which is wired through a
     * RawCommandOutput-shaped failure (non-zero exit + stderr) rather
     * than a CmdError. Both arms must be pinned to fail-closed.
     *
     * Scenario: btrfs filesystem show exits 1 with a stderr message. The
     * runner returns Ok(RawCommandOutput { exit_status: 1, stderr: "<msg>",
     * ... }), and the btrfs-show parser returns ParseError::CommandFailed.
     */
    #[test]
    fn probe_parse_failure_returns_alert_with_latched_computation_error() {
        let (_dir, paths) = isolated_paths();
        let show_failure = RawCommandOutput {
            cmd: "btrfs filesystem show /mnt/storage".to_owned(),
            stdout: String::new(),
            stderr: "ERROR: cannot read filesystem info\n".to_owned(),
            exit_status: 1,
        };
        let runner =
            MonitorTestRunner::with_override(MonitorOverride::BtrfsShowResult(Ok(show_failure)));

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        assert_monitor_single_computation_error(&result);

        assert!(
            paths.alert_latch_json().exists(),
            "alert latch must be written on ProbeError::Parse"
        );
    }

    /*
     * Intent: When btrfs filesystem show reports a device path that is
     * not /dev/mapper/-prefixed, probe_pool_alerts returns ProbeError::PoolDevice;
     * cmd_monitor must latch a ComputationError and return
     * MonitorResult::Alert.
     *
     * Why it exists: PoolDevice is the third reachable variant from
     * probe_pool_alerts (alongside Cmd and Parse). Without this test, a future
     * edit that wrongly maps PoolDevice to PoolOffline would still compile
     * (the match remains exhaustive) and no other test would catch the
     * regression -- breaking the fail-closed contract for the
     * non-/dev/mapper path / inactive-mapper class of pool-show
     * inconsistencies.
     *
     * Scenario: btrfs filesystem show reports a single-disk pool whose
     * device path is /dev/sda1 (raw block device, no LUKS mapper).
     * probe_pool_alerts' invariant -- every pool device must live under
     * /dev/mapper/ -- fails.
     */
    #[test]
    fn probe_pool_device_failure_returns_alert_with_latched_computation_error() {
        let (_dir, paths) = isolated_paths();
        let btrfs_show_non_mapper = "Label: none  uuid: de2b8517-f972-45fc-b121-3e160c8ea432\n\
            \tTotal devices 1 FS bytes used 16.17MiB\n\
            \tdevid    1 size 1008.00MiB used 209.50MiB path /dev/sda1\n";
        let runner = MonitorTestRunner::with_override(MonitorOverride::BtrfsShowPayload(
            btrfs_show_non_mapper.to_owned(),
        ));

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        assert_monitor_single_computation_error(&result);

        assert!(
            paths.alert_latch_json().exists(),
            "alert latch must be written on ProbeError::PoolDevice"
        );
    }

    // Intent: cmd_monitor must thread the smartd flag into alert computation
    // and persist the resulting SmartdAlert cause for a mounted healthy pool.
    // Why it exists: helper tests cover the flag reader and compute helper in
    // isolation, but not the command wiring that merges and saves live causes.
    // Scenario: smartd touched its alert flag while the pool remains mounted
    // and otherwise healthy, so the monitor timer must latch exactly one SMART
    // cause for the headless alert wrapper.
    #[test]
    fn cmd_monitor_latches_smartd_alert_when_mounted() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.smartd_alert(), b"").unwrap();

        let result = cmd_monitor(
            &MonitorTestRunner::with_stale_mapper_stats(),
            &monitor_fs_btrfs(),
            &monitor_mp(),
            &paths,
        );
        let state = alert_state(&result);
        assert_eq!(causes_only(state), vec![AlertCause::SmartdAlert]);

        let saved = alert::load_alert_latch(&paths).unwrap().unwrap();
        assert_eq!(
            &saved, state,
            "saved latch must match returned monitor alert"
        );
    }

    // Intent: cmd_monitor threads the scrub-failed flag into alert computation
    //   and persists a single ScrubFailed cause for a mounted healthy pool.
    // Why it exists: mirrors the smartd command-wiring test -- helper tests
    //   cover the flag reader and compute helper in isolation, but not that
    //   cmd_monitor reads the flag, merges it, and saves the latch.
    // Scenario: braid-scrub-failed.service touched the flag (onFailure) while
    //   the pool is mounted and otherwise healthy.
    #[test]
    fn cmd_monitor_latches_scrub_failed_when_mounted() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.scrub_failed(), b"").unwrap();

        let result = cmd_monitor(
            &MonitorTestRunner::with_stale_mapper_stats(),
            &monitor_fs_btrfs(),
            &monitor_mp(),
            &paths,
        );
        let state = alert_state(&result);
        assert_eq!(causes_only(state), vec![AlertCause::ScrubFailed]);
        assert_eq!(
            state.severity(),
            Some(AlertSeverity::Critical),
            "a failed scrub is a Critical (beeping) cause"
        );

        let saved = alert::load_alert_latch(&paths).unwrap().unwrap();
        assert_eq!(
            &saved, state,
            "saved latch must match returned monitor alert"
        );
    }

    // Intent: the scrub-failed flag, present across two monitor cycles, yields
    //   exactly ONE latched ScrubFailed cause.
    // Why it exists (the dedup regression the single-cycle mirrors miss): the
    //   flag persists from onFailure until ack, so without the
    //   same_cause_key ScrubFailed singleton arm each cycle would append a fresh
    //   duplicate and the latch would grow unbounded. The single-cycle
    //   push/latch test passes regardless; only this two-cycle assertion catches
    //   the missing arm.
    // Scenario: braid-scrub-failed.service set the flag; two monitor timer
    //   cycles run before the operator acks.
    #[test]
    fn cmd_monitor_scrub_failed_latches_single_cause_across_two_cycles() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.scrub_failed(), b"").unwrap();

        let first = cmd_monitor(
            &MonitorTestRunner::with_stale_mapper_stats(),
            &monitor_fs_btrfs(),
            &monitor_mp(),
            &paths,
        );
        assert_eq!(
            causes_only(alert_state(&first)),
            vec![AlertCause::ScrubFailed]
        );

        // Flag still set (monitor never removes it -- only ack does).
        let second = cmd_monitor(
            &MonitorTestRunner::with_stale_mapper_stats(),
            &monitor_fs_btrfs(),
            &monitor_mp(),
            &paths,
        );
        assert_eq!(
            causes_only(alert_state(&second)),
            vec![AlertCause::ScrubFailed],
            "a flag across two cycles must latch exactly one ScrubFailed, not grow the latch"
        );
    }

    // Intent: cmd_monitor_at stamps a newly latched cause with the injected
    //   cycle clock, and a later cycle that re-detects the same cause leaves
    //   detected_at at the first cycle's time on disk.
    // Why it exists: the first-detection guarantee must hold end-to-end across
    //   real monitor cycles (load -> merge -> save), not only in the merge
    //   helper. A regression that re-stamped on the refresh path would make every
    //   latched alert read as freshly detected on the next cycle, defeating the
    //   "historical incident vs live fact" signal the timestamp exists to give.
    // Scenario: smartd's flag is set; the monitor runs at t0 (latching
    //   SmartdAlert) and again at t1 > t0 with the flag still present.
    #[test]
    fn cmd_monitor_at_stamps_first_detection_and_keeps_it_across_cycles() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.smartd_alert(), b"").unwrap();
        let t0 = UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        let t1 = t0 + std::time::Duration::from_secs(7200);
        let expected_ts = crate::util::format_rfc3339_utc_seconds(t0);

        let first = cmd_monitor_at(
            &MonitorTestRunner::with_stale_mapper_stats(),
            &monitor_fs_btrfs(),
            &monitor_mp(),
            &paths,
            t0,
        );
        let first_state = alert_state(&first);
        assert_eq!(causes_only(first_state), vec![AlertCause::SmartdAlert]);
        assert_eq!(
            first_state.causes[0].detected_at, expected_ts,
            "cycle 1 must stamp detected_at with the t0 clock"
        );

        let second = cmd_monitor_at(
            &MonitorTestRunner::with_stale_mapper_stats(),
            &monitor_fs_btrfs(),
            &monitor_mp(),
            &paths,
            t1,
        );
        assert_eq!(
            causes_only(alert_state(&second)),
            vec![AlertCause::SmartdAlert]
        );

        let saved = alert::load_alert_latch(&paths).unwrap().unwrap();
        assert_eq!(
            saved.causes[0].detected_at, expected_ts,
            "re-detecting the same cause at t1 must keep the first-detection time on disk"
        );
    }

    // Intent: cmd_monitor must classify an unmounted pool as offline before
    // consulting or latching the smartd alert flag.
    // Why it exists: without this command-level check, a refactor could let a
    // stale smartd flag make an offline pool beep about SMART instead of
    // returning the quiet PoolOffline state.
    // Scenario: the NAS has booted but the encrypted pool is still locked and
    // unmounted, while an old smartd alert flag remains in braid state.
    #[test]
    fn cmd_monitor_offline_pool_ignores_smartd_flag() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.smartd_alert(), b"").unwrap();

        let result = cmd_monitor(
            &MonitorTestRunner::with_stale_mapper_stats(),
            &monitor_fs_not_mounted(),
            &monitor_mp(),
            &paths,
        );

        assert_eq!(result, MonitorResult::PoolOffline);
        assert!(
            !paths.alert_latch_json().exists(),
            "offline pool must not latch a smartd alert"
        );
    }

    // --- ENOSPC-risk monitor integration (Step 3) ---

    // Intent: a pool that crosses into RAID1 chunk-pair ENOSPC risk latches
    //   exactly one EnospcRisk cause at Warning severity, and the latch round
    //   trips.
    // Why it exists: this is the proactive-alert path's reason to exist -- an
    //   unattended filling pool must warn before the first allocation failure,
    //   and at a non-beeping tier.
    // Scenario: a healthy 2-disk pool fills until device 1 drops to 100 MiB
    //   unallocated, below the 1 GiB threshold.
    #[test]
    fn cmd_monitor_enters_enospc_risk_warns_without_beep() {
        let (_dir, paths) = isolated_paths();
        let runner = MonitorTestRunner::with_usage_payload(usage_atrisk());

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        let state = alert_state(&result);
        assert_eq!(
            causes_only(state),
            vec![AlertCause::EnospcRisk {
                margin: 100 * MIB as i64 - GIB as i64,
                count_below: 1,
                device_count: 2,
            }],
            "exactly one EnospcRisk cause with the deepest-device margin"
        );
        assert_eq!(
            state.severity(),
            Some(AlertSeverity::Warning),
            "ENOSPC risk is a non-beeping Warning"
        );
        let saved = alert::load_alert_latch(&paths).unwrap().unwrap();
        assert_eq!(&saved, state, "latch must round-trip the EnospcRisk cause");
    }

    // Intent: a usage-probe failure skips only the EnospcRisk cause -- a
    //   concurrent device-error cause still latches, and no ComputationError is
    //   folded from the usage failure.
    // Why it exists (key test): this pins the single scoped fail-open exception
    //   to the fail-closed mandate. A regression that propagated the usage
    //   failure into the `?` path would latch ComputationError and mask the real
    //   device-error signal under a generic beep.
    // Scenario: device 1 logs read/corruption errors while the btrfs device
    //   usage probe fails to spawn in the same cycle.
    #[test]
    fn cmd_monitor_usage_probe_failure_isolated_from_device_errors() {
        const STATS_DEVID1_ERRORS: &str = r#"{
    "__header": {"version": "1"},
    "device-stats": [
        {"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": 3, "flush_io_errs": 0, "corruption_errs": 1, "generation_errs": 0},
        {"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
    ]
}"#;
        let (_dir, paths) = isolated_paths();
        let runner = MonitorTestRunner::with_stats_payload_and_usage(
            STATS_DEVID1_ERRORS,
            MonitorOverride::UsageResult(Err(CmdError::Failed(
                "btrfs device usage: spawn failed".into(),
            ))),
        );

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        let state = alert_state(&result);
        assert_eq!(
            causes_only(state),
            vec![AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1)
            }],
            "device-error cause survives; no EnospcRisk, no ComputationError folded"
        );
    }

    // Intent: a usage-probe failure on an otherwise-healthy pool does not raise a
    //   spurious alert and leaves a present baseline untouched.
    // Why it exists: the other skip-without-evaluating path (probe failure) must
    //   also leave the baseline alone -- it never reaches the re-arm branch.
    // Scenario: a healthy pool's usage probe fails while a matching-key baseline
    //   from a prior at-risk episode sits on disk.
    #[test]
    fn cmd_monitor_usage_probe_failure_alone_is_ok_and_keeps_baseline() {
        let (_dir, paths) = isolated_paths();
        seed_enospc_baseline(&paths, matching_pool_key(), open_snooze_deadline());
        let runner = MonitorTestRunner::with_override(MonitorOverride::UsageResult(Err(
            CmdError::Failed("btrfs device usage: spawn failed".into()),
        )));

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert_eq!(
            result,
            MonitorResult::Ok,
            "no spurious alert on probe failure"
        );
        assert!(
            paths.enospc_ack_json().exists(),
            "probe-failure skip must not touch the baseline"
        );
    }

    // Intent: a matching-key marker whose snooze window is still open suppresses a
    //   fresh EnospcRisk while the pool stays at risk.
    // Why it exists: the snooze's whole job is to stop re-nagging an acked pool for
    //   one reminder interval; an open window must keep the monitor quiet and leave
    //   the marker in place.
    // Scenario: a pool acked moments ago (deadline half an interval out) is still
    //   at risk on the next monitor cycle.
    #[test]
    fn cmd_monitor_suppresses_enospc_within_snooze() {
        let (_dir, paths) = isolated_paths();
        seed_enospc_baseline(&paths, matching_pool_key(), open_snooze_deadline());
        let runner = MonitorTestRunner::with_usage_payload(usage_atrisk());

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert_eq!(
            result,
            MonitorResult::Ok,
            "an open snooze window must suppress"
        );
        assert!(
            paths.enospc_ack_json().exists(),
            "suppression leaves the marker in place"
        );
    }

    // Intent: a clean snoozed baseline does not suppress when only the later
    //   usage probe has seen a missing device.
    // Why it exists: the accepted show-vs-usage skew must take the safe
    //   confirmed-mismatch direction -- fire armed and clear the baseline -- even
    //   inside an open snooze window.
    // Scenario: show still reports both devices present, usage reports devid 2 as
    //   `<missing disk>` with device_size 0, and the stored baseline was taken on
    //   the clean both-present key.
    #[test]
    fn cmd_monitor_clean_enospc_baseline_fires_and_clears_under_usage_skew() {
        let (_dir, paths) = isolated_paths();
        seed_enospc_baseline(&paths, matching_pool_key(), open_snooze_deadline());
        let runner = MonitorTestRunner::with_usage_payload(usage_2disk_one_missing());

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert!(
            has_enospc_cause(&result),
            "usage-skewed live key must fire instead of suppressing, got {result:?}"
        );
        assert!(
            load_enospc_ack(&paths).unwrap().is_none(),
            "the confirmed key mismatch must clear the baseline"
        );
    }

    // Intent: a snoozed baseline whose stored key contains btrfs's missing marker
    //   is rejected before the matching-key suppression branch can honor it.
    // Why it exists: pre-fix ack could persist `(devid, 0)` during the same
    //   show-vs-usage skew. A later skewed monitor cycle would otherwise match
    //   that poisoned key and suppress a real ENOSPC risk inside the snooze.
    // Scenario: a legacy poisoned marker is on disk, show still reports both
    //   devices present, and usage re-derives the same zero-sized key.
    #[test]
    fn cmd_monitor_zero_sized_enospc_baseline_fires_and_clears() {
        let (_dir, paths) = isolated_paths();
        seed_enospc_baseline(&paths, missing_pool_key(), open_snooze_deadline());
        let runner = MonitorTestRunner::with_usage_payload(usage_2disk_one_missing());

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert!(
            has_enospc_cause(&result),
            "zero-sized baseline must fire instead of suppressing, got {result:?}"
        );
        assert!(
            load_enospc_ack(&paths).unwrap().is_none(),
            "the poisoned baseline must be removed"
        );
    }

    // Intent: a matching-key marker whose snooze deadline has elapsed re-fires
    //   EnospcRisk while the pool is still at risk.
    // Why it exists: once the reminder interval passes, an acked-but-still-at-risk
    //   pool must remind again every cycle until a re-ack -- the snooze is a
    //   reminder, not a permanent mute.
    // Scenario: a pool acked over a reminder interval ago (deadline far in the
    //   past) is still at risk.
    #[test]
    fn cmd_monitor_refires_enospc_after_snooze_elapsed() {
        let (_dir, paths) = isolated_paths();
        seed_enospc_baseline(&paths, matching_pool_key(), 1);
        let runner = MonitorTestRunner::with_usage_payload(usage_atrisk());

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert!(
            has_enospc_cause(&result),
            "an elapsed snooze must re-fire, got {result:?}"
        );
    }

    // Intent: after an elapsed snooze re-fires EnospcRisk, a real re-ack stamps a
    //   fresh deadline and the next monitor cycle goes quiet again.
    // Why it exists: pins the reminder loop's reset edge -- re-acking a
    //   still-at-risk pool must re-open the snooze window (not stay latched on),
    //   exercising the real ack -> monitor handoff at the cmd level.
    // Scenario: an acked pool's reminder elapses and the monitor reminds; the
    //   operator runs braid ack again while the pool is still at risk.
    #[test]
    fn cmd_monitor_after_reack_is_snoozed_again() {
        let (_dir, paths) = isolated_paths();
        seed_enospc_baseline(&paths, matching_pool_key(), 1); // elapsed
        let runner = MonitorTestRunner::with_usage_payload(usage_atrisk());

        let fired = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        assert!(
            has_enospc_cause(&fired),
            "an elapsed snooze must re-fire before re-ack, got {fired:?}"
        );

        cmd_ack_impl(
            &runner,
            &monitor_fs_btrfs(),
            &monitor_mp(),
            &paths,
            &ack_noop_beeper,
        )
        .expect("re-ack of a still-at-risk pool must succeed");

        let quiet = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        assert_eq!(
            quiet,
            MonitorResult::Ok,
            "re-ack must re-open the snooze window so the monitor goes quiet, got {quiet:?}"
        );
        assert!(
            now_secs() < load_enospc_ack(&paths).unwrap().unwrap().snoozed_until,
            "re-ack stamps a fresh future deadline (not a vacuous pass)"
        );
    }

    // Intent: with no snooze marker on disk, an at-risk pool fires EnospcRisk
    //   immediately (armed).
    // Why it exists (F1): pins the "recurrence after a healthy ack alerts
    //   immediately" half of contract #5 -- a healthy ack writes no marker, so the
    //   next time the pool re-enters risk the monitor must fire at once, not stay
    //   silent. Pairs with cmd_ack_mounted_enospc_healthy_at_ack_writes_no_snooze.
    // Scenario: a pool that recovered and was acked healthy (no marker) later fills
    //   back into risk.
    #[test]
    fn cmd_monitor_at_risk_no_marker_fires_armed() {
        let (_dir, paths) = isolated_paths();
        assert!(
            !paths.enospc_ack_json().exists(),
            "precondition: no snooze marker on disk"
        );
        let runner = MonitorTestRunner::with_usage_payload(usage_atrisk());

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert!(
            has_enospc_cause(&result),
            "no marker -> at-risk pool fires armed, got {result:?}"
        );
    }

    // Intent: a structurally-valid OLD-shape marker ({ pool_key, baseline_margin }
    //   with no snoozed_until) is treated as corrupt -- an at-risk cycle fires
    //   EnospcRisk armed and clears the file.
    // Why it exists (F3): pins the "no on-disk migration" claim. The unreleased
    //   margin-baseline file fails to deserialize (snoozed_until missing), so it
    //   falls through the existing corrupt-marker path (fire armed + remove). A
    //   future #[serde(default)] on snoozed_until would silently turn it into a
    //   deadline-0 (elapsed) marker that still fires but would NOT be removed --
    //   this test fails loudly on that slip.
    // Scenario: a NAS upgraded across this change still has an old margin-shaped
    //   enospc-ack.json on disk while the pool is at risk.
    #[test]
    fn cmd_monitor_old_margin_shaped_marker_fires_armed_and_clears() {
        let (_dir, paths) = isolated_paths();
        let key = matching_pool_key();
        let pool_key_value = serde_json::to_value(&key).unwrap();
        let old_shape = serde_json::json!({
            "pool_key": pool_key_value,
            "baseline_margin": -2048,
        });
        std::fs::write(
            paths.enospc_ack_json(),
            serde_json::to_vec(&old_shape).unwrap(),
        )
        .unwrap();
        let runner = MonitorTestRunner::with_usage_payload(usage_atrisk());

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert!(
            has_enospc_cause(&result),
            "old margin-shaped marker must fire armed, got {result:?}"
        );
        assert!(
            !paths.enospc_ack_json().exists(),
            "old margin-shaped marker (missing snoozed_until) is corrupt -> removed"
        );
    }

    // Intent: a predicate-healthy pool re-arms (drops the baseline) even when one
    //   device is far below the raw threshold, and a later at-risk cycle fires
    //   fresh.
    // Why it exists (F2): re-arm must key off the predicate margin, not raw min
    //   headroom -- a fault-tolerant 4-disk pool with one near-empty device is
    //   healthy and must clear its baseline so a future recurrence alerts fresh.
    // Scenario: an acked pool recovers to a large positive margin (4-disk,
    //   one-low healthy), then later re-enters risk.
    #[test]
    fn cmd_monitor_rearms_on_predicate_health_then_refires() {
        let (_dir, paths) = isolated_paths();
        seed_enospc_baseline(&paths, matching_pool_key(), open_snooze_deadline());
        let healthy = MonitorTestRunner::with_usage_payload(usage_4disk_one_low());

        let rearm = cmd_monitor(&healthy, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        assert_eq!(rearm, MonitorResult::Ok, "predicate-healthy pool re-arms");
        assert!(
            !paths.enospc_ack_json().exists(),
            "re-arm must remove the baseline (keyed on predicate margin, not raw headroom)"
        );

        let atrisk = MonitorTestRunner::with_usage_payload(usage_atrisk());
        let refire = cmd_monitor(&atrisk, &monitor_fs_btrfs(), &monitor_mp(), &paths);
        assert!(
            has_enospc_cause(&refire),
            "a fresh at-risk cycle after re-arm must fire armed, got {refire:?}"
        );
    }

    // Intent: a predicate-healthy re-arm clears only the post-ack snooze marker;
    //   a previously-latched EnospcRisk cause stays latched (sticky-until-ack)
    //   and round-trips.
    // Why it exists: ADR 014 says re-arm differs from sticky-latch only in the
    //   post-ack marker. Existing integration coverage drives this branch with a
    //   MissingDevice latch, while EnospcRisk carry-forward is pinned only at the
    //   merge helper. A cause-specific re-arm filter that drops only EnospcRisk
    //   would leave those tests green.
    // Scenario: a prior cycle latched EnospcRisk and the operator snoozed it
    //   (marker on disk); the pool's predicate margin then recovers to healthy.
    #[test]
    fn cmd_monitor_rearm_carries_forward_latched_enospc_risk() {
        let (_dir, paths) = isolated_paths();
        let latched = AlertCause::EnospcRisk {
            margin: -42,
            count_below: 1,
            device_count: 2,
        };
        alert::save_alert_latch(
            &alert::AlertState {
                causes: vec![latch_entry(latched.clone())],
            },
            &paths,
        )
        .unwrap();
        seed_enospc_baseline(&paths, matching_pool_key(), open_snooze_deadline());
        let runner = MonitorTestRunner::with_usage_payload(usage_4disk_one_low());

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert!(
            !paths.enospc_ack_json().exists(),
            "re-arm must remove the snooze marker"
        );
        let state = alert_state(&result);
        assert_eq!(
            causes_only(state),
            vec![latched],
            "latched EnospcRisk must carry forward across re-arm (sticky-until-ack)"
        );
        let saved = alert::load_alert_latch(&paths).unwrap().unwrap();
        assert_eq!(
            &saved, state,
            "latch must round-trip the carried-forward EnospcRisk"
        );
    }

    // Intent: a baseline whose pool_key no longer matches the live pool is
    //   discarded (not allowed to suppress), across all three mismatch axes:
    //   changed devid set, changed FS UUID, and same-devid changed device_size.
    // Why it exists (F1): a stale baseline from a bootstrap/membership/geometry
    //   change must never silence a fresh risk. The device_size axis is the one
    //   this round closes -- `fsid + devids` alone would still match a
    //   same-devid `braid replace`/resize.
    // Scenario: an at-risk pool carries a baseline acked on an old topology.
    #[test]
    fn cmd_monitor_stale_baseline_key_mismatch_fires_and_clears() {
        let cases: Vec<(&str, PoolKey)> = vec![
            (
                "changed-devid-set",
                PoolKey {
                    fsid: fsid(MONITOR_FSID),
                    devices: vec![
                        (Devid::new(1), USAGE_DEVICE_SIZE),
                        (Devid::new(2), USAGE_DEVICE_SIZE),
                        (Devid::new(99), USAGE_DEVICE_SIZE),
                    ],
                },
            ),
            (
                "changed-fs-uuid",
                PoolKey {
                    fsid: fsid("ffffffff-ffff-ffff-ffff-ffffffffffff"),
                    devices: vec![
                        (Devid::new(1), USAGE_DEVICE_SIZE),
                        (Devid::new(2), USAGE_DEVICE_SIZE),
                    ],
                },
            ),
            (
                "changed-device-size",
                PoolKey {
                    fsid: fsid(MONITOR_FSID),
                    devices: vec![(Devid::new(1), 50 * GIB), (Devid::new(2), 50 * GIB)],
                },
            ),
        ];

        for (label, stale_key) in cases {
            let (_dir, paths) = isolated_paths();
            seed_enospc_baseline(&paths, stale_key, open_snooze_deadline());
            let runner = MonitorTestRunner::with_usage_payload(usage_atrisk());

            let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

            assert!(
                has_enospc_cause(&result),
                "{label}: stale baseline must not suppress -- EnospcRisk must fire, got {result:?}"
            );
            assert!(
                !paths.enospc_ack_json().exists(),
                "{label}: a confirmed-mismatched baseline must be removed"
            );
        }
    }

    // Intent: when the live FS UUID is absent (no usable PoolKey), an at-risk
    //   pool fires armed but the present baseline is LEFT in place.
    // Why it exists: an identity gap is not a confirmed different pool -- a later
    //   cycle with the FS UUID present must still be able to compare and re-arm
    //   the baseline, so the monitor must not delete it here.
    // Scenario: a transient probe yields a btrfs show without a uuid line while
    //   the pool is at risk and a baseline sits on disk.
    #[test]
    fn cmd_monitor_identity_gap_fires_armed_and_keeps_baseline() {
        let (_dir, paths) = isolated_paths();
        seed_enospc_baseline(&paths, matching_pool_key(), open_snooze_deadline());
        let runner = MonitorTestRunner::with_usage_and_override(
            usage_atrisk(),
            MonitorOverride::BtrfsShowPayload(BTRFS_SHOW_2DISK_NO_UUID.to_owned()),
        );

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert!(
            has_enospc_cause(&result),
            "identity gap must fire armed, got {result:?}"
        );
        assert!(
            paths.enospc_ack_json().exists(),
            "an uncomparable baseline must be left in place, not removed"
        );
    }

    // Intent: a legacy pre-rename marker using `pool_key.fs_uuid` is treated as
    //   corrupt even when its value is otherwise a valid UUID, so the at-risk
    //   cycle fires armed and removes the marker.
    // Why it exists: pins the no-migration upgrade path. Accepting the old key
    //   through an alias/default would deserialize the marker and, with no live
    //   FSID, route into the identity-gap arm that leaves the file in place.
    // Scenario: a NAS upgrades with an old snooze marker on disk while the next
    //   mounted probe is at risk but lacks a uuid line.
    #[test]
    fn cmd_monitor_legacy_fs_uuid_marker_fires_armed_and_clears_without_computation_error() {
        let (_dir, paths) = isolated_paths();
        let legacy = serde_json::json!({
            "pool_key": {
                "fs_uuid": MONITOR_FSID,
                "devices": [[1, USAGE_DEVICE_SIZE], [2, USAGE_DEVICE_SIZE]],
            },
            "snoozed_until": open_snooze_deadline(),
        });
        std::fs::write(
            paths.enospc_ack_json(),
            serde_json::to_vec(&legacy).unwrap(),
        )
        .unwrap();
        let runner = MonitorTestRunner::with_usage_and_override(
            usage_atrisk(),
            MonitorOverride::BtrfsShowPayload(BTRFS_SHOW_2DISK_NO_UUID.to_owned()),
        );

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert!(
            has_enospc_cause(&result),
            "legacy-key baseline must fire armed, got {result:?}"
        );
        assert!(
            !paths.enospc_ack_json().exists(),
            "legacy-key baseline is corrupt after the fsid rename -> removed"
        );
        assert!(
            !has_computation_error(&result),
            "a legacy-key baseline must not fold a ComputationError"
        );
    }

    // Intent: a structurally-current marker with a malformed `pool_key.fsid`
    //   fails closed through the corrupt-marker path, so the at-risk cycle fires
    //   armed and removes the marker.
    // Why it exists: `Fsid` deserialization must keep validating the on-disk
    //   marker. A weakened deserializer would accept the garbage and, with no
    //   live FSID, route into the identity-gap arm that leaves the file in place.
    // Scenario: enospc-ack.json is hand-edited to a non-UUID fsid while the next
    //   mounted probe is at risk but lacks a uuid line.
    #[test]
    fn cmd_monitor_malformed_fsid_marker_fires_armed_and_clears_without_computation_error() {
        let (_dir, paths) = isolated_paths();
        let malformed = serde_json::json!({
            "pool_key": {
                "fsid": "not-a-uuid",
                "devices": [[1, USAGE_DEVICE_SIZE], [2, USAGE_DEVICE_SIZE]],
            },
            "snoozed_until": open_snooze_deadline(),
        });
        std::fs::write(
            paths.enospc_ack_json(),
            serde_json::to_vec(&malformed).unwrap(),
        )
        .unwrap();
        let runner = MonitorTestRunner::with_usage_and_override(
            usage_atrisk(),
            MonitorOverride::BtrfsShowPayload(BTRFS_SHOW_2DISK_NO_UUID.to_owned()),
        );

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert!(
            has_enospc_cause(&result),
            "malformed-fsid baseline must fire armed, got {result:?}"
        );
        assert!(
            !paths.enospc_ack_json().exists(),
            "malformed-fsid baseline is corrupt -> removed"
        );
        assert!(
            !has_computation_error(&result),
            "a malformed-fsid baseline must not fold a ComputationError"
        );
    }

    // Intent: a corrupt baseline file with a live at-risk pool fires armed, clears
    //   the corrupt file best-effort, and does NOT fold a ComputationError.
    // Why it exists (F3): the risk-known-but-baseline-lost branch is distinct from
    //   a usage-probe failure -- a corrupt baseline must not silently suppress a
    //   real risk, nor escalate to a beeping ComputationError.
    // Scenario: enospc-ack.json is hand-edited to garbage while the pool is at
    //   risk.
    #[test]
    fn cmd_monitor_corrupt_baseline_fires_armed_without_computation_error() {
        let (_dir, paths) = isolated_paths();
        std::fs::write(paths.enospc_ack_json(), b"not json").unwrap();
        let runner = MonitorTestRunner::with_usage_payload(usage_atrisk());

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert!(
            has_enospc_cause(&result),
            "corrupt baseline must fire armed, got {result:?}"
        );
        assert!(
            !has_computation_error(&result),
            "a corrupt baseline must not fold a ComputationError"
        );
        assert!(
            !paths.enospc_ack_json().exists(),
            "the corrupt baseline must be cleared best-effort"
        );
    }

    // Intent: a degraded pool raises no EnospcRisk and a seeded matching-key
    //   baseline survives the cycle untouched.
    // Why it exists: the monitor must skip ENOSPC *before* the state machine on a
    //   degraded pool. A sentinel-reliant impl that let the i64::MAX degraded
    //   margin reach the re-arm branch would call remove_enospc_ack and silently
    //   drop a still-at-risk pool's suppression memory across a transient device
    //   absence. The file-survival assertion is what fails under that bug -- the
    //   bare "no EnospcRisk" check stays green because the sentinel also
    //   suppresses the cause.
    // Scenario: a 2-present-1-missing degraded pool with a matching baseline on
    //   disk.
    #[test]
    fn cmd_monitor_degraded_skips_enospc_and_preserves_baseline() {
        let (_dir, paths) = isolated_paths();
        seed_enospc_baseline(&paths, matching_pool_key(), open_snooze_deadline());
        let runner = MonitorTestRunner::with_override(MonitorOverride::BtrfsShowPayload(
            BTRFS_SHOW_2DISK_1MISSING.to_owned(),
        ));

        let result = cmd_monitor(&runner, &monitor_fs_btrfs(), &monitor_mp(), &paths);

        assert!(
            !has_enospc_cause(&result),
            "degraded pool must not raise EnospcRisk, got {result:?}"
        );
        assert!(
            paths.enospc_ack_json().exists(),
            "degraded skip must leave the baseline untouched (it is loaded from disk after)"
        );
        // The still-loadable baseline proves the file survived intact.
        assert!(load_enospc_ack(&paths).unwrap().is_some());
    }
}
