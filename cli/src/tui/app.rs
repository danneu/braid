use std::collections::VecDeque;
use std::process::ExitStatus;

use crate::tui::effect::Effect;
use crate::tui::model::{Model, PoolState, PoolStatus};
use crate::tui::state::{CmdId, CmdStatus, CommandState, MAX_LINES, Stream};

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
