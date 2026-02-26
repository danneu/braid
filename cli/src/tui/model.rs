use std::collections::HashMap;
use std::time::Duration;

use crate::parse::types::ScrubState;
use crate::tui::effect::Effect;
use crate::tui::state::{CmdId, CommandState};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Data,
    Encryption,
    Sharing,
}

impl Tab {
    pub const ALL: [Tab; 3] = [Tab::Data, Tab::Encryption, Tab::Sharing];

    pub fn label(self) -> &'static str {
        match self {
            Tab::Data => "Data",
            Tab::Encryption => "Encryption",
            Tab::Sharing => "Sharing",
        }
    }

    pub fn next(self) -> Tab {
        match self {
            Tab::Data => Tab::Encryption,
            Tab::Encryption => Tab::Sharing,
            Tab::Sharing => Tab::Data,
        }
    }

    pub fn prev(self) -> Tab {
        match self {
            Tab::Data => Tab::Sharing,
            Tab::Encryption => Tab::Data,
            Tab::Sharing => Tab::Encryption,
        }
    }
}

pub struct DiskUsage {
    pub size: u64,
    pub data: u64,
    pub metadata: u64,
}

pub struct PoolState {
    pub mount_point: String,
    pub profile: String,
    pub health: String,
    pub used: u64,
    pub total: u64,
    pub disk_usage: HashMap<String, DiskUsage>,
    pub scrub: ScrubState,
}

pub enum PoolStatus {
    Loading,
    NotMounted,
    Mounted(PoolState),
    Error(String),
}

pub struct Model {
    pub running: bool,
    pub tab: Tab,
    pub disk_keys: Vec<String>,
    pub selected_disk: usize,
    pub pool: PoolStatus,
    pub mount_point: String,
    pub commands: HashMap<CmdId, CommandState>,
    pub probe_duration: Option<Duration>,
    next_cmd_id: u64,
}

impl Model {
    pub fn new(disk_keys: Vec<String>, mount_point: String) -> (Self, Vec<Effect>) {
        let effects = vec![Effect::ProbePool {
            mount_point: mount_point.clone(),
        }];
        let model = Self {
            running: true,
            tab: Tab::Data,
            disk_keys,
            selected_disk: 0,
            pool: PoolStatus::Loading,
            mount_point,
            commands: HashMap::new(),
            probe_duration: None,
            next_cmd_id: 0,
        };
        (model, effects)
    }

    pub fn new_demo(disk_keys: Vec<String>, pool: PoolStatus) -> Self {
        Self {
            running: true,
            tab: Tab::Data,
            disk_keys,
            selected_disk: 0,
            pool,
            mount_point: String::new(),
            commands: HashMap::new(),
            probe_duration: None,
            next_cmd_id: 0,
        }
    }
}
