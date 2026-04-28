use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::parse::types::{BtrfsDeviceStatsOutput, DeviceErrorStats, DeviceStatsTarget};
use crate::state_io::atomic_write;
use crate::state_paths::StatePaths;

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
    ComputationError { detail: String },
}

// ---------------------------------------------------------------------------
// Acked state
// ---------------------------------------------------------------------------

/// Keyed by btrfs devid (e.g. "1", "2").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
#[derive(Default)]
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

// ---------------------------------------------------------------------------
// Load / save
// ---------------------------------------------------------------------------

pub fn load_acked_stats(paths: &StatePaths) -> AckedStats {
    load_acked_stats_at(&paths.acked_stats_json())
}

pub fn load_acked_stats_at(path: &Path) -> AckedStats {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return AckedStats::default(),
    };
    serde_json::from_str(&contents).unwrap_or_default()
}

pub fn save_acked_stats(stats: &AckedStats, paths: &StatePaths) -> Result<(), std::io::Error> {
    save_acked_stats_at(&paths.acked_stats_json(), stats)
}

pub fn save_acked_stats_at(path: &Path, stats: &AckedStats) -> Result<(), std::io::Error> {
    let json = serde_json::to_string_pretty(stats).map_err(std::io::Error::other)?;
    atomic_write(path, json.as_bytes())
}

// ---------------------------------------------------------------------------
// Shared computation
// ---------------------------------------------------------------------------

/// Compute alert state from current btrfs stats, acked baselines, and the
/// alert-local missing devids set.
///
/// Identity is `dev.devid` (btrfs supplies it on every stats row). The
/// `<missing disk>` sentinel is skipped here even though its devid is
/// available -- those rows always carry zero counters and `MissingDevice`
/// causes are generated independently from `missing_devids`.
pub fn compute_alert_state(
    current_stats: &BtrfsDeviceStatsOutput,
    acked: &AckedStats,
    missing_devids: &[u64],
    smartd_alert_active: bool,
) -> AlertState {
    let mut causes = Vec::new();

    for dev in &current_stats.devices {
        if matches!(dev.target, DeviceStatsTarget::MissingDisk) {
            continue;
        }
        let devid = dev.devid;
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
) -> AckedStats {
    let mut map = BTreeMap::new();

    for dev in &current_stats.devices {
        // Skip <missing disk> sentinel rows: their devid is available but the
        // row carries zero counters and is already represented in
        // missing_devids below.
        if matches!(dev.target, DeviceStatsTarget::MissingDisk) {
            continue;
        }
        let key = dev.devid.to_string();
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

    // Missing devices get missing_acked = true. Use insert-or-update so
    // that devices which appear in both stats (mapper still exists) and
    // missing_devids (null-underlying) get missing_acked = true.
    for &devid in missing_devids {
        let key = devid.to_string();
        map.entry(key)
            .and_modify(|d| d.missing_acked = true)
            .or_insert(AckedDisk {
                missing_acked: true,
                device_stats: AckedDeviceCounters::default(),
            });
    }

    AckedStats(map)
}

/// Drop the acked entry for `devid`. Returns true if an entry was removed.
pub fn drop_acked_devid(acked: &mut AckedStats, devid: u64) -> bool {
    acked.0.remove(&devid.to_string()).is_some()
}

fn load_acked_stats_fallible(paths: &StatePaths) -> Result<AckedStats, std::io::Error> {
    let path = paths.acked_stats_json();
    let contents = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(AckedStats::default());
        }
        Err(e) => return Err(e),
    };
    serde_json::from_str(&contents).map_err(std::io::Error::other)
}

/// Load acked-stats, drop entries for each devid, and persist on change.
///
/// Read and parse errors are propagated so mutation paths can fail closed
/// when ack state may be stale.
pub fn drop_ghost_acked_for_devids(
    paths: &StatePaths,
    devids: &[u64],
) -> Result<bool, std::io::Error> {
    if devids.is_empty() {
        return Ok(false);
    }

    let mut acked = load_acked_stats_fallible(paths)?;
    let mut changed = false;
    for &devid in devids {
        changed |= drop_acked_devid(&mut acked, devid);
    }
    if changed {
        save_acked_stats(&acked, paths)?;
    }
    Ok(changed)
}

/// Delete acked-stats outright. Absence is already the empty state.
pub fn remove_acked_stats(paths: &StatePaths) -> Result<(), std::io::Error> {
    match std::fs::remove_file(paths.acked_stats_json()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Reconcile acked-stats with current pool membership.
///
/// Unknown keys are kept: if braid cannot parse a key, it should not delete
/// operator data silently.
pub fn reconcile_acked_stats(
    acked: &mut AckedStats,
    still_relevant: &BTreeSet<u64>,
    present: &BTreeSet<u64>,
) -> bool {
    let mut changed = false;
    acked.0.retain(|key, disk| {
        let Ok(devid) = key.parse::<u64>() else {
            return true;
        };
        if !still_relevant.contains(&devid) {
            changed = true;
            return false;
        }
        if disk.missing_acked && present.contains(&devid) {
            disk.missing_acked = false;
            changed = true;
        }
        true
    });
    changed
}

/// Check if the smartd alert flag file exists.
pub fn smartd_alert_active(paths: &StatePaths) -> bool {
    paths.smartd_alert().exists()
}

/// Remove the smartd alert flag file. Returns Ok(()) even if it didn't exist.
pub fn remove_smartd_alert_flag(paths: &StatePaths) -> Result<(), std::io::Error> {
    match std::fs::remove_file(paths.smartd_alert()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Alert latch file
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
pub enum LatchLoadError {
    #[error("read alert latch: {0}")]
    Read(#[from] std::io::Error),
    #[error("parse alert latch: {0}")]
    Parse(#[from] serde_json::Error),
}

pub fn load_alert_latch(paths: &StatePaths) -> Result<Option<AlertState>, LatchLoadError> {
    load_alert_latch_at(&paths.alert_latch_json())
}

pub fn load_alert_latch_at(path: &Path) -> Result<Option<AlertState>, LatchLoadError> {
    // Read bytes (not String) so invalid UTF-8 surfaces as a Parse error via
    // serde_json, not as an io::Error::InvalidData wrapped in Read. Read/Parse
    // splits "filesystem failed" from "on-disk content is wrong"; non-UTF-8
    // bytes are the latter.
    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(LatchLoadError::Read(e)),
    };
    Ok(Some(serde_json::from_slice(&bytes)?))
}

/// Mutation-path variant: on read/parse failure, move the bad file aside to
/// `alert-latch.json.corrupt` (best effort) and return a detail string so the
/// caller can plant a `ComputationError` cause. On success, behaves like
/// `load_alert_latch`.
pub fn load_alert_latch_or_quarantine(paths: &StatePaths) -> (Option<AlertState>, Option<String>) {
    match load_alert_latch(paths) {
        Ok(opt) => (opt, None),
        Err(e) => {
            let detail = e.to_string();
            eprintln!("warning: alert latch unreadable -- quarantining: {detail}");
            let _ = std::fs::rename(paths.alert_latch_json(), paths.alert_latch_corrupt());
            (None, Some(detail))
        }
    }
}

pub fn save_alert_latch(state: &AlertState, paths: &StatePaths) -> Result<(), std::io::Error> {
    let json = serde_json::to_string_pretty(state).map_err(std::io::Error::other)?;
    atomic_write(&paths.alert_latch_json(), json.as_bytes())
}

pub fn remove_alert_latch(paths: &StatePaths) -> Result<(), std::io::Error> {
    match std::fs::remove_file(paths.alert_latch_json()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

pub fn remove_alert_latch_corrupt(paths: &StatePaths) -> Result<(), std::io::Error> {
    match std::fs::remove_file(paths.alert_latch_corrupt()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// Latch merging
// ---------------------------------------------------------------------------

/// Merge existing latched causes with newly detected live causes.
///
/// Algorithm:
/// 1. Start with all causes from the existing latch (carried forward).
/// 2. For each live cause: if a latched cause matches by key, replace it;
///    otherwise append.
/// 3. Result: `AlertState { active: !causes.is_empty(), causes }`.
pub fn merge_into_latch(
    existing_latch: Option<&AlertState>,
    live_causes: &[AlertCause],
) -> AlertState {
    let mut causes: Vec<AlertCause> = existing_latch.map(|s| s.causes.clone()).unwrap_or_default();

    for new_cause in live_causes.iter() {
        if let Some(pos) = causes
            .iter()
            .position(|existing| same_cause_key(existing, new_cause))
        {
            causes[pos] = new_cause.clone();
        } else {
            causes.push(new_cause.clone());
        }
    }

    AlertState {
        active: !causes.is_empty(),
        causes,
    }
}

/// Two causes match by key if they identify the same "slot" in the latch.
fn same_cause_key(a: &AlertCause, b: &AlertCause) -> bool {
    match (a, b) {
        (
            AlertCause::BtrfsDeviceErrors { devid: a },
            AlertCause::BtrfsDeviceErrors { devid: b },
        ) => a == b,
        (AlertCause::MissingDevice { devid: a }, AlertCause::MissingDevice { devid: b }) => a == b,
        (AlertCause::SmartdAlert, AlertCause::SmartdAlert) => true,
        (AlertCause::ComputationError { .. }, AlertCause::ComputationError { .. }) => true,
        _ => false,
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

    fn zero_device(path: &str, devid: u64) -> DeviceErrorStats {
        DeviceErrorStats {
            devid,
            target: DeviceStatsTarget::Path(path.to_owned()),
            read_io_errs: 0,
            write_io_errs: 0,
            flush_io_errs: 0,
            corruption_errs: 0,
            generation_errs: 0,
        }
    }

    fn zero_missing_device(devid: u64) -> DeviceErrorStats {
        DeviceErrorStats {
            devid,
            target: DeviceStatsTarget::MissingDisk,
            read_io_errs: 0,
            write_io_errs: 0,
            flush_io_errs: 0,
            corruption_errs: 0,
            generation_errs: 0,
        }
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

    /*
     * Intent: load_alert_latch returns Ok(None) when the latch file does not
     * exist, distinguishing "no alert state on disk" from "file exists but
     * unreadable / unparseable".
     *
     * Why it exists: the prior load_alert_latch returned Option<AlertState>
     * with .ok()? on both read and parse, conflating absent-file with corrupt-
     * file. Pinning the typed split prevents regressing back to that shape,
     * which silently dropped latched causes when a corrupt file was rebuilt.
     *
     * Scenario: fresh /var/lib/braid with no prior monitor run -- the latch
     * file simply does not exist yet. monitor must treat this as "empty
     * latch", not as an error.
     */
    #[test]
    fn load_alert_latch_absent_returns_ok_none() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nonexistent.json");
        let result = load_alert_latch_at(&path);
        assert!(matches!(result, Ok(None)), "got {result:?}");
    }

    /*
     * Intent: load_alert_latch returns Err(LatchLoadError::Parse) when the
     * latch file exists but does not contain valid AlertState JSON.
     *
     * Why it exists: this is the regression gate for the silent-drop bug.
     * The original .ok()? code returned None on parse failure, causing
     * cmd_monitor to merge live causes onto an empty slate and overwrite
     * the corrupt file -- silently violating "latched until ack". The
     * typed Parse variant lets each caller (monitor, status, ack) pick its
     * own fail-closed policy. Asserting the typed variant (not message
     * substrings) follows the project's typed-error convention.
     *
     * Scenario: /var/lib/braid/alert-latch.json contains garbage bytes
     * (manual edit, external tampering, or a future refactor that drops
     * atomic_write). Next monitor invocation must NOT pretend the file
     * is fine.
     */
    #[test]
    fn load_alert_latch_corrupt_returns_parse_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.json");
        std::fs::write(&path, b"not json").unwrap();
        let result = load_alert_latch_at(&path);
        assert!(
            matches!(result, Err(LatchLoadError::Parse(_))),
            "got {result:?}"
        );
    }

    /*
     * Intent: load_alert_latch round-trips a previously-saved AlertState
     * from disk, returning Ok(Some(state)).
     *
     * Why it exists: lock the happy path so the typed-error refactor
     * cannot regress the common case (a valid latch on disk being read
     * back). Also documents the contract that what save_alert_latch
     * writes, load_alert_latch reads back unchanged.
     *
     * Scenario: monitor wrote a latch on a prior cycle; the next ack/status
     * invocation must see the same AlertState.
     */
    #[test]
    fn load_alert_latch_valid_returns_ok_some() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("good.json");
        let original = AlertState {
            active: true,
            causes: vec![AlertCause::MissingDevice { devid: 7 }],
        };
        std::fs::write(&path, serde_json::to_string(&original).unwrap()).unwrap();
        let result = load_alert_latch_at(&path).unwrap();
        assert_eq!(result, Some(original));
    }

    /*
     * Intent: load_alert_latch_or_quarantine moves a corrupt latch file
     * aside to alert-latch.json.corrupt and returns (None, Some(detail))
     * so the caller can plant a loud ComputationError cause.
     *
     * Why it exists: this is the recovery primitive for cmd_monitor's
     * mutation paths. Without quarantine, the next save_alert_latch call
     * would overwrite the corrupt file and destroy forensic evidence.
     * The detail string is what gets folded into the new latch's
     * ComputationError so status surfaces the corruption instead of
     * silently rebuilding.
     *
     * Scenario: cmd_monitor finds the latch on disk is unparseable. It
     * must (a) preserve the bad bytes for later inspection, (b) report
     * the failure detail to the caller for surfacing.
     */
    #[test]
    fn quarantine_moves_corrupt_file_aside_and_reports_detail() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().to_path_buf());
        let garbage = b"not json".to_vec();
        std::fs::write(paths.alert_latch_json(), &garbage).unwrap();

        let (state, detail) = load_alert_latch_or_quarantine(&paths);

        assert!(state.is_none());
        let detail = detail.expect("quarantine reports a detail string");
        assert!(!detail.is_empty(), "detail must be non-empty");
        assert!(
            !paths.alert_latch_json().exists(),
            "live latch path must be moved aside"
        );
        let preserved = std::fs::read(paths.alert_latch_corrupt())
            .expect("corrupt sidecar must exist after quarantine");
        assert_eq!(preserved, garbage, "sidecar must contain original bytes");
    }

    #[test]
    fn no_alert_when_all_zero() {
        let stats = make_stats(vec![zero_device("/dev/mapper/braid-vda", 1)]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(&stats, &acked, &[], false);
        assert!(!alert.active);
        assert!(alert.causes.is_empty());
    }

    #[test]
    fn alert_on_btrfs_device_errors() {
        let mut dev = zero_device("/dev/mapper/braid-vda", 1);
        dev.read_io_errs = 3;
        dev.corruption_errs = 1;
        let stats = make_stats(vec![dev]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(&stats, &acked, &[], false);
        assert!(alert.active);
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(alert.causes[0], AlertCause::BtrfsDeviceErrors { devid: 1 });
    }

    #[test]
    fn alert_on_missing_device() {
        let stats = make_stats(vec![zero_device("/dev/mapper/braid-vda", 1)]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(&stats, &acked, &[2], false);
        assert!(alert.active);
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(alert.causes[0], AlertCause::MissingDevice { devid: 2 });
    }

    #[test]
    fn alert_on_smartd() {
        let stats = make_stats(vec![zero_device("/dev/mapper/braid-vda", 1)]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(&stats, &acked, &[], true);
        assert!(alert.active);
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(alert.causes[0], AlertCause::SmartdAlert);
    }

    #[test]
    fn no_alert_after_ack() {
        let mut dev = zero_device("/dev/mapper/braid-vda", 1);
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
        let alert = compute_alert_state(&stats, &acked, &[], false);
        assert!(!alert.active);
    }

    #[test]
    fn counter_reset_detection() {
        // Current < acked means counters were reset (remount). Treat acked as 0,
        // so current value (which is > 0) triggers an alert.
        let mut dev = zero_device("/dev/mapper/braid-vda", 1);
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
        let alert = compute_alert_state(&stats, &acked, &[], false);
        assert!(alert.active, "counter reset should trigger alert");
    }

    #[test]
    fn missing_acked_suppresses_alert() {
        let stats = make_stats(vec![zero_device("/dev/mapper/braid-vda", 1)]);
        let mut acked_map = BTreeMap::new();
        acked_map.insert(
            "2".to_owned(),
            AckedDisk {
                missing_acked: true,
                device_stats: AckedDeviceCounters::default(),
            },
        );
        let acked = AckedStats(acked_map);
        let alert = compute_alert_state(&stats, &acked, &[2], false);
        assert!(!alert.active, "acked missing should not trigger alert");
    }

    #[test]
    fn multiple_causes() {
        let mut dev = zero_device("/dev/mapper/braid-vda", 1);
        dev.write_io_errs = 1;
        let stats = make_stats(vec![dev]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(&stats, &acked, &[2], true);
        assert!(alert.active);
        assert_eq!(alert.causes.len(), 3);
    }

    #[test]
    fn snapshot_current_captures_stats() {
        let mut dev = zero_device("/dev/mapper/braid-vda", 1);
        dev.read_io_errs = 3;
        dev.corruption_errs = 1;
        let stats = make_stats(vec![dev]);
        let snapshot = snapshot_current(&stats, &[2]);

        let disk1 = snapshot.0.get("1").unwrap();
        assert!(!disk1.missing_acked);
        assert_eq!(disk1.device_stats.read_io_errs, 3);
        assert_eq!(disk1.device_stats.corruption_errs, 1);

        let disk2 = snapshot.0.get("2").unwrap();
        assert!(disk2.missing_acked);
    }

    #[test]
    fn new_errors_after_ack_trigger_alert() {
        let mut dev = zero_device("/dev/mapper/braid-vda", 1);
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
        let alert = compute_alert_state(&stats, &acked, &[], false);
        assert!(
            alert.active,
            "new errors above acked baseline should trigger alert"
        );
    }

    /*
     * Intent: a stats row whose path doesn't match any pool member but whose
     * devid is unknown produces no BtrfsDeviceErrors cause when its counters
     * are zero. This is the positive replacement for the deleted
     * `unmapped_device_is_error_in_alert` test.
     *
     * Why it exists: with devid as canonical identity, an "orphan" path is
     * no longer a fail-closed condition. The row simply has no acked
     * baseline (acked.0.get("99") is None) and zero counters, so
     * has_new_errors returns false. The alert pipeline must not treat
     * unknown-devid rows as a structural error.
     */
    #[test]
    fn unknown_devid_zero_counters_does_not_alert() {
        let stats = make_stats(vec![zero_device("/dev/mapper/braid-stale", 99)]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(&stats, &acked, &[], false);
        assert!(!alert.active);
        assert!(alert.causes.is_empty());
    }

    #[test]
    fn missing_disk_sentinel_skipped_in_alert() {
        // btrfs emits "<missing disk>" in device stats during degraded mount.
        // This sentinel must be skipped -- missing-device alerting comes from
        // missing_devids, not from stats rows.
        let stats = make_stats(vec![
            zero_device("/dev/mapper/braid-vda", 1),
            zero_missing_device(2),
        ]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(&stats, &acked, &[2], false);
        assert!(alert.active);
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(alert.causes[0], AlertCause::MissingDevice { devid: 2 });
    }

    #[test]
    fn missing_disk_sentinel_skipped_in_snapshot() {
        let stats = make_stats(vec![
            zero_device("/dev/mapper/braid-vda", 1),
            zero_missing_device(2),
        ]);
        let snapshot = snapshot_current(&stats, &[2]);
        // Present device is snapshotted normally
        assert!(snapshot.0.contains_key("1"));
        // Missing device comes from missing_devids, not the sentinel row
        let disk2 = snapshot.0.get("2").unwrap();
        assert!(disk2.missing_acked);
    }

    // --- merge_into_latch tests ---

    #[test]
    fn merge_live_causes_appended() {
        let live = vec![AlertCause::BtrfsDeviceErrors { devid: 1 }];
        let merged = merge_into_latch(None, &live);
        assert!(merged.active);
        assert_eq!(merged.causes.len(), 1);
    }

    #[test]
    fn merge_no_new_causes_carries_forward_latched() {
        let existing = AlertState {
            active: true,
            causes: vec![AlertCause::BtrfsDeviceErrors { devid: 1 }],
        };
        let merged = merge_into_latch(Some(&existing), &[]);
        assert!(merged.active);
        assert_eq!(merged.causes.len(), 1);
    }

    #[test]
    fn merge_live_same_devid_replaces_latched() {
        let existing = AlertState {
            active: true,
            causes: vec![AlertCause::BtrfsDeviceErrors { devid: 1 }],
        };
        let live = vec![AlertCause::BtrfsDeviceErrors { devid: 1 }];
        let merged = merge_into_latch(Some(&existing), &live);
        assert_eq!(merged.causes.len(), 1);
    }

    #[test]
    fn merge_live_missing_devid_preserves_latched() {
        // Key invariant fix: a previously-latched cause for devid 1 persists
        // even when live causes no longer include devid 1.
        let existing = AlertState {
            active: true,
            causes: vec![
                AlertCause::BtrfsDeviceErrors { devid: 1 },
                AlertCause::MissingDevice { devid: 2 },
            ],
        };
        // Live sources only detect devid 2 this cycle (devid 1 resolved)
        let live = vec![AlertCause::MissingDevice { devid: 2 }];
        let merged = merge_into_latch(Some(&existing), &live);
        assert_eq!(merged.causes.len(), 2);
        assert!(merged.active);
    }

    #[test]
    fn same_cause_key_btrfs_device_errors() {
        assert!(same_cause_key(
            &AlertCause::BtrfsDeviceErrors { devid: 1 },
            &AlertCause::BtrfsDeviceErrors { devid: 1 },
        ));
        assert!(!same_cause_key(
            &AlertCause::BtrfsDeviceErrors { devid: 1 },
            &AlertCause::BtrfsDeviceErrors { devid: 2 },
        ));
    }

    #[test]
    fn same_cause_key_missing_device() {
        assert!(same_cause_key(
            &AlertCause::MissingDevice { devid: 1 },
            &AlertCause::MissingDevice { devid: 1 },
        ));
        assert!(!same_cause_key(
            &AlertCause::MissingDevice { devid: 1 },
            &AlertCause::MissingDevice { devid: 2 },
        ));
    }

    #[test]
    fn same_cause_key_smartd_singleton() {
        assert!(same_cause_key(
            &AlertCause::SmartdAlert,
            &AlertCause::SmartdAlert
        ));
    }

    #[test]
    fn same_cause_key_computation_error_singleton() {
        assert!(same_cause_key(
            &AlertCause::ComputationError { detail: "a".into() },
            &AlertCause::ComputationError { detail: "b".into() },
        ));
    }

    #[test]
    fn same_cause_key_cross_variant_never_matches() {
        assert!(!same_cause_key(
            &AlertCause::BtrfsDeviceErrors { devid: 1 },
            &AlertCause::MissingDevice { devid: 1 },
        ));
        assert!(!same_cause_key(
            &AlertCause::SmartdAlert,
            &AlertCause::BtrfsDeviceErrors { devid: 1 },
        ));
    }

    #[test]
    fn acked_stats_roundtrip_via_state_paths() {
        let dir = tempfile::tempdir().unwrap();
        let paths = crate::state_paths::StatePaths::custom(dir.path().into());

        let mut map = BTreeMap::new();
        map.insert(
            "1".to_owned(),
            AckedDisk {
                missing_acked: false,
                device_stats: AckedDeviceCounters {
                    read_io_errs: 7,
                    ..Default::default()
                },
            },
        );
        let stats = AckedStats(map);
        save_acked_stats(&stats, &paths).unwrap();
        let reloaded = load_acked_stats(&paths);
        assert_eq!(reloaded, stats);
    }

    fn acked_disk(missing_acked: bool, read_io_errs: u64) -> AckedDisk {
        AckedDisk {
            missing_acked,
            device_stats: AckedDeviceCounters {
                read_io_errs,
                ..Default::default()
            },
        }
    }

    /*
     * Intent: drop_ghost_acked_for_devids removes only the requested devid
     * entries and persists the rewritten acked-stats file.
     *
     * Why it exists: add/remove cleanup must not rebuild ack state from live
     * counters or disturb unrelated acknowledgments. It only invalidates
     * baselines for devids whose pool ownership just changed.
     *
     * Scenario: a pool has acked baselines for devid 2 and 3, then `braid add`
     * learns btrfs assigned devid 2 to a fresh disk. The stale devid 2 entry
     * is deleted, while devid 3 stays acknowledged.
     */
    #[test]
    fn drop_ghost_acked_for_devids_removes_targets_only() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().into());
        let mut map = BTreeMap::new();
        map.insert("2".to_owned(), acked_disk(true, 7));
        map.insert("3".to_owned(), acked_disk(false, 11));
        save_acked_stats(&AckedStats(map), &paths).unwrap();

        let changed = drop_ghost_acked_for_devids(&paths, &[2]).unwrap();

        assert!(changed, "targeted cleanup must report a change");
        let reloaded = load_acked_stats(&paths);
        assert!(
            !reloaded.0.contains_key("2"),
            "stale target devid must be removed"
        );
        assert_eq!(
            reloaded.0.get("3"),
            Some(&acked_disk(false, 11)),
            "unrelated acked entry must survive"
        );
    }

    /*
     * Intent: drop_ghost_acked_for_devids returns Ok(false) without reading
     * the file when called with an empty devid list.
     *
     * Why it exists: command paths may have no newly assigned devids in edge
     * cases. The helper should be a cheap no-op and, importantly, must not
     * turn an unrelated corrupt file into a command failure when no cleanup
     * was requested.
     *
     * Scenario: a future caller passes an empty list while acked-stats.json
     * happens to contain invalid JSON. Since no devid boundary was crossed,
     * the helper does nothing.
     */
    #[test]
    fn drop_ghost_acked_for_empty_devid_list_does_not_read_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().into());
        std::fs::write(paths.acked_stats_json(), "not json").unwrap();

        let changed = drop_ghost_acked_for_devids(&paths, &[]).unwrap();

        assert!(!changed);
        assert_eq!(
            std::fs::read_to_string(paths.acked_stats_json()).unwrap(),
            "not json"
        );
    }

    /*
     * Intent: drop_ghost_acked_for_devids returns Ok(false) for a non-empty
     * devid list when acked-stats.json does not exist, and does not create
     * the file.
     *
     * Why it exists: command cleanup paths may run before any operator has
     * acknowledged alerts. Missing ack state is the empty state; cleanup must
     * not materialize a new on-disk file just because a boundary was checked.
     *
     * Scenario: `braid add` learns btrfs assigned devid 2 to a fresh disk on
     * a system that has never written acked-stats.json. The helper should be
     * a clean no-op.
     */
    #[test]
    fn drop_ghost_acked_for_devids_missing_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().into());

        let changed = drop_ghost_acked_for_devids(&paths, &[2]).unwrap();

        assert!(!changed, "missing file means no ack entry was dropped");
        assert!(
            !paths.acked_stats_json().exists(),
            "no-op cleanup must not create acked-stats.json"
        );
    }

    /*
     * Intent: drop_ghost_acked_for_devids returns Ok(false) and leaves the
     * file byte-identical when none of the requested devids are present.
     *
     * Why it exists: command cleanup must be narrowly scoped to the device IDs
     * whose ownership changed. A no-match cleanup should not rewrite the file,
     * reorder keys, reformat JSON, or otherwise disturb unrelated acks.
     *
     * Scenario: `braid remove` asks to drop devid 9, but the ack file only
     * contains devid 2 from an unrelated earlier acknowledgment.
     */
    #[test]
    fn drop_ghost_acked_for_devids_no_match_preserves_file_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().into());
        let original = br#"{
  "2": {
    "missing_acked": true,
    "device_stats": {
      "read_io_errs": 7,
      "write_io_errs": 0,
      "flush_io_errs": 0,
      "corruption_errs": 0,
      "generation_errs": 0
    }
  }
}
"#
        .to_vec();
        std::fs::write(paths.acked_stats_json(), &original).unwrap();

        let changed = drop_ghost_acked_for_devids(&paths, &[9]).unwrap();

        assert!(!changed, "no matching key means no persisted change");
        let after = std::fs::read(paths.acked_stats_json()).unwrap();
        assert_eq!(after, original, "no-match cleanup must not rewrite JSON");
    }

    /*
     * Intent: drop_ghost_acked_for_devids propagates corrupt acked-stats JSON
     * instead of treating it as empty state.
     *
     * Why it exists: `load_acked_stats` intentionally swallows corrupt files
     * for detector paths, but the add-time correctness boundary must either
     * prove a stale baseline was absent or fail loudly. Silent empty-state
     * fallback would let a ghost baseline suppress future alerts.
     *
     * Scenario: an old or manually edited acked-stats.json is invalid, then
     * `braid add` assigns a fresh disk to a possibly reused devid. Cleanup
     * must fail the command.
     */
    #[test]
    fn drop_ghost_acked_for_devids_rejects_corrupt_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().into());
        std::fs::write(paths.acked_stats_json(), "not json").unwrap();

        let err = drop_ghost_acked_for_devids(&paths, &[2]).unwrap_err();

        assert_eq!(err.kind(), std::io::ErrorKind::Other);
    }

    /*
     * Intent: remove_acked_stats deletes acked-stats.json and treats a missing
     * file as success.
     *
     * Why it exists: bootstrap creates a new pool identity, making every
     * pre-existing acked baseline stale. The cleanup primitive must support
     * both upgrades from old state and fresh state directories.
     *
     * Scenario: `braid add` bootstraps a new pool once with an old
     * acked-stats.json present, and later on a system where the file was never
     * created. Both paths should continue after cleanup.
     */
    #[test]
    fn remove_acked_stats_deletes_file_and_allows_missing() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().into());
        std::fs::write(paths.acked_stats_json(), "{}").unwrap();

        remove_acked_stats(&paths).unwrap();
        assert!(!paths.acked_stats_json().exists());

        remove_acked_stats(&paths).unwrap();
    }

    /*
     * Intent: reconcile_acked_stats prunes orphan devid entries, clears
     * missing_acked for present devices, and preserves unparsable keys.
     *
     * Why it exists: monitor is the defense-in-depth cleanup layer. It should
     * repair true orphans from crash/manual operations without deleting data
     * whose key format it does not understand.
     *
     * Scenario: monitor sees devid 1 present, devid 2 still missing, and no
     * devid 3 in the pool at all. It clears devid 1's old missing ack, keeps
     * devid 2, drops devid 3, and leaves a non-numeric key untouched.
     */
    #[test]
    fn reconcile_acked_stats_prunes_orphans_and_self_heals_present() {
        let mut map = BTreeMap::new();
        map.insert("1".to_owned(), acked_disk(true, 1));
        map.insert("2".to_owned(), acked_disk(true, 2));
        map.insert("3".to_owned(), acked_disk(false, 3));
        map.insert("legacy".to_owned(), acked_disk(true, 4));
        let mut acked = AckedStats(map);
        let still_relevant = BTreeSet::from([1, 2]);
        let present = BTreeSet::from([1]);

        let changed = reconcile_acked_stats(&mut acked, &still_relevant, &present);

        assert!(changed, "reconcile must report map mutation");
        assert_eq!(acked.0.get("1"), Some(&acked_disk(false, 1)));
        assert_eq!(acked.0.get("2"), Some(&acked_disk(true, 2)));
        assert!(!acked.0.contains_key("3"), "orphan devid must be pruned");
        assert!(
            acked.0.contains_key("legacy"),
            "unparsable keys must be preserved"
        );
    }

    /// Null-underlying device: btrfs device stats reports the mapper path
    /// for a hot-unplugged device whose LUKS mapper is still open. The
    /// row carries its devid directly, and that devid must also appear in
    /// the alert-local missing_devids so a MissingDevice cause fires.
    #[test]
    fn null_underlying_device_triggers_missing_alert() {
        // Device stats include both a healthy device and the null-underlying
        // device (btrfs still reports its mapper path)
        let stats = make_stats(vec![
            zero_device("/dev/mapper/braid-disk1", 1),
            zero_device("/dev/mapper/braid-disk2", 2),
        ]);
        let acked = AckedStats::default();
        // Alert-local missing devids includes the null-underlying device's devid
        let alert_missing = vec![2u64];
        let alert = compute_alert_state(&stats, &acked, &alert_missing, false);
        assert!(alert.active);
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(alert.causes[0], AlertCause::MissingDevice { devid: 2 });
    }
}
