use serde::{Deserialize, Serialize};
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PoolDevice {
    pub mapper: MapperName,
    pub luks_uuid: LuksUuid,
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
