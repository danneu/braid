use std::collections::BTreeSet;

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

/// findmnt --json
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindmntEntry {
    pub target: String,
    pub source: String,
    pub fstype: String,
    pub options: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FindmntOutput {
    pub filesystems: Vec<FindmntEntry>,
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

/// Fixed-point data ratio in hundredths (100 = 1.00, 200 = 2.00).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct DataRatio(u32);

impl DataRatio {
    pub fn parse(s: &str) -> Option<Self> {
        let (whole, frac) = s.split_once('.')?;
        if frac.len() != 2 {
            return None;
        }
        let whole: u32 = whole.parse().ok()?;
        let frac: u32 = frac.parse().ok()?;
        let hundredths = whole * 100 + frac;
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

/// btrfs scrub status
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScrubState {
    Never,
    Running {
        pct: Option<u8>,
        total: Option<String>,
        rate: Option<String>,
    },
    Completed {
        started_at: ScrubTimestamp,
        error_count: u64,
        duration: Option<String>,
        total: Option<String>,
        rate: Option<String>,
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

/// btrfs device stats
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceErrorStats {
    pub device_path: String,
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
        assert_eq!(DataRatio::parse("1.0"), None);
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
}
