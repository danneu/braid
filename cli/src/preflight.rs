use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::types::BalanceState;
use crate::parse::{parse_btrfs_balance_status, parse_btrfs_device_usage, parse_findmnt_json};

/// Refuse if a btrfs balance or device remove is already running.
/// Fail-closed: if we can't determine state, refuse. Unmounting or starting
/// a second exclusive op during an active one risks filesystem corruption.
pub fn check_no_exclusive_op<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
) -> Result<(), String> {
    let raw = runner
        .run(&CmdRequest::BtrfsBalanceStatus {
            mount_point: mount_point.to_owned(),
        })
        .map_err(|e| format!("cannot determine whether an exclusive operation is running: {e}"))?;

    let status = parse_btrfs_balance_status(&raw)
        .map_err(|e| format!("cannot determine whether an exclusive operation is running: {e}"))?;

    match status.state {
        BalanceState::Running { .. } => {
            Err("a btrfs balance is already running. Wait for it to complete.".into())
        }
        BalanceState::Paused { .. } => {
            Err("a btrfs balance is paused. Resume or cancel it before proceeding.".into())
        }
        BalanceState::None => Ok(()),
    }
}

/// Refuse if the pool is mounted read-only.
/// Runs its own findmnt probe — avoids adding mount_options to PoolState
/// and touching all 7+ PoolState construction sites.
pub fn check_not_read_only<R: CommandRunner>(runner: &R, mount_point: &str) -> Result<(), String> {
    let raw = match runner.run(&CmdRequest::FindmntJson {
        mount_point: mount_point.to_owned(),
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("warning: read-only pre-flight failed: {e}; proceeding anyway");
            return Ok(());
        }
    };

    let findmnt = match parse_findmnt_json(&raw) {
        Ok(f) => f,
        Err(e) => {
            eprintln!("warning: read-only pre-flight failed: {e}; proceeding anyway");
            return Ok(());
        }
    };

    let entry = findmnt.filesystems.iter().find(|e| e.target == mount_point);
    if let Some(entry) = entry {
        if entry.options.split(',').any(|opt| opt.trim() == "ro") {
            return Err(format!(
                "pool is mounted read-only. Remount read-write first:\n  \
                 mount -o remount,rw {mount_point}"
            ));
        }
    }
    Ok(())
}

/// Refuse if the pool has missing devices.
pub fn check_no_missing_devices(missing_count: u64, action: &str) -> Result<(), String> {
    if missing_count > 0 {
        Err(format!(
            "pool has {missing_count} missing device{}. \
             Run `braid remove-missing` before you {action}.",
            if missing_count == 1 { "" } else { "s" },
        ))
    } else {
        Ok(())
    }
}

/// Return the set of devids that are missing (device_size == 0 in btrfs
/// device usage output). Used to validate --missing-id arguments.
pub fn probe_missing_devids<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
) -> Result<Vec<u64>, String> {
    let raw = runner
        .run(&CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: mount_point.to_owned(),
        })
        .map_err(|e| format!("failed to probe device usage: {e}"))?;

    let usage =
        parse_btrfs_device_usage(&raw).map_err(|e| format!("failed to parse device usage: {e}"))?;

    Ok(usage
        .devices
        .iter()
        .filter(|d| d.device_size == 0)
        .map(|d| d.devid)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdRequest, MockRunner, RawCommandOutput};

    #[test]
    // Intent: check_no_exclusive_op passes when no balance is active.
    // Why: Confirms the happy path doesn't block valid operations.
    // Scenario: Operator runs a command while the pool is idle.
    fn exclusive_op_passes_when_no_balance() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceStatus {
                mount_point: "/mnt/storage".into(),
            },
            RawCommandOutput {
                cmd: "btrfs balance status /mnt/storage".into(),
                stdout: "No balance found on '/mnt/storage'\n".into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        assert!(check_no_exclusive_op(&runner, "/mnt/storage").is_ok());
    }

    #[test]
    // Intent: check_no_exclusive_op refuses when a balance is running.
    // Why: Proceeding during an active balance risks filesystem corruption.
    // Scenario: Operator tries `braid remove` while a RAID1 balance is in progress.
    fn exclusive_op_refuses_when_balance_running() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceStatus {
                mount_point: "/mnt/storage".into(),
            },
            RawCommandOutput {
                cmd: "btrfs balance status /mnt/storage".into(),
                stdout: "Balance on '/mnt/storage' is running\n\
                         3 out of about 10 chunks balanced (7 considered), 70% left\n"
                    .into(),
                stderr: String::new(),
                exit_status: 1,
            },
        );
        let err = check_no_exclusive_op(&runner, "/mnt/storage").unwrap_err();
        assert!(
            err.contains("already running"),
            "expected 'already running' in: {err}"
        );
    }

    #[test]
    // Intent: check_no_exclusive_op refuses when a balance is paused.
    // Why: A paused balance still holds exclusive state; starting another op fails.
    // Scenario: Operator paused a balance and forgot, then tries `braid add`.
    fn exclusive_op_refuses_when_balance_paused() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsBalanceStatus {
                mount_point: "/mnt/storage".into(),
            },
            RawCommandOutput {
                cmd: "btrfs balance status /mnt/storage".into(),
                stdout: "Balance on '/mnt/storage' is paused\n\
                         5 out of about 12 chunks balanced (8 considered), 58% left\n"
                    .into(),
                stderr: String::new(),
                exit_status: 1,
            },
        );
        let err = check_no_exclusive_op(&runner, "/mnt/storage").unwrap_err();
        assert!(err.contains("paused"), "expected 'paused' in: {err}");
    }

    #[test]
    // Intent: check_no_exclusive_op refuses when the probe itself fails.
    // Why: Fail-closed — if we can't determine state, refusing is safer than
    //   proceeding and potentially unmounting during an active balance.
    // Scenario: btrfs balance status command fails due to permissions or kernel bug.
    fn exclusive_op_refuses_on_probe_failure() {
        let runner = MockRunner::default(); // no mock seeded → MissingMock
        assert!(check_no_exclusive_op(&runner, "/mnt/storage").is_err());
    }

    #[test]
    // Intent: check_not_read_only passes when pool is rw.
    // Why: Confirms rw mounts are not falsely rejected.
    // Scenario: Normal pool mount with default options.
    fn read_only_passes_when_rw() {
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson {
                mount_point: "/mnt/storage".into(),
            },
            RawCommandOutput {
                cmd: "findmnt --json --output TARGET,SOURCE,FSTYPE,OPTIONS --mountpoint /mnt/storage".into(),
                stdout: r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-vdb","fstype":"btrfs","options":"rw,relatime,ssd,space_cache=v2,subvolid=5,subvol=/"}]}"#.into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        assert!(check_not_read_only(&runner, "/mnt/storage").is_ok());
    }

    #[test]
    // Intent: check_not_read_only refuses when pool is ro.
    // Why: After a crash, btrfs remounts ro; writes fail with cryptic errors.
    // Scenario: Pool crashed, operator tries `braid remove` on the ro mount.
    fn read_only_refuses_when_ro() {
        let runner = MockRunner::default().with_output(
            CmdRequest::FindmntJson {
                mount_point: "/mnt/storage".into(),
            },
            RawCommandOutput {
                cmd: "findmnt --json --output TARGET,SOURCE,FSTYPE,OPTIONS --mountpoint /mnt/storage".into(),
                stdout: r#"{"filesystems":[{"target":"/mnt/storage","source":"/dev/mapper/braid-vdb","fstype":"btrfs","options":"ro,relatime,ssd,space_cache=v2,subvolid=5,subvol=/"}]}"#.into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let err = check_not_read_only(&runner, "/mnt/storage").unwrap_err();
        assert!(err.contains("read-only"), "expected 'read-only' in: {err}");
        assert!(
            err.contains("remount"),
            "expected remount guidance in: {err}"
        );
    }

    #[test]
    // Intent: check_not_read_only proceeds when findmnt probe fails.
    // Why: A bug in the safety check shouldn't block valid operations.
    // Scenario: findmnt not found or permissions issue.
    fn read_only_proceeds_on_probe_failure() {
        let runner = MockRunner::default(); // no mock → MissingMock
        assert!(check_not_read_only(&runner, "/mnt/storage").is_ok());
    }

    #[test]
    // Intent: check_no_missing_devices passes when no devices are missing.
    // Why: Confirms healthy pools are not rejected.
    // Scenario: Normal 3-disk pool, all present.
    fn missing_devices_passes_when_none() {
        assert!(check_no_missing_devices(0, "remove a disk").is_ok());
    }

    #[test]
    // Intent: check_no_missing_devices refuses when devices are missing.
    // Why: Removing a live disk from a degraded pool is dangerous.
    // Scenario: One disk has died, operator tries to remove a different live disk.
    fn missing_devices_refuses_when_degraded() {
        let err = check_no_missing_devices(2, "remove a disk").unwrap_err();
        assert!(
            err.contains("2 missing devices"),
            "expected count in: {err}"
        );
        assert!(
            err.contains("remove-missing"),
            "expected guidance in: {err}"
        );
    }

    #[test]
    // Intent: check_no_missing_devices uses singular for 1 device.
    // Why: Grammar correctness in user-facing messages.
    // Scenario: Pool has exactly 1 missing device.
    fn missing_devices_singular_grammar() {
        let err = check_no_missing_devices(1, "remove a disk").unwrap_err();
        assert!(
            err.contains("1 missing device."),
            "expected singular in: {err}"
        );
    }

    #[test]
    // Intent: probe_missing_devids returns devids of missing devices.
    // Why: Used to validate --missing-id arguments against actual missing devids.
    // Scenario: 3-disk pool with one missing device (devid 3).
    fn probe_missing_devids_returns_missing() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsDeviceUsageRaw {
                mount_point: "/mnt/storage".into(),
            },
            RawCommandOutput {
                cmd: "btrfs device usage --raw /mnt/storage".into(),
                stdout: "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

/dev/mapper/braid-disk2, ID: 2
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

<missing disk>, ID: 3
   Device size:                  0
   Device slack:                  0
   Data,RAID1:           2147483648
   Unallocated:                  0

"
                .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let missing = probe_missing_devids(&runner, "/mnt/storage").unwrap();
        assert_eq!(missing, vec![3]);
    }

    #[test]
    // Intent: probe_missing_devids returns empty when no devices are missing.
    // Why: Confirms healthy pools report no missing devids.
    // Scenario: Normal 2-disk pool, all present.
    fn probe_missing_devids_returns_empty_when_healthy() {
        let runner = MockRunner::default().with_output(
            CmdRequest::BtrfsDeviceUsageRaw {
                mount_point: "/mnt/storage".into(),
            },
            RawCommandOutput {
                cmd: "btrfs device usage --raw /mnt/storage".into(),
                stdout: "\
/dev/mapper/braid-disk1, ID: 1
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

/dev/mapper/braid-disk2, ID: 2
   Device size:           520093696
   Device slack:                  0
   Data,RAID1:            469762048
   Unallocated:            50331648

"
                .into(),
                stderr: String::new(),
                exit_status: 0,
            },
        );
        let missing = probe_missing_devids(&runner, "/mnt/storage").unwrap();
        assert!(missing.is_empty());
    }
}
