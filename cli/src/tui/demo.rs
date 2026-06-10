use std::collections::HashMap;
use std::time::Instant;

use crate::parse::types::{
    BtrfsBgType, BtrfsDfEntry, BtrfsProfile, DeviceAllocation, ScrubState, ScrubTimestamp,
    SmartEvidence, SmartHealth, SmartProbe,
};
use crate::status::{BalanceReport, DiskErrors};
use crate::tui::model::{DiskLockState, DiskLuksInfo, DiskLuksState, DiskUsage, PoolState};
use crate::types::MountPoint;

pub(crate) fn sample_disk_names() -> Vec<String> {
    vec![
        "toshiba".to_owned(),
        "ironwolf".to_owned(),
        "wdc".to_owned(),
        "samsung".to_owned(),
    ]
}

/// Demo-only LUKS snapshot kept separate from `sample_pool` because the
/// real TUI observes cryptsetup state even when btrfs is unavailable.
pub(crate) fn sample_disk_luks_states() -> HashMap<String, DiskLuksState> {
    sample_disk_names()
        .into_iter()
        .map(|name| {
            (
                name.clone(),
                DiskLuksState {
                    lock: DiskLockState::Unlocked,
                    underlying_present: Some(format!("/dev/disk/by-id/{name}")),
                    metadata: Some(DiskLuksInfo {
                        cipher: "aes-xts-plain64".to_owned(),
                        key_size_bits: 512,
                        keyslot_count: 1,
                    }),
                },
            )
        })
        .collect()
}

pub(crate) fn sample_pool() -> PoolState {
    let disk_usage = HashMap::from([
        (
            "toshiba".to_owned(),
            DiskUsage {
                size: 6_001_175_126_016,
                allocations: vec![
                    DeviceAllocation {
                        alloc_type: "Data".into(),
                        profile: "RAID1".into(),
                        bytes: 1_483_734_958_080,
                    },
                    DeviceAllocation {
                        alloc_type: "Metadata".into(),
                        profile: "DUP".into(),
                        bytes: 1_610_612_736,
                    },
                    DeviceAllocation {
                        alloc_type: "System".into(),
                        profile: "DUP".into(),
                        bytes: 16_777_216,
                    },
                ],
                unallocated: 4_515_816_777_984,
            },
        ),
        (
            "ironwolf".to_owned(),
            DiskUsage {
                size: 6_001_175_126_016,
                allocations: vec![
                    DeviceAllocation {
                        alloc_type: "Data".into(),
                        profile: "RAID1".into(),
                        bytes: 1_483_734_958_080,
                    },
                    DeviceAllocation {
                        alloc_type: "Metadata".into(),
                        profile: "DUP".into(),
                        bytes: 1_610_612_736,
                    },
                    DeviceAllocation {
                        alloc_type: "System".into(),
                        profile: "DUP".into(),
                        bytes: 16_777_216,
                    },
                ],
                unallocated: 4_515_816_777_984,
            },
        ),
        (
            "wdc".to_owned(),
            DiskUsage {
                size: 4_000_787_030_016,
                allocations: vec![
                    DeviceAllocation {
                        alloc_type: "Data".into(),
                        profile: "RAID1".into(),
                        bytes: 824_633_720_832,
                    },
                    DeviceAllocation {
                        alloc_type: "Metadata".into(),
                        profile: "DUP".into(),
                        bytes: 1_073_741_824,
                    },
                    DeviceAllocation {
                        alloc_type: "System".into(),
                        profile: "DUP".into(),
                        bytes: 16_777_216,
                    },
                ],
                unallocated: 3_175_062_790_144,
            },
        ),
        (
            "samsung".to_owned(),
            DiskUsage {
                size: 2_000_398_934_016,
                allocations: vec![
                    DeviceAllocation {
                        alloc_type: "Data".into(),
                        profile: "RAID1".into(),
                        bytes: 824_633_720_832,
                    },
                    DeviceAllocation {
                        alloc_type: "Metadata".into(),
                        profile: "DUP".into(),
                        bytes: 1_073_741_824,
                    },
                    DeviceAllocation {
                        alloc_type: "System".into(),
                        profile: "DUP".into(),
                        bytes: 16_777_216,
                    },
                ],
                unallocated: 1_174_674_694_144,
            },
        ),
    ]);
    // Index 0 (toshiba) is the headline `snapshot_disk_detail` target -- a
    // degraded SATA drive (reallocated > 0) over clean btrfs errors, so the
    // default detail snapshot exercises a red SATA evidence row and demonstrates
    // the btrfs/SMART independence. ironwolf inverts it (clean SMART over a
    // non-zero btrfs counter), wdc covers the single-health-row Unknown case, and
    // samsung covers the NVMe evidence-row path (wear-degraded).
    let smart = HashMap::from([
        (
            "toshiba".to_owned(),
            SmartProbe {
                health: SmartHealth::Degraded,
                evidence: Some(SmartEvidence::Sata {
                    reallocated_sectors: 2,
                    pending_sectors: 0,
                    offline_uncorrectable: 0,
                }),
                celsius: None,
            },
        ),
        (
            "ironwolf".to_owned(),
            SmartProbe {
                health: SmartHealth::Healthy,
                evidence: Some(SmartEvidence::Sata {
                    reallocated_sectors: 0,
                    pending_sectors: 0,
                    offline_uncorrectable: 0,
                }),
                celsius: None,
            },
        ),
        (
            "wdc".to_owned(),
            SmartProbe {
                health: SmartHealth::Unknown,
                evidence: None,
                celsius: None,
            },
        ),
        (
            "samsung".to_owned(),
            SmartProbe {
                health: SmartHealth::Degraded,
                evidence: Some(SmartEvidence::Nvme {
                    media_errors: 0,
                    critical_warning: 0,
                    percentage_used: 92,
                    available_spare: 100,
                    available_spare_threshold: 10,
                }),
                celsius: None,
            },
        ),
    ]);
    let disk_transport = HashMap::from([
        ("toshiba".to_owned(), "sata".to_owned()),
        ("ironwolf".to_owned(), "sata".to_owned()),
        ("wdc".to_owned(), "usb".to_owned()),
        ("samsung".to_owned(), "nvme".to_owned()),
    ]);
    PoolState {
        mount_point: MountPoint::new("/mnt/storage".to_owned()),
        df_entries: vec![
            BtrfsDfEntry {
                bg_type: BtrfsBgType::Data,
                bg_profile: BtrfsProfile::Raid1,
                bg_used: 2_308_094_370_816,
                bg_total: 5_937_955_045_376,
            },
            BtrfsDfEntry {
                bg_type: BtrfsBgType::Metadata,
                bg_profile: BtrfsProfile::Raid1,
                bg_used: 1_610_612_736,
                bg_total: 2_147_483_648,
            },
            BtrfsDfEntry {
                bg_type: BtrfsBgType::System,
                bg_profile: BtrfsProfile::Raid1,
                bg_used: 16_384,
                bg_total: 16_777_216,
            },
            BtrfsDfEntry {
                bg_type: BtrfsBgType::GlobalReserve,
                bg_profile: BtrfsProfile::Single,
                bg_used: 0,
                bg_total: 5_767_168,
            },
        ],
        disk_usage,
        disk_transport,
        smart,
        disk_temperature_readings: HashMap::new(),
        disk_underlying: HashMap::new(),
        device_errors: HashMap::from([
            (
                "toshiba".to_owned(),
                DiskErrors {
                    read: 0,
                    write: 0,
                    flush: 0,
                    corruption: 0,
                    generation: 0,
                },
            ),
            (
                "ironwolf".to_owned(),
                DiskErrors {
                    read: 3,
                    write: 0,
                    flush: 0,
                    corruption: 0,
                    generation: 0,
                },
            ),
            (
                "wdc".to_owned(),
                DiskErrors {
                    read: 0,
                    write: 0,
                    flush: 0,
                    corruption: 0,
                    generation: 0,
                },
            ),
            (
                "samsung".to_owned(),
                DiskErrors {
                    read: 0,
                    write: 0,
                    flush: 0,
                    corruption: 0,
                    generation: 0,
                },
            ),
        ]),
        unpooled_disks: HashMap::new(),
        alert_state: crate::alert::AlertState::default(),
        scrub: ScrubState::Finished {
            started_at: ScrubTimestamp(time::macros::datetime!(2026-02-24 02:00:07)),
            error_count: 0,
            duration_secs: Some(0),
            total_bytes: Some(33_931_264),
            rate_bytes_per_sec: Some(33_910_682),
        },
        balance: BalanceReport::Idle,
        capacity_total_bytes: Some(8_001_568_641_024),
        capacity_used_bytes: 2_308_094_370_816,
        probed_at: Instant::now(),
    }
}
