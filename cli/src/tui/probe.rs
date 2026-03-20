use std::collections::HashMap;
use std::time::Instant;

use crate::cmd::{CmdRequest, CommandRunner};
use crate::parse::types::{ScrubState, SmartHealth};
use crate::parse::{
    parse_btrfs_device_stats, parse_btrfs_device_usage, parse_btrfs_filesystem_usage,
    parse_btrfs_scrub_status, parse_cryptsetup_luks_dump, parse_lsblk_json, parse_smartctl_health,
};
use crate::probe::probe_pool;
use crate::status::resolve_alert_state;
use crate::status::{estimate_pool_capacity, get_balance_report, DiskErrors};
use crate::tui::model::{DiskLuksInfo, DiskUsage, PoolState};
use crate::types::MountPoint;

pub fn probe_pool_for_tui<R: CommandRunner>(
    runner: &R,
    mount_point: &str,
    disk_by_id: &HashMap<String, String>,
) -> Result<Option<PoolState>, String> {
    let domain = probe_pool(runner, mount_point).map_err(|e| e.to_string())?;

    if !domain.mounted {
        return Ok(None);
    }

    let df_raw = runner
        .run(&CmdRequest::BtrfsFilesystemDfJson {
            mount_point: MountPoint(mount_point.to_owned()),
        })
        .map_err(|e| e.to_string())?;
    let df = crate::parse::parse_btrfs_df_json(&df_raw).map_err(|e| e.to_string())?;

    let dev_usage_raw = runner
        .run(&CmdRequest::BtrfsDeviceUsageRaw {
            mount_point: MountPoint(mount_point.to_owned()),
        })
        .map_err(|e| e.to_string())?;
    let dev_usage = parse_btrfs_device_usage(&dev_usage_raw).map_err(|e| e.to_string())?;

    // Map devid → disk name via probe_pool's devices (from btrfs filesystem show,
    // which reports stable /dev/mapper/braid-* paths). btrfs device usage may
    // report raw /dev/dm-N paths that don't match config disk names.
    let devid_to_name: HashMap<u64, &str> = domain
        .devices
        .iter()
        .filter_map(|d| {
            d.mapper
                .0
                .strip_prefix("braid-")
                .map(|name| (d.devid, name))
        })
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
            mount_point: MountPoint(mount_point.to_owned()),
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
        }) {
            if let Ok(dump) = parse_cryptsetup_luks_dump(&raw) {
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
    }

    // Extract transport type (sata, nvme, usb, etc.) from lsblk tree.
    // Walk parent devices: for each child named "braid-{name}", take the
    // parent's TRAN value. TRAN is only set on physical devices, not dm-crypt.
    let mut disk_transport = HashMap::new();
    if let Ok(lsblk_raw) = runner.run(&CmdRequest::LsblkJson) {
        if let Ok(lsblk) = parse_lsblk_json(&lsblk_raw) {
            for dev in &lsblk.blockdevices {
                if let Some(tran) = &dev.tran {
                    for child in &dev.children {
                        if let Some(name) = child.name.strip_prefix("braid-") {
                            disk_transport.insert(name.to_owned(), tran.clone());
                        }
                    }
                }
            }
        }
    }

    let fs_usage_raw = runner
        .run(&CmdRequest::BtrfsFilesystemUsageRaw {
            mount_point: MountPoint(mount_point.to_owned()),
        })
        .map_err(|e| e.to_string())?;
    let fs_usage = parse_btrfs_filesystem_usage(&fs_usage_raw).map_err(|e| e.to_string())?;

    // Device error stats
    let mut device_errors = HashMap::new();
    let device_stats_raw = runner
        .run(&CmdRequest::BtrfsDeviceStats {
            mount_point: MountPoint(mount_point.to_owned()),
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

    let alert_state = resolve_alert_state();

    let capacity_total_bytes = if domain.missing_count == 0 {
        let sizes: Vec<u64> = dev_usage.devices.iter().map(|d| d.device_size).collect();
        Some(estimate_pool_capacity(&sizes))
    } else {
        None
    };

    Ok(Some(PoolState {
        mount_point: MountPoint(mount_point.to_owned()),
        df_entries: df.entries,
        disk_usage,
        disk_transport,
        smart_health,
        luks_info,
        device_errors,
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

        let result = probe_pool_for_tui(&runner, "/mnt/storage", &HashMap::new()).unwrap();
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
}
