#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::alert::{self, AlertCause, compute_alert_state, merge_into_latch, save_acked_stats};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_btrfs_device_stats;
use crate::probe::{Filesystem, ProbeError, probe_pool};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;

#[derive(Debug, PartialEq, Eq)]
pub enum MonitorResult {
    PoolOffline,
    Ok,
    Alert(alert::AlertState),
}

// Fail closed: any indeterminate-state failure inside cmd_monitor latches a
// ComputationError cause and surfaces as MonitorResult::Alert so the systemd
// wrapper starts the beeper. See docs/decisions/014-alerts.md.
fn latch_computation_error(detail: String, paths: &StatePaths) -> MonitorResult {
    eprintln!("error: {detail}");
    let (existing, latch_corrupt_detail) = alert::load_alert_latch_or_quarantine(paths);
    // merge_into_latch's same_cause_key collapses every ComputationError into
    // one slot, so two distinct ComputationError entries would lose one. Fold
    // the latch-corruption detail into the same cause string instead.
    let combined_detail = match latch_corrupt_detail {
        Some(latch_detail) => format!(
            "{detail}; additionally, previous alert latch was unreadable -- quarantined; {latch_detail}"
        ),
        None => detail,
    };
    let causes = vec![AlertCause::ComputationError {
        detail: combined_detail,
    }];
    let merged = merge_into_latch(existing.as_ref(), &causes);
    if let Err(e) = alert::save_alert_latch(&merged, paths) {
        eprintln!("Warning: failed to write alert latch: {e}");
    }
    MonitorResult::Alert(merged)
}

pub fn cmd_monitor<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> MonitorResult {
    // 1. Check if pool is mounted.
    //
    // Exhaustive over every ProbeError variant on purpose: a future variant
    // must produce a "non-exhaustive patterns" compile error here so the
    // reviewer is forced to classify it as either offline or fail-closed
    // alert. monitor is the headless surface, so the wrong default would
    // propagate silently into operator-visible behavior.
    let pool = match probe_pool(runner, fs, mount_point) {
        Ok(p) => p,
        // Mount target holds a non-btrfs filesystem -- our pool is not here.
        // Treat as offline; no fail-closed beep needed.
        Err(ProbeError::NotBtrfs { .. }) => return MonitorResult::PoolOffline,
        // All remaining variants describe indeterminate pool state -- tooling
        // breakage (Cmd/Parse), pool show internally inconsistent (PoolDevice),
        // or LUKS-side mismatch (UnsupportedLuksVersion / MapperConflict, both
        // unreachable from probe_pool today but listed for the gate). Fail
        // closed per ADR 014: latch ComputationError so the wrapper beeps.
        Err(
            e @ (ProbeError::Cmd(_)
            | ProbeError::Parse(_)
            | ProbeError::PoolDevice { .. }
            | ProbeError::UnsupportedLuksVersion { .. }
            | ProbeError::MapperConflict { .. }
            | ProbeError::MountInfo(_)),
        ) => return latch_computation_error(e.to_string(), paths),
    };

    if !pool.mounted {
        return MonitorResult::PoolOffline;
    }

    // 2. Run btrfs device stats
    let stats_raw = match runner.run(&CmdRequest::BtrfsDeviceStatsJson {
        mount_point: mount_point.clone(),
    }) {
        Ok(r) => r,
        Err(e) => return latch_computation_error(e.to_string(), paths),
    };
    let device_stats = match parse_btrfs_device_stats(&stats_raw) {
        Ok(s) => s,
        Err(e) => return latch_computation_error(e.to_string(), paths),
    };

    // 3. Load acked stats. Fail closed if the file is unreadable or
    // unparseable: an empty fallback would silently re-fire every acked
    // cause as a BtrfsDeviceErrors / MissingDevice cause against a zero
    // baseline.
    let mut acked = match alert::load_acked_stats_fallible(paths) {
        Ok(a) => a,
        Err(e) => {
            return latch_computation_error(format!("acked-stats unreadable -- {e}"), paths);
        }
    };

    // 4. Compute alert-local missing devids: btrfs MISSING ∪ null-underlying
    let alert_missing_devids = pool.alert_missing_devids();

    // 5. Check smartd alert flag
    let smartd_active = alert::smartd_alert_active(paths);

    // 6. Reconcile stale ack state: prune orphan devids and self-heal
    //    missing_acked for devices that are present again.
    let present_devids: BTreeSet<u64> = pool.devices.iter().map(|d| d.devid).collect();
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

    // 9. Load existing latch (quarantine corrupt file if needed)
    let (existing_latch, latch_corrupt_detail) = alert::load_alert_latch_or_quarantine(paths);

    // 9b. If the prior latch was unreadable, surface that as a loud
    // ComputationError cause so status sees it instead of silently rebuilding.
    let mut live_causes = live_causes;
    if let Some(detail) = latch_corrupt_detail {
        live_causes.insert(
            0,
            AlertCause::ComputationError {
                detail: format!("previous alert latch was unreadable -- quarantined; {detail}"),
            },
        );
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
        monitor_fs_mountinfo_error, monitor_mp,
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
     * MonitorResult::Alert. With dev.devid as the canonical identity, an
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
     * Intent: When probe_pool returns any non-NotBtrfs error, cmd_monitor
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
     * show command fails to spawn. probe_pool returns ProbeError::Cmd before
     * any pool device data is available.
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
     * because someone mounted the wrong thing). probe_pool returns
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
     * Intent: When btrfs filesystem show exits non-zero with non-empty
     * stderr, parse_btrfs_filesystem_show maps that to
     * ParseError::CommandFailed,
     * probe_pool wraps it as ProbeError::Parse, and cmd_monitor must
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
     * not /dev/mapper/-prefixed, probe_pool returns ProbeError::PoolDevice;
     * cmd_monitor must latch a ComputationError and return
     * MonitorResult::Alert.
     *
     * Why it exists: PoolDevice is the third reachable variant from
     * probe_pool (alongside Cmd and Parse). Without this test, a future
     * edit that wrongly maps PoolDevice to PoolOffline would still compile
     * (the match remains exhaustive) and no other test would catch the
     * regression -- breaking the fail-closed contract for the
     * non-/dev/mapper path / no-FSID / inactive-mapper class of pool-show
     * inconsistencies (cli/src/probe.rs:259, :270, :287).
     *
     * Scenario: btrfs filesystem show reports a single-disk pool whose
     * device path is /dev/sda1 (raw block device, no LUKS mapper).
     * probe_pool's invariant -- every pool device must live under
     * /dev/mapper/ -- fails at probe.rs:270.
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
