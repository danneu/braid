#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::alert::{self, AlertCause, compute_alert_state, merge_into_latch, save_acked_stats};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_btrfs_device_stats;
use crate::probe::{Filesystem, ProbeError, probe_pool_alerts};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;

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

pub fn cmd_monitor<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
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
            // tooling breakage (Cmd/Parse), pool show internally inconsistent
            // (PoolDevice), or LUKS-side mismatch (UnsupportedLuksVersion /
            // MapperConflict, both unreachable from probe_pool_alerts today
            // but listed for the gate). Fail closed per ADR 014: latch
            // ComputationError so the wrapper beeps.
            Err(
                e @ (ProbeError::Cmd(_)
                | ProbeError::Parse(_)
                | ProbeError::PoolDevice { .. }
                | ProbeError::UnsupportedLuksVersion { .. }
                | ProbeError::MapperConflict { .. }
                | ProbeError::MapperBackingMismatch { .. }
                | ProbeError::MapperBackingResolveError { .. }
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

        // 4. Compute alert-local missing devids: btrfs MISSING ∪ null-underlying
        let alert_missing_devids = pool.alert_missing_devids();

        // 5. Check smartd alert flag
        let smartd_active = alert::smartd_alert_active(paths);

        // 6. Reconcile stale ack state: prune orphan devids and self-heal
        //    missing_acked for devices that are present again.
        let present_devids: BTreeSet<u64> = pool.present_devids.iter().copied().collect();
        let still_relevant_devids: BTreeSet<u64> = present_devids
            .iter()
            .copied()
            .chain(pool.null_underlying.iter().map(|d| d.devid))
            .chain(pool.missing_devids.iter().copied())
            .collect();
        let ack_changed =
            alert::reconcile_acked_stats(&mut acked, &still_relevant_devids, &present_devids);
        if ack_changed && let Err(e) = save_acked_stats(&acked, paths) {
            eprintln!("Warning: failed to update acked stats: {e}");
        }

        // 7. Compute live alert state. Identity is the devid carried on each
        //    stats row by btrfs -- no path-to-devid map needed.
        let live_causes =
            compute_alert_state(&device_stats, &acked, &alert_missing_devids, smartd_active).causes;

        Ok(Some(live_causes))
    })();

    let (mut live_causes, failure_detail) = match classified {
        Ok(None) => return MonitorResult::PoolOffline,
        Ok(Some(causes)) => (causes, None),
        Err(detail) => {
            eprintln!("error: {detail}");
            (Vec::new(), Some(detail))
        }
    };

    // 8. Load existing latch (quarantine corrupt file if needed)
    let (existing_latch, latch_corrupt_detail) = alert::load_alert_latch_or_quarantine(paths);

    // 9. Surface computation failures and latch corruption as at most one
    // ComputationError cause so status sees it instead of silently rebuilding.
    if let Some(detail) = folded_computation_error_detail(failure_detail, latch_corrupt_detail) {
        live_causes.insert(0, AlertCause::ComputationError { detail });
    }

    // 10. Merge: existing latch + live causes
    let merged = merge_into_latch(existing_latch.as_ref(), &live_causes);

    // 11. If merged state active -> write latch
    if merged.active()
        && let Err(e) = alert::save_alert_latch(&merged, paths)
    {
        eprintln!("Warning: failed to write alert latch: {e}");
    }

    // 12. Return result based on merged state
    if merged.active() {
        MonitorResult::Alert(merged)
    } else {
        MonitorResult::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, RawCommandOutput};
    use crate::test_fixtures::{
        MonitorOverride, MonitorReconcileRunner, MonitorTestRunner,
        assert_monitor_single_computation_error, isolated_paths, monitor_fs_btrfs, monitor_fs_ext4,
        monitor_fs_mountinfo_error, monitor_fs_not_mounted, monitor_mp,
    };

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
            .filter_map(|cause| match cause {
                AlertCause::ComputationError { detail } => Some(detail.as_str()),
                _ => None,
            })
            .collect()
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

    // Intent: cmd_monitor returns MonitorResult::Alert with exactly one
    //   ComputationError cause whose detail names "acked-stats" when
    //   acked-stats.json is unreadable / unparseable, and the corrupt
    //   bytes on disk are preserved byte-identical.
    // Why it exists: pins use of load_acked_stats_fallible (not the
    //   lossy load_acked_stats) on monitor's mutation path. Without it,
    //   cmd_monitor would treat a corrupt acked-stats.json as an empty
    //   baseline and silently return MonitorResult::Ok against an
    //   otherwise-healthy pool -- a fail-open hole in the indeterminate-
    //   state contract pinned by ADR 014:74. The byte-identity assertion
    //   also pins that monitor must not silently rewrite corrupt files
    //   (mirrors ack.rs:1018-1022).
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
     * UnsupportedLuksVersion, MapperConflict) used to flow through
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
     * Ok(RawCommandOutput) (cli/src/cmd.rs:841-858); only spawn/signal
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
            causes: vec![AlertCause::MissingDevice { devid: 7 }],
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
                .filter(|cause| matches!(cause, AlertCause::MissingDevice { devid: 7 }))
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
}
