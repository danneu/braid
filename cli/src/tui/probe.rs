use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::cmd::{CmdRequest, CommandRunner};
use crate::luks;
use crate::parse::types::{ScrubState, SmartHealth};
use crate::parse::{
    parse_btrfs_device_stats, parse_btrfs_device_usage, parse_btrfs_filesystem_usage,
    parse_btrfs_scrub_status, parse_cryptsetup_luks_dump, parse_lsblk_json, parse_smartctl_health,
};
use crate::probe::{probe_config_disk, probe_pool, Filesystem};
use crate::state_paths::StatePaths;
use crate::status::resolve_alert_state;
use crate::status::{estimate_pool_capacity, get_balance_report, DiskErrors};
use crate::tui::model::{DiskLuksInfo, DiskUsage, PoolState, UnpooledDiskRender};
use crate::types::{ByIdPath, ConfigDiskState, LuksUuid, MountPoint};

pub fn probe_pool_for_tui<R: CommandRunner, F: Filesystem + ?Sized>(
    runner: &R,
    fs: &F,
    mount_point: &MountPoint,
    disk_by_id: &HashMap<String, String>,
    paths: &StatePaths,
) -> Result<Option<PoolState>, String> {
    let domain = probe_pool(runner, mount_point).map_err(|e| e.to_string())?;

    if !domain.mounted {
        return Ok(None);
    }

    let df_raw = runner
        .run(&CmdRequest::BtrfsFilesystemDfJson {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| e.to_string())?;
    let df = crate::parse::parse_btrfs_df_json(&df_raw).map_err(|e| e.to_string())?;

    let dev_usage_raw = runner
        .run(&CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| e.to_string())?;
    let dev_usage = parse_btrfs_device_usage(&dev_usage_raw).map_err(|e| e.to_string())?;

    // Map devid → disk name via probe_pool's devices (from btrfs filesystem show,
    // which reports stable /dev/mapper/braid-* paths). btrfs device usage may
    // report raw /dev/dm-N paths that don't match config disk names.
    let devid_to_name: HashMap<u64, &str> = domain
        .devices
        .iter()
        .filter_map(|d| crate::config::name_from_mapper(&d.mapper.0).map(|name| (d.devid, name)))
        .collect();

    let mut disk_usage = HashMap::new();
    for entry in &dev_usage.devices {
        let disk_name = match devid_to_name.get(&entry.devid) {
            Some(name) => *name,
            None => continue,
        };
        disk_usage.insert(
            disk_name.to_owned(),
            DiskUsage {
                size: entry.device_size,
                allocations: entry.allocations.clone(),
                unallocated: entry.unallocated,
            },
        );
    }

    let scrub = runner
        .run(&CmdRequest::BtrfsScrubStatus {
            mount_point: mount_point.clone(),
        })
        .ok()
        .and_then(|raw| parse_btrfs_scrub_status(&raw).ok())
        .map(|out| out.state)
        .unwrap_or(ScrubState::Unknown);

    let balance = get_balance_report(runner, mount_point);

    let mut smart_health = HashMap::new();
    let mut luks_info = HashMap::new();
    for (disk_name, by_id_path) in disk_by_id {
        let health = runner
            .run(&CmdRequest::SmartctlHealthJson {
                device: by_id_path.clone(),
            })
            .map(|raw| parse_smartctl_health(&raw))
            .unwrap_or(SmartHealth::Unknown);
        smart_health.insert(disk_name.clone(), health);

        if let Ok(raw) = runner.run(&CmdRequest::CryptsetupLuksDump {
            device: by_id_path.clone(),
        })
            && let Ok(dump) = parse_cryptsetup_luks_dump(&raw) {
                luks_info.insert(
                    disk_name.clone(),
                    DiskLuksInfo {
                        cipher: dump.cipher,
                        key_size_bits: dump.key_size_bits,
                        keyslot_count: dump.keyslot_count,
                    },
                );
            }
    }

    // Extract transport type (sata, nvme, usb, etc.) from lsblk tree.
    // Walk parent devices: for each child named "braid-{name}", take the
    // parent's TRAN value. TRAN is only set on physical devices, not dm-crypt.
    let mut disk_transport = HashMap::new();
    if let Ok(lsblk_raw) = runner.run(&CmdRequest::LsblkJson)
        && let Ok(lsblk) = parse_lsblk_json(&lsblk_raw) {
            for dev in &lsblk.blockdevices {
                if let Some(tran) = &dev.tran {
                    for child in &dev.children {
                        if let Some(name) = crate::config::name_from_mapper(&child.name) {
                            disk_transport.insert(name.to_owned(), tran.clone());
                        }
                    }
                }
            }
        }

    let fs_usage_raw = runner
        .run(&CmdRequest::BtrfsFilesystemUsageRaw {
            mount_point: mount_point.clone(),
        })
        .map_err(|e| e.to_string())?;
    let fs_usage = parse_btrfs_filesystem_usage(&fs_usage_raw).map_err(|e| e.to_string())?;

    // Device error stats
    let mut device_errors = HashMap::new();
    let device_stats_raw = runner
        .run(&CmdRequest::BtrfsDeviceStatsJson {
            mount_point: mount_point.clone(),
        })
        .ok();
    let device_stats = device_stats_raw
        .as_ref()
        .and_then(|raw| parse_btrfs_device_stats(raw).ok());
    if let Some(ref stats) = device_stats {
        for dev in &stats.devices {
            if let Some(name) = dev
                .target
                .as_path()
                .and_then(|p| p.strip_prefix("/dev/mapper/braid-"))
            {
                device_errors.insert(
                    name.to_owned(),
                    DiskErrors {
                        read: dev.read_io_errs,
                        write: dev.write_io_errs,
                        flush: dev.flush_io_errs,
                        corruption: dev.corruption_errs,
                        generation: dev.generation_errs,
                    },
                );
            }
        }
    }

    let alert_state = resolve_alert_state(paths);

    let capacity_total_bytes = if domain.missing_count == 0 {
        let sizes: Vec<u64> = dev_usage.devices.iter().map(|d| d.device_size).collect();
        Some(estimate_pool_capacity(&sizes))
    } else {
        None
    };

    // Classify any declared disk that is NOT in the live pool's
    // disk_usage so the disk table can render Unreadable / Damaged /
    // UnknownLuks / Missing distinctly. The live-pool UUID set is built
    // from `domain.devices` (the authoritative live source); luks_info
    // here only carries cipher/keyslot metadata, not UUIDs, so it cannot
    // answer the "valid LUKS but not in this pool" question.
    let live_pool_uuids: HashSet<LuksUuid> =
        domain.devices.iter().map(|d| d.luks_uuid.clone()).collect();
    let mut unpooled_disks: HashMap<String, UnpooledDiskRender> = HashMap::new();
    for (disk_name, by_id_path) in disk_by_id {
        if disk_usage.contains_key(disk_name) {
            continue;
        }
        let by_id = ByIdPath(by_id_path.clone());
        let probed = match probe_config_disk(runner, fs, disk_name, &by_id) {
            Ok(p) => p,
            Err(_) => continue, // degrade gracefully — skip this disk
        };
        let render = match probed.state {
            ConfigDiskState::Absent => UnpooledDiskRender::Missing,
            ConfigDiskState::PresentLuks { uuid, .. } => {
                if live_pool_uuids.contains(&uuid) {
                    // The disk is part of the live pool by UUID but is
                    // somehow absent from disk_usage — treat as Missing
                    // defensively rather than lying about state.
                    UnpooledDiskRender::Missing
                } else {
                    UnpooledDiskRender::UnknownLuks
                }
            }
            ConfigDiskState::PresentNotLuks => {
                // Refine PresentNotLuks (luksUuid failed) into Unreadable
                // vs Damaged for diagnostic rendering only — do NOT
                // propagate the refinement back into ConfigDiskState.
                // Mutating commands (add/replace) keep the coarse state.
                match luks::probe_luks_header(runner, by_id_path) {
                    luks::LuksHeaderState::Damaged => UnpooledDiskRender::LuksHeaderDamaged,
                    // Unreadable, the inconsistent Ok-but-luksUuid-failed
                    // case, and ProbeFailed all collapse to Unreadable
                    // (consistent with mount.rs::plan_open_pool).
                    _ => UnpooledDiskRender::LuksHeaderUnreadable,
                }
            }
        };
        unpooled_disks.insert(disk_name.clone(), render);
    }

    Ok(Some(PoolState {
        mount_point: mount_point.clone(),
        df_entries: df.entries,
        disk_usage,
        disk_transport,
        smart_health,
        luks_info,
        device_errors,
        unpooled_disks,
        alert_state,
        scrub,
        balance,
        capacity_total_bytes,
        capacity_used_bytes: fs_usage.used_bytes,
        probed_at: Instant::now(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cmd::{MockRunner, RawCommandOutput};
    use crate::parse::types::DeviceAllocation;

    fn test_paths() -> (tempfile::TempDir, StatePaths) {
        let tmp = tempfile::TempDir::new().unwrap();
        let paths = StatePaths::custom(tmp.path().into());
        (tmp, paths)
    }

    /// Filesystem stub for TUI probe tests. The unpooled-disk loop calls
    /// `probe_config_disk` which checks `fs.exists(by_id_path)` first; an
    /// empty default returns false (treated as Absent).
    struct StubFs {
        present_paths: Vec<String>,
    }

    impl StubFs {
        fn empty() -> Self {
            Self {
                present_paths: vec![],
            }
        }

        fn with_paths(paths: &[&str]) -> Self {
            Self {
                present_paths: paths.iter().map(|s| (*s).to_owned()).collect(),
            }
        }
    }

    impl Filesystem for StubFs {
        fn exists(&self, path: &str) -> bool {
            self.present_paths.iter().any(|p| p == path)
        }

        fn is_block_device(&self, _path: &str) -> bool {
            false
        }

        fn list_dir(&self, _path: &str) -> Result<Vec<String>, std::io::Error> {
            Ok(vec![])
        }

        fn read_to_string(&self, _path: &str) -> Result<String, std::io::Error> {
            Err(std::io::Error::new(std::io::ErrorKind::NotFound, "stub"))
        }
    }

    fn ok_raw(cmd: &str, stdout: &str) -> RawCommandOutput {
        RawCommandOutput {
            cmd: cmd.to_owned(),
            stdout: stdout.to_owned(),
            stderr: String::new(),
            exit_status: 0,
        }
    }

    /// Intent: probe_pool_for_tui passes through per-device allocations and
    /// unallocated bytes from btrfs device usage into DiskUsage, rather than
    /// collapsing them into aggregate data/metadata sums.
    ///
    /// Why: the old code discarded per-allocation detail (type + profile),
    /// making it impossible to show a breakdown in the disk detail panel.
    ///
    /// Scenario: 2-disk RAID1 pool. btrfs device usage reports Data, Metadata,
    /// and System allocations per device. The TUI probe must preserve all three
    /// allocation rows and the unallocated value for each disk.
    #[test]
    fn allocations_passed_through() {
        let mp = MountPoint("/mnt/storage".to_owned());

        let runner = MockRunner::default()
            // probe_pool: findmnt
            .with_output(
                CmdRequest::FindmntJson { mount_point: mp.clone() },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-toshiba","fstype":"btrfs"}]}"#,
                ),
            )
            // probe_pool: btrfs filesystem show
            .with_output(
                CmdRequest::BtrfsFilesystemShow { mount_point: mp.clone() },
                ok_raw(
                    "btrfs filesystem show",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 2 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n\
                     \tdevid    2 size 10.00GiB used 2.00GiB path /dev/mapper/braid-ironwolf\n",
                ),
            )
            // probe_pool: cryptsetup status for each device
            .with_output(
                CmdRequest::CryptsetupStatus { mapper: "braid-toshiba".into() },
                ok_raw(
                    "cryptsetup status",
                    "/dev/mapper/braid-toshiba is active.\n\tdevice:  /dev/vda\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/vda".into() },
                ok_raw("cryptsetup luksUUID", "11111111-1111-1111-1111-111111111111\n"),
            )
            .with_output(
                CmdRequest::CryptsetupStatus { mapper: "braid-ironwolf".into() },
                ok_raw(
                    "cryptsetup status",
                    "/dev/mapper/braid-ironwolf is active.\n\tdevice:  /dev/vdb\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid { device: "/dev/vdb".into() },
                ok_raw("cryptsetup luksUUID", "22222222-2222-2222-2222-222222222222\n"),
            )
            // btrfs filesystem df --json
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson { mount_point: mp.clone() },
                ok_raw(
                    "btrfs filesystem df",
                    r#"{"filesystem-df": [
                        {"bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216},
                        {"bg-type": "System", "bg-profile": "RAID1", "total": 4194304, "used": 16384},
                        {"bg-type": "Metadata", "bg-profile": "RAID1", "total": 33554432, "used": 65536},
                        {"bg-type": "GlobalReserve", "bg-profile": "single", "total": 3670016, "used": 0}
                    ]}"#,
                ),
            )
            // btrfs device usage --raw (the key part we're testing)
            .with_output(
                CmdRequest::BtrfsDeviceUsageRaw { mount_point: mp.clone() },
                ok_raw(
                    "btrfs device usage",
                    "/dev/dm-0, ID: 1\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:              0\n\
                     \x20  Data,RAID1:           67108864\n\
                     \x20  Metadata,DUP:         51970048\n\
                     \x20  System,DUP:            8388608\n\
                     \x20  Unallocated:          409403392\n\
                     \n\
                     /dev/dm-1, ID: 2\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:              0\n\
                     \x20  Data,RAID1:           67108864\n\
                     \x20  Metadata,DUP:         51970048\n\
                     \x20  System,DUP:            8388608\n\
                     \x20  Unallocated:          409403392\n",
                ),
            );

        let runner = runner
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs balance status",
                    "No balance found on '/mnt/storage'\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw {
                    mount_point: MountPoint("/mnt/storage".to_owned()),
                },
                ok_raw(
                    "btrfs filesystem usage",
                    "Overall:\n\
                     \tDevice size:\t\t\t1073741824\n\
                     \tDevice allocated:\t\t503316480\n\
                     \tDevice unallocated:\t\t570425344\n\
                     \tUsed:\t\t\t\t33914880\n\
                     \tFree (estimated):\t\t442957824\t(min: 442957824)\n\
                     \tData ratio:\t\t\t2.00\n",
                ),
            );

        let result = probe_pool_for_tui(
            &runner,
            &StubFs::empty(),
            &MountPoint("/mnt/storage".into()),
            &HashMap::new(),
            &test_paths().1,
        )
        .unwrap();
        let pool = result.expect("pool should be Some");

        // Verify balance is idle
        assert_eq!(pool.balance, crate::status::BalanceReport::Idle);

        // Verify toshiba (devid 1) allocations
        let toshiba = pool
            .disk_usage
            .get("toshiba")
            .expect("toshiba should be present");
        assert_eq!(toshiba.size, 536870912);
        assert_eq!(toshiba.unallocated, 409403392);
        assert_eq!(toshiba.allocations.len(), 3);
        assert_eq!(
            toshiba.allocations[0],
            DeviceAllocation {
                alloc_type: "Data".into(),
                profile: "RAID1".into(),
                bytes: 67108864
            },
        );
        assert_eq!(
            toshiba.allocations[1],
            DeviceAllocation {
                alloc_type: "Metadata".into(),
                profile: "DUP".into(),
                bytes: 51970048
            },
        );
        assert_eq!(
            toshiba.allocations[2],
            DeviceAllocation {
                alloc_type: "System".into(),
                profile: "DUP".into(),
                bytes: 8388608
            },
        );
        assert_eq!(toshiba.allocated(), 67108864 + 51970048 + 8388608);

        // Verify ironwolf (devid 2) has same structure
        let ironwolf = pool
            .disk_usage
            .get("ironwolf")
            .expect("ironwolf should be present");
        assert_eq!(ironwolf.allocations.len(), 3);
        assert_eq!(ironwolf.unallocated, 409403392);

        // Verify capacity: 2 equal disks of 536870912 → estimated total = 536870912
        assert_eq!(pool.capacity_total_bytes, Some(536870912));
        assert_eq!(pool.capacity_used_bytes, 33914880);
    }

    /// Helper: build the minimum mock-runner mocks for a single-disk
    /// mounted-pool probe so the unpooled-disk classification tests can
    /// reuse them. Returns a runner with everything set up except for
    /// any per-test cryptsetup mocks the caller wants to add for a
    /// declared but unpooled disk.
    fn one_disk_mounted_pool_runner() -> MockRunner {
        let mp = MountPoint("/mnt/storage".to_owned());
        MockRunner::default()
            .with_output(
                CmdRequest::FindmntJson {
                    mount_point: mp.clone(),
                },
                ok_raw(
                    "findmnt",
                    r#"{"filesystems": [{"target":"/mnt/storage","source":"/dev/mapper/braid-toshiba","fstype":"btrfs"}]}"#,
                ),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemShow {
                    mount_point: mp.clone(),
                },
                ok_raw(
                    "btrfs filesystem show",
                    "Label: none  uuid: aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee\n\
                     \tTotal devices 1 FS bytes used 1.00GiB\n\
                     \tdevid    1 size 10.00GiB used 2.00GiB path /dev/mapper/braid-toshiba\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupStatus {
                    mapper: "braid-toshiba".into(),
                },
                ok_raw(
                    "cryptsetup status",
                    "/dev/mapper/braid-toshiba is active.\n\tdevice:  /dev/vda\n",
                ),
            )
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/vda".into(),
                },
                ok_raw(
                    "cryptsetup luksUUID",
                    "11111111-1111-1111-1111-111111111111\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemDfJson {
                    mount_point: mp.clone(),
                },
                ok_raw(
                    "btrfs filesystem df",
                    r#"{"filesystem-df": [
                        {"bg-type": "Data", "bg-profile": "RAID1", "total": 67108864, "used": 16777216}
                    ]}"#,
                ),
            )
            .with_output(
                CmdRequest::BtrfsDeviceUsageRaw {
                    mount_point: mp.clone(),
                },
                ok_raw(
                    "btrfs device usage",
                    "/dev/dm-0, ID: 1\n\
                     \x20  Device size:          536870912\n\
                     \x20  Device slack:              0\n\
                     \x20  Data,RAID1:           67108864\n\
                     \x20  Unallocated:          409403392\n",
                ),
            )
            .with_output(
                CmdRequest::BtrfsBalanceStatus {
                    mount_point: mp.clone(),
                },
                ok_raw("btrfs balance status", "No balance found on '/mnt/storage'\n"),
            )
            .with_output(
                CmdRequest::BtrfsFilesystemUsageRaw {
                    mount_point: mp.clone(),
                },
                ok_raw(
                    "btrfs filesystem usage",
                    "Overall:\n\
                     \tDevice size:\t\t\t1073741824\n\
                     \tDevice allocated:\t\t503316480\n\
                     \tDevice unallocated:\t\t570425344\n\
                     \tUsed:\t\t\t\t33914880\n\
                     \tFree (estimated):\t\t442957824\t(min: 442957824)\n\
                     \tData ratio:\t\t\t2.00\n",
                ),
            )
    }

    /// Intent: probe_pool_for_tui must classify a declared disk that is
    /// absent from the host as `UnpooledDiskRender::Missing`.
    ///
    /// Why: this is the existing "device unplugged" baseline; ensuring it
    /// produces a record (not just a no-entry hole) means the disk table
    /// can render a per-row label even for the simple unplugged case.
    ///
    /// Scenario: 1-disk live pool plus a second declared disk whose
    /// /dev/disk/by-id path does not exist on the host.
    #[test]
    fn unpooled_disk_absent_classified_as_missing() {
        let runner = one_disk_mounted_pool_runner();
        let fs = StubFs::with_paths(&["/dev/disk/by-id/braid-toshiba"]);

        let disk_by_id = HashMap::from([
            ("toshiba".to_owned(), "/dev/disk/by-id/braid-toshiba".to_owned()),
            ("ironwolf".to_owned(), "/dev/disk/by-id/braid-ironwolf".to_owned()),
        ]);

        let pool = probe_pool_for_tui(
            &runner,
            &fs,
            &MountPoint("/mnt/storage".into()),
            &disk_by_id,
            &test_paths().1,
        )
            .unwrap()
            .expect("pool should be Some");

        assert_eq!(
            pool.unpooled_disks.get("ironwolf"),
            Some(&UnpooledDiskRender::Missing)
        );
        // toshiba is in the live pool — must NOT be in unpooled_disks.
        assert!(
            !pool.unpooled_disks.contains_key("toshiba"),
            "live disks must not appear in unpooled_disks"
        );
    }

    /// Intent: probe_pool_for_tui must classify a declared disk that has a
    /// valid LUKS header whose UUID does NOT belong to the live pool as
    /// `UnpooledDiskRender::UnknownLuks` — distinct from "missing".
    ///
    /// Why: a stale-LUKS disk left over from a previous pool, or a disk
    /// belonging to a different braid instance, should be visibly
    /// different from a hot-unplugged cable so the operator does not
    /// confuse them.
    ///
    /// Scenario: 1-disk live pool with UUID `11111111...`. Second declared
    /// disk has a valid LUKS header reporting UUID `99999999...`.
    #[test]
    fn unpooled_disk_present_luks_unknown_uuid_classified_as_unknown_luks() {
        let runner = one_disk_mounted_pool_runner().with_output(
            CmdRequest::CryptsetupLuksUuid {
                device: "/dev/disk/by-id/braid-ironwolf".into(),
            },
            ok_raw(
                "cryptsetup luksUUID",
                "99999999-9999-9999-9999-999999999999\n",
            ),
        );
        let fs = StubFs::with_paths(&[
            "/dev/disk/by-id/braid-toshiba",
            "/dev/disk/by-id/braid-ironwolf",
        ]);

        let disk_by_id = HashMap::from([
            ("toshiba".to_owned(), "/dev/disk/by-id/braid-toshiba".to_owned()),
            ("ironwolf".to_owned(), "/dev/disk/by-id/braid-ironwolf".to_owned()),
        ]);

        let pool = probe_pool_for_tui(
            &runner,
            &fs,
            &MountPoint("/mnt/storage".into()),
            &disk_by_id,
            &test_paths().1,
        )
            .unwrap()
            .expect("pool should be Some");

        assert_eq!(
            pool.unpooled_disks.get("ironwolf"),
            Some(&UnpooledDiskRender::UnknownLuks)
        );
    }

    /// Intent: probe_pool_for_tui must refine PresentNotLuks → Unreadable
    /// when probe_luks_header reports the LUKS magic is gone.
    ///
    /// Why: the previous TUI rendered every unrepresented disk as plain
    /// "missing"; users could not see whether a header restore was the
    /// right next step. Surfacing Unreadable as a distinct state is the
    /// trigger that points the user at off-system header backups.
    ///
    /// Scenario: 1-disk live pool. Second declared disk: `luksUuid` exits
    /// non-zero, `isLuks` exits non-zero (LUKS magic missing).
    #[test]
    fn unpooled_disk_present_not_luks_unreadable_classified_correctly() {
        let runner = one_disk_mounted_pool_runner()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksUUID".into(),
                    stdout: String::new(),
                    stderr: "Device is not a valid LUKS device.\n".into(),
                    exit_status: 1,
                },
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup isLuks".into(),
                    stdout: String::new(),
                    stderr: "Device is not a valid LUKS device.\n".into(),
                    exit_status: 1,
                },
            );
        let fs = StubFs::with_paths(&[
            "/dev/disk/by-id/braid-toshiba",
            "/dev/disk/by-id/braid-ironwolf",
        ]);

        let disk_by_id = HashMap::from([
            ("toshiba".to_owned(), "/dev/disk/by-id/braid-toshiba".to_owned()),
            ("ironwolf".to_owned(), "/dev/disk/by-id/braid-ironwolf".to_owned()),
        ]);

        let pool = probe_pool_for_tui(
            &runner,
            &fs,
            &MountPoint("/mnt/storage".into()),
            &disk_by_id,
            &test_paths().1,
        )
            .unwrap()
            .expect("pool should be Some");

        assert_eq!(
            pool.unpooled_disks.get("ironwolf"),
            Some(&UnpooledDiskRender::LuksHeaderUnreadable)
        );
    }

    /// Intent: probe_pool_for_tui must refine PresentNotLuks → Damaged
    /// when isLuks succeeds but luksDump fails — the metadata-corruption
    /// case that has a distinct `cryptsetup repair` recovery story.
    ///
    /// Why: metadata damage is potentially repairable in place; collapsing
    /// it into the same "missing" or even Unreadable bucket would steer
    /// the user away from a less-destructive recovery option.
    ///
    /// Scenario: 1-disk live pool. Second declared disk: `luksUuid` fails,
    /// `isLuks` succeeds, `luksDump` fails.
    #[test]
    fn unpooled_disk_present_not_luks_damaged_classified_correctly() {
        let runner = one_disk_mounted_pool_runner()
            .with_output(
                CmdRequest::CryptsetupLuksUuid {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksUUID".into(),
                    stdout: String::new(),
                    stderr: "Cannot read LUKS header metadata.\n".into(),
                    exit_status: 1,
                },
            )
            .with_output(
                CmdRequest::CryptsetupIsLuks {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup isLuks".into(),
                    stdout: String::new(),
                    stderr: String::new(),
                    exit_status: 0,
                },
            )
            .with_output(
                CmdRequest::CryptsetupLuksDumpText {
                    device: "/dev/disk/by-id/braid-ironwolf".into(),
                },
                RawCommandOutput {
                    cmd: "cryptsetup luksDump".into(),
                    stdout: String::new(),
                    stderr: "Cannot read LUKS header metadata.\n".into(),
                    exit_status: 1,
                },
            );
        let fs = StubFs::with_paths(&[
            "/dev/disk/by-id/braid-toshiba",
            "/dev/disk/by-id/braid-ironwolf",
        ]);

        let disk_by_id = HashMap::from([
            ("toshiba".to_owned(), "/dev/disk/by-id/braid-toshiba".to_owned()),
            ("ironwolf".to_owned(), "/dev/disk/by-id/braid-ironwolf".to_owned()),
        ]);

        let pool = probe_pool_for_tui(
            &runner,
            &fs,
            &MountPoint("/mnt/storage".into()),
            &disk_by_id,
            &test_paths().1,
        )
            .unwrap()
            .expect("pool should be Some");

        assert_eq!(
            pool.unpooled_disks.get("ironwolf"),
            Some(&UnpooledDiskRender::LuksHeaderDamaged)
        );
    }
}
