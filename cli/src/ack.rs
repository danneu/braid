use crate::alert::{
    self, AlertCause, AlertState, load_acked_stats_fallible, mark_missing_acked, save_acked_stats,
    snapshot_current,
};
use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::parse_btrfs_device_stats;
use crate::probe::{Filesystem, ProbeError, probe_pool};
use crate::state_paths::StatePaths;
use crate::types::MountPoint;

pub fn cmd_ack<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    paths: &StatePaths,
) -> Result<(), AckError> {
    // 1. Read latch (authoritative alert state). An unreadable latch counts
    //    as an active alert for gating so the user can clear a corrupt file
    //    even with the pool offline. The parsed AlertState is carried into
    //    ack_offline so it can inspect causes.
    let (latch_state, latch_corrupt) = match alert::load_alert_latch(paths) {
        Ok(Some(s)) => (Some(s), false),
        Ok(None) => (None, false),
        Err(e) => {
            eprintln!("warning: alert latch unreadable -- acknowledging anyway: {e}");
            (None, true)
        }
    };
    let latch_count = latch_state.as_ref().map(|s| s.causes.len()).unwrap_or(0);

    // 2. Check if pool is mounted
    let pool = match probe_pool(runner, fs, mount_point) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return ack_offline(latch_state, latch_corrupt, paths);
        }
        Err(e) => return Err(AckError::Probe(e)),
    };

    if !pool.mounted {
        return ack_offline(latch_state, latch_corrupt, paths);
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

    // 6. Remove smartd alert flag + alert latch (+ any corrupt sidecar)
    alert::remove_smartd_alert_flag(paths)?;
    alert::remove_alert_latch(paths)?;
    alert::remove_alert_latch_corrupt(paths)?;

    // 7. Stop beeper (best-effort)
    stop_beeper();

    // 8. Print confirmation using latch count
    if latch_count > 0 {
        println!("acknowledged {latch_count} alert(s)");
    } else {
        println!("no active alerts");
    }

    Ok(())
}

fn ack_offline(
    latch_state: Option<AlertState>,
    latch_corrupt: bool,
    paths: &StatePaths,
) -> Result<(), AckError> {
    let smartd_active = alert::smartd_alert_active(paths);
    let latch_count = latch_state.as_ref().map(|s| s.causes.len()).unwrap_or(0);

    let has_alert = latch_count > 0 || smartd_active || latch_corrupt;
    if !has_alert {
        return Err(AckError::PoolNotMounted);
    }

    // Refuse if the latch contains any BtrfsDeviceErrors cause: the counter
    // baseline that suppresses re-firing requires live `btrfs device stats`
    // output, which we cannot produce with the pool offline. Refusing the
    // *whole* ack (rather than partial-acking other causes) avoids leaving
    // the user in an ambiguous "I acked but it still says ALERT" state.
    if let Some(state) = latch_state.as_ref()
        && state
            .causes
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
    let missing_devids: Vec<u64> = latch_state
        .as_ref()
        .map(|s| {
            s.causes
                .iter()
                .filter_map(|c| match c {
                    AlertCause::MissingDevice { devid } => Some(*devid),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    if !missing_devids.is_empty() {
        let mut acked = load_acked_stats_fallible(paths)?;
        for devid in &missing_devids {
            mark_missing_acked(&mut acked, *devid);
        }
        save_acked_stats(&acked, paths)?;
    }

    alert::remove_alert_latch(paths)?;
    alert::remove_alert_latch_corrupt(paths)?;
    alert::remove_smartd_alert_flag(paths)?;
    stop_beeper();
    println!("acknowledged current alerts");
    Ok(())
}

#[cfg(not(test))]
fn stop_beeper() {
    let result = std::process::Command::new("systemctl")
        .args(["stop", "braid-alert.service"])
        .output();
    if let Err(e) = result {
        eprintln!("Warning: could not stop braid-alert.service: {e}");
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::alert::{
        AckedDeviceCounters, AckedDisk, AckedStats, load_acked_stats, save_alert_latch,
    };
    use crate::cmd::{CmdError, RawCommandOutput};
    use std::collections::BTreeMap;

    /// Mountinfo where /mnt/storage is held by ext4 -> probe_pool returns
    /// ProbeError::NotBtrfs, which jumps to ack_offline. The runner is
    /// never called on this path.
    const MOUNTINFO_EXT4: &str =
        "36 35 0:32 / /mnt/storage rw,noatime shared:1 - ext4 /dev/sda1 rw\n";

    struct PanicRunner;

    impl CommandRunner for PanicRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            panic!("offline ack must not invoke the runner; got: {request:?}");
        }
        fn run_with_stdin(
            &self,
            request: &CmdRequest,
            _stdin: &[u8],
        ) -> Result<RawCommandOutput, CmdError> {
            panic!("offline ack must not invoke run_with_stdin; got: {request:?}");
        }
    }

    struct Ext4Fs;

    impl Filesystem for Ext4Fs {
        fn exists(&self, _path: &str) -> bool {
            false
        }
        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            assert_eq!(path, "/proc/self/mountinfo");
            Ok(MOUNTINFO_EXT4.to_owned())
        }
        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    fn mp() -> MountPoint {
        MountPoint("/mnt/storage".to_owned())
    }

    fn fresh_paths() -> (tempfile::TempDir, StatePaths) {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().to_path_buf());
        (dir, paths)
    }

    fn write_latch(paths: &StatePaths, causes: Vec<AlertCause>) {
        let state = AlertState {
            active: !causes.is_empty(),
            causes,
        };
        save_alert_latch(&state, paths).unwrap();
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
        let (_dir, paths) = fresh_paths();
        write_latch(&paths, vec![AlertCause::MissingDevice { devid: 2 }]);

        cmd_ack(&PanicRunner, &Ext4Fs, &mp(), &paths).unwrap();

        let acked = load_acked_stats(&paths);
        let entry = acked.0.get("2").expect("devid 2 entry must be present");
        assert!(entry.missing_acked);
        assert!(!paths.alert_latch_json().exists());
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
        let (_dir, paths) = fresh_paths();
        write_latch(
            &paths,
            vec![
                AlertCause::BtrfsDeviceErrors { devid: 1 },
                AlertCause::MissingDevice { devid: 2 },
            ],
        );
        let original_latch_bytes = std::fs::read(paths.alert_latch_json()).unwrap();
        assert!(!paths.acked_stats_json().exists());

        let result = cmd_ack(&PanicRunner, &Ext4Fs, &mp(), &paths);
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
     * Why it exists: Pins the helper's insert-or-update contract. A
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
        let (_dir, paths) = fresh_paths();
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

        write_latch(&paths, vec![AlertCause::MissingDevice { devid: 1 }]);

        cmd_ack(&PanicRunner, &Ext4Fs, &mp(), &paths).unwrap();

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
        let (_dir, paths) = fresh_paths();
        std::fs::write(paths.alert_latch_json(), b"not json").unwrap();

        cmd_ack(&PanicRunner, &Ext4Fs, &mp(), &paths).unwrap();

        assert!(!paths.alert_latch_json().exists());
        assert!(!paths.alert_latch_corrupt().exists());
        assert!(
            !paths.acked_stats_json().exists(),
            "no MissingDevice cause means no acked-stats write"
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
        let (_dir, paths) = fresh_paths();
        std::fs::write(paths.acked_stats_json(), b"not json").unwrap();
        let original_bytes = std::fs::read(paths.acked_stats_json()).unwrap();

        write_latch(&paths, vec![AlertCause::MissingDevice { devid: 1 }]);

        let result = cmd_ack(&PanicRunner, &Ext4Fs, &mp(), &paths);
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
        let (_dir, paths) = fresh_paths();
        std::fs::write(paths.acked_stats_json(), b"not json").unwrap();
        let original_bytes = std::fs::read(paths.acked_stats_json()).unwrap();

        write_latch(&paths, vec![AlertCause::SmartdAlert]);
        std::fs::write(paths.smartd_alert(), b"").unwrap();

        cmd_ack(&PanicRunner, &Ext4Fs, &mp(), &paths).unwrap();

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
        let (_dir, paths) = fresh_paths();
        std::fs::write(paths.acked_stats_json(), b"not json").unwrap();
        let original_bytes = std::fs::read(paths.acked_stats_json()).unwrap();

        write_latch(
            &paths,
            vec![AlertCause::ComputationError {
                detail: "test".to_owned(),
            }],
        );

        cmd_ack(&PanicRunner, &Ext4Fs, &mp(), &paths).unwrap();

        assert!(!paths.alert_latch_json().exists());
        let after_bytes = std::fs::read(paths.acked_stats_json()).unwrap();
        assert_eq!(after_bytes, original_bytes);
    }
}
