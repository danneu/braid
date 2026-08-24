use std::collections::BTreeSet;

use serde::{Deserialize, Serialize, Serializer};

use crate::types::{BackingPath, Devid, Fsid, LuksUuid};

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
    pub devid: Devid,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsFilesystemShowOutput {
    pub uuid: Option<Fsid>,
    pub total_devices: u64,
    pub devices: Vec<BtrfsShowDevice>,
    pub has_missing: bool,
    /// Devids of missing devices (extracted from MISSING sentinel lines).
    pub missing_devids: Vec<Devid>,
}

/// Result of `cryptsetup status <mapper>`. The active-vs-inactive split is
/// enforced by the parser: an inactive mapper carries no backing device; an
/// active one always carries a typed backing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CryptsetupStatusOutput {
    Inactive,
    Active { backing: BackingDevice },
}

/// Backing block device reported by an active mapper. Cryptsetup prints
/// `device: (null)` when the underlying block device has been hot-unplugged;
/// braid additionally folds empty or whitespace-only parsed values into `Null`
/// defensively, since `parse_device_line` can yield `""` if the value side of
/// the `device:` line is blank. Folding both into a single `Null` variant
/// prevents consumers from routing either value through the real-path code
/// path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BackingDevice {
    Path(BackingPath),
    Null,
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
    pub segment_offset_bytes: u64,
    pub segment_size: Luks2SegmentSize,
}

/// LUKS2 segment capacity model used to estimate mapper size before opening
/// the replacement target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Luks2SegmentSize {
    Dynamic,
    Fixed(u64),
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
            1 => whole
                .checked_mul(100)?
                .checked_add(frac_val.checked_mul(10)?)?,
            2 => whole.checked_mul(100)?.checked_add(frac_val)?,
            _ => return None,
        };
        if hundredths == 0 {
            return None;
        }
        Some(Self(hundredths))
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
        started_at: Option<ScrubTimestamp>,
        error_count: u64,
        duration_secs: Option<u64>,
        total_bytes: Option<u64>,
        rate_bytes_per_sec: Option<u64>,
    },
    Aborted {
        started_at: Option<ScrubTimestamp>,
        error_count: u64,
        duration_secs: Option<u64>,
        total_bytes: Option<u64>,
        rate_bytes_per_sec: Option<u64>,
    },
    Interrupted {
        started_at: Option<ScrubTimestamp>,
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
    pub devid: Devid,
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
            .saturating_add(self.csum_errors)
            .saturating_add(self.verify_errors)
            .saturating_add(self.uncorrectable_errors)
            .saturating_add(self.corrected_errors)
            .saturating_add(self.super_errors)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsScrubStatusPerDeviceOutput {
    pub uuid: Fsid,
    pub devices: Vec<DeviceScrubEntry>,
}

/// btrfs device stats
///
/// `devid` is the btrfs-native key for a stats row -- always present in the
/// btrfs JSON schema and stable across mapper-path changes. Alert pairing,
/// snapshot keys, and status/replace/TUI row lookups use `devid` for this
/// parser output; LUKS UUID remains braid's persistent member identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceErrorStats {
    pub devid: Devid,
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

/// smartctl health classification. The serde renames pin each variant to the
/// lowercase word the TUI column already prints, so the `--json` `smart.health`
/// string and the TUI verdict cannot drift in vocabulary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SmartHealth {
    #[serde(rename = "ok")]
    Healthy,
    #[serde(rename = "warning")]
    Degraded,
    #[serde(rename = "failing")]
    Failing,
    #[serde(rename = "unknown")]
    Unknown,
}

/// Stable per-field identity for one SMART evidence counter. Decouples a field's
/// identity from its rendered text so the TUI red row, the human parenthetical,
/// and the color test all key off this enum, never a matched label string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SmartField {
    // SATA (ATA attributes)
    Reallocated,
    Pending,
    Uncorrectable,
    // NVMe (health-information log)
    CriticalWarning,
    MediaErrors,
    AvailableSpare,
    PercentageUsed,
}

impl SmartField {
    /// Rendered label for this field, shared by the human `SMART:` parenthetical
    /// and the TUI evidence rows so the two surfaces cannot disagree on wording.
    pub fn label(self) -> &'static str {
        match self {
            Self::Reallocated => "reallocated",
            Self::Pending => "pending",
            Self::Uncorrectable => "uncorrectable",
            Self::CriticalWarning => "critical warning",
            Self::MediaErrors => "media errors",
            Self::AvailableSpare => "available spare",
            Self::PercentageUsed => "percentage used",
        }
    }
}

/// SMART supporting evidence, tagged by `protocol` so the field set is
/// unambiguous and the serialized shape is forward-compatible. SMART's
/// authoritative signal is the pass/fail verdict (`SmartHealth`); these counts
/// are evidence behind it, never a verdict of their own. A single summed
/// `smart_errors` integer was rejected: it mixes units and would render `0` on a
/// drive reporting `passed:false`. `Copy` so one probe value feeds every surface
/// without a clone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "protocol", rename_all = "snake_case")]
pub enum SmartEvidence {
    Sata {
        reallocated_sectors: u64,
        pending_sectors: u64,
        offline_uncorrectable: u64,
    },
    Nvme {
        media_errors: u64,
        critical_warning: u64,
        percentage_used: u64,
        available_spare: u64,
        available_spare_threshold: u64,
    },
}

impl SmartEvidence {
    /// Every display field as `(key, value, is_concern)` -- the single source of
    /// both the shown value and the per-protocol "out of spec" test. The TUI
    /// builds one row per triple and reds it iff `is_concern`; the human line and
    /// verdict consult `concerns()`. `available_spare_threshold` is consulted by
    /// the spare predicate (a threshold pair, not a generic `> 0` rule -- exactly
    /// why a flat numeric rule is wrong for NVMe) but is not itself a row.
    pub fn fields(&self) -> Vec<(SmartField, u64, bool)> {
        match *self {
            Self::Sata {
                reallocated_sectors,
                pending_sectors,
                offline_uncorrectable,
            } => vec![
                (
                    SmartField::Reallocated,
                    reallocated_sectors,
                    reallocated_sectors > 0,
                ),
                (SmartField::Pending, pending_sectors, pending_sectors > 0),
                (
                    SmartField::Uncorrectable,
                    offline_uncorrectable,
                    offline_uncorrectable > 0,
                ),
            ],
            Self::Nvme {
                media_errors,
                critical_warning,
                percentage_used,
                available_spare,
                available_spare_threshold,
            } => vec![
                (
                    SmartField::CriticalWarning,
                    critical_warning,
                    critical_warning != 0,
                ),
                (SmartField::MediaErrors, media_errors, media_errors != 0),
                (
                    SmartField::AvailableSpare,
                    available_spare,
                    available_spare_threshold > 0 && available_spare <= available_spare_threshold,
                ),
                (
                    SmartField::PercentageUsed,
                    percentage_used,
                    percentage_used >= 90,
                ),
            ],
        }
    }

    /// The `is_concern` subset of `fields()` as `(key, value)`. Drives the
    /// SATA/NVMe verdict (`Healthy` iff empty, else `Degraded`) and the human
    /// `SMART:` parenthetical, so the verdict and the itemized concerns share one
    /// threshold definition.
    pub fn concerns(&self) -> Vec<(SmartField, u64)> {
        self.fields()
            .into_iter()
            .filter(|(_, _, is_concern)| *is_concern)
            .map(|(key, value, _)| (key, value))
            .collect()
    }
}

/// smartctl per-probe result: a `health` verdict plus optional supporting
/// `evidence` and current temperature. `evidence` is `None` when the protocol's
/// detail log is absent (the verdict still derives, but there is nothing to
/// itemize); `celsius` is `None` when the drive omits `temperature.current`
/// (USB-bridged drives, NVMe without thermal reporting, parser failure). Both
/// are independent of `health`. The evidence serializes flat so `protocol` + the
/// counters sit at the `smart` object level alongside `health`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SmartProbe {
    pub health: SmartHealth,
    #[serde(flatten, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<SmartEvidence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub celsius: Option<i16>,
}

/// Parsed SMART self-test log summary for doctor classification.
///
/// Carries parser gate flags separately from ATA fields so doctor can preserve
/// smartctl's command-error vs active-self-test-failure distinction.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SelftestSummary {
    pub command_error: bool,
    pub parse_failure: bool,
    pub unsupported_protocol: Option<String>,
    pub power_on_hours: Option<u64>,
    pub active_errors: u32,
    pub last_passing: Option<SelftestEntry>,
    pub last_failure: Option<SelftestEntry>,
}

/// SMART self-test entry selected from the reverse-chronological log table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelftestEntry {
    pub kind: SelftestKind,
    pub lifetime_hours: u32,
    pub status_value: u8,
    pub status_string: String,
}

/// SMART self-test operation type normalized from smartctl's numeric code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelftestKind {
    Short,
    Extended,
    Conveyance,
    Selective,
    Offline,
    Other(String),
}

impl std::fmt::Display for SelftestKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Short => f.write_str("short"),
            Self::Extended => f.write_str("extended"),
            Self::Conveyance => f.write_str("conveyance"),
            Self::Selective => f.write_str("selective"),
            Self::Offline => f.write_str("offline"),
            Self::Other(s) => f.write_str(s),
        }
    }
}

/// btrfs replace status
#[derive(Debug, Clone, PartialEq)]
pub enum ReplaceState {
    /// Filesystem has never had a replace issued.
    NotStarted,
    /// Replace in progress with percentage.
    Running { pct: f64 },
    /// Replace finished.
    Finished,
    /// Kernel-canceled; topology was reverted and btrfs reports zero progress.
    Cancelled,
    /// Kernel-suspended replace; the kernel still treats it as ongoing.
    Suspended { pct: f64 },
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
    pub devid: Devid,
    pub device_size: u64,
    pub device_slack: u64,
    pub allocations: Vec<DeviceAllocation>,
    pub unallocated: u64,
}

impl BtrfsDeviceUsageEntry {
    /// True when btrfs rendered this stanza with a "missing device" path marker rather than a
    /// real block-device path. Trusting a relocation target keys on this, never on
    /// `device_size == 0` alone: btrfs-progs also reports `Device size: 0` for a PRESENT device
    /// whose `device_get_partition_size` probe failed, so size alone cannot tell a missing
    /// member from a live device with a transient probe failure.
    pub fn has_missing_marker(&self) -> bool {
        self.path == super::btrfs_device_usage::MISSING_DEVICE_PATH_MARKER
            || self.path == super::btrfs_device_usage::MISSING_DEVICE_PATH_FALLBACK
    }

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
    /// Rendered token, matching NUT's own `ups.status` vocabulary. `upsc`
    /// emits these verbatim -- nut 2.8.4, clients/upsc.c (fn `list_vars`):
    /// `printf("%s: %s\n", answer[2], answer[3]);`
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
    /// when this flag is present, colors it red in the TUI, and renders
    /// it as a failure in the human UPS status line. This predicate is
    /// the primitive consumed by `UpsSeverity`, which owns the full
    /// cross-surface severity ladder.
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

/// Shared UPS severity classifier for preflight, the TUI, and the human CLI
/// render. Classification lives in the parse domain so every surface consumes
/// the same fail-closed ladder, while presentation choices stay at the caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpsSeverity {
    Online,
    OnBattery,
    Critical,
    Indeterminate,
}

impl UpsSeverity {
    /// Classify the raw `ups.status` flag set. The worst condition wins:
    /// critical flags first, then on-battery, then affirmative utility power,
    /// then indeterminate for empty or unproven states.
    pub fn classify(flags: &[UpsStatusFlag]) -> Self {
        if flags.iter().any(UpsStatusFlag::is_critical) {
            return Self::Critical;
        }
        if flags.contains(&UpsStatusFlag::Ob) {
            return Self::OnBattery;
        }
        if flags.contains(&UpsStatusFlag::Ol) {
            return Self::Online;
        }
        Self::Indeterminate
    }
}

impl UpscOutput {
    /// Shared severity verdict for this parsed UPS output.
    pub fn severity(&self) -> UpsSeverity {
        UpsSeverity::classify(&self.status_flags)
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
    #[serde(rename = "type")]
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
    #[serde(rename = "type")]
    pub type_: Option<String>,
}

/// Typed `upsc <name>` output. `extra` keeps every `key: value` line that
/// did not land in a typed field, so operators can still see unfamiliar
/// entries (e.g. driver-specific debug keys) if they inspect the full
/// structure via `--json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpscOutput {
    /// Flags from `ups.status`, in first-seen token order, deduplicated on
    /// push. That order is the script-facing contract (ADR 020): the human CLI
    /// (`format_status`), `--json`, the TUI bridge (`probe_ups_for_tui`), and
    /// both TUI renders (`format_ups_flags`, Browse) carry this `Vec` verbatim
    /// -- none re-sorts. The `--json` path once lex-sorted via a
    /// `serialize_with` hook; it was removed so every surface agrees.
    /// Membership tests treat the `Vec` as a set; dedupe-on-push keeps those
    /// calls honest.
    pub status_flags: Vec<UpsStatusFlag>,
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
    ///
    /// The u64 widening and `as u32` cast are lossless, not truncating:
    /// `parse_pct` gates `load_pct` to `0..=100`, so the rounded quotient
    /// `(pct * nominal + 50) / 100` is at most `nominal` (equal at 100% load)
    /// and always fits `u32`. `+ 50` before `/ 100` rounds to nearest.
    pub fn watts_estimated(&self) -> Option<u32> {
        match (self.load_pct, self.realpower_nominal_watts) {
            (Some(pct), Some(nominal)) => {
                Some(((u64::from(pct) * u64::from(nominal) + 50) / 100) as u32)
            }
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

    // Intent: DataRatio::parse rejects syntactically valid ratios whose fixed-point
    // representation would overflow u32.
    // Why it exists: btrfs ratio text is parsed from an external tool, so the
    // parser must fail cleanly instead of panicking or wrapping.
    // Scenario: corrupt btrfs output reports an implausibly large data ratio.
    #[test]
    fn data_ratio_parse_overflow_returns_none() {
        assert_eq!(DataRatio::parse("99999999.0"), None);
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

    // Intent: UpsSeverity::classify owns the cross-surface severity ladder.
    // Why: preflight, the TUI, and the human UPS status line must agree about
    //   which condition wins when NUT reports contradictory or advisory flags.
    // Scenario: representative critical, on-battery, online, advisory, empty,
    //   and unknown-only status sets.
    #[test]
    fn ups_severity_classifies_status_flags() {
        assert_eq!(
            UpsSeverity::classify(&[UpsStatusFlag::Ol, UpsStatusFlag::TestFail]),
            UpsSeverity::Critical
        );
        assert_eq!(
            UpsSeverity::classify(&[UpsStatusFlag::Ol, UpsStatusFlag::Ob]),
            UpsSeverity::OnBattery
        );
        assert_eq!(
            UpsSeverity::classify(&[UpsStatusFlag::Ob]),
            UpsSeverity::OnBattery
        );
        assert_eq!(
            UpsSeverity::classify(&[UpsStatusFlag::Ol]),
            UpsSeverity::Online
        );
        assert_eq!(
            UpsSeverity::classify(&[UpsStatusFlag::Ol, UpsStatusFlag::Rb]),
            UpsSeverity::Online
        );
        assert_eq!(UpsSeverity::classify(&[]), UpsSeverity::Indeterminate);
        assert_eq!(
            UpsSeverity::classify(&[UpsStatusFlag::Unknown("WEIRD".into())]),
            UpsSeverity::Indeterminate
        );
    }

    // Intent: a healthy NVMe drive produces an empty `concerns()` set, and its
    //   spare/wear rows in `fields()` carry `is_concern == false`.
    // Why it exists: this is the structure-insensitive guard on the NVMe
    //   threshold-pair inversion -- `available_spare 100` (a *good* value) must
    //   not red the spare row the way a generic `> 0` rule would. It pins no
    //   false-positive concern on a healthy drive, independent of any rendering.
    // Scenario: a fresh NVMe at 12% wear, full spare, threshold 10.
    #[test]
    fn smart_evidence_nvme_healthy_has_no_concerns() {
        let healthy = SmartEvidence::Nvme {
            media_errors: 0,
            critical_warning: 0,
            percentage_used: 12,
            available_spare: 100,
            available_spare_threshold: 10,
        };
        assert_eq!(healthy.concerns(), vec![]);
        // The spare and wear rows render, but neither is a concern.
        let by_key: Vec<(SmartField, bool)> = healthy
            .fields()
            .into_iter()
            .map(|(key, _, is_concern)| (key, is_concern))
            .collect();
        assert!(by_key.contains(&(SmartField::AvailableSpare, false)));
        assert!(by_key.contains(&(SmartField::PercentageUsed, false)));
    }

    // Intent: NVMe wear over the threshold is the only concern, keyed by
    //   `PercentageUsed` with its value.
    // Why it exists: pins the wear predicate to PercentageUsed alone.
    // Scenario: an NVMe at 92% rated endurance, otherwise nominal.
    #[test]
    fn smart_evidence_nvme_wear_concern() {
        let worn = SmartEvidence::Nvme {
            media_errors: 0,
            critical_warning: 0,
            percentage_used: 92,
            available_spare: 100,
            available_spare_threshold: 10,
        };
        assert_eq!(worn.concerns(), vec![(SmartField::PercentageUsed, 92)]);
    }

    // Intent: NVMe spare at/under its threshold is a concern keyed by
    //   `AvailableSpare`.
    // Why it exists: the threshold *pair* (spare <= threshold), not a `> 0` rule,
    //   is what fires -- the exact case a generic numeric rule gets wrong.
    // Scenario: an NVMe whose spare has fallen to 5 against a threshold of 10.
    #[test]
    fn smart_evidence_nvme_low_spare_concern() {
        let low = SmartEvidence::Nvme {
            media_errors: 0,
            critical_warning: 0,
            percentage_used: 0,
            available_spare: 5,
            available_spare_threshold: 10,
        };
        assert_eq!(low.concerns(), vec![(SmartField::AvailableSpare, 5)]);
    }

    // Intent: clean SATA has no concerns; a reallocated count is the lone concern.
    // Why it exists: pins the SATA predicate to Reallocated with its value.
    // Scenario: a clean drive, then one with 2 reallocated sectors.
    #[test]
    fn smart_evidence_sata_concerns() {
        let clean = SmartEvidence::Sata {
            reallocated_sectors: 0,
            pending_sectors: 0,
            offline_uncorrectable: 0,
        };
        assert_eq!(clean.concerns(), vec![]);

        let degraded = SmartEvidence::Sata {
            reallocated_sectors: 2,
            pending_sectors: 0,
            offline_uncorrectable: 0,
        };
        assert_eq!(degraded.concerns(), vec![(SmartField::Reallocated, 2)]);
    }

    // Intent: the `smart` JSON object serializes to the exact locked shape for
    //   SATA, NVMe, and unknown -- health, then the flattened protocol+counters,
    //   then celsius -- and round-trips back through Deserialize.
    // Why it exists: no `status --json` golden covers this, so this is the
    //   contract lock on the flatten + internally-tagged Option<SmartEvidence>
    //   shape (it also verifies the round-trip the no-hand-written-serde decision
    //   relies on).
    // Scenario: the three serialized shapes a DiskReport.smart can take.
    #[test]
    fn smart_probe_serialization_shape() {
        use serde_json::json;

        let sata = SmartProbe {
            health: SmartHealth::Degraded,
            evidence: Some(SmartEvidence::Sata {
                reallocated_sectors: 2,
                pending_sectors: 0,
                offline_uncorrectable: 0,
            }),
            celsius: Some(41),
        };
        assert_eq!(
            serde_json::to_value(sata).unwrap(),
            json!({
                "health": "warning",
                "protocol": "sata",
                "reallocated_sectors": 2,
                "pending_sectors": 0,
                "offline_uncorrectable": 0,
                "celsius": 41
            })
        );

        let nvme = SmartProbe {
            health: SmartHealth::Healthy,
            evidence: Some(SmartEvidence::Nvme {
                media_errors: 0,
                critical_warning: 0,
                percentage_used: 12,
                available_spare: 100,
                available_spare_threshold: 10,
            }),
            celsius: Some(52),
        };
        assert_eq!(
            serde_json::to_value(nvme).unwrap(),
            json!({
                "health": "ok",
                "protocol": "nvme",
                "media_errors": 0,
                "critical_warning": 0,
                "percentage_used": 12,
                "available_spare": 100,
                "available_spare_threshold": 10,
                "celsius": 52
            })
        );

        let unknown = SmartProbe {
            health: SmartHealth::Unknown,
            evidence: None,
            celsius: None,
        };
        assert_eq!(
            serde_json::to_value(unknown).unwrap(),
            json!({ "health": "unknown" })
        );

        for probe in [sata, nvme, unknown] {
            let value = serde_json::to_value(probe).unwrap();
            let back: SmartProbe = serde_json::from_value(value).unwrap();
            assert_eq!(back, probe);
        }
    }
}
