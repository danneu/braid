use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::parse::types::{BtrfsDfEntry, DeviceAllocation, ScrubState, SmartHealth};
use crate::status::BalanceReport;
use crate::tui::effect::Effect;
use crate::tui::state::{CmdId, CommandState};
use crate::types::MountPoint;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Data,
    Sharing,
}

impl Tab {
    pub const ALL: [Tab; 2] = [Tab::Data, Tab::Sharing];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Data => "Data",
            Tab::Sharing => "Sharing",
        }
    }

    pub fn next(self) -> Tab {
        match self {
            Tab::Data => Tab::Sharing,
            Tab::Sharing => Tab::Data,
        }
    }

    pub fn prev(self) -> Tab {
        match self {
            Tab::Data => Tab::Sharing,
            Tab::Sharing => Tab::Data,
        }
    }
}

#[derive(Clone)]
pub struct DiskLuksInfo {
    pub cipher: String,
    pub key_size_bits: u32,
    pub keyslot_count: u32,
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

#[derive(Clone)]
pub struct PoolState {
    pub mount_point: MountPoint,
    pub df_entries: Vec<BtrfsDfEntry>,
    pub disk_usage: HashMap<String, DiskUsage>,
    pub disk_transport: HashMap<String, String>,
    pub smart_health: HashMap<String, SmartHealth>,
    pub luks_info: HashMap<String, DiskLuksInfo>,
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
    next_cmd_id: u64,
}

impl Model {
    pub fn new(
        disk_names: Vec<String>,
        disk_by_id: HashMap<String, String>,
        mount_point: String,
    ) -> (Self, Vec<Effect>) {
        let mount_point = MountPoint(mount_point);
        let effects = vec![Effect::ProbePool {
            mount_point: mount_point.clone(),
            disk_by_id: disk_by_id.clone(),
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
            next_cmd_id: 0,
        }
    }
}
