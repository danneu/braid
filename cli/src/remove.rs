use crate::cmd::{CmdRequest, CommandRunner, Step};
use crate::config::{config_read, mapper_name};
use crate::confirm;
use crate::inhibit::AcquireSleepInhibitor;
use crate::journal;
use crate::membership;
use crate::parse::{parse_btrfs_device_usage, parse_btrfs_df_json, ParseError};
use crate::pool::evict_present_device;
use crate::preflight;
use crate::probe::{probe_pool, Filesystem, ProbeError};
use crate::progress::ProgressOutput;
use crate::state_paths::StatePaths;
use crate::types::*;
use std::path::Path;

#[derive(Debug, thiserror::Error)]
pub enum RemoveError {
    #[error("{0}")]
    Validation(String),
    #[error(
        "pool was modified but membership persist failed: {0}\n\
         pool.json may be stale -- run `braid recover` to reconcile from live state."
    )]
    MembershipPersistFailure(String),
    #[error(
        "pool was modified and membership persisted, but journal clear failed: {0}\n\
         Recovery mode remains active until pending-op.json is cleared -- \
         run `braid recover`."
    )]
    JournalClearFailure(String),
    #[error("probe error: {0}")]
    Probe(#[from] ProbeError),
    #[error("pool error: {0}")]
    Pool(#[from] crate::pool::PoolError),
    #[error("config error: {0}")]
    Config(#[from] crate::config::ConfigError),
    #[error("command error: {0}")]
    Cmd(#[from] crate::cmd::CmdError),
}

/// Classify a `save_membership` failure that occurs *after* the irreversible
/// btrfs device-remove has returned. Callers pass this to `.map_err` on the
/// post-commit `save_membership` call; tests call it directly on a real
/// `MembershipError` so a classification regression inside the helper fails
/// the test.
fn map_membership_persist_failure(e: membership::MembershipError) -> RemoveError {
    RemoveError::MembershipPersistFailure(format!("failed to persist pool membership: {e}"))
}

/// Classify a `clear_journal` failure that occurs after the pool has been
/// modified and pool.json has already been rewritten. Same testing seam as
/// `map_membership_persist_failure` above.
fn map_journal_clear_failure(e: journal::JournalError) -> RemoveError {
    RemoveError::JournalClearFailure(e.to_string())
}

pub struct RemoveParams<'a> {
    pub config_path: &'a Path,
    pub name: &'a str,
    pub dry_run: bool,
    pub yes: bool,
    pub progress: ProgressOutput,
    pub paths: &'a StatePaths,
    /// Seam for acquiring a logind sleep inhibitor before the irreversible
    /// portion of the remove. Production passes `&RealSleepInhibitor`;
    /// unit tests pass `&RecordingInhibitor` to avoid spawning subprocesses.
    pub sleep_inhibitor: &'a dyn AcquireSleepInhibitor,
}

pub fn cmd_remove<R: CommandRunner + Sync, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    params: &RemoveParams<'_>,
) -> Result<(), RemoveError> {
    preflight::check_no_pending_operation(params.paths).map_err(RemoveError::Validation)?;

    let config = config_read(params.config_path)?;

    let pool = match probe_pool(runner, config.mount_point()) {
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
    let fsid = pool.fsid.as_deref().expect("mounted pool must have FSID");
    preflight::require_mutation_preflight(runner, fs, fsid, config.mount_point())
        .map_err(RemoveError::Validation)?;
    preflight::check_ups_not_on_battery(runner, config.ups().map(|u| u.name.as_str()), "remove")
        .map_err(RemoveError::Validation)?;

    let mn = mapper_name(params.name);

    // Is the disk present in the pool?
    let target = match pool.devices.iter().find(|d| d.mapper == mn) {
        Some(d) => d,
        None => {
            let mut msg = format!("disk '{}' not found in pool.", params.name);
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
    };

    preflight::check_no_missing_devices(pool.missing_count, "remove a live disk from the pool")
        .map_err(RemoveError::Validation)?;

    // compile_remove_present_steps owns the remaining == 0 rejection
    // (last-disk gate). Run it first so that `check_eviction_space` is
    // always reached with `remaining >= 1`; the capacity helper does not
    // need to handle the 0-case.
    let steps = compile_remove_present_steps(&mn, &pool, config.mount_point())?;

    let remaining = pool.devices.len() - 1;
    // Pre-flight: reject if the surviving devices lack space to absorb
    // data from the device being removed. Without this, btrfs will
    // either ENOSPC instantly or crash the filesystem to read-only
    // mid-relocation (see tests/repro/). The helper dispatches on
    // `remaining` -- the >=2-survivor path and the 1-survivor path use
    // different models and different error policies.
    check_eviction_space(runner, config.mount_point(), target, remaining)?;

    if params.dry_run {
        Step::print_dry_run(&steps);
        return Ok(());
    }

    // Confirm
    if !params.yes {
        let hw = confirm::query_disk_hw_info(runner, &target.underlying);
        let total = pool.devices.len();
        eprintln!(
            "{}",
            format_remove_confirm(
                &RemoveConfirmDisk {
                    name: params.name,
                    hw: Some(&hw),
                    devid: target.devid,
                },
                remaining,
                total,
            )
        );
        if remaining == 1 {
            eprintln!("WARNING: Pool will have 1 disk -- no RAID1 redundancy.\n");
        }
        confirm::confirm_yes().map_err(RemoveError::Validation)?;
    }

    // Hold a logind sleep inhibitor for the rest of the remove operation --
    // covers the optional pre-remove RAID1->single balance, the long-running
    // btrfs device remove (data migration), and the post-op LUKS close +
    // membership persist. Suspending mid-remove can leave the kernel-side
    // device-remove state machine in a partially-relocated state requiring
    // recovery.
    //
    // Acquired here, AFTER all interactive/reversible work (confirmation)
    // and BEFORE journal::write_journal, so that:
    //   - operator-idle prompts do not block suspend
    //   - a logind failure aborts cleanly without stranding pending-op.json
    //     and forcing the user into recovery mode for an environmental error.
    let _sleep_inhibitor_guard = params
        .sleep_inhibitor
        .acquire("removing disk from pool")
        .map_err(|e| {
            RemoveError::Validation(format!(
                "could not acquire sleep inhibitor (is logind running?): {e}"
            ))
        })?;

    // Build target membership and write journal before irreversible disk op.
    let pre_membership = membership::load_membership(params.paths)
        .map_err(|e| RemoveError::Validation(format!("failed to load pool membership: {e}")))?;
    let mut target_membership = pre_membership.clone();
    target_membership.disks.remove(params.name);
    let journal = journal::build_journal(
        pre_membership,
        target_membership.clone(),
        journal::OpKind::Remove {
            name: params.name.to_owned(),
        },
    );
    journal::write_journal(params.paths, &journal)
        .map_err(|e| RemoveError::Validation(e.to_string()))?;

    // Execute
    evict_present_device(runner, &mn.0, config.mount_point(), params.progress)?;

    // Post-commit: write pool.json and clear journal.
    membership::save_membership(&target_membership, params.paths)
        .map_err(map_membership_persist_failure)?;
    journal::clear_journal(params.paths).map_err(map_journal_clear_failure)?;

    eprintln!("Done. Disk '{}' removed from pool.", params.name);
    Ok(())
}

/// Check that the surviving device(s) have enough space to absorb data from
/// the device being removed. If they don't, `btrfs device remove` will either
/// ENOSPC instantly or crash the filesystem to read-only mid-relocation.
///
/// Two branches with **different error policies**:
///
/// - `remaining >= 2`: RAID1-aware per-type check via
///   `check_raid1_relocation_space`. Input uncertainty (spawn errors,
///   non-`CommandFailed` parse errors) is *warn-and-proceed* -- a best-effort
///   preflight miss here falls through to `btrfs device remove`, which ENOSPCs
///   cleanly without corrupting the filesystem. Only a `CommandFailed` parse
///   error (btrfs itself refused) is surfaced as a validation error.
///
/// - `remaining == 1`: single-survivor capacity check. Every input uncertainty
///   -- spawn error, parser-shape error, `CommandFailed`, or "survivor entry
///   missing from `btrfs device usage`" -- is a hard `RemoveError::Validation`.
///   The post-balance + post-remove state for a lone survivor has no safety
///   net: a missed capacity refusal lets `btrfs device remove` crash the fs
///   read-only mid-migration with `pending-op.json` already on disk. Any
///   uncertainty is fail-closed here by design. Do **not** unify the two
///   error policies -- the asymmetry is the point.
///
/// `remaining == 0` is not a valid input; `compile_remove_present_steps` has
/// already rejected the last-disk case upstream.
fn check_eviction_space<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    target: &PoolDevice,
    remaining: usize,
) -> Result<(), RemoveError> {
    if remaining == 1 {
        return check_single_survivor(runner, mount_point, target);
    }

    // remaining >= 2: existing warn-and-proceed policy.
    let raw = match runner.run(&CmdRequest::BtrfsDeviceUsageRaw {
        mount_point: mount_point.clone(),
    }) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("warning: ENOSPC pre-flight check failed: {e}; proceeding anyway");
            return Ok(());
        }
    };

    let usage = match parse_btrfs_device_usage(&raw) {
        Ok(u) => u,
        Err(ParseError::CommandFailed {
            exit_code, stderr, ..
        }) => {
            return Err(RemoveError::Validation(format!(
                "btrfs device usage failed (exit {exit_code}): {stderr}"
            )));
        }
        Err(e) => {
            eprintln!("warning: ENOSPC pre-flight check failed: {e}; proceeding anyway");
            return Ok(());
        }
    };

    let target_devs: Vec<_> = usage
        .devices
        .iter()
        .filter(|d| d.devid == target.devid)
        .collect();
    let remaining_devs: Vec<_> = usage
        .devices
        .iter()
        .filter(|d| d.devid != target.devid)
        .collect();

    preflight::check_raid1_relocation_space(&target_devs, &remaining_devs).map_err(|e| {
        RemoveError::Validation(format!(
            "{e}\n\nFree up space by deleting files, or add a new device first with `braid add`."
        ))
    })
}

/// 2->1 branch of `check_eviction_space`. Fail-closed on every input
/// uncertainty -- see `check_eviction_space` docstring for the rationale.
fn check_single_survivor<R: CommandRunner>(
    runner: &R,
    mount_point: &MountPoint,
    target: &PoolDevice,
) -> Result<(), RemoveError> {
    let usage_raw = runner
        .run(&CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| {
            RemoveError::Validation(format!(
                "ENOSPC pre-flight (2->1): btrfs device usage spawn failed: {e}. \
                 Refusing to start remove without a validated survivor capacity."
            ))
        })?;
    let usage = parse_btrfs_device_usage(&usage_raw).map_err(|e| match e {
        ParseError::CommandFailed {
            exit_code, stderr, ..
        } => RemoveError::Validation(format!(
            "btrfs device usage failed (exit {exit_code}): {stderr}"
        )),
        other => RemoveError::Validation(format!(
            "ENOSPC pre-flight (2->1): btrfs device usage output unparseable: {other}. \
             Refusing to start remove without a validated survivor capacity."
        )),
    })?;

    let df_raw = runner
        .run(&CmdRequest::BtrfsFilesystemDfJson {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| {
            RemoveError::Validation(format!(
                "ENOSPC pre-flight (2->1): btrfs filesystem df spawn failed: {e}. \
                 Refusing to start remove without a validated survivor capacity."
            ))
        })?;
    let df = parse_btrfs_df_json(&df_raw).map_err(|e| match e {
        ParseError::CommandFailed {
            exit_code, stderr, ..
        } => RemoveError::Validation(format!(
            "btrfs filesystem df failed (exit {exit_code}): {stderr}"
        )),
        other => RemoveError::Validation(format!(
            "ENOSPC pre-flight (2->1): btrfs filesystem df output unparseable: {other}. \
             Refusing to start remove without a validated survivor capacity."
        )),
    })?;

    let survivor = usage
        .devices
        .iter()
        .find(|d| d.devid != target.devid)
        .ok_or_else(|| {
            RemoveError::Validation(format!(
                "ENOSPC pre-flight (2->1): btrfs device usage did not list the \
                 surviving device (target devid {}). Refusing to start remove \
                 without a validated survivor capacity.",
                target.devid,
            ))
        })?;

    preflight::check_single_survivor_capacity(&df, survivor).map_err(RemoveError::Validation)
}

fn compile_remove_present_steps(
    mn: &MapperName,
    pool: &PoolState,
    mount_point: &MountPoint,
) -> Result<Vec<Step>, RemoveError> {
    let remaining = pool.devices.len() - 1;
    if remaining == 0 {
        return Err(RemoveError::Validation(
            "cannot remove the last disk from the pool".into(),
        ));
    }

    let mapper_path = format!("/dev/mapper/{}", mn);
    let mut steps = Vec::new();
    if remaining == 1 {
        steps.push(Step {
            risk: "long",
            description: "btrfs balance -dconvert=single -mconvert=dup -f (RAID1 -> single)".into(),
            commands: vec![CmdRequest::BtrfsBalanceSingle {
                mount_point: mount_point.clone(),
            }],
        });
    }
    steps.push(Step {
        risk: "long",
        description: format!(
            "btrfs device remove /dev/mapper/{} (data migrates off disk)",
            mn
        ),
        commands: vec![CmdRequest::BtrfsDeviceRemove {
            device: mapper_path,
            mount_point: mount_point.clone(),
        }],
    });
    steps.push(Step {
        risk: "safe",
        description: format!("cryptsetup close {}", mn),
        commands: vec![CmdRequest::CryptsetupClose {
            mapper: mn.0.clone(),
        }],
    });
    Ok(steps)
}

// ---------------------------------------------------------------------------
// Confirmation formatter
// ---------------------------------------------------------------------------

struct RemoveConfirmDisk<'a> {
    name: &'a str,
    hw: Option<&'a confirm::DiskHwInfo>,
    devid: u64,
}

fn format_remove_confirm(disk: &RemoveConfirmDisk, remaining: usize, total: usize) -> String {
    let mut msg = "Remove from pool:\n".to_string();
    let hw_line = disk.hw.and_then(confirm::format_hw_info_line);
    if let Some(hw) = &hw_line {
        msg.push_str(&format!("  {}  {}\n", disk.name, hw));
    } else {
        msg.push_str(&format!("  {}\n", disk.name));
    }
    let migrate_word = if remaining == 1 { "disk" } else { "disks" };
    msg.push_str(&format!(
        "  {:width$}devid {} | data will migrate to remaining {}\n",
        "",
        disk.devid,
        migrate_word,
        width = disk.name.len() + 2,
    ));
    msg.push_str(&format!(
        "\nPool: {} {} -> {} {}\n",
        total,
        if total == 1 { "disk" } else { "disks" },
        remaining,
        if remaining == 1 { "disk" } else { "disks" },
    ));
    msg
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{CmdError, CmdRequest, CommandRunner, RawCommandOutput};
    use crate::membership::{self, PoolMembership};
    use crate::probe::Filesystem;
    use crate::progress::ProgressOutput;
    use crate::state_paths::StatePaths;
    use crate::types::ByIdPath;
    use std::collections::BTreeMap;
    use std::sync::{Arc, Mutex};

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

    struct MockFsWithExclop(String);

    impl Filesystem for MockFsWithExclop {
        fn exists(&self, _path: &str) -> bool {
            false
        }
        fn is_block_device(&self, _path: &str) -> bool {
            false
        }
        fn read_to_string(&self, path: &str) -> Result<String, std::io::Error> {
            if path.ends_with("/exclusive_operation") {
                Ok(format!("{}\n", self.0))
            } else {
                Err(std::io::Error::new(std::io::ErrorKind::NotFound, "mock"))
            }
        }
        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }
    }

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
        fail_device_remove: bool,
    }

    impl RecordingRunner {
        fn new(log: Arc<Mutex<Vec<CmdRequest>>>) -> Self {
            Self {
                log,
                fail_device_remove: false,
            }
        }

        fn with_device_remove_failure(log: Arc<Mutex<Vec<CmdRequest>>>) -> Self {
            Self {
                log,
                fail_device_remove: true,
            }
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
                CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(mock_out(
                    "btrfs device usage --raw /mnt/storage",
                    // 2-disk RAID1, each device 1 GiB physical, small
                    // allocations. Used by the 2->1 preflight to resolve
                    // the survivor entry (device_size - device_slack is
                    // the usable capacity).
                    "/dev/mapper/braid-disk1, ID: 1\n\
                     \x20  Device size:         1073741824\n\
                     \x20  Device slack:                 0\n\
                     \x20  Data,RAID1:            52428800\n\
                     \x20  Metadata,RAID1:        10485760\n\
                     \x20  System,RAID1:             32768\n\
                     \x20  Unallocated:         1010794496\n\n\
                     /dev/mapper/braid-disk2, ID: 2\n\
                     \x20  Device size:         1073741824\n\
                     \x20  Device slack:                 0\n\
                     \x20  Data,RAID1:            52428800\n\
                     \x20  Metadata,RAID1:        10485760\n\
                     \x20  System,RAID1:             32768\n\
                     \x20  Unallocated:         1010794496\n",
                    0,
                )),
                CmdRequest::BtrfsFilesystemDfJson { .. } => Ok(mock_out(
                    "btrfs --format json filesystem df /mnt/storage",
                    // Logical usage: Data=50 MiB, Metadata=10 MiB,
                    // System=32 KiB. needed_post_single = 50 + 20 +
                    // 0.06 = ~70 MiB, well under 1 GiB usable.
                    r#"{
  "filesystem-df": [
    { "bg-type": "Data", "bg-profile": "RAID1", "total": 52428800, "used": 52428800 },
    { "bg-type": "Metadata", "bg-profile": "RAID1", "total": 10485760, "used": 10485760 },
    { "bg-type": "System", "bg-profile": "RAID1", "total": 32768, "used": 32768 }
  ]
}"#,
                    0,
                )),
                CmdRequest::BtrfsBalanceSingle { .. } => Ok(mock_out("btrfs balance start", "", 0)),
                CmdRequest::BtrfsDeviceRemove { .. } => {
                    if self.fail_device_remove {
                        Ok(RawCommandOutput {
                            cmd: "btrfs device remove".into(),
                            stdout: String::new(),
                            stderr: "ERROR: error removing device".into(),
                            exit_status: 1,
                        })
                    } else {
                        Ok(mock_out("btrfs device remove", "", 0))
                    }
                }
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
    // Intent: cmd_remove invokes the 2->1 survivor-capacity preflight before
    //   committing any mutation, and proceeds when the survivor has room.
    //
    // Why: A 2-disk RAID1 with a smaller survivor can fit the data in RAID1
    //   (min of the two) yet fail post-balance once metadata is doubled to
    //   DUP on one device. The fix calls check_single_survivor_capacity on
    //   every 2->1 remove so btrfs device remove cannot crash the fs to RO
    //   mid-migration. This test locks in both preflight calls
    //   (BtrfsDeviceUsageRaw + BtrfsFilesystemDfJson) run BEFORE the
    //   balance/device-remove steps, so a regression that reintroduces the
    //   old remaining == 1 skip fails here.
    //
    // Scenario: User removes one disk from a healthy 2-disk pool whose live
    //   data (50 MiB data + 10 MiB metadata) fits comfortably on the survivor.
    //   Preflight runs, reports pass, and the operation proceeds to balance
    //   + device remove. Pre-fix, the preflight calls would be absent.
    fn two_to_one_remove_invokes_survivor_capacity_preflight() {
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
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        cmd_remove(
            &runner,
            &MockFs,
            &RemoveParams {
                config_path: Path::new(&config_path),
                name: "disk2",
                dry_run: false,
                yes: true,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        )
        .expect("remove should succeed");

        let calls = log.lock().unwrap();
        let usage_idx = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsDeviceUsageRaw { .. }))
            .expect("2->1 preflight must call btrfs device usage; calls: {calls:?}");
        let df_idx = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsFilesystemDfJson { .. }))
            .expect("2->1 preflight must call btrfs filesystem df; calls: {calls:?}");
        let balance_idx = calls
            .iter()
            .position(|c| matches!(c, CmdRequest::BtrfsBalanceSingle { .. }))
            .expect("2->1 remove must balance; calls: {calls:?}");
        assert!(
            usage_idx < balance_idx && df_idx < balance_idx,
            "preflight calls must precede the RAID1->single balance; calls: {calls:?}"
        );
        // Locks in the seam placement: a successful 2->1 remove must take the
        // inhibitor exactly once before journal::write_journal.
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
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
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        cmd_remove(
            &runner,
            &MockFs,
            &RemoveParams {
                config_path: Path::new(&config_path),
                name: "disk2",
                dry_run: false,
                yes: true,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
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

    #[test]
    // Intent: pending-op.json survives when eviction fails after journal write.
    //
    // Why it exists: JournalGuard previously cleared the journal on any exit,
    //   including error returns. This left pool.json potentially stale with no
    //   recovery path after a failed btrfs device remove.
    //
    // Scenario: 2-disk pool, btrfs device remove fails mid-eviction. The journal
    //   must persist so `braid recover` can reconcile pool.json from live state.
    fn journal_survives_evict_failure() {
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
        let runner = RecordingRunner::with_device_remove_failure(log);
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let result = cmd_remove(
            &runner,
            &MockFs,
            &RemoveParams {
                config_path: Path::new(&config_path),
                name: "disk2",
                dry_run: false,
                yes: true,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );

        assert!(result.is_err(), "remove should fail when eviction fails");
        assert!(
            journal::load_journal(&paths).unwrap().is_some(),
            "pending-op.json must survive error exit so braid recover can reconcile"
        );
        // The journal exists, which proves we got past journal::write_journal,
        // which proves the inhibitor was acquired exactly once on the way in.
        // Locks in the seam placement: if a refactor moves the acquire to a
        // post-journal point or skips it entirely, this assert flips.
        assert_eq!(
            inhibitor.acquire_count(),
            1,
            "sleep inhibitor must be acquired exactly once on the path through journal::write_journal"
        );
    }

    #[test]
    // Intent: dry-run output shows exact commands for a 3->2 removal.
    // Why: verifies the Step/CmdRequest integration produces correct shell strings.
    // Scenario: 3-disk pool, removing one disk (remaining=2, no balance to single).
    fn dry_run_render_3disk_removal() {
        let mn = MapperName("braid-disk2".into());
        let pool = PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    devid: 1,
                    mapper: MapperName("braid-disk1".into()),
                    luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
                    underlying: "/dev/vda".into(),
                },
                PoolDevice {
                    devid: 2,
                    mapper: MapperName("braid-disk2".into()),
                    luks_uuid: LuksUuid("22222222-2222-2222-2222-222222222222".into()),
                    underlying: "/dev/vdb".into(),
                },
                PoolDevice {
                    devid: 3,
                    mapper: MapperName("braid-disk3".into()),
                    luks_uuid: LuksUuid("33333333-3333-3333-3333-333333333333".into()),
                    underlying: "/dev/vdc".into(),
                },
            ],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 3,
            fsid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
            null_underlying: vec![],
        };
        let mount_point = MountPoint("/mnt/storage".into());
        let steps = compile_remove_present_steps(&mn, &pool, &mount_point).unwrap();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // 2 steps (device remove + close), each with 1 command line = 4 lines
        assert_eq!(lines.len(), 4, "expected 4 lines, got:\n{output}");
        assert!(lines[0].contains("[long       ]"));
        assert!(lines[0].contains("btrfs device remove"));
        assert_eq!(
            lines[1],
            "               $ btrfs device remove --enqueue /dev/mapper/braid-disk2 /mnt/storage"
        );
        assert!(lines[2].contains("[safe       ]"));
        assert!(lines[2].contains("cryptsetup close"));
        assert_eq!(lines[3], "               $ cryptsetup close braid-disk2");
    }

    #[test]
    // Intent: dry-run output includes balance-to-single when 2->1 removal.
    // Why: verifies the conditional balance step renders with its command.
    // Scenario: 2-disk pool, removing one disk leaves no redundancy.
    fn dry_run_render_2disk_removal_includes_balance() {
        let mn = MapperName("braid-disk2".into());
        let pool = PoolState {
            mounted: true,
            devices: vec![
                PoolDevice {
                    devid: 1,
                    mapper: MapperName("braid-disk1".into()),
                    luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
                    underlying: "/dev/vda".into(),
                },
                PoolDevice {
                    devid: 2,
                    mapper: MapperName("braid-disk2".into()),
                    luks_uuid: LuksUuid("22222222-2222-2222-2222-222222222222".into()),
                    underlying: "/dev/vdb".into(),
                },
            ],
            missing_count: 0,
            missing_devids: vec![],
            total_devices: 2,
            fsid: Some("aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee".into()),
            null_underlying: vec![],
        };
        let mount_point = MountPoint("/mnt/storage".into());
        let steps = compile_remove_present_steps(&mn, &pool, &mount_point).unwrap();
        let output = Step::render_dry_run(&steps);
        let lines: Vec<&str> = output.lines().collect();

        // 3 steps (balance + device remove + close), each with 1 command = 6 lines
        assert_eq!(lines.len(), 6, "expected 6 lines, got:\n{output}");
        assert!(lines[0].contains("RAID1 -> single"));
        assert_eq!(
            lines[1],
            "               $ btrfs balance start --enqueue '-dconvert=single' '-mconvert=dup' -f /mnt/storage"
        );
    }

    #[test]
    // Intent: `braid remove` fails fast when a balance is paused.
    // Why: a paused balance holds the exclusive lock and never clears on its own.
    //   --enqueue would hang forever waiting for it.
    // Scenario: operator paused a balance and forgot, then runs `braid remove`.
    fn remove_fails_fast_on_paused_balance() {
        let (_state_dir, paths) = setup_membership(&[("disk1", "/dev/disk/by-id/virtio-disk1")]);
        let tmp = tempfile::tempdir().unwrap();
        let config_path = tmp.path().join("config.json");
        let mut disks = BTreeMap::new();
        disks.insert(
            "disk1".to_owned(),
            serde_json::json!({ "by_id": "/dev/disk/by-id/virtio-disk1" }),
        );
        let config_json = serde_json::json!({
            "disks": disks,
            "mount_point": "/mnt/storage"
        });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = RecordingRunner::new(log.clone());
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        let err = cmd_remove(
            &runner,
            &MockFsWithExclop("balance paused".into()),
            &RemoveParams {
                config_path: Path::new(&config_path),
                name: "disk1",
                dry_run: false,
                yes: true,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        )
        .expect_err("should fail -- balance is paused");
        let msg = err.to_string();
        assert!(msg.contains("paused"), "expected 'paused' in error: {msg}");
        // Preflight failure must NOT acquire the inhibitor -- the failure is
        // reversible and the user should not be stranded in a state where
        // logind unavailability and a paused balance both have to clear before
        // the same braid command can run.
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "preflight failure (paused balance) must NOT acquire the sleep inhibitor"
        );
    }

    #[test]
    // Intent: `braid remove` warns but proceeds when an active op is running.
    // Why: --enqueue on the btrfs command will block until the slot frees;
    //   braid prints a wait message so the user knows what's happening.
    // Scenario: a device remove is already in progress, operator runs `braid remove`.
    //   The preflight detects the active op, prints a warning, and proceeds.
    fn remove_warns_and_proceeds_on_active_op() {
        let (_state_dir, paths) = setup_membership(&[
            ("disk1", "/dev/disk/by-id/virtio-disk1"),
            ("disk2", "/dev/disk/by-id/virtio-disk2"),
            ("disk3", "/dev/disk/by-id/virtio-disk3"),
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
        disks.insert(
            "disk3".to_owned(),
            serde_json::json!({ "by_id": "/dev/disk/by-id/virtio-disk3" }),
        );
        let config_json = serde_json::json!({
            "disks": disks,
            "mount_point": "/mnt/storage"
        });
        std::fs::write(&config_path, serde_json::to_vec(&config_json).unwrap()).unwrap();

        let log = Arc::new(Mutex::new(Vec::new()));
        let runner = RecordingRunner::new(log.clone());
        let inhibitor = crate::inhibit::RecordingInhibitor::new();
        // With an active balance, cmd_remove should NOT error on the preflight --
        // it prints a warning and proceeds. The command itself will eventually
        // fail because our mock doesn't seed all the downstream commands,
        // but the important thing is it does NOT return a Validation error
        // about the exclusive op.
        let result = cmd_remove(
            &runner,
            &MockFsWithExclop("balance".into()),
            &RemoveParams {
                config_path: Path::new(&config_path),
                name: "disk2",
                dry_run: true,
                yes: true,
                progress: ProgressOutput::Off,
                paths: &paths,
                sleep_inhibitor: &inhibitor,
            },
        );
        // dry_run should succeed (no actual btrfs commands executed)
        assert!(
            result.is_ok(),
            "expected dry_run to proceed past active-op preflight, got: {result:?}"
        );
        // dry-run must NOT acquire the inhibitor -- it has no irreversible work
        // to protect.
        assert_eq!(
            inhibitor.acquire_count(),
            0,
            "dry-run must NOT acquire the sleep inhibitor"
        );
    }

    // --- Confirmation formatter tests ---

    #[test]
    fn remove_confirm_normal() {
        let hw = confirm::DiskHwInfo {
            model: Some("Toshiba MN07ACA12T".into()),
            serial: Some("1234ABCD".into()),
            size: Some(12_000_138_625_024),
        };
        let msg = format_remove_confirm(
            &RemoveConfirmDisk {
                name: "toshiba",
                hw: Some(&hw),
                devid: 2,
            },
            2,
            3,
        );
        assert!(msg.contains("Remove from pool:"));
        assert!(msg.contains("toshiba"));
        assert!(msg.contains("Toshiba MN07ACA12T"));
        assert!(msg.contains("serial 1234ABCD"));
        assert!(msg.contains("devid 2"));
        assert!(msg.contains("remaining disks"));
        assert!(msg.contains("3 disks -> 2 disks"));
    }

    #[test]
    fn remove_confirm_degraded() {
        let hw = confirm::DiskHwInfo {
            model: Some("Toshiba MN07ACA12T".into()),
            serial: None,
            size: Some(12_000_138_625_024),
        };
        let msg = format_remove_confirm(
            &RemoveConfirmDisk {
                name: "toshiba",
                hw: Some(&hw),
                devid: 2,
            },
            1,
            2,
        );
        assert!(
            msg.contains("remaining disk"),
            "singular 'disk' when 1 remaining"
        );
        assert!(msg.contains("2 disks -> 1 disk"));
    }

    #[test]
    fn remove_confirm_no_hw_info() {
        let msg = format_remove_confirm(
            &RemoveConfirmDisk {
                name: "toshiba",
                hw: None,
                devid: 2,
            },
            2,
            3,
        );
        assert!(msg.contains("toshiba"));
        assert!(msg.contains("devid 2"));
        assert!(!msg.contains("| |"), "no double separators when hw missing");
    }

    #[test]
    // Intent: the real post-commit mapping function classifies a
    //   save_membership failure as MembershipPersistFailure with remediation
    //   text that names pool.json as the stale artifact.
    //
    // Why: previously wrapped as RemoveError::Validation, which reads like a
    //   pre-flight rejection. A regression inside map_membership_persist_failure
    //   that returns the wrong variant or wrong remediation text fails this
    //   test -- the production callsite at remove.rs:208 passes this same
    //   helper to .map_err, so the test binds to the real mapping.
    //
    // Scenario: `braid remove` succeeds at the btrfs layer, but the atomic
    //   write of pool.json fails (disk full in /var/lib/braid, stale NFS
    //   mount, etc.). Forced here by writing to a path whose parent
    //   directory does not exist.
    fn save_membership_failure_classified_as_membership_persist() {
        let tmp = tempfile::tempdir().unwrap();
        // Force the atomic write to fail: place a regular file where
        // `save_membership_to` expects a directory component. `create_dir_all`
        // in atomic_write will then error with NotADirectory.
        let blocker = tmp.path().join("not-a-dir");
        std::fs::write(&blocker, b"").unwrap();
        let bad_path = blocker.join("pool.json");
        let m = PoolMembership::empty();
        let underlying = membership::save_membership_to(&m, &bad_path)
            .expect_err("write under a non-directory path component must fail");
        let classified = map_membership_persist_failure(underlying);
        assert!(
            matches!(classified, RemoveError::MembershipPersistFailure(_)),
            "variant mismatch: {classified:?}"
        );
        let display = classified.to_string();
        assert!(display.contains("pool was modified"), "got: {display}");
        assert!(display.contains("pool.json may be stale"), "got: {display}");
        assert!(display.contains("braid recover"), "got: {display}");
    }

    #[test]
    // Intent: the real post-commit mapping function classifies a
    //   clear_journal failure as JournalClearFailure with remediation text
    //   that names recovery mode / pending-op.json as the latched artifact.
    //
    // Why: this is the only post-commit mode where pool.json is already
    //   correct and the *journal* is keeping the system in recovery mode. A
    //   regression that reused the membership message would tell the user to
    //   reconcile pool.json when pool.json is fine.
    //
    // Scenario: `braid remove` succeeds, pool.json is rewritten, but
    //   clear_journal fails (forced here by making pending-op.json a
    //   non-empty directory so fs::remove_file errors).
    fn clear_journal_failure_classified_as_journal_clear() {
        use crate::journal;
        let tmp = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        let pending = paths.pending_op_json();
        std::fs::create_dir_all(&pending).unwrap();
        std::fs::write(pending.join("child"), b"x").unwrap();
        let underlying = journal::clear_journal(&paths)
            .expect_err("remove_file on a non-empty directory must fail");
        let classified = map_journal_clear_failure(underlying);
        assert!(
            matches!(classified, RemoveError::JournalClearFailure(_)),
            "variant mismatch: {classified:?}"
        );
        let display = classified.to_string();
        assert!(
            display.contains("pool was modified and membership persisted"),
            "got: {display}"
        );
        assert!(display.contains("journal clear failed"), "got: {display}");
        assert!(
            display.contains("Recovery mode remains active"),
            "got: {display}"
        );
        assert!(display.contains("pending-op.json"), "got: {display}");
        assert!(display.contains("braid recover"), "got: {display}");
    }

    #[test]
    // Intent: check_eviction_space surfaces a non-zero btrfs exit as a hard
    //   validation error instead of swallowing it into warn-and-proceed.
    // Why: btrfs exiting non-zero during pre-flight is a real "cannot read the
    //   filesystem" signal. If the preflight tool itself has failed, a 3->2
    //   remove must not proceed into the irreversible btrfs device-remove
    //   step.
    // Scenario: 3->2 remove on a filesystem that returns EIO (or similar) to
    //   `btrfs device usage --raw`. Before this fix the warning was printed
    //   and remove proceeded; after the fix, remove stops at validation.
    fn check_eviction_space_surfaces_command_failed_as_validation() {
        struct FailingUsageRunner;

        impl CommandRunner for FailingUsageRunner {
            fn run(&self, request: &CmdRequest) -> Result<RawCommandOutput, CmdError> {
                match request {
                    CmdRequest::BtrfsDeviceUsageRaw { .. } => Ok(RawCommandOutput {
                        cmd: "btrfs device usage --raw /mnt/storage".into(),
                        stdout: String::new(),
                        stderr: "ERROR: not a btrfs filesystem: /mnt/storage".into(),
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

        let mount = MountPoint("/mnt/storage".to_owned());
        let target = PoolDevice {
            devid: 1,
            mapper: MapperName("braid-disk1".into()),
            luks_uuid: LuksUuid("11111111-1111-1111-1111-111111111111".into()),
            underlying: "/dev/vda".into(),
        };
        // remaining: 2 exercises the >= 2 branch (3->2 remove), which is the
        // scenario the CommandFailed surfacing was written for.
        let err = check_eviction_space(&FailingUsageRunner, &mount, &target, 2)
            .expect_err("non-zero btrfs exit must surface as validation error");
        match err {
            RemoveError::Validation(msg) => {
                assert!(msg.contains("btrfs device usage failed"), "got: {msg}");
                assert!(msg.contains("exit 1"), "got: {msg}");
                assert!(msg.contains("not a btrfs filesystem"), "got: {msg}");
            }
            other => panic!("expected RemoveError::Validation, got {other:?}"),
        }
    }
}
