mod app;
pub(crate) mod browse;
mod demo;
mod effect;
mod event;
mod keymap;
mod model;
pub(crate) mod probe;
mod view;

use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use app::update;
use effect::{Effect, execute_effect};
use event::InputHandler;
use keymap::KeyContext;
use model::{DiskIdentity, Model, PoolStatus};
use view::view;

use crate::config::config_read;
use crate::luks;
use crate::membership;
use crate::state_paths::StatePaths;

pub fn run(config_path: &Path, paths: &StatePaths) -> io::Result<()> {
    crate::util::require_tty("tui")?;
    let config = config_read(config_path).map_err(|e| io::Error::other(e.to_string()))?;
    let membership =
        membership::load_membership(paths).map_err(|e| io::Error::other(e.to_string()))?;
    let advisories = luks::header_backup_advisories(paths);
    let disks = DiskIdentity::from_membership(&membership);
    let (model, init_effects) = Model::new(
        disks,
        config.mount_point().0.clone(),
        config.fan_control().cloned(),
        config.ups().cloned(),
        advisories,
        paths.clone(),
    );
    run_with_model(model, init_effects)
}

pub fn run_demo() -> io::Result<()> {
    crate::util::require_tty("tui")?;
    let mut model = Model::new_demo(
        demo::sample_disk_names(),
        PoolStatus::Mounted(demo::sample_pool()),
    );
    model.disk_luks_states = demo::sample_disk_luks_states();
    run_with_model(model, vec![])
}

fn run_with_model(mut model: Model, init_effects: Vec<Effect>) -> io::Result<()> {
    let mut terminal = ratatui::init();
    let (_input, cmd_tx, rx) = InputHandler::new();
    for effect in init_effects {
        execute_effect(effect, &cmd_tx);
    }
    let result = run_loop(&mut terminal, &mut model, &rx, &cmd_tx);
    ratatui::restore();
    result
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
        let now = {
            let offset = time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
            let local = time::OffsetDateTime::now_utc().to_offset(offset);
            time::PrimitiveDateTime::new(local.date(), local.time())
        };
        terminal.draw(|f| view(model, f, now))?;

        let mut messages = Vec::new();
        if let Ok(event) = rx.recv_timeout(FRAME_BUDGET) {
            let ctx = key_context(model);
            messages.extend(event.into_message(&ctx));
            for _ in 1..MAX_EVENTS_PER_FRAME {
                match rx.try_recv() {
                    Ok(event) => {
                        let ctx = key_context(model);
                        messages.extend(event.into_message(&ctx))
                    }
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

fn key_context(model: &Model) -> KeyContext {
    KeyContext {
        tab: model.tab,
        show_help: model.show_help,
        show_disk_detail: model.show_disk_detail,
        browse_focus: model.browse.focus,
    }
}
