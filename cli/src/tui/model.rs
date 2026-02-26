use std::collections::HashMap;
use std::time::Duration;

use crate::parse::types::ScrubState;
use crate::tui::effect::Effect;
use crate::tui::state::{CmdId, CommandState};

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

    #[cfg(test)]
    pub fn new_for_test(disk_keys: Vec<String>, pool: PoolStatus) -> Self {
        Self {
            running: true,
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
