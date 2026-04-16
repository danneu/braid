use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::alert::AlertState;
use crate::parse::types::{BtrfsDfEntry, DeviceAllocation, ScrubState, SmartHealth};
use crate::state_paths::StatePaths;
use crate::status::{BalanceReport, DiskErrors};
use crate::tui::effect::Effect;
use crate::tui::state::{CmdId, CommandState};
use crate::types::{ByIdPath, LuksUuid, MountPoint};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Data,
    Scrub,
    Sharing,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Data, Tab::Scrub, Tab::Sharing];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Data => "Data",
            Tab::Scrub => "Scrub",
            Tab::Sharing => "Sharing",
        }
    }

    pub fn next(self) -> Tab {
        match self {
            Tab::Data => Tab::Scrub,
            Tab::Scrub => Tab::Sharing,
            Tab::Sharing => Tab::Data,
        }
    }

    pub fn prev(self) -> Tab {
        match self {
            Tab::Data => Tab::Sharing,
            Tab::Scrub => Tab::Data,
            Tab::Sharing => Tab::Scrub,
        }
    }
}

#[derive(Clone)]
pub struct DiskLuksInfo {
    pub cipher: String,
    pub key_size_bits: u32,
    pub keyslot_count: u32,
}

/// Physical identity of a disk for session-scoped temperature tracking.
/// LUKS UUID is preferred so watermarks survive device-path changes on
/// unplug/replug; by-id path is a fallback for disks whose UUID isn't
/// available in the probe.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum TemperatureDiskId {
    LuksUuid(LuksUuid),
    ByIdPath(ByIdPath),
}

/// Current temperature reading for one disk, produced per probe tick.
/// `celsius` is signed because SMART can legitimately report sub-zero values.
#[derive(Debug, Clone)]
pub struct TemperatureReading {
    pub id: TemperatureDiskId,
    pub celsius: i16,
}

/// Session-scoped hi/lo watermarks for one disk. Reset via Shift+R.
/// No `last` field: the current value is always read from the latest
/// `PoolState` so a failed probe can't produce a stale current temp.
#[derive(Debug, Clone, Copy)]
pub struct TemperatureWatermark {
    pub min_celsius: i16,
    pub max_celsius: i16,
    pub sample_count: u32,
}

#[derive(Clone)]
pub struct DiskUsage {
    pub size: u64,
    pub allocations: Vec<DeviceAllocation>,
    pub unallocated: u64,
}

impl DiskUsage {
    pub fn allocated(&self) -> u64 {
        self.allocations.iter().map(|a| a.bytes).sum()
    }
}

/// Render classification for a declared disk that is NOT currently
/// represented in the live pool's `disk_usage`. Populated by `tui::probe`
/// from the read-only `probe_config_disk` result so the disk table can
/// distinguish unplugged, valid-but-unrelated, and broken-header states
/// instead of collapsing them all into a generic "missing".
///
/// Variants are deliberately prefixed with `LuksHeader` (not just
/// `Header`) to avoid ambiguity in the view layer — `Header` alone could
/// read as a btrfs header, an lsblk column header, etc.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnpooledDiskRender {
    /// `ConfigDiskState::Absent` — device file does not exist.
    Missing,
    /// `ConfigDiskState::PresentLuks` whose UUID is not in the live pool.
    /// LUKS header is valid but the disk does not belong to this pool.
    UnknownLuks,
    /// `ConfigDiskState::PresentNotLuks` refined to
    /// `LuksHeaderState::Unreadable` (or fallback). Severe — needs
    /// off-system header backup restore.
    LuksHeaderUnreadable,
    /// `ConfigDiskState::PresentNotLuks` refined to
    /// `LuksHeaderState::Damaged`. Potentially repairable via
    /// `cryptsetup repair`.
    LuksHeaderDamaged,
    /// `probe_config_disk` returned `ProbeError::UnsupportedLuksVersion`.
    /// The disk is on-disk LUKS but the wrong version (LUKS1 — braid
    /// requires LUKS2). Recovery: back up data, re-add via `braid add`.
    WrongLuksVersion(u32),
}

#[derive(Clone)]
pub struct PoolState {
    pub mount_point: MountPoint,
    pub df_entries: Vec<BtrfsDfEntry>,
    pub disk_usage: HashMap<String, DiskUsage>,
    pub disk_transport: HashMap<String, String>,
    pub smart_health: HashMap<String, SmartHealth>,
    pub disk_temperature_readings: HashMap<String, TemperatureReading>,
    pub luks_info: HashMap<String, DiskLuksInfo>,
    pub device_errors: HashMap<String, DiskErrors>,
    /// Per-declared-disk render classification for disks NOT in
    /// `disk_usage`. Populated by `tui::probe` via `probe_config_disk`
    /// so the disk table can render Unreadable / Damaged / UnknownLuks /
    /// Missing distinctly. Disks present in `disk_usage` are omitted.
    pub unpooled_disks: HashMap<String, UnpooledDiskRender>,
    pub alert_state: AlertState,
    pub scrub: ScrubState,
    pub balance: BalanceReport,
    pub capacity_total_bytes: Option<u64>,
    pub capacity_used_bytes: u64,
    pub probed_at: Instant,
}

pub enum PoolStatus {
    Loading,
    NotMounted,
    Mounted(PoolState),
    Refreshing(PoolState),
    Error(String),
    ErrorStale(String, PoolState),
}

impl PoolStatus {
    pub fn current(&self) -> Option<&PoolState> {
        match self {
            PoolStatus::Mounted(p) | PoolStatus::Refreshing(p) | PoolStatus::ErrorStale(_, p) => {
                Some(p)
            }
            _ => None,
        }
    }

    pub fn is_inflight(&self) -> bool {
        matches!(self, PoolStatus::Loading | PoolStatus::Refreshing(_))
    }
}

pub struct Model {
    pub running: bool,
    pub show_help: bool,
    pub show_disk_detail: bool,
    pub tab: Tab,
    pub disk_names: Vec<String>,
    pub disk_by_id: HashMap<String, String>,
    pub selected_disk: usize,
    pub pool: PoolStatus,
    pub mount_point: MountPoint,
    pub commands: HashMap<CmdId, CommandState>,
    pub probe_duration: Option<Duration>,
    pub frame: u64,
    pub spinner_deadline: Option<Instant>,
    pub advisories: Vec<String>,
    pub paths: StatePaths,
    pub session_temperature_stats: HashMap<TemperatureDiskId, TemperatureWatermark>,
    next_cmd_id: u64,
}

impl Model {
    pub fn new(
        disk_names: Vec<String>,
        disk_by_id: HashMap<String, String>,
        mount_point: String,
        advisories: Vec<String>,
        paths: StatePaths,
    ) -> (Self, Vec<Effect>) {
        let mount_point = MountPoint(mount_point);
        let effects = vec![Effect::ProbePool {
            mount_point: mount_point.clone(),
            disk_by_id: disk_by_id.clone(),
            paths: paths.clone(),
        }];
        let model = Self {
            running: true,
            show_help: false,
            show_disk_detail: false,
            tab: Tab::Data,
            disk_names,
            disk_by_id,
            selected_disk: 0,
            pool: PoolStatus::Loading,
            mount_point,
            commands: HashMap::new(),
            probe_duration: None,
            frame: 0,
            spinner_deadline: Some(Instant::now() + Duration::from_millis(500)),
            advisories,
            paths,
            session_temperature_stats: HashMap::new(),
            next_cmd_id: 0,
        };
        (model, effects)
    }

    pub fn new_demo(disk_names: Vec<String>, pool: PoolStatus) -> Self {
        Self {
            running: true,
            show_help: false,
            show_disk_detail: false,
            tab: Tab::Data,
            disk_names,
            disk_by_id: HashMap::new(),
            selected_disk: 0,
            pool,
            mount_point: MountPoint(String::new()),
            commands: HashMap::new(),
            probe_duration: None,
            frame: 0,
            spinner_deadline: None,
            advisories: vec![],
            paths: StatePaths::production(),
            session_temperature_stats: HashMap::new(),
            next_cmd_id: 0,
        }
    }
}
