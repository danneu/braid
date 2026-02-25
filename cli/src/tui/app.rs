use std::collections::HashMap;
use std::collections::VecDeque;
use std::process::ExitStatus;

use crate::tui::effect::Effect;
use crate::tui::state::{CmdId, CmdStatus, CommandState, MAX_LINES, Stream};

pub struct PoolState {
    pub mount_point: String,
    pub profile: String,
    pub health: String,
    pub used: u64,
    pub total: u64,
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
    pub pool: PoolStatus,
    pub mount_point: String,
    pub commands: HashMap<CmdId, CommandState>,
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
            pool: PoolStatus::Loading,
            mount_point,
            commands: HashMap::new(),
            next_cmd_id: 0,
        };
        (model, effects)
    }

    #[cfg(test)]
    pub fn new_for_test(disk_keys: Vec<String>, pool: PoolStatus) -> Self {
        Self {
            running: true,
            disk_keys,
            pool,
            mount_point: String::new(),
            commands: HashMap::new(),
            next_cmd_id: 0,
        }
    }
}

pub enum Message {
    Quit,
    Tick,
    CommandStarted {
        id: CmdId,
        cmd: String,
    },
    CommandOutput {
        id: CmdId,
        stream: Stream,
        line: String,
    },
    CommandFinished {
        id: CmdId,
        status: ExitStatus,
    },
    PoolProbeFinished(Result<Option<PoolState>, String>),
}

pub fn update(model: &mut Model, msg: Message) -> Vec<Effect> {
    match msg {
        Message::Quit => {
            model.running = false;
            vec![]
        }
        Message::Tick => vec![],
        Message::CommandStarted { id, cmd } => {
            model.commands.insert(
                id,
                CommandState {
                    cmd,
                    status: CmdStatus::Running,
                    output: VecDeque::new(),
                },
            );
            vec![]
        }
        Message::CommandOutput {
            id,
            stream: _,
            line,
        } => {
            if let Some(state) = model.commands.get_mut(&id) {
                state.output.push_back(line);
                if state.output.len() > MAX_LINES {
                    state.output.pop_front();
                }
            }
            vec![]
        }
        Message::CommandFinished { id, status } => {
            if let Some(state) = model.commands.get_mut(&id) {
                state.status = CmdStatus::Finished(status);
            }
            vec![]
        }
        Message::PoolProbeFinished(result) => {
            model.pool = match result {
                Ok(Some(pool)) => PoolStatus::Mounted(pool),
                Ok(None) => PoolStatus::NotMounted,
                Err(e) => PoolStatus::Error(e),
            };
            vec![]
        }
    }
}
