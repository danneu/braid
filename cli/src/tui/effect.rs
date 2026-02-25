use std::sync::mpsc;

use crate::tui::command;
use crate::tui::event::Event;
use crate::tui::state::CmdId;

pub enum Effect {
    SpawnCommand { id: CmdId, cmd: String },
}

pub fn execute_effect(effect: Effect, cmd_tx: &mpsc::Sender<Event>) {
    match effect {
        Effect::SpawnCommand { id, cmd } => {
            command::spawn(id, &cmd, cmd_tx);
        }
    }
}
