use std::collections::BTreeSet;

use serde::{Serialize, Serializer};

use crate::types::LuksUuid;

// --- JSON command output structs ---

/// lsblk --json --bytes
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsblkDevice {
    pub name: String,
    pub device_type: String,
    pub size: Option<u64>,
    pub model: Option<String>,
    pub serial: Option<String>,
    pub uuid: Option<String>,
    pub rota: Option<bool>,
    pub tran: Option<String>,
    pub children: Vec<LsblkDevice>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsblkOutput {
    pub blockdevices: Vec<LsblkDevice>,
}

/// btrfs --format json filesystem df
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BtrfsBgType {
    Data,
    Metadata,
    System,
    GlobalReserve,
}

impl std::fmt::Display for BtrfsBgType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Data => "Data",
            Self::Metadata => "Metadata",
            Self::System => "System",
            Self::GlobalReserve => "GlobalReserve",
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum BtrfsProfile {
    Single,
    Dup,
    Raid0,
    Raid1,
    Raid1c3,
    Raid1c4,
    Raid5,
    Raid6,
    Raid10,
    Unknown(String),
}

impl std::fmt::Display for BtrfsProfile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Single => "single",
            Self::Dup => "DUP",
            Self::Raid0 => "RAID0",
            Self::Raid1 => "RAID1",
            Self::Raid1c3 => "RAID1C3",
            Self::Raid1c4 => "RAID1C4",
            Self::Raid5 => "RAID5",
            Self::Raid6 => "RAID6",
            Self::Raid10 => "RAID10",
            Self::Unknown(s) => return f.write_str(s),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct BtrfsDfEntry {
    pub bg_type: BtrfsBgType,
    pub bg_profile: BtrfsProfile,
    pub bg_used: u64,
    pub bg_total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsDfOutput {
    pub entries: Vec<BtrfsDfEntry>,
}

impl BtrfsDfOutput {
    pub fn profiles_for(&self, bg_type: BtrfsBgType) -> BTreeSet<BtrfsProfile> {
        self.entries
            .iter()
            .filter(|e| e.bg_type == bg_type)
            .map(|e| e.bg_profile.clone())
            .collect()
    }

    /// Logical filesystem-used bytes: Data.used + Metadata.used +
    /// System.used, excluding GlobalReserve. GlobalReserve is an
    /// internal emergency reservation carved out of Metadata, not
    /// additional on-disk data.
    pub fn logical_used_bytes(&self) -> u64 {
        self.entries
            .iter()
            .filter(|e| e.bg_type != BtrfsBgType::GlobalReserve)
            .map(|e| e.bg_used)
            .sum()
    }
}

// --- Text command output structs ---

/// btrfs filesystem show
/// Only devid + path are authoritative. Capacity comes from `btrfs filesystem usage --raw`.
/// Size field from show output is intentionally not parsed — it's human-formatted and unused by domain code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsShowDevice {
    pub devid: u64,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsFilesystemShowOutput {
    pub uuid: Option<String>,
    pub total_devices: u64,
    pub devices: Vec<BtrfsShowDevice>,
    pub has_missing: bool,
    /// Devids of missing devices (extracted from MISSING sentinel lines).
    pub missing_devids: Vec<u64>,
}

/// cryptsetup status
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptsetupStatusOutput {
    pub is_active: bool,
    pub device: Option<String>,
}

/// cryptsetup luksUUID
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptsetupLuksUuidOutput {
    pub uuid: LuksUuid,
}

/// cryptsetup luksDump --dump-json-metadata
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptsetupLuksDumpOutput {
    pub cipher: String,     // e.g. "aes-xts-plain64"
    pub key_size_bits: u32, // e.g. 512
    pub keyslot_count: u32, // e.g. 1
}

/// cryptsetup luksDump (text output) — LUKS label field
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptsetupLuksLabelOutput {
    /// `None` if the label is `(no label)` or empty.
    pub label: Option<String>,
}

/// cryptsetup luksDump (text output) — LUKS version field
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CryptsetupLuksVersionOutput {
    pub version: u32,
}

/// Fixed-point data ratio in hundredths (100 = 1.00, 200 = 2.00).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DataRatio(u32);

impl DataRatio {
    pub fn parse(s: &str) -> Option<Self> {
        let (whole, frac) = s.split_once('.')?;
        if frac.is_empty() {
            return None;
        }
        let whole: u32 = whole.parse().ok()?;
        let frac_val: u32 = frac.parse().ok()?;
        let hundredths = match frac.len() {
            1 => whole * 100 + frac_val * 10,
            2 => whole * 100 + frac_val,
            _ => return None,
        };
        if hundredths == 0 {
            return None;
        }
        Some(Self(hundredths))
    }

    pub fn logical_bytes(self, device_size_bytes: u64) -> u64 {
        device_size_bytes * 100 / u64::from(self.0)
    }
}

/// btrfs filesystem usage --raw
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsFilesystemUsageOutput {
    pub device_size_bytes: u64,
    pub used_bytes: u64,
    pub free_estimated_bytes: u64,
    pub data_ratio: DataRatio,
}

/// Parsed scrub timestamp — the parser converts the raw ctime string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrubTimestamp(pub time::PrimitiveDateTime);

/// btrfs scrub status --raw
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScrubState {
    Never,
    Running {
        started_at: Option<ScrubTimestamp>,
        duration_secs: Option<u64>,
        time_left_secs: Option<u64>,
        eta: Option<ScrubTimestamp>,
        total_bytes: Option<u64>,
        bytes_scrubbed: Option<u64>,
        rate_bytes_per_sec: Option<u64>,
        error_count: u64,
    },
    Finished {
        started_at: ScrubTimestamp,
        error_count: u64,
        duration_secs: Option<u64>,
        total_bytes: Option<u64>,
        rate_bytes_per_sec: Option<u64>,
    },
    Aborted {
        started_at: ScrubTimestamp,
        error_count: u64,
        duration_secs: Option<u64>,
        total_bytes: Option<u64>,
        rate_bytes_per_sec: Option<u64>,
    },
    Interrupted {
        started_at: ScrubTimestamp,
        error_count: u64,
        duration_secs: Option<u64>,
        total_bytes: Option<u64>,
        rate_bytes_per_sec: Option<u64>,
    },
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsScrubStatusOutput {
    pub state: ScrubState,
}

/// btrfs scrub status -d -R (per-device)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceScrubState {
    Running,
    Finished,
    Aborted,
    Interrupted,
    Unknown(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceScrubEntry {
    pub devid: u64,
    pub path: Option<String>,
    pub state: DeviceScrubState,
    pub started_at: Option<ScrubTimestamp>,
    pub duration_secs: u64,
    pub data_bytes_scrubbed: u64,
    pub tree_bytes_scrubbed: u64,
    pub read_errors: u64,
    pub csum_errors: u64,
    pub verify_errors: u64,
    pub uncorrectable_errors: u64,
    pub corrected_errors: u64,
    pub super_errors: u64,
    pub last_physical: u64,
}

impl DeviceScrubEntry {
    pub fn total_errors(&self) -> u64 {
        self.read_errors
            + self.csum_errors
            + self.verify_errors
            + self.uncorrectable_errors
            + self.corrected_errors
            + self.super_errors
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsScrubStatusPerDeviceOutput {
    pub uuid: String,
    pub devices: Vec<DeviceScrubEntry>,
}

/// Target device in `btrfs device stats` output.
///
/// btrfs-progs emits `<missing disk>` as the device path for absent drives
/// during a degraded mount. This enum converts that sentinel into a typed
/// variant at parse time so downstream code uses pattern matching instead of
/// string comparisons.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum DeviceStatsTarget {
    Path(String),
    MissingDisk,
}

impl DeviceStatsTarget {
    pub fn as_path(&self) -> Option<&str> {
        match self {
            Self::Path(p) => Some(p),
            Self::MissingDisk => None,
        }
    }
}

impl std::fmt::Display for DeviceStatsTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Path(p) => f.write_str(p),
            Self::MissingDisk => f.write_str("<missing disk>"),
        }
    }
}

/// btrfs device stats
///
/// `devid` is the canonical identity for a stats row -- always present in the
/// btrfs JSON schema and stable across mapper-path changes. All identity
/// logic (alert pairing, snapshot keys, status/replace/TUI lookups) keys
/// off `devid`. `target` is retained for direct display strings only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceErrorStats {
    pub devid: u64,
    pub target: DeviceStatsTarget,
    pub read_io_errs: u64,
    pub write_io_errs: u64,
    pub corruption_errs: u64,
    pub generation_errs: u64,
    pub flush_io_errs: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsDeviceStatsOutput {
    pub devices: Vec<DeviceErrorStats>,
}

/// lsblk -ndo FIELD
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsblkFieldOutput {
    pub value: Option<String>,
}

/// btrfs balance status
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BalanceState {
    None,
    Running {
        done_chunks: u64,
        estimated_total_chunks: u64,
        considered_chunks: u64,
        pct_left: u8,
    },
    Paused {
        done_chunks: u64,
        estimated_total_chunks: u64,
        considered_chunks: u64,
        pct_left: u8,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsBalanceStatusOutput {
    pub state: BalanceState,
}

/// smartctl health classification
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartHealth {
    Healthy,
    Degraded,
    Failing,
    Unknown,
}

/// smartctl per-probe result: health classification plus optional current
/// temperature in Celsius. `celsius` is `None` when the drive doesn't emit
/// `temperature.current` (USB-bridged drives, NVMe without thermal reporting,
/// parser failure, etc.). `celsius` is independent of `health`: a drive can
/// report temperature while health is Unknown, or vice versa.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SmartProbe {
    pub health: SmartHealth,
    pub celsius: Option<i16>,
}

/// btrfs replace status
#[derive(Debug, Clone, PartialEq)]
pub enum ReplaceState {
    /// No replace operation running.
    None,
    /// Replace in progress with percentage.
    Running { pct: f64 },
    /// Replace finished.
    Finished,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BtrfsReplaceStatusOutput {
    pub state: ReplaceState,
}

/// btrfs device usage --raw
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceAllocation {
    pub alloc_type: String,
    pub profile: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsDeviceUsageEntry {
    pub path: String,
    pub devid: u64,
    pub device_size: u64,
    pub device_slack: u64,
    pub allocations: Vec<DeviceAllocation>,
    pub unallocated: u64,
}

impl BtrfsDeviceUsageEntry {
    pub fn used_bytes(&self) -> u64 {
        self.allocations.iter().map(|a| a.bytes).sum()
    }

    pub fn allocated_by_type(&self, alloc_type: &str) -> u64 {
        self.allocations
            .iter()
            .filter(|a| a.alloc_type == alloc_type)
            .map(|a| a.bytes)
            .sum()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsDeviceUsageOutput {
    pub devices: Vec<BtrfsDeviceUsageEntry>,
}

/// btrfs subvolume list
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsSubvolume {
    pub id: u64,
    pub generation: u64,
    pub top_level: u64,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsSubvolumeListOutput {
    pub subvolumes: Vec<BtrfsSubvolume>,
}

/// NUT `ups.status` flag. The full variant list lives here even though v1
/// preflight only consults `Ob` / `Lb` -- keeping the enum complete means
/// the richer parser does not need to re-land the list.
///
/// `Unknown(String)` preserves any token we don't yet recognize so future
/// NUT statuses are surfaced rather than silently dropped.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum UpsStatusFlag {
    /// On utility power.
    Ol,
    /// On battery.
    Ob,
    /// Low battery.
    Lb,
    /// Replace battery.
    Rb,
    /// High battery.
    Hb,
    /// Charging.
    Chrg,
    /// Discharging.
    Dischrg,
    /// Calibrating.
    Cal,
    /// Bypass active.
    Bypass,
    /// Administratively off.
    Off,
    /// Overload.
    Over,
    /// Trim / SmartTrim (stepping voltage down).
    Trim,
    /// Boost / SmartBoost (stepping voltage up).
    Boost,
    /// Forced shutdown in progress.
    Fsd,
    /// Battery self-test failed (some drivers fold this into `ups.status`).
    TestFail,
    /// Communications with UPS lost. Not standard in `ups.status`, but
    /// some drivers surface it there; `upsmon` also emits COMMBAD as a
    /// notification name. Recognised here so severity rendering picks
    /// it up instead of falling through to `Unknown`.
    CommBad,
    Unknown(String),
}

impl UpsStatusFlag {
    /// Rendered token, matching NUT's own `ups.status` vocabulary
    /// (`reference/nut/clients/upsc.c:141` emits these verbatim).
    pub fn as_token(&self) -> &str {
        match self {
            Self::Ol => "OL",
            Self::Ob => "OB",
            Self::Lb => "LB",
            Self::Rb => "RB",
            Self::Hb => "HB",
            Self::Chrg => "CHRG",
            Self::Dischrg => "DISCHRG",
            Self::Cal => "CAL",
            Self::Bypass => "BYPASS",
            Self::Off => "OFF",
            Self::Over => "OVER",
            Self::Trim => "TRIM",
            Self::Boost => "BOOST",
            Self::Fsd => "FSD",
            Self::TestFail => "TESTFAIL",
            Self::CommBad => "COMMBAD",
            Self::Unknown(s) => s.as_str(),
        }
    }

    /// Is this flag by itself a critical UPS state?
    ///
    /// "Critical" means: braid refuses to start pool-mutating commands
    /// when this flag is present, and the TUI colors it red. Used by
    /// both `preflight::check_ups_not_on_battery` and
    /// `tui::view::ups_severity_color` so the two surfaces stay in
    /// sync -- if a new driver-reported token lands, classification
    /// lives in exactly one place.
    ///
    /// - `Lb` -- low battery, shutdown imminent.
    /// - `TestFail` -- battery self-test failed.
    /// - `CommBad` -- comms with UPS lost.
    /// - `Fsd` -- forced shutdown in progress.
    ///
    /// `Ob` alone is NOT critical (yellow in the TUI); preflight
    /// refuses on `Ob` separately.
    pub fn is_critical(&self) -> bool {
        matches!(self, Self::Lb | Self::TestFail | Self::CommBad | Self::Fsd)
    }
}

impl UpscOutput {
    /// True when the UPS is reporting any critical state. See
    /// `UpsStatusFlag::is_critical` for the token list.
    pub fn is_critical(&self) -> bool {
        self.status_flags.iter().any(UpsStatusFlag::is_critical)
    }

    /// True when the UPS reports it is running on battery (without
    /// necessarily having crossed the low-battery threshold yet). Used
    /// by preflight to refuse mutations that would start during an
    /// outage, narrowing the recovery surface.
    pub fn is_on_battery(&self) -> bool {
        self.status_flags.contains(&UpsStatusFlag::Ob)
    }
}

impl Serialize for UpsStatusFlag {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_token())
    }
}

/// Battery-scoped fields from `upsc`.
///
/// Every field is `Option` because NUT drivers vary widely in which keys
/// they publish. A UPS that reports `ups.status` but no `battery.charge`
/// is not malformed; we render missing fields as `-` instead of failing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BatteryFields {
    /// `battery.charge` -- percent (0-100).
    pub charge_pct: Option<u8>,
    /// `battery.runtime` -- seconds of runtime remaining.
    pub runtime_secs: Option<u32>,
    /// `battery.voltage` -- raw textual value (formats vary across drivers).
    pub voltage: Option<String>,
    /// `battery.type` -- e.g. "PbAc".
    pub type_: Option<String>,
    /// `battery.mfr.date` -- raw textual date.
    pub mfr_date: Option<String>,
    /// `battery.runtime.low` -- low-battery runtime threshold in seconds.
    pub runtime_low_secs: Option<u32>,
}

/// Input (mains) fields from `upsc`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct InputFields {
    /// `input.voltage` -- raw textual value (volts).
    pub voltage: Option<String>,
    /// `input.transfer.low` -- transfer-to-battery low threshold.
    pub transfer_low: Option<String>,
    /// `input.transfer.high` -- transfer-to-battery high threshold.
    pub transfer_high: Option<String>,
    /// `input.sensitivity` -- driver-reported sensitivity setting.
    pub sensitivity: Option<String>,
}

/// Device-scoped fields from `upsc`. Accepts either `device.*` or
/// `ups.*` for model / mfr / serial because different drivers prefer
/// different keys; `device.*` generally wins when both are present.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct DeviceFields {
    pub model: Option<String>,
    pub mfr: Option<String>,
    pub serial: Option<String>,
    pub type_: Option<String>,
}

/// Typed `upsc <name>` output. `extra` keeps every `key: value` line that
/// did not land in a typed field, so operators can still see unfamiliar
/// entries (e.g. driver-specific debug keys) if they inspect the full
/// structure via `--json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpscOutput {
    pub status_flags: std::collections::HashSet<UpsStatusFlag>,
    pub battery: BatteryFields,
    /// `ups.load` -- percent (0-100).
    pub load_pct: Option<u8>,
    /// `ups.realpower.nominal` -- nameplate watts. Combined with `load_pct`
    /// to produce an estimated-watts figure in `braid ups status`.
    pub realpower_nominal_watts: Option<u32>,
    pub input: InputFields,
    /// `ups.test.result` -- last self-test result, verbatim.
    pub test_result: Option<String>,
    pub device: DeviceFields,
    /// Every `key: value` line not captured by a typed field above.
    pub extra: std::collections::BTreeMap<String, String>,
}

impl UpscOutput {
    /// Estimated load in watts when both `load_pct` and
    /// `realpower_nominal_watts` are available. Returns `None` otherwise
    /// (we do not synthesize a figure from a single input -- callers
    /// must render the missing case explicitly).
    pub fn watts_estimated(&self) -> Option<u32> {
        match (self.load_pct, self.realpower_nominal_watts) {
            (Some(pct), Some(nominal)) => Some((u32::from(pct) * nominal + 50) / 100),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_ratio_parse_1_00() {
        assert_eq!(DataRatio::parse("1.00"), Some(DataRatio(100)));
    }

    #[test]
    fn data_ratio_parse_2_00() {
        assert_eq!(DataRatio::parse("2.00"), Some(DataRatio(200)));
    }

    #[test]
    fn data_ratio_parse_intermediate() {
        assert_eq!(DataRatio::parse("1.01"), Some(DataRatio(101)));
    }

    #[test]
    fn data_ratio_parse_no_dot() {
        assert_eq!(DataRatio::parse("abc"), None);
    }

    #[test]
    fn data_ratio_parse_one_frac_digit() {
        assert_eq!(DataRatio::parse("1.0"), Some(DataRatio(100)));
    }

    #[test]
    fn data_ratio_parse_one_frac_digit_nonzero() {
        assert_eq!(DataRatio::parse("1.5"), Some(DataRatio(150)));
    }

    #[test]
    fn data_ratio_parse_three_frac_digits() {
        assert_eq!(DataRatio::parse("1.001"), None);
    }

    #[test]
    fn data_ratio_parse_zero() {
        assert_eq!(DataRatio::parse("0.00"), None);
    }

    #[test]
    fn data_ratio_logical_bytes_raid1() {
        assert_eq!(DataRatio(200).logical_bytes(1_000_000), 500_000);
    }

    #[test]
    fn data_ratio_logical_bytes_intermediate() {
        assert_eq!(DataRatio(101).logical_bytes(1_000_000), 990_099);
    }

    // Intent: UpsStatusFlag::is_critical names the exact set used by
    // both preflight and the TUI severity color mapping.
    // Why: this is the single classifier shared between surfaces. A
    // regression that silently drops a token from the critical set
    // would make preflight pass while the UI still paints red, or
    // vice versa. Pinning the set here makes any shift visible at
    // the source.
    // Scenario: future edits to the UpsStatusFlag enum or severity
    // rules.
    #[test]
    fn ups_status_flag_critical_set() {
        for flag in [
            UpsStatusFlag::Lb,
            UpsStatusFlag::TestFail,
            UpsStatusFlag::CommBad,
            UpsStatusFlag::Fsd,
        ] {
            assert!(flag.is_critical(), "{flag:?} should be critical");
        }
        for flag in [
            UpsStatusFlag::Ol,
            UpsStatusFlag::Ob,
            UpsStatusFlag::Rb,
            UpsStatusFlag::Hb,
            UpsStatusFlag::Chrg,
            UpsStatusFlag::Dischrg,
            UpsStatusFlag::Cal,
            UpsStatusFlag::Bypass,
            UpsStatusFlag::Off,
            UpsStatusFlag::Over,
            UpsStatusFlag::Trim,
            UpsStatusFlag::Boost,
            UpsStatusFlag::Unknown("WHATEVER".into()),
        ] {
            assert!(!flag.is_critical(), "{flag:?} must not be critical");
        }
    }
}
