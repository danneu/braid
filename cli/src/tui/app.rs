use std::collections::VecDeque;
use std::process::ExitStatus;
use std::time::Duration;

use crate::tui::effect::{Effect, PROBE_INTERVAL};
use crate::tui::model::{Model, PoolState, PoolStatus};
use crate::tui::state::{CmdId, CmdStatus, CommandState, Stream, MAX_LINES};

pub enum Message {
    Quit,
    ToggleHelp,
    NextTab,
    PrevTab,
    RefreshPool,
    SelectNextDisk,
    SelectPrevDisk,
    OpenDiskDetail,
    CloseDiskDetail,
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
        Message::ToggleHelp => {
            model.show_help = !model.show_help;
            vec![]
        }
        Message::NextTab => {
            model.tab = model.tab.next();
            vec![]
        }
        Message::PrevTab => {
            model.tab = model.tab.prev();
            vec![]
        }
        Message::RefreshPool => {
            if model.pool.is_inflight() {
                return vec![];
            }
            if let Some(stale) = model.pool.current().cloned() {
                model.pool = PoolStatus::Refreshing(stale);
            } else {
                model.pool = PoolStatus::Loading;
            }
            vec![Effect::ProbePool {
                mount_point: model.mount_point.clone(),
                disk_by_id: model.disk_by_id.clone(),
            }]
        }
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
        Message::OpenDiskDetail => {
            model.show_disk_detail = true;
            vec![]
        }
        Message::CloseDiskDetail => {
            model.show_disk_detail = false;
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
            let stale = model.pool.current().cloned();
            model.pool = match result {
                Ok(Some(pool)) => PoolStatus::Mounted(pool),
                Ok(None) => PoolStatus::NotMounted,
                Err(e) => match stale {
                    Some(s) => PoolStatus::ErrorStale(e, s),
                    None => PoolStatus::Error(e),
                },
            };
            model.probe_duration = Some(elapsed);
            // TODO: re-enable auto-polling
            // vec![Effect::ScheduleProbe {
            //     mount_point: model.mount_point.clone(),
            //     delay: PROBE_INTERVAL,
            // }]
            vec![]
        }
    }
}
