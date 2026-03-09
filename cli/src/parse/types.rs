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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BtrfsBgType {
    Data,
    Metadata,
    System,
    GlobalReserve,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsDfEntry {
    pub bg_type: BtrfsBgType,
    pub bg_profile: String,
    pub bg_used: u64,
    pub bg_total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsDfOutput {
    pub entries: Vec<BtrfsDfEntry>,
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

/// btrfs filesystem usage --raw
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BtrfsFilesystemUsageOutput {
    pub device_size_bytes: u64,
    pub used_bytes: u64,
    pub free_estimated_bytes: u64,
    pub data_ratio: u64,
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
