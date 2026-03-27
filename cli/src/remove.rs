use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::{config_read, mapper_name};
use crate::membership;
use crate::parse::parse_btrfs_device_usage;
use crate::pool::evict_present_device;
use crate::preflight;
use crate::probe::{probe_pool, ProbeError};
use crate::progress::ProgressOutput;
use crate::state_paths::StatePaths;
use crate::types::*;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
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

pub struct RemoveStep {
    pub risk: &'static str,
    pub description: String,
}

pub fn cmd_remove<R: CommandRunner + Sync>(
    runner: &R,
    config_path: &Path,
    name: &str,
    dry_run: bool,
    yes: bool,
    progress: ProgressOutput,
    paths: &StatePaths,
) -> Result<(), RemoveError> {
    let config = config_read(config_path)?;

    let pool = match probe_pool(runner, config.mount_point().as_str()) {
        Ok(p) => p,
        Err(ProbeError::NotBtrfs { .. }) => {
            return Err(RemoveError::Validation(
                "pool is not mounted. Nothing to remove.".into(),
            ));
        }
        Err(e) => return Err(RemoveError::Probe(e)),
    };

    if !pool.mounted {
        return Err(RemoveError::Validation(
            "pool is not mounted. Nothing to remove.".into(),
        ));
    }

    // Preflight
    preflight::check_no_exclusive_op(runner, config.mount_point().as_str())
        .map_err(RemoveError::Validation)?;
    preflight::check_not_read_only(runner, config.mount_point().as_str())
        .map_err(RemoveError::Validation)?;

    let mn = mapper_name(name);

    // Is the disk present in the pool?
    let in_pool = pool.devices.iter().any(|d| d.mapper == mn);

    if !in_pool {
        let mut msg = format!("disk '{}' not found in pool.", name);
        if pool.missing_count > 0 {
            msg.push_str(&format!(
                " ({} missing device{} detected. \
                 To repair onto a new disk, use `braid replace`. \
                 To forget the entry, use `braid remove-missing`.)",
                pool.missing_count,
                if pool.missing_count == 1 { "" } else { "s" }
            ));
        }
        return Err(RemoveError::Validation(msg));
    }

    preflight::check_no_missing_devices(pool.missing_count, "remove a live disk from the pool")
        .map_err(RemoveError::Validation)?;

    let remaining = pool.devices.len() - 1;
    let target_devid = pool
        .devices
        .iter()
        .find(|d| d.mapper == mn)
        .map(|d| d.devid);
    // Pre-flight: reject if other devices lack space to absorb data from
    // the device being removed. Without this, btrfs will either ENOSPC
    // instantly or crash the filesystem to read-only mid-relocation
    // (see tests/repro/).
    //
    // Skip for single-survivor removals (remaining == 1): the eviction
    // path balances RAID1→single first, which handles data redistribution.
    // This does not match the reproduced relocation-failure mode.
    if remaining > 1 {
        if let Some(devid) = target_devid {
            check_eviction_space(runner, config.mount_point().as_str(), devid)?;
        }
    }

    let steps = compile_remove_present_steps(&mn, &pool)?;

    if dry_run {
        for step in &steps {
            println!("[{:<11}] {}", step.risk, step.description);
        }
        return Ok(());
    }

    if steps.is_empty() {
        eprintln!("Nothing to do.");
        return Ok(());
    }

    // Confirm
    if !yes {
        if remaining == 0 {
            return Err(RemoveError::Validation(
                "cannot remove the last disk from the pool".into(),
            ));
        }
        if remaining == 1 {
            eprintln!("WARNING: Removing this disk leaves only 1 disk — no redundancy.");
            eprint!("Type 'remove without redundancy' to confirm: ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).map_err(|e| {
                RemoveError::Validation(format!("failed to read confirmation: {e}"))
            })?;
            if input.trim() != "remove without redundancy" {
                return Err(RemoveError::Validation("aborted by user".into()));
            }
        } else {
            eprintln!(
                "Remove {} from pool? Data will migrate off this disk.",
                name
            );
            eprint!("Type 'yes' to continue: ");
            let mut input = String::new();
            std::io::stdin().read_line(&mut input).map_err(|e| {
                RemoveError::Validation(format!("failed to read confirmation: {e}"))
            })?;
            if input.trim() != "yes" {
                return Err(RemoveError::Validation("aborted by user".into()));
            }
        }
    }

    // Pre-commit: persist membership removal BEFORE irreversible disk op.
    // Fail hard if membership cannot be loaded or saved — proceeding without
    // updating pool.json would let btrfs state diverge from authoritative membership.
    let mut m = membership::load_membership(paths)
        .map_err(|e| RemoveError::Validation(format!("failed to load pool membership: {e}")))?;
    m.disks.remove(name);
    membership::save_membership(&m, paths)
        .map_err(|e| RemoveError::Validation(format!("failed to persist pool membership: {e}")))?;

    // Execute
    evict_present_device(runner, &mn.0, config.mount_point().as_str(), progress)?;

    eprintln!("Done. Disk '{}' removed from pool.", name);
    Ok(())
}

/// Check that the remaining devices have enough RAID1-aware, per-type space to
/// absorb data from the device being removed. If they don't, btrfs device remove
/// will either ENOSPC instantly or crash the filesystem to read-only mid-relocation.
///
/// If the check itself fails (parse error, command error), we log a warning and
/// proceed — a bug in the safety net shouldn't block a valid operation.
fn check_eviction_space<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
    target_devid: u64,
) -> Result<(), RemoveError> {
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

    let target: Vec<_> = usage
        .devices
        .iter()
        .filter(|d| d.devid == target_devid)
        .collect();
    let remaining: Vec<_> = usage
        .devices
        .iter()
        .filter(|d| d.devid != target_devid)
        .collect();

    preflight::check_raid1_relocation_space(&target, &remaining).map_err(|e| {
        RemoveError::Validation(format!(
            "{e}\n\nFree up space by deleting files, or add a new device first with `braid add`."
        ))
    })
}

fn compile_remove_present_steps(
    mn: &MapperName,
    pool: &PoolState,
) -> Result<Vec<RemoveStep>, RemoveError> {
    let remaining = pool.devices.len() - 1;
    if remaining == 0 {
        return Err(RemoveError::Validation(
            "cannot remove the last disk from the pool".into(),
        ));
    }

    let mut steps = Vec::new();
    if remaining == 1 {
        steps.push(RemoveStep {
            risk: "long",
            description: "btrfs balance -dconvert=single -mconvert=dup -f (RAID1 → single)".into(),
        });
    }
    steps.push(RemoveStep {
        risk: "long",
        description: format!(
            "btrfs device remove /dev/mapper/{} (data migrates off disk)",
            mn
        ),
    });
    steps.push(RemoveStep {
        risk: "safe",
        description: format!("cryptsetup close {}", mn),
    });
    Ok(steps)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
    use crate::membership::{self, PoolMembership};
    use crate::progress::ProgressOutput;
    use crate::state_paths::StatePaths;
    use crate::types::ByIdPath;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

    fn setup_membership(disks: &[(&str, &str)]) -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let mut m = PoolMembership::empty();
        for (name, by_id) in disks {
            m.disks.insert(
                name.to_string(),
                membership::DiskMember::from_by_id(ByIdPath(by_id.to_string())),
            );
        }
        membership::save_membership_to(&m, &paths.pool_json()).unwrap();
        (tmp, paths)
    }

    fn mock_out(cmd: &str, stdout: &str, exit_status: i32) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status,
        }
    }

    #[derive(Clone)]
    struct RecordingRunner {
        log: Arc<Mutex<Vec<CmdRequest>>>,
    }

    impl RecordingRunner {
        fn new(log: Arc<Mutex<Vec<CmdRequest>>>) -> Self {
            Self { log }
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
                    "Label: none  uuid: cc86845b-aec3-408e-bef5-553affc1f2b1\n\tTotal devices 2 FS bytes used 16.17MiB\n\tdevid    1 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk1\n\tdevid    2 size 496.00MiB used 121.56MiB path /dev/mapper/braid-disk2\n",
                    0,
                )),
                CmdRequest::CryptsetupStatus { mapper } => {
                    let dev = if mapper == "braid-disk1" {
                        "/dev/vdb"
                    } else {
                        "/dev/vdc"
                    };
                    Ok(mock_out(
                        &format!("cryptsetup status {mapper}"),
                        &format!(
                            "{mapper} is active and is in use.\n  type:    LUKS2\n  device:  {dev}\n  mode:    read/write\n"
                        ),
                        0,
                    ))
                }
                CmdRequest::CryptsetupLuksUuid { device } => {
                    let uuid = if device == "/dev/vdb" {
                        "11111111-1111-1111-1111-111111111111"
                    } else {
                        "22222222-2222-2222-2222-222222222222"
                    };
                    Ok(mock_out(
                        &format!("cryptsetup luksUUID {device}"),
                        &format!("{uuid}\n"),
                        0,
                    ))
                }
                CmdRequest::BtrfsBalanceStatus { .. } => Ok(mock_out(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                    0,
                )),
                CmdRequest::BtrfsBalanceSingle { .. } => Ok(mock_out("btrfs balance start", "", 0)),
                CmdRequest::BtrfsDeviceRemove { .. } => Ok(mock_out("btrfs device remove", "", 0)),
                CmdRequest::CryptsetupClose { .. } => Ok(mock_out("cryptsetup close", "", 0)),
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
    // Intent: cmd_remove succeeds on a 2→1 removal without invoking the ENOSPC
    //   pre-flight check.
    //
    // Why: The 2→1 eviction path balances RAID1→single before device remove,
    //   which does not match the reproduced relocation-failure mode. The pre-
    //   flight check compares pre-balance allocations and would false-positive.
    //
    // Scenario: User removes one disk from a healthy 2-disk pool. The disks are
    //   small and mostly allocated. The operation succeeds because the balance
    //   step handles redistribution, making the space check irrelevant.
    fn enospc_check_skipped_for_two_to_one_removal() {
        let (_state_dir, paths) = setup_membership(&[
            ("disk1", "/dev/disk/by-id/virtio-disk1"),
            ("disk2", "/dev/disk/by-id/virtio-disk2"),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");

        let mut disks = BTreeMap::new();
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
        cmd_remove(
            &runner,
            Path::new(&config_path),
            "disk2",
            false,
            true,
            ProgressOutput::Off,
            &paths,
        )
        .expect("remove should succeed");

        let calls = log.lock().unwrap();
        assert!(
            !calls
                .iter()
                .any(|c| matches!(c, CmdRequest::BtrfsDeviceUsageRaw { .. })),
            "ENOSPC pre-flight should not be invoked for 2→1 removal; calls: {calls:?}"
        );
    }

    #[test]
    // Intent:
    // - What behavior this test (tries to) verify.
    //   - `braid remove` converts RAID1 to single before removing a device when only one disk remains.
    //
    // Why it exists:
    // - What risk/regression this protects against.
    //   - Prevents command-order regressions that make `btrfs device remove` fail under RAID1 minimum-device constraints.
    //
    // Scenario:
    // - Real-world situation this models (user/system story). Especially the
    //   specific scenario that inspired this test (like a real world bug).
    //   - Operator removes one disk from a healthy two-disk pool and expects the operation to succeed end-to-end.
    fn remove_two_disk_pool_balances_single_before_device_remove() {
        let (_state_dir, paths) = setup_membership(&[
            ("disk1", "/dev/disk/by-id/virtio-disk1"),
            ("disk2", "/dev/disk/by-id/virtio-disk2"),
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");

        let mut disks = BTreeMap::new();
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
        cmd_remove(
            &runner,
            Path::new(&config_path),
            "disk2",
            false,
            true,
            ProgressOutput::Off,
            &paths,
        )
        .expect("remove should succeed");

        let calls = log.lock().unwrap();
        let balance_idx = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsBalanceSingle { .. }))
            .expect("expected balance-to-single request");
        let remove_idx = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsDeviceRemove { .. }))
            .expect("expected device-remove request");

        assert!(
            balance_idx < remove_idx,
            "expected balance-to-single before device-remove; calls: {calls:?}"
        );
    }
}
