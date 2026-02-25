use std::collections::HashMap;
use std::collections::VecDeque;
use std::process::ExitStatus;

use crate::tui::effect::Effect;
use crate::tui::state::{CmdId, CmdStatus, CommandState, Stream, MAX_LINES};
use crate::types::ByIdPath;

pub struct Model {
    pub running: bool,
    pub disks: Vec<ByIdPath>,
    pub commands: HashMap<CmdId, CommandState>,
    next_cmd_id: u64,
}

impl Model {
    pub fn new(disks: Vec<ByIdPath>) -> Self {
        Self {
            running: true,
            disks,
            commands: HashMap::new(),
            next_cmd_id: 0,
        }
    }
}

pub enum Message {
    Quit,
    Tick,
    CommandStarted { id: CmdId, cmd: String },
    CommandOutput { id: CmdId, stream: Stream, line: String },
    CommandFinished { id: CmdId, status: ExitStatus },
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
    }
}
