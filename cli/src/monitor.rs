use std::collections::{BTreeMap, BTreeSet};

use crate::alert::{
    self, AlertCause, compute_alert_state_with_devid_map, load_acked_stats, merge_into_latch,
    save_acked_stats,
};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_btrfs_device_stats;
use crate::probe::{ProbeError, probe_pool};
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
    let causes = vec![AlertCause::ComputationError { detail }];
    let existing = alert::load_alert_latch(paths);
    let merged = merge_into_latch(existing.as_ref(), &causes);
    if let Err(e) = alert::save_alert_latch(&merged, paths) {
        eprintln!("Warning: failed to write alert latch: {e}");
    }
    MonitorResult::Alert(merged)
}

pub fn cmd_monitor<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> MonitorResult {
    // 1. Check if pool is mounted
    let pool = match probe_pool(runner, mount_point) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return MonitorResult::PoolOffline;
        }
        Err(e) => return latch_computation_error(e.to_string(), paths),
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

    // 3. Load acked stats
    let mut acked = load_acked_stats(paths);

    // 4. Compute alert-local missing devids: btrfs MISSING ∪ null-underlying
    let alert_missing_devids: Vec<u64> = pool
        .missing_devids
        .iter()
        .copied()
        .chain(pool.null_underlying.iter().map(|d| d.devid))
        .collect::<BTreeSet<u64>>()
        .into_iter()
        .collect();

    // 5. Check smartd alert flag
    let smartd_active = alert::smartd_alert_active(paths);

    // 6. Build devid map from pool devices + null-underlying devices
    let path_to_devid: BTreeMap<String, u64> = pool
        .devices
        .iter()
        .map(|d| (format!("/dev/mapper/{}", d.mapper.0), d.devid))
        .chain(
            pool.null_underlying
                .iter()
                .map(|d| (format!("/dev/mapper/{}", d.mapper.0), d.devid)),
        )
        .collect();

    // 7. Self-heal stale ack state: if a devid was missing_acked but is now
    //    present, reset missing_acked to false
    let mut ack_changed = false;
    let present_devids: Vec<u64> = pool.devices.iter().map(|d| d.devid).collect();
    for (key, disk) in acked.0.iter_mut() {
        if disk.missing_acked
            && let Ok(devid) = key.parse::<u64>()
            && present_devids.contains(&devid)
        {
            disk.missing_acked = false;
            ack_changed = true;
        }
    }
    if ack_changed && let Err(e) = save_acked_stats(&acked, paths) {
        eprintln!("Warning: failed to update acked stats: {e}");
    }

    // 8. Compute live alert state
    let live_causes = match compute_alert_state_with_devid_map(
        &device_stats,
        &acked,
        &alert_missing_devids,
        smartd_active,
        &path_to_devid,
    ) {
        Ok(state) => state.causes,
        Err(e) => return latch_computation_error(e.to_string(), paths),
    };

    // 9. Load existing latch
    let existing_latch = alert::load_alert_latch(paths);

    // 10. Merge: existing latch + live causes
    let merged = merge_into_latch(existing_latch.as_ref(), &live_causes);

    // 11. If merged state active → write latch
    if merged.active
        && let Err(e) = alert::save_alert_latch(&merged, paths)
    {
        eprintln!("Warning: failed to write alert latch: {e}");
    }

    // 12. Return result based on merged state
    if merged.active {
        MonitorResult::Alert(merged)
    } else {
        MonitorResult::Ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, RawCommandOutput};
    use std::sync::Mutex;

    const FINDMNT_BTRFS: &str = r#"{
       "filesystems": [
          {"target": "/mnt/storage", "source": "/dev/mapper/braid-vdb", "fstype": "btrfs"}
       ]
    }"#;

    const BTRFS_SHOW_2DISK: &str = "Label: none  uuid: de2b8517-f972-45fc-b121-3e160c8ea432\n\
        \tTotal devices 2 FS bytes used 16.17MiB\n\
        \tdevid    1 size 1008.00MiB used 209.50MiB path /dev/mapper/braid-vdb\n\
        \tdevid    2 size 1008.00MiB used 209.50MiB path /dev/mapper/braid-vdc\n";

    const CRYPTSETUP_STATUS_VDB: &str = "/dev/mapper/braid-vdb is active and is in use.\n\
          type:    LUKS2\n\
          cipher:  aes-xts-plain64\n\
          keysize: 512 [bits]\n\
          key location: keyring\n\
          device:  /dev/vdb\n\
          sector size:  512 [bytes]\n\
          offset:  32768 [512-byte units] (16777216 [bytes])\n\
          size:    2064384 [512-byte units] (1056964608 [bytes])\n\
          mode:    read/write\n";

    const CRYPTSETUP_STATUS_VDC: &str = "/dev/mapper/braid-vdc is active and is in use.\n\
          type:    LUKS2\n\
          cipher:  aes-xts-plain64\n\
          keysize: 512 [bits]\n\
          key location: keyring\n\
          device:  /dev/vdc\n\
          sector size:  512 [bytes]\n\
          offset:  32768 [512-byte units] (16777216 [bytes])\n\
          size:    2064384 [512-byte units] (1056964608 [bytes])\n\
          mode:    read/write\n";

    const LUKS_UUID: &str = "8c78a966-ef17-4610-b835-5b376ef10b4e\n";

    const STATS_2DISK_HEALTHY: &str = r#"{
        "__header": {"version": "1"},
        "device-stats": [
            {"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
            {"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
        ]
    }"#;

    const STATS_WITH_STALE_MAPPER: &str = r#"{
        "__header": {"version": "1"},
        "device-stats": [
            {"device": "/dev/mapper/braid-vdb", "devid": 1, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
            {"device": "/dev/mapper/braid-vdc", "devid": 2, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0},
            {"device": "/dev/mapper/braid-stale", "devid": 99, "write_io_errs": 0, "read_io_errs": 0, "flush_io_errs": 0, "corruption_errs": 0, "generation_errs": 0}
        ]
    }"#;

    fn ok_output(stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: "test".to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    /// Override response for one CmdRequest variant; everything else uses the
    /// healthy-2disk default. Each entry is matched by variant + key fields.
    enum Override {
        FindmntErr(CmdError),
        StatsResult(Result<RawCommandOutput, CmdError>),
    }

    struct MonitorTestRunner {
        // Stats payload returned for BtrfsDeviceStatsJson on the success path.
        stats_payload: String,
        override_op: Mutex<Option<Override>>,
    }

    impl MonitorTestRunner {
        fn with_unmapped_stats() -> Self {
            Self {
                stats_payload: STATS_WITH_STALE_MAPPER.to_owned(),
                override_op: Mutex::new(None),
            }
        }

        fn with_override(o: Override) -> Self {
            Self {
                stats_payload: STATS_2DISK_HEALTHY.to_owned(),
                override_op: Mutex::new(Some(o)),
            }
        }

        // Take the override only if it matches the given request shape.
        // Avoids consuming a StatsResult override on an earlier FindmntJson call
        // (or vice versa).
        fn take_findmnt_err(&self) -> Option<CmdError> {
            let mut guard = self.override_op.lock().unwrap();
            if matches!(*guard, Some(Override::FindmntErr(_))) {
                if let Some(Override::FindmntErr(e)) = guard.take() {
                    return Some(e);
                }
            }
            None
        }

        fn take_stats_result(&self) -> Option<Result<RawCommandOutput, CmdError>> {
            let mut guard = self.override_op.lock().unwrap();
            if matches!(*guard, Some(Override::StatsResult(_))) {
                if let Some(Override::StatsResult(r)) = guard.take() {
                    return Some(r);
                }
            }
            None
        }
    }

    impl CommandRunner for MonitorTestRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::FindmntJson { .. } => {
                    if let Some(e) = self.take_findmnt_err() {
                        return Err(e);
                    }
                    Ok(ok_output(FINDMNT_BTRFS))
                }
                CmdRequest::BtrfsFilesystemShow { .. } => Ok(ok_output(BTRFS_SHOW_2DISK)),
                CmdRequest::CryptsetupStatus { mapper } => match mapper.as_str() {
                    "braid-vdb" => Ok(ok_output(CRYPTSETUP_STATUS_VDB)),
                    "braid-vdc" => Ok(ok_output(CRYPTSETUP_STATUS_VDC)),
                    other => panic!("unexpected CryptsetupStatus mapper: {other}"),
                },
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(ok_output(LUKS_UUID)),
                CmdRequest::BtrfsDeviceStatsJson { .. } => {
                    if let Some(r) = self.take_stats_result() {
                        return r;
                    }
                    Ok(ok_output(&self.stats_payload))
                }
                other => panic!("unexpected CmdRequest in monitor test: {other:?}"),
            }
        }

        fn run_with_stdin(
            &self,
            _request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            Err(CmdError::MissingMock)
        }
    }

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".to_owned())
    }

    fn fresh_paths() -> (tempfile::TempDir, StatePaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().into());
        (dir, paths)
    }

    fn assert_single_computation_error(result: &MonitorResult) -> &str {
        match result {
            MonitorResult::Alert(state) => {
                assert!(state.active, "AlertState must be active");
                assert_eq!(
                    state.causes.len(),
                    1,
                    "expected exactly one cause, got {:?}",
                    state.causes
                );
                match &state.causes[0] {
                    AlertCause::ComputationError { detail } => detail.as_str(),
                    other => panic!("expected ComputationError, got {other:?}"),
                }
            }
            other => panic!("expected MonitorResult::Alert, got {other:?}"),
        }
    }

    /*
     * Intent: When compute_alert_state_with_devid_map returns
     * UnmappedDeviceError, cmd_monitor must latch a ComputationError and
     * return MonitorResult::Alert (NOT an Err). This is the gate for the
     * "exit 2 means the wrapper never starts the beeper" regression.
     *
     * Why it exists: monitor.rs previously latched the ComputationError but
     * then returned Err(MonitorError::UnmappedDevice), which main.rs mapped
     * to exit 2. The systemd wrapper only starts braid-alert.service on
     * exit 1, so the operator's NAS stayed silent on a real
     * lost-mapping condition. The bug is strictly worse than the
     * structurally-similar hot-unplug case fixed in plans/wip/structured-splashing-fern.md.
     *
     * Scenario: btrfs device stats reports a mapper path the devid map
     * does not contain (e.g. /dev/mapper/braid-stale lingering from a
     * previous configuration). The probe succeeds and the pool is
     * mounted; only the alert-state computation observes the orphan.
     */
    #[test]
    fn unmapped_device_returns_alert_with_latched_computation_error() {
        let (_dir, paths) = fresh_paths();
        let runner = MonitorTestRunner::with_unmapped_stats();

        let result = cmd_monitor(&runner, &mp(), &paths);
        let detail = assert_single_computation_error(&result);
        assert!(
            detail.contains("braid-stale"),
            "ComputationError detail must reference the orphan path: {detail}"
        );

        let latch_path = paths.alert_latch_json();
        assert!(
            latch_path.exists(),
            "alert latch must be written on UnmappedDeviceError"
        );
        let latched = alert::load_alert_latch(&paths).expect("latch present on disk");
        assert!(latched.active);
        assert!(matches!(
            latched.causes.as_slice(),
            [AlertCause::ComputationError { .. }]
        ));
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
     * Scenario: the very first probe command (FindmntJson) fails -- e.g.
     * findmnt was killed, the spawn failed, or mount-table I/O hit an
     * error. probe_pool returns ProbeError::Cmd before any pool data is
     * available.
     */
    #[test]
    fn probe_error_returns_alert_with_latched_computation_error() {
        let (_dir, paths) = fresh_paths();
        let runner = MonitorTestRunner::with_override(Override::FindmntErr(CmdError::Failed(
            "findmnt: spawn failed".into(),
        )));

        let result = cmd_monitor(&runner, &mp(), &paths);
        assert_single_computation_error(&result);

        let latch_path = paths.alert_latch_json();
        assert!(
            latch_path.exists(),
            "alert latch must be written on ProbeError"
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
            ("stats-parse-failure", Ok(ok_output("not valid json {{{"))),
        ];

        for (label, stats_result) in cases {
            let (_dir, paths) = fresh_paths();
            let runner = MonitorTestRunner::with_override(Override::StatsResult(stats_result));

            let result = cmd_monitor(&runner, &mp(), &paths);
            assert_single_computation_error(&result);

            assert!(
                paths.alert_latch_json().exists(),
                "{label}: alert latch must be written"
            );
        }
    }
}
