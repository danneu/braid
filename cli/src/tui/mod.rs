mod app;
mod demo;
mod effect;
mod event;
mod keymap;
mod model;
pub(crate) mod probe;
mod view;

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use app::update;
use effect::{Effect, execute_effect};
use event::InputHandler;
use model::{Model, PoolStatus};
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
    let mut members: Vec<_> = membership.iter().collect();
    members.sort_by(|(_, a), (_, b)| a.name.cmp(&b.name));
    let disk_names: Vec<String> = members
        .iter()
        .map(|(_, member)| member.name.as_str().to_owned())
        .collect();
    let disk_by_id: HashMap<String, String> = members
        .iter()
        .map(|(_, member)| (member.name.as_str().to_owned(), member.by_id.to_string()))
        .collect();
    let disk_luks_uuid: HashMap<String, crate::types::LuksUuid> = members
        .iter()
        .map(|(uuid, member)| (member.name.as_str().to_owned(), (*uuid).clone()))
        .collect();
    let disk_devid: HashMap<String, u64> = members
        .iter()
        .filter_map(|(_, member)| {
            member
                .devid
                .map(|devid| (member.name.as_str().to_owned(), devid))
        })
        .collect();
    let (model, init_effects) = Model::new(
        disk_names,
        disk_by_id,
        disk_luks_uuid,
        disk_devid,
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
    let model = Model::new_demo(
        demo::sample_disk_names(),
        PoolStatus::Mounted(demo::sample_pool()),
    );
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
            messages.extend(event.into_message(model.show_help, model.show_disk_detail));
            for _ in 1..MAX_EVENTS_PER_FRAME {
                match rx.try_recv() {
                    Ok(event) => {
                        messages.extend(event.into_message(model.show_help, model.show_disk_detail))
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
