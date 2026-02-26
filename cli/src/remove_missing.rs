use crate::cmd::{CmdRequest, CommandRunner};
use crate::config::config_read_raw;
use crate::disk_map;
use crate::parse::parse_btrfs_device_usage;
use crate::pool::{pool_remove_devid, pool_remove_missing};
use crate::probe::{probe_pool, ProbeError};
use crate::status::format_bytes;
use crate::types::*;
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
    let (config, _config_raw) = config_read_raw(config_path)?;
    let disk_map_state = disk_map::load_disk_map();
    disk_map::validate_config_key_stability(&config, &disk_map_state)
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

    // Pre-flight: reject if survivors lack space to absorb the missing device's data.
    // Without this check, btrfs will either fail with ENOSPC (harmless) or — worse —
    // start relocating, hit ENOSPC mid-transaction, and force the filesystem read-only.
    check_relocation_space(runner, config.mount_point(), missing_id)?;

    let steps = compile_steps(missing_id, &pool);

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

/// Check that surviving devices have enough unallocated space to absorb the
/// missing device's allocations. If they don't, btrfs device remove will
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

    // Partition devices: missing (device_size == 0) vs surviving (device_size > 0)
    let mut total_allocated_missing: u64 = 0;
    let mut total_unallocated_survivors: u64 = 0;

    for dev in &usage.devices {
        if dev.device_size == 0 {
            // Missing device — count its allocations (optionally filtered by devid)
            if missing_id.is_none() || missing_id == Some(dev.devid) {
                total_allocated_missing += dev.used_bytes();
            }
        } else {
            // Surviving device
            total_unallocated_survivors += dev.unallocated;
        }
    }

    if total_unallocated_survivors < total_allocated_missing {
        return Err(RemoveMissingError::Validation(format!(
            "not enough free space to remove the missing device.\n\n  \
             Missing device has {} allocated (must be relocated to survivors).\n  \
             Surviving devices have {} total unallocated.\n\n\
             Without enough space, btrfs will hang and then crash the filesystem to read-only.\n\
             Free up space by deleting files, or add a new device first with `braid add`.",
            format_bytes(total_allocated_missing),
            format_bytes(total_unallocated_survivors),
        )));
    }

    Ok(())
}

fn compile_steps(missing_id: Option<u64>, _pool: &PoolState) -> Vec<RemoveMissingStep> {
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
            msg.contains("not enough free space"),
            "expected 'not enough free space' in: {msg}"
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
        let fixture = "\
/dev/mapper/braid-disk1, ID: 1
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

        // Targeting devid 2 (50 MB allocated) — should pass (200 MB available)
        let result = check_relocation_space(&runner, "/mnt/storage", Some(2));
        assert!(result.is_ok(), "targeting devid 2 should pass: {result:?}");

        // Targeting devid 3 (5 GB allocated) — should fail
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
