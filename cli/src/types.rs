use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ByIdPath(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LuksUuid(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MapperName(pub String);

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct MountPoint(pub String);

impl MountPoint {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ByIdPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for LuksUuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for MapperName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for MountPoint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

// ---------------------------------------------------------------------------
// Planner input types (produced by probe, consumed by commands)
// ---------------------------------------------------------------------------

/// What we know about the live btrfs pool.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolState {
    pub mounted: bool,
    pub devices: Vec<PoolDevice>,
    pub missing_count: u64,
    pub total_devices: u64,
    /// btrfs filesystem FSID (uuid), populated when pool is mounted.
    pub fsid: Option<String>,
    /// Devids of missing devices (from btrfs filesystem show MISSING sentinels).
    ///
    /// Authoritative to btrfs — does NOT include null-underlying devices.
    /// `remove-missing` uses this to resolve destructive removal targets, so
    /// only devices that btrfs has confirmed as MISSING belong here.
    pub missing_devids: Vec<u64>,
    /// Devices whose LUKS mapper is open but underlying block device is gone
    /// (hot-unplugged). Kept separate from `missing_devids` because
    /// `missing_devids` is used by `remove-missing` to pick destructive
    /// removal targets -- a transient hot-unplug must not look removable.
    ///
    /// Monitor and ack compute an alert-local union (`missing_devids ∪
    /// null_underlying devids`) to fire `MissingDevice` alerts for both cases.
    /// btrfs device stats keeps reporting these devices' mapper paths along
    /// with their devids, so the alert pipeline pairs rows by devid directly
    /// from the parsed stats output -- no path-to-devid map required.
    pub null_underlying: Vec<NullUnderlyingDevice>,
}

impl PoolState {
    /// Devids that must fire `MissingDevice` alert causes: the btrfs-
    /// authoritative MISSING set unioned with null-underlying devids,
    /// deduplicated and sorted. Dedup matters when btrfs has promoted a
    /// hot-unplugged device to MISSING while its LUKS mapper still reports
    /// `(null)` -- without it, the same devid would produce two
    /// `MissingDevice` causes.
    pub fn alert_missing_devids(&self) -> Vec<u64> {
        self.missing_devids
            .iter()
            .copied()
            .chain(self.null_underlying.iter().map(|d| d.devid))
            .collect::<BTreeSet<u64>>()
            .into_iter()
            .collect()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolDevice {
    pub mapper: MapperName,
    pub luks_uuid: LuksUuid,
    pub devid: u64,
    pub underlying: String,
}

/// A pool device whose LUKS mapper is open but the underlying block device
/// is gone (hot-unplugged). These are effectively missing for alerting but
/// not yet confirmed by btrfs as MISSING.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NullUnderlyingDevice {
    pub mapper: MapperName,
    pub devid: u64,
}

/// Pre-probed state of each config disk (produced by probe, consumed by commands).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigDisk {
    pub name: String,
    pub by_id_path: ByIdPath,
    pub state: ConfigDiskState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigDiskState {
    /// Device file doesn't exist (unplugged / absent).
    Absent,
    /// Device exists but is not LUKS-formatted.
    PresentNotLuks,
    /// Device exists, has LUKS header, UUID known.
    /// `mapper_open` = true if /dev/mapper/<name> is already active (crash recovery skip).
    PresentLuks { uuid: LuksUuid, mapper_open: bool },
}
