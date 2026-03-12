use std::collections::HashMap;
use std::sync::mpsc;
use std::thread;

use crate::cmd::RealRunner;
use crate::tui::command;
use crate::tui::event::Event;
use crate::tui::state::CmdId;
use crate::types::MountPoint;

use std::time::Duration;

pub const PROBE_INTERVAL: Duration = Duration::from_secs(5);

pub enum Effect {
    SpawnCommand {
        id: CmdId,
        cmd: String,
    },
    ProbePool {
        mount_point: MountPoint,
        disk_by_id: HashMap<String, String>,
    },
    ScheduleProbe {
        mount_point: MountPoint,
        delay: Duration,
    },
}

pub fn execute_effect(effect: Effect, cmd_tx: &mpsc::Sender<Event>) {
    match effect {
        Effect::SpawnCommand { id, cmd } => {
            command::spawn(id, &cmd, cmd_tx);
        }
        Effect::ProbePool {
            mount_point,
            disk_by_id,
        } => {
            let tx = cmd_tx.clone();
            thread::spawn(move || {
                let start = std::time::Instant::now();
                let runner = RealRunner;
                let result = crate::tui::probe::probe_pool_for_tui(
                    &runner,
                    mount_point.as_str(),
                    &disk_by_id,
                );
                let elapsed = start.elapsed();
                let _ = tx.send(Event::PoolProbeFinished(result, elapsed));
            });
        }
        Effect::ScheduleProbe { mount_point, delay } => {
            let tx = cmd_tx.clone();
            thread::spawn(move || {
                thread::sleep(delay);
                let _ = tx.send(Event::PollRefresh { mount_point });
            });
        }
    }
}
