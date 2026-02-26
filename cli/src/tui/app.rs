use std::collections::VecDeque;
use std::process::ExitStatus;
use std::time::Duration;

use crate::tui::effect::{Effect, PROBE_INTERVAL};
use crate::tui::model::{Model, PoolState, PoolStatus};
use crate::tui::state::{CmdId, CmdStatus, CommandState, Stream, MAX_LINES};

pub enum Message {
    Quit,
    RefreshPool,
    SelectNextDisk,
    SelectPrevDisk,
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
    PoolProbeFinished(Result<Option<PoolState>, String>, Duration),
}

pub fn update(model: &mut Model, msg: Message) -> Vec<Effect> {
    match msg {
        Message::Quit => {
            model.running = false;
            vec![]
        }
        Message::RefreshPool => match model.pool {
            PoolStatus::Loading => vec![],
            _ => {
                model.pool = PoolStatus::Loading;
                vec![Effect::ProbePool {
                    mount_point: model.mount_point.clone(),
                }]
            }
        },
        Message::SelectNextDisk => {
            let len = model.disk_keys.len();
            if len > 0 {
                model.selected_disk = (model.selected_disk + 1) % len;
            }
            vec![]
        }
        Message::SelectPrevDisk => {
            let len = model.disk_keys.len();
            if len > 0 {
                model.selected_disk = (model.selected_disk + len - 1) % len;
            }
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
        Message::PoolProbeFinished(result, elapsed) => {
            model.pool = match result {
                Ok(Some(pool)) => PoolStatus::Mounted(pool),
                Ok(None) => PoolStatus::NotMounted,
                Err(e) => PoolStatus::Error(e),
            };
            model.probe_duration = Some(elapsed);
            vec![Effect::ScheduleProbe {
                mount_point: model.mount_point.clone(),
                delay: PROBE_INTERVAL,
            }]
        }
    }
}
