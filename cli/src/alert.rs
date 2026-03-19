use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::parse::types::{BtrfsDeviceStatsOutput, DeviceErrorStats};
use crate::state_io::atomic_write;

pub const ACKED_STATS_FILE: &str = "/var/lib/braid/acked-stats.json";
pub const SMARTD_ALERT_FILE: &str = "/var/lib/braid/smartd-alert";

// ---------------------------------------------------------------------------
// Alert model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AlertState {
    pub active: bool,
    pub causes: Vec<AlertCause>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AlertCause {
    BtrfsDeviceErrors { devid: u64 },
    MissingDevice { devid: u64 },
    SmartdAlert,
}

// ---------------------------------------------------------------------------
// Acked state
// ---------------------------------------------------------------------------

/// Keyed by btrfs devid (e.g. "1", "2").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub struct AckedStats(pub BTreeMap<String, AckedDisk>);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AckedDisk {
    pub missing_acked: bool,
    pub device_stats: AckedDeviceCounters,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AckedDeviceCounters {
    pub read_io_errs: u64,
    pub write_io_errs: u64,
    pub flush_io_errs: u64,
    pub corruption_errs: u64,
    pub generation_errs: u64,
}

impl Default for AckedStats {
    fn default() -> Self {
        Self(BTreeMap::new())
    }
}

// ---------------------------------------------------------------------------
// Load / save
// ---------------------------------------------------------------------------

pub fn load_acked_stats() -> AckedStats {
    load_acked_stats_at(Path::new(ACKED_STATS_FILE))
}

pub fn load_acked_stats_at(path: &Path) -> AckedStats {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return AckedStats::default(),
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

pub fn save_acked_stats(stats: &AckedStats) -> Result<(), std::io::Error> {
    save_acked_stats_at(Path::new(ACKED_STATS_FILE), stats)
}

pub fn save_acked_stats_at(path: &Path, stats: &AckedStats) -> Result<(), std::io::Error> {
    let json = serde_json::to_string_pretty(stats).map_err(std::io::Error::other)?;
    atomic_write(path, json.as_bytes())
}

// ---------------------------------------------------------------------------
// Shared computation
// ---------------------------------------------------------------------------

/// Compute alert state with explicit devid mapping from device paths.
pub fn compute_alert_state_with_devid_map(
    current_stats: &BtrfsDeviceStatsOutput,
    acked: &AckedStats,
    missing_devids: &[u64],
    smartd_alert_active: bool,
    path_to_devid: &BTreeMap<String, u64>,
) -> AlertState {
    let mut causes = Vec::new();

    for dev in &current_stats.devices {
        let devid = path_to_devid.get(&dev.device_path).copied().unwrap_or(0);
        let key = devid.to_string();
        let acked_disk = acked.0.get(&key);
        let acked_counters = acked_disk.map(|d| &d.device_stats);

        if has_new_errors(dev, acked_counters) {
            causes.push(AlertCause::BtrfsDeviceErrors { devid });
        }
    }

    for &devid in missing_devids {
        let key = devid.to_string();
        let missing_acked = acked.0.get(&key).map(|d| d.missing_acked).unwrap_or(false);
        if !missing_acked {
            causes.push(AlertCause::MissingDevice { devid });
        }
    }

    if smartd_alert_active {
        causes.push(AlertCause::SmartdAlert);
    }

    AlertState {
        active: !causes.is_empty(),
        causes,
    }
}

fn has_new_errors(current: &DeviceErrorStats, acked: Option<&AckedDeviceCounters>) -> bool {
    let zero = AckedDeviceCounters::default();
    let acked = acked.unwrap_or(&zero);

    // Counter reset detection: if current < acked, treat acked as 0
    let effective_read = if current.read_io_errs < acked.read_io_errs {
        0
    } else {
        acked.read_io_errs
    };
    let effective_write = if current.write_io_errs < acked.write_io_errs {
        0
    } else {
        acked.write_io_errs
    };
    let effective_flush = if current.flush_io_errs < acked.flush_io_errs {
        0
    } else {
        acked.flush_io_errs
    };
    let effective_corruption = if current.corruption_errs < acked.corruption_errs {
        0
    } else {
        acked.corruption_errs
    };
    let effective_generation = if current.generation_errs < acked.generation_errs {
        0
    } else {
        acked.generation_errs
    };

    current.read_io_errs > effective_read
        || current.write_io_errs > effective_write
        || current.flush_io_errs > effective_flush
        || current.corruption_errs > effective_corruption
        || current.generation_errs > effective_generation
}

// ---------------------------------------------------------------------------
// Snapshot current state for ack
// ---------------------------------------------------------------------------

pub fn snapshot_current(
    current_stats: &BtrfsDeviceStatsOutput,
    missing_devids: &[u64],
    path_to_devid: &BTreeMap<String, u64>,
) -> AckedStats {
    let mut map = BTreeMap::new();

    for dev in &current_stats.devices {
        let devid = path_to_devid.get(&dev.device_path).copied().unwrap_or(0);
        let key = devid.to_string();
        map.insert(
            key,
            AckedDisk {
                missing_acked: false,
                device_stats: AckedDeviceCounters {
                    read_io_errs: dev.read_io_errs,
                    write_io_errs: dev.write_io_errs,
                    flush_io_errs: dev.flush_io_errs,
                    corruption_errs: dev.corruption_errs,
                    generation_errs: dev.generation_errs,
                },
            },
        );
    }

    // Missing devices get missing_acked = true
    for &devid in missing_devids {
        let key = devid.to_string();
        map.entry(key).or_insert(AckedDisk {
            missing_acked: true,
            device_stats: AckedDeviceCounters::default(),
        });
    }

    AckedStats(map)
}

/// Check if the smartd alert flag file exists.
pub fn smartd_alert_active() -> bool {
    Path::new(SMARTD_ALERT_FILE).exists()
}

/// Remove the smartd alert flag file. Returns Ok(()) even if it didn't exist.
pub fn remove_smartd_alert_flag() -> Result<(), std::io::Error> {
    match std::fs::remove_file(SMARTD_ALERT_FILE) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn make_stats(devices: Vec<DeviceErrorStats>) -> BtrfsDeviceStatsOutput {
        BtrfsDeviceStatsOutput { devices }
    }

    fn zero_device(path: &str) -> DeviceErrorStats {
        DeviceErrorStats {
            device_path: path.to_owned(),
            read_io_errs: 0,
            write_io_errs: 0,
            flush_io_errs: 0,
            corruption_errs: 0,
            generation_errs: 0,
        }
    }

    fn devid_map(entries: &[(&str, u64)]) -> BTreeMap<String, u64> {
        entries
            .iter()
            .map(|(path, id)| (path.to_string(), *id))
            .collect()
    }

    #[test]
    fn roundtrip_acked_stats() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("acked-stats.json");

        let mut map = BTreeMap::new();
        map.insert(
            "1".to_owned(),
            AckedDisk {
                missing_acked: false,
                device_stats: AckedDeviceCounters {
                    read_io_errs: 3,
                    write_io_errs: 0,
                    flush_io_errs: 0,
                    corruption_errs: 1,
                    generation_errs: 0,
                },
            },
        );
        let stats = AckedStats(map);
        save_acked_stats_at(&path, &stats).unwrap();
        let reloaded = load_acked_stats_at(&path);
        assert_eq!(reloaded, stats);
    }

    #[test]
    fn load_missing_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let stats = load_acked_stats_at(&path);
        assert!(stats.0.is_empty());
    }

    #[test]
    fn load_corrupt_file_returns_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, "not json").unwrap();
        let stats = load_acked_stats_at(&path);
        assert!(stats.0.is_empty());
    }

    #[test]
    fn no_alert_when_all_zero() {
        let stats = make_stats(vec![zero_device("/dev/mapper/braid-vda")]);
        let acked = AckedStats::default();
        let map = devid_map(&[("/dev/mapper/braid-vda", 1)]);
        let alert = compute_alert_state_with_devid_map(&stats, &acked, &[], false, &map);
        assert!(!alert.active);
        assert!(alert.causes.is_empty());
    }

    #[test]
    fn alert_on_btrfs_device_errors() {
        let mut dev = zero_device("/dev/mapper/braid-vda");
        dev.read_io_errs = 3;
        dev.corruption_errs = 1;
        let stats = make_stats(vec![dev]);
        let acked = AckedStats::default();
        let map = devid_map(&[("/dev/mapper/braid-vda", 1)]);
        let alert = compute_alert_state_with_devid_map(&stats, &acked, &[], false, &map);
        assert!(alert.active);
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(alert.causes[0], AlertCause::BtrfsDeviceErrors { devid: 1 });
    }

    #[test]
    fn alert_on_missing_device() {
        let stats = make_stats(vec![zero_device("/dev/mapper/braid-vda")]);
        let acked = AckedStats::default();
        let map = devid_map(&[("/dev/mapper/braid-vda", 1)]);
        let alert = compute_alert_state_with_devid_map(&stats, &acked, &[2], false, &map);
        assert!(alert.active);
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(alert.causes[0], AlertCause::MissingDevice { devid: 2 });
    }

    #[test]
    fn alert_on_smartd() {
        let stats = make_stats(vec![zero_device("/dev/mapper/braid-vda")]);
        let acked = AckedStats::default();
        let map = devid_map(&[("/dev/mapper/braid-vda", 1)]);
        let alert = compute_alert_state_with_devid_map(&stats, &acked, &[], true, &map);
        assert!(alert.active);
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(alert.causes[0], AlertCause::SmartdAlert);
    }

    #[test]
    fn no_alert_after_ack() {
        let mut dev = zero_device("/dev/mapper/braid-vda");
        dev.read_io_errs = 3;
        let stats = make_stats(vec![dev]);

        let mut acked_map = BTreeMap::new();
        acked_map.insert(
            "1".to_owned(),
            AckedDisk {
                missing_acked: false,
                device_stats: AckedDeviceCounters {
                    read_io_errs: 3,
                    ..Default::default()
                },
            },
        );
        let acked = AckedStats(acked_map);
        let map = devid_map(&[("/dev/mapper/braid-vda", 1)]);
        let alert = compute_alert_state_with_devid_map(&stats, &acked, &[], false, &map);
        assert!(!alert.active);
    }

    #[test]
    fn counter_reset_detection() {
        // Current < acked means counters were reset (remount). Treat acked as 0,
        // so current value (which is > 0) triggers an alert.
        let mut dev = zero_device("/dev/mapper/braid-vda");
        dev.read_io_errs = 1;
        let stats = make_stats(vec![dev]);

        let mut acked_map = BTreeMap::new();
        acked_map.insert(
            "1".to_owned(),
            AckedDisk {
                missing_acked: false,
                device_stats: AckedDeviceCounters {
                    read_io_errs: 5,
                    ..Default::default()
                },
            },
        );
        let acked = AckedStats(acked_map);
        let map = devid_map(&[("/dev/mapper/braid-vda", 1)]);
        let alert = compute_alert_state_with_devid_map(&stats, &acked, &[], false, &map);
        assert!(alert.active, "counter reset should trigger alert");
    }

    #[test]
    fn missing_acked_suppresses_alert() {
        let stats = make_stats(vec![zero_device("/dev/mapper/braid-vda")]);
        let mut acked_map = BTreeMap::new();
        acked_map.insert(
            "2".to_owned(),
            AckedDisk {
                missing_acked: true,
                device_stats: AckedDeviceCounters::default(),
            },
        );
        let acked = AckedStats(acked_map);
        let map = devid_map(&[("/dev/mapper/braid-vda", 1)]);
        let alert = compute_alert_state_with_devid_map(&stats, &acked, &[2], false, &map);
        assert!(!alert.active, "acked missing should not trigger alert");
    }

    #[test]
    fn multiple_causes() {
        let mut dev = zero_device("/dev/mapper/braid-vda");
        dev.write_io_errs = 1;
        let stats = make_stats(vec![dev]);
        let acked = AckedStats::default();
        let map = devid_map(&[("/dev/mapper/braid-vda", 1)]);
        let alert = compute_alert_state_with_devid_map(&stats, &acked, &[2], true, &map);
        assert!(alert.active);
        assert_eq!(alert.causes.len(), 3);
    }

    #[test]
    fn snapshot_current_captures_stats() {
        let mut dev = zero_device("/dev/mapper/braid-vda");
        dev.read_io_errs = 3;
        dev.corruption_errs = 1;
        let stats = make_stats(vec![dev]);
        let map = devid_map(&[("/dev/mapper/braid-vda", 1)]);
        let snapshot = snapshot_current(&stats, &[2], &map);

        let disk1 = snapshot.0.get("1").unwrap();
        assert!(!disk1.missing_acked);
        assert_eq!(disk1.device_stats.read_io_errs, 3);
        assert_eq!(disk1.device_stats.corruption_errs, 1);

        let disk2 = snapshot.0.get("2").unwrap();
        assert!(disk2.missing_acked);
    }

    #[test]
    fn new_errors_after_ack_trigger_alert() {
        let mut dev = zero_device("/dev/mapper/braid-vda");
        dev.read_io_errs = 5;
        let stats = make_stats(vec![dev]);

        let mut acked_map = BTreeMap::new();
        acked_map.insert(
            "1".to_owned(),
            AckedDisk {
                missing_acked: false,
                device_stats: AckedDeviceCounters {
                    read_io_errs: 3,
                    ..Default::default()
                },
            },
        );
        let acked = AckedStats(acked_map);
        let map = devid_map(&[("/dev/mapper/braid-vda", 1)]);
        let alert = compute_alert_state_with_devid_map(&stats, &acked, &[], false, &map);
        assert!(
            alert.active,
            "new errors above acked baseline should trigger alert"
        );
    }
}
