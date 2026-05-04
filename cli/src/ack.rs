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

    let smartd_active = alert::smartd_alert_active(paths);
    if latch_count == 0 && !smartd_active && !latch_corrupt {
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
    use crate::cmd::{CmdError, MockRunner, RawCommandOutput};
    use std::collections::BTreeMap;

    /// Mountinfo where /mnt/storage is held by ext4 -> probe_pool returns
    /// ProbeError::NotBtrfs, which jumps to ack_offline. The runner is
    /// never called on this path.
    const MOUNTINFO_EXT4: &str =
        "36 35 0:32 / /mnt/storage rw,noatime shared:1 - ext4 /dev/sda1 rw\n";
    const MOUNTINFO_BTRFS: &str =
        "36 35 0:32 / /mnt/storage rw,noatime shared:1 - btrfs /dev/mapper/braid-disk1 rw\n";

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

    struct BtrfsFs;

    impl Filesystem for BtrfsFs {
        fn exists(&self, _path: &str) -> bool {
            false
        }
        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            assert_eq!(path, "/proc/self/mountinfo");
            Ok(MOUNTINFO_BTRFS.to_owned())
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

    fn ok_raw(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    fn btrfs_show_2disk() -> RawCommandOutput {
        ok_raw(
            "btrfs filesystem show /mnt/storage",
            "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
             \tTotal devices 2 FS bytes used 1.00GiB\n\
             \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk1\n\
             \tdevid    3 size 10.00GiB used 2.00GiB path /dev/mapper/braid-disk3\n",
        )
    }

    fn cryptsetup_status_active(mapper: &str, device: &str) -> RawCommandOutput {
        ok_raw(
            &format!("cryptsetup status {mapper}"),
            &format!(
                "/dev/mapper/{mapper} is active and is in use.\n\
                 \ttype:    LUKS2\n\
                 \tcipher:  aes-xts-plain64\n\
                 \tdevice:  {device}\n\
                 \tsector size:  512\n"
            ),
        )
    }

    fn cryptsetup_uuid_ok(device: &str, uuid: &str) -> RawCommandOutput {
        ok_raw(
            &format!("cryptsetup luksUUID {device}"),
            &format!("{uuid}\n"),
        )
    }

    fn btrfs_device_stats_healthy() -> RawCommandOutput {
        ok_raw(
            "btrfs --format json device stats /mnt/storage",
            r#"{
                "device-stats": [
                    {
                        "device": "/dev/mapper/braid-disk1",
                        "devid": 1,
                        "write_io_errs": 0,
                        "read_io_errs": 0,
                        "flush_io_errs": 0,
                        "corruption_errs": 0,
                        "generation_errs": 0
                    },
                    {
                        "device": "/dev/mapper/braid-disk3",
                        "devid": 3,
                        "write_io_errs": 0,
                        "read_io_errs": 0,
                        "flush_io_errs": 0,
                        "corruption_errs": 0,
                        "generation_errs": 0
                    }
                ]
            }"#,
        )
    }

    fn mounted_probe_runner() -> MockRunner {
        MockRunner::default()
            .with_output(
                CmdRequest::BtrfsFilesystemShow { mount_point: mp() },
                btrfs_show_2disk(),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk1".into(),
                },
                cryptsetup_status_active("braid-disk1", "/dev/vda"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                cryptsetup_uuid_ok("/dev/vda", "11111111-1111-1111-1111-111111111111"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-disk3".into(),
                },
                cryptsetup_status_active("braid-disk3", "/dev/vdc"),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vdc".into(),
                },
                cryptsetup_uuid_ok("/dev/vdc", "33333333-3333-3333-3333-333333333333"),
            )
    }

    fn mounted_probe_runner_with_device_stats() -> MockRunner {
        mounted_probe_runner().with_output(
            CmdRequest::BtrfsDeviceStatsJson { mount_point: mp() },
            btrfs_device_stats_healthy(),
        )
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
        let (_dir, paths) = fresh_paths();
        let runner = mounted_probe_runner();

        let result = cmd_ack(&runner, &BtrfsFs, &mp(), &paths);

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
        let (_dir, paths) = fresh_paths();
        std::fs::write(paths.alert_latch_json(), b"not json").unwrap();
        let runner = mounted_probe_runner_with_device_stats();

        let result = cmd_ack(&runner, &BtrfsFs, &mp(), &paths);

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
        let (_dir, paths) = fresh_paths();
        std::fs::write(paths.smartd_alert(), b"").unwrap();
        let runner = mounted_probe_runner_with_device_stats();

        let result = cmd_ack(&runner, &BtrfsFs, &mp(), &paths);

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
