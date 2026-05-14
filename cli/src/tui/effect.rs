use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::mpsc;
use std::thread;

use crate::cmd::{CommandRunner, RawCommandOutput, RealRunner};
use crate::config::FanControl;
use crate::state_paths::StatePaths;
use crate::tui::event::Event;
use crate::types::{LuksUuid, MountPoint};

use std::time::Duration;

pub const PROBE_INTERVAL: Duration = Duration::from_secs(5);
pub const FAN_PROBE_INTERVAL: Duration = Duration::from_secs(5);
pub const UPS_PROBE_INTERVAL: Duration = Duration::from_secs(5);

pub enum Effect {
    ProbePool {
        mount_point: MountPoint,
        disk_by_id: HashMap<String, String>,
        /// Persistent disk identity map passed to the worker probe thread.
        disk_luks_uuid: HashMap<String, LuksUuid>,
        /// Prior btrfs devid bindings for devices whose current probe lacks a
        /// visible LUKS UUID.
        disk_devid: HashMap<String, u64>,
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
    /// Run a raw Browse-tab command on a worker thread and route stdout
    /// back through generation-checked TUI update.
    BrowseRunCommand {
        request: crate::cmd::CmdRequest,
        generation: u64,
    },
}

pub fn execute_effect(effect: Effect, cmd_tx: &mpsc::Sender<Event>) {
    match effect {
        Effect::ProbePool {
            mount_point,
            disk_by_id,
            disk_luks_uuid,
            disk_devid,
            paths,
        } => {
            let tx = cmd_tx.clone();
            thread::spawn(move || {
                let start = std::time::Instant::now();
                let runner = RealRunner;
                let fs = crate::probe::RealFilesystem;
                let backing_path_resolver = crate::luks::RealBackingPathResolver;
                let result = crate::tui::probe::probe_pool_for_tui(
                    &runner,
                    &fs,
                    &mount_point,
                    &disk_by_id,
                    &disk_luks_uuid,
                    &disk_devid,
                    &paths,
                    &backing_path_resolver,
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
        Effect::BrowseRunCommand {
            request,
            generation,
        } => {
            let tx = cmd_tx.clone();
            thread::spawn(move || {
                let runner = RealRunner;
                let raw = match runner.run(&request) {
                    Ok(raw) => raw,
                    Err(e) => RawCommandOutput {
                        cmd: format!("{request:?}"),
                        stdout: String::new(),
                        stderr: format!("error: {e}"),
                        exit_status: 1,
                    },
                };
                let _ = tx.send(Event::BrowseCommandFinished { raw, generation });
            });
        }
    }
}
