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
use std::time::{Duration, Instant};

use app::update;
use effect::execute_effect;
use event::InputHandler;
use model::{DiskLuksInfo, DiskUsage, Model, PoolState, PoolStatus};
use view::view;

use crate::config::config_read;
use crate::hdparm::DrivePowerState;
use crate::parse::types::{ScrubState, ScrubTimestamp, SmartHealth};

pub fn run(config_path: &Path) -> io::Result<()> {
    let config = config_read(config_path).map_err(|e| io::Error::other(e.to_string()))?;
    let mut terminal = ratatui::init();
    let (_input, cmd_tx, rx) = InputHandler::new();
    let disk_keys: Vec<String> = config.disks().keys().cloned().collect();
    let disk_by_id: HashMap<String, String> = config
        .disks()
        .iter()
        .map(|(k, v)| (k.clone(), v.by_id.to_string()))
        .collect();
    let (mut model, init_effects) =
        Model::new(disk_keys, disk_by_id, config.mount_point().to_owned());
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
    let smart_health = HashMap::from([
        ("toshiba".to_owned(), SmartHealth::Healthy),
        ("ironwolf".to_owned(), SmartHealth::Degraded),
        ("wdc".to_owned(), SmartHealth::Unknown),
    ]);
    let power_state = HashMap::from([
        ("toshiba".to_owned(), DrivePowerState::Active),
        ("ironwolf".to_owned(), DrivePowerState::Standby),
        ("wdc".to_owned(), DrivePowerState::Idle),
    ]);
    let luks_info = HashMap::from([
        (
            "toshiba".to_owned(),
            DiskLuksInfo {
                cipher: "aes-xts-plain64".to_owned(),
                key_size_bits: 512,
                keyslot_count: 1,
            },
        ),
        (
            "ironwolf".to_owned(),
            DiskLuksInfo {
                cipher: "aes-xts-plain64".to_owned(),
                key_size_bits: 512,
                keyslot_count: 1,
            },
        ),
        (
            "wdc".to_owned(),
            DiskLuksInfo {
                cipher: "aes-xts-plain64".to_owned(),
                key_size_bits: 512,
                keyslot_count: 1,
            },
        ),
    ]);
    let disk_transport = HashMap::from([
        ("toshiba".to_owned(), "sata".to_owned()),
        ("ironwolf".to_owned(), "sata".to_owned()),
        ("wdc".to_owned(), "usb".to_owned()),
    ]);
    let pool = PoolState {
        mount_point: "/mnt/storage".to_owned(),
        profile: "RAID1".to_owned(),
        used: 2_308_094_370_816,
        total: 5_937_955_045_376,
        disk_usage,
        disk_transport,
        smart_health,
        power_state,
        luks_info,
        scrub: ScrubState::Completed {
            started_at: ScrubTimestamp(time::macros::datetime!(2026-02-24 02:00:07)),
            error_count: 0,
            duration: Some("0:00:00".to_owned()),
            total: Some("32.36MiB".to_owned()),
            rate: Some("32.34MiB/s".to_owned()),
        },
        probed_at: Instant::now(),
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
