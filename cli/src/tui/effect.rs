use std::sync::mpsc;
use std::thread;

use crate::cmd::RealRunner;
use crate::tui::command;
use crate::tui::event::Event;
use crate::tui::state::CmdId;

pub enum Effect {
    SpawnCommand { id: CmdId, cmd: String },
    ProbePool { mount_point: String },
}

pub fn execute_effect(effect: Effect, cmd_tx: &mpsc::Sender<Event>) {
    match effect {
        Effect::SpawnCommand { id, cmd } => {
            command::spawn(id, &cmd, cmd_tx);
        }
        Effect::ProbePool { mount_point } => {
            let tx = cmd_tx.clone();
            thread::spawn(move || {
                let start = std::time::Instant::now();
                let runner = RealRunner;
                let result = crate::tui::probe::probe_pool_for_tui(&runner, &mount_point);
                let elapsed = start.elapsed();
                let _ = tx.send(Event::PoolProbeFinished(result, elapsed));
            });
        }
    }
}
