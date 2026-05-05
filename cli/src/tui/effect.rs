use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use crate::cmd::RealRunner;
use crate::config::FanControl;
use crate::state_paths::StatePaths;
use crate::tui::event::Event;
use crate::types::MountPoint;

use std::time::Duration;

pub const PROBE_INTERVAL: Duration = Duration::from_secs(5);
pub const FAN_PROBE_INTERVAL: Duration = Duration::from_secs(5);
pub const UPS_PROBE_INTERVAL: Duration = Duration::from_secs(5);

pub enum Effect {
    ProbePool {
        mount_point: MountPoint,
        disk_by_id: HashMap<String, String>,
        paths: StatePaths,
    },
    ScheduleProbe {
        mount_point: MountPoint,
        delay: Duration,
    },
    ProbeFan {
        sysfs_root: PathBuf,
        dev_root: PathBuf,
        disk_by_id: HashMap<String, String>,
        fan_control: FanControl,
    },
    ScheduleFanProbe {
        delay: Duration,
    },
    /// Run `upsc <name>` through `query_ups` on a worker thread; the result
    /// becomes `Event::UpsProbeFinished`.
    ProbeUps {
        name: String,
    },
    ScheduleUpsProbe {
        delay: Duration,
    },
}

pub fn execute_effect(effect: Effect, cmd_tx: &mpsc::Sender<Event>) {
    match effect {
        Effect::ProbePool {
            mount_point,
            disk_by_id,
            paths,
        } => {
            let tx = cmd_tx.clone();
            thread::spawn(move || {
                let start = std::time::Instant::now();
                let runner = RealRunner;
                let fs = crate::probe::RealFilesystem;
                let result = crate::tui::probe::probe_pool_for_tui(
                    &runner,
                    &fs,
                    &mount_point,
                    &disk_by_id,
                    &paths,
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
        Effect::ProbeFan {
            sysfs_root,
            dev_root,
            disk_by_id,
            fan_control,
        } => {
            let tx = cmd_tx.clone();
            thread::spawn(move || {
                let runner = RealRunner;
                let snapshot = crate::tui::probe::probe_fan_for_tui(
                    &runner,
                    &sysfs_root,
                    &dev_root,
                    &disk_by_id,
                    &fan_control,
                );
                let _ = tx.send(Event::FanProbeFinished(snapshot));
            });
        }
        Effect::ScheduleFanProbe { delay } => {
            let tx = cmd_tx.clone();
            thread::spawn(move || {
                thread::sleep(delay);
                let _ = tx.send(Event::PollFanRefresh);
            });
        }
        Effect::ProbeUps { name } => {
            let tx = cmd_tx.clone();
            thread::spawn(move || {
                let runner = RealRunner;
                let snapshot = crate::tui::probe::probe_ups_for_tui(&runner, &name);
                let _ = tx.send(Event::UpsProbeFinished(snapshot));
            });
        }
        Effect::ScheduleUpsProbe { delay } => {
            let tx = cmd_tx.clone();
            thread::spawn(move || {
                thread::sleep(delay);
                let _ = tx.send(Event::PollUpsRefresh);
            });
        }
    }
}
