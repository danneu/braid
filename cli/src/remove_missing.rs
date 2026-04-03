use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::config_read;
use crate::confirm;
use crate::journal;
use crate::membership;
use crate::parse::parse_btrfs_device_usage;
use crate::pool::pool_remove_devid;
use crate::preflight;
use crate::probe::{probe_pool, Filesystem, ProbeError};
use crate::progress::ProgressOutput;
use crate::state_paths::StatePaths;
use crate::types::MountPoint;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum RemoveMissingError {
    #[error("{0}")]
    Validation(String),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("pool error: {0}")]
    Pool(#[from] crate::pool::PoolError),
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
}

/// Resolve the missing-device removal target to a (devid, membership-name) pair.
/// Returns Err if the missing device's identity can't be mapped to a pool.json entry.
fn resolve_removal_target(
    target_devid: Option<u64>,
    membership: &membership::PoolMembership,
) -> Result<(u64, String), RemoveMissingError> {
    let devid = target_devid.ok_or_else(|| {
        RemoveMissingError::Validation(
            "cannot determine which device to remove: btrfs did not report \
             the missing device's ID. Pass --missing-id <devid> explicitly."
                .into(),
        )
    })?;

    let name = membership
        .disks
        .iter()
        .find(|(_, member)| member.devid == Some(devid))
        .map(|(name, _)| name.clone())
        .ok_or_else(|| {
            RemoveMissingError::Validation(format!(
                "devid {devid} not found in pool.json membership — \
                 no disk entry has this device ID. \
                 Pool membership may need manual repair."
            ))
        })?;

    Ok((devid, name))
}

pub struct RemoveMissingParams<'a> {
    pub config_path: &'a Path,
    pub missing_id: u64,
    pub dry_run: bool,
    pub yes: bool,
    pub progress: ProgressOutput,
    pub paths: &'a StatePaths,
}

pub fn cmd_remove_missing<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RemoveMissingParams<'_>,
) -> Result<(), RemoveMissingError> {
    preflight::check_no_pending_operation(params.paths).map_err(RemoveMissingError::Validation)?;

    let config = config_read(params.config_path)?;

    let pool = match probe_pool(runner, config.mount_point().as_str()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return Err(RemoveMissingError::Validation(
                "pool is not mounted. Nothing to remove.".into(),
            ));
        }
        Err(e) => return Err(RemoveMissingError::Probe(e)),
    };

    if !pool.mounted {
        return Err(RemoveMissingError::Validation(
            "pool is not mounted. Nothing to remove.".into(),
        ));
    }

    // Preflight
    let fsid = pool.fsid.as_deref().expect("mounted pool must have FSID");
    preflight::require_mutation_preflight(runner, fs, fsid, config.mount_point().as_str())
        .map_err(RemoveMissingError::Validation)?;

    if pool.missing_count == 0 {
        return Err(RemoveMissingError::Validation(
            "no missing devices detected in pool.".into(),
        ));
    }

    if pool.devices.iter().any(|d| d.devid == params.missing_id) {
        return Err(RemoveMissingError::Validation(format!(
            "devid {} is a live device, not a missing one. \
             Use 'braid remove' to remove live devices.",
            params.missing_id
        )));
    }
    let missing_devids = preflight::probe_missing_devids(runner, config.mount_point().as_str())
        .map_err(RemoveMissingError::Validation)?;
    if !missing_devids.contains(&params.missing_id) {
        return Err(RemoveMissingError::Validation(format!(
            "devid {} is not a device in this pool. \
             Use 'braid status' to see device IDs.",
            params.missing_id
        )));
    }

    // Pre-flight: reject if survivors lack space to absorb the missing
    // device's data. Without this check, btrfs will either ENOSPC or
    // crash the filesystem to read-only mid-relocation (see tests/repro/).
    //
    // Skip when only 1 present device survives: in 2-device RAID1, the
    // survivor already has all data (every chunk is mirrored). This does
    // not match the reproduced relocation-failure mode.
    if pool.devices.len() >= 2 {
        check_relocation_space(
            runner,
            config.mount_point().as_str(),
            Some(params.missing_id),
        )?;
    }

    let will_clear_last_missing = pool.missing_count == 1;
    let remaining_present = pool.devices.len();
    let steps = compile_steps(
        params.missing_id,
        will_clear_last_missing,
        remaining_present,
        config.mount_point(),
    );

    if params.dry_run {
        Step::print_dry_run(&steps);
        return Ok(());
    }

    // Resolve devid→name from enriched pool.json before confirmation and journal.
    let pre_membership = membership::load_membership(params.paths).map_err(|e| {
        RemoveMissingError::Validation(format!("failed to load pool membership: {e}"))
    })?;
    let (resolved_devid, name_to_remove) =
        resolve_removal_target(Some(params.missing_id), &pre_membership)?;

    // Confirm
    if !params.yes {
        eprintln!(
            "{}",
            format_remove_missing_confirm(
                &name_to_remove,
                resolved_devid,
                remaining_present,
                pool.missing_count,
            )
        );
        confirm::confirm_yes().map_err(RemoveMissingError::Validation)?;
    }

    // Build journal before btrfs operation.
    let mut target_membership = pre_membership.clone();
    target_membership.disks.remove(&name_to_remove);
    let journal = journal::build_journal(
        pre_membership,
        target_membership.clone(),
        journal::OpKind::RemoveMissing {
            devid: resolved_devid,
        },
    );
    journal::write_journal(params.paths, &journal)
        .map_err(|e| RemoveMissingError::Validation(e.to_string()))?;

    // Execute
    eprintln!(
        "Removing missing device (devid {}) from pool...",
        resolved_devid
    );
    pool_remove_devid(runner, config.mount_point().as_str(), resolved_devid)?;

    crate::pool::maybe_restore_raid1(
        runner,
        config.mount_point().as_str(),
        pool.missing_count,
        params.progress,
    )
    .map_err(RemoveMissingError::Pool)?;

    // Post-commit: write pool.json and clear journal only after the full operation succeeds.
    membership::save_membership(&target_membership, params.paths).map_err(|e| {
        RemoveMissingError::Validation(format!("failed to persist pool membership: {e}"))
    })?;
    journal::clear_journal(params.paths)
        .map_err(|e| RemoveMissingError::Validation(e.to_string()))?;

    eprintln!("Done. Missing device removed from pool.");
    Ok(())
}

/// Check that surviving devices have enough RAID1-aware, per-type space to absorb
/// the missing device's allocations. If they don't, btrfs device remove will
/// either ENOSPC instantly or — worse — crash the filesystem to read-only
/// mid-relocation.
///
/// Missing devices are identified by `device_size == 0` in `btrfs device usage
/// --raw` output. This is reliable: present devices always have device_size > 0,
/// and missing devices always report 0. Their allocation lines (Data, Metadata,
/// System) are preserved and accurate.
///
/// If the check itself fails (parse error, command error), we log a warning and
/// proceed — a bug in the safety net shouldn't block a valid operation.
fn check_relocation_space<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
    missing_id: Option<u64>,
) -> Result<(), RemoveMissingError> {
    let raw = match runner.run(&CmdRequest::BtrfsDeviceUsageRaw {
        mount_point: MountPoint(mount_point.to_owned()),
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("warning: ENOSPC pre-flight check failed: {e}; proceeding anyway");
            return Ok(());
        }
    };

    let usage = match parse_btrfs_device_usage(&raw) {
        Ok(u) => u,
        Err(e) => {
            eprintln!("warning: ENOSPC pre-flight check failed: {e}; proceeding anyway");
            return Ok(());
        }
    };

    // Partition: missing (device_size == 0, optionally filtered by devid) vs surviving (device_size > 0)
    let target: Vec<_> = usage
        .devices
        .iter()
        .filter(|d| d.device_size == 0 && (missing_id.is_none() || missing_id == Some(d.devid)))
        .collect();
    let remaining: Vec<_> = usage.devices.iter().filter(|d| d.device_size > 0).collect();

    preflight::check_raid1_relocation_space(&target, &remaining).map_err(|e| {
        RemoveMissingError::Validation(format!(
            "{e}\n\nFree up space by deleting files, or add a new device first with `braid add`."
        ))
    })
}

fn compile_steps(
    missing_id: u64,
    will_clear_last_missing: bool,
    remaining_present: usize,
    mount_point: &MountPoint,
) -> Vec<Step> {
    let mut steps = Vec::new();
    steps.push(Step {
        risk: "long",
        description: format!(
            "btrfs device remove {} (target specific missing device)",
            missing_id
        ),
        commands: vec![CmdRequest::BtrfsDeviceRemove {
            device: missing_id.to_string(),
            mount_point: mount_point.clone(),
        }],
    });
    if will_clear_last_missing && remaining_present >= 2 {
        steps.push(Step {
            risk: "long",
            description:
                "btrfs balance -dconvert=raid1,soft -mconvert=raid1,soft (restore redundancy)"
                    .into(),
            commands: vec![CmdRequest::BtrfsBalanceRaid1Soft {
                mount_point: mount_point.clone(),
            }],
        });
    }
    steps
}

// ---------------------------------------------------------------------------
// Confirmation formatter
// ---------------------------------------------------------------------------

fn format_remove_missing_confirm(
    name: &str,
    devid: u64,
    remaining_present: usize,
    missing_count: u64,
) -> String {
    let mut msg = "Remove missing device from pool:\n".to_string();
    msg.push_str(&format!(
        "  {} (devid {})  missing \u{2014} no hardware info available\n",
        name, devid
    ));
    if remaining_present >= 2 {
        msg.push_str("  Data on remaining disks will be rebalanced.\n");
    } else {
        msg.push_str("  Surviving disk already has all data.\n");
    }
    msg.push_str(&format!(
        "\nPool: {} present + {} missing \u{2192} {} {}\n",
        remaining_present,
        missing_count,
        remaining_present,
        if remaining_present == 1 {
            "disk"
        } else {
            "disks"
        },
    ));
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
    use crate::membership::{DiskMember, PoolMembership};
    use crate::probe::Filesystem;
    use crate::state_paths::StatePaths;
    use crate::types::ByIdPath;

    struct MockFs;

    impl Filesystem for MockFs {
        fn exists(&self, _path: &str) -> bool {
            false
        }
        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path.ends_with("/exclusive_operation") {
                Ok("none\n".to_owned())
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
            }
        }
        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

    /// Create a StatePaths backed by a temp dir, with pool.json pre-populated.
    /// Each entry is (name, by_id_path, optional_devid).
    fn test_paths(disks: &[(&str, &str, Option<u64>)]) -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let mut m = PoolMembership::empty();
        for (name, by_id, devid) in disks {
            let mut member = DiskMember::from_by_id(ByIdPath(by_id.to_string()));
            member.devid = *devid;
            m.disks.insert(name.to_string(), member);
        }
        membership::save_membership(&m, &paths).unwrap();
        (tmp, paths)
    }

    struct EnospcRunner {
        device_usage_stdout: &'static str,
    }

    impl CommandRunner for EnospcRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            match request {
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(RawCommandOutput {
                    cmd: "btrfs device usage --raw /mnt/storage".to_owned(),
                    stdout: self.device_usage_stdout.to_owned(),
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

    use std::sync::{Arc, Mutex};

    /// End-to-end runner that records all calls, modeling a pool with
    /// 1 present device + 1 missing device.
    #[derive(Clone)]
    struct RecordingRunner {
        log: Arc<Mutex<Vec<CmdRequest>>>,
    }

    impl RecordingRunner {
        fn new(log: Arc<Mutex<Vec<CmdRequest>>>) -> Self {
            Self { log }
        }
    }

    fn mock_out(cmd: &str, stdout: &str, exit_status: i32) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status,
        }
    }

    impl CommandRunner for RecordingRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());

            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                    0,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => Ok(mock_out(
                    &format!("btrfs filesystem show {mount_point}"),
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 0 used 0 path MISSING\n",
                    0,
                )),
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_out(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                    ),
                    0,
                )),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_out(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_out(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                    0,
                )),
                CmdRequest::BtrfsDeviceRemove { .. } => {
                    Ok(mock_out("btrfs device remove", "", 0))
                }
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(mock_out(
                    "btrfs device usage --raw",
                    "/dev/mapper/braid-disk1, ID: 1\n   Device size:           520093696\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:           452984832\n\n<missing disk>, ID: 2\n   Device size:                  0\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:                  0\n\n",
                    0,
                )),
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

    #[test]
    // Intent: cmd_remove_missing succeeds when the pool has 1 present device and
    //   1 missing device, without invoking the ENOSPC pre-flight check.
    //
    // Why: In a 2-device RAID1 pool with 1 missing device, the survivor already
    //   has all data (every chunk is mirrored). This does not match the reproduced
    //   relocation-failure mode. The pre-flight check would false-positive.
    //
    // Scenario: User's 2-disk NAS has one drive die. They run braid remove-missing.
    //   The operation succeeds because no data relocation is needed.
    fn enospc_check_skipped_for_single_survivor() {
        let (_state_tmp, state_paths) = test_paths(&[
            ("disk1", "/dev/disk/by-id/virtio-disk1", Some(1)),
            ("disk2", "/dev/disk/by-id/virtio-disk2", Some(2)),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        let config_json = serde_json::json!({ "mount_point": "/mnt/storage" });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = RecordingRunner::new(log.clone());
        cmd_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 2,
                dry_run: false,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
            },
        )
        .expect("remove-missing should succeed");

        // BtrfsDeviceUsageRaw is called once (by probe_missing_devids), but
        // the ENOSPC check_relocation_space should be skipped for single-survivor.
        let usage_calls = log
            .lock()
            .unwrap()
            .iter()
            .filter(|c| matches!(c, CmdRequest::BtrfsDeviceUsageRaw { .. }))
            .count();
        assert_eq!(
            usage_calls, 1,
            "Expected exactly 1 BtrfsDeviceUsageRaw call (probe_missing_devids only); \
             ENOSPC pre-flight should be skipped for single-survivor removal"
        );
    }

    #[test]
    // Intent: check_relocation_space rejects when survivors lack space for the
    //   missing device's allocations.
    //
    // Why it exists: Without this pre-flight check, btrfs will either ENOSPC
    //   instantly or crash the filesystem to read-only mid-relocation.
    //
    // Scenario: 3-drive RAID1 pool, one drive dies. The dead drive has 2 GiB
    //   allocated but survivors only have 100 MiB unallocated total.
    fn check_relocation_space_rejects_insufficient_space() {
        // Missing device (devid 3): device_size=0, ~2 GiB allocated
        // Survivors (devid 1,2): 50 MiB unallocated each = 100 MiB total
        let fixture = "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Metadata,RAID1:              0
   Unallocated:            50331648

/dev/mapper/braid-disk2, ID: 2
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Metadata,RAID1:              0
   Unallocated:            50331648

<missing disk>, ID: 3
   Device size:                  0
   Device slack:                  0
   Data,RAID1:           2147483648
   Metadata,RAID1:        268435456
   System,RAID1:           33554432
   Unallocated:          1828716544

";

        let runner = EnospcRunner {
            device_usage_stdout: fixture,
        };

        let result = check_relocation_space(&runner, "/mnt/storage", None);
        let err = result.expect_err("should reject insufficient space");
        let msg = err.to_string();
        assert!(
            msg.contains("not enough space to relocate"),
            "expected 'not enough space to relocate' in: {msg}"
        );
    }

    #[test]
    // Intent: check_relocation_space passes when survivors have enough space.
    //
    // Why it exists: Ensures the check doesn't false-positive and block valid
    //   remove-missing operations.
    //
    // Scenario: Missing device has small allocations, survivors have plenty of
    //   unallocated space.
    fn check_relocation_space_passes_sufficient_space() {
        let fixture = "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:           452984832

/dev/mapper/braid-disk2, ID: 2
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:           452984832

<missing disk>, ID: 3
   Device size:                  0
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:                  0

";

        let runner = EnospcRunner {
            device_usage_stdout: fixture,
        };

        let result = check_relocation_space(&runner, "/mnt/storage", None);
        assert!(result.is_ok(), "should pass: {result:?}");
    }

    #[test]
    // Intent: check_relocation_space with --missing-id only counts allocations
    //   for the targeted devid, not all missing devices.
    //
    // Why it exists: When multiple devices are missing, removing just one may
    //   be feasible even if removing all isn't.
    //
    // Scenario: Two missing devices, but only one is targeted. The targeted
    //   device has small allocations that fit in survivors.
    fn check_relocation_space_with_missing_id_filters() {
        // Two surviving devices (4-disk pool, 2 missing). The RAID1-aware check
        // requires >= 2 surviving devices with space, which this fixture satisfies.
        let fixture = "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:           200000000

/dev/mapper/braid-disk4, ID: 4
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:             67108864
   Unallocated:           200000000

<missing disk>, ID: 2
   Device size:                  0
   Device slack:                  0
   Data,RAID1:             50000000
   Unallocated:                  0

<missing disk>, ID: 3
   Device size:                  0
   Device slack:                  0
   Data,RAID1:           5000000000
   Unallocated:                  0

";

        let runner = EnospcRunner {
            device_usage_stdout: fixture,
        };

        // Targeting devid 2 (50 MB Data) — should pass: RAID1 capacity = 200 MB >= 50 MB
        let result = check_relocation_space(&runner, "/mnt/storage", Some(2));
        assert!(result.is_ok(), "targeting devid 2 should pass: {result:?}");

        // Targeting devid 3 (5 GB Data) — should fail: RAID1 capacity = 200 MB < 5 GB
        let result = check_relocation_space(&runner, "/mnt/storage", Some(3));
        assert!(result.is_err(), "targeting devid 3 should fail");
    }

    #[test]
    // Intent: check_relocation_space proceeds gracefully when the command fails.
    //
    // Why it exists: A bug in the safety check shouldn't block a valid operation.
    //
    // Scenario: btrfs device usage returns an error (e.g., old kernel, permission issue).
    fn check_relocation_space_proceeds_on_command_error() {
        struct FailingRunner;
        impl CommandRunner for FailingRunner {
            fn run(&self, _request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
                Err(CmdError::MissingMock)
            }
            fn run_with_stdin(
                &self,
                request: &CmdRequest,
                _stdin: &[u8],
            ) -> Result<RawCommandOutput, CmdError> {
                self.run(request)
            }
        }

        let result = check_relocation_space(&FailingRunner, "/mnt/storage", None);
        assert!(result.is_ok(), "should proceed on error: {result:?}");
    }

    // --- compile_steps tests ---

    #[test]
    // Intent: dry-run with 1 missing + ≥2 survivors shows rebalance step.
    // Why: operator should see the soft balance step in the plan.
    // Scenario: 3-disk pool, 1 disk failed. Dry run should show the balance.
    fn compile_steps_shows_rebalance_when_clearing_last_missing() {
        let steps = compile_steps(3, true, 2, &MountPoint("/mnt/storage".into()));
        assert!(
            steps
                .iter()
                .any(|s| s.description.contains("-dconvert=raid1,soft")),
            "expected soft balance step; got: {:?}",
            steps.iter().map(|s| &s.description).collect::<Vec<_>>()
        );
    }

    #[test]
    // Intent: dry-run with 1 survivor omits rebalance step.
    // Why: can't have RAID1 with only 1 device.
    // Scenario: 2-disk pool, 1 died. Only 1 survivor — no balance.
    fn compile_steps_omits_rebalance_with_single_survivor() {
        let steps = compile_steps(3, true, 1, &MountPoint("/mnt/storage".into()));
        assert!(
            !steps
                .iter()
                .any(|s| s.description.contains("-dconvert=raid1,soft")),
            "should not show soft balance with 1 survivor; got: {:?}",
            steps.iter().map(|s| &s.description).collect::<Vec<_>>()
        );
    }

    #[test]
    // Intent: dry-run when not clearing last missing omits rebalance step.
    // Why: if more missing devices remain, balance would be premature.
    // Scenario: 4-disk pool, 2 missing, removing 1 of them.
    fn compile_steps_omits_rebalance_when_not_last_missing() {
        let steps = compile_steps(3, false, 2, &MountPoint("/mnt/storage".into()));
        assert!(
            !steps
                .iter()
                .any(|s| s.description.contains("-dconvert=raid1,soft")),
            "should not show soft balance when not clearing last missing; got: {:?}",
            steps.iter().map(|s| &s.description).collect::<Vec<_>>()
        );
    }

    // --- RecordingRunner for 3-device pool scenarios ---

    /// 3-device pool RecordingRunner: 2 present + 1 missing.
    /// After remove-missing, shows 2 present + 0 missing (healthy).
    #[derive(Clone)]
    struct ThreeDeviceRunner {
        log: Arc<Mutex<Vec<CmdRequest>>>,
        /// If true, post-op probe still shows 1 missing
        still_degraded_after: bool,
    }

    impl ThreeDeviceRunner {
        fn new(log: Arc<Mutex<Vec<CmdRequest>>>, still_degraded: bool) -> Self {
            Self {
                log,
                still_degraded_after: still_degraded,
            }
        }
    }

    impl CommandRunner for ThreeDeviceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());

            // Track whether we've already removed the missing device
            let remove_done = self
                .log
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. }));

            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                    0,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    let (missing_line, total) = if remove_done && !self.still_degraded_after {
                        ("", 2)
                    } else {
                        ("\tdevid    3 size 0 used 0 path MISSING\n", 3)
                    };
                    Ok(mock_out(
                        &format!("btrfs filesystem show {mount_point}"),
                        &format!(
                            "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices {total} FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n{missing_line}",
                        ),
                        0,
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_out(
                    &format!("cryptsetup status {mapper}"),
                    &format!(
                        "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"
                    ),
                    0,
                )),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_out(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_out(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                    0,
                )),
                CmdRequest::BtrfsDeviceRemove { .. } => {
                    Ok(mock_out("btrfs device remove", "", 0))
                }
                CmdRequest::BtrfsBalanceRaid1Soft { .. } => {
                    Ok(mock_out("btrfs balance start -dconvert=raid1,soft", "", 0))
                }
                CmdRequest::BtrfsDeviceUsageRaw { .. } => {
                    // Return enough space for relocation check to pass
                    Ok(mock_out(
                        "btrfs device usage --raw",
                        "/dev/mapper/braid-disk1, ID: 1\n   Device size:           520093696\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:           452984832\n\n/dev/mapper/braid-disk2, ID: 2\n   Device size:           520093696\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:           452984832\n\n<missing disk>, ID: 3\n   Device size:                  0\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:                  0\n\n",
                        0,
                    ))
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

    fn three_device_config() -> (
        tempfile::TempDir,
        std::path::PathBuf,
        tempfile::TempDir,
        StatePaths,
    ) {
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        let config_json = serde_json::json!({ "mount_point": "/mnt/storage" });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();
        let (state_tmp, state_paths) = test_paths(&[
            ("disk1", "/dev/disk/by-id/virtio-disk1", Some(1)),
            ("disk2", "/dev/disk/by-id/virtio-disk2", Some(2)),
            ("disk3", "/dev/disk/by-id/virtio-disk3", Some(3)),
        ]);
        (tmp, config_path, state_tmp, state_paths)
    }

    #[test]
    // Intent: 3-disk pool, 1 missing → soft rebalance runs after remove-missing.
    // Why: clearing the last missing device should restore RAID1 for chunks
    // written during degraded operation.
    // Scenario: 3-disk NAS, one drive dies. Operator runs remove-missing.
    // After the removal, pool is healthy with 2 survivors → soft balance runs.
    fn three_device_pool_soft_rebalance_runs() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();
        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = ThreeDeviceRunner::new(log.clone(), false);
        cmd_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: false,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
            },
        )
        .expect("remove-missing should succeed");

        let calls = log.lock().unwrap();
        let remove_pos = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. }));
        let balance_pos = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsBalanceRaid1Soft { .. }));
        assert!(
            remove_pos.is_some(),
            "expected BtrfsDeviceRemove; calls: {calls:?}"
        );
        assert!(
            balance_pos.is_some(),
            "expected BtrfsBalanceRaid1Soft; calls: {calls:?}"
        );
        assert!(
            remove_pos.unwrap() < balance_pos.unwrap(),
            "remove-missing must happen before soft balance"
        );
    }

    #[test]
    // Intent: 3-disk pool, 2 missing, targeting 1 → NO rebalance (still degraded).
    // Why: running a balance while still degraded is pointless.
    // Scenario: 3-disk NAS, 2 drives die. Operator removes 1 missing entry.
    // Pool still has 1 missing device → no rebalance.
    fn three_device_two_missing_no_rebalance() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();
        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = ThreeDeviceRunner::new(log.clone(), true);
        cmd_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: false,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
            },
        )
        .expect("remove-missing should succeed");

        let calls = log.lock().unwrap();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsBalanceRaid1Soft { .. })),
            "should NOT call BtrfsBalanceRaid1Soft when still degraded; calls: {calls:?}"
        );
    }

    /// Runner for 3-device pool where the soft balance fails after successful
    /// device removal. Everything succeeds except BtrfsBalanceRaid1Soft.
    struct FailingSoftBalanceRunner {
        log: Arc<Mutex<Vec<CmdRequest>>>,
    }

    impl FailingSoftBalanceRunner {
        fn new(log: Arc<Mutex<Vec<CmdRequest>>>) -> Self {
            Self { log }
        }
    }

    impl CommandRunner for FailingSoftBalanceRunner {
        fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
            self.log.lock().unwrap().push(request.clone());

            let remove_done = self
                .log
                .lock()
                .unwrap()
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. }));

            match request {
                CmdRequest::FindmntJson { mount_point } => Ok(mock_out(
                    &format!("findmnt --json --mountpoint {mount_point}"),
                    r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-disk1","fstype":"btrfs"}]}"#,
                    0,
                )),
                CmdRequest::BtrfsFilesystemShow { mount_point } => {
                    let (missing_line, total) = if remove_done {
                        ("", 2)
                    } else {
                        ("\tdevid    3 size 0 used 0 path MISSING\n", 3)
                    };
                    Ok(mock_out(
                        &format!("btrfs filesystem show {mount_point}"),
                        &format!(
                            "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices {total} FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n{missing_line}",
                        ),
                        0,
                    ))
                }
                CmdRequest::CryptsetupStatus { mapper } => Ok(mock_out(
                    &format!("cryptsetup status {mapper}"),
                    &format!("{mapper} is active and is in use.\n  type:    LUKS2\n  device:  /dev/vdb\n  mode:    read/write\n"),
                    0,
                )),
                CmdRequest::CryptsetupLuksUuid { .. } => Ok(mock_out(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_out(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                    0,
                )),
                CmdRequest::BtrfsDeviceRemove { .. } => {
                    Ok(mock_out("btrfs device remove", "", 0))
                }
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(mock_out(
                    "btrfs device usage --raw",
                    "/dev/mapper/braid-disk1, ID: 1\n   Device size:           520093696\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:           452984832\n\n/dev/mapper/braid-disk2, ID: 2\n   Device size:           520093696\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:           452984832\n\n<missing disk>, ID: 3\n   Device size:                  0\n   Device slack:                  0\n   Data,RAID1:            67108864\n   Unallocated:                  0\n\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceRaid1Soft { .. } => Ok(RawCommandOutput {
                    cmd: "btrfs balance start -dconvert=raid1,soft".into(),
                    stdout: String::new(),
                    stderr: "ERROR: error during balancing: No space left on device".into(),
                    exit_status: 1,
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

    #[test]
    // Intent: pending-op.json survives when soft balance fails after a successful
    //   device removal.
    //
    // Why it exists: remove-missing previously cleared the journal before
    //   maybe_restore_raid1(). If the soft balance failed, the journal was already
    //   gone despite an irreversible pool change, leaving pool.json stale with
    //   no recovery path.
    //
    // Scenario: 3-disk NAS, one drive dies. Operator runs remove-missing. The
    //   device removal succeeds but the post-removal soft balance fails. The
    //   journal must persist so `braid recover` can reconcile.
    fn journal_survives_soft_balance_failure() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = FailingSoftBalanceRunner::new(log.clone());
        let result = cmd_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: false,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
            },
        );

        assert!(
            result.is_err(),
            "remove-missing should fail when soft balance fails"
        );
        assert!(
            journal::load_journal(&state_paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
    }

    #[test]
    // Intent: when soft balance fails with ENOSPC, the surfaced error includes
    //   the recovery hint with a concrete `dusage=0` command.
    // Why: the hint is appended in pool::balance_error, but it must survive
    //   PoolError → RemoveMissingError::Pool → Display without being lost.
    // Scenario: 3-disk NAS, one drive dies. Operator runs remove-missing. Device
    //   removal succeeds but the post-removal soft balance hits ENOSPC. The error
    //   message should guide the user to free empty block groups.
    fn enospc_hint_surfaces_through_error_chain() {
        let (_tmp, config_path, _state_tmp, state_paths) = three_device_config();

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = FailingSoftBalanceRunner::new(log.clone());
        let result = cmd_remove_missing(
            &runner,
            &MockFs,
            &RemoveMissingParams {
                config_path: &config_path,
                missing_id: 3,
                dry_run: false,
                yes: true,
                progress: crate::progress::ProgressOutput::Off,
                paths: &state_paths,
            },
        );

        let err = result.unwrap_err().to_string();
        assert!(
            err.contains("hint:"),
            "error should contain recovery hint: {err}"
        );
        assert!(
            err.contains("dusage=0"),
            "error should suggest dusage=0 filter: {err}"
        );
    }

    // --- resolve_removal_target tests ---

    #[test]
    // Intent: resolve_removal_target fails when no devid is available.
    //
    // Why it exists: When btrfs only prints the "*** Some devices missing"
    //   sentinel (no explicit devid line), target_devid is None. Previously
    //   the code silently skipped membership removal, leaving pool.json
    //   with the dead disk still listed.
    //
    // Scenario: Single missing device on an older kernel that doesn't emit
    //   per-device MISSING lines. User runs remove-missing without --missing-id.
    fn resolve_target_fails_when_devid_unavailable() {
        let m = PoolMembership::empty();
        let err = resolve_removal_target(None, &m).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("missing device's ID"),
            "expected hint about missing device ID; got: {msg}"
        );
    }

    #[test]
    // Intent: resolve_removal_target fails when devid is known but no
    //   pool.json member has that devid enriched.
    //
    // Why it exists: If devid enrichment was skipped or failed, the lookup
    //   returns None. Previously the code silently proceeded, leaving
    //   pool.json unchanged despite the btrfs device being removed.
    //
    // Scenario: User enrolled a disk before devid enrichment existed, then
    //   the disk fails. remove-missing has a devid from btrfs but can't
    //   match it to any pool.json entry.
    fn resolve_target_fails_when_devid_not_in_membership() {
        let mut m = PoolMembership::empty();
        m.disks.insert(
            "disk1".to_string(),
            DiskMember::from_by_id(ByIdPath("/dev/disk/by-id/virtio-disk1".to_string())),
        );
        let err = resolve_removal_target(Some(99), &m).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not found in pool.json"),
            "expected pool.json membership error; got: {msg}"
        );
    }

    #[test]
    // Intent: dry-run for targeted missing-device removal shows the devid command.
    // Why: verifies CmdRequest integration for the targeted removal path.
    // Scenario: one missing device (devid 2), last missing, 2 present → includes balance.
    fn dry_run_render_targeted_removal_with_balance() {
        let mount_point = MountPoint("/mnt/storage".into());
        let steps = compile_steps(2, true, 2, &mount_point);
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // 2 steps: device remove + balance, each with 1 command = 4 lines
        assert_eq!(lines.len(), 4, "expected 4 lines, got:\n{output}");
        assert!(lines[0].contains("target specific missing device"));
        assert_eq!(
            lines[1],
            "               $ btrfs device remove --enqueue 2 /mnt/storage"
        );
        assert!(lines[2].contains("restore redundancy"));
        assert_eq!(
            lines[3],
            "               $ btrfs balance start --enqueue '-dconvert=raid1,soft' '-mconvert=raid1,soft' /mnt/storage"
        );
    }

    // --- Confirmation formatter tests ---

    #[test]
    fn remove_missing_confirm_with_rebalance() {
        let msg = format_remove_missing_confirm("toshiba", 2, 2, 1);
        assert!(msg.contains("Remove missing device from pool:"));
        assert!(msg.contains("toshiba (devid 2)"));
        assert!(msg.contains("missing"));
        assert!(msg.contains("no hardware info available"));
        assert!(msg.contains("rebalanced"));
        assert!(msg.contains("2 present + 1 missing \u{2192} 2 disks"));
    }

    #[test]
    fn remove_missing_confirm_single_survivor() {
        let msg = format_remove_missing_confirm("toshiba", 2, 1, 1);
        assert!(msg.contains("Surviving disk already has all data"));
        assert!(msg.contains("1 present + 1 missing \u{2192} 1 disk"));
    }
}
