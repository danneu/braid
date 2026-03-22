pub mod app;
mod event;
mod keymap;
pub mod model;
mod view;

use std::io;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use app::update;
use event::InputHandler;
use model::Model;
use view::view;

use crate::cmd::{CmdRequest, RealRunner, CommandRunner, RawCommandOutput};
use crate::parse::parse_btrfs_subvolume_list;
use crate::types::MountPoint;

pub enum Effect {
    RunCommand {
        request: CmdRequest,
        generation: u64,
    },
}

pub fn run(mount_point: &str) -> io::Result<()> {
    let mp = MountPoint(mount_point.to_owned());
    let mut terminal = ratatui::init();
    let (_input, cmd_tx, rx) = InputHandler::new();
    let (mut model, init_effects) = Model::new(mp);
    for effect in init_effects {
        execute_effect(effect, &cmd_tx);
    }
    let result = run_loop(&mut terminal, &mut model, &rx, &cmd_tx);
    ratatui::restore();
    result
}

/// Non-interactive check mode: runs key commands and exits 0/1.
pub fn run_check(mount_point: &str) -> io::Result<()> {
    let mp = MountPoint(mount_point.to_owned());
    let runner = RealRunner;

    // 1. btrfs filesystem usage
    let req = CmdRequest::BtrfsFilesystemUsage {
        mount_point: mp.clone(),
    };
    let raw = runner
        .run(&req)
        .map_err(|e| io::Error::other(format!("filesystem usage: {e}")))?;
    if raw.exit_status != 0 {
        return Err(io::Error::other(format!(
            "filesystem usage exited {}: {}",
            raw.exit_status,
            raw.stderr.trim()
        )));
    }
    println!("ok: btrfs filesystem usage");

    // 2. btrfs subvolume list
    let req = CmdRequest::BtrfsSubvolumeList {
        mount_point: mp.clone(),
    };
    let raw = runner
        .run(&req)
        .map_err(|e| io::Error::other(format!("subvolume list: {e}")))?;
    if raw.exit_status != 0 {
        return Err(io::Error::other(format!(
            "subvolume list exited {}: {}",
            raw.exit_status,
            raw.stderr.trim()
        )));
    }
    let parsed = parse_btrfs_subvolume_list(&raw)
        .map_err(|e| io::Error::other(format!("subvolume list parse: {e}")))?;
    println!(
        "ok: btrfs subvolume list ({} subvolumes)",
        parsed.subvolumes.len()
    );

    // 3. Drill into first subvolume if any exist
    if let Some(sv) = parsed.subvolumes.first() {
        let path = format!("{}/{}", mount_point, sv.path);
        let req = CmdRequest::BtrfsSubvolumeShow { path: path.clone() };
        let raw = runner
            .run(&req)
            .map_err(|e| io::Error::other(format!("subvolume show: {e}")))?;
        if raw.exit_status != 0 {
            return Err(io::Error::other(format!(
                "subvolume show exited {}: {}",
                raw.exit_status,
                raw.stderr.trim()
            )));
        }
        println!("ok: btrfs subvolume show {path}");
    }

    Ok(())
}

const FRAME_BUDGET: Duration = Duration::from_millis(16);
const MAX_EVENTS_PER_FRAME: usize = 100;

fn run_loop(
    terminal: &mut ratatui::DefaultTerminal,
    model: &mut Model,
    rx: &mpsc::Receiver<event::Event>,
    cmd_tx: &mpsc::Sender<event::Event>,
) -> io::Result<()> {
    while model.running {
        model.frame = model.frame.wrapping_add(1);
        terminal.draw(|f| view(model, f))?;

        let mut messages = Vec::new();
        if let Ok(event) = rx.recv_timeout(FRAME_BUDGET) {
            messages.extend(event.into_message(&model.mode));
            for _ in 1..MAX_EVENTS_PER_FRAME {
                match rx.try_recv() {
                    Ok(event) => messages.extend(event.into_message(&model.mode)),
                    Err(_) => break,
                }
            }
        }

        let mut effects = Vec::new();
        for msg in messages {
            effects.extend(update(model, msg));
        }

        for effect in effects {
            execute_effect(effect, cmd_tx);
        }
    }
    Ok(())
}

fn execute_effect(effect: Effect, tx: &mpsc::Sender<event::Event>) {
    match effect {
        Effect::RunCommand {
            request,
            generation,
        } => {
            let tx = tx.clone();
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
                let _ = tx.send(event::Event::CommandFinished { raw, generation });
            });
        }
    }
}
