use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::config_read;
use crate::disk_map;
use crate::parse::parse_btrfs_device_usage;
use crate::pool::{pool_remove_devid, pool_remove_missing};
use crate::preflight;
use crate::probe::{probe_pool, ProbeError};
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

pub struct RemoveMissingStep {
    pub risk: &'static str,
    pub description: String,
}

pub fn cmd_remove_missing<R: CommandRunner + Sync>(
    runner: &R,
    config_path: &Path,
    missing_id: Option<u64>,
    dry_run: bool,
    yes: bool,
) -> Result<(), RemoveMissingError> {
    let config = config_read(config_path)?;
    let disk_map_state = disk_map::load_disk_map();
    disk_map::validate_config_name_stability(&config, &disk_map_state)
        .map_err(|e| RemoveMissingError::Validation(e.to_string()))?;

    let pool = match probe_pool(runner, config.mount_point()) {
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
    preflight::check_no_exclusive_op(runner, config.mount_point())
        .map_err(RemoveMissingError::Validation)?;
    preflight::check_not_read_only(runner, config.mount_point())
        .map_err(RemoveMissingError::Validation)?;

    if pool.missing_count == 0 {
        return Err(RemoveMissingError::Validation(
            "no missing devices detected in pool.".into(),
        ));
    }

    if pool.missing_count > 1 && missing_id.is_none() {
        return Err(RemoveMissingError::Validation(format!(
            "multiple missing devices ({} missing). Pass --missing-id <devid> to target a specific one. Use 'braid status --verbose' to see device IDs.",
            pool.missing_count
        )));
    }

    if let Some(devid) = missing_id {
        if pool.devices.iter().any(|d| d.devid == devid) {
            return Err(RemoveMissingError::Validation(format!(
                "devid {devid} is a live device, not a missing one. \
                 Use 'braid remove' to remove live devices."
            )));
        }
        let missing_devids = preflight::probe_missing_devids(runner, config.mount_point())
            .map_err(RemoveMissingError::Validation)?;
        if !missing_devids.contains(&devid) {
            return Err(RemoveMissingError::Validation(format!(
                "devid {devid} is not a device in this pool. \
                 Use 'braid status --verbose' to see device IDs."
            )));
        }
    }

    // Pre-flight: reject if survivors lack space to absorb the missing
    // device's data. Without this check, btrfs will either ENOSPC or
    // crash the filesystem to read-only mid-relocation (see tests/repro/).
    //
    // Skip when only 1 present device survives: in 2-device RAID1, the
    // survivor already has all data (every chunk is mirrored). This does
    // not match the reproduced relocation-failure mode.
    if pool.devices.len() >= 2 {
        check_relocation_space(runner, config.mount_point(), missing_id)?;
    }

    let steps = compile_steps(missing_id);

    if dry_run {
        for step in &steps {
            println!("[{:<11}] {}", step.risk, step.description);
        }
        return Ok(());
    }

    // Confirm
    if !yes {
        if let Some(devid) = missing_id {
            eprintln!("Remove missing device (devid {}) from pool?", devid);
        } else {
            eprintln!("Remove missing device from pool?");
        }
        eprint!("Type 'remove missing' to confirm: ");
        let mut input = String::new();
        std::io::stdin().read_line(&mut input).map_err(|e| {
            RemoveMissingError::Validation(format!("failed to read confirmation: {e}"))
        })?;
        if input.trim() != "remove missing" {
            return Err(RemoveMissingError::Validation("aborted by user".into()));
        }
    }

    // Execute
    if let Some(devid) = missing_id {
        eprintln!("Removing missing device (devid {}) from pool...", devid);
        pool_remove_devid(runner, config.mount_point(), devid)?;
    } else {
        eprintln!("Removing missing device from pool...");
        pool_remove_missing(runner, config.mount_point())?;
    }

    // Update disk map (best effort — never fail the remove-missing)
    if let Some(devid) = missing_id {
        // Targeted removal: prune entries with this specific devid
        disk_map::update_disk_map_best_effort(|map| {
            disk_map::remove_disks_by_devids(map, &[devid]);
        });
    } else if let Ok(pool_after) = probe_pool(runner, config.mount_point()) {
        // General removal: prune entries whose devid is no longer in pool
        let live_devids: Vec<u64> = pool_after.devices.iter().map(|d| d.devid).collect();
        disk_map::update_disk_map_best_effort(|map| {
            disk_map::prune_absent_devids(map, &live_devids);
        });
    }

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
        mount_point: mount_point.to_owned(),
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
    let remaining: Vec<_> = usage
        .devices
        .iter()
        .filter(|d| d.device_size > 0)
        .collect();

    preflight::check_raid1_relocation_space(&target, &remaining)
        .map_err(|e| {
            RemoveMissingError::Validation(format!(
                "{e}\n\nFree up space by deleting files, or add a new device first with `braid add`."
            ))
        })
}

fn compile_steps(missing_id: Option<u64>) -> Vec<RemoveMissingStep> {
    let mut steps = Vec::new();
    if let Some(devid) = missing_id {
        steps.push(RemoveMissingStep {
            risk: "long",
            description: format!(
                "btrfs device remove {} (target specific missing device)",
                devid
            ),
        });
    } else {
        steps.push(RemoveMissingStep {
            risk: "long",
            description: "btrfs device remove missing".into(),
        });
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};

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
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\t*** Some devices missing\n",
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
                CmdRequest::BtrfsDeviceRemoveMissing { .. } => {
                    Ok(mock_out("btrfs device remove missing", "", 0))
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
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");

        let mut disks = std::collections::BTreeMap::new();
        disks.insert(
            "disk1".to_owned(),
            serde_json::json!({ "by_id": "/dev/disk/by-id/virtio-disk1" }),
        );
        disks.insert(
            "disk2".to_owned(),
            serde_json::json!({ "by_id": "/dev/disk/by-id/virtio-disk2" }),
        );
        let config_json = serde_json::json!({
            "disks": disks,
            "mount_point": "/mnt/storage"
        });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = RecordingRunner::new(log.clone());
        cmd_remove_missing(&runner, &config_path, None, false, true)
            .expect("remove-missing should succeed");

        let calls = log.lock().unwrap();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceUsageRaw { .. })),
            "ENOSPC pre-flight should not be invoked for single-survivor removal; calls: {calls:?}"
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
}
