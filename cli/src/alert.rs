use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::parse::types::{BtrfsDeviceStatsOutput, DeviceErrorStats};
use crate::probe::AlertDevids;
use crate::state_io::atomic_write;
use crate::state_paths::StatePaths;
use crate::types::Devid;

// ---------------------------------------------------------------------------
// Alert model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AlertState {
    pub causes: Vec<AlertCause>,
}

impl AlertState {
    pub fn active(&self) -> bool {
        !self.causes.is_empty()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AlertCause {
    BtrfsDeviceErrors { devid: Devid },
    MissingDevice { devid: Devid },
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
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

/// Lossy loader: swallows both `NotFound` and parse errors into
/// `AckedStats::default()`. Use only for test reload assertions or
/// strictly read-only inspection. Production mutation paths must use
/// `load_acked_stats_fallible` so corruption surfaces as
/// `ComputationError` per ADR 014 (`docs/design/decisions/014-alerts.md:74`).
pub fn load_acked_stats(paths: &StatePaths) -> AckedStats {
    load_acked_stats_at(&paths.acked_stats_json())
}

/// Path-based form of the lossy loader; keep production mutation paths on
/// `load_acked_stats_fallible` so corrupt state cannot be treated as empty.
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
/// named alert-devid carrier.
///
/// Identity is `dev.devid` (btrfs supplies it on every stats row). The
/// alert-local missing set is skipped for `BtrfsDeviceErrors` because those
/// devids alert through `MissingDevice`, and rows outside `devids.recognized`
/// are ignored as stale identities.
pub fn compute_alert_state(
    current_stats: &BtrfsDeviceStatsOutput,
    acked: &AckedStats,
    devids: &AlertDevids,
    smartd_alert_active: bool,
) -> AlertState {
    let mut causes = Vec::new();
    let recognized: BTreeSet<Devid> = devids.recognized.iter().copied().collect();
    let missing: BTreeSet<Devid> = devids.missing.iter().copied().collect();

    for dev in &current_stats.devices {
        if missing.contains(&dev.devid) {
            continue;
        }
        if !recognized.contains(&dev.devid) {
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

    for &devid in &devids.missing {
        let key = devid.to_string();
        let missing_acked = acked.0.get(&key).map(|d| d.missing_acked).unwrap_or(false);
        if !missing_acked {
            causes.push(AlertCause::MissingDevice { devid });
        }
    }

    if smartd_alert_active {
        causes.push(AlertCause::SmartdAlert);
    }

    AlertState { causes }
}

/// Alert when `current` exceeds the acked baseline, treating a baseline
/// *above* `current` as 0 so braid fails loud instead of suppressing.
///
/// btrfs device-stats counters are persistent and monotonic -- reset only by
/// `btrfs device stats -z`, which braid never runs -- so a current value below
/// the ack baseline is not a comparable post-ack counter value. It means the
/// baseline belongs to a different counter stream: either a reused devid
/// inherited a ghost baseline before add/recover cleanup dropped its acked entry
/// (ADR 014), or an operator reset the live counters with `-z`. Treat the
/// baseline as 0 so any nonzero current still alerts.
fn exceeds_acked(current: u64, acked: u64) -> bool {
    current > if current < acked { 0 } else { acked }
}

fn has_new_errors(current: &DeviceErrorStats, acked: Option<&AckedDeviceCounters>) -> bool {
    let zero = AckedDeviceCounters::default();
    let a = acked.unwrap_or(&zero);
    exceeds_acked(current.read_io_errs, a.read_io_errs)
        || exceeds_acked(current.write_io_errs, a.write_io_errs)
        || exceeds_acked(current.flush_io_errs, a.flush_io_errs)
        || exceeds_acked(current.corruption_errs, a.corruption_errs)
        || exceeds_acked(current.generation_errs, a.generation_errs)
}

// ---------------------------------------------------------------------------
// Snapshot current state for ack
// ---------------------------------------------------------------------------

pub fn snapshot_current(
    current_stats: &BtrfsDeviceStatsOutput,
    devids: &AlertDevids,
) -> AckedStats {
    let mut map = BTreeMap::new();
    let recognized: BTreeSet<Devid> = devids.recognized.iter().copied().collect();

    for dev in &current_stats.devices {
        if !recognized.contains(&dev.devid) {
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

    // Missing devices get missing_acked = true. Preserve any existing
    // device_stats snapshot for null-underlying devices that also appeared
    // in stats above.
    for &devid in &devids.missing {
        map.entry(devid.to_string()).or_default().missing_acked = true;
    }

    AckedStats(map)
}

/// Drop the acked entry for `devid`. Returns true if an entry was removed.
pub fn drop_acked_devid(acked: &mut AckedStats, devid: Devid) -> bool {
    acked.0.remove(&devid.to_string()).is_some()
}

pub fn load_acked_stats_fallible(paths: &StatePaths) -> Result<AckedStats, std::io::Error> {
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
    devids: &[Devid],
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
    still_relevant: &BTreeSet<Devid>,
    present: &BTreeSet<Devid>,
) -> bool {
    let mut changed = false;
    acked.0.retain(|key, disk| {
        let Ok(devid) = key.parse::<u64>().map(Devid::new) else {
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
///
/// Treats only a regular file at the path as an active alert: a directory or
/// other non-file is ignored so a stray inode cannot wedge `braid ack` (whose
/// cleanup uses `remove_file`).
pub fn smartd_alert_active(paths: &StatePaths) -> bool {
    paths
        .smartd_alert()
        .metadata()
        .map(|m| m.is_file())
        .unwrap_or(false)
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
            let parse_detail = e.to_string();
            eprintln!("warning: alert latch unreadable -- quarantining: {parse_detail}");
            let detail = match quarantine_corrupt_latch(paths) {
                Some(quarantine_detail) => format!("{parse_detail}; {quarantine_detail}"),
                None => parse_detail,
            };
            (None, Some(detail))
        }
    }
}

/// Preserve the first unreadable latch sidecar without clobbering prior bytes.
///
/// The hard-link step is the atomic no-clobber primitive: it fails with
/// AlreadyExists if the sidecar path is already occupied, avoiding an
/// exists-then-rename race that could replace the original forensic bytes.
fn quarantine_corrupt_latch(paths: &StatePaths) -> Option<String> {
    let src = paths.alert_latch_json();
    let dst = paths.alert_latch_corrupt();
    match std::fs::hard_link(&src, &dst) {
        Ok(()) => match std::fs::remove_file(&src) {
            Ok(()) => None,
            Err(e) => Some(format!(
                "quarantined corrupt latch but failed to remove source: {e}"
            )),
        },
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Some(
            "prior alert-latch.json.corrupt sidecar exists -- new corrupt bytes were not separately preserved".to_string(),
        ),
        Err(e) => Some(format!("failed to quarantine corrupt latch: {e}")),
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

/// Check if ack left a regular cleanup-pending sentinel on disk.
///
/// Non-regular inodes are not treated as pending state: cleanup will still
/// attempt to mark the sentinel and surface the underlying I/O error.
pub fn alert_cleanup_pending(paths: &StatePaths) -> bool {
    paths
        .alert_cleanup_pending()
        .metadata()
        .map(|m| m.is_file())
        .unwrap_or(false)
}

/// Mark that ack cleanup has started but not yet completed.
///
/// Existing regular sentinels are accepted without reopening for write so a
/// marker that already drives retry is not re-wedged by later permission drift.
pub fn mark_alert_cleanup_pending(paths: &StatePaths) -> Result<(), std::io::Error> {
    let path = paths.alert_cleanup_pending();
    if path.is_file() {
        return Ok(());
    }
    match path.symlink_metadata() {
        Ok(_) => {
            return Err(std::io::Error::other(format!(
                "alert cleanup pending path is not a regular file: {}",
                path.display()
            )));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e),
    }

    std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(path)
        .map(|_| ())
}

/// Clear the cleanup-pending sentinel. Absence already means no pending work.
pub fn clear_alert_cleanup_pending(paths: &StatePaths) -> Result<(), std::io::Error> {
    match std::fs::remove_file(paths.alert_cleanup_pending()) {
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
/// 3. Result: `AlertState { causes }` (active derived from causes).
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

    AlertState { causes }
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

    fn zero_device(devid: Devid) -> DeviceErrorStats {
        DeviceErrorStats {
            devid,
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

    // Intent: load_alert_latch returns Err(LatchLoadError::Read) when the
    //   latch path exists but cannot be read as a regular file.
    // Why it exists: ADR 014 requires callers to distinguish absent latch,
    //   filesystem read failure, and parse failure. A directory at the latch
    //   path is a root-independent non-NotFound I/O failure that must not be
    //   folded into Parse or Ok(None).
    // Scenario: filesystem damage or external tampering leaves a directory
    //   where alert-latch.json should be. Callers must receive the Read
    //   variant and apply their own fail-closed policy.
    #[test]
    fn load_alert_latch_directory_returns_read_err() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("latch-as-dir");
        std::fs::create_dir(&path).unwrap();

        let result = load_alert_latch_at(&path);

        assert!(
            matches!(result, Err(LatchLoadError::Read(_))),
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
            causes: vec![AlertCause::MissingDevice {
                devid: Devid::new(7),
            }],
        };
        std::fs::write(&path, serde_json::to_string(&original).unwrap()).unwrap();
        let result = load_alert_latch_at(&path).unwrap();
        assert_eq!(result, Some(original));
    }

    /*
     * Intent: load_alert_latch_at parses a JSON latch that contains a legacy
     * "active" key alongside causes, returning Ok(Some(state)) with the
     * preserved causes; AlertState::active() is then derived from causes
     * regardless of what the legacy "active" value was on disk.
     *
     * Why it exists: pre-refactor latches written to
     * /var/lib/braid/alert-latch.json by an older binary still need to load
     * cleanly post-refactor. The refactor relies on serde ignoring the legacy
     * key because AlertState has no deny_unknown_fields. Without this test, a
     * later strictness change could regress every legacy on-disk latch into the
     * corrupt-latch quarantine path on next monitor cycle.
     *
     * Scenario: a NAS upgrades the braid binary across this refactor with a
     * latch from the prior version still on disk. Next monitor/status/ack
     * invocation loads the legacy-shaped JSON; load_alert_latch_at must accept
     * it, and the resulting AlertState must report active() based on its causes
     * vec, not on whatever "active" value the legacy file carried.
     */
    #[test]
    fn load_alert_latch_accepts_legacy_active_key() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("legacy-latch.json");
        let legacy = br#"{"active":true,"causes":[{"type":"missing_device","devid":7}]}"#;
        std::fs::write(&path, legacy).unwrap();

        let state = load_alert_latch_at(&path)
            .unwrap()
            .expect("legacy latch must parse");
        assert_eq!(
            state.causes,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(7)
            }],
            "causes must round-trip from legacy JSON"
        );
        assert!(
            state.active(),
            "active() must be derived from causes (true here, non-empty causes)"
        );

        let path2 = dir.path().join("legacy-latch-empty.json");
        let legacy_empty = br#"{"active":true,"causes":[]}"#;
        std::fs::write(&path2, legacy_empty).unwrap();
        let state2 = load_alert_latch_at(&path2)
            .unwrap()
            .expect("legacy empty latch must parse");
        assert!(state2.causes.is_empty());
        assert!(
            !state2.active(),
            "active() must follow causes, not the legacy active field on disk"
        );
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

    // Intent: When the latch corrupts again before ack, quarantine preserves
    // the first alert-latch.json.corrupt sidecar and reports that later bytes
    // were not separately captured.
    // Why it exists: Unix rename replaces an existing destination, so the old
    // quarantine path could destroy the original and most useful corruption
    // snapshot on a second monitor cycle.
    // Scenario: The latch is quarantined once, braid later writes a valid
    // latch, and external damage corrupts that latch again before the
    // operator runs braid ack.
    #[test]
    fn quarantine_preserves_first_corrupt_sidecar() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().to_path_buf());
        let first = b"first garbage".to_vec();
        std::fs::write(paths.alert_latch_json(), &first).unwrap();

        let (state, detail) = load_alert_latch_or_quarantine(&paths);

        assert!(state.is_none());
        let detail = detail.expect("first quarantine reports parse detail");
        assert!(
            detail.contains("parse alert latch"),
            "first detail should name parse failure, got {detail:?}"
        );
        assert!(
            !detail.contains("; "),
            "first detail should not include quarantine suffix, got {detail:?}"
        );
        assert!(
            !paths.alert_latch_json().exists(),
            "live latch path must be removed after first quarantine"
        );
        let preserved = std::fs::read(paths.alert_latch_corrupt())
            .expect("corrupt sidecar must exist after first quarantine");
        assert_eq!(preserved, first, "sidecar must hold first bytes");

        let second = b"second garbage".to_vec();
        std::fs::write(paths.alert_latch_json(), &second).unwrap();

        let (state, detail) = load_alert_latch_or_quarantine(&paths);

        assert!(state.is_none());
        let detail = detail.expect("second quarantine reports parse and sidecar detail");
        assert!(
            detail.contains("parse alert latch"),
            "second detail should name parse failure, got {detail:?}"
        );
        assert!(
            detail.contains("prior alert-latch.json.corrupt sidecar exists"),
            "second detail should name prior sidecar preservation, got {detail:?}"
        );
        let preserved = std::fs::read(paths.alert_latch_corrupt())
            .expect("first corrupt sidecar must remain present");
        assert_eq!(
            preserved, first,
            "second quarantine must not replace first sidecar"
        );
        let live = std::fs::read(paths.alert_latch_json())
            .expect("second corrupt latch remains for caller overwrite");
        assert_eq!(live, second, "second bytes should remain at live path");
    }

    // Intent: smartd_alert_active treats only a regular file at the flag path
    //   as an active alert source. Absent paths and directories are false; a
    //   symlink resolving to a regular file is true (matches the smartd hook's
    //   `touch` output, including symlink-on-tmpfs deployments).
    // Why it exists: prior behavior used Path::exists(), which counted any
    //   inode -- including a directory -- as an active alert. The subsequent
    //   cleanup (remove_smartd_alert_flag) calls remove_file, which fails on a
    //   directory, so `braid ack` was permanently wedged behind
    //   AckError::CleanupFailed any time a non-file ended up at the flag path.
    //   This test fails loudly on a regression back to Path::exists().
    // Scenario: test scaffolding, a manual operator mistake, or a future hook
    //   bug leaves a directory at /var/lib/braid/smartd-alert.
    //   smartd_alert_active must report false so the ack cleanup chain does not
    //   try to remove_file the directory and wedge subsequent `braid ack`
    //   invocations.
    #[cfg(unix)]
    #[test]
    fn smartd_alert_active_requires_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().to_path_buf());

        assert!(!smartd_alert_active(&paths), "absent path must be false");

        std::fs::write(paths.smartd_alert(), b"").unwrap();
        assert!(
            smartd_alert_active(&paths),
            "regular file must be true (matches smartd hook `touch` output)"
        );
        std::fs::remove_file(paths.smartd_alert()).unwrap();

        std::fs::create_dir(paths.smartd_alert()).unwrap();
        assert!(
            !smartd_alert_active(&paths),
            "directory must be false (regression guard for Path::exists revert)"
        );
        std::fs::remove_dir(paths.smartd_alert()).unwrap();

        let target = dir.path().join("real-flag");
        std::fs::write(&target, b"").unwrap();
        std::os::unix::fs::symlink(&target, paths.smartd_alert()).unwrap();
        assert!(
            smartd_alert_active(&paths),
            "symlink resolving to a regular file must be true"
        );
    }

    // Intent: mark_alert_cleanup_pending creates a regular sentinel file when
    //   none exists.
    // Why it exists: ack cleanup relies on this file as the retry signal after
    //   any later cleanup step fails.
    // Scenario: cleanup starts after a mounted or offline ack has persisted
    //   its ack state, and no previous cleanup attempt left a marker.
    #[test]
    fn mark_alert_cleanup_pending_creates_file_when_absent() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().to_path_buf());

        mark_alert_cleanup_pending(&paths).unwrap();

        assert!(
            paths.alert_cleanup_pending().is_file(),
            "cleanup marker must be a regular file"
        );
    }

    // Intent: mark_alert_cleanup_pending is idempotent when the sentinel is
    //   already a regular file.
    // Why it exists: retry cleanup calls mark again before sweeping leftover
    //   alert files; an existing marker must keep its role as the retry signal.
    // Scenario: the first ack failed after marker creation, leaving the marker
    //   on disk. The next ack resumes cleanup and reaches mark again.
    #[test]
    fn mark_alert_cleanup_pending_is_idempotent_for_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().to_path_buf());
        std::fs::write(paths.alert_cleanup_pending(), b"already pending").unwrap();

        mark_alert_cleanup_pending(&paths).unwrap();

        let bytes = std::fs::read(paths.alert_cleanup_pending()).unwrap();
        assert_eq!(
            bytes, b"already pending",
            "existing sentinel bytes must not be truncated"
        );
    }

    // Intent: mark_alert_cleanup_pending does not reopen an existing regular
    //   sentinel for write, even when that file is read-only.
    // Why it exists: the marker may already be the only retry signal after a
    //   prior cleanup failure; permission drift on that file must not re-wedge
    //   the next cleanup attempt before removals can resume.
    // Scenario: cleanup failed after marker creation, then the marker's mode
    //   drifted to read-only before the operator reran `braid ack`.
    #[cfg(unix)]
    #[test]
    fn mark_alert_cleanup_pending_existing_read_only_file_does_not_require_write_permission() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().to_path_buf());
        std::fs::write(paths.alert_cleanup_pending(), b"already pending").unwrap();
        std::fs::set_permissions(
            paths.alert_cleanup_pending(),
            std::fs::Permissions::from_mode(0o400),
        )
        .unwrap();

        mark_alert_cleanup_pending(&paths).unwrap();

        let bytes = std::fs::read(paths.alert_cleanup_pending()).unwrap();
        assert_eq!(
            bytes, b"already pending",
            "read-only existing sentinel must stay untouched"
        );
        std::fs::set_permissions(
            paths.alert_cleanup_pending(),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
    }

    // Intent: alert_cleanup_pending treats only a regular file sentinel as
    //   pending cleanup state.
    // Why it exists: a poison directory at the marker path should not satisfy
    //   the ack retry gate; cleanup must try mark_alert_cleanup_pending and
    //   surface the I/O error instead.
    // Scenario: manual operator error or a previous bug leaves a directory at
    //   /var/lib/braid/alert-cleanup-pending.
    #[cfg(unix)]
    #[test]
    fn alert_cleanup_pending_requires_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().to_path_buf());

        assert!(
            !alert_cleanup_pending(&paths),
            "absent marker must not count as pending"
        );

        std::fs::create_dir(paths.alert_cleanup_pending()).unwrap();

        assert!(
            !alert_cleanup_pending(&paths),
            "directory marker must not count as pending"
        );
    }

    // Intent: clear_alert_cleanup_pending succeeds when the sentinel is
    //   already absent.
    // Why it exists: retry cleanup uses NotFound-tolerant removals throughout
    //   so partial prior cleanup can converge after the original I/O fault is
    //   fixed.
    // Scenario: cleanup reaches the final clear step after a prior manual
    //   operator removal, or after a retry where the marker was already gone.
    #[test]
    fn clear_alert_cleanup_pending_is_not_found_tolerant() {
        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().to_path_buf());

        clear_alert_cleanup_pending(&paths).unwrap();
    }

    // Intent: A hard_link I/O failure during quarantine is folded into the
    // returned detail instead of being silently dropped.
    // Why it exists: If quarantine cannot create the sidecar, the caller's
    // next save_alert_latch overwrites the bad bytes at alert-latch.json, so
    // the operator needs a visible lost-evidence signal.
    // Scenario: The state directory is readable but not writable when monitor
    // encounters an unreadable latch.
    #[cfg(unix)]
    #[test]
    fn quarantine_link_failure_surfaces_in_detail() {
        use std::os::unix::fs::PermissionsExt;

        struct RestorePerms {
            dir: std::path::PathBuf,
        }

        impl Drop for RestorePerms {
            fn drop(&mut self) {
                let _ = std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700));
            }
        }

        let dir = tempfile::tempdir().unwrap();
        let paths = StatePaths::custom(dir.path().to_path_buf());
        let garbage = b"not json".to_vec();
        std::fs::write(paths.alert_latch_json(), &garbage).unwrap();
        let _restore = RestorePerms {
            dir: dir.path().to_path_buf(),
        };
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o500)).unwrap();

        let (state, detail) = load_alert_latch_or_quarantine(&paths);

        assert!(state.is_none());
        let detail = detail.expect("failed quarantine reports detail");
        assert!(
            detail.contains("failed to quarantine corrupt latch"),
            "detail should name quarantine failure, got {detail:?}"
        );
        let live = std::fs::read(paths.alert_latch_json())
            .expect("hard_link failure must leave corrupt latch in place");
        assert_eq!(live, garbage, "corrupt latch must remain at live path");
    }

    #[test]
    fn no_alert_when_all_zero() {
        let stats = make_stats(vec![zero_device(Devid::new(1))]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![Devid::new(1)],
                missing: vec![],
            },
            false,
        );
        assert!(!alert.active());
        assert!(alert.causes.is_empty());
    }

    #[test]
    fn alert_on_btrfs_device_errors() {
        let mut dev = zero_device(Devid::new(1));
        dev.read_io_errs = 3;
        dev.corruption_errs = 1;
        let stats = make_stats(vec![dev]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![Devid::new(1)],
                missing: vec![],
            },
            false,
        );
        assert!(alert.active());
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(
            alert.causes[0],
            AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1)
            }
        );
    }

    #[test]
    fn alert_on_missing_device() {
        let stats = make_stats(vec![zero_device(Devid::new(1))]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![Devid::new(1), Devid::new(2)],
                missing: vec![Devid::new(2)],
            },
            false,
        );
        assert!(alert.active());
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(
            alert.causes[0],
            AlertCause::MissingDevice {
                devid: Devid::new(2)
            }
        );
    }

    #[test]
    fn alert_on_smartd() {
        let stats = make_stats(vec![zero_device(Devid::new(1))]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![Devid::new(1)],
                missing: vec![],
            },
            true,
        );
        assert!(alert.active());
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(alert.causes[0], AlertCause::SmartdAlert);
    }

    #[test]
    fn no_alert_after_ack() {
        let mut dev = zero_device(Devid::new(1));
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
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![Devid::new(1)],
                missing: vec![],
            },
            false,
        );
        assert!(!alert.active());
    }

    // Intent: when the acked baseline is higher than the current counter,
    //   exceeds_acked treats the baseline as 0 and alerts on any nonzero current
    //   rather than suppressing.
    // Why it exists: btrfs device-stats counters are persistent and monotonic,
    //   so a current value below the ack baseline is not a comparable post-ack
    //   counter value -- the baseline belongs to a different counter stream (a
    //   reused-devid ghost baseline before add/recover cleanup, or a manual
    //   `-z`). This pins the fail-loud behavior so a future "simplify to
    //   current > acked" change, which would silently suppress a later nonzero
    //   counter, fails here.
    // Scenario: an add reused devid 1 (last_devid+1) and crashed before
    //   drop_ghost_acked_for_devids ran, so the acked baseline still reads
    //   read_io_errs=5 from the prior holder. A monitor cycle runs before
    //   recover sweeps it, and the fresh disk has already logged 1 read error.
    #[test]
    fn stale_high_baseline_does_not_suppress_alert() {
        let mut dev = zero_device(Devid::new(1));
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
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![Devid::new(1)],
                missing: vec![],
            },
            false,
        );
        assert!(alert.active(), "stale-high baseline should trigger alert");
    }

    #[test]
    fn missing_acked_suppresses_alert() {
        let stats = make_stats(vec![zero_device(Devid::new(1))]);
        let mut acked_map = BTreeMap::new();
        acked_map.insert(
            "2".to_owned(),
            AckedDisk {
                missing_acked: true,
                device_stats: AckedDeviceCounters::default(),
            },
        );
        let acked = AckedStats(acked_map);
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![Devid::new(1), Devid::new(2)],
                missing: vec![Devid::new(2)],
            },
            false,
        );
        assert!(!alert.active(), "acked missing should not trigger alert");
    }

    #[test]
    fn multiple_causes() {
        let mut dev = zero_device(Devid::new(1));
        dev.write_io_errs = 1;
        let stats = make_stats(vec![dev]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![Devid::new(1), Devid::new(2)],
                missing: vec![Devid::new(2)],
            },
            true,
        );
        assert!(alert.active());
        assert_eq!(alert.causes.len(), 3);
    }

    #[test]
    fn snapshot_current_captures_stats() {
        let mut dev = zero_device(Devid::new(1));
        dev.read_io_errs = 3;
        dev.corruption_errs = 1;
        let stats = make_stats(vec![dev]);
        let snapshot = snapshot_current(
            &stats,
            &AlertDevids {
                recognized: vec![Devid::new(1), Devid::new(2)],
                missing: vec![Devid::new(2)],
            },
        );

        let disk1 = snapshot.0.get("1").unwrap();
        assert!(!disk1.missing_acked);
        assert_eq!(disk1.device_stats.read_io_errs, 3);
        assert_eq!(disk1.device_stats.corruption_errs, 1);

        let disk2 = snapshot.0.get("2").unwrap();
        assert!(disk2.missing_acked);
    }

    // Intent: snapshot_current marks a null-underlying devid as
    // missing-acked while preserving the stats row counters for that devid.
    // Why it exists: the missing-devid pass must update the existing snapshot
    // entry, not overwrite it with default counters.
    // Scenario: btrfs still reports the mapper for devid 2 after hot-unplug,
    // while probe also classifies devid 2 as alert-local missing.
    #[test]
    fn snapshot_current_preserves_null_underlying_stats() {
        let mut dev = zero_device(Devid::new(2));
        dev.read_io_errs = 3;
        dev.write_io_errs = 4;
        dev.flush_io_errs = 5;
        dev.corruption_errs = 6;
        dev.generation_errs = 7;
        let stats = make_stats(vec![dev]);

        let snapshot = snapshot_current(
            &stats,
            &AlertDevids {
                recognized: vec![Devid::new(2)],
                missing: vec![Devid::new(2)],
            },
        );

        let disk2 = snapshot.0.get("2").unwrap();
        assert!(disk2.missing_acked);
        assert_eq!(disk2.device_stats.read_io_errs, 3);
        assert_eq!(disk2.device_stats.write_io_errs, 4);
        assert_eq!(disk2.device_stats.flush_io_errs, 5);
        assert_eq!(disk2.device_stats.corruption_errs, 6);
        assert_eq!(disk2.device_stats.generation_errs, 7);
    }

    #[test]
    fn new_errors_after_ack_trigger_alert() {
        let mut dev = zero_device(Devid::new(1));
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
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![Devid::new(1)],
                missing: vec![],
            },
            false,
        );
        assert!(
            alert.active(),
            "new errors above acked baseline should trigger alert"
        );
    }

    /*
     * Intent: a recognized stats row with no acked baseline produces no
     * BtrfsDeviceErrors cause when its counters are zero. This is the
     * positive replacement for the deleted `unmapped_device_is_error_in_alert`
     * test.
     *
     * Why it exists: with devid as the btrfs stats row key, an "orphan" path
     * is no longer a fail-closed condition. A recognized row can still have no
     * acked baseline (acked.0.get("99") is None) and zero counters, so
     * has_new_errors returns false.
     */
    #[test]
    fn unknown_devid_zero_counters_does_not_alert() {
        let stats = make_stats(vec![zero_device(Devid::new(99))]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![Devid::new(99)],
                missing: vec![],
            },
            false,
        );
        assert!(!alert.active());
        assert!(alert.causes.is_empty());
    }

    // Intent: an unrecognized stats row cannot emit BtrfsDeviceErrors, even
    //   when its counters are non-zero.
    // Why it exists: stale btrfs stats rows outside the current pool
    //   membership used to latch alerts that ack/reconcile could never clear.
    // Scenario: a lingering devid 99 row carries read and corruption errors,
    //   while the current probe recognizes no such devid.
    #[test]
    fn unrecognized_devid_with_errors_does_not_alert() {
        let mut dev = zero_device(Devid::new(99));
        dev.read_io_errs = 3;
        dev.corruption_errs = 1;
        let stats = make_stats(vec![dev]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![],
                missing: vec![],
            },
            false,
        );
        assert!(!alert.active());
        assert!(alert.causes.is_empty());
    }

    // Intent: a missing devid's stats row never produces BtrfsDeviceErrors,
    //   even with non-zero counters -- it alerts solely via MissingDevice.
    // Why it exists: the skip used to key on the btrfs device path string; a
    //   version-drifted string could fall through and fire a spurious
    //   BtrfsDeviceErrors on top of MissingDevice.
    // Scenario: degraded pool, devid 2 missing, btrfs reports devid 2's
    //   persisted read and corruption counters as non-zero on its stats row.
    #[test]
    fn missing_devid_with_errors_alerts_only_as_missing_device() {
        let mut dev = zero_device(Devid::new(2));
        dev.read_io_errs = 3;
        dev.corruption_errs = 1;
        let stats = make_stats(vec![zero_device(Devid::new(1)), dev]);
        let acked = AckedStats::default();

        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![Devid::new(1), Devid::new(2)],
                missing: vec![Devid::new(2)],
            },
            false,
        );

        assert_eq!(
            alert.causes,
            vec![AlertCause::MissingDevice {
                devid: Devid::new(2)
            }]
        );
    }

    // Intent: a missing devid's stats row never produces
    //   BtrfsDeviceErrors, even when btrfs emits a normal-looking row.
    // Why it exists: missing-device alerting must key on `missing_devids`,
    //   not on the unpinned btrfs device string.
    // Scenario: degraded pool, devid 2 missing, btrfs still reports a stats
    //   row for devid 2.
    #[test]
    fn missing_devid_row_skipped_in_alert() {
        let stats = make_stats(vec![zero_device(Devid::new(1)), zero_device(Devid::new(2))]);
        let acked = AckedStats::default();
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: vec![Devid::new(1), Devid::new(2)],
                missing: vec![Devid::new(2)],
            },
            false,
        );
        assert!(alert.active());
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(
            alert.causes[0],
            AlertCause::MissingDevice {
                devid: Devid::new(2)
            }
        );
    }

    // Intent: a missing devid's stats row is still snapshotted by devid while
    //   the missing flag is layered on top from `missing_devids`.
    // Why it exists: ack baselines must preserve counters for missing or
    //   null-underlying rows so a returning member does not re-alert on old
    //   counts.
    // Scenario: degraded pool, devid 2 missing, btrfs reports persisted
    //   counters for devid 2 in its stats row.
    #[test]
    fn missing_devid_row_snapshotted_and_marked_missing_acked() {
        let mut dev = zero_device(Devid::new(2));
        dev.read_io_errs = 3;
        let stats = make_stats(vec![zero_device(Devid::new(1)), dev]);
        let snapshot = snapshot_current(
            &stats,
            &AlertDevids {
                recognized: vec![Devid::new(1), Devid::new(2)],
                missing: vec![Devid::new(2)],
            },
        );
        assert!(snapshot.0.contains_key("1"));
        let disk2 = snapshot.0.get("2").unwrap();
        assert!(disk2.missing_acked);
        assert_eq!(disk2.device_stats.read_io_errs, 3);
    }

    // --- merge_into_latch tests ---

    #[test]
    fn merge_live_causes_appended() {
        let live = vec![AlertCause::BtrfsDeviceErrors {
            devid: Devid::new(1),
        }];
        let merged = merge_into_latch(None, &live);
        assert!(merged.active());
        assert_eq!(merged.causes.len(), 1);
    }

    #[test]
    fn merge_no_new_causes_carries_forward_latched() {
        let existing = AlertState {
            causes: vec![AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1),
            }],
        };
        let merged = merge_into_latch(Some(&existing), &[]);
        assert!(merged.active());
        assert_eq!(merged.causes.len(), 1);
    }

    #[test]
    fn merge_live_same_devid_replaces_latched() {
        let existing = AlertState {
            causes: vec![AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1),
            }],
        };
        let live = vec![AlertCause::BtrfsDeviceErrors {
            devid: Devid::new(1),
        }];
        let merged = merge_into_latch(Some(&existing), &live);
        assert_eq!(merged.causes.len(), 1);
    }

    #[test]
    fn merge_live_missing_devid_preserves_latched() {
        // Key invariant fix: a previously-latched cause for devid 1 persists
        // even when live causes no longer include devid 1.
        let existing = AlertState {
            causes: vec![
                AlertCause::BtrfsDeviceErrors {
                    devid: Devid::new(1),
                },
                AlertCause::MissingDevice {
                    devid: Devid::new(2),
                },
            ],
        };
        // Live sources only detect devid 2 this cycle (devid 1 resolved)
        let live = vec![AlertCause::MissingDevice {
            devid: Devid::new(2),
        }];
        let merged = merge_into_latch(Some(&existing), &live);
        assert_eq!(merged.causes.len(), 2);
        assert!(merged.active());
    }

    #[test]
    fn same_cause_key_btrfs_device_errors() {
        assert!(same_cause_key(
            &AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1)
            },
            &AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1)
            },
        ));
        assert!(!same_cause_key(
            &AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1)
            },
            &AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(2)
            },
        ));
    }

    #[test]
    fn same_cause_key_missing_device() {
        assert!(same_cause_key(
            &AlertCause::MissingDevice {
                devid: Devid::new(1)
            },
            &AlertCause::MissingDevice {
                devid: Devid::new(1)
            },
        ));
        assert!(!same_cause_key(
            &AlertCause::MissingDevice {
                devid: Devid::new(1)
            },
            &AlertCause::MissingDevice {
                devid: Devid::new(2)
            },
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
            &AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1)
            },
            &AlertCause::MissingDevice {
                devid: Devid::new(1)
            },
        ));
        assert!(!same_cause_key(
            &AlertCause::SmartdAlert,
            &AlertCause::BtrfsDeviceErrors {
                devid: Devid::new(1)
            },
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

        let changed = drop_ghost_acked_for_devids(&paths, &[Devid::new(2)]).unwrap();

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

        let changed = drop_ghost_acked_for_devids(&paths, &[Devid::new(2)]).unwrap();

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

        let changed = drop_ghost_acked_for_devids(&paths, &[Devid::new(9)]).unwrap();

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

        let err = drop_ghost_acked_for_devids(&paths, &[Devid::new(2)]).unwrap_err();

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
        let still_relevant = BTreeSet::from([Devid::new(1), Devid::new(2)]);
        let present = BTreeSet::from([Devid::new(1)]);

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

    // Intent: AlertState serializes AlertCause variants with bare-integer devid
    //   values, and round-trips back to the original value.
    // Why it exists: Devid is #[serde(transparent)], so its JSON representation
    //   must be a bare number ("devid":7, not "devid":{"0":7} or similar).
    //   An exact-substring assertion pins this invariant so a future serde
    //   attribute change that switches to a wrapped form would fail here instead
    //   of silently making alert-latch.json unreadable by older binaries or
    //   braid-status parsing. The legacy-key test (load_alert_latch_accepts_
    //   legacy_active_key) already asserts {"type":"missing_device","devid":7}
    //   parses; this test asserts what the serializer emits.
    // Scenario: monitor writes a latch with two causes; the on-disk bytes must
    //   contain the exact shape the format contract specifies.
    #[test]
    fn alert_state_json_shape_bare_integer_devid() {
        let state = AlertState {
            causes: vec![
                AlertCause::MissingDevice {
                    devid: Devid::new(7),
                },
                AlertCause::BtrfsDeviceErrors {
                    devid: Devid::new(2),
                },
            ],
        };

        let json = serde_json::to_string(&state).expect("serialization must succeed");

        assert!(
            json.contains(r#"{"type":"missing_device","devid":7}"#),
            "MissingDevice cause must serialize devid as a bare integer; got: {json}"
        );
        assert!(
            json.contains(r#"{"type":"btrfs_device_errors","devid":2}"#),
            "BtrfsDeviceErrors cause must serialize devid as a bare integer; got: {json}"
        );

        let back: AlertState =
            serde_json::from_str(&json).expect("deserialization must round-trip");
        assert_eq!(back, state, "round-tripped AlertState must equal original");
    }

    /// Null-underlying device: btrfs device stats reports the mapper path
    /// for a hot-unplugged device whose LUKS mapper is still open. The
    /// row carries its devid directly, and that devid must also appear in
    /// the alert-local missing_devids so a MissingDevice cause fires.
    #[test]
    fn null_underlying_device_triggers_missing_alert() {
        // Device stats include both a healthy device and the null-underlying
        // device (btrfs still reports its mapper path)
        let stats = make_stats(vec![zero_device(Devid::new(1)), zero_device(Devid::new(2))]);
        let acked = AckedStats::default();
        // Alert-local missing devids includes the null-underlying device's devid
        let alert_missing = vec![Devid::new(2)];
        let recognized = vec![Devid::new(1), Devid::new(2)];
        let alert = compute_alert_state(
            &stats,
            &acked,
            &AlertDevids {
                recognized: recognized.clone(),
                missing: alert_missing.clone(),
            },
            false,
        );
        assert!(alert.active());
        assert_eq!(alert.causes.len(), 1);
        assert_eq!(
            alert.causes[0],
            AlertCause::MissingDevice {
                devid: Devid::new(2)
            }
        );
    }
}
