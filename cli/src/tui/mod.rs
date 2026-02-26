mod app;
mod command;
mod effect;
mod event;
mod keymap;
mod model;
pub(crate) mod probe;
mod state;
mod view;

use std::collections::HashMap;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use app::update;
use effect::execute_effect;
use event::InputHandler;
use model::{DiskUsage, Model, PoolState, PoolStatus};
use view::view;

use crate::config::config_read;
use crate::parse::types::{ScrubState, ScrubTimestamp};

pub fn run(config_path: &Path) -> io::Result<()> {
    let config = config_read(config_path).map_err(|e| io::Error::other(e.to_string()))?;
    let mut terminal = ratatui::init();
    let (_input, cmd_tx, rx) = InputHandler::new();
    let disk_keys: Vec<String> = config.disks().keys().cloned().collect();
    let (mut model, init_effects) = Model::new(disk_keys, config.mount_point().to_owned());
    for effect in init_effects {
        execute_effect(effect, &cmd_tx);
    }
    let result = run_loop(&mut terminal, &mut model, &rx, &cmd_tx);
    ratatui::restore();
    result
}

pub fn run_demo() -> io::Result<()> {
    let disk_keys = vec![
        "toshiba".to_owned(),
        "ironwolf".to_owned(),
        "wdc".to_owned(),
    ];
    let disk_usage = HashMap::from([
        (
            "toshiba".to_owned(),
            DiskUsage {
                size: 6_001_175_126_016,
                data: 1_483_734_958_080,
                metadata: 1_610_612_736,
            },
        ),
        (
            "ironwolf".to_owned(),
            DiskUsage {
                size: 6_001_175_126_016,
                data: 1_483_734_958_080,
                metadata: 1_610_612_736,
            },
        ),
        (
            "wdc".to_owned(),
            DiskUsage {
                size: 4_000_787_030_016,
                data: 824_633_720_832,
                metadata: 1_073_741_824,
            },
        ),
    ]);
    let pool = PoolState {
        mount_point: "/mnt/storage".to_owned(),
        profile: "RAID1".to_owned(),
        health: "healthy".to_owned(),
        used: 2_308_094_370_816,
        total: 5_937_955_045_376,
        disk_usage,
        scrub: ScrubState::Completed {
            started_at: ScrubTimestamp(time::macros::datetime!(2026-02-24 02:00:07)),
            error_count: 0,
            duration: Some("0:00:00".to_owned()),
            total: Some("32.36MiB".to_owned()),
            rate: Some("32.34MiB/s".to_owned()),
        },
    };
    let mut model = Model::new_demo(disk_keys, PoolStatus::Mounted(pool));

    let mut terminal = ratatui::init();
    let (_input, cmd_tx, rx) = InputHandler::new();
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
        let now = {
            let offset =
                time::UtcOffset::current_local_offset().unwrap_or(time::UtcOffset::UTC);
            let local = time::OffsetDateTime::now_utc().to_offset(offset);
            time::PrimitiveDateTime::new(local.date(), local.time())
        };
        terminal.draw(|f| view(model, f, now))?;

        let mut messages = Vec::new();
        if let Ok(event) = rx.recv_timeout(FRAME_BUDGET) {
            messages.extend(event.into_message());
            for _ in 1..MAX_EVENTS_PER_FRAME {
                match rx.try_recv() {
                    Ok(event) => messages.extend(event.into_message()),
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
